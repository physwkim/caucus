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
use crate::panel::lifecycle::{self, Panel, PanelState};
use crate::render::{Layout, Rect};
use crate::role::spec::AgentCli;
use crate::session::id::PanelId;
use crate::session::state::Session;
use crate::signal::TurnSignal;
use crate::signal::server::SignalServer;
use crate::worktree::cleanup::{CleanupJob, CleanupQueue};

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
    /// Whole-screen area the layout tiles.
    area: Rect,
    /// Set when the user requested quit (`Ctrl-A q`).
    quit: bool,
    /// Monotonic counter for agent-name suffixes per role.
    role_counts: HashMap<String, usize>,
}

impl Multiplexer {
    /// Build a multiplexer for `session`, binding the turn-signal socket.
    ///
    /// The session root directory and its `agents/` + `panels/` subdirectories
    /// are created here so manifest writes and capture spills have a home.
    pub fn new(session: Session, config: Config, area: Rect) -> Result<(Self, SignalServer)> {
        std::fs::create_dir_all(session.root_dir.join("agents"))
            .with_context(|| format!("create {}", session.root_dir.display()))?;
        std::fs::create_dir_all(session.root_dir.join("panels"))?;

        let sock_path = socket_path(&session);
        let signal_server = SignalServer::bind(&sock_path)
            .with_context(|| format!("bind turn-signal socket {}", sock_path.display()))?;

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
                area,
                quit: false,
                role_counts: HashMap::new(),
            },
            signal_server,
        ))
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
    /// pre-created worktree. The CEO panel is just `spawn_panel("ceo", ...)`.
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
            if let Err(err) = panel.pump() {
                warn!(panel = %panel.id, error = %err, "panel pump failed");
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

    /// Ingest a turn-completion signal: close the panel's capture turn and
    /// flip it to `Idle` (`docs/design.md` §4, §8.3).
    pub fn handle_signal(&mut self, signal: TurnSignal) {
        let Some(panel) = self.panels.iter_mut().find(|p| p.id == signal.panel_id) else {
            return;
        };
        panel.end_turn();
        // A turn signal means the agent is idle, waiting for the next prompt.
        if panel.state() == PanelState::Working {
            let _ = lifecycle::transition(panel, PanelState::Idle);
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

/// Choose a turn-signal socket path for `session`.
///
/// `docs/design.md` §7.1 names `<repo>/.caucus/sessions/<id>/caucus.sock`, but
/// a unix-domain socket path has a hard OS length cap (`SUN_LEN`, ~104 bytes
/// on macOS). A deep repo path easily blows that, so the socket is bound in
/// the system temp dir under a short, session-keyed name instead. The path is
/// injected into every agent as `CAUCUS_SOCK`, so its exact location is an
/// internal detail — only the env var is contract.
fn socket_path(session: &Session) -> PathBuf {
    let id = session.id.to_string();
    // Last 10 chars of the ULID — collision-safe enough for one host's
    // concurrent sessions, and keeps the path well under SUN_LEN.
    let suffix: String = id.chars().rev().take(10).collect::<String>()
        .chars()
        .rev()
        .collect();
    std::env::temp_dir().join(format!("caucus-{suffix}.sock"))
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
        let (mux, _server) = Multiplexer::new(session, config, area()).unwrap();
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
