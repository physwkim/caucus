//! Execution phase: spawn an agent inside a fresh git worktree, capture
//! commit provenance on finish.

pub mod lifecycle;
pub mod pipeline;

pub use lifecycle::{
    AbandonOutcome, ExecuteError, ExecuteLayout, ExecuteStartOutcome, ExecuteStartRequest,
    FinishOutcome, abandon, finish, start,
};
pub use pipeline::{
    PipelineError, PipelineOutcome, PipelineRequest, PipelineStatus, StepKind, StepOutcome,
    response_is_blocked, run as pipeline_run,
};
