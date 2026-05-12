//! Persistent record for a session: id, topic, state, round number, role
//! roster, sub-agent ids. Lives at `<session_root>/session.json` and is
//! written atomically.
//!
//! **Invariant I-1** (`docs/design.md` §12): mutation of `state` goes through
//! `Session::transition`; mutation of the roster goes through
//! `Session::register_agent` / `Session::deregister_agent`. Direct field
//! access from outside this module is via `pub(crate)` only — the public
//! surface is the named methods.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::id::{AgentId, SessionId};
use super::state::{SessionState, TransitionError, transition};

/// The on-disk session record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub topic: String,
    pub repo_root: PathBuf,
    /// Resolved session root: `<repo>/.caucus/sessions/<id>/`.
    pub session_root: PathBuf,
    pub state: SessionState,
    pub max_rounds: u32,
    /// 0 before any round starts, 1..N during meeting, N at converge time.
    pub current_round: u32,
    /// Roles configured for this session, in declaration order.
    pub roles: Vec<String>,
    /// (role, agent_id) pairs registered with this session — meeting agents
    /// and execute agents alike.
    pub agents: Vec<(String, AgentId)>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Session {
    /// Allocate a fresh session in `Created` state. Caller persists via
    /// [`write_session`] before doing anything visible.
    pub fn new(repo_root: PathBuf, topic: String, roles: Vec<String>, max_rounds: u32) -> Self {
        let id = SessionId::new();
        let session_root = repo_root
            .join(".caucus")
            .join("sessions")
            .join(id.to_string());
        let now = Utc::now();
        Self {
            id,
            topic,
            repo_root,
            session_root,
            state: SessionState::Created,
            max_rounds,
            current_round: 0,
            roles,
            agents: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Try to move the session to `to`. On success, `state` and
    /// `updated_at` are updated and `Ok(())` is returned; the caller must
    /// persist via [`write_session`].
    pub fn transition(&mut self, to: SessionState) -> Result<(), TransitionError> {
        let next = transition(self.state, to)?;
        self.state = next;
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Increment the round counter. Errors if `max_rounds` would be exceeded.
    pub fn advance_round(&mut self) -> Result<u32, SessionRecordError> {
        if self.current_round >= self.max_rounds {
            return Err(SessionRecordError::MaxRoundsExhausted {
                max: self.max_rounds,
            });
        }
        self.current_round += 1;
        self.updated_at = Utc::now();
        Ok(self.current_round)
    }

    pub fn register_agent(&mut self, role: &str, agent: AgentId) {
        self.agents.push((role.to_string(), agent));
        self.updated_at = Utc::now();
    }

    pub fn deregister_agent(&mut self, agent: AgentId) {
        self.agents.retain(|(_, a)| *a != agent);
        self.updated_at = Utc::now();
    }

    pub fn meeting_agents(&self) -> Vec<(String, AgentId)> {
        self.agents.clone()
    }

    pub fn json_path(&self) -> PathBuf {
        self.session_root.join("session.json")
    }
}

#[derive(Debug, Error)]
pub enum SessionRecordError {
    #[error("session io ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("session json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("round counter is exhausted (max_rounds = {max})")]
    MaxRoundsExhausted { max: u32 },
    #[error(transparent)]
    Transition(#[from] TransitionError),
}

/// Atomic write to `<session_root>/session.json`.
pub fn write_session(session: &Session) -> Result<PathBuf, SessionRecordError> {
    let path = session.json_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| SessionRecordError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(session)?;
    std::fs::write(&tmp, &bytes).map_err(|source| SessionRecordError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, &path).map_err(|source| SessionRecordError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// Read a session by id under a repo root. Useful for `caucus session show`.
pub fn read_session(repo_root: &Path, id: SessionId) -> Result<Session, SessionRecordError> {
    let session_root = repo_root
        .join(".caucus")
        .join("sessions")
        .join(id.to_string());
    let path = session_root.join("session.json");
    let bytes = std::fs::read(&path).map_err(|source| SessionRecordError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// List session ids under a repo root, sorted lex-asc (which is also time
/// order for ULIDs).
pub fn list_sessions(repo_root: &Path) -> Result<Vec<SessionId>, SessionRecordError> {
    let root = repo_root.join(".caucus").join("sessions");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(&root).map_err(|source| SessionRecordError::Io {
        path: root.clone(),
        source,
    })? {
        let entry = entry.map_err(|source| SessionRecordError::Io {
            path: root.clone(),
            source,
        })?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            if let Ok(id) = name.parse::<SessionId>() {
                ids.push(id);
            }
        }
    }
    ids.sort();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_advance_caps_at_max() {
        let mut s = Session::new(PathBuf::from("/r"), "t".into(), vec!["a".into()], 2);
        assert_eq!(s.advance_round().unwrap(), 1);
        assert_eq!(s.advance_round().unwrap(), 2);
        assert!(matches!(
            s.advance_round().unwrap_err(),
            SessionRecordError::MaxRoundsExhausted { .. }
        ));
    }

    #[test]
    fn transition_updates_state_and_timestamp() {
        let mut s = Session::new(PathBuf::from("/r"), "t".into(), vec![], 5);
        let t0 = s.updated_at;
        std::thread::sleep(std::time::Duration::from_millis(2));
        s.transition(SessionState::MeetingInProgress).unwrap();
        assert_eq!(s.state, SessionState::MeetingInProgress);
        assert!(s.updated_at > t0);
    }

    #[test]
    fn write_then_read_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut s = Session::new(
            tmp.path().to_path_buf(),
            "demo".into(),
            vec!["architect".into(), "reviewer".into()],
            3,
        );
        s.transition(SessionState::MeetingInProgress).unwrap();
        write_session(&s).unwrap();

        let back = read_session(tmp.path(), s.id).unwrap();
        assert_eq!(back.id, s.id);
        assert_eq!(back.state, SessionState::MeetingInProgress);
        assert_eq!(back.roles, vec!["architect", "reviewer"]);
    }

    #[test]
    fn list_sessions_excludes_garbage_dirs() {
        let tmp = TempDir::new().unwrap();
        let s = Session::new(tmp.path().to_path_buf(), "t".into(), vec![], 1);
        write_session(&s).unwrap();

        // A non-ULID-named sibling dir should be ignored.
        std::fs::create_dir_all(tmp.path().join(".caucus").join("sessions").join("scratch"))
            .unwrap();
        // A stray file should be ignored.
        std::fs::write(
            tmp.path()
                .join(".caucus")
                .join("sessions")
                .join("not_a_dir.txt"),
            "hi",
        )
        .unwrap();

        let ids = list_sessions(tmp.path()).unwrap();
        assert_eq!(ids, vec![s.id]);
    }

    #[test]
    fn register_and_deregister_agent() {
        let mut s = Session::new(PathBuf::from("/r"), "t".into(), vec![], 1);
        let a = AgentId::new();
        let b = AgentId::new();
        s.register_agent("backend", a);
        s.register_agent("reviewer", b);
        assert_eq!(s.agents.len(), 2);
        s.deregister_agent(a);
        assert_eq!(s.agents, vec![("reviewer".into(), b)]);
    }
}
