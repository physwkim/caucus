//! `AgentManifest` — the single authoritative record per agent
//! (`docs/design.md` §8.1). Persisted as a JSON file plus a sibling Markdown
//! view.
//!
//! **Invariant I-2** (`docs/design.md` §12): all manifest mutation — LaneEvent
//! append or status change — goes through [`write`]. External code does not
//! write the JSON directly.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::role::spec::AgentCli;
use crate::session::id::{AgentId, PanelId, SessionId};

use super::derive_state::DerivedState;
use super::lane_event::{LaneEvent, LaneEventBlocker, LaneEventKind};

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
/// reads via the accessors and mutates only through [`write`].
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
}

impl AgentManifest {
    /// Allocate a fresh manifest in the `Live` / `Working` initial state.
    /// Persist it via [`write`].
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
        }
    }

    /// Read-only view of the lane-event timeline.
    pub fn lane_events(&self) -> &[LaneEvent] {
        &self.lane_events
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

/// Atomic on-disk write of the JSON + Markdown pair. Module-private — callers
/// go through [`write`].
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
        LaneEventKind::Blocked { blocker } => {
            format!("blocked ({:?}: {})", blocker.failure_class, blocker.detail)
        }
        LaneEventKind::Failed { blocker } => {
            format!("failed ({:?}: {})", blocker.failure_class, blocker.detail)
        }
        LaneEventKind::Finished { detail } => format!("finished ({detail})"),
        LaneEventKind::CommitCreated { provenance } => {
            format!("commit_created ({})", provenance.commit)
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
