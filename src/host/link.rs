//! Main's side of a sub-host: one [`HostLink`] per host, one [`RemotePty`] per
//! panel on it.
//!
//! The link keeps a connection to the host's daemon and opens a new one
//! whenever it drops. On every new connection it first re-sends what the
//! daemon may not have — spawns it has not confirmed, kills it has not
//! confirmed, worktree requests it has not answered — then re-attaches each
//! panel from the first byte main has not seen and re-sends its input from
//! the first byte the host has not taken. Frames written while disconnected
//! wait in the outbound queue.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::load::HostLoad;
use super::proto::{
    Bytes, ClientFrame, DaemonFrame, HostInfo, MAX_FRAME_BYTES, PROTO_VERSION, SpawnSpec, encode,
};
use super::{sh_quote, sh_quote_path};
use crate::line_io::{CappedLine, read_capped_line};
use crate::pty::{PtyCommand, PtyError};
use crate::session::id::{PanelId, SessionId};

/// Frames queued toward the host, connected or not, before writes report
/// [`PtyError::WriterFull`].
const OUTBOUND_QUEUE: usize = 1024;
/// Output chunks buffered for one panel before the link stops reading.
const PANEL_QUEUE: usize = 1024;
/// Input a panel keeps to re-send after a drop; more than a connection holds
/// in flight.
const INPUT_REPLAY: usize = 1 << 20;
/// How long a connection has to answer `hello`.
const HANDSHAKE_WAIT: Duration = Duration::from_secs(15);
const MIN_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// How long a connection stays up before its drop restarts the backoff.
const STABLE_LINK: Duration = Duration::from_secs(10);

/// How the link reaches the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// A command whose stdin/stdout reach a daemon — normally
    /// [`Transport::ssh`] running `caucus host bridge`.
    Command {
        program: OsString,
        args: Vec<OsString>,
    },
    /// The daemon's control socket on this machine.
    Socket(PathBuf),
}

impl Transport {
    /// `ssh <destination>` running `<caucus> host bridge` ([`ssh_args`]).
    pub fn ssh(destination: &str, caucus: &str) -> Self {
        Self::Command {
            program: "ssh".into(),
            args: ssh_args(destination, caucus, "host bridge"),
        }
    }
}

/// `ssh`'s arguments for running `<caucus> <subcommand>` on `destination`, in
/// the remote user's login shell so `caucus` resolves on the PATH their
/// profile sets.
pub fn ssh_args(destination: &str, caucus: &str, subcommand: &str) -> Vec<OsString> {
    let run = format!("exec {} {subcommand}", sh_quote_path(caucus));
    let login = format!("exec \"${{SHELL:-/bin/sh}}\" -lc {}", sh_quote(&run));
    let remote = format!("sh -c {}", sh_quote(&login));
    let args = [
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "ServerAliveCountMax=3",
        "--",
        destination,
        &remote,
    ];
    args.into_iter().map(OsString::from).collect()
}

/// Where the link is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkStatus {
    /// Not connected; `last_error` says why the last attempt ended.
    Connecting {
        last_error: Option<String>,
    },
    Connected(HostInfo),
    /// [`HostLink::close`] ran; the link will not reconnect.
    Closed,
}

#[derive(Debug, Error)]
pub enum LinkError {
    #[error("host {host} did not connect within {waited:?}{}", detail(.last_error))]
    Timeout {
        host: String,
        waited: Duration,
        last_error: Option<String>,
    },
    #[error("host {host}: link closed")]
    Closed { host: String },
}

fn detail(last_error: &Option<String>) -> String {
    last_error
        .as_ref()
        .map(|e| format!(": {e}"))
        .unwrap_or_default()
}

/// A turn-signal line a panel on `host` posted, as the daemon parsed it.
/// It parses like a line from the local signal socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSignal {
    pub host: String,
    pub line: String,
}

/// A connection to one sub-host daemon, kept alive across drops. Cheap to
/// clone; every clone is the same link.
#[derive(Clone)]
pub struct HostLink {
    inner: Arc<Inner>,
}

struct Inner {
    name: String,
    state: Mutex<LinkState>,
    status: watch::Sender<LinkStatus>,
    outbound: mpsc::Sender<ClientFrame>,
    /// Wakes the connection's writer for a frame kept in [`LinkState`]
    /// rather than queued: a kill or a signal confirmation.
    owed_ready: Notify,
    stop: watch::Sender<bool>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Default)]
struct LinkState {
    panels: HashMap<PanelId, LinkPanel>,
    /// Panels of this session the host may still have that main will adopt.
    /// Any other the host lists and `panels` lacks is killed on the welcome.
    adopting: HashSet<PanelId>,
    /// Panels killed but not yet confirmed by the daemon, and whether the
    /// current connection has carried each kill. Only the connection's writer
    /// sends a kill, never the bounded queue, so none can be refused.
    kills: HashMap<PanelId, Kill>,
    /// Worktree requests the daemon has not answered, by id. Dropping one
    /// (when the link closes) answers its caller with "link closed".
    requests: BTreeMap<u64, Request>,
    next_req: u64,
    /// The host's load as of the current connection's latest report.
    load: Option<HostLoad>,
    /// Each panel the latest welcome listed, with the first input byte the
    /// host had not taken.
    taken: HashMap<PanelId, u64>,
    /// The next signal of the link itself main has not seen.
    signals_next: u64,
    /// Signal confirmations the current connection has yet to carry, the
    /// latest per panel (`None`: the link's own). Like kills, only the
    /// writer sends them, so a full queue cannot refuse one.
    acks: HashMap<Option<PanelId>, u64>,
    closed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kill {
    Unsent,
    Sent,
}

struct LinkPanel {
    /// The spawn, until the daemon confirms it.
    unconfirmed: Option<SpawnSpec>,
    /// First output byte main has not seen.
    next: u64,
    size: (u16, u16),
    exited: bool,
    out: mpsc::Sender<Chunk>,
    /// The latest input, kept to re-send what a dropped connection lost;
    /// `input[0]` is input byte `input_base`, counted from the spawn.
    input: VecDeque<u8>,
    input_base: u64,
    /// The next signal of this panel main has not seen.
    signals_next: u64,
}

impl LinkPanel {
    fn input_end(&self) -> u64 {
        self.input_base + self.input.len() as u64
    }

    /// Forget input before `upto`, which the host has taken.
    fn drop_input_before(&mut self, upto: u64) {
        let taken = (upto.saturating_sub(self.input_base) as usize).min(self.input.len());
        self.input.drain(..taken);
        self.input_base += taken as u64;
    }
}

struct Request {
    frame: ClientFrame,
    done: oneshot::Sender<Result<String, String>>,
}

/// What the link hands a [`RemotePty`], in order.
enum Chunk {
    Data(Vec<u8>),
    Exited(Option<String>),
}

impl HostLink {
    /// Start connecting to `name` over `transport` as `session`. Must run
    /// inside a tokio runtime. Signals from the host's panels go to `signals`.
    /// Of the panels the host already has for the session, those in
    /// `adopting` are left for [`HostLink::adopt`] and every other is killed.
    pub fn connect(
        name: impl Into<String>,
        session: SessionId,
        transport: Transport,
        signals: mpsc::UnboundedSender<RemoteSignal>,
        adopting: HashSet<PanelId>,
    ) -> Self {
        let (outbound, rx) = mpsc::channel(OUTBOUND_QUEUE);
        let inner = Arc::new(Inner {
            name: name.into(),
            state: Mutex::new(LinkState {
                adopting,
                ..LinkState::default()
            }),
            status: watch::Sender::new(LinkStatus::Connecting { last_error: None }),
            outbound,
            owed_ready: Notify::new(),
            stop: watch::Sender::new(false),
            supervisor: Mutex::new(None),
        });
        let task = tokio::spawn(supervise(
            Arc::clone(&inner),
            session,
            transport,
            rx,
            signals,
        ));
        *inner.supervisor.lock().expect("supervisor slot") = Some(task);
        Self { inner }
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn status(&self) -> LinkStatus {
        self.inner.status.borrow().clone()
    }

    /// The host's latest load report; `None` while not connected.
    pub fn load(&self) -> Option<HostLoad> {
        self.inner.lock().load.clone()
    }

    /// Wait until the link is connected and return what the host said about
    /// itself.
    pub async fn wait_connected(&self, timeout: Duration) -> Result<HostInfo, LinkError> {
        let mut status = self.inner.status.subscribe();
        let ready = status.wait_for(|s| !matches!(s, LinkStatus::Connecting { .. }));
        match tokio::time::timeout(timeout, ready).await {
            Ok(Ok(status)) => match &*status {
                LinkStatus::Connected(info) => Ok(info.clone()),
                _ => Err(self.closed()),
            },
            Ok(Err(_)) => Err(self.closed()),
            Err(_) => Err(LinkError::Timeout {
                host: self.inner.name.clone(),
                waited: timeout,
                last_error: match self.status() {
                    LinkStatus::Connecting { last_error } => last_error,
                    _ => None,
                },
            }),
        }
    }

    fn closed(&self) -> LinkError {
        LinkError::Closed {
            host: self.inner.name.clone(),
        }
    }

    /// Start `command` on the host as `panel`. The spawn is queued; a link that
    /// is down sends it when it reconnects.
    pub fn spawn(
        &self,
        panel: PanelId,
        command: &PtyCommand,
        cols: u16,
        rows: u16,
        trust_codex_cwd: bool,
    ) -> Result<RemotePty, PtyError> {
        let spec = SpawnSpec {
            trust_codex_cwd,
            ..SpawnSpec::from_command(command, cols, rows).map_err(PtyError::Spawn)?
        };
        self.open(
            panel,
            (cols, rows),
            Some(spec.clone()),
            0,
            [ClientFrame::Spawn { panel, spec }],
        )
    }

    /// Take over `panel`, which the host still runs for this session from
    /// before main restarted and [`HostLink::connect`] was told to adopt,
    /// replaying its output from the first byte the host kept.
    pub fn adopt(&self, panel: PanelId, cols: u16, rows: u16) -> Result<RemotePty, PtyError> {
        let taken = self.inner.lock().taken.get(&panel).copied().unwrap_or(0);
        self.open(
            panel,
            (cols, rows),
            None,
            taken,
            [
                ClientFrame::Attach { panel, from: 0 },
                ClientFrame::Resize { panel, cols, rows },
            ],
        )
    }

    fn open<const N: usize>(
        &self,
        panel: PanelId,
        size: (u16, u16),
        unconfirmed: Option<SpawnSpec>,
        input_base: u64,
        frames: [ClientFrame; N],
    ) -> Result<RemotePty, PtyError> {
        let (out, rx) = mpsc::channel(PANEL_QUEUE);
        let adopting = {
            let mut state = self.inner.lock();
            if state.closed {
                return Err(PtyError::Spawn(format!(
                    "host {}: link closed",
                    self.name()
                )));
            }
            let adopting = state.adopting.remove(&panel);
            state.panels.insert(
                panel,
                LinkPanel {
                    unconfirmed,
                    next: 0,
                    size,
                    exited: false,
                    out,
                    input: VecDeque::new(),
                    input_base,
                    signals_next: 0,
                },
            );
            adopting
        };
        for frame in frames {
            if let Err(err) = self.send(frame) {
                let mut state = self.inner.lock();
                state.panels.remove(&panel);
                if adopting {
                    state.adopting.insert(panel);
                }
                return Err(err);
            }
        }
        Ok(RemotePty {
            link: self.clone(),
            panel,
            rx,
            size,
            exited: false,
            killed: false,
        })
    }

    fn send(&self, frame: ClientFrame) -> Result<(), PtyError> {
        self.inner
            .outbound
            .try_send(frame)
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(_) => PtyError::WriterFull,
                mpsc::error::TrySendError::Closed(_) => PtyError::WriterClosed,
            })
    }

    /// Send `bytes` as `panel`'s next input, kept to re-send after a drop.
    fn input(&self, panel: PanelId, bytes: &[u8]) -> Result<(), PtyError> {
        let mut state = self.inner.lock();
        let p = state.panels.get_mut(&panel).ok_or(PtyError::WriterClosed)?;
        self.send(ClientFrame::Input {
            panel,
            offset: p.input_end(),
            data: Bytes(bytes.to_vec()),
        })?;
        p.input.extend(bytes);
        let over = p.input.len().saturating_sub(INPUT_REPLAY) as u64;
        p.drop_input_before(p.input_base + over);
        Ok(())
    }

    fn resize(&self, panel: PanelId, cols: u16, rows: u16) -> Result<(), PtyError> {
        if let Some(p) = self.inner.lock().panels.get_mut(&panel) {
            p.size = (cols, rows);
        }
        self.send(ClientFrame::Resize { panel, cols, rows })
    }

    /// Kill `panel` on the host. Re-sent on every reconnect until the daemon
    /// confirms it, so a drop cannot leave the panel running there.
    pub(crate) fn kill(&self, panel: PanelId) {
        {
            let mut state = self.inner.lock();
            state.panels.remove(&panel);
            state.adopting.remove(&panel);
            if state.closed {
                return;
            }
            state.kills.entry(panel).or_insert(Kill::Unsent);
        }
        self.inner.owed_ready.notify_one();
    }

    /// Check out the existing `branch` at `path` as a worktree of the host's
    /// repo at `repo` (both may start with `~`, the host user's home) and
    /// return its absolute path there. Waits through reconnects; fails only
    /// on the host's answer or when the link closes.
    pub async fn add_worktree(
        &self,
        repo: &Path,
        path: &Path,
        branch: &str,
    ) -> Result<PathBuf, String> {
        let (repo, path) = (utf8_path(repo)?, utf8_path(path)?);
        let branch = branch.to_string();
        self.request(|req| ClientFrame::AddWorktree {
            req,
            repo,
            path,
            branch,
        })
        .await
        .map(PathBuf::from)
    }

    /// Remove the host's worktree at `path` (its uncommitted work committed
    /// onto its branch first), then delete `delete_branch` if given. Runs on
    /// the host after every kill sent before it.
    pub async fn remove_worktree(
        &self,
        repo: &Path,
        path: &Path,
        delete_branch: Option<&str>,
    ) -> Result<(), String> {
        let (repo, path) = (utf8_path(repo)?, utf8_path(path)?);
        let delete_branch = delete_branch.map(str::to_string);
        self.request(|req| ClientFrame::RemoveWorktree {
            req,
            repo,
            path,
            delete_branch,
        })
        .await
        .map(drop)
    }

    async fn request(&self, frame: impl FnOnce(u64) -> ClientFrame) -> Result<String, String> {
        let (done, answer) = oneshot::channel();
        let frame = {
            let mut state = self.inner.lock();
            if state.closed {
                return Err(self.closed().to_string());
            }
            let req = state.next_req;
            state.next_req += 1;
            let frame = frame(req);
            state.requests.insert(
                req,
                Request {
                    frame: frame.clone(),
                    done,
                },
            );
            frame
        };
        // Waiting for room here, unlike `send`, drops nothing.
        let _ = self.inner.outbound.send(frame).await;
        answer
            .await
            .unwrap_or_else(|_| Err(self.closed().to_string()))
    }

    /// Send what is queued (kills and worktree requests included) for up to
    /// `timeout`, then stop. Panels still on the link see their output end,
    /// and unanswered requests fail.
    pub async fn close(&self, timeout: Duration) {
        let drained = async {
            loop {
                let idle = self.inner.outbound.capacity() == OUTBOUND_QUEUE && {
                    let state = self.inner.lock();
                    state.kills.is_empty() && state.requests.is_empty()
                };
                if idle || *self.inner.stop.borrow() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let _ = tokio::time::timeout(timeout, drained).await;
        self.inner.stop.send_replace(true);
        let task = self
            .inner
            .supervisor
            .lock()
            .expect("supervisor slot")
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, LinkState> {
        self.state.lock().expect("host link state poisoned")
    }

    /// What a new connection sends before anything queued.
    fn reattach_frames(&self) -> Vec<ClientFrame> {
        let mut state = self.lock();
        let LinkState {
            panels,
            kills,
            requests,
            taken,
            acks,
            ..
        } = &mut *state;
        let mut frames = Vec::new();
        for (&panel, p) in panels.iter_mut().filter(|(_, p)| !p.exited) {
            if let Some(spec) = &p.unconfirmed {
                frames.push(ClientFrame::Spawn {
                    panel,
                    spec: spec.clone(),
                });
            }
            frames.push(ClientFrame::Attach {
                panel,
                from: p.next,
            });
            frames.push(ClientFrame::Resize {
                panel,
                cols: p.size.0,
                rows: p.size.1,
            });
            // A panel the welcome does not list has taken none of its input
            // yet, or is gone and drops it.
            if let Some(&upto) = taken.get(&panel) {
                p.drop_input_before(upto);
            }
            if !p.input.is_empty() {
                frames.push(ClientFrame::Input {
                    panel,
                    offset: p.input_base,
                    data: Bytes(p.input.iter().copied().collect()),
                });
            }
        }
        for (&panel, kill) in kills.iter_mut() {
            *kill = Kill::Sent;
            frames.push(ClientFrame::Kill { panel });
        }
        frames.extend(take_acks(acks));
        frames.extend(requests.values().map(|r| r.frame.clone()));
        frames
    }

    /// The kills and signal confirmations the current connection has yet to
    /// carry, now counted as carried.
    fn owed_frames(&self) -> Vec<ClientFrame> {
        let mut state = self.lock();
        let mut frames = Vec::new();
        for (&panel, kill) in &mut state.kills {
            if *kill == Kill::Unsent {
                *kill = Kill::Sent;
                frames.push(ClientFrame::Kill { panel });
            }
        }
        frames.extend(take_acks(&mut state.acks));
        frames
    }

    /// Whether a queued frame still means anything. A spawn queued before a
    /// drop and confirmed (or killed) since must not start the panel again,
    /// and an answered worktree request must not run again.
    fn still_wanted(&self, frame: &ClientFrame) -> bool {
        match frame {
            ClientFrame::Spawn { panel, .. } => self
                .lock()
                .panels
                .get(panel)
                .is_some_and(|p| p.unconfirmed.is_some()),
            ClientFrame::AddWorktree { req, .. } | ClientFrame::RemoveWorktree { req, .. } => {
                self.lock().requests.contains_key(req)
            }
            _ => true,
        }
    }

    async fn on_frame(&self, frame: DaemonFrame, signals: &mpsc::UnboundedSender<RemoteSignal>) {
        match frame {
            DaemonFrame::Welcome(_) => warn!(host = %self.name, "unexpected second welcome"),
            DaemonFrame::Load(load) => self.lock().load = Some(load),
            DaemonFrame::Spawned { panel } => {
                if let Some(p) = self.lock().panels.get_mut(&panel) {
                    p.unconfirmed = None;
                }
            }
            DaemonFrame::Output {
                panel,
                offset,
                data,
            } => {
                let deliver = {
                    let mut state = self.lock();
                    let Some(p) = state.panels.get_mut(&panel) else {
                        return;
                    };
                    let end = offset + data.0.len() as u64;
                    if end <= p.next {
                        return;
                    }
                    if offset > p.next {
                        warn!(host = %self.name, %panel, lost = offset - p.next, "sub-host output gap");
                    }
                    let skip = p.next.saturating_sub(offset) as usize;
                    p.next = end;
                    (p.out.clone(), skip)
                };
                let (out, skip) = deliver;
                let mut bytes = data.0;
                bytes.drain(..skip);
                let _ = out.send(Chunk::Data(bytes)).await;
            }
            DaemonFrame::Skipped { panel, to, modes } => {
                let out = {
                    let mut state = self.lock();
                    let Some(p) = state.panels.get_mut(&panel) else {
                        return;
                    };
                    if to <= p.next {
                        return;
                    }
                    warn!(host = %self.name, %panel, lost = to - p.next, "sub-host output dropped unread");
                    p.next = to;
                    p.out.clone()
                };
                // The grid takes the modes as the dropped bytes would have
                // set them.
                let replay: String = modes
                    .iter()
                    .map(|&(mode, on)| format!("\x1b[?{mode}{}", if on { 'h' } else { 'l' }))
                    .collect();
                let _ = out.send(Chunk::Data(replay.into_bytes())).await;
            }
            DaemonFrame::Exited { panel, reason } => {
                let out = {
                    let mut state = self.lock();
                    state.kills.remove(&panel);
                    state.panels.get_mut(&panel).and_then(|p| {
                        (!p.exited).then(|| {
                            p.exited = true;
                            p.out.clone()
                        })
                    })
                };
                if let Some(out) = out {
                    let _ = out.send(Chunk::Exited(reason)).await;
                }
            }
            DaemonFrame::Signal { panel, seq, line } => {
                let fresh = {
                    let mut state = self.lock();
                    let next = match panel {
                        Some(panel) => state.panels.get_mut(&panel).map(|p| &mut p.signals_next),
                        None => Some(&mut state.signals_next),
                    };
                    match next {
                        Some(next) if seq < *next => false,
                        Some(next) => {
                            *next = seq + 1;
                            true
                        }
                        // A panel main has let go of; the host forgets it too.
                        None => true,
                    }
                };
                if fresh {
                    let _ = signals.send(RemoteSignal {
                        host: self.name.clone(),
                        line,
                    });
                }
                {
                    let mut state = self.lock();
                    let ack = state.acks.entry(panel).or_default();
                    *ack = (*ack).max(seq + 1);
                }
                self.owed_ready.notify_one();
            }
            DaemonFrame::WorktreeDone { req, result } => {
                if let Some(request) = self.lock().requests.remove(&req) {
                    let _ = request.done.send(result);
                }
            }
            DaemonFrame::Error { msg } => {
                warn!(host = %self.name, %msg, "sub-host reported an error")
            }
        }
    }
}

/// Keep a connection up until [`HostLink::close`].
async fn supervise(
    inner: Arc<Inner>,
    session: SessionId,
    transport: Transport,
    mut outbound: mpsc::Receiver<ClientFrame>,
    signals: mpsc::UnboundedSender<RemoteSignal>,
) {
    let mut stop = inner.stop.subscribe();
    let mut backoff = MIN_BACKOFF;
    // A frame whose write failed, sent first on the next connection.
    let mut retry = None;
    loop {
        let attempt = connection(
            &inner,
            session,
            &transport,
            &mut outbound,
            &mut retry,
            &signals,
        );
        let ended = tokio::select! {
            ended = attempt => ended,
            _ = stop.wait_for(|stop| *stop) => break,
        };
        let last_error = match ended {
            Ended::Dropped { connected } => {
                // A host that hangs up soon after its welcome is failing, and
                // redialling it every MIN_BACKOFF would never settle.
                if connected.elapsed() >= STABLE_LINK {
                    backoff = MIN_BACKOFF;
                }
                info!(host = %inner.name, "sub-host link dropped; reconnecting");
                "link dropped".to_string()
            }
            Ended::Failed(err) => {
                warn!(host = %inner.name, error = %err, "sub-host connect failed");
                err
            }
        };
        inner.lock().load = None;
        inner.status.send_replace(LinkStatus::Connecting {
            last_error: Some(last_error),
        });
        tokio::select! {
            () = tokio::time::sleep(backoff) => {}
            _ = stop.wait_for(|stop| *stop) => break,
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
    let mut state = inner.lock();
    state.closed = true;
    state.kills.clear();
    state.requests.clear();
    state.load = None;
    // Dropping each panel's sender ends its output.
    state.panels.clear();
    drop(state);
    inner.status.send_replace(LinkStatus::Closed);
}

enum Ended {
    /// Got a welcome at `connected`, then lost the connection.
    Dropped { connected: Instant },
    /// Never got a welcome.
    Failed(String),
}

struct Conn {
    reader: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    /// The transport process, killed when the connection is dropped.
    _child: Option<tokio::process::Child>,
    /// The transport's last stderr line (ssh says why it failed there).
    stderr: Arc<Mutex<Option<String>>>,
}

async fn open(transport: &Transport) -> std::io::Result<Conn> {
    match transport {
        Transport::Socket(path) => {
            let (read, write) = tokio::net::UnixStream::connect(path).await?.into_split();
            Ok(Conn {
                reader: BufReader::new(Box::new(read)),
                writer: Box::new(write),
                _child: None,
                stderr: Arc::default(),
            })
        }
        Transport::Command { program, args } => {
            let mut child = tokio::process::Command::new(program)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let stdin = child.stdin.take().expect("piped stdin");
            let stdout = child.stdout.take().expect("piped stdout");
            let stderr_tail = Arc::new(Mutex::new(None));
            if let Some(stderr) = child.stderr.take() {
                tokio::spawn(keep_stderr_tail(stderr, Arc::clone(&stderr_tail)));
            }
            Ok(Conn {
                reader: BufReader::new(Box::new(stdout)),
                writer: Box::new(stdin),
                _child: Some(child),
                stderr: stderr_tail,
            })
        }
    }
}

async fn keep_stderr_tail(stderr: tokio::process::ChildStderr, tail: Arc<Mutex<Option<String>>>) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    while let Ok(CappedLine::Line) = read_capped_line(&mut reader, &mut line, 64 * 1024).await {
        if !line.trim().is_empty() {
            warn!(stderr = %line, "sub-host transport");
            *tail.lock().expect("stderr tail") = Some(line.clone());
        }
    }
}

/// One connection: handshake, re-attach, then relay until it drops.
async fn connection(
    inner: &Inner,
    session: SessionId,
    transport: &Transport,
    outbound: &mut mpsc::Receiver<ClientFrame>,
    retry: &mut Option<ClientFrame>,
    signals: &mpsc::UnboundedSender<RemoteSignal>,
) -> Ended {
    let mut conn = match open(transport).await {
        Ok(conn) => conn,
        Err(err) => return Ended::Failed(err.to_string()),
    };
    let hello = handshake(&mut conn, session, &inner.name);
    let info = match tokio::time::timeout(HANDSHAKE_WAIT, hello).await {
        Ok(Ok(info)) => info,
        Ok(Err(err)) => {
            // Let a failing transport finish printing why.
            tokio::time::sleep(Duration::from_millis(50)).await;
            let stderr = conn.stderr.lock().expect("stderr tail").clone();
            return Ended::Failed(match stderr {
                Some(stderr) => format!("{err}: {stderr}"),
                None => err,
            });
        }
        Err(_) => return Ended::Failed(format!("no welcome within {HANDSHAKE_WAIT:?}")),
    };
    let connected = Instant::now();
    info!(host = %inner.name, hostname = %info.hostname, version = %info.version, "sub-host connected");
    {
        let mut state = inner.lock();
        let LinkState {
            panels,
            adopting,
            kills,
            taken,
            ..
        } = &mut *state;
        *taken = info.panels.iter().map(|p| (p.id, p.input)).collect();
        // Sent with the re-attach below.
        for p in &info.panels {
            if !panels.contains_key(&p.id) && !adopting.contains(&p.id) {
                kills.entry(p.id).or_insert(Kill::Unsent);
            }
        }
    }
    inner.status.send_replace(LinkStatus::Connected(info));

    let mut first = inner.reattach_frames();
    first.extend(retry.take());
    for frame in &first {
        if conn
            .writer
            .write_all(encode(frame).as_bytes())
            .await
            .is_err()
        {
            return Ended::Dropped { connected };
        }
    }
    if conn.writer.flush().await.is_err() {
        return Ended::Dropped { connected };
    }
    let Conn { reader, writer, .. } = &mut conn;
    tokio::select! {
        () = read_frames(inner, reader, signals) => {}
        () = write_frames(inner, writer, outbound, retry) => {}
    }
    Ended::Dropped { connected }
}

async fn handshake(conn: &mut Conn, session: SessionId, host: &str) -> Result<HostInfo, String> {
    let hello = encode(&ClientFrame::Hello {
        proto: PROTO_VERSION,
        session,
        host: host.to_string(),
    });
    conn.writer
        .write_all(hello.as_bytes())
        .await
        .map_err(|e| format!("send hello: {e}"))?;
    conn.writer
        .flush()
        .await
        .map_err(|e| format!("send hello: {e}"))?;
    // The host user's login profile may print ahead of the bridge; whatever
    // is not a frame before the welcome is its output, skipped.
    let mut line = String::new();
    let mut printed = None;
    let frame = loop {
        match read_capped_line(&mut conn.reader, &mut line, MAX_FRAME_BYTES).await {
            Ok(CappedLine::Line) => {}
            Ok(CappedLine::TooLong) => continue,
            Ok(CappedLine::Eof) => {
                return Err(match printed {
                    Some(printed) => format!("closed before welcome, after printing {printed:?}"),
                    None => "closed before welcome".into(),
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::InvalidData => continue,
            Err(err) => return Err(format!("read welcome: {err}")),
        }
        match serde_json::from_str::<DaemonFrame>(&line) {
            Ok(frame) => break frame,
            Err(_) if line.trim().is_empty() => {}
            Err(_) => printed = Some(std::mem::take(&mut line)),
        }
    };
    match frame {
        DaemonFrame::Welcome(info) if info.proto == PROTO_VERSION => Ok(info),
        DaemonFrame::Welcome(info) => Err(format!(
            "host speaks protocol {}, this caucus {PROTO_VERSION}",
            info.proto
        )),
        DaemonFrame::Error { msg } => Err(msg),
        other => Err(format!("expected welcome, got {other:?}")),
    }
}

async fn read_frames<R: AsyncBufRead + Unpin>(
    inner: &Inner,
    reader: &mut R,
    signals: &mpsc::UnboundedSender<RemoteSignal>,
) {
    let mut line = String::new();
    loop {
        match read_capped_line(reader, &mut line, MAX_FRAME_BYTES).await {
            Ok(CappedLine::Line) if line.is_empty() => {}
            Ok(CappedLine::Line) => match serde_json::from_str::<DaemonFrame>(&line) {
                Ok(frame) => inner.on_frame(frame, signals).await,
                Err(err) => warn!(host = %inner.name, error = %err, "unreadable sub-host frame"),
            },
            Ok(CappedLine::TooLong) => {
                warn!(host = %inner.name, "sub-host frame over {MAX_FRAME_BYTES} bytes");
                return;
            }
            Ok(CappedLine::Eof) | Err(_) => return,
        }
    }
}

async fn write_frames<W: AsyncWrite + Unpin>(
    inner: &Inner,
    writer: &mut W,
    outbound: &mut mpsc::Receiver<ClientFrame>,
    retry: &mut Option<ClientFrame>,
) {
    loop {
        let queued = tokio::select! {
            biased;
            () = inner.owed_ready.notified() => None,
            frame = outbound.recv() => match frame {
                Some(frame) => Some(frame),
                None => return,
            },
        };
        // Kills go out ahead of the frame queued after them: a worktree
        // request must not run while a panel being killed still works there.
        // One whose write fails is re-sent by the next connection; a lost
        // confirmation is made good when the host sends the signal again.
        for owed in inner.owed_frames() {
            if writer.write_all(encode(&owed).as_bytes()).await.is_err() {
                *retry = queued.filter(|frame| inner.still_wanted(frame));
                return;
            }
        }
        if let Some(frame) = queued.filter(|frame| inner.still_wanted(frame))
            && writer.write_all(encode(&frame).as_bytes()).await.is_err()
        {
            *retry = Some(frame);
            return;
        }
        if outbound.is_empty() && writer.flush().await.is_err() {
            return;
        }
    }
}

fn take_acks(acks: &mut HashMap<Option<PanelId>, u64>) -> impl Iterator<Item = ClientFrame> {
    acks.drain()
        .map(|(panel, next)| ClientFrame::SignalsSeen { panel, next })
}

fn utf8_path(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| format!("not valid UTF-8: {}", path.display()))
}

/// A panel's terminal on a sub-host, driven through its host's [`HostLink`].
pub struct RemotePty {
    link: HostLink,
    panel: PanelId,
    rx: mpsc::Receiver<Chunk>,
    size: (u16, u16),
    /// Set only once `read` has handed out every byte before the exit, so a
    /// caller that reaps on `!is_alive()` never drops the final output.
    exited: bool,
    killed: bool,
}

impl RemotePty {
    pub fn host(&self) -> &str {
        self.link.name()
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    pub(crate) fn read(&mut self) -> Result<Vec<u8>, PtyError> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(Chunk::Data(bytes)) => out.extend_from_slice(&bytes),
                Ok(Chunk::Exited(reason)) => {
                    if let Some(reason) = reason {
                        info!(host = %self.host(), panel = %self.panel, %reason, "sub-host panel exited");
                    }
                    self.exited = true;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                // The link closed: nothing more will arrive.
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.exited = true;
                    break;
                }
            }
        }
        Ok(out)
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<(), PtyError> {
        if self.exited || self.killed {
            return Err(PtyError::WriterClosed);
        }
        self.link.input(self.panel, bytes)
    }

    pub(crate) fn is_alive(&mut self) -> bool {
        !self.exited && !self.killed
    }

    pub(crate) fn resize(&mut self, cols: u16, rows: u16) -> Result<(), PtyError> {
        self.size = (cols, rows);
        if self.exited || self.killed {
            return Ok(());
        }
        self.link.resize(self.panel, cols, rows)
    }

    pub(crate) fn kill(&mut self) -> Result<(), PtyError> {
        if !self.killed {
            self.killed = true;
            self.link.kill(self.panel);
        }
        Ok(())
    }
}

impl Drop for RemotePty {
    /// Like a local PTY, a dropped remote one takes its process with it.
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_transport_runs_the_bridge_through_a_login_shell() {
        let Transport::Command { program, args } = Transport::ssh("build-1", "caucus") else {
            panic!("ssh is a command transport");
        };
        assert_eq!(program, "ssh");
        let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            &args[..8],
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ServerAliveInterval=10",
                "-o",
                "ServerAliveCountMax=3",
                "--"
            ]
        );
        assert_eq!(args[8], "build-1", "the destination is never an option");
        // The remote command, run by `sh` here the way sshd would, reaches
        // the bridge invocation intact.
        let remote = args[9].replace("exec \"${SHELL:-/bin/sh}\" -lc", "printf %s");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&remote)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "exec 'caucus' host bridge"
        );
    }

    #[test]
    fn a_caucus_path_under_home_is_found_on_the_host() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("my bin");
        std::fs::create_dir(&bin).unwrap();
        let caucus = bin.join("caucus");
        std::fs::write(&caucus, "#!/bin/sh\nprintf 'ran %s' \"$*\"\n").unwrap();
        std::fs::set_permissions(&caucus, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let Transport::Command { args, .. } = Transport::ssh("h", "~/my bin/caucus") else {
            panic!("ssh is a command transport");
        };
        // A plain shell stands in for the login shell, whose profile is the
        // host's business.
        let remote = args[9]
            .to_str()
            .unwrap()
            .replace("exec \"${SHELL:-/bin/sh}\" -lc", "exec /bin/sh -c");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&remote)
            .env("HOME", home.path())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "ran host bridge",
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
