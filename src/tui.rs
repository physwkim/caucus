//! Full-screen multiplexer TUI runner (`docs/design.md` §0 #2, §1).
//!
//! Owns the crossterm terminal lifecycle and the ratatui event loop:
//!
//! 1. enter raw mode + the alternate screen behind a `TerminalGuard` that
//!    restores the terminal on *any* exit path, including a panic;
//! 2. build the [`Multiplexer`], spawn the main worker panel (+ any `--roles`);
//! 3. loop: receive crossterm input from the stdin reader thread → route via
//!    `input/`; drain each panel's PTY via `panel pump`; ingest turn signals;
//!    redraw on a ~60 Hz tick;
//! 4. on quit, kill every panel and restore the terminal.
//!
//! Not a tty? [`run`] fails cleanly with a message rather than panicking, so
//! `caucus` is safe to invoke from a non-interactive context.

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
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
use crate::panel::PanelState;
use crate::render::{self, Rect};
use crate::role::spec::AgentCli;
use crate::session::record::{PanelRecord, SessionRecord};
use crate::session::state::Session;
use crate::session::{LaunchMode, Multiplexer};

/// Event-loop redraw period — ~60 Hz. Paces the *fastest* redraw and the
/// render-signature check; within a tick a frame is painted only when the
/// signature changed (see [`Multiplexer::render_signature`]).
const TICK: Duration = Duration::from_millis(16);

/// Safety-net repaint interval for the dirty-gated draw. Even when the render
/// signature is unchanged, the screen is repainted at least this often, so any
/// draw input not modelled by the signature (e.g. a no-op-interior resize)
/// degrades to at most this much staleness rather than a frozen frame.
const FORCED_REDRAW_INTERVAL: Duration = Duration::from_secs(1);

/// Slept by the stdin reader thread after a terminal-I/O error before
/// retrying, so a persistently failing terminal cannot hot-spin that thread
/// (a failing `read` returns immediately). Terminal I/O errors are *always*
/// treated as transient (see [`spawn_input_reader`]), so this backoff is the
/// only thing pacing the retry while a display wake or a monitor/DPI switch
/// has the terminal briefly unavailable — the session itself is never ended
/// by such an error.
const TERMINAL_ERROR_BACKOFF: Duration = Duration::from_millis(10);

/// How long the controlling terminal must stay unreachable after a `SIGHUP`
/// before the session is torn down. A genuine hangup never recovers, so the
/// window only delays a real shutdown by ~1 s (the window is closed — nobody is
/// watching); a spurious `SIGHUP` recovers on the very first probe and never
/// reaches this. See [`spawn_hangup_listener`] / [`controlling_terminal_alive`].
const HANGUP_CONFIRM_WINDOW: Duration = Duration::from_millis(1000);

/// Spawn a task that sets `hangup` on every `SIGHUP` (window closed, parent
/// process exited, SSH dropped — *or* a terminal emulator that delivers a
/// `SIGHUP` across a macOS display wake while its window is still open).
///
/// `SIGHUP` is treated as a *trigger to verify*, not an authoritative verdict.
/// The earlier design keyed shutdown directly off `SIGHUP` on the premise that
/// a display wake delivers only `SIGWINCH`, never `SIGHUP`. That premise does
/// not hold for WezTerm on macOS: it fires a real `SIGHUP` when the display
/// powers back on, and trusting it tore down a live session (clean exit to the
/// shell) — the same false-positive class as the old error-count give-up
/// heuristic, just via a different signal. The loop instead confirms the
/// terminal is genuinely gone (see [`controlling_terminal_alive`]) before
/// ending through the orderly [`Multiplexer::shutdown`] path.
///
/// The flag is set (not consumed) here and `swap`-cleared by the loop, so
/// several coalesced `SIGHUP`s collapse into one verification pass and each
/// later wake re-arms it.
#[cfg(unix)]
fn spawn_hangup_listener(hangup: Arc<AtomicBool>) {
    use tokio::signal::unix::{SignalKind, signal};
    tokio::spawn(async move {
        let Ok(mut hup) = signal(SignalKind::hangup()) else {
            return;
        };
        while hup.recv().await.is_some() {
            hangup.store(true, Ordering::SeqCst);
        }
    });
}

/// Non-unix stub: there is no `SIGHUP`, so the hangup path never arms.
#[cfg(not(unix))]
fn spawn_hangup_listener(_hangup: Arc<AtomicBool>) {}

/// Whether this process still has a controlling terminal.
///
/// `open("/dev/tty")` succeeds iff the process has a controlling terminal; a
/// genuine hangup (window closed, parent exited, SSH dropped) makes the kernel
/// revoke it, after which the open fails (`ENXIO`). A spurious `SIGHUP` —
/// WezTerm delivering one across a macOS display wake while the window is still
/// open — leaves `/dev/tty` openable, so this cleanly tells the two apart.
/// `EINTR` from a concurrent `SIGWINCH` (the same display event also resizes the
/// window) is retried, so a signal storm is never mistaken for terminal loss.
/// The handle is dropped immediately; the probe neither reads nor writes, and
/// `/dev/tty` only ever refers to the *existing* controlling terminal, so it
/// cannot acquire or change one.
#[cfg(unix)]
fn controlling_terminal_alive() -> bool {
    use std::io::ErrorKind;
    for _ in 0..16 {
        match std::fs::OpenOptions::new().read(true).open("/dev/tty") {
            Ok(_) => return true,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    // Persistent `EINTR` means signals are storming, not that the terminal is
    // gone — treat as alive and let a later probe settle it.
    true
}

/// Non-unix stub: the hangup path never runs, so liveness is moot.
#[cfg(not(unix))]
fn controlling_terminal_alive() -> bool {
    true
}

/// What the event loop should do about a pending `SIGHUP`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HangupAction {
    /// Terminal is still live — the `SIGHUP` was spurious; clear the suspicion
    /// and keep the session running.
    Survive,
    /// Terminal has been unreachable for the whole confirm window — a genuine
    /// hangup; end the session through the orderly shutdown path.
    Confirm,
    /// Terminal is currently unreachable but still within the window — keep
    /// probing on the next tick.
    Wait,
}

/// Classify a pending hangup. A live terminal always wins (`Survive`) so a
/// spurious `SIGHUP` can never end the session; a terminal that stays
/// unreachable for `window` is a genuine hangup (`Confirm`). Pure so the
/// boundary behaviour is unit-tested without a real terminal.
fn hangup_verdict(
    terminal_alive: bool,
    suspect_elapsed: Duration,
    window: Duration,
) -> HangupAction {
    if terminal_alive {
        HangupAction::Survive
    } else if suspect_elapsed >= window {
        HangupAction::Confirm
    } else {
        HangupAction::Wait
    }
}

/// Spawn the dedicated stdin reader thread and return its event channel.
///
/// The event loop must never touch terminal input directly: crossterm treats
/// a stdin EOF (the pty master closed under us — e.g. `tmux kill-server`) as
/// "no data yet" and busy-loops *inside* `event::poll` without ever
/// returning, which wedges whatever thread called it at 100% CPU. Confining
/// the read to its own thread makes the loop's pacing the loop's own —
/// whatever state the terminal is in, ticks keep running, so the
/// hangup/orphan checks can still end the session in bounded time (the
/// wedged reader thread dies with the process).
///
/// The thread forwards `event::read()` results as-is; on a read `Err` it
/// backs off [`TERMINAL_ERROR_BACKOFF`] (transient wake/DPI-switch glitches,
/// absorbed exactly as the loop always has) and keeps reading. It exits only
/// when the receiver is gone. If the thread cannot be spawned at all, the
/// returned channel reads as disconnected and the loop shuts down orderly.
fn spawn_input_reader() -> mpsc::Receiver<std::io::Result<Event>> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("stdin-events".into())
        .spawn(move || {
            loop {
                let result = event::read();
                let errored = result.is_err();
                if tx.send(result).is_err() {
                    return;
                }
                if errored {
                    std::thread::sleep(TERMINAL_ERROR_BACKOFF);
                }
            }
        });
    if let Err(e) = spawned {
        warn!(error = %e, "failed to spawn the stdin reader thread");
    }
    rx
}

/// Current parent pid; the non-unix stub never reads as orphaned.
#[cfg(unix)]
fn parent_pid() -> u32 {
    std::os::unix::process::parent_id()
}
#[cfg(not(unix))]
fn parent_pid() -> u32 {
    u32::MAX
}

/// Whether the hosting process is gone: caucus started under a live parent
/// (shell, tmux server, terminal emulator) and has since been reparented to
/// init. This is the death signal the `SIGHUP` machinery cannot see when
/// caucus is its pty's *session leader* — tmux runs each pane command as the
/// session leader of the pane's pty, and a pty is only revoked when its
/// session leader exits, so after `tmux kill-server` the `/dev/tty` probe
/// keeps succeeding forever while crossterm silently busy-loops on the
/// stdin EOF (100% CPU, no error surfaced, panels left running headless).
/// Reparenting is decisive the other way too: a *detached* tmux keeps its
/// panes parented to the live server, so detach never reads as orphaned.
/// A process legitimately started under init (`initial_ppid == 1`) can never
/// trip this. (On Linux a subreaper may adopt orphans instead of init; there
/// this check simply never fires, which is the pre-existing behaviour.)
fn orphaned(initial_ppid: u32, current_ppid: u32) -> bool {
    current_ppid == 1 && initial_ppid != 1
}

/// Install file logging at `<repo>/.caucus/caucus.log` plus a panic hook that
/// records panics there too. This is the *only* place caucus installs a tracing
/// subscriber — without it every `warn!`/`error!` in the event loop (terminal
/// I/O errors, `SIGHUP` classification) goes nowhere, and a panic raised inside
/// the alternate screen is lost to the cleared scrollback. Idempotent and
/// best-effort: a second call or any setup failure is a silent no-op and never
/// blocks the TUI from starting. `RUST_LOG` overrides the default filter.
fn init_logging(repo: &std::path::Path) {
    use tracing_subscriber::{EnvFilter, fmt};
    let dir = repo.join(".caucus");
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("caucus.log"))
    else {
        return;
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("caucus=info"));
    let installed = fmt()
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .with_env_filter(filter)
        .try_init()
        .is_ok();
    if installed {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let backtrace = std::backtrace::Backtrace::force_capture();
            tracing::error!(panic = %info, %backtrace, "caucus panicked");
            default(info);
        }));
    }
}

/// RAII guard that restores the terminal: leaves the alternate screen and
/// disables raw mode on drop, so caucus never leaves the user's terminal in
/// a broken state — even if the event loop panics.
struct TerminalGuard {
    /// Whether this guard enabled mouse capture, so `Drop` disables exactly what
    /// `enter` enabled — never emitting a `DisableMouseCapture` that the host
    /// never saw enabled.
    mouse: bool,
}

impl TerminalGuard {
    /// Enter raw mode + the alternate screen, and enable bracketed paste so the
    /// host terminal hands a paste to caucus as one [`Event::Paste`] burst
    /// rather than streaming it key-by-key (which would submit at every embedded
    /// newline — see [`Multiplexer::handle_paste`]).
    ///
    /// When `mouse` is set (`[settings] mouse = true`; off by default), also
    /// capture the mouse so the scroll wheel reaches caucus as `PageUp`/`PageDown`
    /// ([`Multiplexer::handle_mouse`]). Capture suppresses the terminal's native
    /// drag-to-select, so the default leaves it off to keep native copy working.
    fn enter(mouse: bool) -> Result<Self> {
        enable_raw_mode().context("enable raw mode")?;
        crossterm::execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)
            .context("enter alternate screen")?;
        if mouse {
            crossterm::execute!(io::stdout(), EnableMouseCapture)
                .context("enable mouse capture")?;
        }
        Ok(Self { mouse })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort restore in reverse order of `enter`: nothing useful to do
        // if these fail during teardown, and a panic-in-drop would mask the
        // original error.
        if self.mouse {
            let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
        }
        let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// Launch the multiplexer TUI in the git repo at `repo`, starting a main
/// worker panel plus one panel per entry of `roles`. `main_cli` selects the
/// main worker's backend (`None` → the default claude main worker). `topic` is
/// the session's human label for `caucus sessions`; `None` falls back to
/// `default_topic`.
///
/// Fails cleanly (no panic) when stdout is not a terminal.
pub fn run(
    repo: &std::path::Path,
    roles: &[String],
    main_cli: Option<AgentCli>,
    prefix: Option<char>,
    topic: Option<String>,
) -> Result<()> {
    init_logging(repo);
    require_tty()?;
    let config = Config::load(repo).context("load caucus configuration")?;
    // Explicit --prefix/CAUCUS_PREFIX > `[settings] prefix` > the tmux-aware
    // default. After `init_logging` so a dodge lands in caucus.log.
    let prefix = crate::input::effective_prefix(prefix, config.settings.prefix);
    let topic = topic.unwrap_or_else(|| default_topic(repo));
    let session = Session::new(topic, repo.to_path_buf());
    let roles = roles.to_vec();

    // A multi-thread runtime: the signal server and worktree cleanup queue
    // run as tokio tasks alongside the (blocking) event loop.
    let runtime = tokio::runtime::Runtime::new().context("start tokio runtime")?;
    runtime.block_on(async move {
        let _guard = TerminalGuard::enter(config.settings.mouse)?;
        let (terminal, mut mux, signal, control) =
            setup(config, session, prefix, LaunchMode::Fresh)?;
        spawn_fresh_roster(&mut mux, &roles, main_cli)?;
        event_loop(terminal, mux, signal, control).await
    })
}

/// The session label used when `--topic` is omitted: the repository's directory
/// name, so `caucus sessions` distinguishes projects instead of listing every
/// session as a generic "caucus session". Falls back to that generic label only
/// when the repo path has no final component (e.g. the filesystem root).
fn default_topic(repo: &std::path::Path) -> String {
    repo.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| "caucus session".to_string())
}

/// Launch the multiplexer TUI restoring a previously-persisted session
/// (`caucus resume <id>`). Reads `<repo>/.caucus/sessions/<id>/session.json`,
/// recreates every panel in `order_index` order, and restores the layout
/// mode. Fails cleanly with a message when the record is missing or corrupt.
pub fn run_resumed(
    repo: &std::path::Path,
    session_id: crate::session::SessionId,
    prefix: Option<char>,
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
    init_logging(repo);
    require_tty()?;
    let config = Config::load(repo).context("load caucus configuration")?;
    // Same resolution chain as a fresh launch (`tui::run`).
    let prefix = crate::input::effective_prefix(prefix, config.settings.prefix);
    let session = Session::from_record(&record);

    let runtime = tokio::runtime::Runtime::new().context("start tokio runtime")?;
    runtime.block_on(async move {
        let _guard = TerminalGuard::enter(config.settings.mouse)?;
        let (terminal, mut mux, signal, control) =
            setup(config, session, prefix, LaunchMode::Resume)?;
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
    mode: LaunchMode,
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
        Multiplexer::new(session, config, area, prefix, mode).context("build multiplexer")?;
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
        // branch. The directory was removed on a clean shutdown; the branch
        // (with the agent's commits) persisted. A *crash* leaves the prior
        // directory and its git registration in place, so reconcile any stale
        // caucus-owned checkout of the branch before attaching.
        //
        // A worktree-marked panel MUST resume inside a worktree — never in the
        // repo root with full Edit/Bash, which would silently strip the
        // isolation the user is relying on. So if attach fails (branch gone /
        // unrecoverable), create a fresh isolated worktree instead; only if
        // even that fails is the panel skipped, never run un-isolated.
        let worktree_path = match &panel.worktree_branch {
            Some(branch) => {
                let path = resume_worktree_path(&record.repo_path, record.id, panel);
                crate::worktree::manager::reconcile_stale(&record.repo_path, branch);
                match crate::worktree::manager::attach(&record.repo_path, &path, branch) {
                    Ok(handle) => Some((handle.path, handle.branch)),
                    Err(err) => {
                        warn!(
                            role = %panel.role, branch = %branch, error = %format!("{err}"),
                            "worktree branch unrecoverable on resume — creating a fresh isolated worktree"
                        );
                        match mux.create_role_worktree(&panel.role) {
                            Ok(h) => Some((h.path, h.branch)),
                            Err(e2) => {
                                warn!(
                                    role = %panel.role, error = %format!("{e2}"),
                                    "could not create a fresh worktree — skipping this panel to preserve isolation"
                                );
                                continue;
                            }
                        }
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

    // Recounting the roster above only counts surviving panels, so the per-role
    // spawn counter under-counts when a panel was killed mid-session. Raise it
    // back to the persisted high-water mark before any further `spawn_role`, so
    // the next `<role>-N` worktree branch index cannot collide with a surviving
    // branch (and the re-persisted record below carries the true high-water).
    mux.seed_role_counts_floor(&record.role_counts);

    // Restore the panel arrangement, then persist the rebuilt roster.
    mux.set_layout_mode(record.layout_mode);
    mux.persist_record();
    // Surface any rounds the prior instance left in flight — preserve their
    // captured work and queue a notice for the resumed main worker so it does
    // not wait forever for a delivery that cannot cross the restart.
    mux.ingest_resumed_rounds();
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
    // Dirty-gated redraw bookkeeping: `last_sig` is the render signature of the
    // last painted frame (`None` forces the first paint), and `last_forced_draw`
    // is the safety-net timer that repaints periodically even when the signature
    // is unchanged, so any unmodelled draw input degrades to bounded staleness.
    let mut last_sig: Option<u64> = None;
    let mut last_forced_draw = Instant::now();
    // `SIGHUP` is a *trigger to verify*, not a verdict. A genuine hangup (window
    // closed, parent gone, SSH dropped) revokes the controlling terminal; a
    // spurious one (WezTerm delivering `SIGHUP` across a macOS display wake while
    // the window is still open) leaves it attached. The loop probes the terminal
    // to tell them apart and ends the session only when it is confirmed gone for
    // `HANGUP_CONFIRM_WINDOW` — so a display wake can no longer tear down a live
    // session, while a real hangup still ends through the orderly shutdown path.
    let hangup = Arc::new(AtomicBool::new(false));
    spawn_hangup_listener(hangup.clone());
    let mut hangup_suspect_since: Option<Instant> = None;
    // Stdin events come through a dedicated reader thread so terminal I/O
    // can never wedge this loop (see [`spawn_input_reader`]).
    let input_rx = spawn_input_reader();
    // Orphan watch: the hosting-process-death signal the `SIGHUP` machinery
    // cannot see when caucus is its pty's session leader (see [`orphaned`]).
    let initial_ppid = parent_pid();
    let mut orphaned_since: Option<Instant> = None;
    loop {
        // Hosting process gone (tmux server killed, terminal emulator died):
        // confirm over the same window as a hangup, then end the session
        // through the orderly shutdown path so panels are not left running
        // headless.
        if orphaned(initial_ppid, parent_pid()) {
            let since = orphaned_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= HANGUP_CONFIRM_WINDOW {
                warn!(
                    initial_ppid,
                    ?HANGUP_CONFIRM_WINDOW,
                    "hosting process gone (reparented to init); shutting down"
                );
                break;
            }
        } else {
            orphaned_since = None;
        }

        // Begin — or restart — confirming a hangup whenever a `SIGHUP` arrives.
        if hangup.swap(false, Ordering::SeqCst) {
            hangup_suspect_since.get_or_insert_with(Instant::now);
        }
        if let Some(since) = hangup_suspect_since {
            match hangup_verdict(
                controlling_terminal_alive(),
                since.elapsed(),
                HANGUP_CONFIRM_WINDOW,
            ) {
                HangupAction::Survive => {
                    warn!(
                        "SIGHUP received but controlling terminal is still live; \
                         ignoring as a spurious hangup (e.g. a display wake)"
                    );
                    hangup_suspect_since = None;
                }
                HangupAction::Confirm => {
                    warn!(
                        ?HANGUP_CONFIRM_WINDOW,
                        "controlling terminal confirmed gone (SIGHUP, /dev/tty \
                         unopenable for the confirm window); shutting down"
                    );
                    break;
                }
                // Within the confirm window and currently unreachable: keep
                // looping and re-probe next tick.
                HangupAction::Wait => {}
            }
        }

        // 1. Input — received through the reader thread's channel, never by
        //    touching the terminal from this loop (see [`spawn_input_reader`]:
        //    crossterm can wedge inside `poll` on a dead stdin, and pacing is
        //    this loop's own job). The timeout is the old poll window, so the
        //    pump/redraw cadence is unchanged. A read error is transient and
        //    recoverable (display wakes / monitor switches make crossterm
        //    briefly fail inside its SIGWINCH handler): warn and keep the
        //    session alive — the reader thread already backed off. Only a
        //    confirmed hangup or orphaning (checked above) ends the loop.
        match input_rx.recv_timeout(Duration::from_millis(4)) {
            Ok(Ok(Event::Key(key))) if key.kind == event::KeyEventKind::Press => {
                mux.handle_key(key);
            }
            Ok(Ok(Event::Paste(text))) => {
                // A host-side bracketed paste: deliver it to the focused
                // panel as one paste burst instead of letting it stream
                // key-by-key and submit at every newline.
                mux.handle_paste(&text);
            }
            Ok(Ok(Event::Mouse(mouse))) => {
                // Only delivered when mouse capture is on (`[settings]
                // mouse`). A wheel notch is routed as a `PageUp`/`PageDown`
                // keypress — no mapping of its own.
                mux.handle_mouse(mouse);
            }
            Ok(Ok(Event::Resize(w, h))) => {
                let _ = mux.resize(body_area(Rect {
                    x: 0,
                    y: 0,
                    width: w,
                    height: h,
                }));
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                warn!(error = %e, "terminal read error; continuing");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The reader thread only ends when it cannot deliver (this
                // receiver gone) or never started; either way stdin is lost
                // for good — end the session through the orderly shutdown.
                warn!("stdin reader gone; shutting down");
                break;
            }
        }

        // 1b. Clipboard — a copy-mode yank (`v` then `y` in the pager) queued an
        //     OSC 52 set-clipboard sequence; emit it to the host terminal so the
        //     selection lands on the system clipboard. Best-effort: terminals
        //     without OSC 52 support silently drop it, and a write error here is
        //     non-fatal (the session keeps running).
        if let Some(seq) = mux.take_pending_clipboard()
            && let Err(e) = write_clipboard_osc(&seq)
        {
            warn!(error = %e, "clipboard OSC 52 write failed");
        }

        // 2. Signal events — drain whatever the socket server has queued: turn
        //    signals settle panels (a main-panel signal's reply slot may carry
        //    a due round back through the waiting Stop hook), mid-turn notes
        //    are recorded without any state transition. A note's reply slot is
        //    always `None`; dropping a turn signal's slot answers allow.
        while let Ok((event, reply)) = signal_server.signals().try_recv() {
            match event {
                crate::signal::SignalEvent::Turn(signal) => {
                    mux.handle_signal_with_reply(signal, reply)
                }
                crate::signal::SignalEvent::Unbound(sig) => mux.handle_unbound_signal(sig, reply),
                crate::signal::SignalEvent::Note(note) => mux.handle_note(note),
            }
        }

        // 3. Control jobs — execute the main worker's queued MCP tool calls against
        //    live panels on this same thread (Invariant I-5).
        mux.drain_control(&mut control_server);

        // 3b. Deferred worktree spawns — a `spawn_role(worktree=true)` runs its
        //     slow `git worktree add` on a worker thread so it never freezes this
        //     loop; finish any whose worktree is now ready (launch the panel and
        //     answer the deferred MCP call). New panels are pumped by step 4.
        mux.poll_pending_spawns();

        // 4. PTY pump — drain every panel into its grid + capture, reap exits.
        mux.pump_all();

        // 4b. Deferred submits — a bracketed paste's submitting Enter is held
        //     out of the paste burst (the agent swallows a `\r` while it commits
        //     a `[Pasted text #N]` placeholder) and written here as a discrete
        //     keypress once its delay has elapsed, a tick after the paste landed.
        mux.poll_pending_submits();

        // 4c. Resume notice — if this session resumed with rounds left in flight
        //     by the prior instance, tell the main worker (whose conversation
        //     reloaded believing its round was live) once it is idle, so it
        //     stops waiting. One-shot; shares the one-push-per-tick gate and
        //     runs before round delivery so the dropped-round notice lands first.
        mux.poll_resume_notice();

        // 4d. In-band notifications — drain the OSC 9/99/777 texts the pump
        //     just parsed into each grid and record them on the panel's
        //     manifest timeline (`NotificationSeen`). Capture only — no state
        //     transition, no push — so it competes with nothing below.
        mux.poll_notifications();

        // 5. Blocked panels — if a panel in a pending round has stopped on an
        //    interactive chooser or a raw `[y/n]` prompt (no Stop hook fires, so
        //    its round never settles), announce it to the main worker so it can
        //    answer and let the round finish. Runs before round delivery:
        //    unblocking a stuck panel takes precedence, and both share the
        //    one-push-per-tick gate.
        mux.poll_round_blocked_panels();

        // 6. Round delivery — if a registered round's panels have now settled
        //    (or its fallback deadline passed), assemble their results and
        //    inject them into the main worker's panel (the caucus→main push).
        mux.poll_pending_rounds();

        // 6b. Question notices — a panel agent posted a `question` note
        //     (`caucus signal note`); announce it to the main worker. After
        //     round delivery: a finished round is the primary deliverable, and
        //     all pushes share the one-push-per-tick gate.
        mux.poll_question_notices();

        // 7. Stranded-main guard — if the main worker went idle with no round
        //    registered while sub-panels still run, nothing above can ever
        //    re-prompt it (both pushes need a round). Nudge it. Runs last: it
        //    only fires when `pending_rounds` is empty, so it never competes
        //    with steps 5–6 for the one-push-per-tick gate.
        mux.poll_stranded_main();

        if mux.should_quit() {
            break;
        }

        // 8. Redraw on the tick — dirty-gated to keep an idle session off the
        //    CPU (an unchanged screen otherwise repaints every 16 ms). The TICK
        //    paces the signature check; within a tick a frame is painted only
        //    when `render_signature` changed since the last paint, plus a
        //    periodic forced repaint as a safety net. A draw failure is treated
        //    like a poll/read error — always transient and recoverable — so a
        //    write hiccup during a display wake or monitor switch does not tear
        //    the session down, and the TICK gate paces it with no extra backoff.
        if last_draw.elapsed() >= TICK {
            // Reconcile the layout's basis with the terminal's *actual* size
            // before deciding to paint. The Resize event is the fast path, but
            // it can be lost: during a display wake crossterm's SIGWINCH-side
            // size query fails mid-transition and the poll/read error is
            // absorbed above — leaving the layout tiling a stale area while
            // ratatui's autoresize already shrank the buffer (the draw layer
            // clips such a frame instead of panicking; see `render::draw`).
            // This probe heals the stale area within one tick. A size error
            // here is the same transient class as a poll error — skip and
            // retry next tick.
            if let Ok(size) = terminal.size() {
                let body = body_area(Rect {
                    x: 0,
                    y: 0,
                    width: size.width,
                    height: size.height,
                });
                if body != mux.area() {
                    let _ = mux.resize(body);
                }
            }
            let sig = mux.render_signature();
            let forced = last_forced_draw.elapsed() >= FORCED_REDRAW_INTERVAL;
            if forced || Some(sig) != last_sig {
                if let Err(e) = draw(&mut terminal, &mux) {
                    warn!(error = %e, "terminal draw error; continuing");
                }
                last_sig = Some(sig);
                // Any paint — dirty or forced — resets the safety-net timer.
                last_forced_draw = Instant::now();
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

/// Write a copy-mode OSC 52 set-clipboard sequence to the host terminal. The
/// sequence carries no visible glyphs and leaves the cursor put, so emitting it
/// between ratatui frames does not disturb the drawn screen; stdout is locked
/// and flushed so it is delivered as one uninterrupted burst.
fn write_clipboard_osc(seq: &str) -> io::Result<()> {
    use std::io::Write;
    let mut out = io::stdout().lock();
    out.write_all(seq.as_bytes())?;
    out.flush()
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

/// One-line status bar: panel count, focus, working-panel count, and the keymap hint.
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
    let working = mux
        .panels()
        .iter()
        .filter(|p| p.state() == PanelState::Working)
        .count();
    format!(
        " caucus · {} panel(s) · focus: {} · working: {} · \
         Ctrl-{key} then n/p/arrows focus, z zoom, x close, t transcript, [ scroll, q quit{}{}{}",
        mux.panels().len(),
        focused,
        working,
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
    fn default_topic_is_the_repo_directory_name() {
        assert_eq!(
            default_topic(std::path::Path::new("/Users/me/codes/caucus")),
            "caucus"
        );
        // No final component → the generic fallback rather than an empty label.
        assert_eq!(default_topic(std::path::Path::new("/")), "caucus session");
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

    // Boundary cases for the SIGHUP classifier (one per invariant boundary, not
    // per scenario): a live terminal always survives; a dead terminal only
    // confirms once the window has elapsed.
    #[test]
    fn hangup_verdict_live_terminal_always_survives() {
        let w = Duration::from_millis(1000);
        assert_eq!(
            hangup_verdict(true, Duration::ZERO, w),
            HangupAction::Survive
        );
        assert_eq!(hangup_verdict(true, w, w), HangupAction::Survive);
        assert_eq!(hangup_verdict(true, w * 2, w), HangupAction::Survive);
    }

    #[test]
    fn hangup_verdict_dead_within_window_waits() {
        let w = Duration::from_millis(1000);
        assert_eq!(hangup_verdict(false, Duration::ZERO, w), HangupAction::Wait);
        assert_eq!(
            hangup_verdict(false, w - Duration::from_millis(1), w),
            HangupAction::Wait
        );
    }

    #[test]
    fn hangup_verdict_dead_past_window_confirms() {
        // The boundary is inclusive: elapsed == window confirms.
        let w = Duration::from_millis(1000);
        assert_eq!(hangup_verdict(false, w, w), HangupAction::Confirm);
        assert_eq!(hangup_verdict(false, w * 2, w), HangupAction::Confirm);
    }

    // A real controlling terminal is openable; the test harness inherits one
    // under `cargo test` from a terminal. When it does not (CI with no tty),
    // the probe returns `false` — either way it must not panic, and the result
    // is a plain bool. This guards the probe against regressions in the retry
    // loop / error mapping without asserting a tty is present.
    #[cfg(unix)]
    #[test]
    fn controlling_terminal_probe_is_total() {
        let _alive: bool = controlling_terminal_alive();
    }

    /// Reparenting to init after starting under a live parent is the orphan
    /// signal (tmux server killed, terminal emulator died).
    #[test]
    fn orphaned_detects_reparenting_to_init() {
        assert!(orphaned(4242, 1));
    }

    /// While the original parent (or any non-init parent) is in place, the
    /// session is hosted — never orphaned.
    #[test]
    fn orphaned_is_false_under_a_live_parent() {
        assert!(!orphaned(4242, 4242));
        assert!(!orphaned(4242, 7));
    }

    /// A process legitimately started under init can never read as orphaned;
    /// otherwise it would confirm shutdown at the first loop iteration.
    #[test]
    fn orphaned_is_false_when_started_under_init() {
        assert!(!orphaned(1, 1));
    }
}
