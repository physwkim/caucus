//! Full-screen multiplexer TUI runner (`docs/design.md` §0 #2, §1).
//!
//! Owns the crossterm terminal lifecycle and the ratatui event loop:
//!
//! 1. enter raw mode + the alternate screen behind a [`TerminalGuard`] that
//!    restores the terminal on *any* exit path, including a panic;
//! 2. build the [`Multiplexer`], spawn the CEO panel (+ any `--roles`);
//! 3. loop: poll crossterm input → route via `input/`; drain each panel's PTY
//!    via `panel pump`; ingest turn signals; redraw on a ~60 Hz tick;
//! 4. on quit, kill every panel and restore the terminal.
//!
//! Not a tty? [`run`] fails cleanly with a message rather than panicking, so
//! `caucus` is safe to invoke from a non-interactive context.

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect as TuiRect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use tracing::warn;

use crate::config::Config;
use crate::render::{self, Rect};
use crate::session::Multiplexer;
use crate::session::state::Session;

/// Event-loop redraw period — ~60 Hz.
const TICK: Duration = Duration::from_millis(16);

/// RAII guard that restores the terminal: leaves the alternate screen and
/// disables raw mode on drop, so caucus never leaves the user's terminal in
/// a broken state — even if the event loop panics.
struct TerminalGuard;

impl TerminalGuard {
    /// Enter raw mode + the alternate screen.
    fn enter() -> Result<Self> {
        enable_raw_mode().context("enable raw mode")?;
        crossterm::execute!(io::stdout(), EnterAlternateScreen)
            .context("enter alternate screen")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort restore: nothing useful to do if these fail during
        // teardown, and a panic-in-drop would mask the original error.
        let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// Launch the multiplexer TUI in the git repo at `repo`, starting a CEO panel
/// plus one panel per entry of `roles`.
///
/// Fails cleanly (no panic) when stdout is not a terminal.
pub fn run(repo: &std::path::Path, roles: &[String]) -> Result<()> {
    if !std::io::IsTerminal::is_terminal(&io::stdout()) {
        bail!(
            "caucus TUI needs an interactive terminal (stdout is not a tty).\n\
             Run `caucus` from a real terminal, or use a subcommand \
             (`caucus doctor`, `caucus role list`, ...)."
        );
    }

    let config = Config::load(repo).context("load caucus configuration")?;
    let session = Session::new("caucus session", repo.to_path_buf());

    // A multi-thread runtime: the signal server and worktree cleanup queue
    // run as tokio tasks alongside the (blocking) event loop.
    let runtime = tokio::runtime::Runtime::new().context("start tokio runtime")?;
    runtime.block_on(async move { run_loop(config, session, roles).await })
}

/// The async body of [`run`]: terminal setup, panel spawn, the event loop.
async fn run_loop(config: Config, session: Session, roles: &[String]) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stdout())).context("init ratatui terminal")?;
    terminal.clear().ok();

    let area = whole_screen(&terminal)?;
    let (mut mux, mut signal_server) = Multiplexer::new(session, config, area)
        .context("build multiplexer")?;

    // TODO(mcp wave): start the CEO-control MCP server here. The CEO panel
    // drives the other panels through `send_keys` / `spawn_role` / ... over
    // MCP (`docs/design.md` §0 #4). `mcp::McpServer` is a stub until then;
    // wire `McpServer::serve` against the live `Multiplexer` in that wave.

    // The CEO panel always exists (`docs/design.md` §10). Treat the CEO as a
    // role; fall back to `reviewer` if no `ceo` role is configured so the TUI
    // still starts on a stock config.
    let ceo_role = if mux.config.roles.contains("ceo") {
        "ceo"
    } else {
        "reviewer"
    };
    if let Err(err) = mux.spawn_panel(ceo_role, None, None, None) {
        bail!("failed to spawn the CEO panel: {err:#}");
    }
    for role in roles {
        if let Err(err) = mux.spawn_panel(role, None, None, None) {
            warn!(role = %role, error = %format!("{err:#}"), "skipping initial panel");
        }
    }

    let mut last_draw = Instant::now();
    loop {
        // 1. Input — poll without blocking the pump/redraw cadence.
        if event::poll(Duration::from_millis(4)).context("poll terminal events")? {
            match event::read().context("read terminal event")? {
                Event::Key(key) if key.kind == event::KeyEventKind::Press => {
                    mux.handle_key(key);
                }
                Event::Resize(w, h) => {
                    let _ = mux.resize(Rect {
                        x: 0,
                        y: 0,
                        width: w,
                        height: h,
                    });
                }
                _ => {}
            }
        }

        // 2. Turn signals — drain whatever the socket server has queued.
        while let Ok(signal) = signal_server.signals().try_recv() {
            mux.handle_signal(signal);
        }

        // 3. PTY pump — drain every panel into its grid + capture, reap exits.
        mux.pump_all();

        if mux.should_quit() {
            break;
        }

        // 4. Redraw on the tick.
        if last_draw.elapsed() >= TICK {
            draw(&mut terminal, &mux)?;
            last_draw = Instant::now();
        }
    }

    mux.shutdown();
    terminal.show_cursor().ok();
    Ok(())
}

/// Whole-screen [`Rect`] from the terminal's current size.
fn whole_screen(terminal: &Terminal<CrosstermBackend<Stdout>>) -> Result<Rect> {
    let size = terminal.size().context("read terminal size")?;
    Ok(Rect {
        x: 0,
        y: 0,
        width: size.width,
        height: size.height,
    })
}

/// Draw one frame: every panel, plus a one-line status bar at the bottom.
fn draw(terminal: &mut Terminal<CrosstermBackend<Stdout>>, mux: &Multiplexer) -> Result<()> {
    terminal
        .draw(|frame| {
            let full = frame.area();
            // Reserve the last row for the status bar.
            let body = TuiRect {
                height: full.height.saturating_sub(1),
                ..full
            };
            let status_row = TuiRect {
                y: full.y + full.height.saturating_sub(1),
                height: 1,
                ..full
            };

            // Re-tile within the body area (the layout was computed against
            // the full screen; draw clips to the body so the status bar is
            // never overdrawn).
            render::draw(frame, mux.layout(), mux.panels(), mux.focused());

            let status = status_line(mux);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    status,
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                )),
                status_row,
            );
            let _ = body;
        })
        .context("draw frame")?;
    Ok(())
}

/// One-line status bar: panel count, focus, and the keymap hint.
fn status_line(mux: &Multiplexer) -> String {
    let focused = mux
        .focused()
        .and_then(|id| mux.panels().iter().find(|p| p.id == id))
        .map(|p| format!("{} ({})", p.role, p.state_label()))
        .unwrap_or_else(|| "none".into());
    let prefix = if mux.prefix_armed() {
        "  [PREFIX]"
    } else {
        ""
    };
    format!(
        " caucus · {} panel(s) · focus: {} · Ctrl-A then n/p focus, q quit{}",
        mux.panels().len(),
        focused,
        prefix
    )
}
