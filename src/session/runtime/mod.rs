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
use std::time::Instant;

use anyhow::{Context, Result};

use crate::agent::manifest::AgentManifest;
use crate::config::Config;
use crate::input::FocusRouter;
use crate::mcp::control_server::ControlServer;
use crate::panel::lifecycle::Panel;
use crate::render::{Layout, LayoutMode, LayoutTree, Rect};
use crate::session::id::PanelId;
use crate::session::lock::{SessionLock, SessionLockError};
use crate::session::state::Session;
use crate::signal::server::SignalServer;
use crate::worktree::cleanup::CleanupQueue;

mod control;
mod input;
mod layout;
mod mcp;
mod persist;
mod rounds;
mod scroll;
mod spawn;
mod spawn_async;

use self::mcp::PendingSubmit;
use self::rounds::PendingRound;
pub(crate) use self::scroll::ScrollState;

/// The live multiplexer: one [`Session`] plus every panel running in it.
pub struct Multiplexer {
    /// The session this multiplexer drives.
    pub session: Session,
    /// Merged role configuration (embedded + global + project).
    pub config: Config,
    /// Single-instance lock on the session root, held for this multiplexer's
    /// lifetime so no second caucus can open the same session concurrently. Kept
    /// only for its `Drop` (the kernel releases the lock on process exit).
    _session_lock: SessionLock,
    /// Live panels, in spawn order — also the focus-cycle order.
    panels: Vec<Panel>,
    /// Per-panel agent manifest, keyed by panel id. Mutated only via
    /// `agent::manifest::write`.
    manifests: HashMap<PanelId, AgentManifest>,
    /// Current screen layout — the projection of [`Multiplexer::layout_tree`]
    /// onto [`Multiplexer::area`] (or a single full-area slot while zoomed),
    /// recomputed on every spawn/kill/resize/move.
    layout: Layout,
    /// The live binary space-partition behind the tiling. Rebuilt from the
    /// `layout_mode` preset + panel order on every structural change
    /// ([`Multiplexer::rebuild_layout_tree`]); `Ctrl-A Ctrl-arrow`
    /// ([`Multiplexer::resize_focused`]) perturbs its split ratios in place so
    /// a manual resize survives terminal resizes but resets on the next
    /// spawn/kill/move/mode switch (tmux `select-layout` semantics). `None`
    /// until the first panel is spawned.
    layout_tree: Option<LayoutTree>,
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
    /// `spawn_role(worktree=true)` calls whose `git worktree add` is running on
    /// a worker thread. Finished off the event loop so a slow worktree create
    /// never freezes the TUI; completed by [`Multiplexer::poll_pending_spawns`].
    pending_spawns: Vec<spawn_async::PendingSpawn>,
    /// Submitting Enters held back from bracketed pastes until the agent has
    /// ingested the paste. A bracketed-paste agent (Claude Code) that commits a
    /// large paste to a `[Pasted text #N]` placeholder swallows a `\r` arriving
    /// in the *same* PTY burst, so the submit is delivered as a discrete
    /// keypress on a later tick ([`Multiplexer::poll_pending_submits`]).
    pending_submits: Vec<PendingSubmit>,
    /// The panel awaiting a close confirmation (`Ctrl-A x`), or `None`. While
    /// `Some`, the close-confirm prompt is drawn and the focus router captures
    /// keys ([`crate::input::FocusRouter::set_confirm_open`]): `y` kills the
    /// panel via [`Multiplexer::kill_panel`], `n`/`Esc` clears it. The main
    /// worker panel is never placed here — it is protected from closing.
    pending_close: Option<PanelId>,
    /// The main worker panel — the round-delivery target. Set when the main
    /// panel is spawned; `None` before then.
    main_panel_id: Option<PanelId>,
    /// Instant of the last *un-submitted* human keystroke to the main panel —
    /// i.e. the user may be mid-composing a line that has not been sent yet.
    /// Cleared the moment that line is submitted
    /// ([`Multiplexer::note_prompt_delivered`]), so it never outlives the
    /// compose and a stale timestamp from an already-submitted line cannot hold
    /// the next round. Gates round delivery: caucus holds an injected turn for
    /// `COMPOSE_GRACE` after this instant so the injection never lands in the
    /// middle of a line the user is composing.
    main_compose_since: Option<Instant>,
    /// Blocking prompts already announced to the main worker, keyed by the
    /// panel showing one — value is the prompt's content signature
    /// ([`rounds::BlockedPrompt::signature`]). Dedups the proactive
    /// blocked-panel push ([`Multiplexer::poll_round_blocked_panels`]) so a
    /// panel sitting on one selection menu or `[y/n]` prompt is announced once,
    /// not every tick; an entry is dropped when its panel leaves the prompt, and
    /// replaced when the prompt's content changes.
    notified_blockers: HashMap<PanelId, u64>,
    /// Instant of the last stranded-main nudge, or `None` while not stranded.
    /// caucus's only caucus→main pushes require a registered round; if the
    /// main worker ends its turn without one while sub-panels still run, no
    /// path ever re-prompts it. [`Multiplexer::poll_stranded_main`] prods it,
    /// rate-limited by this latch (`STRANDED_NUDGE_COOLDOWN`) so a main that
    /// keeps idling without registering a round is nudged periodically, never
    /// every tick. Cleared the moment main is no longer stranded, so a fresh
    /// stranding re-arms immediately.
    main_stranded_last_nudge: Option<Instant>,
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
    /// Cached blocking-prompt scan per panel, keyed by panel id, valued by the
    /// grid `generation` it was computed against plus the detected
    /// [`rounds::BlockedPrompt`] (or `None`).
    /// [`Multiplexer::poll_round_blocked_panels`] runs every tick while a round
    /// is pending; without this it would re-materialise each round panel's full
    /// viewport and re-run the grid scanners on every iteration even when the
    /// grid did not change. The cache recomputes only when a panel's grid
    /// generation advances; an entry is pruned when its panel is killed.
    blocked_scan_cache: HashMap<PanelId, (u64, Option<rounds::BlockedPrompt>)>,
    /// Notice to inject into the resumed main worker once it is idle: the
    /// in-flight rounds a prior caucus instance dropped on quit/crash. The
    /// main worker's claude conversation reloads still believing its
    /// `register_round` is live, so without this it waits forever for a
    /// delivery that can never come. Set once by
    /// [`Multiplexer::ingest_resumed_rounds`] from the persisted
    /// `pending-rounds.json`; delivered and cleared by
    /// [`Multiplexer::poll_resume_notice`]. `None` on a fresh launch.
    resume_round_notice: Option<String>,
    /// Instant of the last child-liveness probe, or `None` before the first.
    /// [`Multiplexer::pump_all`] drains every PTY each tick for responsiveness
    /// but probes process liveness (a `try_wait`/`waitpid` syscall per panel)
    /// only every [`LIVENESS_PROBE_INTERVAL`]: on the idle loop (~250 Hz) a
    /// per-panel `waitpid` every tick is pure overhead, and a child exit may
    /// surface up to one interval late without any user-visible cost.
    last_liveness_probe: Option<Instant>,
    /// Monotonic counter bumped on every handled key event. The draw loop is
    /// dirty-gated on [`Multiplexer::render_signature`], whose other inputs
    /// (grid generations, panel set, derived states) cover everything that
    /// changes off a PTY read or a turn signal. Key-driven view changes —
    /// layout/zoom/scroll/transcript toggles, the prefix-armed status hint,
    /// the close-confirm prompt — have no such counter, so this epoch is the
    /// catch-all that forces exactly one redraw after any keystroke.
    view_epoch: u64,
    /// An OSC 52 set-clipboard escape sequence the pager's copy-mode yank
    /// queued, awaiting flush to the host terminal. The Multiplexer never
    /// writes to stdout itself (it owns no terminal handle and must stay
    /// unit-testable); the event loop drains this with
    /// [`Multiplexer::take_pending_clipboard`] and writes it. `None` when no
    /// copy is pending.
    pending_clipboard: Option<String>,
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
        prefix: char,
    ) -> Result<(Self, SignalServer, ControlServer)> {
        std::fs::create_dir_all(session.root_dir.join("agents"))
            .with_context(|| format!("create {}", session.root_dir.display()))?;
        std::fs::create_dir_all(session.root_dir.join("panels"))?;

        // Claim the session before binding its sockets: `SignalServer::bind`
        // unlinks any existing socket, so acquiring the lock first means a
        // second caucus refuses here instead of stealing the live owner's
        // socket. The lock is held for the multiplexer's lifetime.
        let session_lock = SessionLock::acquire(&session.root_dir).map_err(|err| match err {
            SessionLockError::AlreadyRunning { .. } => anyhow::anyhow!(
                "session {} is already open in another caucus process; \
                 close it first (or it exited uncleanly — retry)",
                session.id
            ),
            other => anyhow::Error::new(other),
        })?;

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
                _session_lock: session_lock,
                panels: Vec::new(),
                manifests: HashMap::new(),
                layout: Layout::default(),
                layout_tree: None,
                focus: FocusRouter::with_prefix(prefix),
                cleanup,
                sock_path,
                control_sock_path,
                area,
                quit: false,
                role_counts: HashMap::new(),
                pending_rounds: Vec::new(),
                pending_spawns: Vec::new(),
                pending_submits: Vec::new(),
                pending_close: None,
                main_panel_id: None,
                main_compose_since: None,
                notified_blockers: HashMap::new(),
                main_stranded_last_nudge: None,
                layout_mode: LayoutMode::default(),
                zoom: None,
                show_transcript: false,
                scroll: None,
                worktree_branches: HashMap::new(),
                blocked_scan_cache: HashMap::new(),
                resume_round_notice: None,
                last_liveness_probe: None,
                view_epoch: 0,
                pending_clipboard: None,
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

    /// The reserved prefix letter — caucus commands are `Ctrl-<this>`.
    pub fn prefix(&self) -> char {
        self.focus.prefix()
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

    /// Per-panel manifests, keyed by panel id — read-only, for the overlay.
    pub fn manifests(&self) -> &HashMap<PanelId, AgentManifest> {
        &self.manifests
    }

    /// Whether the read-only transcript overlay is currently shown.
    pub fn show_transcript(&self) -> bool {
        self.show_transcript
    }

    /// The panel awaiting a close confirmation (`Ctrl-A x`), if the prompt is
    /// open — drives the confirm prompt drawn in the status bar.
    pub fn pending_close(&self) -> Option<PanelId> {
        self.pending_close
    }

    /// A hash of everything [`crate::tui::draw`] reads, so the event loop can
    /// skip a redraw when nothing visible changed (the idle-CPU floor: an idle
    /// session otherwise repaints the whole screen every 16 ms `TICK`).
    ///
    /// The inputs partition cleanly by what can change them:
    /// - **PTY reads** bump each panel's grid `generation` (any byte ingest,
    ///   including cursor moves — see [`crate::term::Grid::advance`]).
    /// - **Spawn/kill** change the panel set (ids + count).
    /// - **Turn signals / exit reaping** change `state_label` and the
    ///   manifest-derived state.
    /// - **Keystrokes** change the view with no other counter, so they bump
    ///   [`view_epoch`](Self::view_epoch) (focus, layout, zoom, scroll,
    ///   transcript, prefix hint, close-confirm).
    ///
    /// Per-panel and per-manifest contributions are XOR-folded so the
    /// non-deterministic `HashMap` iteration order of `manifests` cannot
    /// perturb the result. The draw loop also forces a periodic redraw, so a
    /// missed input degrades to bounded staleness, never a permanent freeze.
    pub fn render_signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        fn hash_one(v: impl Hash) -> u64 {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            v.hash(&mut h);
            h.finish()
        }
        let mut acc = hash_one((self.view_epoch, self.focused(), self.panels.len()));
        for p in &self.panels {
            acc ^= hash_one((
                p.id,
                p.grid().generation(),
                p.state_label(),
                p.role.as_str(),
            ));
        }
        for (id, manifest) in &self.manifests {
            acc ^= hash_one((*id, manifest.derived_state().as_str()));
        }
        acc
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
pub(crate) mod test_support;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::runtime::test_support::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn new_creates_session_dirs_and_socket() {
        let tmp = TempDir::new().unwrap();
        let session = Session::new("test", tmp.path().to_path_buf());
        let config = Config::load(tmp.path()).unwrap();
        // Hold the signal server handle: the turn-signal socket exists only
        // while it is alive (removed on drop — see `SignalServer`'s Drop).
        let (mux, _signal, _control) = Multiplexer::new(session, config, area(), 'a').unwrap();
        assert!(mux.session.root_dir.join("agents").is_dir());
        assert!(mux.session.root_dir.join("panels").is_dir());
        assert!(mux.sock_path().exists());
    }

    #[tokio::test]
    async fn new_binds_a_control_socket() {
        let tmp = TempDir::new().unwrap();
        let session = Session::new("test", tmp.path().to_path_buf());
        let config = Config::load(tmp.path()).unwrap();
        let (mux, _signal, control) = Multiplexer::new(session, config, area(), 'a').unwrap();
        // The control socket is distinct from the turn-signal socket and
        // exists on disk.
        assert!(control.sock_path().exists());
        assert_ne!(control.sock_path(), mux.sock_path());
        assert_eq!(mux.control_sock_path(), control.sock_path());
    }
}
