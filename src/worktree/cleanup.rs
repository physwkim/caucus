//! Serialised worktree cleanup queue.
//!
//! **Invariant I-3** (`docs/design.md` §12): all `git worktree remove` calls
//! go through this module. The queue is a single tokio task consuming from
//! an `mpsc::UnboundedReceiver<CleanupJob>`; jobs run sequentially so a slow
//! `git worktree remove --force` on a large directory cannot stall the rest
//! of the system.
//!
//! Nested worktrees (worktrees inside a worktree) are removed depth-desc:
//! deepest first, so a parent removal never fails because a child is still
//! referenced.

use std::path::PathBuf;

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
    /// Optional branch names to delete after the worktrees are gone.
    pub branches_to_delete: Vec<String>,
    /// Optional channel for the caller to await completion. The result is
    /// `Ok(())` if every step that could be attempted was attempted; partial
    /// failures are logged but do not poison the queue.
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

/// Handle to the singleton cleanup queue. Cloneable; all clones share the
/// same underlying channel.
#[derive(Debug, Clone)]
pub struct CleanupQueue {
    tx: mpsc::UnboundedSender<CleanupJob>,
}

impl CleanupQueue {
    /// Spawn the consumer task. Returns the queue handle and the join handle
    /// for the consumer (callers may await it for an orderly shutdown).
    pub fn spawn() -> (Self, JoinHandle<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(consumer_loop(rx));
        (Self { tx }, handle)
    }

    pub fn enqueue(&self, job: CleanupJob) -> Result<(), QueueClosed> {
        self.tx.send(job).map_err(|_| QueueClosed)
    }
}

async fn consumer_loop(mut rx: mpsc::UnboundedReceiver<CleanupJob>) {
    while let Some(job) = rx.recv().await {
        let summary = run_job(&job).await;
        if let Some(reply) = job.done {
            // Receiver may already be gone — that's fine.
            let _ = reply.send(summary);
        }
    }
    debug!("cleanup queue consumer shut down");
}

async fn run_job(job: &CleanupJob) -> CleanupSummary {
    let mut summary = CleanupSummary::default();

    // Deeper paths first. Falls back to lex-descending if depths tie.
    let mut paths: Vec<PathBuf> = job.worktree_paths.clone();
    paths.sort_by(|a, b| {
        let depth_b = b.components().count();
        let depth_a = a.components().count();
        depth_b
            .cmp(&depth_a)
            .then_with(|| b.as_os_str().cmp(a.as_os_str()))
    });

    for path in paths {
        match remove_worktree(&job.repo_root, &path).await {
            Ok(()) => summary.removed_worktrees.push(path),
            Err(err) => {
                let msg = err.to_string();
                warn!(?path, %msg, "worktree remove failed");
                summary.failed_worktrees.push((path, msg));
            }
        }
    }

    for branch in &job.branches_to_delete {
        match delete_branch(&job.repo_root, branch).await {
            Ok(true) => summary.deleted_branches.push(branch.clone()),
            // Branch didn't exist — not a failure.
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

async fn remove_worktree(
    repo: &std::path::Path,
    path: &std::path::Path,
) -> Result<(), WorktreeError> {
    let _ = run_git(
        repo,
        &[
            "worktree".into(),
            "remove".into(),
            "--force".into(),
            path.display().to_string(),
        ],
    )
    .await?;
    Ok(())
}

/// Returns `Ok(true)` if the branch existed and was deleted, `Ok(false)`
/// if it did not exist, `Err(_)` for any other git failure.
async fn delete_branch(repo: &std::path::Path, branch: &str) -> Result<bool, WorktreeError> {
    let exists = run_git(
        repo,
        &[
            "show-ref".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("refs/heads/{branch}"),
        ],
    )
    .await;
    if exists.is_err() {
        return Ok(false);
    }
    let _ = run_git(repo, &["branch".into(), "-D".into(), branch.into()]).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
            let depth_b = b.components().count();
            let depth_a = a.components().count();
            depth_b
                .cmp(&depth_a)
                .then_with(|| b.as_os_str().cmp(a.as_os_str()))
        });
        let labels: Vec<_> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(labels, vec!["deep", "nested", "b", "a"]);
    }

    #[tokio::test]
    async fn queue_drops_silently_when_no_done_channel() {
        // Pure smoke: spawn, send a no-op job, shut down.
        let (queue, handle) = CleanupQueue::spawn();
        queue
            .enqueue(CleanupJob {
                repo_root: PathBuf::from("/tmp"),
                worktree_paths: vec![],
                branches_to_delete: vec![],
                done: None,
            })
            .unwrap();
        drop(queue); // closes the channel; consumer ends gracefully.
        handle.await.unwrap();
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
        assert!(summary.failed_worktrees.is_empty());
    }
}

#[cfg(test)]
mod integration {
    use super::*;
    use crate::session::id::SessionId;
    use crate::worktree::manager::{WorktreeRequest, create, run_git};
    use tempfile::TempDir;

    async fn init_repo(dir: &std::path::Path) {
        run_git(dir, &["init".into(), "-q".into()]).await.unwrap();
        run_git(
            dir,
            &[
                "config".into(),
                "user.email".into(),
                "caucus@test.invalid".into(),
            ],
        )
        .await
        .unwrap();
        run_git(
            dir,
            &["config".into(), "user.name".into(), "caucus-test".into()],
        )
        .await
        .unwrap();
        // First commit so HEAD exists.
        std::fs::write(dir.join("seed"), "seed\n").unwrap();
        run_git(dir, &["add".into(), "seed".into()]).await.unwrap();
        run_git(
            dir,
            &["commit".into(), "-q".into(), "-m".into(), "seed".into()],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires git on PATH; runs git init + worktree add/remove"]
    async fn worktree_create_then_cleanup_round_trip() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path()).await;

        let req = WorktreeRequest {
            repo_root: tmp.path().to_path_buf(),
            session_id: SessionId::new(),
            role: "backend".into(),
            branch: None,
            base_ref: None,
        };
        let handle = create(&req).await.unwrap();
        assert!(handle.path.exists(), "worktree dir should exist");
        assert!(handle.path.join("seed").exists());

        // Run cleanup through the serial queue.
        let (queue, qjoin) = CleanupQueue::spawn();
        let (tx, rx) = oneshot::channel();
        queue
            .enqueue(CleanupJob {
                repo_root: tmp.path().to_path_buf(),
                worktree_paths: vec![handle.path.clone()],
                branches_to_delete: vec![handle.branch.clone()],
                done: Some(tx),
            })
            .unwrap();
        let summary = rx.await.unwrap();
        drop(queue);
        qjoin.await.unwrap();

        assert_eq!(summary.removed_worktrees, vec![handle.path.clone()]);
        assert!(summary.failed_worktrees.is_empty());
        assert_eq!(summary.deleted_branches, vec![handle.branch.clone()]);
        assert!(!handle.path.exists(), "worktree dir should be gone");
    }
}
