//! Serialised worktree cleanup queue (`docs/design.md` §5, §12).
//!
//! **Invariant I-3** (`docs/design.md` §12): every `git worktree remove` goes
//! through this module. The queue is a single tokio task draining an
//! `mpsc` receiver; jobs run sequentially so a slow removal cannot stall the
//! rest of the system, and it never blocks the UI.
//!
//! **Invariant I-8**: no `git worktree remove --force` destroys uncommitted
//! work. Only a *clean* or *salvaged* worktree may be force-removed; a tree
//! whose work could not be salvaged is left on disk instead.
//!
//! Two functions force-remove, and both enforce it:
//! [`salvage_before_removal`] here (the disposal queue — `kill_panel`,
//! shutdown, spawn-failure) and [`super::manager::reconcile_stale`] (the
//! crash/resume re-attach). Both salvage through
//! [`super::manager::salvage_uncommitted_work`], committing a dirty tree onto
//! its own branch first. The branch outlives the worktree on every path that
//! keeps it, so the commit is recoverable; the spawn-failure path deletes the
//! branch and so has nothing to preserve.
//!
//! Nested worktrees are removed depth-desc: deepest first, so a parent
//! removal never fails on a still-referenced child.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::manager::{WorktreeError, run_git, salvage_uncommitted_work, worktree_is_dirty};
use crate::agent::lane_event::{LaneEvent, LaneEventKind};
use crate::agent::manifest;
use crate::session::id::AgentId;

/// The agent that owned a worktree in a [`CleanupJob`], so the worker can record
/// the removal on that agent's timeline (`LaneEventKind::WorktreeRemoved`).
///
/// The removal is a fact only the worker knows: by the time a job runs, the
/// owning panel is detached and its manifest is gone from the multiplexer's live
/// map, and the removal can still fail or be refused (**Invariant I-8** leaves a
/// worktree whose work could not be salvaged on disk). So the worker records it
/// against the manifest on disk, from the outcome it actually got — never the
/// intention the caller had. It holds that manifest exclusively; no live copy
/// races it.
///
/// A job with no owner (an aborted spawn's orphan worktree — no agent ever ran
/// in it) simply records nothing.
#[derive(Debug, Clone)]
pub struct WorktreeOwner {
    pub worktree: PathBuf,
    pub session_root: PathBuf,
    pub agent_id: AgentId,
}

/// One unit of work for the cleanup task.
#[derive(Debug)]
pub struct CleanupJob {
    pub repo_root: PathBuf,
    /// Worktrees to remove, in any order — the queue depth-sorts internally.
    pub worktree_paths: Vec<PathBuf>,
    /// Branch names to delete after the worktrees are gone.
    pub branches_to_delete: Vec<String>,
    /// Agents to credit the removals to. Only worktrees that were *actually*
    /// removed are recorded; see [`WorktreeOwner`].
    pub owners: Vec<WorktreeOwner>,
    /// Optional channel for the caller to await the outcome.
    pub done: Option<oneshot::Sender<CleanupSummary>>,
}

/// Aggregated outcome of a single job.
#[derive(Debug, Default)]
pub struct CleanupSummary {
    pub removed_worktrees: Vec<PathBuf>,
    pub failed_worktrees: Vec<(PathBuf, String)>,
    /// Worktrees whose uncommitted work was committed onto their branch before
    /// removal, as `(worktree, branch)` — recover with `git show <branch>`.
    pub salvaged_worktrees: Vec<(PathBuf, String)>,
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
        match salvage_before_removal(&path, &job.branches_to_delete) {
            Ok(Some(branch)) => summary.salvaged_worktrees.push((path.clone(), branch)),
            Ok(None) => {}
            // The tree holds work we could not preserve: refuse to remove it.
            // A leaked worktree is recoverable by hand; a force-removed one is
            // not.
            Err(err) => {
                let msg = err.to_string();
                warn!(?path, %msg, "worktree salvage failed; not removing");
                summary.failed_worktrees.push((path, msg));
                continue;
            }
        }
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

    record_removals(job, &summary);
    summary
}

/// Append `WorktreeRemoved` to the timeline of every agent whose worktree this
/// job actually removed (see [`WorktreeOwner`]).
///
/// Keyed off `summary.removed_worktrees`, so a worktree left on disk by the
/// I-8 salvage refusal — or one whose `git worktree remove` failed — records
/// nothing: the event states what happened, not what was attempted. Salvaged
/// trees are removed after their work is committed, so they are in that list and
/// are recorded like any other.
///
/// Best-effort, like every other timeline append: a manifest that cannot be read
/// or written is logged. The worktree is already gone either way, and failing the
/// cleanup over its bookkeeping would strand the queue.
fn record_removals(job: &CleanupJob, summary: &CleanupSummary) {
    for owner in &job.owners {
        if !summary.removed_worktrees.contains(&owner.worktree) {
            continue;
        }
        let mut manifest = match manifest::read(&owner.session_root, owner.agent_id) {
            Ok(m) => m,
            Err(err) => {
                warn!(agent = %owner.agent_id, error = %err, "worktree-removed manifest read failed");
                continue;
            }
        };
        let event = LaneEvent::now(LaneEventKind::WorktreeRemoved {
            path: owner.worktree.clone(),
        });
        if let Err(err) = manifest::write(&mut manifest, &owner.session_root, Some(event)) {
            warn!(agent = %owner.agent_id, error = %err, "worktree-removed manifest write failed");
        }
    }
}

/// Commit a doomed worktree's uncommitted changes onto its own branch, so the
/// `--force` removal that follows destroys nothing (**Invariant I-8**).
///
/// `Ok(Some(branch))` when work was salvaged, `Ok(None)` when there was nothing
/// to salvage or nowhere to salvage it to, `Err` when the tree is dirty and the
/// salvage failed — the caller must then leave the worktree alone.
///
/// Two cases legitimately salvage nothing:
///
/// - **Clean tree.** Nothing to preserve.
/// - **Its branch is being deleted too** (`branches_to_delete`, the spawn-failure
///   path): a commit onto a branch this same job then deletes preserves nothing,
///   and such a worktree was never handed to an agent.
///
/// A dirty worktree on a **detached HEAD** has no branch to commit onto — the
/// commit would be unreachable the moment the worktree is gone — so it is an
/// error, not a silent removal. caucus's own worktrees always sit on a branch
/// (`manager::create`), so this means someone re-pointed it by hand.
fn salvage_before_removal(
    path: &Path,
    branches_to_delete: &[String],
) -> Result<Option<String>, WorktreeError> {
    if !worktree_is_dirty(path) {
        return Ok(None);
    }
    let Some(branch) = current_branch(path) else {
        return Err(WorktreeError::DirtyDetachedHead(path.to_path_buf()));
    };
    if branches_to_delete.contains(&branch) {
        return Ok(None);
    }
    salvage_uncommitted_work(path, &branch)?;
    Ok(Some(branch))
}

/// The branch `worktree` has checked out, or `None` on a detached HEAD.
fn current_branch(worktree: &Path) -> Option<String> {
    let out = run_git(
        worktree,
        &["rev-parse".into(), "--abbrev-ref".into(), "HEAD".into()],
    )
    .ok()?;
    let branch = out.trim();
    (!branch.is_empty() && branch != "HEAD").then(|| branch.to_string())
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
                owners: Vec::new(),
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
                owners: Vec::new(),
                done: Some(tx),
            })
            .unwrap();
        let summary = rx.await.unwrap();
        assert_eq!(summary.removed_worktrees, vec![handle.path.clone()]);
        assert!(summary.failed_worktrees.is_empty(), "no removal failures");
        assert!(!handle.path.exists(), "worktree directory removed");
        assert_eq!(summary.deleted_branches, vec![handle.branch]);
    }

    /// A hermetic repo with one commit, plus a `git` runner rooted at it.
    fn repo_with_one_commit() -> tempfile::TempDir {
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
        repo
    }

    fn make_worktree(
        repo: &std::path::Path,
        name: &str,
    ) -> crate::worktree::manager::WorktreeHandle {
        use crate::session::id::SessionId;
        use crate::worktree::manager::{WorktreeRequest, create};
        create(&WorktreeRequest {
            repo_root: repo.to_path_buf(),
            session_id: SessionId::new(),
            role: "worker".into(),
            branch: None,
            base_ref: None,
            name_override: Some(name.into()),
        })
        .expect("git worktree add")
    }

    fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("run git");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// **Invariant I-8**, the boundary that matters: a panel killed with
    /// uncommitted work in its worktree must not lose it. The removal owner
    /// commits it onto the branch first, and the branch outlives the worktree.
    #[test]
    fn dirty_worktree_is_salvaged_onto_its_branch_before_removal() {
        let repo = repo_with_one_commit();
        let handle = make_worktree(repo.path(), "ts-dirty-1");
        std::fs::write(handle.path.join("wip.rs"), "fn uncommitted() {}").unwrap();

        // Branch kept, as `retire_worktree` (kill_panel) and shutdown do.
        let summary = run_blocking(&CleanupJob {
            repo_root: repo.path().to_path_buf(),
            worktree_paths: vec![handle.path.clone()],
            branches_to_delete: Vec::new(),
            owners: Vec::new(),
            done: None,
        });

        assert_eq!(
            summary.salvaged_worktrees,
            vec![(handle.path.clone(), handle.branch.clone())]
        );
        assert_eq!(summary.removed_worktrees, vec![handle.path.clone()]);
        assert!(!handle.path.exists(), "worktree directory removed");

        // The work is reachable on the branch after the worktree is gone.
        let files = git_in(repo.path(), &["ls-tree", "--name-only", &handle.branch]);
        assert!(files.contains("wip.rs"), "salvaged tree: {files}");
    }

    /// A clean worktree is removed with no salvage commit — the invariant must
    /// not manufacture empty commits on every kill.
    #[test]
    fn clean_worktree_is_removed_without_a_salvage_commit() {
        let repo = repo_with_one_commit();
        let handle = make_worktree(repo.path(), "ts-clean-1");
        let before = git_in(repo.path(), &["rev-parse", &handle.branch]);

        let summary = run_blocking(&CleanupJob {
            repo_root: repo.path().to_path_buf(),
            worktree_paths: vec![handle.path.clone()],
            branches_to_delete: Vec::new(),
            owners: Vec::new(),
            done: None,
        });

        assert!(summary.salvaged_worktrees.is_empty(), "nothing to salvage");
        assert_eq!(summary.removed_worktrees, vec![handle.path.clone()]);
        let after = git_in(repo.path(), &["rev-parse", &handle.branch]);
        assert_eq!(before, after, "branch tip unmoved");
    }

    /// The other boundary: when the job deletes the branch too (the
    /// spawn-failure path), salvaging onto it would preserve nothing. Remove
    /// without a pointless commit.
    #[test]
    fn dirty_worktree_whose_branch_is_deleted_is_not_salvaged() {
        let repo = repo_with_one_commit();
        let handle = make_worktree(repo.path(), "ts-doomed-1");
        std::fs::write(handle.path.join("wip.rs"), "fn uncommitted() {}").unwrap();

        let summary = run_blocking(&CleanupJob {
            repo_root: repo.path().to_path_buf(),
            worktree_paths: vec![handle.path.clone()],
            branches_to_delete: vec![handle.branch.clone()],
            owners: Vec::new(),
            done: None,
        });

        assert!(
            summary.salvaged_worktrees.is_empty(),
            "branch is going away"
        );
        assert_eq!(summary.removed_worktrees, vec![handle.path.clone()]);
        assert_eq!(summary.deleted_branches, vec![handle.branch]);
    }

    /// A dirty worktree we cannot salvage (detached HEAD — no branch to commit
    /// onto) is left on disk. A leaked worktree is recoverable by hand; a
    /// force-removed one is not.
    #[test]
    fn dirty_detached_head_worktree_is_kept_not_force_removed() {
        let repo = repo_with_one_commit();
        let handle = make_worktree(repo.path(), "ts-detached-1");
        git_in(&handle.path, &["checkout", "-q", "--detach"]);
        std::fs::write(handle.path.join("wip.rs"), "fn uncommitted() {}").unwrap();

        let summary = run_blocking(&CleanupJob {
            repo_root: repo.path().to_path_buf(),
            worktree_paths: vec![handle.path.clone()],
            branches_to_delete: Vec::new(),
            owners: Vec::new(),
            done: None,
        });

        assert!(summary.removed_worktrees.is_empty(), "must not remove");
        assert_eq!(summary.failed_worktrees.len(), 1);
        assert!(handle.path.exists(), "worktree left on disk for recovery");
    }

    /// Seed a manifest on disk for an agent that owned `worktree`, and return
    /// `(session_root, owner)`. The agent's panel is detached by the time a
    /// cleanup job runs, so this mirrors what the worker actually finds: a
    /// manifest with no live copy anywhere.
    fn detached_owner(worktree: &std::path::Path) -> (tempfile::TempDir, WorktreeOwner) {
        use crate::agent::manifest::AgentManifest;
        use crate::role::spec::AgentCli;
        use crate::session::id::{PanelId, SessionId};

        let session = tempfile::tempdir().unwrap();
        let mut mf = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "worker",
            "worker-1",
            AgentCli::Claude,
            None,
        );
        mf.worktree_path = Some(worktree.to_path_buf());
        manifest::write(&mut mf, session.path(), None).unwrap();
        let owner = WorktreeOwner {
            worktree: worktree.to_path_buf(),
            session_root: session.path().to_path_buf(),
            agent_id: mf.agent_id,
        };
        (session, owner)
    }

    fn recorded_removals(owner: &WorktreeOwner) -> Vec<PathBuf> {
        manifest::read(&owner.session_root, owner.agent_id)
            .unwrap()
            .lane_events()
            .iter()
            .filter_map(|e| match &e.kind {
                LaneEventKind::WorktreeRemoved { path } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    /// The worker records `WorktreeRemoved` on the owning agent's timeline —
    /// the removal is a fact only it knows, since the panel is long detached.
    #[test]
    fn a_removed_worktree_is_recorded_on_its_owners_timeline() {
        let repo = repo_with_one_commit();
        let handle = make_worktree(repo.path(), "ts-owned-1");
        let (_session, owner) = detached_owner(&handle.path);

        let summary = run_blocking(&CleanupJob {
            repo_root: repo.path().to_path_buf(),
            worktree_paths: vec![handle.path.clone()],
            branches_to_delete: Vec::new(),
            owners: vec![owner.clone()],
            done: None,
        });

        assert_eq!(summary.removed_worktrees, vec![handle.path.clone()]);
        assert_eq!(
            recorded_removals(&owner),
            vec![handle.path],
            "the agent's timeline records the removal"
        );
    }

    /// The event states what happened, not what was attempted: a worktree the
    /// I-8 salvage refusal left on disk records nothing. Without this the
    /// timeline would claim a removal for a directory still sitting there —
    /// exactly the lie that recording at enqueue time would tell.
    #[test]
    fn a_worktree_left_on_disk_records_no_removal() {
        let repo = repo_with_one_commit();
        let handle = make_worktree(repo.path(), "ts-owned-kept-1");
        git_in(&handle.path, &["checkout", "-q", "--detach"]);
        std::fs::write(handle.path.join("wip.rs"), "fn uncommitted() {}").unwrap();
        let (_session, owner) = detached_owner(&handle.path);

        let summary = run_blocking(&CleanupJob {
            repo_root: repo.path().to_path_buf(),
            worktree_paths: vec![handle.path.clone()],
            branches_to_delete: Vec::new(),
            owners: vec![owner.clone()],
            done: None,
        });

        assert!(summary.removed_worktrees.is_empty());
        assert!(handle.path.exists(), "left on disk by I-8");
        assert!(
            recorded_removals(&owner).is_empty(),
            "a worktree still on disk was not removed, so nothing is recorded"
        );
    }

    /// A salvaged worktree *is* removed (its work is committed onto the branch
    /// first), so it is recorded like any other removal.
    #[test]
    fn a_salvaged_worktree_is_recorded_as_removed() {
        let repo = repo_with_one_commit();
        let handle = make_worktree(repo.path(), "ts-owned-salvage-1");
        std::fs::write(handle.path.join("wip.rs"), "fn uncommitted() {}").unwrap();
        let (_session, owner) = detached_owner(&handle.path);

        let summary = run_blocking(&CleanupJob {
            repo_root: repo.path().to_path_buf(),
            worktree_paths: vec![handle.path.clone()],
            branches_to_delete: Vec::new(),
            owners: vec![owner.clone()],
            done: None,
        });

        assert_eq!(summary.salvaged_worktrees.len(), 1, "work was salvaged");
        assert_eq!(recorded_removals(&owner), vec![handle.path]);
    }
}
