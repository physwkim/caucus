//! Round protocol: agenda → response files → transcript assembly.
//!
//! See `docs/design.md` §4.

pub mod lifecycle;
pub mod transcript;

pub use lifecycle::{
    RoleStatus, RoundError, RoundLayout, RoundStatus, compose_follower_brief,
    nudge_pane_with_brief, nudge_role, prepare_round, record_pane_gone, record_pane_hint,
    record_sentinel, round_status, write_follower_brief,
};
pub use transcript::{TranscriptError, assemble};
