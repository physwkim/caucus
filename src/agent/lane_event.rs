//! Lane events: append-only timeline per agent. Mirrors claw-code's
//! `LaneEvent` enum (see `docs/claw-code-analysis.md` §4.1) and is the natural
//! unit the CEO syncs to Notion via its own MCP.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::provenance::LaneCommitProvenance;

/// Failure-class taxonomy for blockers, used by the orchestrator to choose a
/// retry strategy.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneFailureClass {
    PromptDelivery,
    TrustGate,
    MergeConflict,
    BranchStaleAgainstMain,
    McpHandshake,
    Transport,
    Unknown,
}

/// A blocker attached to a `Blocked` or `Failed` event.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaneEventBlocker {
    pub failure_class: LaneFailureClass,
    pub detail: String,
}

/// Discriminant for `LaneEvent`. Kept as a separate enum so that the JSON
/// representation has a stable `kind` field independent of payload changes.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaneEvent {
    Started {
        ts: DateTime<Utc>,
    },
    PromptDelivered {
        ts: DateTime<Utc>,
        prompt_path: PathBuf,
    },
    SentinelReceived {
        ts: DateTime<Utc>,
        sentinel_kind: String,
    },
    ResponseFileWritten {
        ts: DateTime<Utc>,
        path: PathBuf,
        bytes: u64,
    },
    Blocked {
        ts: DateTime<Utc>,
        blocker: LaneEventBlocker,
    },
    Failed {
        ts: DateTime<Utc>,
        blocker: LaneEventBlocker,
    },
    Finished {
        ts: DateTime<Utc>,
        detail: String,
    },
    CommitCreated {
        ts: DateTime<Utc>,
        provenance: LaneCommitProvenance,
    },
    WorktreeCreated {
        ts: DateTime<Utc>,
        path: PathBuf,
    },
    WorktreeRemoved {
        ts: DateTime<Utc>,
        path: PathBuf,
    },
}

impl LaneEvent {
    /// Convenience constructor for `Started`.
    pub fn started(ts: DateTime<Utc>) -> Self {
        Self::Started { ts }
    }

    /// Constructor that stamps the event with `Utc::now()`.
    pub fn started_now() -> Self {
        Self::started(Utc::now())
    }

    /// Wall-clock timestamp of this event.
    pub fn ts(&self) -> DateTime<Utc> {
        match self {
            Self::Started { ts }
            | Self::PromptDelivered { ts, .. }
            | Self::SentinelReceived { ts, .. }
            | Self::ResponseFileWritten { ts, .. }
            | Self::Blocked { ts, .. }
            | Self::Failed { ts, .. }
            | Self::Finished { ts, .. }
            | Self::CommitCreated { ts, .. }
            | Self::WorktreeCreated { ts, .. }
            | Self::WorktreeRemoved { ts, .. } => *ts,
        }
    }
}

/// Classify a free-form error message into a `LaneFailureClass`. Substring
/// heuristics from claw-code (`classify_lane_failure`); see
/// `docs/claw-code-analysis.md` §4.
pub fn classify_failure(error: &str) -> LaneFailureClass {
    let e = error.to_ascii_lowercase();
    if e.contains("prompt") && e.contains("deliver") {
        return LaneFailureClass::PromptDelivery;
    }
    if e.contains("trust") {
        return LaneFailureClass::TrustGate;
    }
    if e.contains("branch") && (e.contains("stale") || e.contains("diverg")) {
        return LaneFailureClass::BranchStaleAgainstMain;
    }
    if e.contains("merge conflict") || e.contains("cherry-pick") {
        return LaneFailureClass::MergeConflict;
    }
    if e.contains("mcp") {
        return LaneFailureClass::McpHandshake;
    }
    if e.contains("transport")
        || e.contains("broken pipe")
        || e.contains("connection")
        || e.contains("interrupted")
    {
        return LaneFailureClass::Transport;
    }
    LaneFailureClass::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_basics() {
        assert_eq!(
            classify_failure("merge conflict while rebasing"),
            LaneFailureClass::MergeConflict
        );
        assert_eq!(
            classify_failure("mcp handshake timed out"),
            LaneFailureClass::McpHandshake
        );
        assert_eq!(
            classify_failure("connection refused"),
            LaneFailureClass::Transport
        );
        assert_eq!(
            classify_failure("trust gate denied"),
            LaneFailureClass::TrustGate
        );
        assert_eq!(
            classify_failure("branch stale against main"),
            LaneFailureClass::BranchStaleAgainstMain
        );
        assert_eq!(classify_failure("???"), LaneFailureClass::Unknown);
    }

    #[test]
    fn event_ts_extraction() {
        let now = Utc::now();
        assert_eq!(LaneEvent::Started { ts: now }.ts(), now);
    }

    #[test]
    fn event_serde_roundtrip() {
        let ev = LaneEvent::Finished {
            ts: Utc::now(),
            detail: "ok".into(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: LaneEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
        assert!(s.contains("\"kind\":\"finished\""));
    }
}
