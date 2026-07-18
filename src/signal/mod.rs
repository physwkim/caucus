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

/// One parsed line off the signal socket: the historical turn signal, or a
/// mid-turn note. Untagged so each client's wire line stays exactly what it
/// already posts — discrimination is by shape, which cannot collide: the two
/// `kind` vocabularies are disjoint (`stop|tool_blocked|error` vs
/// `progress|artifact|question`) and each requires a field the other lacks
/// (`raw_hook_payload` vs `body`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SignalEvent {
    /// A turn ended ([`TurnSignal`]).
    Turn(TurnSignal),
    /// A mid-turn note ([`AgentNote`]).
    Note(AgentNote),
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
