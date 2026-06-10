//! Persistent session record — the on-disk shape that powers `caucus resume`
//! (`docs/design.md` §3, §10).
//!
//! caucus sessions are otherwise ephemeral: when caucus exits the agent
//! processes die. Per-agent manifests persist under `agents/`, but there is no
//! single file describing the *roster* — which roles, in which order, with
//! which CLI/model/worktree/conversation-id. [`SessionRecord`] is that file:
//! the [`crate::session::Multiplexer`] writes `<session_root>/session.json`
//! whenever the roster changes, and `caucus resume` reads it back to recreate
//! the panels.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::render::LayoutMode;
use crate::role::spec::AgentCli;

use super::id::SessionId;

/// File name of the session record under a session root.
pub const SESSION_RECORD_FILE: &str = "session.json";

/// One panel as captured for resume — enough to re-spawn it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PanelRecord {
    /// Role name (`architect`, `backend`, ...).
    pub role: String,
    /// Backend CLI the panel ran (`claude` / `codex`).
    pub agent_cli: AgentCli,
    /// Model override, if any.
    #[serde(default)]
    pub model: Option<String>,
    /// Position in the panel/focus-cycle order — panels resume in this order.
    pub order_index: usize,
    /// Whether this panel was the main worker. Persisted independently from
    /// `order_index` so moving panels in the UI cannot reassign the main role
    /// on resume. Old records omit this and fall back to `order_index == 0`.
    #[serde(default)]
    pub is_main: bool,
    /// Git branch of the panel's worktree, if it had one. The branch persists
    /// across shutdown (it holds the agent's commits); resume re-attaches a
    /// worktree on it via `crate::worktree::manager::attach`.
    #[serde(default)]
    pub worktree_branch: Option<String>,
    /// Claude Code conversation id, when the agent emitted one. `claude
    /// --resume <id>` continues the conversation on the resume launch path.
    #[serde(default)]
    pub claude_session_id: Option<String>,
}

/// The persistent record of one caucus session — written to
/// `<session_root>/session.json` and read back by `caucus resume`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionRecord {
    pub id: SessionId,
    /// Free-form topic the team was convened around.
    pub topic: String,
    /// Git repository the session was launched in.
    pub repo_path: PathBuf,
    pub created_at: DateTime<Utc>,
    /// Panel arrangement mode at the time of the write.
    #[serde(default)]
    pub layout_mode: LayoutMode,
    /// Panels, in `order_index` order.
    #[serde(default)]
    pub panels: Vec<PanelRecord>,
}

/// Errors reading or writing a [`SessionRecord`].
#[derive(Debug, thiserror::Error)]
pub enum SessionRecordError {
    #[error("session record io: {0}")]
    Io(#[from] std::io::Error),
    #[error("session record json: {0}")]
    Json(#[from] serde_json::Error),
}

impl SessionRecord {
    /// `<session_root>/session.json`.
    pub fn path(session_root: &Path) -> PathBuf {
        session_root.join(SESSION_RECORD_FILE)
    }

    /// Atomically write the record to `<session_root>/session.json`.
    ///
    /// Single owner of session-record persistence: the multiplexer calls this
    /// whenever the roster changes. Write-to-temp + rename so a relaunch never
    /// reads a half-written file.
    pub fn write(&self, session_root: &Path) -> Result<(), SessionRecordError> {
        std::fs::create_dir_all(session_root)?;
        let path = Self::path(session_root);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Read a record from `<session_root>/session.json`.
    pub fn read(session_root: &Path) -> Result<Self, SessionRecordError> {
        let bytes = std::fs::read(Self::path(session_root))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Look up one session's record by id under `<repo>/.caucus/sessions/`.
    pub fn read_for_id(repo: &Path, id: SessionId) -> Result<Self, SessionRecordError> {
        Self::read(&session_root(repo, id))
    }
}

/// `<repo>/.caucus/sessions/`.
fn sessions_dir(repo: &Path) -> PathBuf {
    repo.join(".caucus").join("sessions")
}

/// `<repo>/.caucus/sessions/<id>/` — the on-disk root of one session. The
/// single place the per-session path layout is spelled, so `caucus gc` and the
/// resume path agree on where a session's state lives.
pub fn session_root(repo: &Path, id: SessionId) -> PathBuf {
    sessions_dir(repo).join(id.to_string())
}

/// Scan `<repo>/.caucus/sessions/*/session.json` for resumable sessions,
/// newest first (`created_at` descending). Directories without a parseable
/// `session.json` are skipped — a corrupt or partial record is not resumable.
pub fn discover(repo: &Path) -> Vec<SessionRecord> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(sessions_dir(repo)) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        match SessionRecord::read(&path) {
            Ok(record) => found.push(record),
            // A directory without a `session.json` is simply not a session
            // dir — silent. A present-but-unreadable record is corruption the
            // user is silently losing on `resume`; surface it so they know
            // that session was dropped, not absent.
            Err(SessionRecordError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                warn!(dir = %path.display(), error = %err, "skipping unreadable session record");
            }
        }
    }
    found.sort_by_key(|r| std::cmp::Reverse(r.created_at));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SessionRecord {
        SessionRecord {
            id: SessionId::new(),
            topic: "ship resume".into(),
            repo_path: PathBuf::from("/repo"),
            created_at: Utc::now(),
            layout_mode: LayoutMode::MainVertical,
            panels: vec![
                PanelRecord {
                    role: "main".into(),
                    agent_cli: AgentCli::Claude,
                    model: None,
                    order_index: 0,
                    is_main: true,
                    worktree_branch: None,
                    claude_session_id: Some("conv-abc".into()),
                },
                PanelRecord {
                    role: "backend".into(),
                    agent_cli: AgentCli::Codex,
                    model: Some("gpt-5".into()),
                    order_index: 1,
                    is_main: false,
                    worktree_branch: Some("caucus/s1/backend".into()),
                    claude_session_id: None,
                },
            ],
        }
    }

    #[test]
    fn session_record_round_trips_through_json() {
        let rec = sample();
        let json = serde_json::to_string_pretty(&rec).unwrap();
        let back: SessionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec, back);
    }

    #[test]
    fn write_then_read_round_trips_on_disk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rec = sample();
        rec.write(tmp.path()).unwrap();
        assert!(SessionRecord::path(tmp.path()).is_file());
        let back = SessionRecord::read(tmp.path()).unwrap();
        assert_eq!(rec, back);
    }

    /// Old on-disk JSON without the newer optional fields still parses —
    /// `layout_mode`, `panels`, and the per-panel optionals are serde-default.
    #[test]
    fn minimal_json_parses_via_serde_defaults() {
        let id = SessionId::new();
        let json = serde_json::json!({
            "id": id,
            "topic": "legacy",
            "repo_path": "/repo",
            "created_at": "2026-05-16T00:00:00Z",
        })
        .to_string();
        let rec: SessionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec.id, id);
        assert_eq!(rec.layout_mode, LayoutMode::Tiled);
        assert!(rec.panels.is_empty());
    }

    /// `discover` finds written `session.json` files and skips a directory
    /// whose record is corrupt.
    #[test]
    fn discover_finds_written_records_and_skips_corrupt() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path();

        let rec = sample();
        let root = repo
            .join(".caucus")
            .join("sessions")
            .join(rec.id.to_string());
        rec.write(&root).unwrap();

        // A sibling directory with a corrupt record — must be skipped, not
        // panic the scan.
        let corrupt = repo.join(".caucus").join("sessions").join("garbage");
        std::fs::create_dir_all(&corrupt).unwrap();
        std::fs::write(corrupt.join(SESSION_RECORD_FILE), b"{not json").unwrap();

        let found = discover(repo);
        assert_eq!(found.len(), 1, "only the valid record is discoverable");
        assert_eq!(found[0].id, rec.id);
    }

    /// A `PanelRecord` written without the optional fields still parses.
    #[test]
    fn minimal_panel_record_parses() {
        let json = serde_json::json!({
            "role": "reviewer",
            "agent_cli": "claude",
            "order_index": 2,
        })
        .to_string();
        let pr: PanelRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(pr.role, "reviewer");
        assert_eq!(pr.order_index, 2);
        assert!(!pr.is_main);
        assert!(pr.model.is_none());
        assert!(pr.worktree_branch.is_none());
        assert!(pr.claude_session_id.is_none());
    }
}
