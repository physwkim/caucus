//! Off-event-loop worktree creation for `spawn_role(worktree=true)`.
//!
//! `git worktree add` is a subprocess that can take hundreds of milliseconds on
//! a large repo or a slow disk. Running it inline in [`Multiplexer::drain_control`]
//! froze the single-threaded event loop for that whole time — no input, no PTY
//! pumping, no redraw. So the live control-socket path defers it: the cheap
//! request-building stays on the event loop, the slow `git worktree add` runs on
//! a worker thread, and the MCP tool call's reply is sent *later* — from
//! [`Multiplexer::poll_pending_spawns`], once the worktree exists and the panel
//! has launched. This is the one deferred-reply path in the control protocol
//! (`docs/design.md` §0 #4); every other request still answers synchronously.

use super::*;
use crate::mcp::McpError;
use crate::mcp::protocol::ControlResponse;
use crate::role::spec::AgentCli;
use crate::session::id::PanelId;
use crate::worktree::cleanup::CleanupJob;
use crate::worktree::manager::{WorktreeHandle, create as create_worktree};
use tokio::sync::oneshot;
use tracing::warn;

/// A `spawn_role(worktree=true)` whose `git worktree add` is running on a worker
/// thread. Once the worktree result arrives on `rx`, the panel is launched on
/// the event loop and `reply` answers the deferred MCP tool call.
pub(crate) struct PendingSpawn {
    /// The role label — also counted by [`Multiplexer::role_worktree_request`]
    /// so a second concurrent same-role spawn gets a distinct sequence number.
    pub(crate) role: String,
    /// Result of `git worktree add` from the worker thread.
    rx: std::sync::mpsc::Receiver<Result<WorktreeHandle, String>>,
    model: Option<String>,
    agent_cli: Option<AgentCli>,
    prompt: Option<String>,
    /// The control-socket oneshot the MCP call is blocked on.
    reply: oneshot::Sender<ControlResponse>,
}

impl Multiplexer {
    /// Launch a panel for `role` once its worktree (if any) is ready, sharing
    /// the leak-guard with the synchronous path: a spawn that fails *after* the
    /// worktree was created enqueues the dir+branch for cleanup so it is never
    /// orphaned (the branch is empty — the sub-agent never ran — so it is
    /// deleted, Invariant I-3). The single place this finish logic lives.
    pub(crate) fn finish_spawn_role(
        &mut self,
        role: &str,
        model: Option<&str>,
        agent_cli: Option<AgentCli>,
        prompt: Option<&str>,
        wt_handle: Option<WorktreeHandle>,
    ) -> Result<PanelId, McpError> {
        let worktree_path = wt_handle.as_ref().map(|h| h.path.clone());
        let worktree_branch = wt_handle.as_ref().map(|h| h.branch.clone());
        // `spawn_panel_resume` with no resume id is a plain spawn that also
        // records the worktree branch (so `caucus resume` can re-attach it). The
        // inline `prompt`, when set, becomes the role's system prompt — the
        // free-form-role path (`docs/design.md` §6).
        let spawned = self.spawn_panel_resume(
            role,
            agent_cli,
            model.map(str::to_string),
            worktree_path,
            worktree_branch,
            None,
            prompt.map(str::to_string),
        );
        match spawned {
            Ok(id) => {
                self.persist_record();
                Ok(id)
            }
            Err(e) => {
                if let Some(h) = wt_handle {
                    let _ = self.cleanup.enqueue(CleanupJob {
                        repo_root: self.session.repo_path.clone(),
                        worktree_paths: vec![h.path],
                        branches_to_delete: vec![h.branch],
                        // No agent ever ran in this worktree — nothing to credit.
                        owners: Vec::new(),
                        done: None,
                    });
                }
                Err(McpError::Tool(format!("spawn_role: {e:#}")))
            }
        }
    }

    /// Begin a deferred `spawn_role(worktree=true)`: build the worktree request
    /// on the event loop (cheap), run `git worktree add` on a worker thread, and
    /// register a [`PendingSpawn`] that [`Multiplexer::poll_pending_spawns`]
    /// finishes. The `reply` is *not* answered here — it is moved into the
    /// pending entry and sent once the spawn completes (or fails).
    pub(crate) fn begin_spawn_role_worktree(
        &mut self,
        role: String,
        model: Option<String>,
        agent_cli: Option<AgentCli>,
        prompt: Option<String>,
        reply: oneshot::Sender<ControlResponse>,
    ) {
        let req = self.role_worktree_request(&role);
        let (tx, rx) = std::sync::mpsc::channel();
        // The worker thread owns only the request; it touches no `Multiplexer`
        // state, so nothing `!Send` crosses the boundary.
        std::thread::spawn(move || {
            let result = create_worktree(&req).map_err(|e| format!("worktree create: {e}"));
            // The receiver lives in the event loop's `pending_spawns`; a send
            // failure just means the multiplexer is gone — nothing to do.
            let _ = tx.send(result);
        });
        self.pending_spawns.push(PendingSpawn {
            role,
            rx,
            model,
            agent_cli,
            prompt,
            reply,
        });
    }

    /// Finish every deferred spawn whose `git worktree add` has completed:
    /// launch its panel and answer the MCP call. Called once per event-loop
    /// tick. Entries whose worktree is still building are left in place.
    pub(crate) fn poll_pending_spawns(&mut self) {
        let mut i = 0;
        while i < self.pending_spawns.len() {
            let outcome = match self.pending_spawns[i].rx.try_recv() {
                Ok(result) => result,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // The worker thread vanished without a result (panic). No
                    // worktree was created, so there is nothing to clean up.
                    Err("worktree creation thread failed".to_string())
                }
            };
            let pending = self.pending_spawns.remove(i);
            let response = match outcome {
                Ok(handle) => match self.finish_spawn_role(
                    &pending.role,
                    pending.model.as_deref(),
                    pending.agent_cli,
                    pending.prompt.as_deref(),
                    Some(handle),
                ) {
                    Ok(panel) => ControlResponse::Spawned { panel },
                    Err(err) => ControlResponse::error(err),
                },
                Err(msg) => ControlResponse::error(msg),
            };
            // A dropped reply channel means the MCP client disconnected before
            // we answered — the spawn still happened, which is fine.
            let _ = pending.reply.send(response);
            // `remove(i)` shifted the next entry into `i`; do not advance.
        }
    }

    /// Tear down every in-flight deferred spawn on shutdown. Each worker thread
    /// is almost done (`git worktree add` is the slow step we are waiting on),
    /// so block briefly for its result; any worktree it created is removed
    /// synchronously here — the panel will never launch, so the dir+branch must
    /// not be left behind (the cleanup queue's consumer is torn down with the
    /// runtime, so deferring is not an option). Each blocked MCP call is told
    /// the multiplexer is shutting down.
    pub(crate) fn abort_pending_spawns(&mut self) {
        for pending in self.pending_spawns.drain(..) {
            // The worker thread sends exactly once then exits; recv blocks only
            // until `git worktree add` returns.
            if let Ok(Ok(handle)) = pending.rx.recv() {
                let summary = crate::worktree::cleanup::run_blocking(&CleanupJob {
                    repo_root: self.session.repo_path.clone(),
                    worktree_paths: vec![handle.path],
                    // The agent never ran — delete the empty branch too, and
                    // credit the removal to nobody.
                    branches_to_delete: vec![handle.branch],
                    owners: Vec::new(),
                    done: None,
                });
                for (path, msg) in &summary.failed_worktrees {
                    warn!(?path, %msg, "pending-spawn worktree cleanup on shutdown failed");
                }
            }
            let _ = pending.reply.send(ControlResponse::error(
                "caucus multiplexer is shutting down",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::mcp::protocol::ControlResponse;
    use crate::session::runtime::test_support::*;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::oneshot;

    /// A temp git repo so `git worktree add` succeeds.
    fn init_git_repo(dir: &std::path::Path) {
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .expect("run git");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "caucus-test"]);
        git(&["commit", "--allow-empty", "-q", "-m", "init"]);
    }

    /// Drive `poll_pending_spawns` until the queue drains or the budget runs
    /// out. Returns whether it drained.
    async fn drain(mux: &mut crate::session::Multiplexer) -> bool {
        for _ in 0..200 {
            mux.poll_pending_spawns();
            if mux.pending_spawns.is_empty() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        mux.pending_spawns.is_empty()
    }

    /// An in-flight deferred spawn reserves its per-role sequence number, so a
    /// second concurrent same-role spawn does not compute the same branch name
    /// and collide on `git worktree add`. This is the concurrency invariant the
    /// async path introduces (the synchronous path could not interleave).
    #[tokio::test]
    async fn deferred_spawn_reserves_the_next_role_count() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let first = mux.role_worktree_request("worker");
        assert!(first.branch.as_ref().unwrap().ends_with("-1"));

        // Register an in-flight spawn (its git add will fail outside a repo, but
        // the pending entry is recorded immediately — we never poll it).
        let (tx, _rx) = oneshot::channel();
        mux.begin_spawn_role_worktree("worker".into(), None, None, None, tx);
        assert_eq!(mux.pending_spawns.len(), 1);

        let second = mux.role_worktree_request("worker");
        assert!(
            second.branch.as_ref().unwrap().ends_with("-2"),
            "the in-flight spawn must reserve -1 so the next is -2: {:?}",
            second.branch
        );
    }

    /// A deferred spawn whose `git worktree add` fails (outside a repo) drains
    /// and answers the blocked MCP call with an error rather than hanging.
    #[tokio::test]
    async fn deferred_spawn_worktree_failure_replies_error() {
        let tmp = TempDir::new().unwrap(); // not a git repo
        let mut mux = mux(&tmp);

        let (tx, rx) = oneshot::channel();
        mux.begin_spawn_role_worktree("reviewer".into(), None, None, None, tx);
        assert!(
            drain(&mut mux).await,
            "a failed worktree create still drains"
        );

        let resp = rx.await.expect("a reply is delivered");
        assert!(
            matches!(resp, ControlResponse::Error { .. }),
            "git worktree add outside a repo must fail the spawn: {resp:?}"
        );
    }

    /// In a real repo the off-thread `git worktree add` completes and
    /// `poll_pending_spawns` delivers exactly one reply, then drains. (With an
    /// agent CLI on PATH the panel launches → `Spawned`; without one the spawn
    /// fails after the worktree is created → `Error`. Either way the deferral
    /// delivered a reply off the event loop, which is the contract.)
    #[tokio::test]
    async fn deferred_spawn_in_a_repo_delivers_a_reply_and_drains() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path());
        let mut mux = mux(&tmp);

        let (tx, rx) = oneshot::channel();
        mux.begin_spawn_role_worktree("reviewer".into(), None, None, None, tx);
        assert_eq!(
            mux.pending_spawns.len(),
            1,
            "the deferred spawn is registered"
        );

        assert!(drain(&mut mux).await, "the deferred spawn must drain");
        let resp = rx.await.expect("a reply is delivered");
        assert!(matches!(
            resp,
            ControlResponse::Spawned { .. } | ControlResponse::Error { .. }
        ));

        mux.shutdown();
    }

    /// `abort_pending_spawns` (run on shutdown) blocks for the worker thread,
    /// removes the orphan worktree it created (the panel will never launch), and
    /// answers the blocked MCP call — no leak, no hang.
    #[tokio::test]
    async fn shutdown_aborts_pending_spawns_and_cleans_their_worktrees() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path());
        let mut mux = mux(&tmp);
        let worktrees = tmp.path().join(".caucus").join("worktrees");

        let (tx, rx) = oneshot::channel();
        mux.begin_spawn_role_worktree("reviewer".into(), None, None, None, tx);

        // Tear down without ever polling: abort owns the cleanup.
        mux.abort_pending_spawns();
        assert!(mux.pending_spawns.is_empty());

        let resp = rx.await.expect("a reply is delivered");
        assert!(
            matches!(resp, ControlResponse::Error { .. }),
            "an aborted spawn is answered with a shutdown error: {resp:?}"
        );
        let empty = std::fs::read_dir(&worktrees)
            .map(|mut d| d.next().is_none())
            .unwrap_or(true);
        assert!(empty, "the orphan worktree must be removed synchronously");
    }
}
