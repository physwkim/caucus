//! v0 consensus policy: **CEO-decided.** The CEO inspects the round's
//! per-role responses and calls one of
//!
//! - `caucus session converge --decision-file PATH` — locks the decision and
//!   moves the session to `MeetingConverged`.
//! - `caucus session deadlock` — moves to `MeetingDeadlocked`.
//!
//! No automated rule fires here in v0. Rule-based (majority / unanimous) and
//! LLM-judge policies are deferred to v1+ (see `docs/design.md` §13).
//!
//! This module exists so that the CLI has a stable enum to dispatch over
//! and so that v1 can drop in additional variants without renaming.

use serde::{Deserialize, Serialize};

/// Picker for which consensus policy a session should use.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsensusPolicy {
    /// The CEO inspects responses and calls converge/deadlock. v0 default.
    #[default]
    CeoDecided,
}

/// Outcome the CEO records when converging.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    /// Free-form decision text — copied from `--decision-file` verbatim.
    pub body: String,
    /// Optional one-line summary the CEO chose (used in transcripts).
    pub summary: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_ceo_decided() {
        assert_eq!(ConsensusPolicy::default(), ConsensusPolicy::CeoDecided);
    }

    #[test]
    fn policy_serialises_to_kebab_case() {
        let s = serde_json::to_string(&ConsensusPolicy::CeoDecided).unwrap();
        assert_eq!(s, "\"ceo-decided\"");
    }
}
