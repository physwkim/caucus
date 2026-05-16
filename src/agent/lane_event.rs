//! Lane events: append-only timeline per agent (`docs/design.md` §8.2).
//! "Lane" is claw-code's term for one agent's work flow.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::provenance::LaneCommitProvenance;

/// Failure-class taxonomy for blockers, used to choose a retry strategy.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneFailureClass {
    PromptDelivery,
    PermissionPrompt,
    MergeConflict,
    BackgroundJob,
    McpHandshake,
    Transport,
    Unknown,
}

/// A blocker attached to a `Blocked` or `Failed` lane event.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaneEventBlocker {
    pub failure_class: LaneFailureClass,
    pub detail: String,
}

impl LaneEventBlocker {
    pub fn new(failure_class: LaneFailureClass, detail: impl Into<String>) -> Self {
        Self {
            failure_class,
            detail: detail.into(),
        }
    }
}

/// Discriminant + payload for a lane event (`docs/design.md` §8.2).
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaneEventKind {
    /// Agent process started.
    Started,
    /// The CEO delivered an agenda to the panel via `send_keys`.
    PromptDelivered,
    /// A `Stop`-hook turn signal was received for this panel.
    TurnCompleted,
    /// The agent is blocked (recoverable) — see `blocker`.
    Blocked { blocker: LaneEventBlocker },
    /// The agent failed (terminal for this turn) — see `blocker`.
    Failed { blocker: LaneEventBlocker },
    /// The agent finished its delegated task.
    Finished { detail: String },
    /// A commit was created in the agent's worktree.
    CommitCreated { provenance: LaneCommitProvenance },
    /// A worktree was created for this agent.
    WorktreeCreated { path: PathBuf },
    /// The agent's worktree was removed.
    WorktreeRemoved { path: PathBuf },
}

/// One timestamped lane event.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaneEvent {
    pub kind: LaneEventKind,
    pub ts: DateTime<Utc>,
}

impl LaneEvent {
    /// Build an event stamped with `Utc::now()`.
    pub fn now(kind: LaneEventKind) -> Self {
        Self {
            kind,
            ts: Utc::now(),
        }
    }

    /// Build a `Started` event at `ts`.
    pub fn started(ts: DateTime<Utc>) -> Self {
        Self {
            kind: LaneEventKind::Started,
            ts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_completed_serde_roundtrip() {
        let ev = LaneEvent::now(LaneEventKind::TurnCompleted);
        let s = serde_json::to_string(&ev).unwrap();
        let back: LaneEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
        assert!(s.contains("\"kind\":\"turn_completed\""));
    }

    #[test]
    fn blocked_carries_blocker() {
        let ev = LaneEvent::now(LaneEventKind::Blocked {
            blocker: LaneEventBlocker::new(LaneFailureClass::PermissionPrompt, "y/n prompt"),
        });
        let s = serde_json::to_string(&ev).unwrap();
        let back: LaneEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }
}
