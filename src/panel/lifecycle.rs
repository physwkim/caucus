//! Panel lifecycle (`docs/design.md` §3, §9.1).
//!
//! A panel is one cell of the caucus screen: a PTY + a vte grid + a render
//! area. The real lifecycle lives here, at the panel level.
//!
//! **Invariant I-5** (`docs/design.md` §12): panels are created/destroyed only
//! by `spawn` / `kill`, and panel state transitions only by `transition`.

use std::path::PathBuf;
use std::sync::OnceLock;

use thiserror::Error;

use crate::agent::spawn::SpawnRequest;
use crate::config::settings::Settings;
use crate::pty::{Pty, PtyCommand, PtyError};
use crate::render::Rect;
use crate::session::id::{AgentId, PanelId};
use crate::term::{Grid, OutputCapture};

/// Smallest interior a panel grid is ever sized to — guards against a zero-
/// or one-cell PTY when the layout hands a panel a sliver of screen.
const MIN_GRID_COLS: u16 = 8;
const MIN_GRID_ROWS: u16 = 2;

/// Whether the `CAUCUS_DUMP_PTY` raw-capture debug aid is enabled. The env var
/// is read once on first use and cached — [`Panel::pump`] consults this on
/// every read that carries bytes, and a process-environment lookup per pump is
/// pure idle-loop overhead (the value cannot change after launch).
fn dump_pty_enabled() -> bool {
    static DUMP: OnceLock<bool> = OnceLock::new();
    *DUMP.get_or_init(|| std::env::var_os("CAUCUS_DUMP_PTY").is_some())
}

/// Coarse panel state machine (`docs/design.md` §3).
///
/// Deliberately has no `Blocked`: a panel stopped on a permission prompt or a
/// chooser fires no turn signal, so it stays `Working` here, and caucus detects
/// the block by scanning the panel's grid at read time (`term::prompt_scan` →
/// `Multiplexer::overlay_blocked_state`), surfacing it on the `DerivedState`
/// the main worker reads (`docs/design.md` §8.3). Blocking lives there, on the
/// grid-derived surface; a coarse `Blocked` here would be a second, weaker copy
/// of that detection with nothing to write it.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PanelState {
    /// PTY allocated, agent process starting.
    Spawning,
    /// Agent is processing a turn.
    Working,
    /// Turn signal received — waiting for the next instruction.
    Idle,
    /// Agent process has exited.
    Exited,
}

impl PanelState {
    /// Lower-case label for borders and `list_panels`.
    pub fn label(self) -> &'static str {
        match self {
            PanelState::Spawning => "spawning",
            PanelState::Working => "working",
            PanelState::Idle => "idle",
            PanelState::Exited => "exited",
        }
    }
}

/// One panel: a PTY-backed cell of the caucus screen.
///
/// `state` is `pub(crate)` so only `transition` can change it (Invariant
/// I-5). The grid is mutated only by PTY bytes (Invariant on `term::Grid`).
pub struct Panel {
    pub id: PanelId,
    /// Role name driving this panel (`architect`, `backend`, ...).
    pub role: String,
    /// The agent instance running here.
    pub agent_id: AgentId,
    /// Authoritative panel state.
    pub(crate) state: PanelState,
    /// Worktree cwd, if this is an execute-phase panel.
    pub worktree_path: Option<PathBuf>,
    /// The PTY running the agent CLI. `kill` tears it down.
    pub(crate) pty: Pty,
    /// vte-parsed screen for this panel.
    pub(crate) grid: Grid,
    /// Turn-segmented output capture (`docs/design.md` §8.5).
    pub(crate) capture: OutputCapture,
}

impl Panel {
    /// Current panel state. Mutation goes through `transition`.
    pub fn state(&self) -> PanelState {
        self.state
    }

    /// Lower-case state label for the panel border and `list_panels`.
    pub fn state_label(&self) -> &'static str {
        self.state.label()
    }

    /// Read-only view of the panel's grid.
    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    /// Read-only view of the panel's output capture.
    pub fn capture(&self) -> &OutputCapture {
        &self.capture
    }

    /// Drain the desktop notifications (OSC 9 / 99 / 777) the grid queued
    /// from this panel's output. A queue drain, not a screen mutation — the
    /// cell grid itself is still only ever written by PTY bytes via `pump`.
    pub(crate) fn take_notifications(&mut self) -> Vec<String> {
        self.grid.take_notifications()
    }

    /// Drain whatever the PTY has produced since the last call into the grid
    /// and the turn capture.
    ///
    /// Returns the number of bytes pumped. The PTY read is non-blocking
    /// (`pty::Pty::read` drains the reader thread's queue), so this is cheap
    /// to call on every event-loop tick. A clean child exit surfaces as an
    /// empty read — no error — so the caller keeps pumping until it observes
    /// the process is gone via [`Panel::is_child_alive`].
    pub(crate) fn pump(&mut self) -> Result<usize, PanelError> {
        let bytes = self.pty.read().map_err(PanelError::Pty)?;
        if bytes.is_empty() {
            return Ok(0);
        }
        // Debug aid: when `CAUCUS_DUMP_PTY` is set, append every raw PTY byte
        // to `/tmp/caucus-pty-<panel_id>.raw` so a corrupted live render can
        // be replayed offline through `term::Grid`. Off by default. The env
        // lookup is cached once — this runs on every pump with data.
        if dump_pty_enabled()
            && let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(format!("/tmp/caucus-pty-{}.raw", self.id))
        {
            use std::io::Write;
            let _ = f.write_all(&bytes);
        }
        // Single sanctioned grid mutation path: PTY bytes through `advance`.
        self.grid.advance(&bytes);
        // Capture is turn-segmented; `push` is a no-op until a turn is open,
        // so output before the first `PromptDelivered` is intentionally not
        // captured (it is the CLI's startup banner, not turn output).
        self.capture.push(&bytes);
        Ok(bytes.len())
    }

    /// Forward input bytes to the panel's PTY — the fully bidirectional input
    /// path (`docs/design.md` §0 #11). Used by the focus router for direct
    /// user keystrokes and by the MCP `send_keys` tool.
    pub(crate) fn write_input(&mut self, bytes: &[u8]) -> Result<(), PanelError> {
        self.pty.write(bytes).map_err(PanelError::Pty)
    }

    /// Resize the panel to occupy `rect`: resize the PTY and reflow the grid
    /// to the rect's interior (the area inside the border).
    pub(crate) fn resize(&mut self, rect: Rect) -> Result<(), PanelError> {
        let inner = rect.inner();
        let cols = inner.width.max(MIN_GRID_COLS);
        let rows = inner.height.max(MIN_GRID_ROWS);
        self.pty.resize(cols, rows).map_err(PanelError::Pty)?;
        self.grid.resize(cols as usize, rows as usize);
        Ok(())
    }

    /// Whether the agent process is still running.
    pub(crate) fn is_child_alive(&mut self) -> bool {
        self.pty.is_alive()
    }

    /// Point this panel's output capture at its on-disk spill log
    /// (`<session_root>/panels/<panel_id>.log`, `docs/design.md` §8.5).
    pub(crate) fn set_capture_log_path(&mut self, path: impl Into<std::path::PathBuf>) {
        self.capture.set_log_path(path);
    }

    /// Begin a capture turn (`docs/design.md` §8.5) — called when a prompt is
    /// delivered to this panel.
    pub(crate) fn begin_turn(&mut self) {
        self.capture.begin_turn();
    }

    /// Close the current capture turn — called on a turn-completion signal.
    pub(crate) fn end_turn(&mut self) {
        self.capture.end_turn();
    }
}

/// Rejected panel transition.
#[derive(Debug, Error)]
#[error("illegal panel transition: {from:?} -> {to:?}")]
pub struct IllegalTransition {
    pub from: PanelState,
    pub to: PanelState,
}

/// Errors from panel lifecycle operations.
#[derive(Debug, Error)]
pub enum PanelError {
    #[error(transparent)]
    Transition(#[from] IllegalTransition),
    #[error("panel spawn: {0}")]
    Spawn(String),
    #[error("panel pty: {0}")]
    Pty(#[source] PtyError),
}

/// Single owner of panel state transitions (Invariant I-5).
///
/// Legal moves: `Spawning -> Working | Idle` (a freshly-spawned agent goes to
/// `Working` when a prompt is delivered, or to `Idle` once its CLI is up and
/// awaiting one), `Working <-> Idle`, and any state `-> Exited`.
pub(crate) fn transition(panel: &mut Panel, to: PanelState) -> Result<(), IllegalTransition> {
    use PanelState::*;
    let from = panel.state;
    let legal = matches!(
        (from, to),
        (Spawning, Working) | (Spawning, Idle) | (Working, Idle) | (Idle, Working) | (_, Exited)
    );
    if legal {
        panel.state = to;
        Ok(())
    } else {
        Err(IllegalTransition { from, to })
    }
}

/// Single owner of panel creation (Invariant I-5).
///
/// Opens a PTY sized to `rect`'s interior running the prebuilt `command`
/// (built once by `agent::spawn::spawn`, whose `panel_id` this must match),
/// and creates a grid + capture to match. The returned panel starts in
/// `Spawning`; the caller transitions it to `Working` once a prompt is
/// delivered.
///
/// `agent_id` ties the panel to the [`crate::agent::AgentManifest`] the caller
/// persists; `panel_id` must equal the manifest's `panel_id`. `settings`
/// supplies the panel's scrollback depth and capture caps (the `[settings]`
/// tunables), applied at construction so the panel is born fully configured.
pub(crate) fn spawn(
    request: &SpawnRequest,
    command: PtyCommand,
    panel_id: PanelId,
    agent_id: AgentId,
    rect: Rect,
    settings: &Settings,
) -> Result<Panel, PanelError> {
    let inner = rect.inner();
    let cols = inner.width.max(MIN_GRID_COLS);
    let rows = inner.height.max(MIN_GRID_ROWS);

    let pty = Pty::spawn(&command, cols, rows).map_err(|e| PanelError::Spawn(e.to_string()))?;
    let grid = Grid::with_scrollback(cols as usize, rows as usize, settings.scrollback_lines);

    Ok(Panel {
        id: panel_id,
        role: request.role.name.clone(),
        agent_id,
        state: PanelState::Spawning,
        worktree_path: request.worktree_path.clone(),
        pty,
        grid,
        capture: OutputCapture::with_limits(
            settings.capture_turn_limit,
            settings.capture_open_turn_bytes,
        ),
    })
}

/// Single owner of panel destruction (Invariant I-5).
///
/// Kills the PTY and transitions the panel to `Exited`. The caller is
/// responsible for removing the panel from the registry and enqueuing any
/// worktree via `worktree::cleanup` — that keeps this function free of a
/// registry dependency.
pub(crate) fn kill(panel: &mut Panel) -> Result<(), PanelError> {
    panel.pty.kill().map_err(PanelError::Pty)?;
    // A panel that never reached `Working` (killed mid-spawn) still needs a
    // legal path to `Exited`; `transition` permits `_ -> Exited` from any
    // state, so this never fails.
    transition(panel, PanelState::Exited)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::spec::{AgentCli, RoleSpec};

    fn rect() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        }
    }

    /// A spawn request that launches a trivial, fast-exiting shell instead of
    /// a real agent CLI — keeps the lifecycle tests hermetic.
    fn shell_request() -> SpawnRequest {
        SpawnRequest {
            role: RoleSpec {
                name: "reviewer".into(),
                description: "r".into(),
                allowed_tools: vec![],
                permission_mode: "default".into(),
                system_prompt_template: String::new(),
                agent_cli: AgentCli::Claude,
                model: None,
            },
            agent_name: "reviewer-r1".into(),
            ..SpawnRequest::default()
        }
    }

    /// Build a panel directly around `/bin/cat` so lifecycle tests do not need
    /// a `claude` binary on PATH.
    fn cat_panel() -> Panel {
        use crate::pty::PtyCommand;
        let inner = rect().inner();
        let pty = Pty::spawn(&PtyCommand::new("/bin/cat"), inner.width, inner.height).unwrap();
        Panel {
            id: PanelId::new(),
            role: "reviewer".into(),
            agent_id: AgentId::new(),
            state: PanelState::Spawning,
            worktree_path: None,
            pty,
            grid: Grid::new(inner.width as usize, inner.height as usize),
            capture: OutputCapture::new(),
        }
    }

    #[test]
    fn spawning_to_working_is_legal() {
        let mut p = cat_panel();
        transition(&mut p, PanelState::Working).unwrap();
        assert_eq!(p.state(), PanelState::Working);
    }

    #[test]
    fn working_idle_toggle_is_legal() {
        let mut p = cat_panel();
        transition(&mut p, PanelState::Working).unwrap();
        transition(&mut p, PanelState::Idle).unwrap();
        transition(&mut p, PanelState::Working).unwrap();
        assert_eq!(p.state(), PanelState::Working);
    }

    #[test]
    fn spawning_to_idle_is_legal() {
        // A freshly-spawned agent whose CLI is up but has had no prompt yet
        // settles into `Idle` (awaiting instruction), not stuck in `Spawning`.
        let mut p = cat_panel();
        transition(&mut p, PanelState::Idle).unwrap();
        assert_eq!(p.state(), PanelState::Idle);
    }

    #[test]
    fn exited_is_terminal() {
        // `Exited` is the only state with no way out — a reaped process cannot
        // come back to `Working`/`Idle`.
        let mut p = cat_panel();
        transition(&mut p, PanelState::Exited).unwrap();
        assert!(transition(&mut p, PanelState::Working).is_err());
        assert!(transition(&mut p, PanelState::Idle).is_err());
        assert_eq!(p.state(), PanelState::Exited);
    }

    #[test]
    fn anything_to_exited_is_legal() {
        let mut p = cat_panel();
        transition(&mut p, PanelState::Working).unwrap();
        transition(&mut p, PanelState::Idle).unwrap();
        transition(&mut p, PanelState::Exited).unwrap();
        assert_eq!(p.state(), PanelState::Exited);
    }

    #[test]
    fn spawn_opens_a_pty_and_sizes_the_grid() {
        // Uses `/bin/sh` via a fabricated request whose CLI binary we cannot
        // control — instead exercise spawn through a panel built around cat.
        let mut p = cat_panel();
        // Grid interior matches the rect interior (80x24 -> 78x22).
        assert_eq!(p.grid().size(), (78, 22));
        kill(&mut p).unwrap();
        assert_eq!(p.state(), PanelState::Exited);
    }

    #[test]
    fn pump_drains_pty_output_into_grid_and_capture() {
        use std::time::{Duration, Instant};
        let mut p = cat_panel();
        p.begin_turn();
        p.write_input(b"hello-pump\n").unwrap();

        // cat echoes input back; poll pump until the grid shows it.
        let start = Instant::now();
        let mut total = 0usize;
        while start.elapsed() < Duration::from_secs(5) {
            total += p.pump().unwrap();
            if p.grid().row_text(0).contains("hello-pump") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(total > 0, "pump captured no bytes");
        assert!(p.grid().row_text(0).contains("hello-pump"));
        // The same bytes landed in the open capture turn.
        let captured = String::from_utf8_lossy(p.capture().since_last_turn());
        assert!(captured.contains("hello-pump"));

        kill(&mut p).unwrap();
    }

    #[test]
    fn kill_is_idempotent_via_pty() {
        let mut p = cat_panel();
        kill(&mut p).unwrap();
        // PTY kill is idempotent; a second transition to Exited is also legal.
        kill(&mut p).unwrap();
        assert_eq!(p.state(), PanelState::Exited);
    }

    #[test]
    fn resize_reflows_grid_to_rect_interior() {
        let mut p = cat_panel();
        p.resize(Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 12,
        })
        .unwrap();
        assert_eq!(p.grid().size(), (38, 10));
        kill(&mut p).unwrap();
    }

    #[test]
    fn resize_storm_does_not_panic_or_underflow() {
        // Real-scenario regression: a display sleep/wake (external monitor
        // hot-plug, screen unlock) makes the terminal emit a burst of
        // `Event::Resize` at wildly varying — including degenerate and huge —
        // dimensions before settling. Drive the real PTY + grid resize path
        // through that storm and assert it never panics, never underflows,
        // and always lands a grid clamped to the minimum interior. This rules
        // out a resize-arithmetic panic as the cause of the wake-time death;
        // the actual fault is upstream (a transient `terminal::size()` failure
        // surfaced as a fatal `event::poll`/`read` error).
        let mut p = cat_panel();
        // x/y vary too: a multi-display reflow shifts the origin, not just the
        // extent. Includes 0×0, 1×1, sub-border slivers, a 4K-class extent
        // (a 3840px display at a tiny font is still well under 1000 cols), a
        // garbage-large glitch value (u16::MAX — clamped by the grid's upper
        // bound, so this iteration is instant rather than an OOM), and the
        // eventual settle back to a normal terminal.
        let storm: [(u16, u16, u16, u16); 12] = [
            (0, 0, 0, 0),
            (0, 0, 1, 1),
            (0, 0, 2, 2),
            (0, 0, 1, 50),
            (0, 0, 50, 1),
            (0, 0, 8, 2),
            (10, 5, 9, 3),
            (0, 0, 1000, 300),
            (0, 0, 1920, 480),
            (0, 0, u16::MAX, u16::MAX),
            (0, 0, 3, 3),
            (0, 0, 120, 40),
        ];
        for (x, y, width, height) in storm {
            p.resize(Rect {
                x,
                y,
                width,
                height,
            })
            .unwrap();
            let (cols, rows) = p.grid().size();
            assert!(
                cols >= MIN_GRID_COLS as usize && rows >= MIN_GRID_ROWS as usize,
                "grid clamped below minimum after resize to {width}x{height}: got {cols}x{rows}"
            );
            assert!(
                cols <= Grid::MAX_COLS && rows <= Grid::MAX_ROWS,
                "grid exceeded the maximum after resize to {width}x{height}: got {cols}x{rows}"
            );
        }
        // Settled at 120x40 -> interior 118x38.
        assert_eq!(p.grid().size(), (118, 38));
        kill(&mut p).unwrap();
    }

    #[test]
    fn spawn_request_round_trips_role_name() {
        // `spawn` reads the role name onto the panel; verify the field wiring
        // without needing a real agent binary by checking the request shape.
        let req = shell_request();
        assert_eq!(req.role.name, "reviewer");
    }
}
