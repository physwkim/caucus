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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
use crate::role::spec::AgentCli;
use crate::session::Multiplexer;
use crate::session::record::{PanelRecord, SessionRecord};
use crate::session::state::Session;

/// Event-loop redraw period — ~60 Hz.
const TICK: Duration = Duration::from_millis(16);

/// Slept after a terminal-I/O error before retrying, so a persistently failing
/// terminal cannot hot-spin the loop (a failing `poll` returns immediately,
/// with no timeout wait). Terminal I/O errors are *always* treated as transient
/// (see [`event_loop`]), so this backoff is the only thing pacing the retry
/// while a display wake or a monitor/DPI switch has the terminal briefly
/// unavailable — the session itself is never ended by such an error.
const TERMINAL_ERROR_BACKOFF: Duration = Duration::from_millis(10);

/// Spawn a task that flips `terminate` when the controlling terminal hangs up
/// (`SIGHUP` — window closed, parent process exited, SSH dropped). This is the
/// *authoritative* "terminal is gone" signal, and the only thing besides an
/// explicit user quit that ends the event loop.
///
/// A display wake or a monitor/DPI switch — the WezTerm "window jumps back to
/// the external monitor when it powers on" case — delivers only `SIGWINCH`
/// (a resize), never `SIGHUP`. The old give-up heuristic counted the transient
/// `terminal::size()` failures that switch produces (crossterm's `TIOCGWINSZ`
/// has no `EINTR` retry, so a `SIGWINCH` storm makes every `size()` fail) and,
/// because the failures arrive back-to-back with no successful idle poll
/// between them to clear the streak, tripped after ~2.5 s and tore down a live
/// session. Keying shutdown off `SIGHUP` instead means only a genuinely-gone
/// terminal ends the loop, and it still ends through the orderly
/// [`Multiplexer::shutdown`] path (persist record, kill panels, clean
/// worktrees) rather than the kernel's default hard-kill on `SIGHUP`.
#[cfg(unix)]
fn spawn_terminate_listener(terminate: Arc<AtomicBool>) {
    use tokio::signal::unix::{SignalKind, signal};
    tokio::spawn(async move {
        let Ok(mut hup) = signal(SignalKind::hangup()) else {
            return;
        };
        hup.recv().await;
        terminate.store(true, Ordering::SeqCst);
    });
}

/// Non-unix stub: there is no `SIGHUP`, so genuine terminal loss is left to the
/// platform's default behaviour and the loop ends only on explicit quit.
#[cfg(not(unix))]
fn spawn_terminate_listener(_terminate: Arc<AtomicBool>) {}

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
/// worker panel plus one panel per entry of `roles`. `main_cli` selects the
/// main worker's backend (`None` → the default claude main worker).
///
/// Fails cleanly (no panic) when stdout is not a terminal.
pub fn run(
    repo: &std::path::Path,
    roles: &[String],
    main_cli: Option<AgentCli>,
    prefix: char,
) -> Result<()> {
    require_tty()?;
    let config = Config::load(repo).context("load caucus configuration")?;
    let session = Session::new("caucus session", repo.to_path_buf());
    let roles = roles.to_vec();

    // A multi-thread runtime: the signal server and worktree cleanup queue
    // run as tokio tasks alongside the (blocking) event loop.
    let runtime = tokio::runtime::Runtime::new().context("start tokio runtime")?;
    runtime.block_on(async move {
        let _guard = TerminalGuard::enter()?;
        let (terminal, mut mux, signal, control) = setup(config, session, prefix)?;
        spawn_fresh_roster(&mut mux, &roles, main_cli)?;
        event_loop(terminal, mux, signal, control).await
    })
}

/// Launch the multiplexer TUI restoring a previously-persisted session
/// (`caucus resume <id>`). Reads `<repo>/.caucus/sessions/<id>/session.json`,
/// recreates every panel in `order_index` order, and restores the layout
/// mode. Fails cleanly with a message when the record is missing or corrupt.
pub fn run_resumed(
    repo: &std::path::Path,
    session_id: crate::session::SessionId,
    prefix: char,
) -> Result<()> {
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
        let (terminal, mut mux, signal, control) = setup(config, session, prefix)?;
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
    prefix: char,
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
        Multiplexer::new(session, config, area, prefix).context("build multiplexer")?;
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
/// `main_cli` selects the main worker's backend (`None` → claude default).
fn spawn_fresh_roster(
    mux: &mut Multiplexer,
    roles: &[String],
    main_cli: Option<AgentCli>,
) -> Result<()> {
    let role = main_role(mux);
    if let Err(err) = mux.spawn_main_panel(role, &caucus_bin(), main_cli) {
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
/// The panel marked `is_main` is the main worker and resumes through
/// `spawn_main_panel_resume` so its agent reloads the caucus MCP server. Legacy
/// records without an explicit marker fall back to `order_index == 0`. Worktree
/// panels re-attach a worktree on their persisted branch; if the branch is gone
/// the panel spawns fresh (no worktree, no `--resume`). The layout mode is
/// restored last.
fn restore_roster(mux: &mut Multiplexer, record: &SessionRecord) -> Result<()> {
    let bin = caucus_bin();
    let mut panels = record.panels.clone();
    panels.sort_by_key(|p| p.order_index);
    let has_explicit_main = panels.iter().any(|p| p.is_main);
    let mut restored_main = false;

    for panel in &panels {
        let is_main = should_restore_panel_as_main(panel, has_explicit_main, restored_main);
        if is_main {
            restored_main = true;
        }

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
            // Restore the main worker on its persisted backend (codex or claude).
            mux.spawn_main_panel_resume(
                &panel.role,
                &bin,
                panel.claude_session_id.clone(),
                Some(panel.agent_cli),
            )
        } else {
            mux.spawn_panel_resume(
                &panel.role,
                Some(panel.agent_cli),
                panel.model.clone(),
                worktree_path.as_ref().map(|(p, _)| p.clone()),
                worktree_path.as_ref().map(|(_, b)| b.clone()),
                panel.claude_session_id.clone(),
                // A restored panel keeps its preset role's prompt template; the
                // inline-prompt path is live `spawn_role` only.
                None,
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

/// Whether `panel` should be restored as the main worker. New records carry an
/// explicit `is_main` marker; old records did not, so the first ordered panel
/// remains the compatibility fallback. `restored_main` makes corrupted records
/// with multiple markers restore only the first marked panel as main.
fn should_restore_panel_as_main(
    panel: &PanelRecord,
    has_explicit_main: bool,
    restored_main: bool,
) -> bool {
    !restored_main
        && if has_explicit_main {
            panel.is_main
        } else {
            panel.order_index == 0
        }
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
        crate::worktree::manager::role_slug(&panel.role),
        panel.order_index
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
    let terminate = Arc::new(AtomicBool::new(false));
    spawn_terminate_listener(terminate.clone());
    loop {
        // A genuine terminal hangup (window closed, parent gone, SSH dropped)
        // is the only non-quit reason to end the loop. A monitor/DPI switch or
        // display wake never sets this — it only produces the transient errors
        // absorbed below.
        if terminate.load(Ordering::SeqCst) {
            warn!("controlling terminal hung up (SIGHUP); shutting down");
            break;
        }

        // 1. Input — poll without blocking the pump/redraw cadence. A poll/read
        //    error here is always transient and recoverable: a display wake or a
        //    monitor/DPI switch makes crossterm briefly fail `terminal::size()`
        //    inside its SIGWINCH handler (the `TIOCGWINSZ` ioctl has no EINTR
        //    retry, then the `tput` fallback also fails mid-transition), which
        //    surfaces as an `Err` out of `poll`/`read`. Absorb it — warn, back
        //    off, keep the session alive. Only a genuine SIGHUP (checked above)
        //    ends the loop, and then via the orderly shutdown below.
        match event::poll(Duration::from_millis(4)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == event::KeyEventKind::Press => {
                    mux.handle_key(key);
                }
                Ok(Event::Resize(w, h)) => {
                    let _ = mux.resize(body_area(Rect {
                        x: 0,
                        y: 0,
                        width: w,
                        height: h,
                    }));
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "terminal read error; continuing");
                    std::thread::sleep(TERMINAL_ERROR_BACKOFF);
                }
            },
            Ok(false) => {}
            Err(e) => {
                warn!(error = %e, "terminal poll error; continuing");
                std::thread::sleep(TERMINAL_ERROR_BACKOFF);
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

        // 4b. Deferred submits — a bracketed paste's submitting Enter is held
        //     out of the paste burst (the agent swallows a `\r` while it commits
        //     a `[Pasted text #N]` placeholder) and written here as a discrete
        //     keypress once its delay has elapsed, a tick after the paste landed.
        mux.poll_pending_submits();

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

        // 8. Redraw on the tick. A draw failure is treated like a poll/read
        //    error — always transient and recoverable — so a write hiccup during
        //    a display wake or monitor switch does not tear the session down.
        //    The TICK gate already paces this, so no extra backoff is needed.
        if last_draw.elapsed() >= TICK {
            if let Err(e) = draw(&mut terminal, &mux) {
                warn!(error = %e, "terminal draw error; continuing");
            }
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
    // The close-panel confirm is modal and destructive — it supersedes every
    // other status hint until answered.
    if let Some(id) = mux.pending_close() {
        let role = mux
            .panels()
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.role.as_str())
            .unwrap_or("panel");
        return format!(" caucus · close panel '{role}'?   y = close · n/Esc = cancel");
    }
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
    let key = mux.prefix().to_ascii_uppercase();
    format!(
        " caucus · {} panel(s) · focus: {} · layout: {} · \
         Ctrl-{key} then n/p/arrows focus, Ctrl-arrows resize, z zoom, </> move, x close, Space layout, t transcript, [ scroll, q quit{}{}{}",
        mux.panels().len(),
        focused,
        mux.layout_mode().label(),
        zoom,
        transcript,
        prefix
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::record::PanelRecord;

    fn panel_record(order_index: usize, is_main: bool) -> PanelRecord {
        PanelRecord {
            role: format!("role-{order_index}"),
            agent_cli: AgentCli::Claude,
            model: None,
            order_index,
            is_main,
            worktree_branch: None,
            claude_session_id: None,
        }
    }

    #[test]
    fn restore_main_selection_prefers_explicit_marker_over_order() {
        let first = panel_record(0, false);
        let second = panel_record(1, true);
        assert!(!should_restore_panel_as_main(&first, true, false));
        assert!(should_restore_panel_as_main(&second, true, false));
    }

    #[test]
    fn restore_main_selection_falls_back_for_legacy_records() {
        let first = panel_record(0, false);
        let second = panel_record(1, false);
        assert!(should_restore_panel_as_main(&first, false, false));
        assert!(!should_restore_panel_as_main(&second, false, true));
    }
}
