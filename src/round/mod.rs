//! Round protocol: agenda → response files → transcript assembly.
//!
//! See `docs/design.md` §4.

pub mod lifecycle;
pub mod transcript;

pub use lifecycle::{
    RoleStatus, RoundError, RoundLayout, RoundStatus, nudge_role, prepare_round, record_pane_gone,
    record_pane_hint, record_sentinel, round_status,
};
pub use transcript::{TranscriptError, assemble};
