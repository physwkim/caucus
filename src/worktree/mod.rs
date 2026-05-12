//! Git worktree management. Removals go through a single serialised queue —
//! see `docs/design.md` Invariant I-3.

pub mod cleanup;
pub mod manager;

pub use cleanup::{CleanupJob, CleanupQueue, CleanupSummary, QueueClosed};
pub use manager::{WorktreeError, WorktreeHandle, WorktreeRequest, create, current_branch};
