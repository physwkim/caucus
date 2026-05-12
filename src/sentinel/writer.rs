//! Atomic writer for sentinel JSON files.
//!
//! Invocation paths:
//!
//! 1. Claude `Stop` hook → `caucus sentinel write --session ID --agent ID
//!    --kind stop` → `crate::cli::sentinel::write_subcommand`.
//! 2. caucus itself, when it needs to fabricate a terminal sentinel (e.g.
//!    `caucus agent kill` records `kind = "killed"`).
//!
//! Both call into [`write_sentinel`]. The write is atomic: serialise into a
//! sibling `.tmp` file, then `rename(2)` over the final path. This ensures
//! the watcher in `sentinel::watcher` never observes a half-written JSON.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::session::id::{AgentId, SessionId};

/// Discriminant for [`Sentinel::kind`]. The string values are the contract
/// with the Claude hook script — keep them stable.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SentinelKind {
    /// Claude finished its turn cleanly (the most common case).
    Stop,
    /// Claude refused a tool the orchestrator did not allowlist.
    ToolBlocked,
    /// An unrecoverable Claude error before any meaningful output.
    Error,
    /// caucus-side cancellation (e.g. `caucus agent kill`).
    Killed,
}

/// On-disk sentinel payload. Serialised pretty-printed for human inspection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sentinel {
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub ts: DateTime<Utc>,
    pub kind: SentinelKind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub raw_hook_payload: Option<serde_json::Value>,
}

impl Sentinel {
    pub fn new(
        session_id: SessionId,
        agent_id: AgentId,
        kind: SentinelKind,
        last_message: Option<String>,
        raw_hook_payload: Option<serde_json::Value>,
    ) -> Self {
        Self {
            agent_id,
            session_id,
            ts: Utc::now(),
            kind,
            last_message,
            raw_hook_payload,
        }
    }
}

/// Where the sentinel lives on disk:
/// `<session_root>/agents/<agent_id>.sentinel.json`.
pub fn sentinel_path(session_root: &Path, agent_id: AgentId) -> PathBuf {
    session_root
        .join("agents")
        .join(format!("{agent_id}.sentinel.json"))
}

#[derive(Debug, Error)]
pub enum SentinelError {
    #[error("sentinel io ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("sentinel json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Atomic write. Creates parent directories as needed. Overwrites any
/// previous sentinel at that path.
pub fn write_sentinel(session_root: &Path, sentinel: &Sentinel) -> Result<PathBuf, SentinelError> {
    let path = sentinel_path(session_root, sentinel.agent_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| SentinelError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let bytes = serde_json::to_vec_pretty(sentinel)?;
    let tmp = path.with_extension("sentinel.json.tmp");
    std::fs::write(&tmp, &bytes).map_err(|source| SentinelError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, &path).map_err(|source| SentinelError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// Read a sentinel from disk.
pub fn read_sentinel(session_root: &Path, agent_id: AgentId) -> Result<Sentinel, SentinelError> {
    let path = sentinel_path(session_root, agent_id);
    let bytes = std::fs::read(&path).map_err(|source| SentinelError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_then_read_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let session = SessionId::new();
        let agent = AgentId::new();
        let s = Sentinel::new(
            session,
            agent,
            SentinelKind::Stop,
            Some("done".into()),
            None,
        );
        let written = write_sentinel(tmp.path(), &s).unwrap();
        assert_eq!(written, sentinel_path(tmp.path(), agent));
        let back = read_sentinel(tmp.path(), agent).unwrap();
        assert_eq!(back.agent_id, agent);
        assert_eq!(back.kind, SentinelKind::Stop);
        assert_eq!(back.last_message.as_deref(), Some("done"));
    }

    #[test]
    fn temporary_file_is_cleaned_up() {
        let tmp = TempDir::new().unwrap();
        let s = Sentinel::new(
            SessionId::new(),
            AgentId::new(),
            SentinelKind::ToolBlocked,
            None,
            None,
        );
        let path = write_sentinel(tmp.path(), &s).unwrap();
        let leftover = path.with_extension("sentinel.json.tmp");
        // `rename(2)` semantics: the source no longer exists after success.
        assert!(!leftover.exists());
    }

    #[test]
    fn second_write_replaces_first() {
        let tmp = TempDir::new().unwrap();
        let session = SessionId::new();
        let agent = AgentId::new();
        let first = Sentinel::new(session, agent, SentinelKind::Stop, Some("a".into()), None);
        write_sentinel(tmp.path(), &first).unwrap();
        let second = Sentinel::new(session, agent, SentinelKind::Error, Some("b".into()), None);
        write_sentinel(tmp.path(), &second).unwrap();
        let back = read_sentinel(tmp.path(), agent).unwrap();
        assert_eq!(back.kind, SentinelKind::Error);
        assert_eq!(back.last_message.as_deref(), Some("b"));
    }

    #[test]
    fn schema_keeps_kind_tag() {
        let s = Sentinel::new(
            SessionId::new(),
            AgentId::new(),
            SentinelKind::Stop,
            None,
            None,
        );
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"kind\":\"stop\""));
    }
}
