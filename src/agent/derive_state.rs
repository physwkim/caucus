//! Derived agent state (`docs/design.md` §8.3, §8.4).
//!
//! A coarse-grained state surface the main worker inspects via `list_panels`,
//! computed from `(status, last turn signal, error, blocker)`. Recomputed on
//! every turn-signal ingest.
//!
//! A blocking prompt caucus sees on the *grid* rather than in a turn signal (a
//! chooser, a `[y/n]`) does not enter here: it is overlaid onto the derived
//! state at read time by `Multiplexer::overlay_blocked_state`, which scans the
//! live grid. This module is the pure, manifest-only half.

use serde::{Deserialize, Serialize};

use super::lane_event::{LaneEventBlocker, LaneFailureClass};

/// Coarse state surface for the main worker (`docs/design.md` §8.3).
///
/// Every variant has exactly one producer, and there are only two: a blocker
/// born from the turn signal ([`blocker_state`]) and a prompt seen on the live
/// grid (`Multiplexer::overlay_blocked_state`). A variant with no producer is
/// worse than absent — it tells the main worker caucus can report a condition
/// it has no way to detect.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivedState {
    /// After `PromptDelivered`, before the next turn signal.
    Working,
    /// Turn signal received — waiting for the next instruction.
    Idle,
    /// A tool-permission `[y/n]` prompt: either the turn ended `tool_blocked`,
    /// or the prompt is visible on the grid. caucus never auto-answers — the
    /// main worker replies with `send_keys`.
    BlockedPermissionPrompt,
    /// Grid shows an interactive selection menu (an `AskUserQuestion`-style
    /// chooser): the agent stopped mid-turn waiting for an option to be picked.
    /// No `Stop` hook fires here, so this is detected from the grid, not a
    /// turn signal. The main worker answers it with `select_option`.
    AwaitingSelection,
    /// The turn ended on a transport-level error, or the agent's process is
    /// flagged `failed`.
    InterruptedTransport,
    /// The agent process exited.
    Exited,
}

impl DerivedState {
    /// Canonical `snake_case` name — the wire string `list_panels` returns to
    /// the main worker and the vocabulary `docs/design.md` §8.3 documents
    /// (`working` / `idle` / `awaiting_selection` / `blocked_permission_prompt`
    /// / …). Mirrors the `#[serde(rename_all = "snake_case")]` representation
    /// (pinned by a test) so the MCP string and the serde wire form cannot
    /// drift. This is the single source of the state's external name — callers
    /// must not re-derive it from `Debug`, which drops the underscores.
    pub fn as_str(self) -> &'static str {
        match self {
            DerivedState::Working => "working",
            DerivedState::Idle => "idle",
            DerivedState::BlockedPermissionPrompt => "blocked_permission_prompt",
            DerivedState::AwaitingSelection => "awaiting_selection",
            DerivedState::InterruptedTransport => "interrupted_transport",
            DerivedState::Exited => "exited",
        }
    }
}

/// Pure function deriving the coarse state (`docs/design.md` §8.4).
///
/// `status` is the raw agent status string from the manifest
/// (`live` / `exited` / `failed`). A recorded blocker dominates; otherwise
/// `turn_settled` decides: `true` means the open phase reached a completion
/// boundary — a received turn signal, or a local slash command (`/compact`,
/// `/clear`) whose finish arrived as a lifecycle signal — so the agent is
/// `Idle`; `false` means the phase is still open, so a live agent is
/// `Working`.
pub fn derive_agent_state(
    status: &str,
    turn_settled: bool,
    error: Option<&str>,
    blocker: Option<&LaneEventBlocker>,
) -> DerivedState {
    // Classification precedence (`docs/design.md` §8.3):
    //   1. `exited` status is terminal — overrides everything.
    //   2. A live `error` string puts the agent in a transport-interrupted
    //      state (the process is alive but its turn ended badly).
    //   3. A recorded `blocker` maps onto its blocked/degraded variant.
    //   4. Otherwise: a settled phase means `Idle`, a live agent with an
    //      open phase means `Working`.

    // `exited` is terminal regardless of any pending blocker.
    if status == "exited" {
        return DerivedState::Exited;
    }

    if error.is_some() {
        return DerivedState::InterruptedTransport;
    }

    if let Some(blocker) = blocker {
        return blocker_state(blocker.failure_class);
    }

    match status {
        "failed" => DerivedState::InterruptedTransport,
        _ if turn_settled => DerivedState::Idle,
        _ => DerivedState::Working,
    }
}

/// Map a blocker's failure class onto a `DerivedState` variant
/// (`docs/design.md` §8.3).
fn blocker_state(class: LaneFailureClass) -> DerivedState {
    match class {
        LaneFailureClass::PermissionPrompt => DerivedState::BlockedPermissionPrompt,
        LaneFailureClass::Transport => DerivedState::InterruptedTransport,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `as_str` is the single source of each state's external name, and must
    /// stay identical to the `#[serde(rename_all = "snake_case")]` wire form —
    /// `list_panels` uses `as_str`, protocol round-trips use serde, and they
    /// must agree. Enumerated explicitly so a newly added variant fails to
    /// compile here until it is listed (and thus checked).
    #[test]
    fn as_str_matches_the_serde_snake_case_name() {
        use DerivedState::*;
        for state in [
            Working,
            Idle,
            BlockedPermissionPrompt,
            AwaitingSelection,
            InterruptedTransport,
            Exited,
        ] {
            let serde_name = serde_json::to_value(state).unwrap();
            assert_eq!(
                serde_name.as_str().unwrap(),
                state.as_str(),
                "as_str must match the serde snake_case name for {state:?}"
            );
        }
        // Spot-check the underscored forms the Debug-lowercase path used to
        // drop — the exact regression the MCP wire string had.
        assert_eq!(
            BlockedPermissionPrompt.as_str(),
            "blocked_permission_prompt"
        );
        assert_eq!(AwaitingSelection.as_str(), "awaiting_selection");
    }

    #[test]
    fn live_with_open_phase_is_working() {
        assert_eq!(
            derive_agent_state("live", false, None, None),
            DerivedState::Working
        );
    }

    #[test]
    fn exited_status_is_exited() {
        assert_eq!(
            derive_agent_state("exited", false, None, None),
            DerivedState::Exited
        );
    }

    #[test]
    fn settled_phase_with_no_blocker_is_idle() {
        assert_eq!(
            derive_agent_state("live", true, None, None),
            DerivedState::Idle
        );
    }

    #[test]
    fn error_string_is_interrupted_transport() {
        assert_eq!(
            derive_agent_state("live", false, Some("pipe broke"), None),
            DerivedState::InterruptedTransport
        );
    }

    /// Every `LaneFailureClass` maps onto its `DerivedState` via `blocker_state`
    /// — enumerated in full so a newly added failure class fails to compile here
    /// until it is listed (and thus checked).
    #[test]
    fn blocker_maps_every_failure_class() {
        use LaneFailureClass::*;
        let cases = [
            (PermissionPrompt, DerivedState::BlockedPermissionPrompt),
            (Transport, DerivedState::InterruptedTransport),
        ];
        for (class, expected) in cases {
            let blk = LaneEventBlocker::new(class, "detail");
            assert_eq!(
                derive_agent_state("live", false, None, Some(&blk)),
                expected,
                "failure class {class:?} must map to {expected:?}"
            );
        }
    }

    /// A `failed` status with no blocker is transport-interrupted (the
    /// `match status` `"failed"` arm) — the path `record_*` takes when an
    /// agent's process is flagged failed rather than cleanly exited.
    #[test]
    fn failed_status_is_interrupted_transport() {
        assert_eq!(
            derive_agent_state("failed", false, None, None),
            DerivedState::InterruptedTransport
        );
    }

    /// `failed` is weighed before a settled phase: a stale `Stop` signal must
    /// not surface a failed agent as `Idle`.
    #[test]
    fn failed_status_beats_a_settled_phase() {
        assert_eq!(
            derive_agent_state("failed", true, None, None),
            DerivedState::InterruptedTransport
        );
    }

    #[test]
    fn exited_dominates_error_and_blocker() {
        let blk = LaneEventBlocker::new(LaneFailureClass::PermissionPrompt, "y/n");
        assert_eq!(
            derive_agent_state("exited", false, Some("err"), Some(&blk)),
            DerivedState::Exited
        );
    }

    /// A blocker outranks a settled phase: an agent whose turn ended on
    /// `tool_blocked` is blocked, not `Idle`.
    #[test]
    fn blocker_outranks_a_settled_phase() {
        let blk = LaneEventBlocker::new(LaneFailureClass::PermissionPrompt, "y/n");
        assert_eq!(
            derive_agent_state("live", true, None, Some(&blk)),
            DerivedState::BlockedPermissionPrompt
        );
    }
}
