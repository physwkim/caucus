//! Session state machine (`docs/design.md` §3).
//!
//! A session is one caucus multiplexer instance — the set of panels running
//! around one topic. The state surface is intentionally tiny: a session is
//! either `Active` or `Closed`.
//!
//! **Invariant I-1** (`docs/design.md` §12): every session state transition
//! goes through [`transition`]. The `Session.state` field is `pub(crate)`;
//! external crates cannot mutate it.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::id::SessionId;

/// Coarse session lifecycle. Real lifecycle lives at the panel level
/// (`panel::lifecycle`); the session is just the container.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Panels are being spawned/killed, rounds run, execution happens — all
    /// inside this state.
    Active,
    /// Every panel has exited or the user quit caucus.
    Closed,
}

/// One caucus multiplexer instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    /// Free-form topic the team is convened around.
    pub topic: String,
    /// Authoritative state. Mutated only by [`transition`].
    pub(crate) state: SessionState,
    /// Git repository the session was launched in.
    pub repo_path: PathBuf,
    /// `<repo>/.caucus/sessions/<session_id>/` — session-scoped storage root.
    pub root_dir: PathBuf,
    pub created_at: DateTime<Utc>,
}

impl Session {
    /// Allocate a fresh `Active` session rooted at `repo_path`.
    pub fn new(topic: impl Into<String>, repo_path: PathBuf) -> Self {
        let id = SessionId::new();
        let root_dir = repo_path
            .join(".caucus")
            .join("sessions")
            .join(id.to_string());
        Self {
            id,
            topic: topic.into(),
            state: SessionState::Active,
            repo_path,
            root_dir,
            created_at: Utc::now(),
        }
    }

    /// Rebuild a session from a persisted [`super::record::SessionRecord`] —
    /// the `caucus resume` path. Reuses the original id, topic, repo, and
    /// `created_at` so the session root (`.caucus/sessions/<id>/`) and its
    /// `session.json` continue in place; the session is `Active` again.
    pub fn from_record(record: &super::record::SessionRecord) -> Self {
        let root_dir = record
            .repo_path
            .join(".caucus")
            .join("sessions")
            .join(record.id.to_string());
        Self {
            id: record.id,
            topic: record.topic.clone(),
            state: SessionState::Active,
            repo_path: record.repo_path.clone(),
            root_dir,
            created_at: record.created_at,
        }
    }

    /// Current state. Read-only accessor; mutation goes through [`transition`].
    pub fn state(&self) -> SessionState {
        self.state
    }
}

/// Rejected transition.
#[derive(Debug, Error)]
#[error("illegal session transition: {from:?} -> {to:?}")]
pub struct IllegalTransition {
    pub from: SessionState,
    pub to: SessionState,
}

/// Single owner of session state transitions (Invariant I-1).
///
/// The only legal transition is `Active -> Closed`. `Closed -> *` and the
/// no-op `Active -> Active` are rejected.
pub(crate) fn transition(
    session: &mut Session,
    to: SessionState,
) -> Result<(), IllegalTransition> {
    let from = session.state;
    match (from, to) {
        (SessionState::Active, SessionState::Closed) => {
            session.state = to;
            Ok(())
        }
        _ => Err(IllegalTransition { from, to }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_is_active() {
        let s = Session::new("topic", PathBuf::from("/repo"));
        assert_eq!(s.state(), SessionState::Active);
    }

    #[test]
    fn active_to_closed_is_legal() {
        let mut s = Session::new("topic", PathBuf::from("/repo"));
        transition(&mut s, SessionState::Closed).unwrap();
        assert_eq!(s.state(), SessionState::Closed);
    }

    #[test]
    fn closed_to_active_is_rejected() {
        let mut s = Session::new("topic", PathBuf::from("/repo"));
        transition(&mut s, SessionState::Closed).unwrap();
        assert!(transition(&mut s, SessionState::Active).is_err());
    }
}
