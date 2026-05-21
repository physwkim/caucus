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
use tracing::warn;

use crate::agent::derive_state::DerivedState;
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

/// Default fallback budget for a registered round when the caller omits
/// `fallback_secs` — the safety-net deadline after which caucus delivers a
/// partial report even if some panels never settled.
const ROUND_FALLBACK_DEFAULT_SECS: u64 = 600;
/// Hard cap on a round's fallback budget.
const ROUND_FALLBACK_MAX_SECS: u64 = 3600;
/// A registered round's results are only injected into the main worker's
/// panel once it has been free of human keystrokes for at least this long —
/// so a delivery never lands in the middle of a line the user is composing.
const QUIET_WINDOW: Duration = Duration::from_millis(1000);

/// A round caucus is watching on the main worker's behalf
/// ([`Multiplexer::poll_pending_rounds`]).
///
/// Unlike a control request (each answered immediately), a round carries no
/// reply channel — `register_round` already acked at registration. Instead
/// the event loop watches it each tick and, once every panel has settled (or
/// `fallback_deadline` passes), assembles the panels' results and *injects*
/// them into the main worker's panel as a fresh turn. This is the caucus→main
/// push that the pull-only MCP transport cannot do.
struct PendingRound {
    /// Panel ids in the round. Ids that no longer exist count as settled
    /// (see [`Multiplexer::wait_panels_settled`]).
    panels: Vec<PanelId>,
    /// How each panel's result is read for the delivered report.
    read_mode: ReadPanelMode,
    /// Wall-clock instant past which the round is delivered regardless of
    /// state — the safety net, marking still-`working` panels unfinished.
    fallback_deadline: Instant,
}

/// Open scrollback-pager state (`Ctrl-A [`): a *frozen* snapshot of one
/// panel's rendered scrollback plus the current scroll offset.
///
/// The panel keeps running underneath; the pager shows this snapshot until
/// closed (tmux copy-mode behavior — new output appears only after exit).
/// Built by [`Multiplexer::enter_scroll`]; fields are `pub(crate)` so the
/// `render` layer can window the lines without a getter per field.
pub(crate) struct ScrollState {
    /// Role of the snapshotted panel, for the pager header.
    pub(crate) role: String,
    /// Rendered scrollback + live viewport, one entry per line, oldest first.
    pub(crate) lines: Vec<String>,
    /// Index of the topmost visible line — `0` is the oldest.
    pub(crate) offset: usize,
    /// Visible body height in rows (the page step), set at entry. Also the
    /// clamp window: the maximum offset is `lines.len() - page`.
    pub(crate) page: usize,
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
    /// Rounds caucus is watching to deliver to the main worker
    /// ([`Multiplexer::poll_pending_rounds`]).
    pending_rounds: Vec<PendingRound>,
    /// The main worker panel — the round-delivery target. Set when the main
    /// panel is spawned; `None` before then.
    main_panel_id: Option<PanelId>,
    /// Instant of the last human keystroke routed to the main panel. Gates
    /// round delivery so an injected turn never collides with a line the user
    /// is mid-compose (see [`QUIET_WINDOW`]).
    last_human_input: Option<Instant>,
    /// Selection menus already announced to the main worker, keyed by the
    /// panel showing the menu — value is the menu's content signature
    /// ([`Multiplexer::menu_signature`]). Dedups the proactive
    /// selection-prompt push ([`Multiplexer::poll_round_selection_prompts`])
    /// so a panel sitting on one chooser is announced once, not every tick;
    /// an entry is dropped when its panel leaves the menu, and replaced when
    /// the menu's content changes.
    notified_menus: HashMap<PanelId, u64>,
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
    /// Open scrollback pager (`Ctrl-A [`), or `None` for the live tiled view.
    /// While `Some`, the pager is drawn full-screen and captures input via the
    /// router's `scroll_open` gate; panels keep pumping underneath.
    scroll: Option<ScrollState>,
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
                pending_rounds: Vec::new(),
                main_panel_id: None,
                last_human_input: None,
                notified_menus: HashMap::new(),
                layout_mode: LayoutMode::default(),
                zoom: None,
                show_transcript: false,
                scroll: None,
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
            repo_root: self.session.repo_path.clone(),
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
                // A human keystroke to the main panel: stamp it so a round
                // delivery never lands in the middle of a line the user is
                // composing (see `poll_pending_rounds` / `QUIET_WINDOW`).
                if Some(panel) == self.main_panel_id {
                    self.last_human_input = Some(Instant::now());
                }
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
            CaucusCommand::EnterScroll => self.enter_scroll(),
            CaucusCommand::ExitScroll => self.exit_scroll(),
            CaucusCommand::ScrollUp => self.scroll_by(-1),
            CaucusCommand::ScrollDown => self.scroll_by(1),
            CaucusCommand::ScrollPageUp => self.scroll_page(-1),
            CaucusCommand::ScrollPageDown => self.scroll_page(1),
            CaucusCommand::ScrollTop => self.scroll_to_edge(true),
            CaucusCommand::ScrollBottom => self.scroll_to_edge(false),
        }
    }

    /// Whether the read-only transcript overlay is currently shown.
    pub fn show_transcript(&self) -> bool {
        self.show_transcript
    }

    /// The open scrollback pager, if any — for the draw layer (`tui::draw`).
    /// `pub(crate)`: [`ScrollState`] is an internal type, consumed only in-crate.
    pub(crate) fn scroll_state(&self) -> Option<&ScrollState> {
        self.scroll.as_ref()
    }

    /// Open the scrollback pager on the focused panel (`Ctrl-A [`): snapshot
    /// its rendered scrollback and freeze it for scrolling. A no-op when no
    /// panel is focused (or the focused id no longer resolves). Opening the
    /// pager supersedes the transcript overlay.
    fn enter_scroll(&mut self) {
        let Some(focused) = self.focus.focused() else {
            return;
        };
        let Some(panel) = self.panels.iter().find(|p| p.id == focused) else {
            return;
        };
        let role = panel.role.clone();
        let lines: Vec<String> = Self::scrollback_text(panel)
            .lines()
            .map(str::to_string)
            .collect();
        // Body height = whole screen minus the pager's border (2) + title (1)
        // + status (1). Approximate; the render layer re-clamps to the real
        // height, and `offset` clamps against this `page` consistently.
        let page = (self.area.height as usize).saturating_sub(4).max(1);
        // Open at the bottom (newest), like tmux copy-mode entry.
        let offset = lines.len().saturating_sub(page);
        self.show_transcript = false;
        self.focus.set_transcript_open(false);
        self.scroll = Some(ScrollState {
            role,
            lines,
            offset,
            page,
        });
        self.focus.set_scroll_open(true);
    }

    /// Close the scrollback pager, returning to the live tiled view.
    fn exit_scroll(&mut self) {
        self.scroll = None;
        self.focus.set_scroll_open(false);
    }

    /// Scroll the pager by `delta` lines (negative = toward older output),
    /// clamped to `[0, lines.len() - page]`. No-op when the pager is closed.
    fn scroll_by(&mut self, delta: isize) {
        if let Some(state) = self.scroll.as_mut() {
            let max = state.lines.len().saturating_sub(state.page) as isize;
            state.offset = (state.offset as isize + delta).clamp(0, max) as usize;
        }
    }

    /// Scroll the pager by `pages` pages (negative = toward older output).
    fn scroll_page(&mut self, pages: isize) {
        let step = self.scroll.as_ref().map_or(0, |s| s.page as isize);
        self.scroll_by(pages * step);
    }

    /// Jump the pager to the oldest line (`top`) or the newest (`!top`).
    fn scroll_to_edge(&mut self, top: bool) {
        if let Some(state) = self.scroll.as_mut() {
            state.offset = if top {
                0
            } else {
                state.lines.len().saturating_sub(state.page)
            };
        }
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
            ControlRequest::RegisterRound {
                panels,
                read_mode,
                fallback_secs,
            } => self.register_round(panels, read_mode, fallback_secs),
            ControlRequest::ReadMenu { panel } => match self.read_menu(panel) {
                Ok(text) => ControlResponse::Panel { text },
                Err(err) => ControlResponse::error(err),
            },
            ControlRequest::SelectOption { panel, index } => {
                match self.select_option(panel, index) {
                    Ok(()) => ControlResponse::Ok,
                    Err(err) => ControlResponse::error(err),
                }
            }
        }
    }

    /// Drain every queued control job from `server`, execute it, and answer
    /// each through its oneshot reply. Called once per event-loop tick — the
    /// single point at which main worker MCP tool calls touch live panels, on
    /// the same thread that pumps PTYs (Invariant I-5).
    ///
    /// Every request is answered immediately via
    /// [`Multiplexer::execute_control`]. `register_round` is non-blocking too:
    /// it acks with a panel snapshot and the round is delivered later by the
    /// caucus→main push in [`Multiplexer::poll_pending_rounds`] — so the event
    /// loop is never blocked and there is no deferred-reply path.
    pub fn drain_control(&mut self, server: &mut ControlServer) {
        while let Ok(job) = server.jobs().try_recv() {
            let ControlJob { request, reply } = job;
            let response = self.execute_control(request);
            // A dropped reply channel means the control-socket connection
            // closed before we answered — nothing to do.
            let _ = reply.send(response);
        }
    }

    /// Whether every panel id counts as "settled" for a round: its
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

    /// A [`ControlResponse::Panels`] snapshot of `panels` — the immediate ack
    /// `register_round` returns. A missing id is omitted (it is gone —
    /// `list_panels` would not show it either).
    fn panel_snapshot(&self, panels: &[PanelId]) -> ControlResponse {
        ControlResponse::Panels {
            panels: self.panel_summaries(panels),
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
    /// `register_round` return. A bad id is visible by its absence from that
    /// list, so the main worker can tell which panels a typo missed while the
    /// good ones still ran.
    fn broadcast(&mut self, panels: &[PanelId], text: &str, enter: bool) -> ControlResponse {
        for &panel in panels {
            // Per-panel failures (no such panel, write error) are non-fatal:
            // the other panels in the round still get the text.
            let _ = self.send_keys(panel, text, enter);
        }
        ControlResponse::Panels {
            panels: self.panel_summaries(panels),
        }
    }

    /// The [`PanelSummary`] for each id in `panels` that still exists, in the
    /// caller's order — missing ids are omitted (they were killed or the id
    /// was bad).
    fn panel_summaries(&self, panels: &[PanelId]) -> Vec<PanelSummary> {
        let all = self.list_panels();
        panels
            .iter()
            .filter_map(|id| all.iter().find(|s| s.panel_id == *id).cloned())
            .collect()
    }

    /// Register a round on `panels`: stash a [`PendingRound`] for the event
    /// loop to deliver and ack immediately with the panels' current snapshot.
    /// `fallback_secs` is clamped to `[1, ROUND_FALLBACK_MAX_SECS]`, defaulting
    /// to `ROUND_FALLBACK_DEFAULT_SECS`; `read_mode` defaults to `LastMessage`.
    ///
    /// Unlike the removed blocking wait, this never special-cases an
    /// already-settled round — delivery is decided uniformly by
    /// [`Multiplexer::poll_pending_rounds`] (which also gates on the main panel
    /// being idle), so the registration path has exactly one shape.
    fn register_round(
        &mut self,
        panels: Vec<PanelId>,
        read_mode: Option<ReadPanelMode>,
        fallback_secs: Option<u64>,
    ) -> ControlResponse {
        let budget = fallback_secs
            .unwrap_or(ROUND_FALLBACK_DEFAULT_SECS)
            .clamp(1, ROUND_FALLBACK_MAX_SECS);
        let ack = self.panel_snapshot(&panels);
        self.pending_rounds.push(PendingRound {
            panels,
            read_mode: read_mode.unwrap_or(ReadPanelMode::LastMessage),
            fallback_deadline: Instant::now() + Duration::from_secs(budget),
        });
        ack
    }

    /// Deliver one due, deliverable round to the main worker — the caucus→main
    /// push. Called once per event-loop tick, after signals + pump have
    /// updated panel state.
    ///
    /// A round is *due* when all its panels have settled, or its
    /// `fallback_deadline` has passed. It is *delivered* only while the main
    /// panel exists, is `Idle`, and has seen no human keystroke within
    /// [`QUIET_WINDOW`] — so an injected turn never collides with a line the
    /// user is composing. At most one round is delivered per tick: the
    /// injection flips the main panel to `Working`, so any other due round
    /// naturally holds until the main worker is idle again. A due round with
    /// no main panel to deliver to is dropped (it would otherwise be stranded).
    pub fn poll_pending_rounds(&mut self) {
        if self.pending_rounds.is_empty() {
            return;
        }
        let now = Instant::now();
        // Take the queue so the settle-checks below can borrow `self`.
        let rounds = std::mem::take(&mut self.pending_rounds);

        let main_id = self.main_panel_id;
        let deliverable = self.main_deliverable();

        let mut delivered = false;
        for round in rounds {
            let due = now >= round.fallback_deadline || self.wait_panels_settled(&round.panels);
            match main_id {
                // Due, gate open, nothing delivered yet this tick: assemble +
                // inject into the main panel, then drop the round.
                Some(main_id) if due && deliverable && !delivered => {
                    let report = self.assemble_round_report(&round.panels, round.read_mode);
                    if let Err(err) = McpToolSurface::send_keys(self, main_id, &report, true) {
                        warn!(error = %err, "round delivery to main panel failed");
                    }
                    delivered = true;
                }
                // Due but there is no main panel to deliver to: drop it.
                None if due => {}
                // Not due, gate closed, or already delivered one this tick:
                // keep it for a later tick.
                _ => self.pending_rounds.push(round),
            }
        }
    }

    /// Whether a caucus→main push may land *this tick*: the main panel exists,
    /// is coarse `Idle`, and no human keystroke hit it within [`QUIET_WINDOW`]
    /// (so the user is not mid-compose). The single gate shared by both push
    /// paths — round completion ([`Multiplexer::poll_pending_rounds`]) and
    /// selection prompts ([`Multiplexer::poll_round_selection_prompts`]). Each
    /// push flips the main panel to `Working`, closing the gate for the rest
    /// of the tick, so at most one push of either kind lands per tick.
    fn main_deliverable(&self) -> bool {
        let main_idle = self.main_panel_id.is_some_and(|id| {
            self.panels
                .iter()
                .find(|p| p.id == id)
                .is_some_and(|p| p.state() == PanelState::Idle)
        });
        let quiet = self
            .last_human_input
            .is_none_or(|t| Instant::now().duration_since(t) >= QUIET_WINDOW);
        main_idle && quiet
    }

    /// Announce to the main worker when a panel in a pending round has stopped
    /// on an interactive selection menu — the caucus→main *selection* push.
    ///
    /// A chooser fires no `Stop` hook, so the panel stays coarse `Working` and
    /// its round never settles; without this the main worker would only learn
    /// at the fallback deadline. caucus pushes an interim notice so the main
    /// worker can answer it (`read_menu` / `select_option`) and let the round
    /// finish. Gated by [`Multiplexer::main_deliverable`] and deduped by menu
    /// content signature ([`Multiplexer::notified_menus`]): a panel sitting on
    /// one menu is announced once; a menu whose content changes re-announces;
    /// a panel that leaves its menu is forgotten so a future menu re-announces.
    /// At most one notice per tick (shares the deliverability gate with round
    /// completion, which a push closes by flipping the main panel to `Working`).
    pub fn poll_round_selection_prompts(&mut self) {
        let Some(main_id) = self.main_panel_id else {
            return;
        };
        if self.pending_rounds.is_empty() {
            return;
        }

        // Round panels currently showing a menu, with a content signature
        // (question + options, not the cursor row) so cursor movement alone
        // never re-announces.
        let round_panels: std::collections::HashSet<PanelId> = self
            .pending_rounds
            .iter()
            .flat_map(|r| r.panels.iter().copied())
            .collect();
        let mut open: Vec<(PanelId, u64)> = Vec::new();
        let mut menus: HashMap<PanelId, crate::term::Menu> = HashMap::new();
        for pid in round_panels {
            if pid == main_id {
                continue;
            }
            if let Some(p) = self.panels.iter().find(|p| p.id == pid)
                && let Some(menu) = Self::panel_menu(p)
            {
                open.push((pid, Self::menu_signature(&menu)));
                menus.insert(pid, menu);
            }
        }

        let (pick, open_set) = Self::pick_menu_to_notify(&open, &self.notified_menus);
        // Forget panels that have left their menu, so a future menu re-announces.
        self.notified_menus.retain(|pid, _| open_set.contains(pid));

        // One notice per tick, only while the gate is open. Dedup state above
        // is updated regardless; the panel is marked notified only on a real
        // push, so a closed gate this tick still announces next tick.
        if !self.main_deliverable() {
            return;
        }
        let Some(pid) = pick else {
            return;
        };
        // `pick` came from `open`, so both lookups are present.
        let sig = open
            .iter()
            .find(|(p, _)| *p == pid)
            .map(|(_, s)| *s)
            .unwrap();
        let menu = menus.remove(&pid).unwrap();
        let role = self
            .panels
            .iter()
            .find(|p| p.id == pid)
            .map(|p| p.role.clone())
            .unwrap_or_default();
        let notice = format!(
            "[caucus] panel {pid} (role: {role}) is waiting on a selection — \
             answer it so the round can finish.\n{}\n(answer with \
             select_option({pid}, <number>); for a free-text reply pick the \
             'type something' option, then send_keys your text.)",
            Self::render_menu(&menu)
        );
        if let Err(err) = McpToolSurface::send_keys(self, main_id, &notice, true) {
            warn!(error = %err, "selection-prompt notice to main panel failed");
        }
        self.notified_menus.insert(pid, sig);
    }

    /// Pick which round panel to announce a selection menu for this tick.
    ///
    /// Pure decision core of [`Multiplexer::poll_round_selection_prompts`]:
    /// given the panels currently showing a menu as `(panel, signature)` and
    /// the already-notified set, return the first panel whose signature is new
    /// or changed (the one to push), plus the set of panels showing a menu now
    /// (so the caller can prune stale dedup entries).
    fn pick_menu_to_notify(
        open: &[(PanelId, u64)],
        notified: &HashMap<PanelId, u64>,
    ) -> (Option<PanelId>, std::collections::HashSet<PanelId>) {
        let open_set = open.iter().map(|(p, _)| *p).collect();
        let pick = open
            .iter()
            .find(|(p, sig)| notified.get(p) != Some(sig))
            .map(|(p, _)| *p);
        (pick, open_set)
    }

    /// Content signature of a selection menu — a hash of the question and the
    /// numbered option labels, **excluding** the cursor row. Two reads of the
    /// same chooser hash equal even as the highlighted row moves; a changed
    /// question or option set hashes differently. Used to dedup the
    /// selection-prompt push ([`Multiplexer::notified_menus`]).
    fn menu_signature(menu: &crate::term::Menu) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        menu.question.hash(&mut h);
        for opt in &menu.options {
            opt.number.hash(&mut h);
            opt.label.hash(&mut h);
        }
        h.finish()
    }

    /// Assemble a round's delivery message: a self-describing block per panel
    /// — role + current state, plus its result read via `read_mode` (settled
    /// panels) or an "unfinished" marker (a panel still `working` when the
    /// fallback deadline forced delivery). A panel id that no longer exists is
    /// reported as gone. This is the text injected into the main worker's
    /// panel as a fresh turn.
    fn assemble_round_report(&self, panels: &[PanelId], read_mode: ReadPanelMode) -> String {
        let all_settled = self.wait_panels_settled(panels);
        let mut out = format!(
            "[caucus] Round complete — {} panel(s){}.\n",
            panels.len(),
            if all_settled {
                ""
            } else {
                " (fallback deadline reached; some panels did not finish)"
            }
        );
        for &id in panels {
            let Some(panel) = self.panels.iter().find(|p| p.id == id) else {
                out.push_str(&format!("\n## panel {id} — gone (killed)\n"));
                continue;
            };
            let state = panel.state();
            out.push_str(&format!(
                "\n## panel {id} (role: {}) — {}\n",
                panel.role,
                state.label()
            ));
            if matches!(state, PanelState::Working | PanelState::Spawning) {
                out.push_str("⏳ still working — did not finish within the fallback window.\n");
                continue;
            }
            let body = self
                .read_panel(id, read_mode)
                .unwrap_or_else(|e| format!("(could not read panel: {e})"));
            let body = body.trim();
            out.push_str(if body.is_empty() {
                "(no output captured)\n"
            } else {
                body
            });
            if !body.is_empty() {
                out.push('\n');
            }
        }
        out
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

    /// Scan a panel's visible grid for an interactive selection menu
    /// ([`crate::term::scan_menu`]). `None` unless one is confidently detected.
    fn panel_menu(panel: &Panel) -> Option<crate::term::Menu> {
        let (_, rows) = panel.grid().size();
        let lines: Vec<String> = (0..rows)
            .map(|r| panel.grid().row_text(r).trim_end().to_string())
            .collect();
        crate::term::scan_menu(&lines)
    }

    /// Overlay a live selection-menu detection onto the turn-signal-derived
    /// state. A visible menu means the agent stopped mid-turn for a choice —
    /// which the `Stop`-hook state cannot see — so it outranks the
    /// signal-derived `Working`/`Idle` (mirroring `derive_agent_state`, where
    /// a grid hint is weighed before the turn signal). It never masks a
    /// stronger state (`Exited`/`Blocked*`/`Interrupted`/`Degraded`).
    fn overlay_menu_state(base: DerivedState, has_menu: bool) -> DerivedState {
        if has_menu && matches!(base, DerivedState::Working | DerivedState::Idle) {
            DerivedState::AwaitingSelection
        } else {
            base
        }
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

/// Bytes to write to a panel's PTY to deliver `text` and, when `enter`,
/// submit it.
///
/// A TUI agent (e.g. Claude Code) that has enabled bracketed-paste mode
/// (`CSI ?2004h`) treats a multi-byte input burst as a *paste*: a `\r` carried
/// inside the burst is inserted into the prompt buffer as a literal newline
/// instead of submitting the line — the "Enter doesn't go through, but caucus
/// thinks it did" bug. Delivering `text` as a *proper* bracketed paste
/// (`ESC[200~` … `ESC[201~`) and placing the submitting `\r` **after** the
/// paste-end marker makes the agent insert the text verbatim (multi-line safe,
/// so a multi-line round report no longer submits at its first newline) and
/// then see the trailing `\r` as a discrete keypress that submits. The
/// paste-end marker delimits the paste explicitly, so this is robust in a
/// single burst — no inter-write timing gap is needed.
///
/// When `bracketed` is false the agent has not enabled the mode, so the
/// markers would land as literal `[200~`/`[201~` garbage; fall back to the raw
/// `text` (+ `\r`). An empty `text` is never wrapped — a bare Enter is just
/// `\r`.
fn encode_input(text: &[u8], enter: bool, bracketed: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 14);
    if bracketed && !text.is_empty() {
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text);
        out.extend_from_slice(b"\x1b[201~");
    } else {
        out.extend_from_slice(text);
    }
    if enter {
        out.push(b'\r');
    }
    out
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
        // Frame the prompt for the agent's input mode: when it has enabled
        // bracketed paste, deliver `text` as a real paste so the submitting
        // `\r` is seen as a discrete keypress (and any newline *inside* the
        // text does not submit early) — see `encode_input`.
        let bytes = encode_input(text.as_bytes(), enter, p.grid().bracketed_paste());
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
                // A live selection menu on the grid overlays `awaiting_selection`
                // (no Stop hook fires while a chooser is open).
                let (state, agent_cli) = match self.manifests.get(&p.id) {
                    Some(m) => {
                        let st = Self::overlay_menu_state(
                            m.derived_state(),
                            Self::panel_menu(p).is_some(),
                        );
                        (st.as_str().to_string(), m.agent_cli)
                    }
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

    fn read_menu(&self, panel: PanelId) -> Result<String, McpError> {
        let p = self
            .panels
            .iter()
            .find(|p| p.id == panel)
            .ok_or(McpError::NoSuchPanel(panel))?;
        Ok(match Self::panel_menu(p) {
            Some(menu) => Self::render_menu(&menu),
            None => "(no selection menu visible on this panel)".to_string(),
        })
    }

    fn select_option(&mut self, panel: PanelId, index: usize) -> Result<(), McpError> {
        // Scan the menu (immutable) before writing (mutable) — no overlapping
        // borrows of `self.panels`.
        let menu = {
            let p = self
                .panels
                .iter()
                .find(|p| p.id == panel)
                .ok_or(McpError::NoSuchPanel(panel))?;
            Self::panel_menu(p)
                .ok_or_else(|| McpError::Tool("no selection menu visible on this panel".into()))?
        };
        let target = menu
            .options
            .iter()
            .position(|o| o.number == index)
            .ok_or_else(|| {
                McpError::Tool(format!(
                    "no option {index} in the menu (options 1..={})",
                    menu.options.len()
                ))
            })?;
        let bytes = Self::menu_nav_bytes(menu.cursor, target);

        let p = self
            .panels
            .iter_mut()
            .find(|p| p.id == panel)
            .ok_or(McpError::NoSuchPanel(panel))?;
        p.write_input(&bytes)
            .map_err(|e| McpError::Tool(format!("select_option: {e}")))?;
        // Submitting a selection resumes the agent's turn — open a capture turn
        // and flip the panel to `Working`, exactly like the `send_keys` path.
        self.note_prompt_delivered(panel);
        Ok(())
    }
}

impl Multiplexer {
    /// Render a detected [`crate::term::Menu`] as readable text for the main
    /// worker: the question, the numbered options (the highlighted one marked),
    /// and how to answer.
    fn render_menu(menu: &crate::term::Menu) -> String {
        let mut out = String::from("selection menu:\n");
        if !menu.question.is_empty() {
            out.push_str(&format!("question: {}\n", menu.question));
        }
        for (i, opt) in menu.options.iter().enumerate() {
            let marker = if i == menu.cursor { "❯ " } else { "  " };
            out.push_str(&format!("{marker}{}. {}\n", opt.number, opt.label));
        }
        out.push_str("(answer with select_option(panel, <number>))");
        out
    }

    /// Bytes that move a chooser's cursor from `cursor` to `target` and submit:
    /// `|target-cursor|` arrow keys (down when target is lower, up otherwise)
    /// then Enter. Reuses [`crate::input::encode_key`] so the xterm sequences
    /// match what a real keyboard would send.
    fn menu_nav_bytes(cursor: usize, target: usize) -> Vec<u8> {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (code, count) = if target >= cursor {
            (KeyCode::Down, target - cursor)
        } else {
            (KeyCode::Up, cursor - target)
        };
        let arrow =
            crate::input::encode_key(&KeyEvent::new(code, KeyModifiers::NONE)).unwrap_or_default();
        let mut bytes = Vec::new();
        for _ in 0..count {
            bytes.extend_from_slice(&arrow);
        }
        bytes.extend_from_slice(
            &crate::input::encode_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap_or_default(),
        );
        bytes
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

    /// A live selection menu overlays `AwaitingSelection` onto an otherwise
    /// signal-derived state, but never masks a stronger state.
    #[test]
    fn overlay_menu_state_only_overrides_working_and_idle() {
        use DerivedState::*;
        // Mid-turn (Working) or back-at-prompt (Idle) + menu → AwaitingSelection.
        assert_eq!(
            Multiplexer::overlay_menu_state(Working, true),
            AwaitingSelection
        );
        assert_eq!(
            Multiplexer::overlay_menu_state(Idle, true),
            AwaitingSelection
        );
        // No menu detected → unchanged.
        assert_eq!(Multiplexer::overlay_menu_state(Working, false), Working);
        // Stronger states are never masked by a stray on-screen menu.
        assert_eq!(Multiplexer::overlay_menu_state(Exited, true), Exited);
        assert_eq!(
            Multiplexer::overlay_menu_state(BlockedMergeConflict, true),
            BlockedMergeConflict
        );
        assert_eq!(
            Multiplexer::overlay_menu_state(InterruptedTransport, true),
            InterruptedTransport
        );
    }

    /// `encode_input` frames a prompt for the agent's input mode — bracketed
    /// when the agent enabled `?2004h`, raw otherwise — per boundary.
    #[test]
    fn encode_input_wraps_a_paste_with_enter_after_the_marker() {
        // Bracketed + submit: text inside the paste, the submitting `\r` AFTER
        // the paste-end marker so it is a discrete keypress, not absorbed.
        assert_eq!(
            encode_input(b"hello", true, true),
            b"\x1b[200~hello\x1b[201~\r",
        );
    }

    #[test]
    fn encode_input_paste_keeps_internal_newlines_inside_the_markers() {
        // A multi-line report: the internal newline stays *inside* the paste
        // (does not submit early); only the trailing `\r` is outside.
        assert_eq!(
            encode_input(b"line1\nline2", true, true),
            b"\x1b[200~line1\nline2\x1b[201~\r",
        );
    }

    #[test]
    fn encode_input_bracketed_without_enter_has_no_trailing_cr() {
        assert_eq!(encode_input(b"hi", false, true), b"\x1b[200~hi\x1b[201~");
    }

    #[test]
    fn encode_input_unbracketed_is_raw_text_plus_cr() {
        // Agent without `?2004`: markers would be literal garbage, so fall back
        // to raw text + `\r`.
        assert_eq!(encode_input(b"hello", true, false), b"hello\r");
        assert_eq!(encode_input(b"hello", false, false), b"hello");
    }

    #[test]
    fn encode_input_empty_text_is_a_bare_enter_never_wrapped() {
        // A bare Enter (e.g. confirming) is just `\r`, never empty paste markers.
        assert_eq!(encode_input(b"", true, true), b"\r");
        assert_eq!(encode_input(b"", true, false), b"\r");
        assert_eq!(encode_input(b"", false, true), b"");
    }

    /// `menu_nav_bytes` emits the right count + direction of arrow keys, then
    /// Enter — per navigation boundary.
    #[test]
    fn menu_nav_bytes_moves_the_cursor_and_submits() {
        // Down two then Enter (cursor 0 → option index 2).
        assert_eq!(Multiplexer::menu_nav_bytes(0, 2), b"\x1b[B\x1b[B\r");
        // Up two then Enter (cursor 2 → index 0).
        assert_eq!(Multiplexer::menu_nav_bytes(2, 0), b"\x1b[A\x1b[A\r");
        // Already on target: just Enter, no arrows.
        assert_eq!(Multiplexer::menu_nav_bytes(1, 1), b"\r");
    }

    /// `render_menu` lists the options and marks the highlighted one.
    #[test]
    fn render_menu_marks_the_cursor_option() {
        let menu = crate::term::Menu {
            question: "Pick one".to_string(),
            options: vec![
                crate::term::MenuOption {
                    number: 1,
                    label: "alpha".to_string(),
                },
                crate::term::MenuOption {
                    number: 2,
                    label: "beta".to_string(),
                },
            ],
            cursor: 1,
        };
        let text = Multiplexer::render_menu(&menu);
        assert!(text.contains("question: Pick one"));
        assert!(text.contains("❯ 2. beta"), "cursor option marked: {text:?}");
        assert!(text.contains("  1. alpha"));
        assert!(text.contains("select_option"));
    }

    /// Build a two-option menu with the given question, labels, and cursor.
    fn menu_of(question: &str, labels: [&str; 2], cursor: usize) -> crate::term::Menu {
        crate::term::Menu {
            question: question.to_string(),
            options: labels
                .iter()
                .enumerate()
                .map(|(i, l)| crate::term::MenuOption {
                    number: i + 1,
                    label: l.to_string(),
                })
                .collect(),
            cursor,
        }
    }

    /// `menu_signature` tracks menu *content* — question + option labels — and
    /// ignores the cursor row, so navigation alone never re-announces.
    #[test]
    fn menu_signature_ignores_cursor_tracks_content() {
        let base = menu_of("Pick one", ["alpha", "beta"], 0);
        // Same content, cursor moved → same signature.
        let moved = menu_of("Pick one", ["alpha", "beta"], 1);
        assert_eq!(
            Multiplexer::menu_signature(&base),
            Multiplexer::menu_signature(&moved),
            "cursor movement must not change the signature"
        );
        // Changed option label → different signature.
        let relabelled = menu_of("Pick one", ["alpha", "gamma"], 0);
        assert_ne!(
            Multiplexer::menu_signature(&base),
            Multiplexer::menu_signature(&relabelled),
            "a changed option must change the signature"
        );
        // Changed question → different signature.
        let requestioned = menu_of("Pick another", ["alpha", "beta"], 0);
        assert_ne!(
            Multiplexer::menu_signature(&base),
            Multiplexer::menu_signature(&requestioned),
            "a changed question must change the signature"
        );
    }

    /// `pick_menu_to_notify` announces a panel's menu once, re-announces on a
    /// content change, stays silent while unchanged, and reports the open set
    /// so the caller can prune panels that have left their menus.
    #[test]
    fn pick_menu_to_notify_announces_new_and_dedups() {
        let pid = PanelId::new();
        let sig_a = 11u64;
        let sig_b = 22u64;

        // Nothing on screen → nothing to announce, empty open set.
        let (pick, open) = Multiplexer::pick_menu_to_notify(&[], &HashMap::new());
        assert_eq!(pick, None);
        assert!(open.is_empty());

        // A menu not yet notified → announce it; open set carries the panel.
        let (pick, open) = Multiplexer::pick_menu_to_notify(&[(pid, sig_a)], &HashMap::new());
        assert_eq!(pick, Some(pid));
        assert!(open.contains(&pid));

        // Same menu already notified → silent.
        let notified = HashMap::from([(pid, sig_a)]);
        let (pick, open) = Multiplexer::pick_menu_to_notify(&[(pid, sig_a)], &notified);
        assert_eq!(pick, None);
        assert!(open.contains(&pid));

        // Menu content changed under the same panel → re-announce.
        let (pick, _) = Multiplexer::pick_menu_to_notify(&[(pid, sig_b)], &notified);
        assert_eq!(pick, Some(pid));

        // Panel left its menu (not in `open`) → not in the open set, so the
        // caller's retain drops its dedup entry.
        let (pick, open) = Multiplexer::pick_menu_to_notify(&[], &notified);
        assert_eq!(pick, None);
        assert!(!open.contains(&pid));
    }

    /// `EnterScroll` with no focused panel is a no-op (no pager, no panic).
    #[tokio::test]
    async fn enter_scroll_with_no_focus_is_a_noop() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.apply_command(CaucusCommand::EnterScroll);
        assert!(mux.scroll_state().is_none());
    }

    /// Inject a known pager state directly (no PTY needed) and prove the offset
    /// clamps at both ends — per-boundary, not per-scenario.
    #[tokio::test]
    async fn scroll_offset_clamps_at_both_ends() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        // 10 lines, page of 4 → max offset = 6. Start mid-buffer.
        let lines: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
        mux.scroll = Some(ScrollState {
            role: "worker".to_string(),
            lines,
            offset: 3,
            page: 4,
        });

        mux.apply_command(CaucusCommand::ScrollUp);
        assert_eq!(mux.scroll_state().unwrap().offset, 2);
        mux.apply_command(CaucusCommand::ScrollDown);
        assert_eq!(mux.scroll_state().unwrap().offset, 3);

        // Top edge: never below 0.
        mux.apply_command(CaucusCommand::ScrollTop);
        assert_eq!(mux.scroll_state().unwrap().offset, 0);
        mux.apply_command(CaucusCommand::ScrollUp);
        assert_eq!(mux.scroll_state().unwrap().offset, 0);
        mux.apply_command(CaucusCommand::ScrollPageUp);
        assert_eq!(mux.scroll_state().unwrap().offset, 0);

        // Bottom edge: never past lines.len() - page (= 6).
        mux.apply_command(CaucusCommand::ScrollBottom);
        assert_eq!(mux.scroll_state().unwrap().offset, 6);
        mux.apply_command(CaucusCommand::ScrollDown);
        assert_eq!(mux.scroll_state().unwrap().offset, 6);
        mux.apply_command(CaucusCommand::ScrollPageDown);
        assert_eq!(mux.scroll_state().unwrap().offset, 6);

        // One page up from the bottom lands a page (4) earlier.
        mux.apply_command(CaucusCommand::ScrollPageUp);
        assert_eq!(mux.scroll_state().unwrap().offset, 2);
    }

    /// A buffer shorter than a page pins the offset at 0 (max = 0).
    #[tokio::test]
    async fn scroll_offset_pins_at_zero_when_buffer_shorter_than_page() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.scroll = Some(ScrollState {
            role: "worker".to_string(),
            lines: vec!["only line".to_string()],
            offset: 0,
            page: 4,
        });
        mux.apply_command(CaucusCommand::ScrollBottom);
        assert_eq!(mux.scroll_state().unwrap().offset, 0);
        mux.apply_command(CaucusCommand::ScrollDown);
        assert_eq!(mux.scroll_state().unwrap().offset, 0);
    }

    /// `ExitScroll` clears the pager state.
    #[tokio::test]
    async fn exit_scroll_clears_the_pager() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.scroll = Some(ScrollState {
            role: "worker".to_string(),
            lines: vec!["a".to_string(), "b".to_string()],
            offset: 0,
            page: 1,
        });
        mux.apply_command(CaucusCommand::ExitScroll);
        assert!(mux.scroll_state().is_none());
    }

    /// `EnterScroll` snapshots the focused panel and opens at the bottom
    /// (newest). CLI-gated: spawning a panel needs a real agent CLI.
    #[tokio::test]
    async fn enter_scroll_snapshots_the_focused_panel() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let Ok(_id) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        // Spawning the first panel auto-focuses it.
        mux.apply_command(CaucusCommand::EnterScroll);
        let state = mux.scroll_state().expect("pager open after EnterScroll");
        assert_eq!(state.role, "reviewer");
        // Opened at the bottom: offset is the clamped maximum.
        assert_eq!(state.offset, state.lines.len().saturating_sub(state.page));

        mux.shutdown();
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

    /// `register_round` acks immediately with a panel snapshot and stashes a
    /// `PendingRound` — it never blocks. An unknown id is omitted from the ack
    /// (it would not appear in `list_panels` either). `read_mode` defaults to
    /// `last_message`.
    #[tokio::test]
    async fn register_round_acks_and_stashes_a_pending_round() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let ghost = PanelId::new();

        let ack = mux.register_round(vec![ghost], None, Some(60));
        match ack {
            ControlResponse::Panels { panels } => assert!(panels.is_empty()),
            other => panic!("expected an immediate Panels ack, got {other:?}"),
        }
        assert_eq!(mux.pending_rounds.len(), 1, "round must be stashed");
        assert_eq!(mux.pending_rounds[0].read_mode, ReadPanelMode::LastMessage);
    }

    /// `assemble_round_report` reports an id that no longer exists as gone
    /// rather than panicking or omitting it silently.
    #[tokio::test]
    async fn assemble_round_report_marks_a_missing_panel_gone() {
        let tmp = TempDir::new().unwrap();
        let mux = mux(&tmp);
        let ghost = PanelId::new();

        let report = mux.assemble_round_report(&[ghost], ReadPanelMode::LastMessage);
        assert!(report.contains("Round complete"), "report: {report}");
        assert!(
            report.contains("gone"),
            "a missing panel must be marked gone: {report}"
        );
    }

    /// A due round with no main panel to deliver to is dropped — it would
    /// otherwise be stranded forever. (A non-existent id counts as settled, so
    /// the round is due immediately.)
    #[tokio::test]
    async fn poll_pending_rounds_drops_a_due_round_with_no_main() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        assert!(mux.main_panel_id.is_none());

        mux.register_round(vec![PanelId::new()], None, Some(600));
        assert_eq!(mux.pending_rounds.len(), 1);

        mux.poll_pending_rounds();
        assert!(
            mux.pending_rounds.is_empty(),
            "a due round with no main panel must be dropped"
        );
    }

    /// A due round is *held*, not delivered, while the main panel is not idle.
    /// Here `main_panel_id` points at an id with no live panel, so the idle
    /// gate is closed and the round stays pending for a later tick.
    #[tokio::test]
    async fn poll_pending_rounds_holds_when_main_not_idle() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.main_panel_id = Some(PanelId::new());

        mux.register_round(vec![PanelId::new()], None, Some(600));
        mux.poll_pending_rounds();
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "round must be held while the main panel is not idle"
        );
    }

    /// The caucus→main push end to end: a round on a `Working` sub-panel is
    /// held until the panel settles, then delivered to the idle main panel —
    /// proven by the main panel flipping to `Working` (the injection opens a
    /// turn) and the round being dropped. A fresh human keystroke also holds
    /// delivery (the quiet window).
    ///
    /// Spawning panels needs a real agent CLI; the test is skipped (not
    /// failed) when none is on PATH, matching `tests/mcp_integration.rs`.
    #[tokio::test]
    async fn poll_pending_rounds_delivers_to_idle_main_on_settle() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let session_id = mux.session.id;

        // Spawn a main panel and drive it to Idle (Spawning -> Working -> Idle).
        let Ok(main) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.main_panel_id = Some(main);
        mux.note_prompt_delivered(main);
        mux.handle_signal(TurnSignal::now(
            session_id,
            main,
            crate::signal::TurnKind::Stop,
            None,
            serde_json::Value::Null,
        ));
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Idle,
        );

        // A sub-panel in `Working`, with a round registered on it.
        let Ok(sub) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.note_prompt_delivered(sub);
        mux.register_round(vec![sub], None, Some(600));

        // Sub still working -> round held.
        mux.poll_pending_rounds();
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "round held while the sub-panel is working"
        );

        // Settle the sub-panel (Working -> Idle).
        mux.handle_signal(TurnSignal::now(
            session_id,
            sub,
            crate::signal::TurnKind::Stop,
            None,
            serde_json::Value::Null,
        ));

        // A fresh human keystroke to main closes the quiet window: still held.
        mux.last_human_input = Some(Instant::now());
        mux.poll_pending_rounds();
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "round held while the user is mid-compose (quiet window)"
        );

        // Clear the quiet window: now the round delivers.
        mux.last_human_input = None;
        mux.poll_pending_rounds();
        assert!(
            mux.pending_rounds.is_empty(),
            "round delivered once due + main idle + quiet"
        );
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "delivery injects a turn into the main panel",
        );

        mux.shutdown();
    }

    /// `assemble_round_report` marks a panel still `Working` (the fallback
    /// case) as unfinished rather than reading a half-done turn.
    ///
    /// Spawning a panel needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn assemble_round_report_marks_a_working_panel_unfinished() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let Ok(sub) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.note_prompt_delivered(sub);
        assert_eq!(
            mux.panels().iter().find(|p| p.id == sub).unwrap().state(),
            PanelState::Working,
        );

        let report = mux.assemble_round_report(&[sub], ReadPanelMode::LastMessage);
        assert!(
            report.contains("still working"),
            "a Working panel must be marked unfinished: {report}"
        );

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
