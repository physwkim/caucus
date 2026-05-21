//! Derived agent state (`docs/design.md` §8.3, §8.4).
//!
//! A coarse-grained state surface the main worker inspects via `list_panels`, computed
//! from `(status, last turn signal, error, blocker, grid hint)`. Recomputed on
//! every turn-signal ingest and on grid changes.

use serde::{Deserialize, Serialize};

use super::lane_event::{LaneEventBlocker, LaneFailureClass};
use crate::signal::TurnSignal;

/// Coarse state surface for the main worker (`docs/design.md` §8.3).
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
    /// Grid shows an interactive selection menu (an `AskUserQuestion`-style
    /// chooser): the agent stopped mid-turn waiting for an option to be picked.
    /// No `Stop` hook fires here, so this is detected from the grid, not a
    /// turn signal. The main worker answers it with `select_option`.
    AwaitingSelection,
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
    /// An interactive selection menu (`AskUserQuestion`-style chooser) is
    /// visible — the agent is waiting for an option to be picked.
    SelectionMenuVisible,
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
    // Classification precedence (`docs/design.md` §8.3):
    //   1. `exited` status is terminal — overrides everything.
    //   2. A live `error` string puts the agent in a transport-interrupted
    //      state (the process is alive but its turn ended badly).
    //   3. A recorded `blocker` maps onto its blocked/degraded variant.
    //   4. A blocking grid hint (the heuristic fallback for backends without
    //      a turn-completion hook) maps the same way — lower-confidence than
    //      the hook-fed `blocker`, so it is weighed after it.
    //   5. Otherwise: a received turn signal means `Idle`, a live agent with
    //      no signal yet means `Working`.

    // `exited` is terminal regardless of any pending blocker or hint.
    if status == "exited" {
        return DerivedState::Exited;
    }

    if error.is_some() {
        return DerivedState::InterruptedTransport;
    }

    if let Some(blocker) = blocker {
        return blocker_state(blocker.failure_class);
    }

    if let Some(hint) = grid_hint {
        match hint {
            GridHint::PermissionPromptVisible => return DerivedState::BlockedPermissionPrompt,
            GridHint::SelectionMenuVisible => return DerivedState::AwaitingSelection,
            GridHint::MergeConflictVisible => return DerivedState::BlockedMergeConflict,
            GridHint::BackgroundJobVisible => return DerivedState::BlockedBackgroundJob,
            GridHint::PromptReady => {}
        }
    }

    match status {
        "failed" => DerivedState::InterruptedTransport,
        _ if last_turn_signal.is_some() => DerivedState::Idle,
        _ => DerivedState::Working,
    }
}

/// Map a blocker's failure class onto a `DerivedState` variant
/// (`docs/design.md` §8.3).
fn blocker_state(class: LaneFailureClass) -> DerivedState {
    match class {
        LaneFailureClass::PermissionPrompt => DerivedState::BlockedPermissionPrompt,
        LaneFailureClass::MergeConflict => DerivedState::BlockedMergeConflict,
        LaneFailureClass::BackgroundJob => DerivedState::BlockedBackgroundJob,
        LaneFailureClass::McpHandshake => DerivedState::DegradedMcp,
        LaneFailureClass::Transport => DerivedState::InterruptedTransport,
        // PromptDelivery / Unknown have no dedicated state surface; treat as
        // transport-interrupted so the main worker still sees a non-Idle panel.
        LaneFailureClass::PromptDelivery | LaneFailureClass::Unknown => {
            DerivedState::InterruptedTransport
        }
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

    #[test]
    fn selection_menu_hint_is_awaiting_selection() {
        // A visible selection menu means the agent stopped mid-turn for a
        // choice — distinct from a [y/n] permission prompt.
        assert_eq!(
            derive_agent_state(
                "live",
                None,
                None,
                None,
                Some(&GridHint::SelectionMenuVisible)
            ),
            DerivedState::AwaitingSelection
        );
    }

    #[test]
    fn turn_signal_with_no_blocker_is_idle() {
        let sig = TurnSignal::now(
            crate::session::id::SessionId::new(),
            crate::session::id::PanelId::new(),
            crate::signal::TurnKind::Stop,
            Some("done".into()),
            serde_json::Value::Null,
        );
        assert_eq!(
            derive_agent_state("live", Some(&sig), None, None, None),
            DerivedState::Idle
        );
    }

    #[test]
    fn error_string_is_interrupted_transport() {
        assert_eq!(
            derive_agent_state("live", None, Some("pipe broke"), None, None),
            DerivedState::InterruptedTransport
        );
    }

    #[test]
    fn blocker_maps_to_blocked_variant() {
        let merge = LaneEventBlocker::new(LaneFailureClass::MergeConflict, "conflict in foo.rs");
        assert_eq!(
            derive_agent_state("live", None, None, Some(&merge), None),
            DerivedState::BlockedMergeConflict
        );
        let mcp = LaneEventBlocker::new(LaneFailureClass::McpHandshake, "handshake failed");
        assert_eq!(
            derive_agent_state("live", None, None, Some(&mcp), None),
            DerivedState::DegradedMcp
        );
        let perm = LaneEventBlocker::new(LaneFailureClass::PermissionPrompt, "y/n");
        assert_eq!(
            derive_agent_state("live", None, None, Some(&perm), None),
            DerivedState::BlockedPermissionPrompt
        );
    }

    #[test]
    fn exited_dominates_blocker_and_hint() {
        let blk = LaneEventBlocker::new(LaneFailureClass::PermissionPrompt, "y/n");
        assert_eq!(
            derive_agent_state(
                "exited",
                None,
                Some("err"),
                Some(&blk),
                Some(&GridHint::PermissionPromptVisible),
            ),
            DerivedState::Exited
        );
    }

    #[test]
    fn blocker_outranks_grid_hint() {
        // Hook-fed blocker (merge conflict) beats a lower-confidence grid
        // hint (permission prompt).
        let blk = LaneEventBlocker::new(LaneFailureClass::MergeConflict, "conflict");
        assert_eq!(
            derive_agent_state(
                "live",
                None,
                None,
                Some(&blk),
                Some(&GridHint::PermissionPromptVisible),
            ),
            DerivedState::BlockedMergeConflict
        );
    }

    #[test]
    fn prompt_ready_hint_does_not_block() {
        assert_eq!(
            derive_agent_state("live", None, None, None, Some(&GridHint::PromptReady)),
            DerivedState::Working
        );
    }
}
