//! Turn-completion signals (`docs/design.md` §7).
//!
//! When an agent finishes a turn, its Claude `Stop` hook posts a one-line JSON
//! [`TurnSignal`] to the caucus unix-domain socket. caucus reads it live —
//! no file sentinel, no polling.

pub mod post;
pub mod server;

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::id::{PanelId, SessionId};

/// What kind of turn-completion event the signal carries (`docs/design.md`
/// §7.4). Serialised lowercase: `stop | tool_blocked | error`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnKind {
    /// Normal turn completion (Claude `Stop` hook).
    Stop,
    /// The agent stopped because a tool was blocked (e.g. permission prompt).
    ToolBlocked,
    /// The turn ended in an error.
    Error,
}

/// One turn-completion signal, posted by an agent's Stop hook to the caucus
/// socket. Schema mirrors `docs/design.md` §7.4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnSignal {
    pub session_id: SessionId,
    pub panel_id: PanelId,
    pub ts: DateTime<Utc>,
    pub kind: TurnKind,
    /// The agent's final assistant message, lifted from the hook payload.
    /// Lets the main worker judge most turns without scraping the terminal.
    pub last_message: Option<String>,
    /// Path of the agent's conversation transcript (JSONL), lifted from the
    /// hook payload's `transcript_path`. Lets the main worker read the whole
    /// conversation from disk instead of scraping the terminal. `None` for
    /// payloads that carry no such path (codex's notify JSON has no
    /// counterpart) — and for signal lines posted by an older `caucus` binary,
    /// hence the serde default.
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
    /// Whether the poster waits for a one-line [`SignalReply`] on the same
    /// connection before its hook exits (`docs/design.md` §7.6). Set only by
    /// `caucus signal post` running under `CAUCUS_HOOK_REPLY=1` — which caucus
    /// injects into the claude **main** panel alone. `false` serialises to
    /// nothing at all, so every other poster's wire line stays byte-identical
    /// to the pre-reply protocol, and a line from an older binary parses with
    /// `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wants_reply: bool,
    /// The raw Claude hook payload, retained verbatim for diagnostics.
    pub raw_hook_payload: serde_json::Value,
}

impl TurnSignal {
    /// Construct a signal stamped with `Utc::now()`. `transcript_path` is
    /// lifted from `raw_hook_payload` here, so a constructed signal can never
    /// disagree with its own payload about where the transcript lives.
    pub fn now(
        session_id: SessionId,
        panel_id: PanelId,
        kind: TurnKind,
        last_message: Option<String>,
        raw_hook_payload: serde_json::Value,
    ) -> Self {
        let transcript_path = extract_transcript_path(&raw_hook_payload);
        Self {
            session_id,
            panel_id,
            ts: Utc::now(),
            kind,
            last_message,
            transcript_path,
            wants_reply: false,
            raw_hook_payload,
        }
    }

    /// Background work this session is still waiting on, as the hook payload
    /// reports it — `Some(n)` with `n > 0` means the agent stopped *paused*,
    /// not finished.
    ///
    /// Claude Code puts a `background_tasks` array on the `Stop` payload for
    /// exactly this question, describing it as what "lets hooks distinguish
    /// 'session is done' from 'session is paused waiting for background work
    /// to wake it'". The array is pre-filtered to in-flight work only — a task
    /// is included only when its status is `running` or `pending` *and* it is
    /// backgrounded — so its emptiness is the whole predicate; caucus does not
    /// re-interpret the entries.
    ///
    /// `None` means the payload does not carry the field at all: a Claude Code
    /// old enough to predate it, or a non-Claude backend (codex's notify JSON
    /// has no counterpart). Absence is not evidence of in-flight work, so it
    /// reads as "nothing known", and the caller treats the turn as it always
    /// did.
    pub fn background_tasks_in_flight(&self) -> Option<usize> {
        self.raw_hook_payload
            .get("background_tasks")
            .and_then(Value::as_array)
            .map(Vec::len)
    }
}

/// What a mid-turn [`AgentNote`] carries. Serialised lowercase:
/// `progress | artifact | question`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    /// A progress report partway through a long turn.
    Progress,
    /// An artifact the agent produced, named by path or reference.
    Artifact,
    /// A question for the main worker; caucus forwards it as a notice.
    Question,
}

impl NoteKind {
    /// Canonical lowercase name, as serialised on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Progress => "progress",
            Self::Artifact => "artifact",
            Self::Question => "question",
        }
    }
}

/// One mid-turn note, posted by an agent via `caucus signal note` using the
/// `CAUCUS_*` env caucus injects into every panel. Unlike a [`TurnSignal`] it
/// does not end a turn: the panel stays `Working` and no state transition
/// happens — the note is recorded on the panel's manifest, and a
/// [`NoteKind::Question`] is additionally forwarded to the main worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentNote {
    pub session_id: SessionId,
    pub panel_id: PanelId,
    pub ts: DateTime<Utc>,
    pub kind: NoteKind,
    pub body: String,
}

/// Longest note body caucus records or forwards, in bytes. A note is a line
/// on a timeline, not a payload channel — an artifact belongs in a file,
/// referenced by path. The socket accepts lines up to `MAX_IPC_LINE_BYTES`
/// (~1MiB) and the manifest JSON is atomically rewritten on every write, so an
/// uncapped body would bloat every subsequent manifest write.
pub const NOTE_BODY_MAX_BYTES: usize = 2048;

impl AgentNote {
    /// Construct a note stamped with `Utc::now()`.
    pub fn now(session_id: SessionId, panel_id: PanelId, kind: NoteKind, body: String) -> Self {
        Self {
            session_id,
            panel_id,
            ts: Utc::now(),
            kind,
            body,
        }
    }

    /// This note with its body capped at [`NOTE_BODY_MAX_BYTES`] (cut on a
    /// char boundary, with a truncation marker). Applied once at ingest
    /// (`Multiplexer::handle_note`) so everything downstream — the manifest
    /// record and the main-worker notice — sees the same capped body.
    pub fn truncated(mut self) -> Self {
        if self.body.len() > NOTE_BODY_MAX_BYTES {
            let mut end = NOTE_BODY_MAX_BYTES;
            while !self.body.is_char_boundary(end) {
                end -= 1;
            }
            self.body.truncate(end);
            self.body.push_str("… [truncated]");
        }
        self
    }
}

/// How a compaction was triggered, lifted from the `PostCompact` hook
/// payload's `trigger` field. `manual` is the user (or the main worker via
/// `send_keys`) running `/compact` from the input prompt; `auto` is Claude Code
/// compacting because the context window filled — which happens *inside* the
/// agent's query loop, between turns of a running turn, and ends nothing.
///
/// This is the whole reason caucus reads `trigger` rather than inferring: only
/// `manual` closes a command phase. Treating an `auto` compaction as a
/// completion settles a panel that is still working.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactTrigger {
    Manual,
    Auto,
}

/// What kind of lifecycle event a [`LifecycleSignal`] carries. Serialised
/// internally tagged on `kind` (`post_compact` / `session_start`) so the wire
/// vocabulary stays disjoint from turn signals and notes.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LifecycleKind {
    /// Claude Code finished compacting the conversation (`PostCompact` hook).
    /// Carries the trigger, so a manual `/compact` completing is distinguished
    /// from an auto-compaction mid-turn by the payload itself.
    PostCompact { trigger: CompactTrigger },
    /// A session (re)started (`SessionStart` hook). `source` is Claude Code's
    /// own vocabulary — `startup` / `resume` / `clear` / `compact` — kept as a
    /// string so a source this binary does not know is ignored, not a parse
    /// failure that drops the line.
    SessionStart { source: String },
}

/// One session-lifecycle signal, posted by the `PostCompact` / `SessionStart`
/// hooks to the caucus socket (`docs/design.md` §7). Not a turn boundary by
/// itself: the runtime (`Multiplexer::handle_lifecycle`) decides whether it
/// closes a local-command phase (`/compact`, `/clear`) — the completions the
/// Stop hook can never report, because a local builtin runs no agent turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleSignal {
    pub session_id: SessionId,
    pub panel_id: PanelId,
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: LifecycleKind,
    /// The raw Claude hook payload, retained verbatim for diagnostics.
    pub raw_hook_payload: serde_json::Value,
}

impl LifecycleSignal {
    /// Construct a signal stamped with `Utc::now()`.
    pub fn now(
        session_id: SessionId,
        panel_id: PanelId,
        kind: LifecycleKind,
        raw_hook_payload: serde_json::Value,
    ) -> Self {
        Self {
            session_id,
            panel_id,
            ts: Utc::now(),
            kind,
            raw_hook_payload,
        }
    }
}

/// One parsed line off the signal socket: the historical turn signal, a
/// mid-turn note, a session-lifecycle signal, or an unbound turn signal from a
/// hook that lost its `CAUCUS_*` env. Untagged so each client's wire line stays
/// exactly what it already posts — discrimination is by shape, which cannot
/// collide: the three `kind` vocabularies are disjoint
/// (`stop|tool_blocked|error` vs `progress|artifact|question` vs
/// `post_compact|session_start`), a note lacks `raw_hook_payload` while the
/// others lack `body`, and an unbound line is the only one carrying the
/// `unbound` marker while lacking `panel_id`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SignalEvent {
    /// A turn ended ([`TurnSignal`]).
    Turn(TurnSignal),
    /// A mid-turn note ([`AgentNote`]).
    Note(AgentNote),
    /// A turn ended, but the posting hook does not know which panel it
    /// belongs to ([`UnboundSignal`]).
    Unbound(UnboundSignal),
    /// A session-lifecycle event ([`LifecycleSignal`]).
    Lifecycle(LifecycleSignal),
}

/// A turn-completion signal posted **without** panel identity
/// (`caucus signal post --discover`, `docs/design.md` §7.8).
///
/// The `CAUCUS_*` env the exact path relies on is inherited process state, and
/// Claude Code can move a live conversation into a process that inherited
/// nothing from the panel's PTY — the `claude daemon` re-hosts a session
/// (`--fork-session`) on auto-update restarts and crash recovery. The hook then
/// still fires, but knows neither the socket nor the panel. This shape carries
/// what the hook payload itself knows — Claude's own conversation id, the
/// transcript path, the cwd — and the *server* side resolves which panel (if
/// any) it belongs to (`Multiplexer::handle_unbound_signal`).
///
/// The poster broadcasts it to every live caucus signal socket and always
/// waits for one reply line per socket, so there is no `wants_reply` field:
/// an unbound signal implies it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnboundSignal {
    /// Wire discriminator for the untagged [`SignalEvent`]: its *presence*
    /// (combined with the absent `panel_id`) is what routes a line here, so it
    /// is a required field with no default.
    pub unbound: bool,
    pub ts: DateTime<Utc>,
    pub kind: TurnKind,
    /// Claude Code's own conversation id, lifted from the hook payload's
    /// `session_id`. `None` when the payload carries none — such a signal can
    /// never be resolved to a panel.
    pub claude_session_id: Option<String>,
    /// Conversation transcript path, lifted from the payload (as on
    /// [`TurnSignal`]). Doubles as the lineage source when the conversation id
    /// is one caucus has never seen (a fork's fresh id).
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
    /// The agent process's working directory, lifted from the payload's
    /// `cwd`. Used to cheaply bound which caucus session could own the signal
    /// before any transcript is read.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// The agent's final assistant message, as on [`TurnSignal`].
    pub last_message: Option<String>,
    /// The raw Claude hook payload, retained verbatim — and re-used by
    /// `record_turn_completed` to heal the manifest's conversation id.
    pub raw_hook_payload: serde_json::Value,
}

impl UnboundSignal {
    /// Construct an unbound signal stamped with `Utc::now()`, lifting the
    /// conversation id, transcript path, and cwd out of `raw_hook_payload` so
    /// a constructed signal can never disagree with its own payload.
    pub fn now(kind: TurnKind, last_message: Option<String>, raw_hook_payload: Value) -> Self {
        let claude_session_id = raw_hook_payload
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let transcript_path = extract_transcript_path(&raw_hook_payload);
        let cwd = raw_hook_payload
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        Self {
            unbound: true,
            ts: Utc::now(),
            kind,
            claude_session_id,
            transcript_path,
            cwd,
            last_message,
            raw_hook_payload,
        }
    }
}

/// What the runtime tells a waiting main-panel Stop hook to do
/// (`docs/design.md` §7.6): sent through the per-signal oneshot the server
/// pairs with a `wants_reply` turn signal, and serialised by the server as the
/// connection's [`SignalReply`] line.
///
/// There is deliberately no `Allow` variant. Every non-deliver path — the
/// panel is not main, no round is due, the human is mid-compose, the runtime
/// is gone — expresses itself by *dropping the sender*, which the server turns
/// into [`SignalReply::Allow`]. Allow is the absence of a directive, so no
/// code path can forget to send it.
#[derive(Debug)]
pub enum StopDirective {
    /// Deliver `reason` into the main worker's continuing turn: the hook
    /// prints Claude's Stop-hook block JSON (`{"decision":"block","reason":…}`),
    /// and Claude receives `reason` as feedback in the same turn.
    Deliver {
        /// The round summary to inject (`Multiplexer::take_due_round_summary`).
        reason: String,
    },
}

/// One reply line on the signal socket, server → `caucus signal post`
/// (`docs/design.md` §7.6). Internally tagged by `action` so a future action
/// this binary does not know fails the parse — and the client treats a failed
/// parse as allow (fail-open).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SignalReply {
    /// Nothing to inject: the hook exits silently and the turn ends normally.
    Allow,
    /// Print the Stop-hook block JSON so `reason` continues the turn.
    Deliver { reason: String },
}

/// One parsed signal-socket event plus its reply slot. The sender is `Some`
/// only for a turn signal that asked for a reply (`TurnSignal::wants_reply`);
/// the server holds the matching receiver and answers the connection with
/// whatever arrives — or [`SignalReply::Allow`] when the sender is dropped or
/// the wait times out.
pub type SignalEnvelope = (
    SignalEvent,
    Option<tokio::sync::oneshot::Sender<StopDirective>>,
);

/// Lift the conversation-transcript path out of a hook payload. Claude Code's
/// Stop hook documents the `transcript_path` field; codex's notify JSON has no
/// counterpart, so codex signals carry `None`.
fn extract_transcript_path(payload: &serde_json::Value) -> Option<PathBuf> {
    payload
        .get("transcript_path")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_lifts_transcript_path_from_payload() {
        let signal = TurnSignal::now(
            SessionId::new(),
            PanelId::new(),
            TurnKind::Stop,
            None,
            serde_json::json!({ "transcript_path": "/logs/conv.jsonl" }),
        );
        assert_eq!(
            signal.transcript_path.as_deref(),
            Some(std::path::Path::new("/logs/conv.jsonl"))
        );
    }

    #[test]
    fn now_without_transcript_path_is_none() {
        let signal = TurnSignal::now(
            SessionId::new(),
            PanelId::new(),
            TurnKind::Stop,
            None,
            serde_json::json!({ "last_message": "no path here" }),
        );
        assert_eq!(signal.transcript_path, None);
    }

    /// An oversized body is cut on a char boundary and marked. Multi-byte
    /// chars ("한" is 3 bytes; the cap is not a multiple of 3) straddle the
    /// cap, so a raw byte-index `truncate` would panic — the cut must back
    /// off to the boundary.
    #[test]
    fn truncated_caps_an_oversized_body_on_a_char_boundary() {
        let body = "한".repeat(NOTE_BODY_MAX_BYTES);
        let note =
            AgentNote::now(SessionId::new(), PanelId::new(), NoteKind::Progress, body).truncated();
        let kept = note
            .body
            .strip_suffix("… [truncated]")
            .expect("an over-cap body carries the truncation marker");
        assert!(kept.len() <= NOTE_BODY_MAX_BYTES);
        assert!(
            kept.chars().all(|c| c == '한'),
            "the cut must not split a char"
        );
    }

    /// A signal that does not ask for a reply serialises with no `wants_reply`
    /// key at all — byte-identical to the pre-reply wire — and a line without
    /// the key (an older binary) parses as `false`.
    #[test]
    fn wants_reply_false_is_absent_from_the_wire() {
        let signal = TurnSignal::now(
            SessionId::new(),
            PanelId::new(),
            TurnKind::Stop,
            None,
            serde_json::Value::Null,
        );
        let line = serde_json::to_string(&signal).unwrap();
        assert!(
            !line.contains("wants_reply"),
            "false must serialise to nothing: {line}"
        );

        let back: TurnSignal = serde_json::from_str(&line).unwrap();
        assert!(!back.wants_reply);
    }

    /// The reply line round-trips both actions, and an action this binary does
    /// not know fails the parse — which the client treats as allow.
    #[test]
    fn signal_reply_wire_forms() {
        let allow = serde_json::to_string(&SignalReply::Allow).unwrap();
        assert_eq!(allow, r#"{"action":"allow"}"#);
        let deliver = serde_json::to_string(&SignalReply::Deliver {
            reason: "round done".into(),
        })
        .unwrap();
        assert_eq!(deliver, r#"{"action":"deliver","reason":"round done"}"#);

        assert!(matches!(
            serde_json::from_str::<SignalReply>(&allow).unwrap(),
            SignalReply::Allow
        ));
        let SignalReply::Deliver { reason } = serde_json::from_str(&deliver).unwrap() else {
            panic!("deliver line must parse as Deliver");
        };
        assert_eq!(reason, "round done");

        assert!(
            serde_json::from_str::<SignalReply>(r#"{"action":"reticulate"}"#).is_err(),
            "an unknown action must fail the parse (client fails open to allow)"
        );
    }

    /// `UnboundSignal::now` lifts identity out of the payload itself, so a
    /// constructed signal can never disagree with the payload it carries.
    #[test]
    fn unbound_now_lifts_identity_from_payload() {
        let sig = UnboundSignal::now(
            TurnKind::Stop,
            Some("done".into()),
            serde_json::json!({
                "session_id": "conv-1",
                "transcript_path": "/logs/conv-1.jsonl",
                "cwd": "/repo",
            }),
        );
        assert!(sig.unbound);
        assert_eq!(sig.claude_session_id.as_deref(), Some("conv-1"));
        assert_eq!(
            sig.transcript_path.as_deref(),
            Some(std::path::Path::new("/logs/conv-1.jsonl"))
        );
        assert_eq!(sig.cwd.as_deref(), Some(std::path::Path::new("/repo")));
    }

    /// Untagged discrimination: an unbound line routes to `Unbound`, and the
    /// two historical shapes still route where they always did — the `unbound`
    /// marker is what separates a panel-less turn line from garbage.
    #[test]
    fn signal_event_discriminates_unbound_lines() {
        let unbound = UnboundSignal::now(TurnKind::Stop, None, serde_json::json!({}));
        let line = serde_json::to_string(&unbound).unwrap();
        assert!(matches!(
            serde_json::from_str::<SignalEvent>(&line).unwrap(),
            SignalEvent::Unbound(_)
        ));

        let turn = TurnSignal::now(
            SessionId::new(),
            PanelId::new(),
            TurnKind::Stop,
            None,
            serde_json::Value::Null,
        );
        let line = serde_json::to_string(&turn).unwrap();
        assert!(matches!(
            serde_json::from_str::<SignalEvent>(&line).unwrap(),
            SignalEvent::Turn(_)
        ));

        let note = AgentNote::now(
            SessionId::new(),
            PanelId::new(),
            NoteKind::Progress,
            "half done".into(),
        );
        let line = serde_json::to_string(&note).unwrap();
        assert!(matches!(
            serde_json::from_str::<SignalEvent>(&line).unwrap(),
            SignalEvent::Note(_)
        ));
    }

    #[test]
    fn truncated_leaves_a_body_within_the_cap_alone() {
        let note = AgentNote::now(
            SessionId::new(),
            PanelId::new(),
            NoteKind::Artifact,
            "src/report.md".into(),
        )
        .truncated();
        assert_eq!(note.body, "src/report.md");
    }
}
