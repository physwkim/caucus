//! Worktree: per-role git worktrees for the execute phase. Creation
//! (`manager`) and serialised cleanup (`cleanup`). See `docs/design.md` §5.

pub mod cleanup;
pub mod manager;

pub use cleanup::{CleanupJob, CleanupQueue, CleanupSummary};
pub use manager::{WorktreeError, WorktreeHandle, WorktreeRequest};
