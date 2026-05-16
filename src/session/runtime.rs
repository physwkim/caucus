//! Multiplexer runtime — the live object behind `caucus run_tui`.
//!
//! Ties the leaf modules into a running session: it owns the panel vector,
//! the [`crate::render::Layout`], the [`crate::input::FocusRouter`], the
//! turn-signal [`SignalServer`], and the worktree [`CleanupQueue`].
//!
//! Single-owner discipline (`docs/design.md` §9.1):
//! * panels are created/destroyed only through [`Multiplexer::spawn_panel`] /
//!   [`Multiplexer::kill_panel`], which delegate to `panel::lifecycle`;
//! * manifests are written only through `agent::manifest::write`;
//! * worktree removal is enqueued only through `worktree::cleanup`.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing::warn;

use crate::agent::manifest::{self, AgentManifest};
use crate::agent::spawn::SpawnRequest;
use crate::config::Config;
use crate::input::{CaucusCommand, FocusRouter};
use crate::mcp::control_server::{ControlJob, ControlServer};
use crate::mcp::protocol::{ControlRequest, ControlResponse};
use crate::mcp::{McpError, McpToolSurface, PanelSummary, ReadPanelMode};
use crate::panel::lifecycle::{self, Panel, PanelState};
use crate::render::{Layout, Rect};
use crate::role::spec::AgentCli;
use crate::session::id::PanelId;
use crate::session::state::Session;
use crate::signal::TurnSignal;
use crate::signal::server::SignalServer;
use crate::worktree::cleanup::{CleanupJob, CleanupQueue};
use crate::worktree::manager::{WorktreeRequest, create as create_worktree};

/// The live multiplexer: one [`Session`] plus every panel running in it.
pub struct Multiplexer {
    /// The session this multiplexer drives.
    pub session: Session,
    /// Merged role configuration (embedded + global + project).
    pub config: Config,
    /// Live panels, in spawn order — also the focus-cycle order.
    panels: Vec<Panel>,
    /// Per-panel agent manifest, keyed by panel id. Mutated only via
    /// `agent::manifest::write`.
    manifests: HashMap<PanelId, AgentManifest>,
    /// Current screen layout. Recomputed on every spawn/kill/resize.
    layout: Layout,
    /// Input focus + reserved-prefix state.
    focus: FocusRouter,
    /// Worktree cleanup queue (serial, off the UI path).
    cleanup: CleanupQueue,
    /// Turn-signal socket path injected into spawned agents as `CAUCUS_SOCK`.
    sock_path: PathBuf,
    /// Control socket path — the CEO panel's MCP server connects here
    /// (`docs/design.md` §0 #4). Wired into the CEO panel's `.mcp.json`.
    control_sock_path: PathBuf,
    /// Whole-screen area the layout tiles.
    area: Rect,
    /// Set when the user requested quit (`Ctrl-A q`).
    quit: bool,
    /// Monotonic counter for agent-name suffixes per role.
    role_counts: HashMap<String, usize>,
}

impl Multiplexer {
    /// Build a multiplexer for `session`, binding the turn-signal socket and
    /// the MCP control socket (`docs/design.md` §0 #4).
    ///
    /// The session root directory and its `agents/` + `panels/` subdirectories
    /// are created here so manifest writes and capture spills have a home.
    ///
    /// Returns the multiplexer plus the two socket servers the event loop
    /// drains: the [`SignalServer`] (turn signals) and the [`ControlServer`]
    /// (CEO MCP tool calls).
    pub fn new(
        session: Session,
        config: Config,
        area: Rect,
    ) -> Result<(Self, SignalServer, ControlServer)> {
        std::fs::create_dir_all(session.root_dir.join("agents"))
            .with_context(|| format!("create {}", session.root_dir.display()))?;
        std::fs::create_dir_all(session.root_dir.join("panels"))?;

        let sock_path = socket_path(&session);
        let signal_server = SignalServer::bind(&sock_path)
            .with_context(|| format!("bind turn-signal socket {}", sock_path.display()))?;

        let control_sock_path = control_socket_path(&session);
        let control_server = ControlServer::bind(&control_sock_path)
            .with_context(|| format!("bind control socket {}", control_sock_path.display()))?;

        let (cleanup, _consumer) = CleanupQueue::spawn();

        Ok((
            Self {
                session,
                config,
                panels: Vec::new(),
                manifests: HashMap::new(),
                layout: Layout::default(),
                focus: FocusRouter::new(),
                cleanup,
                sock_path,
                control_sock_path,
                area,
                quit: false,
                role_counts: HashMap::new(),
            },
            signal_server,
            control_server,
        ))
    }

    /// The MCP control socket path — wired into the CEO panel's `.mcp.json`.
    pub fn control_sock_path(&self) -> &std::path::Path {
        &self.control_sock_path
    }

    /// Live panels, in focus-cycle order.
    pub fn panels(&self) -> &[Panel] {
        &self.panels
    }

    /// The current layout.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The focused panel id, if any.
    pub fn focused(&self) -> Option<PanelId> {
        self.focus.focused()
    }

    /// Whether a quit was requested.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// The turn-signal socket path.
    pub fn sock_path(&self) -> &std::path::Path {
        &self.sock_path
    }

    /// Resize the whole-screen area and reflow every panel's PTY + grid.
    pub fn resize(&mut self, area: Rect) -> Result<()> {
        self.area = area;
        self.reflow();
        Ok(())
    }

    /// Recompute the layout for the current panels and resize each panel's
    /// PTY/grid to its new slot (`docs/design.md` §0 #10).
    fn reflow(&mut self) {
        let ids: Vec<PanelId> = self.panels.iter().map(|p| p.id).collect();
        self.layout = Layout::reflow(&ids, self.area);
        for panel in &mut self.panels {
            if let Some(rect) = self.layout.rect_of(panel.id) {
                if let Err(err) = panel.resize(rect) {
                    warn!(panel = %panel.id, error = %err, "panel resize failed");
                }
            }
        }
    }

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
        self.spawn_panel_inner(role, agent_cli, model, worktree_path, None)
    }

    /// Spawn the CEO panel (`docs/design.md` §0 #4, #10).
    ///
    /// Writes the caucus MCP config (`.mcp.json`) into the session root and
    /// registers it with the CEO's Claude Code instance via `--mcp-config`, so
    /// the CEO can drive the other panels through the six caucus MCP tools.
    /// `caucus_bin` is the absolute path of the running `caucus` binary so the
    /// `mcp-serve` child is the exact same build.
    pub fn spawn_ceo_panel(&mut self, role: &str, caucus_bin: &std::path::Path) -> Result<PanelId> {
        let mcp_config = crate::mcp::serve::write_mcp_config(
            &self.session.root_dir,
            caucus_bin,
            &self.control_sock_path,
        )
        .context("write CEO panel .mcp.json")?;
        self.spawn_panel_inner(role, None, None, None, Some(mcp_config))
    }

    /// Shared spawn path for [`Multiplexer::spawn_panel`] and
    /// [`Multiplexer::spawn_ceo_panel`]; `mcp_config_path` is set only for the
    /// CEO panel.
    fn spawn_panel_inner(
        &mut self,
        role: &str,
        agent_cli: Option<AgentCli>,
        model: Option<String>,
        worktree_path: Option<PathBuf>,
        mcp_config_path: Option<PathBuf>,
    ) -> Result<PanelId> {
        let spec = self
            .config
            .roles
            .get(role)
            .with_context(|| format!("unknown role '{role}'"))?
            .clone();

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
            sock_path: Some(self.sock_path.clone()),
            // Panels are non-interactive for the agent's own prompts; the
            // role allowlist remains the real boundary (`SpawnRequest` doc).
            skip_permissions: true,
            mcp_config_path,
        };

        // Provisional layout: compute the slot the new panel will occupy so
        // its PTY is sized correctly from the first byte.
        let mut ids: Vec<PanelId> = self.panels.iter().map(|p| p.id).collect();
        let outcome = crate::agent::spawn::spawn(&request)
            .map_err(|e| anyhow::anyhow!("agent spawn: {e}"))?;
        ids.push(outcome.panel_id);
        let provisional = Layout::reflow(&ids, self.area);
        let rect = provisional
            .rect_of(outcome.panel_id)
            .unwrap_or(self.area);

        let mut panel = lifecycle::spawn(
            &request,
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

        self.panels.push(panel);
        if self.focus.focused().is_none() {
            self.focus.set_focus(Some(panel_id));
        }
        self.reflow();
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

        // Refocus: keep focus valid after removal.
        if self.focus.focused() == Some(panel_id) {
            let next = self.panels.get(idx).or_else(|| self.panels.last());
            self.focus.set_focus(next.map(|p| p.id));
        }
        self.reflow();
        Ok(())
    }

    /// Drain every panel's PTY into its grid + capture, and reap panels whose
    /// agent process has exited. Called once per event-loop tick.
    pub fn pump_all(&mut self) {
        let mut exited = Vec::new();
        for panel in &mut self.panels {
            match panel.pump() {
                Ok(n) => {
                    // First output from a freshly-spawned agent: its CLI
                    // process is alive and drawing its UI — it has left
                    // `Spawning` and is now an idle agent awaiting its first
                    // instruction.
                    if n > 0 && panel.state() == PanelState::Spawning {
                        let _ = lifecycle::transition(panel, PanelState::Idle);
                    }
                }
                Err(err) => {
                    warn!(panel = %panel.id, error = %err, "panel pump failed");
                }
            }
            if panel.state() != PanelState::Exited && !panel.is_child_alive() {
                exited.push(panel.id);
            }
        }
        for id in exited {
            if let Some(panel) = self.panels.iter_mut().find(|p| p.id == id) {
                // Drain any final output, then mark exited.
                let _ = panel.pump();
                let _ = lifecycle::transition(panel, PanelState::Exited);
            }
            // Reflect the exit on the manifest so `list_panels` shows `exited`.
            if let Some(manifest) = self.manifests.get_mut(&id) {
                if manifest.status() != crate::agent::AgentStatus::Exited {
                    if let Err(err) = manifest::record_exited(manifest, &self.session.root_dir) {
                        warn!(panel = %id, error = %err, "manifest exit write failed");
                    }
                }
            }
        }
    }

    /// Apply a key event via the focus router. Returns `true` while caucus
    /// should keep running, `false` once quit was requested.
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        use crate::input::InputAction;
        match self.focus.route(key) {
            InputAction::ToPanel { panel, bytes } => {
                if let Some(p) = self.panels.iter_mut().find(|p| p.id == panel) {
                    if let Err(err) = p.write_input(&bytes) {
                        warn!(panel = %panel, error = %err, "panel write failed");
                    }
                }
                // A submitted line (Enter) typed directly into a panel is a
                // prompt delivered by the user — flip it to `Working`, the
                // same as the MCP `send_keys` path.
                if bytes.contains(&b'\r') || bytes.contains(&b'\n') {
                    self.note_prompt_delivered(panel);
                }
            }
            InputAction::Caucus(cmd) => self.apply_command(cmd),
            InputAction::Ignore => {}
        }
    }

    /// Whether the reserved prefix key is armed (for a status hint).
    pub fn prefix_armed(&self) -> bool {
        self.focus.prefix_armed()
    }

    /// Apply a caucus-level command (focus switch / quit).
    fn apply_command(&mut self, cmd: CaucusCommand) {
        match cmd {
            CaucusCommand::Quit => self.quit = true,
            CaucusCommand::FocusNext => self.cycle_focus(1),
            CaucusCommand::FocusPrev => self.cycle_focus(-1),
        }
    }

    /// Move focus by `delta` panels, wrapping around.
    fn cycle_focus(&mut self, delta: isize) {
        if self.panels.is_empty() {
            return;
        }
        let cur = self
            .focus
            .focused()
            .and_then(|id| self.panels.iter().position(|p| p.id == id))
            .unwrap_or(0);
        let n = self.panels.len() as isize;
        let next = ((cur as isize + delta) % n + n) % n;
        self.focus.set_focus(Some(self.panels[next as usize].id));
    }

    /// Ingest a turn-completion signal: close the panel's capture turn, flip
    /// it to `Idle`, and record a `TurnCompleted` lane event on the panel's
    /// manifest so `list_panels` shows `idle` (`docs/design.md` §4, §8.3).
    ///
    /// The manifest mutation routes through `agent::manifest::record_turn_completed`
    /// — the single owner of that transition (Invariant I-2) — which also
    /// recomputes `derived_state` and stores the signal's `last_message`.
    pub fn handle_signal(&mut self, signal: TurnSignal) {
        let Some(panel) = self.panels.iter_mut().find(|p| p.id == signal.panel_id) else {
            return;
        };
        panel.end_turn();
        // A turn signal means the agent is idle, waiting for the next prompt.
        if panel.state() == PanelState::Working {
            let _ = lifecycle::transition(panel, PanelState::Idle);
        }

        // Append the TurnCompleted lane event + recompute derived_state.
        if let Some(manifest) = self.manifests.get_mut(&signal.panel_id) {
            if let Err(err) =
                manifest::record_turn_completed(manifest, &self.session.root_dir, &signal)
            {
                warn!(panel = %signal.panel_id, error = %err, "manifest turn-signal write failed");
            }
        }
    }

    /// Mark a panel as having received a prompt: open a capture turn and flip
    /// it to `Working` (`docs/design.md` §4). Used by the MCP `send_keys` path.
    pub fn note_prompt_delivered(&mut self, panel_id: PanelId) {
        if let Some(panel) = self.panels.iter_mut().find(|p| p.id == panel_id) {
            panel.begin_turn();
            match panel.state() {
                PanelState::Spawning | PanelState::Idle => {
                    let _ = lifecycle::transition(panel, PanelState::Working);
                }
                _ => {}
            }
        }
    }

    /// Execute one queued [`ControlRequest`] against the live panels and
    /// produce its [`ControlResponse`] (`docs/design.md` §0 #4).
    ///
    /// Called by the event loop for every [`ControlJob`] drained from the
    /// control socket — see [`Multiplexer::drain_control`]. Each variant maps
    /// onto one [`McpToolSurface`] method; failures become
    /// [`ControlResponse::Error`] so the CEO sees the message in-band.
    pub fn execute_control(&mut self, request: ControlRequest) -> ControlResponse {
        match request {
            ControlRequest::SendKeys { panel, text, enter } => {
                match self.send_keys(panel, &text, enter) {
                    Ok(()) => ControlResponse::Ok,
                    Err(err) => ControlResponse::error(err),
                }
            }
            ControlRequest::CtrlC { panel } => match self.ctrl_c(panel) {
                Ok(()) => ControlResponse::Ok,
                Err(err) => ControlResponse::error(err),
            },
            ControlRequest::ReadPanel { panel, mode } => match self.read_panel(panel, mode) {
                Ok(text) => ControlResponse::Panel { text },
                Err(err) => ControlResponse::error(err),
            },
            ControlRequest::SpawnRole {
                role,
                worktree,
                model,
                agent_cli,
            } => match self.spawn_role(&role, worktree, model.as_deref(), agent_cli) {
                Ok(panel) => ControlResponse::Spawned { panel },
                Err(err) => ControlResponse::error(err),
            },
            ControlRequest::KillPanel { panel } => {
                // The trait method (McpError) — the inherent `kill_panel`
                // (anyhow) is shadowed, so call it through the trait.
                match McpToolSurface::kill_panel(self, panel) {
                    Ok(()) => ControlResponse::Ok,
                    Err(err) => ControlResponse::error(err),
                }
            }
            ControlRequest::ListPanels => ControlResponse::Panels {
                panels: self.list_panels(),
            },
        }
    }

    /// Drain every queued control job from `server`, execute it, and answer
    /// each through its oneshot reply. Called once per event-loop tick — the
    /// single point at which CEO MCP tool calls touch live panels, on the same
    /// thread that pumps PTYs (Invariant I-5).
    pub fn drain_control(&mut self, server: &mut ControlServer) {
        while let Ok(job) = server.jobs().try_recv() {
            let ControlJob { request, reply } = job;
            let response = self.execute_control(request);
            // A dropped reply channel means the control-socket connection
            // closed before we answered — nothing to do.
            let _ = reply.send(response);
        }
    }

    /// Render a panel's visible grid viewport as text, one row per line.
    fn screen_text(panel: &Panel) -> String {
        let (_, rows) = panel.grid().size();
        let mut out = String::new();
        for row in 0..rows {
            out.push_str(panel.grid().row_text(row).trim_end());
            out.push('\n');
        }
        // Drop trailing blank lines so the CEO is not handed a wall of spaces.
        while out.ends_with("\n\n") {
            out.pop();
        }
        out
    }

    /// Render a panel's scrollback ring as text, oldest row first.
    fn scrollback_text(panel: &Panel) -> String {
        let mut out = String::new();
        for row in panel.grid().scrollback() {
            let line: String = row.iter().filter(|c| c.ch != '\0').map(|c| c.ch).collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    /// Kill every panel and close the session — used on shutdown so no agent
    /// process is orphaned.
    ///
    /// The `Active -> Closed` transition goes through `session::state::transition`
    /// (Invariant I-1, the single owner of session state).
    pub fn shutdown(&mut self) {
        let ids: Vec<PanelId> = self.panels.iter().map(|p| p.id).collect();
        for id in ids {
            if let Err(err) = self.kill_panel(id) {
                warn!(panel = %id, error = %err, "panel kill on shutdown failed");
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

/// The CEO's MCP tool surface, backed by the live panel registry
/// (`docs/design.md` §0 #4). Every method runs on the multiplexer's own
/// thread — control jobs are executed by [`Multiplexer::drain_control`] inside
/// the event loop, never concurrently with `pump_all` (Invariant I-5).
impl McpToolSurface for Multiplexer {
    fn send_keys(&mut self, panel: PanelId, text: &str, enter: bool) -> Result<(), McpError> {
        let p = self
            .panels
            .iter_mut()
            .find(|p| p.id == panel)
            .ok_or(McpError::NoSuchPanel(panel))?;
        let mut bytes = text.as_bytes().to_vec();
        if enter {
            bytes.push(b'\r');
        }
        p.write_input(&bytes)
            .map_err(|e| McpError::Tool(format!("send_keys: {e}")))?;

        // Delivering a prompt opens a capture turn and flips the panel to
        // `Working` (`docs/design.md` §4) — only when the line is submitted,
        // since a partial line is not yet a turn.
        if enter {
            self.note_prompt_delivered(panel);
        }
        Ok(())
    }

    fn ctrl_c(&mut self, panel: PanelId) -> Result<(), McpError> {
        let p = self
            .panels
            .iter_mut()
            .find(|p| p.id == panel)
            .ok_or(McpError::NoSuchPanel(panel))?;
        // 0x03 = ETX = Ctrl-C.
        p.write_input(&[0x03])
            .map_err(|e| McpError::Tool(format!("ctrl_c: {e}")))
    }

    fn read_panel(&self, panel: PanelId, mode: ReadPanelMode) -> Result<String, McpError> {
        let p = self
            .panels
            .iter()
            .find(|p| p.id == panel)
            .ok_or(McpError::NoSuchPanel(panel))?;
        Ok(match mode {
            ReadPanelMode::Screen => Self::screen_text(p),
            ReadPanelMode::Scrollback => Self::scrollback_text(p),
            ReadPanelMode::SinceLastTurn => {
                // Whole-turn capture (`docs/design.md` §8.5) — the CEO never
                // races the screen because this is the captured turn output,
                // not the live grid.
                String::from_utf8_lossy(p.capture().since_last_turn()).into_owned()
            }
            ReadPanelMode::LastMessage => self
                .manifests
                .get(&panel)
                .and_then(|m| m.last_message().map(str::to_string))
                .unwrap_or_default(),
        })
    }

    fn spawn_role(
        &mut self,
        role: &str,
        worktree: bool,
        model: Option<&str>,
        agent_cli: Option<AgentCli>,
    ) -> Result<PanelId, McpError> {
        let worktree_path = if worktree {
            Some(self.create_role_worktree(role)?)
        } else {
            None
        };
        self.spawn_panel(role, agent_cli, model.map(str::to_string), worktree_path)
            .map_err(|e| McpError::Tool(format!("spawn_role: {e:#}")))
    }

    fn kill_panel(&mut self, panel: PanelId) -> Result<(), McpError> {
        if !self.panels.iter().any(|p| p.id == panel) {
            return Err(McpError::NoSuchPanel(panel));
        }
        // Delegate to the inherent single-owner destruction path.
        Multiplexer::kill_panel(self, panel)
            .map_err(|e| McpError::Tool(format!("kill_panel: {e:#}")))
    }

    fn list_panels(&self) -> Vec<PanelSummary> {
        self.panels
            .iter()
            .map(|p| {
                // Prefer the manifest's derived_state (turn-signal fed); fall
                // back to the coarse panel-state label before the first turn.
                let (state, agent_cli) = match self.manifests.get(&p.id) {
                    Some(m) => (
                        format!("{:?}", m.derived_state()).to_ascii_lowercase(),
                        m.agent_cli,
                    ),
                    None => (p.state_label().to_string(), AgentCli::Claude),
                };
                PanelSummary {
                    panel_id: p.id,
                    role: p.role.clone(),
                    state,
                    agent_cli,
                }
            })
            .collect()
    }
}

impl Multiplexer {
    /// Create an execute-phase worktree for a `spawn_role(worktree=true)` call
    /// (`docs/design.md` §5). Single owner of worktree creation is
    /// `worktree::manager::create` (Invariant I-3).
    ///
    /// `spawn_role` runs synchronously inside the event loop; the async git
    /// driver is bridged with `block_in_place` + `Handle::block_on` so the
    /// multiplexer thread is not blocked off-runtime.
    fn create_role_worktree(&self, role: &str) -> Result<PathBuf, McpError> {
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
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            McpError::Tool("spawn_role(worktree): no tokio runtime".to_string())
        })?;
        let handle_result = tokio::task::block_in_place(|| handle.block_on(create_worktree(&req)));
        match handle_result {
            Ok(wt) => Ok(wt.path),
            Err(err) => Err(McpError::Tool(format!("worktree create: {err}"))),
        }
    }
}

/// Choose a turn-signal socket path for `session`.
///
/// `docs/design.md` §7.1 names `<repo>/.caucus/sessions/<id>/caucus.sock`, but
/// a unix-domain socket path has a hard OS length cap (`SUN_LEN`, ~104 bytes
/// on macOS). A deep repo path easily blows that, so the socket is bound in
/// the system temp dir under a short, session-keyed name instead. The path is
/// injected into every agent as `CAUCUS_SOCK`, so its exact location is an
/// internal detail — only the env var is contract.
fn socket_path(session: &Session) -> PathBuf {
    std::env::temp_dir().join(format!("caucus-{}.sock", session_suffix(session)))
}

/// Choose the MCP control socket path for `session` — distinct from the
/// turn-signal socket, same SUN_LEN-safe temp-dir scheme.
fn control_socket_path(session: &Session) -> PathBuf {
    std::env::temp_dir().join(format!("caucus-{}-ctl.sock", session_suffix(session)))
}

/// Short, collision-safe session suffix (last 10 ULID chars) — keeps socket
/// paths well under the OS `SUN_LEN` cap.
fn session_suffix(session: &Session) -> String {
    let id = session.id.to_string();
    id.chars()
        .rev()
        .take(10)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        }
    }

    /// Build a multiplexer rooted in a temp dir. The tokio runtime is needed
    /// because `SignalServer::bind` and `CleanupQueue::spawn` call
    /// `tokio::spawn`.
    fn mux(tmp: &TempDir) -> Multiplexer {
        let session = Session::new("test", tmp.path().to_path_buf());
        let config = Config::load(tmp.path()).unwrap();
        let (mux, _signal, _control) = Multiplexer::new(session, config, area()).unwrap();
        mux
    }

    #[tokio::test]
    async fn new_creates_session_dirs_and_socket() {
        let tmp = TempDir::new().unwrap();
        let mux = mux(&tmp);
        assert!(mux.session.root_dir.join("agents").is_dir());
        assert!(mux.session.root_dir.join("panels").is_dir());
        assert!(mux.sock_path().exists());
    }

    #[tokio::test]
    async fn new_binds_a_control_socket() {
        let tmp = TempDir::new().unwrap();
        let session = Session::new("test", tmp.path().to_path_buf());
        let config = Config::load(tmp.path()).unwrap();
        let (mux, _signal, control) = Multiplexer::new(session, config, area()).unwrap();
        // The control socket is distinct from the turn-signal socket and
        // exists on disk.
        assert!(control.sock_path().exists());
        assert_ne!(control.sock_path(), mux.sock_path());
        assert_eq!(mux.control_sock_path(), control.sock_path());
    }

    #[tokio::test]
    async fn control_request_for_unknown_panel_is_an_error() {
        use crate::mcp::protocol::{ControlRequest, ControlResponse};
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let ghost = PanelId::new();

        for req in [
            ControlRequest::CtrlC { panel: ghost },
            ControlRequest::SendKeys {
                panel: ghost,
                text: "hi".into(),
                enter: true,
            },
            ControlRequest::ReadPanel {
                panel: ghost,
                mode: crate::mcp::ReadPanelMode::Screen,
            },
            ControlRequest::KillPanel { panel: ghost },
        ] {
            let resp = mux.execute_control(req);
            assert!(
                matches!(resp, ControlResponse::Error { .. }),
                "expected an error response for an unknown panel"
            );
        }
    }

    #[tokio::test]
    async fn list_panels_control_request_is_empty_for_a_fresh_mux() {
        use crate::mcp::protocol::{ControlRequest, ControlResponse};
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        match mux.execute_control(ControlRequest::ListPanels) {
            ControlResponse::Panels { panels } => assert!(panels.is_empty()),
            other => panic!("expected Panels, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cycle_focus_wraps_with_no_panels() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        // No panels: cycling is a no-op, not a panic.
        mux.cycle_focus(1);
        assert!(mux.focused().is_none());
    }

    #[tokio::test]
    async fn quit_command_sets_should_quit() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        assert!(!mux.should_quit());
        mux.apply_command(CaucusCommand::Quit);
        assert!(mux.should_quit());
    }
}
