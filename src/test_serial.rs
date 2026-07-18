//! Test-only serialization of subprocess-heavy tests.
//!
//! A few unit tests shell out to `git` dozens or hundreds of times: the
//! provenance depth test alone makes ~200 `git` calls to build a 100-commit
//! branch, and every worktree test runs `git worktree add`. Run concurrently on
//! a small CI runner (2 cores, so `cargo test` defaults to `RUST_TEST_THREADS=2`)
//! the fork/exec pressure is enough that a `git` call transiently fails, flaking
//! a test whose logic is correct.
//!
//! This is one process-wide [`RwLock`] used purely as a scheduler, not as a data
//! guard — the guarded `()` carries nothing:
//!
//! - the heaviest test takes it [`exclusive`]ly (write), so nothing else that
//!   respects the lock runs alongside it;
//! - each `git worktree add` (`worktree::manager::{create, attach}`) takes it
//!   [`shared`]ly (read), so those tests still parallelize with one another but
//!   never overlap the exclusive holder.
//!
//! Poison is recovered from rather than propagated: a test that panics while
//! holding the lock must not cascade every other guarded test into a
//! poisoned-lock panic that hides the original failure.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

static HEAVY_GIT: RwLock<()> = RwLock::new(());

/// Take the lock exclusively: no [`shared`] holder runs while the returned guard
/// is alive. For the one test heavy enough to need the runner to itself.
pub(crate) fn exclusive() -> RwLockWriteGuard<'static, ()> {
    HEAVY_GIT
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Take the lock shared: blocks only while an [`exclusive`] holder is alive.
/// For the `git worktree add` owner, so worktree tests still parallelize with
/// each other but yield to the exclusive holder.
pub(crate) fn shared() -> RwLockReadGuard<'static, ()> {
    HEAVY_GIT
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
