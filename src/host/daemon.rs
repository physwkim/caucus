//! `caucus host serve` — the sub-host daemon.
//!
//! One task owns every panel on the host; connections only parse frames and
//! write lines. A panel's output is numbered from its first byte and kept in a
//! ring of the last 8 MiB, so a main caucus that lost its link
//! re-attaches from the first byte it has not seen.
//!
//! Each connection belongs to one session (its `hello`); a newer connection
//! for the same session replaces the older one. A connection streams only the
//! panels it attached, and only as fast as it reads: output goes out through
//! a bounded queue and waits in the ring while the queue is full.
//!
//! Turn signals from panels here land on the daemon's own signal socket. A
//! signal for a panel is held until that panel's output has gone out, so main
//! never sees a turn end before the bytes the turn printed.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions, TryLockError};
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use super::proto::{
    Bytes, ClientFrame, DaemonFrame, HostInfo, MAX_FRAME_BYTES, OUTPUT_CHUNK, PROTO_VERSION,
    SpawnSpec, encode,
};
use super::{HostPaths, home_relative, private_dir};
use crate::line_io::{CappedLine, read_capped_line};
use crate::pty::LocalPty;
use crate::session::id::{PanelId, SessionId};
use crate::signal::SignalEvent;
use crate::signal::server::SignalServer;
use crate::worktree::cleanup::{self, CleanupJob};
use crate::worktree::manager::{self, run_git};

/// Output a panel keeps for a client to re-read after a drop.
const LOG_CAP: usize = 8 << 20;
/// How often the daemon drains its PTYs while it has panels.
const TICK: Duration = Duration::from_millis(5);
/// How often it asks whether each child is still running.
const LIVENESS_PROBE: Duration = Duration::from_millis(250);
/// Lines queued toward one connection before output waits in the ring.
const CONNECTION_QUEUE: usize = 64;
/// Signals held for one panel or session before the oldest is dropped.
const SIGNAL_BACKLOG: usize = 1024;
/// How long a fresh connection has to say `hello`.
const HELLO_WAIT: Duration = Duration::from_secs(10);
/// How often each connection is sent the host's load.
const LOAD_EVERY: Duration = Duration::from_secs(5);

/// The daemon's single-instance lock. Holding it is what makes removing a
/// stale control socket safe, so [`run`] takes it as an argument.
#[derive(Debug)]
pub struct DaemonLock {
    _file: File,
}

/// Take the daemon lock, or `None` when another daemon holds it.
pub fn lock(paths: &HostPaths) -> Result<Option<DaemonLock>> {
    private_dir(&paths.state_dir)
        .with_context(|| format!("prepare {}", paths.state_dir.display()))?;
    let path = paths.lock_file();
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {
            // The pid is for a human (or a test) looking for the daemon.
            let _ = file.set_len(0);
            let _ = (&file).write_all(format!("{}\n", std::process::id()).as_bytes());
            Ok(Some(DaemonLock { _file: file }))
        }
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => {
            Err(err).with_context(|| format!("lock {}", path.display()))
        }
    }
}

/// `caucus host serve`: run the daemon until `SIGTERM`/`SIGINT`, or return at
/// once when another daemon already holds the lock.
pub fn serve(paths: HostPaths) -> Result<()> {
    // The daemon outlives the SSH session that started it. Panels get the
    // default disposition back (portable-pty resets it in the child).
    // SAFETY: runs before any thread exists; `SIG_IGN` is a valid disposition.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("caucus=info"));
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .try_init();
    let Some(lock) = lock(&paths)? else {
        info!(lock = %paths.lock_file().display(), "a caucus host daemon is already running");
        return Ok(());
    };
    let runtime = tokio::runtime::Runtime::new().context("start tokio runtime")?;
    runtime.block_on(run(paths, lock, terminated()))
}

async fn terminated() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        // No handler means the default disposition still ends the process.
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// Serve until `shutdown` resolves, then kill every panel.
pub async fn run(
    paths: HostPaths,
    _lock: DaemonLock,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    if let Some(dir) = paths.control_sock.parent() {
        private_dir(dir).with_context(|| format!("prepare {}", dir.display()))?;
    }
    // A socket file left by a dead daemon; the lock says none is alive.
    let _ = std::fs::remove_file(&paths.control_sock);
    let listener = UnixListener::bind(&paths.control_sock)
        .with_context(|| format!("bind {}", paths.control_sock.display()))?;
    let mut signals = SignalServer::bind(&paths.signal_sock)
        .with_context(|| format!("bind {}", paths.signal_sock.display()))?;
    info!(
        control = %paths.control_sock.display(),
        signal = %paths.signal_sock.display(),
        "caucus host daemon listening"
    );

    let (events_tx, mut events) = mpsc::channel(1024);
    let accept = tokio::spawn(accept_loop(listener, events_tx));
    let (chores_done, mut chore_replies) = mpsc::unbounded_channel();
    let mut daemon = Daemon::new(paths.clone(), chores_done);
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut load_tick = tokio::time::interval(LOAD_EVERY);
    load_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            Some(event) = events.recv() => daemon.on_event(event),
            Some((session, frame)) = chore_replies.recv() => daemon.reply(session, &frame),
            Some((signal, reply)) = signals.signals().recv() => {
                // Main answers a sub-host signal too late to hold a hook, so
                // the hook is released at once (an empty reply means allow).
                drop(reply);
                daemon.on_signal(&signal);
            }
            _ = tick.tick(), if daemon.wants_tick() => daemon.pump(Instant::now()),
            _ = load_tick.tick(), if !daemon.clients.is_empty() => daemon.push_load(),
        }
    }
    accept.abort();
    daemon.shutdown().await;
    let _ = std::fs::remove_file(&paths.control_sock);
    info!("caucus host daemon stopped");
    Ok(())
}

/// What a connection task tells the daemon.
enum ConnEvent {
    Hello {
        conn: u64,
        session: SessionId,
        tx: mpsc::Sender<String>,
        /// Dropping this ends the connection.
        kick: oneshot::Sender<()>,
    },
    Frame {
        conn: u64,
        frame: ClientFrame,
    },
    Closed {
        conn: u64,
    },
}

async fn accept_loop(listener: UnixListener, events: mpsc::Sender<ConnEvent>) {
    let mut next = 0u64;
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                next += 1;
                tokio::spawn(connection(next, stream, events.clone()));
            }
            Err(err) => {
                warn!(error = %err, "host control accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Read one connection's frames until it closes or the daemon kicks it.
async fn connection(conn: u64, stream: UnixStream, events: mpsc::Sender<ConnEvent>) {
    let (read, write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    let hello = tokio::time::timeout(
        HELLO_WAIT,
        read_capped_line(&mut reader, &mut line, MAX_FRAME_BYTES),
    )
    .await;
    if !matches!(hello, Ok(Ok(CappedLine::Line))) {
        return;
    }
    let session = match serde_json::from_str::<ClientFrame>(&line) {
        Ok(ClientFrame::Hello { proto, session }) if proto == PROTO_VERSION => session,
        Ok(ClientFrame::Hello { proto, .. }) => {
            refuse(
                write,
                format!("protocol {proto} not supported; this host speaks {PROTO_VERSION}"),
            )
            .await;
            return;
        }
        _ => {
            refuse(write, "first frame must be hello".into()).await;
            return;
        }
    };
    let (tx, rx) = mpsc::channel(CONNECTION_QUEUE);
    let (kick, mut kicked) = oneshot::channel();
    tokio::spawn(write_loop(write, rx));
    let hello = ConnEvent::Hello {
        conn,
        session,
        tx,
        kick,
    };
    if events.send(hello).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            _ = &mut kicked => return,
            read = read_capped_line(&mut reader, &mut line, MAX_FRAME_BYTES) => match read {
                Ok(CappedLine::Line) if line.is_empty() => {}
                Ok(CappedLine::Line) => match serde_json::from_str::<ClientFrame>(&line) {
                    Ok(frame) => {
                        if events.send(ConnEvent::Frame { conn, frame }).await.is_err() {
                            return;
                        }
                    }
                    Err(err) => warn!(conn, error = %err, "unreadable client frame"),
                },
                Ok(CappedLine::TooLong) => {
                    warn!(conn, "client frame over {MAX_FRAME_BYTES} bytes; closing");
                    break;
                }
                Ok(CappedLine::Eof) | Err(_) => break,
            },
        }
    }
    let _ = events.send(ConnEvent::Closed { conn }).await;
}

async fn refuse(mut write: OwnedWriteHalf, msg: String) {
    warn!(%msg, "refusing host connection");
    let _ = write
        .write_all(encode(&DaemonFrame::Error { msg }).as_bytes())
        .await;
    let _ = write.shutdown().await;
}

/// Write queued lines until the daemon drops the queue's sender.
async fn write_loop(write: OwnedWriteHalf, mut rx: mpsc::Receiver<String>) {
    let mut out = BufWriter::new(write);
    while let Some(line) = rx.recv().await {
        if out.write_all(line.as_bytes()).await.is_err() {
            return;
        }
        if rx.is_empty() && out.flush().await.is_err() {
            return;
        }
    }
    let _ = out.flush().await;
    let _ = out.shutdown().await;
}

/// The connection currently serving one session.
struct Client {
    conn: u64,
    tx: mpsc::Sender<String>,
    _kick: oneshot::Sender<()>,
    /// Replies and session-wide signals, sent ahead of panel output.
    queued: VecDeque<String>,
}

impl Client {
    /// Send queued lines until the connection's queue fills. `true` when
    /// nothing is left waiting.
    fn flush(&mut self) -> bool {
        while let Some(line) = self.queued.pop_front() {
            if let Err(err) = self.tx.try_send(line) {
                match err {
                    mpsc::error::TrySendError::Full(line) => self.queued.push_front(line),
                    // The connection is gone; its `Closed` event is on the way.
                    mpsc::error::TrySendError::Closed(_) => self.queued.clear(),
                }
                return false;
            }
        }
        true
    }

    fn has_room(&self) -> bool {
        self.tx.capacity() > 0
    }

    fn send_now(&self, frame: &DaemonFrame) {
        // Only called after `has_room`, and the daemon is the only sender.
        let _ = self.tx.try_send(encode(frame));
    }
}

/// Where a panel's child is in dying. A child seen dead stays `Dying` for one
/// probe interval so output its reader thread still holds reaches the ring
/// before the panel is reported gone.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Life {
    Running,
    Dying,
    Exited(Option<String>),
}

struct DPanel {
    session: SessionId,
    /// `None` when the launch failed; the error is in the output.
    pty: Option<LocalPty>,
    /// The last [`LOG_CAP`] output bytes; `log[0]` is byte `base`.
    log: VecDeque<u8>,
    base: u64,
    /// Next byte to send, while the session's connection has attached the
    /// panel. `None` until it attaches.
    sent: Option<u64>,
    life: Life,
    /// Whether this connection has been told about the exit.
    exit_sent: bool,
    /// Signals the panel posted, held until its output has gone out.
    signals: VecDeque<String>,
}

impl DPanel {
    fn new(session: SessionId, pty: Option<LocalPty>) -> Self {
        Self {
            session,
            pty,
            log: VecDeque::new(),
            base: 0,
            sent: None,
            life: Life::Running,
            exit_sent: false,
            signals: VecDeque::new(),
        }
    }

    fn end(&self) -> u64 {
        self.base + self.log.len() as u64
    }

    fn append(&mut self, bytes: &[u8]) {
        self.log.extend(bytes);
        let over = self.log.len().saturating_sub(LOG_CAP);
        if over > 0 {
            self.log.drain(..over);
            self.base += over as u64;
        }
    }

    /// Stream to `client` until its queue fills. `true` once everything the
    /// panel has — output, held signals, its exit — has gone out.
    fn stream(&mut self, id: PanelId, client: &mut Client) -> bool {
        let Some(mut sent) = self.sent else {
            return true;
        };
        if sent < self.base {
            warn!(panel = %id, lost = self.base - sent, "client fell behind the output ring");
            sent = self.base;
        }
        while sent < self.end() {
            if !client.has_room() {
                self.sent = Some(sent);
                return false;
            }
            let start = (sent - self.base) as usize;
            let stop = (start + OUTPUT_CHUNK).min(self.log.len());
            let data: Vec<u8> = self.log.range(start..stop).copied().collect();
            client.send_now(&DaemonFrame::Output {
                panel: id,
                offset: sent,
                data: Bytes(data),
            });
            sent += (stop - start) as u64;
        }
        self.sent = Some(sent);
        while let Some(line) = self.signals.pop_front() {
            if !client.has_room() {
                self.signals.push_front(line);
                return false;
            }
            let _ = client.tx.try_send(line);
        }
        if let Life::Exited(reason) = &self.life
            && !self.exit_sent
        {
            if !client.has_room() {
                return false;
            }
            client.send_now(&DaemonFrame::Exited {
                panel: id,
                reason: reason.clone(),
            });
            self.exit_sent = true;
        }
        true
    }
}

/// Blocking work the daemon hands off its loop. One thread runs it in
/// arrival order, so a worktree removal never starts before the kills of the
/// panels working there have ended.
enum Chore {
    /// `LocalPty::kill` joins threads and waits for the child.
    Kill(LocalPty),
    Worktree {
        session: SessionId,
        req: u64,
        op: WorktreeOp,
    },
}

enum WorktreeOp {
    Add {
        repo: PathBuf,
        path: PathBuf,
        branch: String,
    },
    Remove {
        repo: PathBuf,
        path: PathBuf,
        delete_branch: Option<String>,
    },
}

impl WorktreeOp {
    fn run(self) -> Result<PathBuf, String> {
        match self {
            Self::Add { repo, path, branch } => add_worktree(&repo, &path, &branch),
            Self::Remove {
                repo,
                path,
                delete_branch,
            } => remove_worktree(&repo, &path, delete_branch),
        }
    }
}

/// The chore thread and its queue; dropping `tx` lets the thread finish the
/// queue and exit.
struct Chores {
    tx: std::sync::mpsc::Sender<Chore>,
    thread: std::thread::JoinHandle<()>,
}

impl Chores {
    fn start(done: mpsc::UnboundedSender<(SessionId, DaemonFrame)>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Chore>();
        let thread = std::thread::spawn(move || {
            for chore in rx {
                match chore {
                    Chore::Kill(mut pty) => {
                        let _ = pty.kill();
                    }
                    Chore::Worktree { session, req, op } => {
                        let result = op.run().map(|path| path.display().to_string());
                        let _ = done.send((session, DaemonFrame::WorktreeDone { req, result }));
                    }
                }
            }
        });
        Self { tx, thread }
    }
}

struct Daemon {
    paths: HostPaths,
    hostname: String,
    caucus_bin: PathBuf,
    panels: HashMap<PanelId, DPanel>,
    clients: HashMap<SessionId, Client>,
    last_probe: Instant,
    /// `None` once shutdown has drained it.
    chores: Option<Chores>,
}

impl Daemon {
    /// A worktree request's answer goes to `chores_done`, for the run loop to
    /// send to the session's connection.
    fn new(paths: HostPaths, chores_done: mpsc::UnboundedSender<(SessionId, DaemonFrame)>) -> Self {
        Self {
            paths,
            hostname: hostname(),
            caucus_bin: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("caucus")),
            panels: HashMap::new(),
            clients: HashMap::new(),
            last_probe: Instant::now(),
            chores: Some(Chores::start(chores_done)),
        }
    }

    fn chore(&self, chore: Chore) {
        if let Some(chores) = &self.chores {
            let _ = chores.tx.send(chore);
        }
    }

    fn wants_tick(&self) -> bool {
        !self.panels.is_empty() || self.clients.values().any(|c| !c.queued.is_empty())
    }

    fn on_event(&mut self, event: ConnEvent) {
        match event {
            ConnEvent::Hello {
                conn,
                session,
                tx,
                kick,
            } => self.on_hello(conn, session, tx, kick),
            ConnEvent::Frame { conn, frame } => {
                // A replaced connection's last frames are dropped.
                let Some(session) = self
                    .clients
                    .iter()
                    .find_map(|(s, c)| (c.conn == conn).then_some(*s))
                else {
                    return;
                };
                self.on_frame(session, frame);
            }
            ConnEvent::Closed { conn } => {
                self.clients.retain(|_, c| c.conn != conn);
            }
        }
    }

    fn on_hello(
        &mut self,
        conn: u64,
        session: SessionId,
        tx: mpsc::Sender<String>,
        kick: oneshot::Sender<()>,
    ) {
        let session_dir = self
            .paths
            .state_dir
            .join("sessions")
            .join(session.to_string());
        let first = match std::fs::create_dir_all(&session_dir) {
            Ok(()) => DaemonFrame::Welcome(HostInfo {
                proto: PROTO_VERSION,
                version: env!("CARGO_PKG_VERSION").to_string(),
                hostname: self.hostname.clone(),
                caucus_bin: self.caucus_bin.clone(),
                signal_sock: self.paths.signal_sock.clone(),
                session_dir,
            }),
            Err(err) => {
                let _ = tx.try_send(encode(&DaemonFrame::Error {
                    msg: format!("create {}: {err}", session_dir.display()),
                }));
                return;
            }
        };
        let mut client = Client {
            conn,
            tx,
            _kick: kick,
            queued: VecDeque::from([
                encode(&first),
                encode(&DaemonFrame::Load(super::load::sample())),
            ]),
        };
        client.flush();
        // Dropping the replaced client kicks its connection.
        if self.clients.insert(session, client).is_some() {
            info!(%session, "session reconnected; replacing its old connection");
        }
        // Nothing streams until the new connection says where it left off.
        for panel in self.panels.values_mut().filter(|p| p.session == session) {
            panel.sent = None;
            panel.exit_sent = false;
        }
    }

    /// Send every connection the host's load. A connection that is backed up
    /// skips this sample; the next one supersedes it.
    fn push_load(&self) {
        let line = encode(&DaemonFrame::Load(super::load::sample()));
        for client in self.clients.values() {
            let _ = client.tx.try_send(line.clone());
        }
    }

    fn reply(&mut self, session: SessionId, frame: &DaemonFrame) {
        if let Some(client) = self.clients.get_mut(&session) {
            client.queued.push_back(encode(frame));
            client.flush();
        }
    }

    fn on_frame(&mut self, session: SessionId, frame: ClientFrame) {
        let frame = match frame {
            ClientFrame::Hello { .. } => {
                self.reply(
                    session,
                    &DaemonFrame::Error {
                        msg: "hello sent twice".into(),
                    },
                );
                return;
            }
            ClientFrame::AddWorktree {
                req,
                repo,
                path,
                branch,
            } => {
                let op = WorktreeOp::Add {
                    repo: home_relative(repo.into()),
                    path: home_relative(path.into()),
                    branch,
                };
                self.chore(Chore::Worktree { session, req, op });
                return;
            }
            ClientFrame::RemoveWorktree {
                req,
                repo,
                path,
                delete_branch,
            } => {
                let op = WorktreeOp::Remove {
                    repo: home_relative(repo.into()),
                    path: home_relative(path.into()),
                    delete_branch,
                };
                self.chore(Chore::Worktree { session, req, op });
                return;
            }
            frame => frame,
        };
        let panel_id = match &frame {
            ClientFrame::Spawn { panel, .. }
            | ClientFrame::Input { panel, .. }
            | ClientFrame::Resize { panel, .. }
            | ClientFrame::Attach { panel, .. }
            | ClientFrame::Kill { panel } => *panel,
            ClientFrame::Hello { .. }
            | ClientFrame::AddWorktree { .. }
            | ClientFrame::RemoveWorktree { .. } => unreachable!("answered above"),
        };
        if self
            .panels
            .get(&panel_id)
            .is_some_and(|p| p.session != session)
        {
            self.reply(
                session,
                &DaemonFrame::Error {
                    msg: format!("panel {panel_id} belongs to another session"),
                },
            );
            return;
        }
        match frame {
            ClientFrame::Hello { .. }
            | ClientFrame::AddWorktree { .. }
            | ClientFrame::RemoveWorktree { .. } => unreachable!("answered above"),
            ClientFrame::Spawn { panel, spec } => {
                self.panels
                    .entry(panel)
                    .or_insert_with(|| spawn_panel(session, &spec));
                self.reply(session, &DaemonFrame::Spawned { panel });
            }
            ClientFrame::Input { panel, data } => {
                if let Some(pty) = self.panels.get_mut(&panel).and_then(|p| p.pty.as_mut())
                    && let Err(err) = pty.write(&data.0)
                {
                    warn!(%panel, error = %err, "input to a sub-host panel was dropped");
                }
            }
            ClientFrame::Resize { panel, cols, rows } => {
                if let Some(pty) = self.panels.get_mut(&panel).and_then(|p| p.pty.as_mut())
                    && let Err(err) = pty.resize(cols, rows)
                {
                    warn!(%panel, error = %err, "sub-host panel resize failed");
                }
            }
            ClientFrame::Attach { panel, from } => match self.panels.get_mut(&panel) {
                Some(p) => {
                    let from = from.min(p.end());
                    if from < p.base {
                        // The client's grid lost bytes it can never get back;
                        // a size change makes the agent repaint its screen.
                        warn!(%panel, lost = p.base - from, "re-attach past the output ring");
                        jiggle(p);
                    }
                    p.sent = Some(from.max(p.base));
                    p.exit_sent = false;
                }
                None => self.reply(
                    session,
                    &DaemonFrame::Exited {
                        panel,
                        reason: Some("no such panel on this host (daemon restarted?)".into()),
                    },
                ),
            },
            ClientFrame::Kill { panel } => {
                if let Some(mut p) = self.panels.remove(&panel)
                    && let Some(pty) = p.pty.take()
                {
                    self.chore(Chore::Kill(pty));
                }
                self.reply(
                    session,
                    &DaemonFrame::Exited {
                        panel,
                        reason: Some("killed".into()),
                    },
                );
            }
        }
    }

    /// Route a signal posted on this host to the connection of its session.
    fn on_signal(&mut self, event: &SignalEvent) {
        let line = match serde_json::to_string(event) {
            Ok(line) => encode(&DaemonFrame::Signal { line }),
            Err(err) => {
                warn!(error = %err, "signal does not re-serialise");
                return;
            }
        };
        let target = match event {
            SignalEvent::Turn(s) => Some((s.session_id, s.panel_id)),
            SignalEvent::Note(n) => Some((n.session_id, n.panel_id)),
            SignalEvent::Lifecycle(l) => Some((l.session_id, l.panel_id)),
            SignalEvent::Unbound(_) => None,
        };
        match target {
            Some((session, panel)) => {
                if let Some(p) = self.panels.get_mut(&panel)
                    && p.session == session
                {
                    push_capped(&mut p.signals, line);
                } else if let Some(client) = self.clients.get_mut(&session) {
                    push_capped(&mut client.queued, line);
                    client.flush();
                }
            }
            // An unbound hook names no session; each session decides whether
            // the conversation is one of its panels' sub-agents.
            None => {
                for client in self.clients.values_mut() {
                    push_capped(&mut client.queued, line.clone());
                    client.flush();
                }
            }
        }
    }

    fn pump(&mut self, now: Instant) {
        for panel in self.panels.values_mut() {
            if let Some(pty) = panel.pty.as_mut()
                && let Ok(bytes) = pty.read()
                && !bytes.is_empty()
            {
                panel.append(&bytes);
            }
        }
        if now.duration_since(self.last_probe) >= LIVENESS_PROBE {
            self.last_probe = now;
            for panel in self.panels.values_mut() {
                match &panel.life {
                    Life::Running => {
                        if panel.pty.as_mut().is_some_and(|pty| !pty.is_alive()) {
                            panel.life = Life::Dying;
                        }
                    }
                    Life::Dying => panel.life = Life::Exited(None),
                    Life::Exited(_) => {}
                }
            }
        }
        self.stream();
    }

    fn stream(&mut self) {
        for (session, client) in &mut self.clients {
            if !client.flush() {
                continue;
            }
            for (id, panel) in self.panels.iter_mut() {
                if panel.session == *session && !panel.stream(*id, client) {
                    break;
                }
            }
        }
    }

    async fn shutdown(&mut self) {
        let kills: Vec<_> = self
            .panels
            .drain()
            .filter_map(|(_, mut p)| p.pty.take())
            .map(|mut pty| tokio::task::spawn_blocking(move || pty.kill()))
            .collect();
        for kill in kills {
            let _ = kill.await;
        }
        // Let queued kills and worktree requests finish.
        if let Some(Chores { tx, thread }) = self.chores.take() {
            drop(tx);
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}

/// Check `branch` out at `path`. A worktree already there on `branch` is the
/// one an earlier copy of the same request made.
fn add_worktree(repo: &Path, path: &Path, branch: &str) -> Result<PathBuf, String> {
    if path.exists() {
        let head = run_git(
            path,
            &["rev-parse".into(), "--abbrev-ref".into(), "HEAD".into()],
        );
        return match head {
            Ok(head) if head.trim() == branch => Ok(path.to_path_buf()),
            _ => Err(format!(
                "{} already exists and is not a worktree on {branch}",
                path.display()
            )),
        };
    }
    manager::attach(repo, path, branch)
        .map(|handle| handle.path)
        .map_err(|err| err.to_string())
}

/// Remove the worktree at `path` through the cleanup owner, which commits its
/// uncommitted work onto its branch first (Invariant I-8).
fn remove_worktree(
    repo: &Path,
    path: &Path,
    delete_branch: Option<String>,
) -> Result<PathBuf, String> {
    let summary = cleanup::run_blocking(&CleanupJob {
        repo_root: repo.to_path_buf(),
        worktree_paths: if path.exists() {
            vec![path.to_path_buf()]
        } else {
            Vec::new()
        },
        branches_to_delete: delete_branch.into_iter().collect(),
        owners: Vec::new(),
        done: None,
    });
    let failed: Vec<String> = summary
        .failed_worktrees
        .iter()
        .map(|(path, msg)| format!("{}: {msg}", path.display()))
        .chain(
            summary
                .failed_branches
                .iter()
                .map(|(branch, msg)| format!("{branch}: {msg}")),
        )
        .collect();
    if failed.is_empty() {
        Ok(path.to_path_buf())
    } else {
        Err(failed.join("; "))
    }
}

fn spawn_panel(session: SessionId, spec: &SpawnSpec) -> DPanel {
    let mut command = spec.to_command();
    command.cwd = command.cwd.map(home_relative);
    if spec.trust_codex_cwd
        && let Some(cwd) = command.cwd.as_deref()
        && let Err(err) = crate::agent::codex_trust::ensure_trusted(cwd)
    {
        warn!(cwd = %cwd.display(), error = %err, "codex directory-trust pre-grant failed");
    }
    match LocalPty::spawn(&command, spec.cols, spec.rows) {
        Ok(pty) => {
            let mut panel = DPanel::new(session, Some(pty));
            panel.sent = Some(0);
            panel
        }
        Err(err) => {
            let mut panel = DPanel::new(session, None);
            panel.append(
                format!("caucus host: cannot start {}: {err}\r\n", spec.program).as_bytes(),
            );
            panel.life = Life::Exited(Some(err.to_string()));
            panel.sent = Some(0);
            panel
        }
    }
}

/// Resize one row short and back, so the child gets a `SIGWINCH` and redraws.
fn jiggle(panel: &mut DPanel) {
    if let Some(pty) = panel.pty.as_mut() {
        let (cols, rows) = pty.size();
        let _ = pty.resize(cols, rows.saturating_sub(1).max(1));
        let _ = pty.resize(cols, rows);
    }
}

fn push_capped(queue: &mut VecDeque<String>, line: String) {
    if queue.len() >= SIGNAL_BACKLOG {
        queue.pop_front();
        warn!("sub-host signal backlog full; dropped the oldest signal");
    }
    queue.push_back(line);
}

pub(crate) fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is valid for `buf.len()` bytes; gethostname writes at most
    // that many.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "unknown".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{TurnKind, TurnSignal};

    fn frames(rx: &mut mpsc::Receiver<String>) -> Vec<DaemonFrame> {
        let mut out = Vec::new();
        while let Ok(line) = rx.try_recv() {
            out.push(serde_json::from_str(line.trim_end()).unwrap());
        }
        out
    }

    fn daemon_with_client(session: SessionId) -> (Daemon, mpsc::Receiver<String>) {
        let tmp = std::env::temp_dir();
        let (chores_done, _) = mpsc::unbounded_channel();
        let mut daemon = Daemon::new(
            HostPaths {
                state_dir: tmp.join(format!("caucus-host-test-{}", SessionId::new())),
                control_sock: tmp.join("unused.sock"),
                signal_sock: tmp.join("unused-signal.sock"),
            },
            chores_done,
        );
        let (tx, rx) = mpsc::channel(CONNECTION_QUEUE);
        let (kick, _kicked) = oneshot::channel();
        daemon.clients.insert(
            session,
            Client {
                conn: 1,
                tx,
                _kick: kick,
                queued: VecDeque::new(),
            },
        );
        (daemon, rx)
    }

    /// A panel's turn signal goes out only after the output in front of it,
    /// even when the output takes several ticks to drain.
    #[test]
    fn a_panel_signal_waits_for_the_panel_output() {
        let session = SessionId::new();
        let (mut daemon, mut rx) = daemon_with_client(session);
        let id = PanelId::new();
        let mut panel = DPanel::new(session, None);
        let big = vec![b'x'; OUTPUT_CHUNK * (CONNECTION_QUEUE + 3)];
        panel.append(&big);
        panel.sent = Some(0);
        daemon.panels.insert(id, panel);
        daemon.on_signal(&SignalEvent::Turn(TurnSignal::now(
            session,
            id,
            TurnKind::Stop,
            None,
            serde_json::json!({}),
        )));

        daemon.pump(Instant::now());
        let first = frames(&mut rx);
        assert_eq!(first.len(), CONNECTION_QUEUE);
        assert!(
            first
                .iter()
                .all(|f| matches!(f, DaemonFrame::Output { .. })),
            "the signal stays behind output still in the ring"
        );

        daemon.pump(Instant::now());
        let rest = frames(&mut rx);
        assert_eq!(rest.len(), 4, "{rest:?}");
        assert!(
            rest[..3]
                .iter()
                .all(|f| matches!(f, DaemonFrame::Output { .. }))
        );
        let DaemonFrame::Signal { line } = &rest[3] else {
            panic!("expected the signal last, got {:?}", rest[3]);
        };
        assert!(matches!(
            crate::signal::server::ingest(line).unwrap(),
            SignalEvent::Turn(sig) if sig.panel_id == id
        ));
    }

    /// The ring keeps the newest bytes and advances `base` past what it drops.
    #[test]
    fn the_output_ring_keeps_the_newest_bytes() {
        let mut panel = DPanel::new(SessionId::new(), None);
        panel.append(&vec![b'a'; LOG_CAP]);
        panel.append(b"tail");
        assert_eq!(panel.log.len(), LOG_CAP);
        assert_eq!(panel.base, 4);
        assert_eq!(panel.end(), LOG_CAP as u64 + 4);
        assert!(panel.log.iter().rev().take(4).eq(b"liat".iter()));
    }

    /// A hello for a session that already has a connection replaces it and
    /// stops every panel of the session from streaming until it re-attaches.
    #[test]
    fn a_second_hello_replaces_the_connection_and_pauses_streaming() {
        let session = SessionId::new();
        let (mut daemon, _old) = daemon_with_client(session);
        let id = PanelId::new();
        let mut panel = DPanel::new(session, None);
        panel.append(b"before");
        panel.sent = Some(6);
        panel.exit_sent = true;
        daemon.panels.insert(id, panel);

        let (tx, mut rx) = mpsc::channel(CONNECTION_QUEUE);
        let (kick, mut kicked) = oneshot::channel::<()>();
        daemon.on_event(ConnEvent::Hello {
            conn: 2,
            session,
            tx,
            kick,
        });
        assert!(matches!(
            frames(&mut rx).as_slice(),
            [DaemonFrame::Welcome(_), DaemonFrame::Load(_)]
        ));
        assert_eq!(daemon.clients[&session].conn, 2);
        assert!(kicked.try_recv().is_err(), "the new connection stays open");
        let p = &daemon.panels[&id];
        assert_eq!((p.sent, p.exit_sent), (None, false));
        let _ = std::fs::remove_dir_all(&daemon.paths.state_dir);
    }

    /// A worktree request re-sent after a reconnect gets the answer the first
    /// one got: the add finds its worktree, the remove finds it gone.
    #[test]
    fn worktree_requests_repeat_safely() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            run_git(dir, &args).map(|out| out.trim().to_string())
        };
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@example.com"],
            &["config", "user.name", "t"],
            &["commit", "-q", "--allow-empty", "-m", "root"],
            &["branch", "lane"],
        ] {
            git(&repo, args).unwrap();
        }
        let path = repo.join(".caucus").join("worktrees").join("lane");

        assert_eq!(add_worktree(&repo, &path, "lane"), Ok(path.clone()));
        assert_eq!(add_worktree(&repo, &path, "lane"), Ok(path.clone()));
        let err = add_worktree(&repo, &path, "main").unwrap_err();
        assert!(err.contains("not a worktree on main"), "{err}");

        for _ in 0..2 {
            assert_eq!(
                remove_worktree(&repo, &path, Some("lane".into())),
                Ok(path.clone())
            );
        }
        assert!(!path.exists());
        assert!(git(&repo, &["rev-parse", "--verify", "lane"]).is_err());
    }
}
