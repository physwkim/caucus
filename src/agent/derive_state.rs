//! Derived agent state (`docs/design.md` §8.3, §8.4).
//!
//! A coarse-grained state surface the CEO inspects via `list_panels`, computed
//! from `(status, last turn signal, error, blocker, grid hint)`. Recomputed on
//! every turn-signal ingest and on grid changes.

use serde::{Deserialize, Serialize};

use super::lane_event::LaneEventBlocker;
use crate::signal::TurnSignal;

/// Coarse state surface for the CEO (`docs/design.md` §8.3).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivedState {
    /// After `PromptDelivered`, before the next turn signal.
    Working,
    /// Turn signal received — waiting for the next instruction.
    Idle,
    /// Grid shows a tool-permission `[y/n]` prompt. caucus never auto-answers.
    BlockedPermissionPrompt,
    /// Grid shows a merge conflict.
    BlockedMergeConflict,
    /// Grid shows a stuck background job.
    BlockedBackgroundJob,
    /// MCP handshake degraded.
    DegradedMcp,
    /// The transport to the agent was interrupted.
    InterruptedTransport,
    /// The agent process exited.
    Exited,
}

/// Hint extracted from a panel's grid by the regex fallback in `term/`.
/// `None` means caucus has no opinion. Used for backends without a
/// turn-completion hook, and for blocked-state detection (`docs/design.md`
/// §8.3).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GridHint {
    /// The agent is back at a bare input prompt (turn likely done).
    PromptReady,
    /// A `Allow this tool? [y/n]`-style permission prompt is visible.
    PermissionPromptVisible,
    /// A merge-conflict marker is visible.
    MergeConflictVisible,
    /// A long-running background job appears stuck.
    BackgroundJobVisible,
}

/// Pure function deriving the coarse state (`docs/design.md` §8.4).
///
/// `status` is the raw agent status string from the manifest
/// (`live` / `exited` / `failed`). A visible blocking grid hint dominates;
/// otherwise a received turn signal means `Idle`, and `live` with no signal
/// means `Working`.
pub fn derive_agent_state(
    status: &str,
    last_turn_signal: Option<&TurnSignal>,
    error: Option<&str>,
    blocker: Option<&LaneEventBlocker>,
    grid_hint: Option<&GridHint>,
) -> DerivedState {
    // TODO(phase 2): full classification per §8.3 — fold `error` and
    // `blocker` into the blocked variants, weigh hook vs grid heuristics.
    let _ = (error, blocker);

    if let Some(hint) = grid_hint {
        match hint {
            GridHint::PermissionPromptVisible => return DerivedState::BlockedPermissionPrompt,
            GridHint::MergeConflictVisible => return DerivedState::BlockedMergeConflict,
            GridHint::BackgroundJobVisible => return DerivedState::BlockedBackgroundJob,
            GridHint::PromptReady => {}
        }
    }

    match status {
        "exited" => DerivedState::Exited,
        _ if last_turn_signal.is_some() => DerivedState::Idle,
        _ => DerivedState::Working,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_with_no_signal_is_working() {
        assert_eq!(
            derive_agent_state("live", None, None, None, None),
            DerivedState::Working
        );
    }

    #[test]
    fn exited_status_is_exited() {
        assert_eq!(
            derive_agent_state("exited", None, None, None, None),
            DerivedState::Exited
        );
    }

    #[test]
    fn permission_prompt_hint_dominates() {
        assert_eq!(
            derive_agent_state(
                "live",
                None,
                None,
                None,
                Some(&GridHint::PermissionPromptVisible)
            ),
            DerivedState::BlockedPermissionPrompt
        );
    }
}
