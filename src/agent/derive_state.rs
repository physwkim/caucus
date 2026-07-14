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
            DerivedState::BlockedMergeConflict => "blocked_merge_conflict",
            DerivedState::BlockedBackgroundJob => "blocked_background_job",
            DerivedState::AwaitingSelection => "awaiting_selection",
            DerivedState::DegradedMcp => "degraded_mcp",
            DerivedState::InterruptedTransport => "interrupted_transport",
            DerivedState::Exited => "exited",
        }
    }
}

/// Pure function deriving the coarse state (`docs/design.md` §8.4).
///
/// `status` is the raw agent status string from the manifest
/// (`live` / `exited` / `failed`). A recorded blocker dominates; otherwise a
/// received turn signal means `Idle`, and `live` with no signal means
/// `Working`.
pub fn derive_agent_state(
    status: &str,
    last_turn_signal: Option<&TurnSignal>,
    error: Option<&str>,
    blocker: Option<&LaneEventBlocker>,
) -> DerivedState {
    // Classification precedence (`docs/design.md` §8.3):
    //   1. `exited` status is terminal — overrides everything.
    //   2. A live `error` string puts the agent in a transport-interrupted
    //      state (the process is alive but its turn ended badly).
    //   3. A recorded `blocker` maps onto its blocked/degraded variant.
    //   4. Otherwise: a received turn signal means `Idle`, a live agent with
    //      no signal yet means `Working`.

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
            BlockedMergeConflict,
            BlockedBackgroundJob,
            AwaitingSelection,
            DegradedMcp,
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
    fn live_with_no_signal_is_working() {
        assert_eq!(
            derive_agent_state("live", None, None, None),
            DerivedState::Working
        );
    }

    #[test]
    fn exited_status_is_exited() {
        assert_eq!(
            derive_agent_state("exited", None, None, None),
            DerivedState::Exited
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
            derive_agent_state("live", Some(&sig), None, None),
            DerivedState::Idle
        );
    }

    #[test]
    fn error_string_is_interrupted_transport() {
        assert_eq!(
            derive_agent_state("live", None, Some("pipe broke"), None),
            DerivedState::InterruptedTransport
        );
    }

    /// Every `LaneFailureClass` maps onto its `DerivedState` via `blocker_state`
    /// — enumerated in full so a newly added failure class fails to compile here
    /// until it is listed (and thus checked). `BackgroundJob`, `Transport`,
    /// `PromptDelivery`, and `Unknown` had no prior coverage.
    #[test]
    fn blocker_maps_every_failure_class() {
        use LaneFailureClass::*;
        let cases = [
            (PermissionPrompt, DerivedState::BlockedPermissionPrompt),
            (MergeConflict, DerivedState::BlockedMergeConflict),
            (BackgroundJob, DerivedState::BlockedBackgroundJob),
            (McpHandshake, DerivedState::DegradedMcp),
            (Transport, DerivedState::InterruptedTransport),
            // No dedicated state surface — treated as transport-interrupted so
            // the main worker still sees a non-Idle panel.
            (PromptDelivery, DerivedState::InterruptedTransport),
            (Unknown, DerivedState::InterruptedTransport),
        ];
        for (class, expected) in cases {
            let blk = LaneEventBlocker::new(class, "detail");
            assert_eq!(
                derive_agent_state("live", None, None, Some(&blk)),
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
            derive_agent_state("failed", None, None, None),
            DerivedState::InterruptedTransport
        );
    }

    /// `failed` is weighed before a turn signal: a stale `Stop` signal must not
    /// surface a failed agent as `Idle`.
    #[test]
    fn failed_status_beats_a_turn_signal() {
        let sig = TurnSignal::now(
            crate::session::id::SessionId::new(),
            crate::session::id::PanelId::new(),
            crate::signal::TurnKind::Stop,
            Some("done".into()),
            serde_json::Value::Null,
        );
        assert_eq!(
            derive_agent_state("failed", Some(&sig), None, None),
            DerivedState::InterruptedTransport
        );
    }

    #[test]
    fn exited_dominates_error_and_blocker() {
        let blk = LaneEventBlocker::new(LaneFailureClass::PermissionPrompt, "y/n");
        assert_eq!(
            derive_agent_state("exited", None, Some("err"), Some(&blk)),
            DerivedState::Exited
        );
    }

    /// A blocker outranks a received turn signal: an agent whose turn ended on
    /// `tool_blocked` is blocked, not `Idle`.
    #[test]
    fn blocker_outranks_a_turn_signal() {
        let sig = TurnSignal::now(
            crate::session::id::SessionId::new(),
            crate::session::id::PanelId::new(),
            crate::signal::TurnKind::ToolBlocked,
            None,
            serde_json::Value::Null,
        );
        let blk = LaneEventBlocker::new(LaneFailureClass::PermissionPrompt, "y/n");
        assert_eq!(
            derive_agent_state("live", Some(&sig), None, Some(&blk)),
            DerivedState::BlockedPermissionPrompt
        );
    }
}
