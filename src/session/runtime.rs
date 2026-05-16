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
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::oneshot;
use tracing::warn;

use crate::agent::manifest::{self, AgentManifest};
use crate::agent::spawn::SpawnRequest;
use crate::config::Config;
use crate::input::{CaucusCommand, FocusRouter};
use crate::mcp::control_server::{ControlJob, ControlServer};
use crate::mcp::protocol::{ControlRequest, ControlResponse};
use crate::mcp::{McpError, McpToolSurface, PanelSummary, ReadPanelMode};
use crate::panel::lifecycle::{self, Panel, PanelState};
use crate::render::{Layout, LayoutMode, Rect};
use crate::role::spec::AgentCli;
use crate::session::id::PanelId;
use crate::session::state::Session;
use crate::signal::TurnSignal;
use crate::signal::server::SignalServer;
use crate::worktree::cleanup::{CleanupJob, CleanupQueue};
use crate::worktree::manager::{WorktreeHandle, WorktreeRequest, create as create_worktree};

/// Default wait budget for `wait_for_panels` when the caller omits
/// `timeout_secs`.
const WAIT_DEFAULT_SECS: u64 = 600;
/// Hard cap on a `wait_for_panels` budget — a runaway wait can never park the
/// main worker's blocking MCP call longer than this.
const WAIT_MAX_SECS: u64 = 3600;

/// A `wait_for_panels` request the event loop has not answered yet.
///
/// The control protocol is otherwise "one request → one immediate response";
/// `WaitForPanels` is the sole exception. [`Multiplexer::drain_control`]
/// stashes one of these instead of calling `execute_control`, and
/// [`Multiplexer::poll_pending_waits`] fires + drops it once every waited
/// panel has settled or `deadline` passes — so the event loop is never
/// blocked.
struct PendingWait {
    /// Panel ids the caller is waiting on. Ids that no longer exist count as
    /// settled (see [`Multiplexer::wait_panels_settled`]).
    panels: Vec<PanelId>,
    /// Wall-clock instant past which the wait is answered regardless of state.
    deadline: Instant,
    /// Oneshot the deferred [`ControlResponse`] is delivered on.
    reply: oneshot::Sender<ControlResponse>,
}

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
    /// Control socket path — the main worker panel's MCP server connects here
    /// (`docs/design.md` §0 #4). Wired into the main worker panel's `.mcp.json`.
    control_sock_path: PathBuf,
    /// Whole-screen area the layout tiles.
    area: Rect,
    /// Set when the user requested quit (`Ctrl-A q`).
    quit: bool,
    /// Monotonic counter for agent-name suffixes per role.
    role_counts: HashMap<String, usize>,
    /// `wait_for_panels` requests awaiting a deferred reply
    /// ([`Multiplexer::poll_pending_waits`]).
    pending_waits: Vec<PendingWait>,
    /// Panel arrangement mode for [`Layout::reflow`] — cycled by `Ctrl-A Space`.
    layout_mode: LayoutMode,
    /// When `Some` and the id is still live, the layout shows only that panel
    /// full-screen (`Ctrl-A z`). Hidden panels keep running — `pump_all`
    /// always pumps every panel; only the layout is restricted.
    zoom: Option<PanelId>,
    /// When set, the read-only transcript overlay is drawn on top of the
    /// panels (`Ctrl-A t`). Draw-time only — panels keep pumping and input
    /// keeps routing as normal.
    show_transcript: bool,
    /// Git branch of each worktree-backed panel, keyed by panel id. The
    /// manifest stores only the worktree *path* (which is removed on
    /// shutdown); the branch persists and is what `caucus resume` re-attaches
    /// a worktree on. Populated at spawn, dropped on kill.
    worktree_branches: HashMap<PanelId, String>,
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
    /// (main worker MCP tool calls).
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
                pending_waits: Vec::new(),
                layout_mode: LayoutMode::default(),
                zoom: None,
                show_transcript: false,
                worktree_branches: HashMap::new(),
            },
            signal_server,
            control_server,
        ))
    }

    /// The MCP control socket path — wired into the main worker panel's `.mcp.json`.
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
    ///
    /// When [`Multiplexer::zoom`] names a still-live panel the layout is a
    /// single full-area slot for that panel; otherwise the panels tile per
    /// the current [`LayoutMode`]. Hidden (un-tiled) panels keep their last
    /// PTY size — they are resized again the moment they reappear in a slot.
    fn reflow(&mut self) {
        let ids: Vec<PanelId> = self.panels.iter().map(|p| p.id).collect();
        let zoomed = self
            .zoom
            .filter(|id| self.panels.iter().any(|p| p.id == *id));
        self.layout = match zoomed {
            Some(id) => Layout {
                slots: vec![(id, self.area)],
            },
            None => Layout::reflow(&ids, self.area, self.layout_mode),
        };
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
        self.spawn_panel_inner(SpawnPanelOpts {
            role,
            agent_cli: None,
            model: None,
            worktree_path: None,
            worktree_branch: None,
            mcp_config_path: Some(mcp_config),
            resume_session_id,
        })
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

    /// Build a [`SessionRecord`] from the live panels + manifests.
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
            resume_session_id,
        };

        // Provisional layout: compute the slot the new panel will occupy so
        // its PTY is sized correctly from the first byte.
        let mut ids: Vec<PanelId> = self.panels.iter().map(|p| p.id).collect();
        let outcome = crate::agent::spawn::spawn(&request)
            .map_err(|e| anyhow::anyhow!("agent spawn: {e}"))?;
        ids.push(outcome.panel_id);
        let provisional = Layout::reflow(&ids, self.area, self.layout_mode);
        let rect = provisional.rect_of(outcome.panel_id).unwrap_or(self.area);

        let mut panel =
            lifecycle::spawn(&request, outcome.panel_id, outcome.manifest.agent_id, rect)?;
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
        self.worktree_branches.remove(&panel_id);

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
        self.reflow();
        self.persist_record();
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

    /// Apply a caucus-level command (focus switch / quit / layout control).
    fn apply_command(&mut self, cmd: CaucusCommand) {
        match cmd {
            CaucusCommand::Quit => self.quit = true,
            CaucusCommand::FocusNext => self.cycle_focus(1),
            CaucusCommand::FocusPrev => self.cycle_focus(-1),
            CaucusCommand::ToggleZoom => self.toggle_zoom(),
            CaucusCommand::MovePanelEarlier => self.move_panel(-1),
            CaucusCommand::MovePanelLater => self.move_panel(1),
            CaucusCommand::CycleLayout => {
                self.layout_mode = self.layout_mode.next();
                self.reflow();
                // The record carries `layout_mode` and the panel order.
                self.persist_record();
            }
            CaucusCommand::ToggleTranscript => {
                self.show_transcript = !self.show_transcript;
                self.focus.set_transcript_open(self.show_transcript);
            }
            CaucusCommand::HideTranscript => {
                self.show_transcript = false;
                self.focus.set_transcript_open(false);
            }
        }
    }

    /// Whether the read-only transcript overlay is currently shown.
    pub fn show_transcript(&self) -> bool {
        self.show_transcript
    }

    /// Per-panel manifests, keyed by panel id — read-only, for the overlay.
    pub fn manifests(&self) -> &HashMap<PanelId, AgentManifest> {
        &self.manifests
    }

    /// Toggle full-screen zoom on the focused panel. A second toggle (or a
    /// toggle while a different panel is zoomed) restores the tiled layout
    /// or moves the zoom; with no focused panel it is a no-op.
    fn toggle_zoom(&mut self) {
        let Some(focused) = self.focus.focused() else {
            return;
        };
        self.zoom = if self.zoom == Some(focused) {
            None
        } else {
            Some(focused)
        };
        self.reflow();
    }

    /// Move the focused panel one step (`delta` = -1 earlier, +1 later) in the
    /// panel order — which is also the tile order and the focus-cycle order.
    /// A no-op when there is no focused panel or it is already at the end.
    fn move_panel(&mut self, delta: isize) {
        let Some(focused) = self.focus.focused() else {
            return;
        };
        let Some(idx) = self.panels.iter().position(|p| p.id == focused) else {
            return;
        };
        let target = idx as isize + delta;
        if target < 0 || target as usize >= self.panels.len() {
            return;
        }
        self.panels.swap(idx, target as usize);
        self.reflow();
        // `order_index` in the record changed.
        self.persist_record();
    }

    /// The current panel arrangement mode (for the status bar).
    pub fn layout_mode(&self) -> LayoutMode {
        self.layout_mode
    }

    /// Set the panel arrangement mode and reflow — used by `caucus resume` to
    /// restore the persisted layout. Does not persist the record itself; the
    /// resume path persists once after the whole roster is rebuilt.
    pub fn set_layout_mode(&mut self, mode: LayoutMode) {
        self.layout_mode = mode;
        self.reflow();
    }

    /// The zoomed panel id, if a panel is currently zoomed.
    pub fn zoomed(&self) -> Option<PanelId> {
        self.zoom
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
        // A turn signal can carry Claude's conversation id for the first time;
        // re-persist the session record so a relaunch can `--resume` it.
        let mut session_id_changed = false;
        if let Some(manifest) = self.manifests.get_mut(&signal.panel_id) {
            let before = manifest.claude_session_id().map(str::to_string);
            if let Err(err) =
                manifest::record_turn_completed(manifest, &self.session.root_dir, &signal)
            {
                warn!(panel = %signal.panel_id, error = %err, "manifest turn-signal write failed");
            }
            session_id_changed = manifest.claude_session_id().map(str::to_string) != before;
        }
        if session_id_changed {
            self.persist_record();
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
    /// [`ControlResponse::Error`] so the main worker sees the message in-band.
    pub fn execute_control(&mut self, request: ControlRequest) -> ControlResponse {
        match request {
            ControlRequest::SendKeys { panel, text, enter } => {
                match self.send_keys(panel, &text, enter) {
                    Ok(()) => ControlResponse::Ok,
                    Err(err) => ControlResponse::error(err),
                }
            }
            ControlRequest::Broadcast {
                panels,
                text,
                enter,
            } => self.broadcast(&panels, &text, enter),
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
            // `WaitForPanels` is a deferred-reply request — `drain_control`
            // routes it to `register_wait` and never reaches here. Handled
            // for match exhaustiveness: answer with the current panel snapshot
            // (a degenerate zero-timeout wait) rather than panicking.
            ControlRequest::WaitForPanels { panels, .. } => self.wait_response(&panels),
        }
    }

    /// Drain every queued control job from `server`, execute it, and answer
    /// each through its oneshot reply. Called once per event-loop tick — the
    /// single point at which main worker MCP tool calls touch live panels, on
    /// the same thread that pumps PTYs (Invariant I-5).
    ///
    /// Every request bar [`ControlRequest::WaitForPanels`] is answered
    /// immediately via [`Multiplexer::execute_control`]. `WaitForPanels` is a
    /// blocking tool: if its panels are already all settled the reply is sent
    /// now, otherwise the job is stashed as a [`PendingWait`] and answered
    /// later by [`Multiplexer::poll_pending_waits`] — the event loop is never
    /// blocked.
    pub fn drain_control(&mut self, server: &mut ControlServer) {
        while let Ok(job) = server.jobs().try_recv() {
            let ControlJob { request, reply } = job;
            match request {
                ControlRequest::WaitForPanels {
                    panels,
                    timeout_secs,
                } => self.register_wait(panels, timeout_secs, reply),
                other => {
                    let response = self.execute_control(other);
                    // A dropped reply channel means the control-socket
                    // connection closed before we answered — nothing to do.
                    let _ = reply.send(response);
                }
            }
        }
    }

    /// Whether a panel id counts as "settled" for `wait_for_panels`: its
    /// `PanelState` is `Idle`/`Blocked`/`Exited` — i.e. NOT `Working` and NOT
    /// `Spawning`. A panel id that does not exist counts as settled (there is
    /// nothing left to wait for — it was killed or never spawned).
    fn wait_panels_settled(&self, panels: &[PanelId]) -> bool {
        panels
            .iter()
            .all(|id| match self.panels.iter().find(|p| p.id == *id) {
                Some(p) => !matches!(p.state(), PanelState::Working | PanelState::Spawning),
                None => true,
            })
    }

    /// Build the deferred reply for a satisfied/timed-out `wait_for_panels`:
    /// each waited panel's current [`PanelSummary`]. A waited id that no longer
    /// exists is omitted (it is gone — `list_panels` would not show it
    /// either); the main worker should treat a missing id as fully done.
    fn wait_response(&self, panels: &[PanelId]) -> ControlResponse {
        ControlResponse::Panels {
            panels: self.wait_response_summaries(panels),
        }
    }

    /// Type the same `text` into every panel in `panels` — a round's fan-out
    /// (`docs/design.md` §4). Each panel is driven exactly as the MCP
    /// `send_keys` tool would drive it: the text is written, a `\r` appended
    /// when `enter`, and on `enter` [`Multiplexer::note_prompt_delivered`]
    /// opens a capture turn and flips the panel to `Working`.
    ///
    /// A panel id that does not exist (or whose write fails) is reported, not
    /// fatal — the remaining panels still receive the text. The reply is
    /// always [`ControlResponse::Panels`]: the post-broadcast [`PanelSummary`]
    /// of each targeted id that exists, the same shape `list_panels` /
    /// `wait_for_panels` return. A bad id is visible by its absence from that
    /// list, so the main worker can tell which panels a typo missed while the
    /// good ones still ran.
    fn broadcast(&mut self, panels: &[PanelId], text: &str, enter: bool) -> ControlResponse {
        for &panel in panels {
            // Per-panel failures (no such panel, write error) are non-fatal:
            // the other panels in the round still get the text.
            let _ = self.send_keys(panel, text, enter);
        }
        ControlResponse::Panels {
            panels: self.wait_response_summaries(panels),
        }
    }

    /// The [`PanelSummary`] for each id in `panels` that still exists, in the
    /// caller's order — missing ids are omitted (they were killed or the id
    /// was bad).
    fn wait_response_summaries(&self, panels: &[PanelId]) -> Vec<PanelSummary> {
        let all = self.list_panels();
        panels
            .iter()
            .filter_map(|id| all.iter().find(|s| s.panel_id == *id).cloned())
            .collect()
    }

    /// Register a `wait_for_panels` request. If the panels are already all
    /// settled the reply is sent immediately; otherwise a [`PendingWait`] is
    /// stashed for [`Multiplexer::poll_pending_waits`]. The `timeout_secs`
    /// budget is clamped to `[1, WAIT_MAX_SECS]`, defaulting to
    /// `WAIT_DEFAULT_SECS`.
    fn register_wait(
        &mut self,
        panels: Vec<PanelId>,
        timeout_secs: Option<u64>,
        reply: oneshot::Sender<ControlResponse>,
    ) {
        if self.wait_panels_settled(&panels) {
            let _ = reply.send(self.wait_response(&panels));
            return;
        }
        let budget = timeout_secs
            .unwrap_or(WAIT_DEFAULT_SECS)
            .clamp(1, WAIT_MAX_SECS);
        self.pending_waits.push(PendingWait {
            panels,
            deadline: Instant::now() + Duration::from_secs(budget),
            reply,
        });
    }

    /// Fire and remove every [`PendingWait`] whose panels have all settled or
    /// whose `deadline` has passed. Called once per event-loop tick. A
    /// `reply.send` failure (the `mcp-serve` connection closed) just drops the
    /// wait — there is no one left to answer.
    pub fn poll_pending_waits(&mut self) {
        if self.pending_waits.is_empty() {
            return;
        }
        let now = Instant::now();
        // Take the vec so the satisfied-check can borrow `self` immutably.
        let waits = std::mem::take(&mut self.pending_waits);
        for wait in waits {
            if now >= wait.deadline || self.wait_panels_settled(&wait.panels) {
                let _ = wait.reply.send(self.wait_response(&wait.panels));
            } else {
                self.pending_waits.push(wait);
            }
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
        // Drop trailing blank lines so the main worker is not handed a wall of spaces.
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
        // Include the live viewport so `scrollback` is the complete retained
        // buffer (history + current screen), not just the off-screen rows.
        out.push_str(&Self::screen_text(panel));
        out
    }

    /// Render a raw PTY byte capture — a whole turn, escape sequences and all —
    /// into readable text by replaying it through a throwaway grid. Without
    /// this, `read_panel(since_last_turn)` would hand the main worker an
    /// escape-sequence soup instead of the turn's rendered output.
    fn rendered_capture_text(bytes: &[u8], cols: usize) -> String {
        let mut grid = crate::term::Grid::new(cols.max(20), 50);
        grid.advance(bytes);
        let mut out = String::new();
        for row in grid.scrollback() {
            let line: String = row.iter().filter(|c| c.ch != '\0').map(|c| c.ch).collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        let (_, rows) = grid.size();
        for r in 0..rows {
            out.push_str(grid.row_text(r).trim_end());
            out.push('\n');
        }
        while out.ends_with("\n\n") {
            out.pop();
        }
        out
    }

    /// Kill every panel and close the session — used on shutdown so no agent
    /// process is orphaned.
    ///
    /// The `Active -> Closed` transition goes through `session::state::transition`
    /// (Invariant I-1, the single owner of session state).
    pub fn shutdown(&mut self) {
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

/// The main worker's MCP tool surface, backed by the live panel registry
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
                // Whole-turn capture (`docs/design.md` §8.5), rendered to
                // readable text — the main worker never races the screen and
                // is never handed raw escape sequences.
                let (cols, _) = p.grid().size();
                Self::rendered_capture_text(p.capture().since_last_turn(), cols)
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
        let wt_handle = if worktree {
            Some(self.create_role_worktree(role)?)
        } else {
            None
        };
        let worktree_path = wt_handle.as_ref().map(|h| h.path.clone());
        let worktree_branch = wt_handle.as_ref().map(|h| h.branch.clone());
        // `spawn_panel_resume` with no resume id is a plain spawn that also
        // records the worktree branch (so `caucus resume` can re-attach it).
        let spawned = self.spawn_panel_resume(
            role,
            agent_cli,
            model.map(str::to_string),
            worktree_path,
            worktree_branch,
            None,
        );
        match spawned {
            Ok(id) => {
                self.persist_record();
                Ok(id)
            }
            Err(e) => {
                // The panel never came up — don't leak the worktree (dir +
                // branch) `create_role_worktree` just created. Enqueue it for
                // serial cleanup (Invariant I-3); the branch is empty (the
                // sub-agent never ran) so it is deleted, not preserved.
                if let Some(h) = wt_handle {
                    let _ = self.cleanup.enqueue(CleanupJob {
                        repo_root: self.session.repo_path.clone(),
                        worktree_paths: vec![h.path],
                        branches_to_delete: vec![h.branch],
                        done: None,
                    });
                }
                Err(McpError::Tool(format!("spawn_role: {e:#}")))
            }
        }
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
    /// `worktree::manager::create` is synchronous (`git worktree add` is a
    /// fast subprocess); the event loop calls it directly on its own thread —
    /// no async bridging, so no nested-runtime panic.
    fn create_role_worktree(&self, role: &str) -> Result<WorktreeHandle, McpError> {
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

    /// `CycleLayout` advances the arrangement mode through the full cycle and
    /// wraps back to `Tiled`.
    #[tokio::test]
    async fn cycle_layout_advances_the_mode() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        assert_eq!(mux.layout_mode(), LayoutMode::Tiled);
        mux.apply_command(CaucusCommand::CycleLayout);
        assert_eq!(mux.layout_mode(), LayoutMode::EvenHorizontal);
        mux.apply_command(CaucusCommand::CycleLayout);
        assert_eq!(mux.layout_mode(), LayoutMode::EvenVertical);
        mux.apply_command(CaucusCommand::CycleLayout);
        assert_eq!(mux.layout_mode(), LayoutMode::MainVertical);
        mux.apply_command(CaucusCommand::CycleLayout);
        assert_eq!(mux.layout_mode(), LayoutMode::Tiled);
    }

    /// `ToggleTranscript` flips `show_transcript`; `HideTranscript` always
    /// clears it.
    #[tokio::test]
    async fn toggle_transcript_flips_show_transcript() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        assert!(!mux.show_transcript());

        mux.apply_command(CaucusCommand::ToggleTranscript);
        assert!(mux.show_transcript());

        mux.apply_command(CaucusCommand::ToggleTranscript);
        assert!(!mux.show_transcript());

        // Open it, then hide it explicitly.
        mux.apply_command(CaucusCommand::ToggleTranscript);
        assert!(mux.show_transcript());
        mux.apply_command(CaucusCommand::HideTranscript);
        assert!(!mux.show_transcript());
    }

    /// `ToggleZoom` with no focused panel is a no-op (no panic).
    #[tokio::test]
    async fn toggle_zoom_with_no_panels_is_a_noop() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.apply_command(CaucusCommand::ToggleZoom);
        assert!(mux.zoomed().is_none());
    }

    /// The zoom layout is a single full-area slot for the zoomed panel; a
    /// second toggle restores the tiled layout.
    #[tokio::test]
    async fn zoom_yields_one_full_area_slot() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        // Three synthetic panel ids in `panels` would need real panels; the
        // zoom layout is computed in `reflow` from `self.zoom` + `self.area`,
        // so drive it directly with a known id.
        let id = PanelId::new();
        mux.zoom = Some(id);
        // `id` is not a live panel — zoom is filtered to live ids only, so
        // the layout falls back to the (empty) tiled reflow.
        mux.reflow();
        assert!(mux.layout().slots.is_empty());

        // With a live zoomed panel the layout is exactly one full-area slot.
        let Ok(panel) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.focus.set_focus(Some(panel));
        mux.apply_command(CaucusCommand::ToggleZoom);
        assert_eq!(mux.zoomed(), Some(panel));
        assert_eq!(mux.layout().slots.len(), 1);
        assert_eq!(mux.layout().slots[0], (panel, area()));

        // Toggling again restores the tiled layout.
        mux.apply_command(CaucusCommand::ToggleZoom);
        assert!(mux.zoomed().is_none());

        mux.shutdown();
    }

    /// `MovePanelEarlier`/`MovePanelLater` swap adjacent entries in the panel
    /// order; moving past either end is a no-op.
    #[tokio::test]
    async fn move_panel_swaps_order() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Ok(a) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        let Ok(b) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        let order = |m: &Multiplexer| m.panels().iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(order(&mux), vec![a, b]);

        // Focus `a` (index 0) and move it later — order becomes [b, a].
        mux.focus.set_focus(Some(a));
        mux.apply_command(CaucusCommand::MovePanelLater);
        assert_eq!(order(&mux), vec![b, a]);

        // `a` is now last — moving later again is a no-op.
        mux.apply_command(CaucusCommand::MovePanelLater);
        assert_eq!(order(&mux), vec![b, a]);

        // Move `a` back earlier — order returns to [a, b].
        mux.apply_command(CaucusCommand::MovePanelEarlier);
        assert_eq!(order(&mux), vec![a, b]);

        // `a` is first — moving earlier again is a no-op.
        mux.apply_command(CaucusCommand::MovePanelEarlier);
        assert_eq!(order(&mux), vec![a, b]);

        mux.shutdown();
    }

    /// Killing the zoomed panel clears the zoom so the layout never points at
    /// a dead id.
    #[tokio::test]
    async fn killing_the_zoomed_panel_clears_zoom() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Ok(panel) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.focus.set_focus(Some(panel));
        mux.apply_command(CaucusCommand::ToggleZoom);
        assert_eq!(mux.zoomed(), Some(panel));

        Multiplexer::kill_panel(&mut mux, panel).unwrap();
        assert!(
            mux.zoomed().is_none(),
            "zoom must clear when its panel dies"
        );

        mux.shutdown();
    }

    /// A `wait_for_panels` whose ids do not exist is answered immediately —
    /// `register_wait` sends the reply now and stashes no `PendingWait`.
    #[tokio::test]
    async fn wait_for_unknown_panels_replies_immediately() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let ghost = PanelId::new();

        let (reply_tx, reply_rx) = oneshot::channel();
        mux.register_wait(vec![ghost], Some(60), reply_tx);

        // No pending wait stashed; the reply is already available.
        assert!(mux.pending_waits.is_empty());
        match reply_rx.await.unwrap() {
            // The ghost id is omitted from the summary (it never existed).
            ControlResponse::Panels { panels } => assert!(panels.is_empty()),
            other => panic!("expected Panels, got {other:?}"),
        }
    }

    /// A `wait_for_panels` whose deadline has already passed fires on the next
    /// `poll_pending_waits` tick even though no panel settled.
    #[tokio::test]
    async fn wait_times_out_via_poll_pending_waits() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        // Stash a wait whose deadline is already in the past, against a panel
        // id the multiplexer has no panel for *and* that we keep "unsettled"
        // by registering it directly with an already-elapsed deadline.
        let (reply_tx, mut reply_rx) = oneshot::channel();
        mux.pending_waits.push(PendingWait {
            panels: vec![PanelId::new()],
            deadline: Instant::now() - Duration::from_secs(1),
            reply: reply_tx,
        });
        // Not yet fired.
        assert!(reply_rx.try_recv().is_err());

        mux.poll_pending_waits();
        assert!(
            mux.pending_waits.is_empty(),
            "timed-out wait must be dropped"
        );
        match reply_rx.try_recv() {
            Ok(ControlResponse::Panels { .. }) => {}
            other => panic!("expected a Panels reply after timeout, got {other:?}"),
        }
    }

    /// The deferred-reply path end to end: register a wait on a `Working`
    /// panel, confirm `poll_pending_waits` keeps it pending, then settle the
    /// panel and confirm the next poll fires the reply.
    ///
    /// Spawning a panel needs a real agent CLI; the test is skipped (not
    /// failed) when none is on PATH, matching `tests/mcp_integration.rs`.
    #[tokio::test]
    async fn poll_pending_waits_fires_when_a_panel_settles() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Ok(panel) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        // Force the panel into `Working` so the wait is genuinely pending.
        mux.note_prompt_delivered(panel);
        assert_eq!(
            mux.panels().iter().find(|p| p.id == panel).unwrap().state(),
            PanelState::Working,
        );

        let (reply_tx, mut reply_rx) = oneshot::channel();
        mux.register_wait(vec![panel], Some(600), reply_tx);
        assert_eq!(
            mux.pending_waits.len(),
            1,
            "wait must be stashed, not answered"
        );

        // A poll while the panel is still working leaves the wait pending.
        mux.poll_pending_waits();
        assert_eq!(mux.pending_waits.len(), 1);
        assert!(reply_rx.try_recv().is_err());

        // Settle the panel via a turn-completion signal (Working -> Idle).
        let session_id = mux.session.id;
        mux.handle_signal(TurnSignal::now(
            session_id,
            panel,
            crate::signal::TurnKind::Stop,
            None,
            serde_json::Value::Null,
        ));
        assert_eq!(
            mux.panels().iter().find(|p| p.id == panel).unwrap().state(),
            PanelState::Idle,
        );

        // The next poll fires + drops the wait, returning the panel's summary.
        mux.poll_pending_waits();
        assert!(mux.pending_waits.is_empty());
        match reply_rx.try_recv() {
            Ok(ControlResponse::Panels { panels }) => {
                assert_eq!(panels.len(), 1);
                assert_eq!(panels[0].panel_id, panel);
            }
            other => panic!("expected a Panels reply, got {other:?}"),
        }

        mux.shutdown();
    }

    /// `execute_control(Broadcast{..})` fans the same text into every panel:
    /// each real panel that exists is flipped to `Working` (with `enter`) and
    /// appears in the `Panels` reply; a non-existent id is non-fatal and is
    /// simply absent from the reply.
    ///
    /// Spawning a panel needs a real agent CLI; the test is skipped (not
    /// failed) when none is on PATH, matching `tests/mcp_integration.rs`.
    #[tokio::test]
    async fn broadcast_control_request_fans_text_into_every_panel() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Ok(a) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        let b = mux.spawn_panel("reviewer", None, None, None).unwrap();
        let ghost = PanelId::new();

        let resp = mux.execute_control(ControlRequest::Broadcast {
            // The ghost is interleaved between the two real ids — it must not
            // stop `b` from receiving the text.
            panels: vec![a, ghost, b],
            text: "the agenda".into(),
            enter: true,
        });

        match resp {
            ControlResponse::Panels { panels } => {
                // Only the two real panels come back; the ghost is omitted.
                assert_eq!(panels.len(), 2, "ghost id must be reported by absence");
                let ids: Vec<PanelId> = panels.iter().map(|s| s.panel_id).collect();
                assert!(ids.contains(&a) && ids.contains(&b));
                assert!(!ids.contains(&ghost));
            }
            other => panic!("expected Panels, got {other:?}"),
        }

        // `enter=true` opened a capture turn and flipped each real panel to
        // `Working`; the ghost did nothing.
        for id in [a, b] {
            assert_eq!(
                mux.panels().iter().find(|p| p.id == id).unwrap().state(),
                PanelState::Working,
            );
        }

        mux.shutdown();
    }

    #[test]
    fn rendered_capture_strips_escape_sequences() {
        // A raw turn capture: SGR colour, CR/LF, cursor moves — what a real
        // agent emits. `read_panel(since_last_turn)` must hand the main
        // worker readable text, never this escape soup.
        let raw = b"\x1b[1;32mhello\x1b[0m\r\nfrom \x1b[31mcaucus\x1b[0m\x1b[K\r\n";
        let text = Multiplexer::rendered_capture_text(raw, 80);
        assert!(
            !text.contains('\x1b'),
            "escape sequences must be rendered away: {text:?}"
        );
        assert!(text.contains("hello"), "got: {text:?}");
        assert!(text.contains("from caucus"), "got: {text:?}");
    }

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
