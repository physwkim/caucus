//! Worktree: per-role git worktrees for the execute phase. Creation
//! (`manager`), serialised cleanup (`cleanup`), and a sub-host lane's branch
//! moving to and from the host (`remote`). See `docs/design.md` §5.

pub mod cleanup;
pub mod manager;
pub(crate) mod remote;

pub use cleanup::{CleanupJob, CleanupQueue, CleanupSummary};
pub use manager::{WorktreeError, WorktreeHandle, WorktreeRequest};
