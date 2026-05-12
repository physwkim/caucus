//! 8-state derived view over `(status, response_file_present, error, blocker,
//! pane_hint)`. Modelled on claw-code's `derive_agent_state` (see
//! `docs/claw-code-analysis.md` §4.2), with one caucus-specific addition:
//! `BlockedPermissionPrompt` for the tmux case where the spawned `claude`
//! pane is stuck on a tool-permission y/n prompt.

use serde::{Deserialize, Serialize};

/// Coarse-grained state surface for the orchestrator. The CEO inspects this,
/// not the raw `(status, error, blocker)` tuple.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivedState {
    Working,
    FinishedCleanable,
    FinishedPendingReport,
    BlockedBackgroundJob,
    BlockedMergeConflict,
    BlockedPermissionPrompt,
    DegradedMcp,
    InterruptedTransport,
    TrulyIdle,
}

/// Raw agent status, persisted in the manifest.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawStatus {
    Running,
    Completed,
    Failed,
}

/// Hint extracted from the pane screen by the regex fallback in
/// `crate::status::pane_hint`. `None` means we have no opinion.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneScreenHint {
    EscToInterruptVisible,
    PermissionPromptVisible,
    BareOpenPrompt,
}

/// Pure function: every input combination produces one `DerivedState`.
/// Called on every sentinel ingest and every poller tick.
pub fn derive(
    status: RawStatus,
    response_file_non_empty: bool,
    error: Option<&str>,
    pane_hint: Option<PaneScreenHint>,
) -> DerivedState {
    if let Some(PaneScreenHint::PermissionPromptVisible) = pane_hint {
        return DerivedState::BlockedPermissionPrompt;
    }

    match status {
        RawStatus::Running => DerivedState::Working,
        RawStatus::Completed => {
            if response_file_non_empty {
                DerivedState::FinishedCleanable
            } else {
                DerivedState::FinishedPendingReport
            }
        }
        RawStatus::Failed => classify_failure(error.unwrap_or_default()),
    }
}

/// Classify a failure message string into a `DerivedState`. Matches
/// claw-code's substring heuristics so error wording is interchangeable.
pub fn classify_failure(error: &str) -> DerivedState {
    let e = error.to_ascii_lowercase();
    if e.contains("background") {
        return DerivedState::BlockedBackgroundJob;
    }
    if e.contains("merge conflict") || e.contains("cherry-pick") {
        return DerivedState::BlockedMergeConflict;
    }
    if e.contains("mcp") {
        return DerivedState::DegradedMcp;
    }
    if e.contains("transport")
        || e.contains("broken pipe")
        || e.contains("connection")
        || e.contains("interrupted")
    {
        return DerivedState::InterruptedTransport;
    }
    DerivedState::TrulyIdle
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_is_working() {
        assert_eq!(
            derive(RawStatus::Running, false, None, None),
            DerivedState::Working
        );
    }

    #[test]
    fn completed_with_report_is_cleanable() {
        assert_eq!(
            derive(RawStatus::Completed, true, None, None),
            DerivedState::FinishedCleanable
        );
    }

    #[test]
    fn completed_without_report_is_pending() {
        assert_eq!(
            derive(RawStatus::Completed, false, None, None),
            DerivedState::FinishedPendingReport
        );
    }

    #[test]
    fn merge_conflict_classified() {
        assert_eq!(
            derive(
                RawStatus::Failed,
                false,
                Some("merge conflict while rebasing"),
                None
            ),
            DerivedState::BlockedMergeConflict
        );
    }

    #[test]
    fn mcp_handshake_classified() {
        assert_eq!(
            derive(
                RawStatus::Failed,
                false,
                Some("mcp handshake timed out"),
                None
            ),
            DerivedState::DegradedMcp
        );
    }

    #[test]
    fn permission_prompt_dominates_running() {
        // Even if we still think the agent is running, a visible permission
        // prompt is the more informative signal — surface it.
        assert_eq!(
            derive(
                RawStatus::Running,
                false,
                None,
                Some(PaneScreenHint::PermissionPromptVisible)
            ),
            DerivedState::BlockedPermissionPrompt
        );
    }

    #[test]
    fn unknown_failure_falls_back_to_truly_idle() {
        assert_eq!(
            derive(RawStatus::Failed, false, Some("weird thing"), None),
            DerivedState::TrulyIdle
        );
    }
}
