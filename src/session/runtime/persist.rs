use super::*;
use crate::panel::lifecycle;
use crate::role::spec::AgentCli;
use crate::worktree::cleanup::CleanupJob;
use tracing::warn;

impl Multiplexer {
    /// Build a [`SessionRecord`](crate::session::record::SessionRecord) from the live panels + manifests.
    fn build_record(&self) -> crate::session::record::SessionRecord {
        use crate::session::record::{PanelRecord, SessionRecord};
        let panels = self
            .panels
            .iter()
            .enumerate()
            .map(|(idx, panel)| {
                let manifest = self.manifests.get(&panel.id);
                PanelRecord {
                    role: panel.role.clone(),
                    agent_cli: manifest.map(|m| m.agent_cli).unwrap_or(AgentCli::Claude),
                    model: manifest.and_then(|m| m.model.clone()),
                    order_index: idx,
                    is_main: self.main_panel_id == Some(panel.id),
                    worktree_branch: self.worktree_branches.get(&panel.id).cloned(),
                    claude_session_id: manifest
                        .and_then(|m| m.claude_session_id().map(str::to_string)),
                }
            })
            .collect();
        SessionRecord {
            id: self.session.id,
            topic: self.session.topic.clone(),
            repo_path: self.session.repo_path.clone(),
            created_at: self.session.created_at,
            layout_mode: self.layout_mode,
            panels,
        }
    }

    /// Persist the session roster to `<session_root>/session.json`. Called
    /// after every roster change so a relaunch can resume from the latest
    /// state. A write failure is logged, not fatal — a stale record is better
    /// than aborting the session.
    pub fn persist_record(&self) {
        let record = self.build_record();
        if let Err(err) = record.write(&self.session.root_dir) {
            warn!(error = %err, "session record write failed");
        }
    }

    /// Kill every panel and close the session — used on shutdown so no agent
    /// process is orphaned.
    ///
    /// The `Active -> Closed` transition goes through `session::state::transition`
    /// (Invariant I-1, the single owner of session state).
    pub fn shutdown(&mut self) {
        // Tear down any in-flight deferred worktree spawns first: block for each
        // worker thread's `git worktree add`, remove the orphan it created (the
        // panel will never launch), and answer the blocked MCP call. Done before
        // `persist_record` so the record reflects no half-spawned panels.
        self.abort_pending_spawns();

        // Persist the final roster before tearing panels down — this is the
        // record `caucus resume` reads. Worktree directories are removed
        // below, but their branches persist, so the record stays resumable.
        self.persist_record();

        // Kill every panel's PTY and collect its worktree. `kill_panel` is
        // avoided here: it enqueues worktree cleanup onto the async queue,
        // whose consumer task is aborted with the tokio runtime the instant
        // the event loop returns — so the worktrees would never be removed.
        let mut worktrees: Vec<PathBuf> = Vec::new();
        for panel in &mut self.panels {
            if let Err(err) = lifecycle::kill(panel) {
                warn!(panel = %panel.id, error = %err, "panel kill on shutdown failed");
            }
            if let Some(wt) = panel.worktree_path.clone() {
                worktrees.push(wt);
            }
        }
        self.panels.clear();
        self.manifests.clear();

        // Clean the worktrees synchronously, before the runtime goes away.
        // Branches are kept (`branches_to_delete` empty) — they hold the
        // sub-agents' commits and merging is the user's call (`docs/design.md` §5).
        if !worktrees.is_empty() {
            let summary = crate::worktree::cleanup::run_blocking(&CleanupJob {
                repo_root: self.session.repo_path.clone(),
                worktree_paths: worktrees,
                branches_to_delete: Vec::new(),
                done: None,
            });
            for (path, msg) in &summary.failed_worktrees {
                warn!(?path, %msg, "worktree cleanup on shutdown failed");
            }
        }

        if self.session.state() == crate::session::state::SessionState::Active {
            let _ = crate::session::state::transition(
                &mut self.session,
                crate::session::state::SessionState::Closed,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::input::CaucusCommand;
    use crate::mcp::protocol::{ControlRequest, ControlResponse};
    use crate::panel::lifecycle::{Panel, PanelState};
    use crate::pty::{Pty, PtyCommand};
    use crate::render::LayoutMode;
    use crate::session::id::AgentId;
    use crate::session::runtime::test_support::*;
    use crate::term::{Grid, OutputCapture};
    use tempfile::TempDir;

    fn push_cat_panel(mux: &mut super::Multiplexer, role: &str) -> crate::session::PanelId {
        let id = crate::session::PanelId::new();
        let inner = area().inner();
        let pty = Pty::spawn(&PtyCommand::new("/bin/cat"), inner.width, inner.height).unwrap();
        mux.panels.push(Panel {
            id,
            role: role.to_string(),
            agent_id: AgentId::new(),
            state: PanelState::Idle,
            worktree_path: None,
            pty,
            grid: Grid::new(inner.width as usize, inner.height as usize),
            capture: OutputCapture::new(),
        });
        mux.rebuild_layout_tree();
        id
    }

    /// The multiplexer writes `session.json` on `shutdown`, and the record
    /// round-trips back through `SessionRecord::read`.
    #[tokio::test]
    async fn shutdown_persists_a_session_record() {
        use crate::session::record::SessionRecord;
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let root = mux.session.root_dir.clone();
        let id = mux.session.id;

        mux.shutdown();

        let record = SessionRecord::read(&root).expect("session.json written on shutdown");
        assert_eq!(record.id, id);
        assert_eq!(record.layout_mode, LayoutMode::Tiled);
        assert!(record.panels.is_empty(), "no panels were spawned");
    }

    /// A layout-mode change persists the new mode into `session.json`.
    #[tokio::test]
    async fn cycle_layout_persists_the_record() {
        use crate::session::record::SessionRecord;
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let root = mux.session.root_dir.clone();

        mux.apply_command(CaucusCommand::CycleLayout);
        let record = SessionRecord::read(&root).expect("session.json written");
        assert_eq!(record.layout_mode, LayoutMode::EvenHorizontal);
    }

    /// The main worker identity is persisted independently from panel order:
    /// moving the main panel later must not make panel 0 become main on resume.
    #[tokio::test]
    async fn build_record_marks_main_independent_of_order_index() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let other = push_cat_panel(&mut mux, "worker");
        let main = push_cat_panel(&mut mux, "main");
        mux.main_panel_id = Some(main);

        let record = mux.build_record();
        assert_eq!(record.panels[0].order_index, 0);
        assert_eq!(record.panels[0].role, "worker");
        assert_ne!(other, main);
        assert!(
            !record.panels[0].is_main,
            "panel 0 must not become main just because it is first"
        );
        assert!(
            record.panels[1].is_main,
            "the main marker must follow main_panel_id, not order_index"
        );

        mux.shutdown();
    }

    /// `shutdown` must remove worktrees synchronously — the async cleanup
    /// queue is aborted with the tokio runtime the instant the event loop
    /// returns, so an enqueued cleanup would never run and the worktree would
    /// leak on every caucus exit.
    #[tokio::test]
    async fn shutdown_cleans_up_worktrees_synchronously() {
        let tmp = TempDir::new().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(tmp.path())
                .args(args)
                .output()
                .expect("run git");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "caucus-test"]);
        git(&["commit", "--allow-empty", "-q", "-m", "init"]);

        let mut mux = mux(&tmp);
        let resp = mux.execute_control(ControlRequest::SpawnRole {
            role: "worker".into(),
            worktree: true,
            model: None,
            agent_cli: None,
            prompt: None,
        });
        let panel_id = match resp {
            ControlResponse::Spawned { panel } => panel,
            // No `claude` on PATH / spawn failed — cannot exercise shutdown
            // worktree cleanup here; skip rather than fail spuriously.
            _ => {
                mux.shutdown();
                return;
            }
        };
        let wt = mux
            .panels()
            .iter()
            .find(|p| p.id == panel_id)
            .and_then(|p| p.worktree_path.clone())
            .expect("worktree panel has a worktree path");
        assert!(wt.is_dir(), "worktree directory created");

        mux.shutdown();
        assert!(
            !wt.exists(),
            "shutdown must remove the worktree, not leak it"
        );
    }
}
