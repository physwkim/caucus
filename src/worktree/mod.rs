//! Worktree: per-role git worktrees for the execute phase. Creation
//! (`manager`), serialised cleanup (`cleanup`), and a sub-host lane's branch
//! moving to and from the host (`remote`). See `docs/design.md` §5.

pub mod cleanup;
pub mod manager;
pub(crate) mod remote;

pub use cleanup::{CleanupJob, CleanupQueue, CleanupSummary};
pub use manager::{WorktreeError, WorktreeHandle, WorktreeRequest};

use std::path::{Path, PathBuf};

/// Whether `dir` is in the repository at `repo`, as its worktrees and
/// subdirectories are. A directory git cannot place counts as not.
pub(crate) fn same_repository(dir: &Path, repo: &Path) -> bool {
    let common_dir = |at: &Path| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(at)
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .output()
            .ok()
            .filter(|out| out.status.success())?;
        let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim_end());
        path.canonicalize().ok()
    };
    matches!((common_dir(dir), common_dir(repo)), (Some(a), Some(b)) if a == b)
}
