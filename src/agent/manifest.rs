//! `AgentManifest` — the single authoritative record per agent
//! (`docs/design.md` §8.1). Persisted as a JSON file plus a sibling Markdown
//! view.
//!
//! **Invariant I-2** (`docs/design.md` §12): all manifest mutation — LaneEvent
//! append or status change — goes through `write()`. External code does not
//! write the JSON directly.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::role::spec::AgentCli;
use crate::session::id::{AgentId, PanelId, SessionId};

use super::derive_state::DerivedState;
use super::lane_event::{LaneEvent, LaneEventBlocker, LaneEventKind};
use super::provenance::{LaneCommitProvenance, SupersededBy};

/// Raw agent status, persisted in the manifest. Coarser than `DerivedState`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Spawned, process running.
    Live,
    /// Process exited cleanly.
    Exited,
    /// Process failed.
    Failed,
}

impl AgentStatus {
    /// String form used by [`super::derive_state::derive_agent_state`].
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Exited => "exited",
            Self::Failed => "failed",
        }
    }
}

/// Authoritative on-disk record for one agent (`docs/design.md` §8.1).
///
/// Fields are `pub(crate)` so only this module can mutate them; external code
/// reads via the accessors and mutates only through `write()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub role: String,
    pub agent_name: String,
    pub panel_id: PanelId,
    pub agent_cli: AgentCli,
    pub worktree_path: Option<PathBuf>,
    pub model: Option<String>,
    pub(crate) status: AgentStatus,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub exited_at: Option<DateTime<Utc>>,
    pub(crate) lane_events: Vec<LaneEvent>,
    pub(crate) current_blocker: Option<LaneEventBlocker>,
    pub(crate) derived_state: DerivedState,
    pub(crate) error: Option<String>,
    /// The agent's final assistant message from its most recent turn signal
    /// (`docs/design.md` §7.4, §8.5). Backs `read_panel(mode=last_message)`.
    #[serde(default)]
    pub(crate) last_message: Option<String>,
    /// Claude Code's own conversation id, lifted from the Stop hook payload's
    /// `session_id` field. `claude --resume <id>` needs this to continue the
    /// agent's conversation after a caucus relaunch. `None` until the first
    /// turn signal carries one (or for non-claude backends).
    #[serde(default)]
    pub(crate) claude_session_id: Option<String>,
    /// Path of the agent's conversation transcript (JSONL), lifted from its
    /// turn signals (`TurnSignal::transcript_path`). Lets the main worker read
    /// the whole conversation from disk instead of scraping the terminal.
    /// Kept once learned: a later signal that omits it does not clear it.
    #[serde(default)]
    pub(crate) transcript_path: Option<PathBuf>,
    /// The agent's most recent mid-turn note (`caucus signal note`), rendered
    /// as `[kind] body`. The full history is in the lane events; this scalar
    /// is the display convenience `list_panels` surfaces, like `last_message`.
    #[serde(default)]
    pub(crate) last_note: Option<String>,
    /// The most recent desktop-notification text the panel's process emitted
    /// in-band (OSC 9 / 99 / 777, `docs/design.md` §7.7). Same shape as
    /// `last_note`: full history in the lane events, scalar for `list_panels`.
    #[serde(default)]
    pub(crate) last_notification: Option<String>,
}

impl AgentManifest {
    /// Allocate a fresh manifest in the `Live` / `Working` initial state.
    /// Persist it via `write()`.
    pub fn new(
        session_id: SessionId,
        panel_id: PanelId,
        role: impl Into<String>,
        agent_name: impl Into<String>,
        agent_cli: AgentCli,
        model: Option<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            agent_id: AgentId::new(),
            session_id,
            role: role.into(),
            agent_name: agent_name.into(),
            panel_id,
            agent_cli,
            worktree_path: None,
            model,
            status: AgentStatus::Live,
            created_at: now,
            started_at: Some(now),
            exited_at: None,
            lane_events: vec![LaneEvent::started(now)],
            current_blocker: None,
            derived_state: DerivedState::Working,
            error: None,
            last_message: None,
            claude_session_id: None,
            transcript_path: None,
            last_note: None,
            last_notification: None,
        }
    }

    /// The agent's final message from its most recent turn signal, if any
    /// (`docs/design.md` §7.4) — backs `read_panel(mode=last_message)`.
    pub fn last_message(&self) -> Option<&str> {
        self.last_message.as_deref()
    }

    /// Claude Code's conversation id for this agent, if a turn signal has
    /// carried one — what `claude --resume` needs to continue the session.
    pub fn claude_session_id(&self) -> Option<&str> {
        self.claude_session_id.as_deref()
    }

    /// Path of the agent's conversation transcript (JSONL), if a turn signal
    /// has carried one — surfaced by `list_panels` so the main worker can read
    /// the conversation directly.
    pub fn transcript_path(&self) -> Option<&Path> {
        self.transcript_path.as_deref()
    }

    /// The agent's most recent mid-turn note as `[kind] body`, if it has
    /// posted one (`caucus signal note`) — surfaced by `list_panels`.
    pub fn last_note(&self) -> Option<&str> {
        self.last_note.as_deref()
    }

    /// The most recent in-band desktop notification (OSC 9 / 99 / 777) the
    /// panel's process emitted, if any — surfaced by `list_panels`.
    pub fn last_notification(&self) -> Option<&str> {
        self.last_notification.as_deref()
    }

    /// Read-only view of the lane-event timeline.
    pub fn lane_events(&self) -> &[LaneEvent] {
        &self.lane_events
    }

    /// The commits this agent created that its branch still holds, oldest first.
    ///
    /// A commit's standing is *derived* from the append-only timeline rather
    /// than stored: `CommitCreated` announces it, a later `CommitSuperseded`
    /// retires it, and what survives is live. A commit re-announced after being
    /// superseded (the agent names the same sha again) does not resurrect — the
    /// branch is what decides, and the supersession is what the branch said.
    pub fn live_commits(&self) -> Vec<LaneCommitProvenance> {
        let superseded: Vec<&str> = self
            .lane_events
            .iter()
            .filter_map(|e| match &e.kind {
                LaneEventKind::CommitSuperseded { commit, .. } => Some(commit.as_str()),
                _ => None,
            })
            .collect();
        let mut live: Vec<LaneCommitProvenance> = Vec::new();
        for event in &self.lane_events {
            let LaneEventKind::CommitCreated { provenance } = &event.kind else {
                continue;
            };
            if superseded.contains(&provenance.commit.as_str())
                || live.iter().any(|p| p.commit == provenance.commit)
            {
                continue;
            }
            live.push(provenance.clone());
        }
        live
    }

    /// Current raw status.
    pub fn status(&self) -> AgentStatus {
        self.status
    }

    /// Current derived state.
    pub fn derived_state(&self) -> DerivedState {
        self.derived_state
    }

    /// JSON path under a session root: `<session_root>/agents/<id>.json`.
    pub fn json_path(session_root: &Path, agent_id: AgentId) -> PathBuf {
        session_root.join("agents").join(format!("{agent_id}.json"))
    }

    /// Markdown view path: same directory, `.md` extension.
    pub fn md_path(session_root: &Path, agent_id: AgentId) -> PathBuf {
        session_root.join("agents").join(format!("{agent_id}.md"))
    }
}

/// Errors surfaced while reading or writing a manifest.
#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("manifest io: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Single owner of manifest persistence (Invariant I-2).
///
/// Append a lane event then atomically rewrite the JSON + Markdown pair.
/// All manifest mutation routes through here.
pub(crate) fn write(
    manifest: &mut AgentManifest,
    session_root: &Path,
    event: Option<LaneEvent>,
) -> Result<(), ManifestError> {
    if let Some(event) = event {
        manifest.lane_events.push(event);
    }
    to_disk(manifest, session_root)
}

/// Record a turn-completion signal on the manifest (`docs/design.md` §7, §8.3).
///
/// Single owner of the `TurnCompleted` transition (Invariant I-2): appends a
/// `TurnCompleted` lane event, stores the signal's `last_message`, recomputes
/// `derived_state` via [`super::derive_state::derive_agent_state`], and
/// persists the JSON + Markdown pair.
///
/// A non-`Stop` signal kind (`tool_blocked` / `error`) is recorded as a
/// blocker so `derived_state` reflects it, *and* appended to the timeline as a
/// `Blocked` / `Failed` event — one `match` on the signal's kind produces both,
/// so the timeline and `current_blocker` cannot disagree about how the turn
/// ended. `TurnCompleted` is appended for every kind: the turn *did* end, which
/// is what the signal means and what the panel's turn counter counts. A plain
/// `Stop` clears any prior blocker and lands the agent in `Idle`.
pub(crate) fn record_turn_completed(
    manifest: &mut AgentManifest,
    session_root: &Path,
    signal: &crate::signal::TurnSignal,
) -> Result<(), ManifestError> {
    use super::lane_event::LaneFailureClass;
    use crate::signal::TurnKind;

    manifest
        .lane_events
        .push(LaneEvent::now(LaneEventKind::TurnCompleted));
    manifest.last_message = signal.last_message.clone();

    // Lift Claude Code's own conversation id from the Stop hook payload — it
    // is what `claude --resume <id>` needs to continue this agent after a
    // caucus relaunch. Absent for non-claude backends; keep any prior value
    // if a later signal omits it.
    if let Some(sid) = signal
        .raw_hook_payload
        .get("session_id")
        .and_then(serde_json::Value::as_str)
    {
        manifest.claude_session_id = Some(sid.to_string());
    }

    // Same keep-if-absent rule as the conversation id: the transcript path is
    // stable for the agent's lifetime, so a signal without one (codex, a
    // malformed payload) must not erase what an earlier signal taught us.
    if let Some(path) = &signal.transcript_path {
        manifest.transcript_path = Some(path.clone());
    }

    // A non-Stop turn signal carries a failure. One match produces both the
    // blocker `derived_state` reads and the timeline event that records it, so
    // the two cannot drift: a manifest showing a blocker always has the event
    // that explains it, and vice versa.
    let (blocker, event) = match signal.kind {
        TurnKind::Stop => (None, None),
        TurnKind::ToolBlocked => {
            let b = LaneEventBlocker::new(
                LaneFailureClass::PermissionPrompt,
                "turn signal: tool_blocked",
            );
            let ev = LaneEventKind::Blocked { blocker: b.clone() };
            (Some(b), Some(ev))
        }
        TurnKind::Error => {
            let b = LaneEventBlocker::new(LaneFailureClass::Transport, "turn signal: error");
            let ev = LaneEventKind::Failed { blocker: b.clone() };
            (Some(b), Some(ev))
        }
    };
    if let Some(event) = event {
        manifest.lane_events.push(LaneEvent::now(event));
    }
    manifest.current_blocker = blocker;

    manifest.derived_state = super::derive_state::derive_agent_state(
        manifest.status.as_str(),
        true, // the signal settles the turn
        manifest.error.as_deref(),
        manifest.current_blocker.as_ref(),
    );
    to_disk(manifest, session_root)
}

/// Record a local slash command (`/compact`, `/clear`) finishing in the panel
/// (`docs/design.md` §7, §8.3) — the settle owner for command phases that end
/// with **no turn signal**: a local builtin runs no agent turn, so no Stop hook
/// ever fires for it, and its completion arrives as a lifecycle signal instead.
/// The sibling of [`record_turn_completed`]: appends a `LocalCommandCompleted`
/// lane event, clears any stale blocker (the panel is at its input prompt), and
/// recomputes `derived_state` as settled — `Idle`, even when the panel has never
/// received a turn signal in this generation.
pub(crate) fn record_local_command_completed(
    manifest: &mut AgentManifest,
    session_root: &Path,
    command: &str,
) -> Result<(), ManifestError> {
    manifest
        .lane_events
        .push(LaneEvent::now(LaneEventKind::LocalCommandCompleted {
            command: command.to_string(),
        }));
    // The command finished at the input prompt, so no blocker can still be
    // live. (A prompt genuinely on screen is re-detected and overlaid at read
    // time by `list_panels`, so clearing here cannot hide one.)
    manifest.current_blocker = None;
    manifest.derived_state = super::derive_state::derive_agent_state(
        manifest.status.as_str(),
        true, // the lifecycle signal settles the command phase
        manifest.error.as_deref(),
        manifest.current_blocker.as_ref(),
    );
    to_disk(manifest, session_root)
}

/// Record a delivered prompt on the manifest — the turn-phase transition *into*
/// `Working` (`docs/design.md` §8.3). Single owner of the `PromptDelivered`
/// transition (Invariant I-2): appends the event, clears any blocker the new
/// prompt supersedes, recomputes `derived_state`, and persists — the mirror of
/// [`record_turn_completed`], which pins `Idle` once the turn's Stop hook fires.
///
/// A freshly delivered prompt has no turn signal yet, so `derive_agent_state`
/// yields `Working`. Without this recompute, `derived_state` — the value
/// `list_panels` reports to the main worker — would stay whatever the last turn
/// signal left it: the reason a resumed sub-panel, whose reloaded conversation
/// already fired its Stop hook (→ `Idle`), was reported `idle` no matter how
/// many prompts it was given. `PromptDelivered` was the sole turn-phase event
/// that appended through the generic [`write`] without recomputing state.
pub(crate) fn record_prompt_delivered(
    manifest: &mut AgentManifest,
    session_root: &Path,
) -> Result<(), ManifestError> {
    manifest
        .lane_events
        .push(LaneEvent::now(LaneEventKind::PromptDelivered));
    // A new prompt supersedes a stale blocker: `select_option` answering an
    // open menu resumes the turn, so the panel is `Working`, not blocked. (A
    // still-open live prompt is re-detected and overlaid by `list_panels`, so
    // clearing here cannot hide a menu that is genuinely still on screen.)
    manifest.current_blocker = None;
    manifest.derived_state = super::derive_state::derive_agent_state(
        manifest.status.as_str(),
        false, // no settle since this prompt — the turn is open again
        manifest.error.as_deref(),
        manifest.current_blocker.as_ref(),
    );
    to_disk(manifest, session_root)
}

/// Record a mid-turn [`crate::signal::AgentNote`] on the manifest: append a
/// `NoteRecorded` lane event and store the rendered note as `last_note`.
///
/// Deliberately NO state transition and no `derived_state` recompute — a note
/// arrives mid-turn; the turn is talking, not ending, so `Working` must
/// survive it. The caller (`Multiplexer::handle_note`) has already capped the
/// body (`AgentNote::truncated`).
pub(crate) fn record_note(
    manifest: &mut AgentManifest,
    session_root: &Path,
    note: &crate::signal::AgentNote,
) -> Result<(), ManifestError> {
    manifest.last_note = Some(format!("[{}] {}", note.kind.as_str(), note.body));
    write(
        manifest,
        session_root,
        Some(LaneEvent::now(LaneEventKind::NoteRecorded {
            note_kind: note.kind,
            body: note.body.clone(),
        })),
    )
}

/// Record an in-band desktop notification (OSC 9 / 99 / 777) the panel's
/// process emitted (`docs/design.md` §7.7): append a `NotificationSeen` lane
/// event and store the text as `last_notification`.
///
/// Like [`record_note`], deliberately NO state transition and no
/// `derived_state` recompute — capture only. Whether a notification may ever
/// hint turn settlement is D-2's decision, and would have to route through
/// [`record_turn_completed`], the turn-completion owner — never through here.
pub(crate) fn record_notification(
    manifest: &mut AgentManifest,
    session_root: &Path,
    body: &str,
) -> Result<(), ManifestError> {
    manifest.last_notification = Some(body.to_string());
    write(
        manifest,
        session_root,
        Some(LaneEvent::now(LaneEventKind::NotificationSeen {
            body: body.to_string(),
        })),
    )
}

/// Mark an agent's process as exited (`docs/design.md` §8.3).
///
/// Single owner of the terminal `Exited` transition: flips `status` to
/// `Exited`, stamps `exited_at`, recomputes `derived_state`, and persists.
pub(crate) fn record_exited(
    manifest: &mut AgentManifest,
    session_root: &Path,
) -> Result<(), ManifestError> {
    manifest.status = AgentStatus::Exited;
    manifest.exited_at = Some(Utc::now());
    manifest.derived_state = super::derive_state::derive_agent_state(
        manifest.status.as_str(),
        false,
        manifest.error.as_deref(),
        manifest.current_blocker.as_ref(),
    );
    to_disk(manifest, session_root)
}

/// Atomic on-disk write of the JSON + Markdown pair. Module-private — callers
/// go through `write()`.
fn to_disk(manifest: &AgentManifest, session_root: &Path) -> Result<(), ManifestError> {
    let path = AgentManifest::json_path(session_root, manifest.agent_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(manifest)?)?;
    std::fs::rename(&tmp, &path)?;

    let md_path = AgentManifest::md_path(session_root, manifest.agent_id);
    let md_tmp = md_path.with_extension("md.tmp");
    std::fs::write(&md_tmp, render_md(manifest))?;
    std::fs::rename(&md_tmp, &md_path)?;
    Ok(())
}

/// Read a manifest by id under a session root.
pub fn read(session_root: &Path, agent_id: AgentId) -> Result<AgentManifest, ManifestError> {
    let bytes = std::fs::read(AgentManifest::json_path(session_root, agent_id))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn render_md(m: &AgentManifest) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "# agent {} ({})", m.agent_name, m.agent_id);
    let _ = writeln!(s);
    let _ = writeln!(s, "- session: {}", m.session_id);
    let _ = writeln!(s, "- panel: {}", m.panel_id);
    let _ = writeln!(s, "- role: {}", m.role);
    let _ = writeln!(s, "- agent_cli: {:?}", m.agent_cli);
    let _ = writeln!(s, "- status: {:?}", m.status);
    let _ = writeln!(s, "- derived_state: {:?}", m.derived_state);
    if let Some(model) = &m.model {
        let _ = writeln!(s, "- model: {model}");
    }
    if let Some(wt) = &m.worktree_path {
        let _ = writeln!(s, "- worktree: {}", wt.display());
    }
    if let Some(t) = &m.transcript_path {
        let _ = writeln!(s, "- transcript: {}", t.display());
    }
    let _ = writeln!(s, "- created_at: {}", m.created_at);
    if let Some(t) = m.exited_at {
        let _ = writeln!(s, "- exited_at: {t}");
    }
    if let Some(err) = &m.error {
        let _ = writeln!(s, "\n## error\n\n{err}");
    }
    let _ = writeln!(s, "\n## lane events\n");
    for ev in &m.lane_events {
        let _ = writeln!(s, "- {} — {}", ev.ts, event_label(&ev.kind));
    }
    s
}

fn event_label(kind: &LaneEventKind) -> String {
    match kind {
        LaneEventKind::Started => "started".into(),
        LaneEventKind::PromptDelivered => "prompt_delivered".into(),
        LaneEventKind::TurnCompleted => "turn_completed".into(),
        LaneEventKind::LocalCommandCompleted { command } => {
            format!("local_command_completed ({command})")
        }
        LaneEventKind::Blocked { blocker } => {
            format!("blocked ({:?}: {})", blocker.failure_class, blocker.detail)
        }
        LaneEventKind::Failed { blocker } => {
            format!("failed ({:?}: {})", blocker.failure_class, blocker.detail)
        }
        LaneEventKind::CommitCreated { provenance } => {
            format!("commit_created ({})", provenance.commit)
        }
        LaneEventKind::CommitSuperseded { commit, by } => match by {
            SupersededBy::Commit { commit: by } => {
                format!("commit_superseded ({commit} -> {by})")
            }
            SupersededBy::Unknown => format!("commit_superseded ({commit} -> unknown)"),
        },
        LaneEventKind::NoteRecorded { note_kind, body } => {
            format!("note ({}: {})", note_kind.as_str(), body)
        }
        LaneEventKind::NotificationSeen { body } => {
            format!("notification ({body})")
        }
        LaneEventKind::WorktreeCreated { path } => {
            format!("worktree_created ({})", path.display())
        }
        LaneEventKind::WorktreeRemoved { path } => {
            format!("worktree_removed ({})", path.display())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_read_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "reviewer",
            "reviewer-r1",
            AgentCli::Claude,
            Some("opus".into()),
        );
        write(&mut manifest, tmp.path(), None).unwrap();
        let back = read(tmp.path(), manifest.agent_id).unwrap();
        assert_eq!(back.agent_id, manifest.agent_id);
        assert_eq!(back.role, "reviewer");
        assert_eq!(back.lane_events().len(), 1);

        let md =
            std::fs::read_to_string(AgentManifest::md_path(tmp.path(), manifest.agent_id)).unwrap();
        assert!(md.contains("role: reviewer"));
    }

    #[test]
    fn record_turn_completed_updates_state_and_message() {
        use crate::signal::{TurnKind, TurnSignal};
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "reviewer",
            "reviewer-1",
            AgentCli::Claude,
            None,
        );
        assert_eq!(manifest.derived_state(), DerivedState::Working);

        let signal = TurnSignal::now(
            manifest.session_id,
            manifest.panel_id,
            TurnKind::Stop,
            Some("review done".into()),
            serde_json::Value::Null,
        );
        record_turn_completed(&mut manifest, tmp.path(), &signal).unwrap();

        assert_eq!(manifest.derived_state(), DerivedState::Idle);
        assert_eq!(manifest.last_message(), Some("review done"));
        assert!(
            manifest
                .lane_events()
                .iter()
                .any(|e| matches!(e.kind, LaneEventKind::TurnCompleted))
        );
        // Persisted: a fresh read sees the same derived state.
        let back = read(tmp.path(), manifest.agent_id).unwrap();
        assert_eq!(back.derived_state(), DerivedState::Idle);
    }

    /// A delivered prompt reopens the turn: `derived_state` returns to `Working`
    /// even after a prior turn signal pinned it `Idle`, and the recompute is
    /// persisted. Regression — a resumed sub-panel whose reloaded conversation
    /// had already fired its Stop hook was reported `idle` to the main worker
    /// after every command, because `PromptDelivered` never recomputed state.
    #[test]
    fn record_prompt_delivered_reopens_the_turn_to_working() {
        use crate::signal::{TurnKind, TurnSignal};
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "reviewer",
            "reviewer-1",
            AgentCli::Claude,
            None,
        );
        // A completed turn pins Idle (the resume Stop-hook scenario).
        let signal = TurnSignal::now(
            manifest.session_id,
            manifest.panel_id,
            TurnKind::Stop,
            Some("prior turn".into()),
            serde_json::Value::Null,
        );
        record_turn_completed(&mut manifest, tmp.path(), &signal).unwrap();
        assert_eq!(manifest.derived_state(), DerivedState::Idle);

        // Delivering a prompt reopens the turn.
        record_prompt_delivered(&mut manifest, tmp.path()).unwrap();
        assert_eq!(manifest.derived_state(), DerivedState::Working);
        assert!(
            manifest
                .lane_events()
                .iter()
                .any(|e| matches!(e.kind, LaneEventKind::PromptDelivered))
        );
        // Persisted: a fresh read agrees, so `list_panels` reads `working` too.
        let back = read(tmp.path(), manifest.agent_id).unwrap();
        assert_eq!(back.derived_state(), DerivedState::Working);
    }

    /// A delivered prompt clears a stale blocker — `select_option` answering an
    /// open menu resumes the turn to `Working`, not the blocked state the prior
    /// signal recorded.
    #[test]
    fn record_prompt_delivered_clears_a_stale_blocker() {
        use crate::agent::lane_event::{LaneEventBlocker, LaneFailureClass};
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "reviewer",
            "reviewer-1",
            AgentCli::Claude,
            None,
        );
        // Stand the manifest up in a blocked state, as a `tool_blocked` signal
        // would have left it.
        manifest.current_blocker = Some(LaneEventBlocker::new(
            LaneFailureClass::PermissionPrompt,
            "y/n prompt",
        ));
        manifest.derived_state = crate::agent::derive_state::derive_agent_state(
            manifest.status.as_str(),
            false,
            None,
            manifest.current_blocker.as_ref(),
        );
        assert_ne!(manifest.derived_state(), DerivedState::Working);

        record_prompt_delivered(&mut manifest, tmp.path()).unwrap();
        assert!(
            manifest.current_blocker.is_none(),
            "the new prompt supersedes the blocker"
        );
        assert_eq!(manifest.derived_state(), DerivedState::Working);
    }

    /// A local slash command settles the command phase to `Idle` even when the
    /// manifest has **never** seen a turn signal — the boundary that forced
    /// `derive_agent_state`'s second parameter to a plain `turn_settled` bool:
    /// there is no `TurnSignal` to point at here, because a local builtin runs
    /// no agent turn. Blocker cleared, event on the timeline, persisted.
    #[test]
    fn record_local_command_completed_settles_without_any_turn_signal() {
        use crate::agent::lane_event::{LaneEventBlocker, LaneFailureClass};
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "reviewer",
            "reviewer-1",
            AgentCli::Claude,
            None,
        );
        // The wedge state: a prompt was delivered (the `/compact` submit),
        // no turn signal ever followed. Also plant a stale blocker to prove
        // the completion clears it.
        record_prompt_delivered(&mut manifest, tmp.path()).unwrap();
        manifest.current_blocker = Some(LaneEventBlocker::new(
            LaneFailureClass::PermissionPrompt,
            "stale",
        ));
        assert_eq!(manifest.derived_state(), DerivedState::Working);

        record_local_command_completed(&mut manifest, tmp.path(), "/compact").unwrap();

        assert_eq!(manifest.derived_state(), DerivedState::Idle);
        assert!(manifest.current_blocker.is_none());
        assert!(manifest.lane_events().iter().any(|e| matches!(
            &e.kind,
            LaneEventKind::LocalCommandCompleted { command } if command == "/compact"
        )));
        // Persisted: `list_panels` and a resumed session both read `idle`.
        let back = read(tmp.path(), manifest.agent_id).unwrap();
        assert_eq!(back.derived_state(), DerivedState::Idle);
    }

    /// A turn signal whose raw hook payload carries `session_id` lands that
    /// id on the manifest as `claude_session_id` — what `claude --resume`
    /// needs to continue the conversation.
    #[test]
    fn record_turn_completed_extracts_claude_session_id() {
        use crate::signal::{TurnKind, TurnSignal};
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "backend",
            "backend-1",
            AgentCli::Claude,
            None,
        );
        assert_eq!(manifest.claude_session_id(), None);

        let signal = TurnSignal::now(
            manifest.session_id,
            manifest.panel_id,
            TurnKind::Stop,
            Some("done".into()),
            serde_json::json!({ "session_id": "claude-conv-7b2", "cwd": "/repo" }),
        );
        record_turn_completed(&mut manifest, tmp.path(), &signal).unwrap();
        assert_eq!(manifest.claude_session_id(), Some("claude-conv-7b2"));

        // Persisted: a fresh read sees the same id.
        let back = read(tmp.path(), manifest.agent_id).unwrap();
        assert_eq!(back.claude_session_id(), Some("claude-conv-7b2"));
    }

    /// A turn signal without a `session_id` in its payload leaves the
    /// manifest's `claude_session_id` untouched (and prior values survive).
    #[test]
    fn record_turn_completed_without_session_id_keeps_prior() {
        use crate::signal::{TurnKind, TurnSignal};
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "backend",
            "backend-1",
            AgentCli::Claude,
            None,
        );
        manifest.claude_session_id = Some("kept-id".into());
        let signal = TurnSignal::now(
            manifest.session_id,
            manifest.panel_id,
            TurnKind::Stop,
            None,
            serde_json::Value::Null,
        );
        record_turn_completed(&mut manifest, tmp.path(), &signal).unwrap();
        assert_eq!(manifest.claude_session_id(), Some("kept-id"));
    }

    /// A turn signal whose payload carries `transcript_path` lands the path on
    /// the manifest — the whole lift chain, since `TurnSignal::now` extracts
    /// it from the payload.
    #[test]
    fn record_turn_completed_extracts_transcript_path() {
        use crate::signal::{TurnKind, TurnSignal};
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "backend",
            "backend-1",
            AgentCli::Claude,
            None,
        );
        assert_eq!(manifest.transcript_path(), None);

        let signal = TurnSignal::now(
            manifest.session_id,
            manifest.panel_id,
            TurnKind::Stop,
            Some("done".into()),
            serde_json::json!({ "transcript_path": "/logs/conv.jsonl" }),
        );
        record_turn_completed(&mut manifest, tmp.path(), &signal).unwrap();
        assert_eq!(
            manifest.transcript_path(),
            Some(Path::new("/logs/conv.jsonl"))
        );

        // Persisted: a fresh read sees the same path.
        let back = read(tmp.path(), manifest.agent_id).unwrap();
        assert_eq!(back.transcript_path(), Some(Path::new("/logs/conv.jsonl")));
    }

    /// A mid-turn note lands `last_note` plus a persisted `NoteRecorded` lane
    /// event — and changes no state: the note arrives mid-turn, the turn is
    /// talking rather than ending, so `status` and `derived_state` must
    /// survive it untouched.
    #[test]
    fn record_note_records_without_a_state_transition() {
        use crate::signal::{AgentNote, NoteKind};
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "backend",
            "backend-1",
            AgentCli::Claude,
            None,
        );
        let status_before = manifest.status;
        let derived_before = manifest.derived_state;

        let note = AgentNote::now(
            manifest.session_id,
            manifest.panel_id,
            NoteKind::Question,
            "which schema version?".into(),
        );
        record_note(&mut manifest, tmp.path(), &note).unwrap();

        assert_eq!(
            manifest.last_note(),
            Some("[question] which schema version?")
        );
        assert_eq!(manifest.status, status_before);
        assert_eq!(manifest.derived_state, derived_before);

        // Persisted: a fresh read sees the note and its lane event.
        let back = read(tmp.path(), manifest.agent_id).unwrap();
        assert_eq!(back.last_note(), Some("[question] which schema version?"));
        assert!(
            back.lane_events().iter().any(|e| matches!(
                &e.kind,
                LaneEventKind::NoteRecorded { note_kind: NoteKind::Question, body }
                    if body == "which schema version?"
            )),
            "the note is on the persisted timeline: {:?}",
            back.lane_events()
        );
    }

    /// An in-band notification lands `last_notification` plus a persisted
    /// `NotificationSeen` lane event, with no state transition.
    #[test]
    fn record_notification_records_without_a_state_transition() {
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "backend",
            "backend-1",
            AgentCli::Claude,
            None,
        );
        let status_before = manifest.status;
        let derived_before = manifest.derived_state;

        record_notification(&mut manifest, tmp.path(), "build finished").unwrap();

        assert_eq!(manifest.last_notification(), Some("build finished"));
        assert_eq!(manifest.status, status_before);
        assert_eq!(manifest.derived_state, derived_before);

        let back = read(tmp.path(), manifest.agent_id).unwrap();
        assert_eq!(back.last_notification(), Some("build finished"));
        assert!(
            back.lane_events().iter().any(|e| matches!(
                &e.kind,
                LaneEventKind::NotificationSeen { body } if body == "build finished"
            )),
            "the notification is on the persisted timeline: {:?}",
            back.lane_events()
        );
    }

    /// A turn signal without a transcript path (codex, malformed payload)
    /// leaves a previously learned path untouched.
    #[test]
    fn record_turn_completed_without_transcript_path_keeps_prior() {
        use crate::signal::{TurnKind, TurnSignal};
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "backend",
            "backend-1",
            AgentCli::Claude,
            None,
        );
        manifest.transcript_path = Some("/logs/kept.jsonl".into());
        let signal = TurnSignal::now(
            manifest.session_id,
            manifest.panel_id,
            TurnKind::Stop,
            None,
            serde_json::Value::Null,
        );
        record_turn_completed(&mut manifest, tmp.path(), &signal).unwrap();
        assert_eq!(
            manifest.transcript_path(),
            Some(Path::new("/logs/kept.jsonl"))
        );
    }

    #[test]
    fn record_turn_completed_error_kind_is_interrupted() {
        use crate::signal::{TurnKind, TurnSignal};
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "backend",
            "backend-1",
            AgentCli::Claude,
            None,
        );
        let signal = TurnSignal::now(
            manifest.session_id,
            manifest.panel_id,
            TurnKind::Error,
            None,
            serde_json::Value::Null,
        );
        record_turn_completed(&mut manifest, tmp.path(), &signal).unwrap();
        assert_eq!(manifest.derived_state(), DerivedState::InterruptedTransport);
    }

    /// A `tool_blocked` turn signal records a `PermissionPrompt` blocker and
    /// lands the agent in `BlockedPermissionPrompt` — the mid-turn permission
    /// stop, distinct from the `error` kind (transport-interrupted).
    #[test]
    fn record_turn_completed_tool_blocked_kind_is_blocked_permission_prompt() {
        use crate::agent::lane_event::LaneFailureClass;
        use crate::signal::{TurnKind, TurnSignal};
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "backend",
            "backend-1",
            AgentCli::Claude,
            None,
        );
        let signal = TurnSignal::now(
            manifest.session_id,
            manifest.panel_id,
            TurnKind::ToolBlocked,
            None,
            serde_json::Value::Null,
        );
        record_turn_completed(&mut manifest, tmp.path(), &signal).unwrap();
        assert_eq!(
            manifest.derived_state(),
            DerivedState::BlockedPermissionPrompt
        );
        let blocker = manifest
            .current_blocker
            .as_ref()
            .expect("a tool_blocked signal must record a blocker");
        assert_eq!(blocker.failure_class, LaneFailureClass::PermissionPrompt);
    }

    /// Each turn-signal kind lands its own event on the timeline, and the event
    /// carries the same blocker `current_blocker` holds — one `match` produces
    /// both, so a manifest showing a blocker always has the event that explains
    /// it. `TurnCompleted` is appended for every kind: the turn ended whichever
    /// way it went, and that is what the panel's turn counter counts.
    #[test]
    fn record_turn_completed_appends_an_event_per_turn_kind() {
        use crate::agent::lane_event::LaneFailureClass;
        use crate::signal::{TurnKind, TurnSignal};
        for kind in [TurnKind::Stop, TurnKind::ToolBlocked, TurnKind::Error] {
            let tmp = TempDir::new().unwrap();
            let mut manifest = AgentManifest::new(
                SessionId::new(),
                PanelId::new(),
                "backend",
                "backend-1",
                AgentCli::Claude,
                None,
            );
            let signal = TurnSignal::now(
                manifest.session_id,
                manifest.panel_id,
                kind,
                None,
                serde_json::Value::Null,
            );
            record_turn_completed(&mut manifest, tmp.path(), &signal).unwrap();

            let kinds: Vec<&LaneEventKind> =
                manifest.lane_events().iter().map(|e| &e.kind).collect();
            assert!(
                kinds
                    .iter()
                    .any(|k| matches!(k, LaneEventKind::TurnCompleted)),
                "{kind:?}: every turn signal ends a turn"
            );
            match kind {
                TurnKind::Stop => assert!(
                    !kinds.iter().any(|k| matches!(
                        k,
                        LaneEventKind::Blocked { .. } | LaneEventKind::Failed { .. }
                    )),
                    "a clean stop records no blocker event"
                ),
                TurnKind::ToolBlocked => assert!(
                    kinds.iter().any(|k| matches!(
                        k,
                        LaneEventKind::Blocked { blocker }
                            if blocker.failure_class == LaneFailureClass::PermissionPrompt
                    )),
                    "tool_blocked records a Blocked event carrying its blocker"
                ),
                TurnKind::Error => assert!(
                    kinds.iter().any(|k| matches!(
                        k,
                        LaneEventKind::Failed { blocker }
                            if blocker.failure_class == LaneFailureClass::Transport
                    )),
                    "error records a Failed event carrying its blocker"
                ),
            }
            // The event's blocker and `current_blocker` agree.
            assert_eq!(
                manifest.current_blocker.is_some(),
                kinds.iter().any(|k| matches!(
                    k,
                    LaneEventKind::Blocked { .. } | LaneEventKind::Failed { .. }
                )),
                "{kind:?}: blocker and its explaining event travel together"
            );
        }
    }

    /// `render_md` emits every optional field that is set (model / worktree /
    /// exited_at / error) and a label for each lane-event kind. Exercises the
    /// conditional branches and `event_label`'s formatting arms, which the
    /// roundtrip test (single field) did not reach.
    #[test]
    fn render_md_emits_optional_fields_and_every_event_label() {
        use crate::agent::lane_event::LaneFailureClass;
        use crate::agent::provenance::LaneCommitProvenance;
        let mut m = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "backend",
            "backend-1",
            AgentCli::Claude,
            Some("opus".into()),
        );
        m.worktree_path = Some(PathBuf::from("/tmp/wt/backend-1"));
        m.error = Some("pipe broke".into());
        m.status = AgentStatus::Exited;
        m.exited_at = Some(Utc::now());
        // One event of every kind beyond the `Started` that `new` seeds, so
        // every `event_label` arm renders.
        for kind in [
            LaneEventKind::PromptDelivered,
            LaneEventKind::TurnCompleted,
            LaneEventKind::Blocked {
                blocker: LaneEventBlocker::new(LaneFailureClass::PermissionPrompt, "Allow? [y/n]"),
            },
            LaneEventKind::Failed {
                blocker: LaneEventBlocker::new(LaneFailureClass::Transport, "pipe"),
            },
            LaneEventKind::CommitCreated {
                provenance: LaneCommitProvenance {
                    commit: "abc1234".into(),
                    branch: "feat/x".into(),
                    worktree: None,
                },
            },
            LaneEventKind::NoteRecorded {
                note_kind: crate::signal::NoteKind::Question,
                body: "which port should the mock bind?".into(),
            },
            LaneEventKind::WorktreeCreated {
                path: PathBuf::from("/tmp/wt/backend-1"),
            },
            LaneEventKind::WorktreeRemoved {
                path: PathBuf::from("/tmp/wt/backend-1"),
            },
        ] {
            m.lane_events.push(LaneEvent::now(kind));
        }

        let md = render_md(&m);
        // Optional field branches.
        for needle in [
            "- model: opus",
            "- worktree: /tmp/wt/backend-1",
            "- status: Exited",
            "- exited_at:",
            "## error",
            "pipe broke",
        ] {
            assert!(md.contains(needle), "render_md missing {needle:?}:\n{md}");
        }
        // Every event_label arm.
        for needle in [
            "started",
            "prompt_delivered",
            "turn_completed",
            "blocked (PermissionPrompt: Allow? [y/n])",
            "failed (Transport: pipe)",
            "commit_created (abc1234)",
            "note (question: which port should the mock bind?)",
            "worktree_created (/tmp/wt/backend-1)",
            "worktree_removed (/tmp/wt/backend-1)",
        ] {
            assert!(
                md.contains(needle),
                "render_md missing label {needle:?}:\n{md}"
            );
        }
    }

    #[test]
    fn record_exited_is_terminal() {
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "qa",
            "qa-1",
            AgentCli::Claude,
            None,
        );
        record_exited(&mut manifest, tmp.path()).unwrap();
        assert_eq!(manifest.status(), AgentStatus::Exited);
        assert_eq!(manifest.derived_state(), DerivedState::Exited);
        assert!(manifest.exited_at.is_some());
    }

    #[test]
    fn write_appends_lane_event() {
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "backend",
            "backend",
            AgentCli::Claude,
            None,
        );
        write(
            &mut manifest,
            tmp.path(),
            Some(LaneEvent::now(LaneEventKind::TurnCompleted)),
        )
        .unwrap();
        assert_eq!(manifest.lane_events().len(), 2);
        let back = read(tmp.path(), manifest.agent_id).unwrap();
        assert_eq!(back.lane_events().len(), 2);
    }
}
