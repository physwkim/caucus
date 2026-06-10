use super::*;
use crate::agent::manifest::{self};
use crate::agent::spawn::{CaucusMcp, SpawnRequest};
use crate::mcp::McpError;
use crate::panel::lifecycle;
use crate::render::Layout;
use crate::role::spec::{AgentCli, RoleSpec};
use crate::session::id::PanelId;
use crate::worktree::cleanup::CleanupJob;
use crate::worktree::manager::{
    WorktreeHandle, WorktreeRequest, create as create_worktree, role_worktree_stem,
};
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
    /// Inline system prompt for a free-form role (`docs/design.md` §6). When
    /// `Some`, it *is* the role's instructions and replaces the spec's prompt
    /// template; when `None`, the template is resolved as before.
    inline_prompt: Option<String>,
    /// caucus MCP server registration — set only for the main worker panel, so
    /// a codex-backed main worker can drive the sub-agents (codex registers it
    /// via `-c mcp_servers.caucus.*`; claude uses `mcp_config_path`). `None` for
    /// every sub-agent panel.
    caucus_mcp: Option<CaucusMcp>,
}

/// The worktree a [`Multiplexer::detach_panel`] left behind, for the caller to
/// dispose of: `kill_panel` enqueues it for deletion, `restart_panel` reuses it
/// in place. `worktree_path` is `None` for a panel sharing the main checkout.
struct DetachedPanel {
    worktree_path: Option<PathBuf>,
    worktree_branch: Option<String>,
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
            inline_prompt: None,
            caucus_mcp: None,
        })?;
        self.persist_record();
        Ok(id)
    }

    /// Spawn the main worker panel (`docs/design.md` §0 #4, #10).
    ///
    /// `agent_cli` selects the main worker's backend — `None` or
    /// `Some(Claude)` is the default Claude Code main worker; `Some(Codex)`
    /// runs codex as the orchestrator. Either way the main worker is registered
    /// with the caucus MCP server so it can drive the sub-agent panels through
    /// the caucus MCP tools (claude via `--mcp-config`, codex via
    /// `-c mcp_servers.caucus.*`). `caucus_bin` is the absolute path of the
    /// running `caucus` binary so the `mcp-serve` child is the exact same build.
    pub fn spawn_main_panel(
        &mut self,
        role: &str,
        caucus_bin: &std::path::Path,
        agent_cli: Option<AgentCli>,
    ) -> Result<PanelId> {
        let id = self.spawn_main_panel_resume(role, caucus_bin, None, agent_cli)?;
        self.persist_record();
        Ok(id)
    }

    /// Spawn the main worker panel, optionally resuming its prior Claude
    /// conversation via `resume_session_id` (`caucus resume`). `agent_cli`
    /// selects the backend (see [`Multiplexer::spawn_main_panel`]). The record
    /// is *not* persisted here — the resume path persists once, after the whole
    /// roster is rebuilt.
    pub fn spawn_main_panel_resume(
        &mut self,
        role: &str,
        caucus_bin: &std::path::Path,
        resume_session_id: Option<String>,
        agent_cli: Option<AgentCli>,
    ) -> Result<PanelId> {
        // The caucus MCP server registration — both backends need it, delivered
        // through their own mechanism. codex consumes `caucus_mcp` directly;
        // claude needs an on-disk `.mcp.json` it is pointed at with
        // `--mcp-config`, so write that file only for a claude main worker (a
        // codex main ignores it).
        let caucus_mcp = CaucusMcp {
            caucus_bin: caucus_bin.to_path_buf(),
            control_sock: self.control_sock_path.clone(),
        };
        let mcp_config_path = if agent_cli == Some(AgentCli::Codex) {
            None
        } else {
            Some(
                crate::mcp::serve::write_mcp_config(
                    &self.session.root_dir,
                    caucus_bin,
                    &self.control_sock_path,
                )
                .context("write main worker panel .mcp.json")?,
            )
        };
        let id = self.spawn_panel_inner(SpawnPanelOpts {
            role,
            agent_cli,
            model: None,
            worktree_path: None,
            worktree_branch: None,
            mcp_config_path,
            resume_session_id,
            inline_prompt: None,
            caucus_mcp: Some(caucus_mcp),
        })?;
        // The main worker panel is caucus's round-delivery target.
        self.main_panel_id = Some(id);
        Ok(id)
    }

    /// Spawn a panel restoring a prior agent — used by `caucus resume` — or, on
    /// the live `spawn_role` path, an ad-hoc panel carrying an inline system
    /// prompt. The record is *not* persisted here; the resume path persists
    /// once after the full roster is rebuilt.
    ///
    /// `inline_prompt`, when `Some`, becomes the role's system prompt and
    /// replaces the spec's prompt template (`docs/design.md` §6); `None` keeps
    /// the template-resolved prompt. The resume path passes `None` (a restored
    /// agent keeps its preset role's prompt).
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_panel_resume(
        &mut self,
        role: &str,
        agent_cli: Option<AgentCli>,
        model: Option<String>,
        worktree_path: Option<PathBuf>,
        worktree_branch: Option<String>,
        resume_session_id: Option<String>,
        inline_prompt: Option<String>,
    ) -> Result<PanelId> {
        self.spawn_panel_inner(SpawnPanelOpts {
            role,
            agent_cli,
            model,
            worktree_path,
            worktree_branch,
            mcp_config_path: None,
            resume_session_id,
            inline_prompt,
            caucus_mcp: None,
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
            inline_prompt,
            caucus_mcp,
        } = opts;

        // `role` is a free-form label (`docs/design.md` §6): a known preset
        // reuses its spec (tool allowlist + permission mode), any other name is
        // an ad-hoc role built on the generic `worker` defaults under that
        // label. So the main worker is never limited to the fixed roster — it
        // names a role on the fly and defines it with an inline prompt.
        let spec = match self.config.roles.get(role) {
            Ok(s) => s.clone(),
            Err(_) => self.generic_role_spec(role),
        };

        // The role's system prompt. An inline prompt (a free-form role's
        // instructions) wins outright and replaces the template; otherwise the
        // spec's template is resolved. Resolving is fallible I/O kept here so
        // build_command stays infallible — embedded defaults resolve without
        // disk; a missing user template fails the spawn with a clear error
        // rather than spawning an agent silently stripped of its role guidance.
        let system_prompt = match inline_prompt {
            Some(p) => Some(p),
            None => {
                crate::role::prompt::resolve(&spec.system_prompt_template, &self.session.repo_path)
                    .with_context(|| {
                        format!(
                            "resolve system prompt '{}' for role '{role}'",
                            spec.system_prompt_template
                        )
                    })?
            }
        };

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
            caucus_mcp,
        };

        // Provisional layout: compute the slot the new panel will occupy so
        // its PTY is sized correctly from the first byte.
        let mut ids: Vec<PanelId> = self.panels.iter().map(|p| p.id).collect();
        let outcome = crate::agent::spawn::spawn(&request)
            .map_err(|e| anyhow::anyhow!("agent spawn: {e}"))?;

        // codex stalls on its interactive "Do you trust this directory?" gate
        // the first time it runs in a directory, before the agent turn caucus
        // drives — nothing answers it and the panel hangs. Pre-grant trust for
        // the panel's cwd in codex's config (the same entry codex persists on
        // "Yes"; codex honors only its on-disk config, not a `-c` override).
        // Best-effort: a failure must not fail the spawn — the panel still
        // launches and the user can answer the gate by hand.
        if request.effective_cli() == AgentCli::Codex {
            if let Some(cwd) = outcome.command.cwd.as_deref() {
                if let Err(err) = crate::agent::codex_trust::ensure_trusted(cwd) {
                    warn!(
                        cwd = %cwd.display(),
                        error = %err,
                        "codex directory-trust pre-grant failed; codex may stall on its trust gate"
                    );
                }
            }
        }

        ids.push(outcome.panel_id);
        let provisional = Layout::reflow(&ids, self.area, self.layout_mode);
        let rect = provisional.rect_of(outcome.panel_id).unwrap_or(self.area);

        let mut panel = lifecycle::spawn(
            &request,
            outcome.command,
            outcome.panel_id,
            outcome.manifest.agent_id,
            rect,
            &self.config.settings,
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
    /// Single owner of panel destruction (Invariant I-5). The registry removal
    /// itself lives in [`Multiplexer::detach_panel`]; `kill_panel` is the
    /// disposition that *deletes* the detached worktree. [`Multiplexer::restart_panel`]
    /// is the other disposition — it *reuses* the worktree in place.
    pub fn kill_panel(&mut self, panel_id: PanelId) -> Result<()> {
        let detached = self.detach_panel(panel_id)?;

        // Enqueue the worktree for serial cleanup (Invariant I-3). The branch
        // is kept (not in `branches_to_delete`) — `caucus resume` re-attaches a
        // fresh worktree on it.
        if let Some(worktree) = detached.worktree_path {
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
        Ok(())
    }

    /// Restart a sub-agent panel in place: tear it down and spawn a fresh agent
    /// that *resumes* the same conversation (`claude_session_id`) in the same
    /// worktree, under the same role / model / backend. Returns the NEW panel
    /// id (a fresh PTY is a fresh id).
    ///
    /// Unlike kill + `spawn_role`, this preserves the panel's worktree — the
    /// branch and its commits (and any uncommitted changes) stay checked out in
    /// place, so the new agent picks up exactly where the old one left off — and
    /// its agent session, so a wedged agent (OOM, a hung transport, a crashed
    /// CLI) comes back with its context intact.
    ///
    /// The MAIN worker panel cannot be restarted: it is the caller of this very
    /// tool and caucus's round-delivery target, so tearing it down mid-call
    /// would orphan the request. That is an error, not a silent no-op.
    pub fn restart_panel(&mut self, panel_id: PanelId) -> Result<PanelId> {
        if self.main_panel_id == Some(panel_id) {
            anyhow::bail!("cannot restart the main worker panel");
        }
        // Capture the agent identity to resume *before* the teardown drops the
        // manifest. The role label (not the agent_name) is what the spawn path
        // wants — it derives a fresh `role-N` name.
        let manifest = self
            .manifests
            .get(&panel_id)
            .ok_or_else(|| anyhow::anyhow!("no such panel: {panel_id}"))?;
        let role = manifest.role.clone();
        let agent_cli = Some(manifest.agent_cli);
        let model = manifest.model.clone();
        let resume_session_id = manifest.claude_session_id().map(str::to_string);

        // Tear down through the single owner; reuse — do NOT clean up — the
        // worktree it leaves behind.
        let detached = self.detach_panel(panel_id)?;

        let new_id = self.spawn_panel_resume(
            &role,
            agent_cli,
            model,
            detached.worktree_path,
            detached.worktree_branch,
            resume_session_id,
            // A restarted panel keeps its preset role's prompt; the inline
            // prompt is a live-`spawn_role` concern only.
            None,
        )?;
        // `spawn_panel_resume` does not persist; persist the rebuilt roster now
        // that the replacement panel is live.
        self.persist_record();
        Ok(new_id)
    }

    /// Remove a panel from the live registry — the single owner of that
    /// transition (Invariant I-5): tear down its PTY, drop it from `panels` +
    /// `manifests` + the side maps, fix focus / zoom / the main-pointer, reflow,
    /// and persist the rebuilt roster.
    ///
    /// Returns the worktree the panel occupied (path + branch), if any, so the
    /// caller decides its fate rather than this function reaching into a shared
    /// queue: [`Multiplexer::kill_panel`] enqueues it for deletion,
    /// [`Multiplexer::restart_panel`] reuses it. This keeps the registry
    /// teardown in one place while leaving the worktree disposition explicit at
    /// each call site.
    fn detach_panel(&mut self, panel_id: PanelId) -> Result<DetachedPanel> {
        let Some(idx) = self.panels.iter().position(|p| p.id == panel_id) else {
            anyhow::bail!("no such panel: {panel_id}");
        };
        let mut panel = self.panels.remove(idx);
        lifecycle::kill(&mut panel)?;

        let detached = DetachedPanel {
            worktree_path: panel.worktree_path.clone(),
            worktree_branch: self.worktree_branches.remove(&panel_id),
        };
        self.manifests.remove(&panel_id);
        self.blocked_scan_cache.remove(&panel_id);

        // Keep `main_panel_id` an accurate invariant: it points to a live
        // panel or is None. Without this, killing main leaves it Some(stale),
        // so `poll_pending_rounds` neither delivers (the gate resolves the id
        // to no live panel) nor drops the round (its drop arm fires only when
        // the id is None) — every due round would re-queue forever.
        if self.main_panel_id == Some(panel_id) {
            self.main_panel_id = None;
        }

        // Detaching the zoomed panel clears the zoom — the layout falls back to
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
        Ok(detached)
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

    /// Spec for a free-form (non-preset) role label: the generic `worker`
    /// defaults — tool allowlist, permission mode, default backend — under the
    /// caller's `role` name (`docs/design.md` §6). The main worker pairs this
    /// with an inline `prompt` to invent a role on the fly without it being in
    /// any `roles.toml`.
    ///
    /// Derived from the live `worker` preset so a `roles.toml` override of
    /// `worker` carries through; falls back to a built-in generic spec only if
    /// `worker` was somehow removed from the registry (the embedded defaults
    /// make that practically impossible, but the fallback keeps the free-form
    /// path total — a label can never fail to resolve to a spec).
    fn generic_role_spec(&self, role: &str) -> RoleSpec {
        let mut spec = self
            .config
            .roles
            .get("worker")
            .cloned()
            .unwrap_or_else(|_| RoleSpec {
                name: role.to_string(),
                description: "Ad-hoc sub-agent role.".to_string(),
                allowed_tools: ["Read", "Glob", "Grep", "Edit", "Write", "Bash", "TodoWrite"]
                    .iter()
                    .map(|t| (*t).to_string())
                    .collect(),
                permission_mode: "acceptEdits".to_string(),
                system_prompt_template: "roles/worker.md".to_string(),
                agent_cli: AgentCli::Claude,
                model: Some("sonnet".to_string()),
            });
        // The label is the caller's, the defaults are worker's.
        spec.name = role.to_string();
        spec
    }

    /// Build the [`WorktreeRequest`] for a `spawn_role(worktree=true)` call —
    /// the cheap, event-loop-thread part of worktree creation (`docs/design.md`
    /// §5). The slow `git worktree add` is run from this request by
    /// [`Multiplexer::create_role_worktree`] (synchronous) or off-thread by
    /// [`Multiplexer::begin_spawn_role_worktree`] (the live socket path).
    ///
    /// The per-role sequence number that disambiguates the branch/path counts
    /// completed spawns (`role_counts`) **plus** spawns already in flight
    /// (`pending_spawns` for this role). Counting the in-flight ones is what
    /// keeps two concurrent same-role `spawn_role` calls from computing the same
    /// branch name and colliding on `git worktree add` — `role_counts` is only
    /// bumped later, when each deferred spawn actually launches its panel.
    pub(crate) fn role_worktree_request(&self, role: &str) -> WorktreeRequest {
        let inflight = self
            .pending_spawns
            .iter()
            .filter(|s| s.role == role)
            .count();
        let next = self.role_counts.get(role).copied().unwrap_or(0) + inflight + 1;
        let stem = role_worktree_stem(role);
        WorktreeRequest {
            repo_root: self.session.repo_path.clone(),
            session_id: self.session.id,
            role: role.to_string(),
            branch: Some(format!(
                "caucus/{}/{}-{}",
                session_suffix(&self.session),
                stem,
                next
            )),
            base_ref: None,
            // Disambiguate concurrent worktrees for the same role with the
            // per-role spawn counter.
            name_override: Some(format!(
                "{}-{}-{}",
                session_suffix(&self.session),
                stem,
                next,
            )),
        }
    }

    /// Create an execute-phase worktree for a `spawn_role(worktree=true)` call,
    /// synchronously (`docs/design.md` §5). Single owner of worktree creation is
    /// `worktree::manager::create` (Invariant I-3). Used by the synchronous
    /// `spawn_role` trait method and tests; the live socket path defers the
    /// `git worktree add` off the event loop instead
    /// ([`Multiplexer::begin_spawn_role_worktree`]).
    pub(crate) fn create_role_worktree(&self, role: &str) -> Result<WorktreeHandle, McpError> {
        let req = self.role_worktree_request(role);
        create_worktree(&req).map_err(|err| McpError::Tool(format!("worktree create: {err}")))
    }
}

#[cfg(test)]
mod tests {
    use crate::mcp::protocol::{ControlRequest, ControlResponse};
    use crate::session::id::PanelId;
    use crate::session::runtime::test_support::*;
    use tempfile::TempDir;

    /// `spawn_role(worktree=true)` must not leak the worktree when the panel
    /// spawn fails. An unknown role no longer fails (it becomes an ad-hoc role
    /// on the worker defaults), so the deterministic failure is a *known* role
    /// whose `system_prompt_template` points at a missing user file:
    /// `create_role_worktree` creates the worktree, then `resolve` fails in
    /// `spawn_panel_inner` before any PTY launch — exactly the orphan path. The
    /// worktree must be enqueued for cleanup.
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

        // A role whose system-prompt template is a non-embedded path that does
        // not exist on disk — `resolve` returns Err, failing the spawn after the
        // worktree is created.
        std::fs::create_dir_all(tmp.path().join(".caucus")).unwrap();
        std::fs::write(
            tmp.path().join(".caucus").join("roles.toml"),
            "[roles.bad-template]\n\
             description = \"missing template\"\n\
             allowed_tools = [\"Read\"]\n\
             permission_mode = \"default\"\n\
             system_prompt_template = \"roles/intentionally-missing.md\"\n",
        )
        .unwrap();

        let mut mux = mux(&tmp);
        let worktrees = tmp.path().join(".caucus").join("worktrees");

        let resp = mux.execute_control(ControlRequest::SpawnRole {
            role: "bad-template".into(),
            worktree: true,
            model: None,
            agent_cli: None,
            prompt: None,
        });
        assert!(
            matches!(resp, ControlResponse::Error { .. }),
            "a missing prompt template must fail the spawn: {resp:?}"
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

    /// A free-form (non-preset) role label resolves to the generic `worker`
    /// defaults under that label — so the main worker can name a role caucus
    /// has never heard of and pair it with an inline prompt.
    #[tokio::test]
    async fn generic_role_spec_uses_worker_defaults_under_the_caller_label() {
        let tmp = TempDir::new().unwrap();
        let mux = mux(&tmp);
        let worker = mux.config.roles.get("worker").unwrap().clone();

        let spec = mux.generic_role_spec("perf-profiler");
        assert_eq!(spec.name, "perf-profiler", "keeps the caller's label");
        assert_eq!(
            spec.allowed_tools, worker.allowed_tools,
            "inherits the worker tool allowlist"
        );
        assert_eq!(
            spec.permission_mode, worker.permission_mode,
            "inherits the worker permission mode"
        );
        assert!(
            !spec.allows_task(),
            "a free-form role must not grant the Task tool (Invariant I-7)"
        );
    }

    /// `spawn_role(worktree=true)` accepts free-form role labels: the visible
    /// role keeps its original text, while the git branch/path use a safe slug
    /// and a per-role counter so repeated worktree spawns do not collide.
    #[tokio::test]
    async fn create_role_worktree_sanitizes_labels_and_uses_unique_branches() {
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
        let role = "Perf Profiler: QA/2";
        let first = mux.create_role_worktree(role).unwrap();
        mux.role_counts.insert(role.to_string(), 1);
        let second = mux.create_role_worktree(role).unwrap();

        assert!(first.path.to_string_lossy().contains("perf-profiler-qa-2-"));
        assert!(
            second
                .path
                .to_string_lossy()
                .contains("perf-profiler-qa-2-")
        );
        assert!(first.path.to_string_lossy().ends_with("-1"));
        assert!(second.path.to_string_lossy().ends_with("-2"));
        assert!(first.branch.contains("/perf-profiler-qa-2-"));
        assert!(second.branch.contains("/perf-profiler-qa-2-"));
        assert!(first.branch.ends_with("-1"));
        assert!(second.branch.ends_with("-2"));
        assert_ne!(first.branch, second.branch);

        let summary =
            crate::worktree::cleanup::run_blocking(&crate::worktree::cleanup::CleanupJob {
                repo_root: tmp.path().to_path_buf(),
                worktree_paths: vec![first.path, second.path],
                branches_to_delete: vec![first.branch, second.branch],
                done: None,
            });
        assert!(summary.failed_worktrees.is_empty(), "{summary:?}");
        assert!(summary.failed_branches.is_empty(), "{summary:?}");
    }

    /// The main worker panel cannot be restarted — it is the caller and the
    /// round-delivery target, so tearing it down mid-call would orphan the
    /// request. The guard fires before any teardown (no live panel needed).
    #[tokio::test]
    async fn restart_panel_refuses_the_main_worker() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = PanelId::new();
        mux.main_panel_id = Some(main);

        let err = mux.restart_panel(main).unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot restart the main worker panel"),
            "got: {err}"
        );
    }

    /// Restarting a sub-agent panel replaces it with a fresh PTY (a new id) in
    /// the same registry slot — not a duplicate — and keeps its role. Spawning
    /// needs a real agent CLI; the test is skipped when none is on PATH.
    #[tokio::test]
    async fn restart_panel_replaces_the_panel_with_a_fresh_id() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Ok(old) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        assert_eq!(mux.panels().len(), 1);

        let new = mux.restart_panel(old).unwrap();
        assert_ne!(new, old, "a restart is a fresh PTY → a fresh id");
        assert_eq!(
            mux.panels().len(),
            1,
            "the panel is replaced in place, not duplicated"
        );
        assert!(mux.panels().iter().any(|p| p.id == new));
        assert!(!mux.panels().iter().any(|p| p.id == old));
        assert_eq!(
            mux.panels().iter().find(|p| p.id == new).unwrap().role,
            "reviewer",
            "the role is preserved across the restart"
        );

        mux.shutdown();
    }
}
