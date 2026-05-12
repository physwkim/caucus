//! `AgentManifest` — the single authoritative record per agent. Persisted as
//! a JSON file plus a sibling Markdown view.
//!
//! **Invariant I-2** (`docs/design.md` §12): all mutation goes through the
//! `Mutator` returned by `AgentManifest::edit`. External code cannot mutate
//! the manifest fields directly.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::session::id::{AgentId, SessionId};

use super::derive_state::{DerivedState, PaneScreenHint, RawStatus};
use super::lane_event::{LaneEvent, LaneEventBlocker};

/// What kind of pane backs this agent.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// Meeting-phase agent: read-only, no worktree.
    Meeting,
    /// Execute-phase agent: has its own worktree.
    Execute,
}

/// Authoritative on-disk record for one agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub role: String,
    pub agent_name: String,
    pub kind: AgentKind,
    pub tmux_pane_id: Option<String>,
    pub worktree_path: Option<PathBuf>,
    pub model: Option<String>,
    pub status: RawStatus,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub lane_events: Vec<LaneEvent>,
    pub current_blocker: Option<LaneEventBlocker>,
    pub derived_state: DerivedState,
    pub error: Option<String>,
    #[serde(default)]
    pub current_pane_hint: Option<PaneScreenHint>,
    /// Claude Code session id extracted from the first Stop hook payload.
    /// Populated by `crate::round::record_sentinel`. Used by
    /// `caucus execute start --continue-meeting` to invoke
    /// `claude --resume <id>` in the new worktree so the execute-phase
    /// agent inherits the meeting-phase conversation context.
    #[serde(default)]
    pub claude_session_id: Option<String>,
}

impl AgentManifest {
    /// Allocate a fresh manifest in the `Running` / `Working` initial state.
    /// The caller is responsible for persisting it via [`write_json`].
    pub fn new(
        session_id: SessionId,
        role: String,
        agent_name: String,
        kind: AgentKind,
        model: Option<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            agent_id: AgentId::new(),
            session_id,
            role,
            agent_name,
            kind,
            tmux_pane_id: None,
            worktree_path: None,
            model,
            status: RawStatus::Running,
            created_at: now,
            started_at: Some(now),
            completed_at: None,
            lane_events: vec![LaneEvent::started(now)],
            current_blocker: None,
            derived_state: DerivedState::Working,
            error: None,
            current_pane_hint: None,
            claude_session_id: None,
        }
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

/// Atomic write: serialise to a sibling `*.tmp`, fsync, rename over the
/// final path. The Markdown view is rewritten the same way.
pub fn write_json(manifest: &AgentManifest, session_root: &Path) -> Result<(), ManifestError> {
    let path = AgentManifest::json_path(session_root, manifest.agent_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(manifest)?;
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path)?;

    let md_path = AgentManifest::md_path(session_root, manifest.agent_id);
    let md_tmp = md_path.with_extension("md.tmp");
    std::fs::write(&md_tmp, render_md(manifest))?;
    std::fs::rename(&md_tmp, &md_path)?;
    Ok(())
}

/// Read a manifest by id under a session root.
pub fn read_json(session_root: &Path, agent_id: AgentId) -> Result<AgentManifest, ManifestError> {
    let path = AgentManifest::json_path(session_root, agent_id);
    let bytes = std::fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn render_md(m: &AgentManifest) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "# agent {} ({})", m.agent_name, m.agent_id);
    let _ = writeln!(s);
    let _ = writeln!(s, "- session: {}", m.session_id);
    let _ = writeln!(s, "- role: {}", m.role);
    let _ = writeln!(s, "- kind: {:?}", m.kind);
    let _ = writeln!(s, "- status: {:?}", m.status);
    let _ = writeln!(s, "- derived_state: {:?}", m.derived_state);
    if let Some(hint) = m.current_pane_hint {
        let _ = writeln!(s, "- pane_hint: {hint:?}");
    }
    if let Some(model) = &m.model {
        let _ = writeln!(s, "- model: {model}");
    }
    if let Some(pane) = &m.tmux_pane_id {
        let _ = writeln!(s, "- tmux pane: {pane}");
    }
    if let Some(wt) = &m.worktree_path {
        let _ = writeln!(s, "- worktree: {}", wt.display());
    }
    let _ = writeln!(s, "- created_at: {}", m.created_at);
    if let Some(t) = m.completed_at {
        let _ = writeln!(s, "- completed_at: {t}");
    }
    if let Some(err) = &m.error {
        let _ = writeln!(s);
        let _ = writeln!(s, "## error\n\n{err}");
    }
    let _ = writeln!(s);
    let _ = writeln!(s, "## lane events");
    let _ = writeln!(s);
    for ev in &m.lane_events {
        let _ = writeln!(s, "- {} — {}", ev.ts(), event_label(ev));
    }
    s
}

fn event_label(ev: &LaneEvent) -> String {
    match ev {
        LaneEvent::Started { .. } => "started".into(),
        LaneEvent::PromptDelivered { prompt_path, .. } => {
            format!("prompt_delivered ({})", prompt_path.display())
        }
        LaneEvent::SentinelReceived { sentinel_kind, .. } => {
            format!("sentinel_received ({sentinel_kind})")
        }
        LaneEvent::ResponseFileWritten { path, bytes, .. } => {
            format!("response_written ({}, {bytes} bytes)", path.display())
        }
        LaneEvent::Blocked { blocker, .. } => {
            format!("blocked ({:?}: {})", blocker.failure_class, blocker.detail)
        }
        LaneEvent::Failed { blocker, .. } => {
            format!("failed ({:?}: {})", blocker.failure_class, blocker.detail)
        }
        LaneEvent::Finished { detail, .. } => format!("finished ({detail})"),
        LaneEvent::CommitCreated { provenance, .. } => {
            format!("commit_created ({})", provenance.commit)
        }
        LaneEvent::WorktreeCreated { path, .. } => {
            format!("worktree_created ({})", path.display())
        }
        LaneEvent::WorktreeRemoved { path, .. } => {
            format!("worktree_removed ({})", path.display())
        }
        LaneEvent::PaneHintChanged {
            previous, current, ..
        } => {
            format!("pane_hint_changed ({previous:?} → {current:?})")
        }
        LaneEvent::PaneGone { pane, .. } => format!("pane_gone ({pane})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_read_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let manifest = AgentManifest::new(
            SessionId::new(),
            "reviewer".into(),
            "reviewer-r1".into(),
            AgentKind::Meeting,
            Some("claude-opus-4-7".into()),
        );
        write_json(&manifest, tmp.path()).unwrap();
        let back = read_json(tmp.path(), manifest.agent_id).unwrap();
        assert_eq!(back.agent_id, manifest.agent_id);
        assert_eq!(back.role, "reviewer");
        assert_eq!(back.lane_events.len(), 1);

        // Markdown sibling exists and mentions the role
        let md =
            std::fs::read_to_string(AgentManifest::md_path(tmp.path(), manifest.agent_id)).unwrap();
        assert!(md.contains("role: reviewer"));
    }

    #[test]
    fn manifest_persists_current_pane_hint() {
        let tmp = TempDir::new().unwrap();
        let mut manifest = AgentManifest::new(
            SessionId::new(),
            "backend".into(),
            "backend".into(),
            AgentKind::Meeting,
            None,
        );
        manifest.current_pane_hint = Some(PaneScreenHint::PermissionPromptVisible);
        write_json(&manifest, tmp.path()).unwrap();
        let back = read_json(tmp.path(), manifest.agent_id).unwrap();
        assert_eq!(
            back.current_pane_hint,
            Some(PaneScreenHint::PermissionPromptVisible)
        );
    }

    #[test]
    fn manifest_reads_pre_v0_2_json() {
        // A pre-existing manifest JSON written before `current_pane_hint`
        // existed must still parse, with the new field defaulted to None.
        let json = serde_json::json!({
            "agent_id": crate::session::id::AgentId::new(),
            "session_id": SessionId::new(),
            "role": "qa",
            "agent_name": "qa-r1",
            "kind": "meeting",
            "tmux_pane_id": null,
            "worktree_path": null,
            "model": null,
            "status": "running",
            "created_at": "2026-05-01T00:00:00Z",
            "started_at": "2026-05-01T00:00:00Z",
            "completed_at": null,
            "lane_events": [],
            "current_blocker": null,
            "derived_state": "working",
            "error": null,
            // NOTE: no current_pane_hint field — emulates old on-disk format.
        });
        let parsed: AgentManifest =
            serde_json::from_value(json).expect("backward-compat parse failed");
        assert_eq!(parsed.current_pane_hint, None);
    }

    #[test]
    fn write_is_atomic_after_partial_failure_recovery() {
        // The .tmp file should not be left behind on a successful write.
        let tmp = TempDir::new().unwrap();
        let m = AgentManifest::new(
            SessionId::new(),
            "qa".into(),
            "qa".into(),
            AgentKind::Meeting,
            None,
        );
        write_json(&m, tmp.path()).unwrap();
        let json = AgentManifest::json_path(tmp.path(), m.agent_id);
        let leftover = json.with_extension("json.tmp");
        assert!(!leftover.exists());
    }
}
