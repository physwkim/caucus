//! Full-screen multiplexer TUI runner (`docs/design.md` §0 #2, §1).
//!
//! Owns the crossterm terminal lifecycle and the ratatui event loop:
//!
//! 1. enter raw mode + the alternate screen behind a `TerminalGuard` that
//!    restores the terminal on *any* exit path, including a panic;
//! 2. build the [`Multiplexer`], spawn the main worker panel (+ any `--roles`);
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
use crate::session::record::SessionRecord;
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

/// Launch the multiplexer TUI in the git repo at `repo`, starting a main
/// worker panel plus one panel per entry of `roles`.
///
/// Fails cleanly (no panic) when stdout is not a terminal.
pub fn run(repo: &std::path::Path, roles: &[String]) -> Result<()> {
    require_tty()?;
    let config = Config::load(repo).context("load caucus configuration")?;
    let session = Session::new("caucus session", repo.to_path_buf());
    let roles = roles.to_vec();

    // A multi-thread runtime: the signal server and worktree cleanup queue
    // run as tokio tasks alongside the (blocking) event loop.
    let runtime = tokio::runtime::Runtime::new().context("start tokio runtime")?;
    runtime.block_on(async move {
        let _guard = TerminalGuard::enter()?;
        let (terminal, mut mux, signal, control) = setup(config, session)?;
        spawn_fresh_roster(&mut mux, &roles)?;
        event_loop(terminal, mux, signal, control).await
    })
}

/// Launch the multiplexer TUI restoring a previously-persisted session
/// (`caucus resume <id>`). Reads `<repo>/.caucus/sessions/<id>/session.json`,
/// recreates every panel in `order_index` order, and restores the layout
/// mode. Fails cleanly with a message when the record is missing or corrupt.
pub fn run_resumed(repo: &std::path::Path, session_id: crate::session::SessionId) -> Result<()> {
    // Resolve the record first — a missing/corrupt `session.json` fails with a
    // pointed message regardless of whether stdout is a tty.
    let record = SessionRecord::read_for_id(repo, session_id).with_context(|| {
        format!(
            "no resumable session '{session_id}' \
             (expected .caucus/sessions/{session_id}/session.json) — \
             run `caucus sessions` to list resumable sessions"
        )
    })?;
    require_tty()?;
    let config = Config::load(repo).context("load caucus configuration")?;
    let session = Session::from_record(&record);

    let runtime = tokio::runtime::Runtime::new().context("start tokio runtime")?;
    runtime.block_on(async move {
        let _guard = TerminalGuard::enter()?;
        let (terminal, mut mux, signal, control) = setup(config, session)?;
        restore_roster(&mut mux, &record)?;
        event_loop(terminal, mux, signal, control).await
    })
}

/// Bail cleanly when stdout is not an interactive terminal.
fn require_tty() -> Result<()> {
    if !std::io::IsTerminal::is_terminal(&io::stdout()) {
        bail!(
            "caucus TUI needs an interactive terminal (stdout is not a tty).\n\
             Run `caucus` from a real terminal, or use a subcommand \
             (`caucus doctor`, `caucus role list`, ...)."
        );
    }
    Ok(())
}

/// The resolved type of the ratatui terminal the event loop drives.
type Term = Terminal<CrosstermBackend<Stdout>>;

/// Build the ratatui terminal and the [`Multiplexer`] (with its socket
/// servers). Shared by the fresh-launch and resume paths.
fn setup(
    config: Config,
    session: Session,
) -> Result<(
    Term,
    Multiplexer,
    crate::signal::server::SignalServer,
    crate::mcp::control_server::ControlServer,
)> {
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stdout())).context("init ratatui terminal")?;
    terminal.clear().ok();
    // Panels tile the *body* — the whole screen minus the one-row status bar.
    let area = body_area(whole_screen(&terminal)?);
    let (mux, signal_server, control_server) =
        Multiplexer::new(session, config, area).context("build multiplexer")?;
    Ok((terminal, mux, signal_server, control_server))
}

/// The `main` role for the main worker panel, falling back to `reviewer` if a
/// config override removed it (`docs/design.md` §10).
fn main_role(mux: &Multiplexer) -> &'static str {
    if mux.config.roles.contains("main") {
        "main"
    } else {
        warn!("no `main` role configured — falling back to `reviewer` for the main worker panel");
        "reviewer"
    }
}

/// Absolute path of the running `caucus` binary — so the `mcp-serve` child is
/// the exact same build.
fn caucus_bin() -> std::path::PathBuf {
    std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("caucus"))
}

/// Spawn a fresh roster: the main worker panel plus one panel per `roles`.
fn spawn_fresh_roster(mux: &mut Multiplexer, roles: &[String]) -> Result<()> {
    let role = main_role(mux);
    if let Err(err) = mux.spawn_main_panel(role, &caucus_bin()) {
        bail!("failed to spawn the main worker panel: {err:#}");
    }
    for role in roles {
        if let Err(err) = mux.spawn_panel(role, None, None, None) {
            warn!(role = %role, error = %format!("{err:#}"), "skipping initial panel");
        }
    }
    Ok(())
}

/// Recreate every panel of a persisted session in `order_index` order.
///
/// The `order_index == 0` panel is the main worker (always spawned first on a
/// fresh launch); it resumes through `spawn_main_panel_resume` so its claude
/// reloads the caucus MCP server. Worktree panels re-attach a worktree on
/// their persisted branch; if the branch is gone the panel spawns fresh
/// (no worktree, no `--resume`). The layout mode is restored last.
fn restore_roster(mux: &mut Multiplexer, record: &SessionRecord) -> Result<()> {
    let bin = caucus_bin();
    let mut panels = record.panels.clone();
    panels.sort_by_key(|p| p.order_index);

    for panel in &panels {
        let is_main = panel.order_index == 0;

        // A worktree-backed panel: re-attach a worktree on its persisted
        // branch. The directory was removed on the prior shutdown; the branch
        // (with the agent's commits) persisted.
        let worktree_path = match &panel.worktree_branch {
            Some(branch) => {
                let path = resume_worktree_path(&record.repo_path, record.id, panel);
                match crate::worktree::manager::attach(&record.repo_path, &path, branch) {
                    Ok(handle) => Some((handle.path, branch.clone())),
                    Err(err) => {
                        // Branch gone (or path collision): spawn fresh rather
                        // than abort the whole resume.
                        warn!(
                            role = %panel.role, branch = %branch, error = %format!("{err}"),
                            "worktree branch unavailable on resume — spawning panel without it"
                        );
                        None
                    }
                }
            }
            None => None,
        };

        let result = if is_main {
            mux.spawn_main_panel_resume(&panel.role, &bin, panel.claude_session_id.clone())
        } else {
            mux.spawn_panel_resume(
                &panel.role,
                Some(panel.agent_cli),
                panel.model.clone(),
                worktree_path.as_ref().map(|(p, _)| p.clone()),
                worktree_path.as_ref().map(|(_, b)| b.clone()),
                panel.claude_session_id.clone(),
            )
        };
        if let Err(err) = result {
            warn!(
                role = %panel.role, error = %format!("{err:#}"),
                "skipping panel on resume"
            );
        }
    }

    // Restore the panel arrangement, then persist the rebuilt roster.
    mux.set_layout_mode(record.layout_mode);
    mux.persist_record();
    Ok(())
}

/// Worktree directory for a resumed panel under `<repo>/.caucus/worktrees/`.
/// A fresh, collision-safe leaf — the prior directory was cleaned on shutdown.
fn resume_worktree_path(
    repo: &std::path::Path,
    session_id: crate::session::SessionId,
    panel: &crate::session::record::PanelRecord,
) -> std::path::PathBuf {
    let id = session_id.to_string();
    let suffix: String = id
        .chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    repo.join(".caucus").join("worktrees").join(format!(
        "{suffix}-{}-resume{}",
        panel.role, panel.order_index
    ))
}

/// The shared event loop: input → signals → control → pump → redraw, until
/// quit. `mux` is consumed; on exit every panel is killed and the terminal is
/// restored by the [`TerminalGuard`] held by the caller.
async fn event_loop(
    mut terminal: Term,
    mut mux: Multiplexer,
    mut signal_server: crate::signal::server::SignalServer,
    mut control_server: crate::mcp::control_server::ControlServer,
) -> Result<()> {
    let mut last_draw = Instant::now();
    loop {
        // 1. Input — poll without blocking the pump/redraw cadence.
        if event::poll(Duration::from_millis(4)).context("poll terminal events")? {
            match event::read().context("read terminal event")? {
                Event::Key(key) if key.kind == event::KeyEventKind::Press => {
                    mux.handle_key(key);
                }
                Event::Resize(w, h) => {
                    let _ = mux.resize(body_area(Rect {
                        x: 0,
                        y: 0,
                        width: w,
                        height: h,
                    }));
                }
                _ => {}
            }
        }

        // 2. Turn signals — drain whatever the socket server has queued.
        while let Ok(signal) = signal_server.signals().try_recv() {
            mux.handle_signal(signal);
        }

        // 3. Control jobs — execute the main worker's queued MCP tool calls against
        //    live panels on this same thread (Invariant I-5).
        mux.drain_control(&mut control_server);

        // 4. PTY pump — drain every panel into its grid + capture, reap exits.
        mux.pump_all();

        // 5. Selection prompts — if a panel in a pending round has stopped on
        //    an interactive chooser (no Stop hook fires, so its round never
        //    settles), announce it to the main worker so it can answer and let
        //    the round finish. Runs before round delivery: unblocking a stuck
        //    panel takes precedence, and both share the one-push-per-tick gate.
        mux.poll_round_selection_prompts();

        // 6. Round delivery — if a registered round's panels have now settled
        //    (or its fallback deadline passed), assemble their results and
        //    inject them into the main worker's panel (the caucus→main push).
        mux.poll_pending_rounds();

        // 7. Stranded-main guard — if the main worker went idle with no round
        //    registered while sub-panels still run, nothing above can ever
        //    re-prompt it (both pushes need a round). Nudge it. Runs last: it
        //    only fires when `pending_rounds` is empty, so it never competes
        //    with steps 5–6 for the one-push-per-tick gate.
        mux.poll_stranded_main();

        if mux.should_quit() {
            break;
        }

        // 8. Redraw on the tick.
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

/// The panel-tiling area: the whole screen minus the one-row status bar at
/// the bottom. Saturating, so a zero-height terminal yields a zero-height
/// body rather than underflowing.
fn body_area(screen: Rect) -> Rect {
    Rect {
        height: screen.height.saturating_sub(1),
        ..screen
    }
}

/// Draw one frame: every panel, plus a one-line status bar at the bottom.
fn draw(terminal: &mut Terminal<CrosstermBackend<Stdout>>, mux: &Multiplexer) -> Result<()> {
    terminal
        .draw(|frame| {
            let full = frame.area();
            // The last row is the status bar; the multiplexer's layout was
            // already reflowed against the body (whole screen minus this
            // row), so panels tile the body and never overdraw the status.
            let status_row = TuiRect {
                y: full.y + full.height.saturating_sub(1),
                height: 1,
                ..full
            };

            render::draw(frame, mux.layout(), mux.panels(), mux.focused());

            // The transcript overlay paints on top of the panels — draw-time
            // only; the panels keep pumping and input keeps routing.
            if mux.show_transcript() {
                render::draw_transcript(frame, mux.panels(), mux.manifests(), mux.focused());
            }

            // The scrollback pager (`Ctrl-A [`) is modal and supersedes the
            // transcript overlay — drawn last so it sits on top. Panels keep
            // pumping underneath; the pager shows a frozen snapshot.
            if let Some(scroll) = mux.scroll_state() {
                render::draw_scroll_pager(frame, scroll);
            }

            let status = status_line(mux);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    status,
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                )),
                status_row,
            );
        })
        .context("draw frame")?;
    Ok(())
}

/// One-line status bar: panel count, focus, layout mode, and the keymap hint.
fn status_line(mux: &Multiplexer) -> String {
    let focused = mux
        .focused()
        .and_then(|id| mux.panels().iter().find(|p| p.id == id))
        .map(|p| format!("{} ({})", p.role, p.state_label()))
        .unwrap_or_else(|| "none".into());
    let prefix = if mux.prefix_armed() { "  [PREFIX]" } else { "" };
    let zoom = if mux.zoomed().is_some() {
        "  [ZOOM]"
    } else {
        ""
    };
    let transcript = if mux.show_transcript() {
        "  [TRANSCRIPT]"
    } else {
        ""
    };
    // While the pager is open it is modal and captures input, so show its own
    // key hints rather than the live keymap.
    if mux.scroll_state().is_some() {
        return " caucus · scrollback · ↑↓ k/j line · PgUp/PgDn page · g/G top/bottom · Esc/q exit"
            .to_string();
    }
    format!(
        " caucus · {} panel(s) · focus: {} · layout: {} · \
         Ctrl-A then n/p focus, z zoom, </> move, Space layout, t transcript, [ scroll, q quit{}{}{}",
        mux.panels().len(),
        focused,
        mux.layout_mode().label(),
        zoom,
        transcript,
        prefix
    )
}
