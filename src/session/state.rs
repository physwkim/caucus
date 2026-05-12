//! Session state machine.
//!
//! **Invariant I-1** (see `docs/design.md` §12): the only legal way to move a
//! session between states is `Session::transition`. Direct field mutation is
//! prevented by the field being `pub(crate)` and `Session::new`'s constructor
//! enforcing `Created` as the entry state.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Session-level state machine. See `docs/design.md` §3.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Created,
    MeetingInProgress,
    MeetingConverged,
    MeetingDeadlocked,
    Executing,
    ExecutionBlocked,
    Reviewing,
    Merged,
    Abandoned,
}

impl SessionState {
    /// True if the state is terminal — no further transition is allowed.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Merged | Self::Abandoned)
    }
}

/// Errors raised by an illegal state transition.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum TransitionError {
    #[error("illegal transition {from:?} -> {to:?}")]
    Illegal {
        from: SessionState,
        to: SessionState,
    },
    #[error("session is terminal in {from:?}; no transition allowed")]
    Terminal { from: SessionState },
}

/// The single owner of state transitions. Other modules emit events; the
/// orchestrator interprets them and calls `transition`. Any other path to a
/// new state is a bug.
pub fn transition(from: SessionState, to: SessionState) -> Result<SessionState, TransitionError> {
    if from.is_terminal() {
        return Err(TransitionError::Terminal { from });
    }
    if is_legal(from, to) {
        Ok(to)
    } else {
        Err(TransitionError::Illegal { from, to })
    }
}

const fn is_legal(from: SessionState, to: SessionState) -> bool {
    use SessionState::*;
    match (from, to) {
        // Initial path
        (Created, MeetingInProgress) => true,

        // Meeting outcomes
        (MeetingInProgress, MeetingConverged) => true,
        (MeetingInProgress, MeetingDeadlocked) => true,

        // From deadlock — escalate (abandon) or explore (still treated as
        // executing because each option spawns its own worktree).
        (MeetingDeadlocked, Abandoned) => true,
        (MeetingDeadlocked, Executing) => true,

        // Converged → execute
        (MeetingConverged, Executing) => true,
        (MeetingConverged, Abandoned) => true,

        // Execution
        (Executing, ExecutionBlocked) => true,
        (Executing, Reviewing) => true,
        (Executing, Abandoned) => true,

        // Blocked → unblock back into execution, or give up.
        (ExecutionBlocked, Executing) => true,
        (ExecutionBlocked, Abandoned) => true,

        // Review
        (Reviewing, Merged) => true,
        (Reviewing, Executing) => true, // request_changes → another execute pass
        (Reviewing, Abandoned) => true,

        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::SessionState::*;
    use super::*;

    #[test]
    fn legal_meeting_path() {
        let s = transition(Created, MeetingInProgress).unwrap();
        let s = transition(s, MeetingConverged).unwrap();
        let s = transition(s, Executing).unwrap();
        let s = transition(s, Reviewing).unwrap();
        let s = transition(s, Merged).unwrap();
        assert!(s.is_terminal());
    }

    #[test]
    fn legal_deadlock_then_abandon() {
        let s = transition(Created, MeetingInProgress).unwrap();
        let s = transition(s, MeetingDeadlocked).unwrap();
        let s = transition(s, Abandoned).unwrap();
        assert!(s.is_terminal());
    }

    #[test]
    fn legal_review_request_changes_loops_to_executing() {
        let s = transition(Created, MeetingInProgress).unwrap();
        let s = transition(s, MeetingConverged).unwrap();
        let s = transition(s, Executing).unwrap();
        let s = transition(s, Reviewing).unwrap();
        let s = transition(s, Executing).unwrap();
        assert_eq!(s, Executing);
    }

    #[test]
    fn illegal_skip_meeting() {
        let err = transition(Created, Executing).unwrap_err();
        assert!(matches!(err, TransitionError::Illegal { .. }));
    }

    #[test]
    fn illegal_after_terminal() {
        let err = transition(Merged, Executing).unwrap_err();
        assert!(matches!(err, TransitionError::Terminal { .. }));
    }

    #[test]
    fn illegal_meeting_to_merged() {
        let err = transition(MeetingInProgress, Merged).unwrap_err();
        assert!(matches!(err, TransitionError::Illegal { .. }));
    }
}
