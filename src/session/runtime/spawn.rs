use super::*;
use crate::agent::manifest::{self};
use crate::agent::spawn::SpawnRequest;
use crate::mcp::McpError;
use crate::panel::lifecycle;
use crate::render::Layout;
use crate::role::spec::AgentCli;
use crate::session::id::PanelId;
use crate::worktree::cleanup::CleanupJob;
use crate::worktree::manager::{WorktreeHandle, WorktreeRequest, create as create_worktree};
use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::warn;

/// Arguments to the shared [`Multiplexer::spawn_panel_inner`] path. Bundling
/// them keeps the function arity sane as the spawn surface grows (worktree
/// branch + resume id for `caucus resume`).
struct SpawnPanelOpts<'a> {
    role: &'a str,
    agent_cli: Option<AgentCli>,
    model: Option<String>,
    worktree_path: Option<PathBuf>,
    worktree_branch: Option<String>,
    mcp_config_path: Option<PathBuf>,
    resume_session_id: Option<String>,
}

impl Multiplexer {
    /// Spawn a panel for `role`, optionally with a CLI/model override and a
    /// pre-created worktree.
    ///
    /// Returns the new panel id. Single owner of panel creation: delegates to
    /// `panel::lifecycle::spawn` (Invariant I-5) and persists a fresh manifest
    /// via `agent::manifest::write` (Invariant I-2).
    pub fn spawn_panel(
        &mut self,
        role: &str,
        agent_cli: Option<AgentCli>,
        model: Option<String>,
        worktree_path: Option<PathBuf>,
    ) -> Result<PanelId> {
        let id = self.spawn_panel_inner(SpawnPanelOpts {
            role,
            agent_cli,
            model,
            worktree_path,
            worktree_branch: None,
            mcp_config_path: None,
            resume_session_id: None,
        })?;
        self.persist_record();
        Ok(id)
    }

    /// Spawn the main worker panel (`docs/design.md` §0 #4, #10).
    ///
    /// Writes the caucus MCP config (`.mcp.json`) into the session root and
    /// registers it with the main worker's Claude Code instance via
    /// `--mcp-config`, so the main worker can drive the sub-agent panels
    /// through the six caucus MCP tools. `caucus_bin` is the absolute path of
    /// the running `caucus` binary so the `mcp-serve` child is the exact same
    /// build.
    pub fn spawn_main_panel(
        &mut self,
        role: &str,
        caucus_bin: &std::path::Path,
    ) -> Result<PanelId> {
        let id = self.spawn_main_panel_resume(role, caucus_bin, None)?;
        self.persist_record();
        Ok(id)
    }

    /// Spawn the main worker panel, optionally resuming its prior Claude
    /// conversation via `resume_session_id` (`caucus resume`). The record is
    /// *not* persisted here — the resume path persists once, after the whole
    /// roster is rebuilt.
    pub fn spawn_main_panel_resume(
        &mut self,
        role: &str,
        caucus_bin: &std::path::Path,
        resume_session_id: Option<String>,
    ) -> Result<PanelId> {
        let mcp_config = crate::mcp::serve::write_mcp_config(
            &self.session.root_dir,
            caucus_bin,
            &self.control_sock_path,
        )
        .context("write main worker panel .mcp.json")?;
        let id = self.spawn_panel_inner(SpawnPanelOpts {
            role,
            agent_cli: None,
            model: None,
            worktree_path: None,
            worktree_branch: None,
            mcp_config_path: Some(mcp_config),
            resume_session_id,
        })?;
        // The main worker panel is caucus's round-delivery target.
        self.main_panel_id = Some(id);
        Ok(id)
    }

    /// Spawn a panel restoring a prior agent — used by `caucus resume`. The
    /// record is *not* persisted here; the resume path persists once after the
    /// full roster is rebuilt.
    pub fn spawn_panel_resume(
        &mut self,
        role: &str,
        agent_cli: Option<AgentCli>,
        model: Option<String>,
        worktree_path: Option<PathBuf>,
        worktree_branch: Option<String>,
        resume_session_id: Option<String>,
    ) -> Result<PanelId> {
        self.spawn_panel_inner(SpawnPanelOpts {
            role,
            agent_cli,
            model,
            worktree_path,
            worktree_branch,
            mcp_config_path: None,
            resume_session_id,
        })
    }

    /// Shared spawn path. `mcp_config_path` is set only for the main worker
    /// panel; `worktree_branch` / `resume_session_id` only on the resume path.
    fn spawn_panel_inner(&mut self, opts: SpawnPanelOpts<'_>) -> Result<PanelId> {
        let SpawnPanelOpts {
            role,
            agent_cli,
            model,
            worktree_path,
            worktree_branch,
            mcp_config_path,
            resume_session_id,
        } = opts;
        let spec = self
            .config
            .roles
            .get(role)
            .with_context(|| format!("unknown role '{role}'"))?
            .clone();

        // Resolve the role's system prompt now (fallible I/O) so build_command
        // stays infallible. Embedded defaults resolve without disk; a missing
        // user template fails the spawn with a clear error rather than spawning
        // an agent silently stripped of its role guidance.
        let system_prompt =
            crate::role::prompt::resolve(&spec.system_prompt_template, &self.session.repo_path)
                .with_context(|| {
                    format!(
                        "resolve system prompt '{}' for role '{role}'",
                        spec.system_prompt_template
                    )
                })?;

        let count = self.role_counts.entry(role.to_string()).or_insert(0);
        *count += 1;
        let agent_name = format!("{role}-{count}");

        let request = SpawnRequest {
            session_id: self.session.id,
            role: spec,
            agent_name,
            agent_cli_override: agent_cli,
            model_override: model,
            worktree_path: worktree_path.clone(),
            repo_root: self.session.repo_path.clone(),
            session_dir: self.session.root_dir.clone(),
            sock_path: Some(self.sock_path.clone()),
            // Panels are non-interactive for the agent's own prompts; the
            // role allowlist remains the real boundary (`SpawnRequest` doc).
            skip_permissions: true,
            mcp_config_path,
            resume_session_id,
            system_prompt,
        };

        // Provisional layout: compute the slot the new panel will occupy so
        // its PTY is sized correctly from the first byte.
        let mut ids: Vec<PanelId> = self.panels.iter().map(|p| p.id).collect();
        let outcome = crate::agent::spawn::spawn(&request)
            .map_err(|e| anyhow::anyhow!("agent spawn: {e}"))?;
        ids.push(outcome.panel_id);
        let provisional = Layout::reflow(&ids, self.area, self.layout_mode);
        let rect = provisional.rect_of(outcome.panel_id).unwrap_or(self.area);

        let mut panel = lifecycle::spawn(
            &request,
            outcome.command,
            outcome.panel_id,
            outcome.manifest.agent_id,
            rect,
        )?;
        let panel_id = panel.id;
        // Turn-segmented capture spills to `<session>/panels/<panel>.log`
        // (`docs/design.md` §8.5).
        panel.set_capture_log_path(
            self.session
                .root_dir
                .join("panels")
                .join(format!("{panel_id}.log")),
        );

        // Persist the manifest (Invariant I-2).
        let mut mf = outcome.manifest;
        mf.worktree_path = worktree_path;
        if let Err(err) = manifest::write(&mut mf, &self.session.root_dir, None) {
            warn!(panel = %panel_id, error = %err, "manifest write failed");
        }
        self.manifests.insert(panel_id, mf);

        // Remember the worktree branch — the branch persists across shutdown
        // and is what `caucus resume` re-attaches a worktree on.
        if let Some(branch) = worktree_branch {
            self.worktree_branches.insert(panel_id, branch);
        }

        self.panels.push(panel);
        if self.focus.focused().is_none() {
            self.focus.set_focus(Some(panel_id));
        }
        self.rebuild_layout_tree();
        Ok(panel_id)
    }

    /// Kill a panel: tear down the PTY, drop it from the registry, enqueue any
    /// worktree for cleanup, and reflow (`docs/design.md` §5).
    ///
    /// Single owner of panel destruction (Invariant I-5).
    pub fn kill_panel(&mut self, panel_id: PanelId) -> Result<()> {
        let Some(idx) = self.panels.iter().position(|p| p.id == panel_id) else {
            anyhow::bail!("no such panel: {panel_id}");
        };
        let mut panel = self.panels.remove(idx);
        lifecycle::kill(&mut panel)?;

        // Enqueue the worktree for serial cleanup (Invariant I-3).
        if let Some(worktree) = panel.worktree_path.clone() {
            let job = CleanupJob {
                repo_root: self.session.repo_path.clone(),
                worktree_paths: vec![worktree],
                branches_to_delete: Vec::new(),
                done: None,
            };
            if self.cleanup.enqueue(job).is_err() {
                warn!(panel = %panel_id, "worktree cleanup queue closed");
            }
        }
        self.manifests.remove(&panel_id);
        self.worktree_branches.remove(&panel_id);

        // Keep `main_panel_id` an accurate invariant: it points to a live
        // panel or is None. Without this, killing main leaves it Some(stale),
        // so `poll_pending_rounds` neither delivers (the gate resolves the id
        // to no live panel) nor drops the round (its drop arm fires only when
        // the id is None) — every due round would re-queue forever.
        if self.main_panel_id == Some(panel_id) {
            self.main_panel_id = None;
        }

        // Killing the zoomed panel clears the zoom — the layout falls back to
        // the tiled arrangement rather than zooming a now-dead id.
        if self.zoom == Some(panel_id) {
            self.zoom = None;
        }

        // Refocus: keep focus valid after removal.
        if self.focus.focused() == Some(panel_id) {
            let next = self.panels.get(idx).or_else(|| self.panels.last());
            self.focus.set_focus(next.map(|p| p.id));
        }
        self.rebuild_layout_tree();
        self.persist_record();
        Ok(())
    }

    /// Arm the close-panel confirm prompt (`Ctrl-A x`) for the focused panel.
    ///
    /// The main worker panel is protected — it owns the MCP control channel and
    /// is the round-delivery target, so closing it is disallowed and the
    /// request is a no-op. With no focused panel it is also a no-op.
    pub(crate) fn arm_close_confirm(&mut self) {
        let Some(focused) = self.focus.focused() else {
            return;
        };
        if Some(focused) == self.main_panel_id {
            warn!(panel = %focused, "refusing to close the main worker panel");
            return;
        }
        self.pending_close = Some(focused);
        self.focus.set_confirm_open(true);
    }

    /// Confirm the pending close: kill the panel through [`Multiplexer::kill_panel`]
    /// (the single owner of panel destruction, Invariant I-5) and dismiss the
    /// prompt. The prompt is always dismissed, even if the kill fails.
    pub(crate) fn confirm_close(&mut self) {
        self.focus.set_confirm_open(false);
        if let Some(id) = self.pending_close.take() {
            if let Err(err) = self.kill_panel(id) {
                warn!(panel = %id, error = %err, "close-panel failed");
            }
        }
    }

    /// Cancel the pending close: dismiss the prompt, keep the panel.
    pub(crate) fn cancel_close(&mut self) {
        self.pending_close = None;
        self.focus.set_confirm_open(false);
    }

    /// Create an execute-phase worktree for a `spawn_role(worktree=true)` call
    /// (`docs/design.md` §5). Single owner of worktree creation is
    /// `worktree::manager::create` (Invariant I-3).
    ///
    /// `worktree::manager::create` is synchronous (`git worktree add` is a
    /// fast subprocess); the event loop calls it directly on its own thread —
    /// no async bridging, so no nested-runtime panic.
    pub(crate) fn create_role_worktree(&self, role: &str) -> Result<WorktreeHandle, McpError> {
        let req = WorktreeRequest {
            repo_root: self.session.repo_path.clone(),
            session_id: self.session.id,
            role: role.to_string(),
            branch: None,
            base_ref: None,
            // Disambiguate concurrent worktrees for the same role with the
            // per-role spawn counter.
            name_override: Some(format!(
                "{}-{}-{}",
                session_suffix(&self.session),
                role,
                self.role_counts.get(role).copied().unwrap_or(0) + 1,
            )),
        };
        create_worktree(&req).map_err(|err| McpError::Tool(format!("worktree create: {err}")))
    }
}

#[cfg(test)]
mod tests {
    use crate::mcp::protocol::{ControlRequest, ControlResponse};
    use crate::session::runtime::test_support::*;
    use tempfile::TempDir;

    /// `spawn_role(worktree=true)` must not leak the worktree when the panel
    /// spawn fails. `create_role_worktree` does not validate the role, so an
    /// unknown role creates the worktree and then fails in `spawn_panel` —
    /// exactly the orphan path. The worktree must be enqueued for cleanup.
    #[tokio::test]
    async fn spawn_role_failure_does_not_leak_the_worktree() {
        use std::time::Duration;

        // A temp git repo so `git worktree add` succeeds.
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
        let worktrees = tmp.path().join(".caucus").join("worktrees");

        let resp = mux.execute_control(ControlRequest::SpawnRole {
            role: "no-such-role-xyz".into(),
            worktree: true,
            model: None,
            agent_cli: None,
        });
        assert!(
            matches!(resp, ControlResponse::Error { .. }),
            "unknown role must fail: {resp:?}"
        );

        // Cleanup is a serial async queue — poll for the orphan's removal.
        let mut cleaned = false;
        for _ in 0..100 {
            let empty = std::fs::read_dir(&worktrees)
                .map(|mut d| d.next().is_none())
                .unwrap_or(true);
            if empty {
                cleaned = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(cleaned, "orphan worktree must be enqueued for cleanup");

        mux.shutdown();
    }
}
