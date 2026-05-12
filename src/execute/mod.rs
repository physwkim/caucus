//! Execution phase: spawn an agent inside a fresh git worktree, capture
//! commit provenance on finish.

pub mod lifecycle;

pub use lifecycle::{
    AbandonOutcome, ExecuteError, ExecuteLayout, ExecuteStartOutcome, ExecuteStartRequest,
    FinishOutcome, abandon, finish, start,
};
