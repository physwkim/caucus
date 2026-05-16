//! Serialised worktree cleanup queue (`docs/design.md` §5, §12).
//!
//! **Invariant I-3** (`docs/design.md` §12): every `git worktree remove` goes
//! through this module. The queue is a single tokio task draining an
//! `mpsc` receiver; jobs run sequentially so a slow removal cannot stall the
//! rest of the system, and it never blocks the UI.
//!
//! Nested worktrees are removed depth-desc: deepest first, so a parent
//! removal never fails on a still-referenced child.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::manager::{WorktreeError, run_git};

/// One unit of work for the cleanup task.
#[derive(Debug)]
pub struct CleanupJob {
    pub repo_root: PathBuf,
    /// Worktrees to remove, in any order — the queue depth-sorts internally.
    pub worktree_paths: Vec<PathBuf>,
    /// Branch names to delete after the worktrees are gone.
    pub branches_to_delete: Vec<String>,
    /// Optional channel for the caller to await the outcome.
    pub done: Option<oneshot::Sender<CleanupSummary>>,
}

/// Aggregated outcome of a single job.
#[derive(Debug, Default)]
pub struct CleanupSummary {
    pub removed_worktrees: Vec<PathBuf>,
    pub failed_worktrees: Vec<(PathBuf, String)>,
    pub deleted_branches: Vec<String>,
    pub failed_branches: Vec<(String, String)>,
}

#[derive(Debug, Error)]
#[error("cleanup queue has been shut down")]
pub struct QueueClosed;

/// Handle to the singleton cleanup queue. Cloneable; all clones share one
/// channel.
#[derive(Debug, Clone)]
pub struct CleanupQueue {
    tx: mpsc::UnboundedSender<CleanupJob>,
}

impl CleanupQueue {
    /// Spawn the consumer task. Returns the queue handle and the consumer's
    /// join handle.
    pub fn spawn() -> (Self, JoinHandle<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(consumer_loop(rx));
        (Self { tx }, handle)
    }

    /// Single owner of worktree deletion (Invariant I-3): enqueue a cleanup
    /// job onto the serial queue.
    pub(crate) fn enqueue(&self, job: CleanupJob) -> Result<(), QueueClosed> {
        self.tx.send(job).map_err(|_| QueueClosed)
    }
}

async fn consumer_loop(mut rx: mpsc::UnboundedReceiver<CleanupJob>) {
    while let Some(job) = rx.recv().await {
        let summary = run_one(&job);
        if let Some(reply) = job.done {
            let _ = reply.send(summary);
        }
    }
    debug!("cleanup queue consumer shut down");
}

/// Run one cleanup job synchronously, off the async queue.
///
/// The async [`CleanupQueue`] is the normal path, but on caucus shutdown the
/// tokio runtime is dropped — and the queue's consumer task aborted — before
/// it can drain. Shutdown therefore cleans worktrees through this blocking
/// entry point instead, so they are not leaked on every exit.
pub(crate) fn run_blocking(job: &CleanupJob) -> CleanupSummary {
    run_one(job)
}

/// Run one cleanup job. Module-private — external code [`CleanupQueue::enqueue`]s
/// or, on shutdown, calls [`run_blocking`].
///
/// Synchronous git calls; the consumer task awaits the next job, then runs
/// this. The serial queue tolerates the brief block per job.
fn run_one(job: &CleanupJob) -> CleanupSummary {
    let mut summary = CleanupSummary::default();

    // Deeper paths first; lex-descending on ties.
    let mut paths: Vec<PathBuf> = job.worktree_paths.clone();
    paths.sort_by(|a, b| {
        b.components()
            .count()
            .cmp(&a.components().count())
            .then_with(|| b.as_os_str().cmp(a.as_os_str()))
    });

    for path in paths {
        match remove_worktree(&job.repo_root, &path) {
            Ok(()) => summary.removed_worktrees.push(path),
            Err(err) => {
                let msg = err.to_string();
                warn!(?path, %msg, "worktree remove failed");
                summary.failed_worktrees.push((path, msg));
            }
        }
    }

    for branch in &job.branches_to_delete {
        match delete_branch(&job.repo_root, branch) {
            Ok(true) => summary.deleted_branches.push(branch.clone()),
            Ok(false) => {}
            Err(err) => {
                let msg = err.to_string();
                warn!(branch, %msg, "branch delete failed");
                summary.failed_branches.push((branch.clone(), msg));
            }
        }
    }

    summary
}

fn remove_worktree(repo: &Path, path: &Path) -> Result<(), WorktreeError> {
    run_git(
        repo,
        &[
            "worktree".into(),
            "remove".into(),
            "--force".into(),
            path.display().to_string(),
        ],
    )?;
    Ok(())
}

/// `Ok(true)` if the branch existed and was deleted, `Ok(false)` if absent.
fn delete_branch(repo: &Path, branch: &str) -> Result<bool, WorktreeError> {
    let exists = run_git(
        repo,
        &[
            "show-ref".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("refs/heads/{branch}"),
        ],
    );
    if exists.is_err() {
        return Ok(false);
    }
    run_git(repo, &["branch".into(), "-D".into(), branch.into()])?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn depth_desc_sort_keeps_deepest_first() {
        let mut paths: Vec<PathBuf> = [
            "/root/.caucus/worktrees/a",
            "/root/.caucus/worktrees/a/nested/deep",
            "/root/.caucus/worktrees/a/nested",
            "/root/.caucus/worktrees/b",
        ]
        .iter()
        .map(|s| p(s))
        .collect();
        paths.sort_by(|a, b| {
            b.components()
                .count()
                .cmp(&a.components().count())
                .then_with(|| b.as_os_str().cmp(a.as_os_str()))
        });
        let labels: Vec<_> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(labels, vec!["deep", "nested", "b", "a"]);
    }

    #[tokio::test]
    async fn enqueue_returns_done_summary() {
        let (queue, _h) = CleanupQueue::spawn();
        let (tx, rx) = oneshot::channel();
        queue
            .enqueue(CleanupJob {
                repo_root: PathBuf::from("/tmp"),
                worktree_paths: vec![],
                branches_to_delete: vec![],
                done: Some(tx),
            })
            .unwrap();
        let summary = rx.await.unwrap();
        assert!(summary.removed_worktrees.is_empty());
    }

    /// End-to-end execute-phase check: `manager::create` makes a real
    /// `git worktree add`, and the cleanup queue removes it (worktree + branch)
    /// — the worktree half of `spawn_role(worktree=true)` (`docs/design.md` §5).
    #[tokio::test]
    async fn create_then_cleanup_a_real_worktree() {
        use crate::session::id::SessionId;
        use crate::worktree::manager::{WorktreeRequest, create};

        // Hermetic temp git repo with one commit so `worktree add` has a HEAD.
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(repo.path())
                .args(args)
                .output()
                .expect("run git")
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "caucus-test"]);
        git(&["commit", "--allow-empty", "-q", "-m", "init"]);

        // Create the worktree (Invariant I-3 owner).
        let req = WorktreeRequest {
            repo_root: repo.path().to_path_buf(),
            session_id: SessionId::new(),
            role: "worker".into(),
            branch: None,
            base_ref: None,
            name_override: Some("ts-worker-1".into()),
        };
        let handle = create(&req).expect("git worktree add");
        assert!(handle.path.is_dir(), "worktree directory created");
        assert!(
            handle.path.join(".git").exists(),
            "worktree .git marker present"
        );

        // Clean it up through the serial queue.
        let (queue, _h) = CleanupQueue::spawn();
        let (tx, rx) = oneshot::channel();
        queue
            .enqueue(CleanupJob {
                repo_root: repo.path().to_path_buf(),
                worktree_paths: vec![handle.path.clone()],
                branches_to_delete: vec![handle.branch.clone()],
                done: Some(tx),
            })
            .unwrap();
        let summary = rx.await.unwrap();
        assert_eq!(summary.removed_worktrees, vec![handle.path.clone()]);
        assert!(summary.failed_worktrees.is_empty(), "no removal failures");
        assert!(!handle.path.exists(), "worktree directory removed");
        assert_eq!(summary.deleted_branches, vec![handle.branch]);
    }
}
