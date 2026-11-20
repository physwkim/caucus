//! A daemon and a link in one process, over real sockets and real PTYs.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use super::proto::{ClientFrame, DaemonFrame, PROTO_VERSION, encode};
use super::{
    HostInfo, HostLink, HostPaths, LinkStatus, RemotePty, RemoteSignal, Transport, daemon,
};
use crate::pty::{PtyCommand, PtyError};
use crate::session::id::{PanelId, SessionId};
use crate::signal::server::ingest;
use crate::signal::{SignalEvent, TurnKind, TurnSignal};

const WAIT: Duration = Duration::from_secs(10);

pub(crate) struct TestDaemon {
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<anyhow::Result<()>>>,
}

impl TestDaemon {
    pub(crate) async fn start(paths: &HostPaths) -> Self {
        let lock = daemon::lock(paths).unwrap().expect("no other daemon");
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(daemon::run(paths.clone(), lock, async {
            let _ = stopped.await;
        }));
        let deadline = Instant::now() + WAIT;
        while UnixStream::connect(&paths.control_sock).await.is_err() {
            assert!(Instant::now() < deadline, "daemon never listened");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Self {
            stop: Some(stop),
            task: Some(task),
        }
    }

    pub(crate) async fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.unwrap().unwrap();
        }
    }
}

pub(crate) fn test_paths(tmp: &tempfile::TempDir) -> HostPaths {
    HostPaths {
        state_dir: tmp.path().join("state"),
        control_sock: tmp.path().join("ctl").join("daemon.sock"),
        signal_sock: tmp.path().join("signal.sock"),
    }
}

fn sh(script: &str) -> PtyCommand {
    let mut command = PtyCommand::new("/bin/sh");
    command.args = vec!["-c".into(), script.into()];
    command
}

/// Read from `pty` into `out` until `needle` shows up.
async fn read_until(pty: &mut RemotePty, out: &mut String, needle: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        out.push_str(&String::from_utf8_lossy(&pty.read().unwrap()));
        if out.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "never saw {needle:?}; got {out:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_status(link: &HostLink, want: impl Fn(&LinkStatus) -> bool) {
    let deadline = Instant::now() + WAIT;
    while !want(&link.status()) {
        assert!(
            Instant::now() < deadline,
            "status stuck at {:?}",
            link.status()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn link(
    paths: &HostPaths,
    session: SessionId,
) -> (HostLink, mpsc::UnboundedReceiver<RemoteSignal>) {
    resumed_link(paths, session, HashSet::new())
}

/// [`link`] for a main that adopts `adopting`.
fn resumed_link(
    paths: &HostPaths,
    session: SessionId,
    adopting: HashSet<PanelId>,
) -> (HostLink, mpsc::UnboundedReceiver<RemoteSignal>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let link = HostLink::connect(
        "sub-1",
        session,
        Transport::Socket(paths.control_sock.clone()),
        tx,
        adopting,
    );
    (link, rx)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_panel_echoes_input_and_hands_over_its_last_output_before_it_exits() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp);
    let mut host = TestDaemon::start(&paths).await;
    let session = SessionId::new();
    let (link, _signals) = link(&paths, session);
    let info = link.wait_connected(WAIT).await.unwrap();
    assert_eq!(info.signal_sock, paths.signal_sock);
    assert_eq!(
        info.session_dir,
        paths.state_dir.join("sessions").join(session.to_string())
    );
    assert!(info.session_dir.is_dir());

    let mut pty = link
        .spawn(
            PanelId::new(),
            &sh("read line; printf 'last-%s' \"$line\""),
            80,
            24,
            false,
        )
        .unwrap();
    assert_eq!(pty.host(), "sub-1");
    pty.write(b"ping\n").unwrap();
    let mut out = String::new();
    let deadline = Instant::now() + WAIT;
    while pty.is_alive() {
        out.push_str(&String::from_utf8_lossy(&pty.read().unwrap()));
        assert!(Instant::now() < deadline, "panel never exited; got {out:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        out.contains("last-ping"),
        "the output before the exit was handed over first: {out:?}"
    );
    assert!(matches!(
        pty.write(b"x"),
        Err(crate::pty::PtyError::WriterClosed)
    ));

    link.close(WAIT).await;
    host.stop().await;
}

/// The daemon reports its load right after the welcome; the link drops the
/// report with the connection, so a stale load never outlives it.
#[tokio::test(flavor = "multi_thread")]
async fn the_link_holds_the_hosts_load_only_while_connected() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp);
    let mut host = TestDaemon::start(&paths).await;
    let (link, _signals) = link(&paths, SessionId::new());
    link.wait_connected(WAIT).await.unwrap();
    let deadline = Instant::now() + WAIT;
    while link.load().is_none() {
        assert!(
            Instant::now() < deadline,
            "no load report after the welcome"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(link.load().unwrap().cpus >= 1);

    host.stop().await;
    wait_status(&link, |s| matches!(s, LinkStatus::Connecting { .. })).await;
    assert_eq!(link.load(), None);
    link.close(WAIT).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn killing_a_remote_panel_kills_its_process_on_the_host() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp);
    let mut host = TestDaemon::start(&paths).await;
    let (link, _signals) = link(&paths, SessionId::new());
    link.wait_connected(WAIT).await.unwrap();

    let mut pty = link
        .spawn(
            PanelId::new(),
            &sh("echo pid=$$; exec sleep 60"),
            80,
            24,
            false,
        )
        .unwrap();
    let mut out = String::new();
    read_until(&mut pty, &mut out, "\n").await;
    let pid: libc::pid_t = out
        .trim()
        .strip_prefix("pid=")
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no pid in {out:?}"));

    pty.kill().unwrap();
    assert!(!pty.is_alive());
    let deadline = Instant::now() + WAIT;
    // SAFETY: signal 0 only probes whether `pid` exists.
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(Instant::now() < deadline, "pid {pid} survived the kill");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    link.close(WAIT).await;
    host.stop().await;
}

/// A socket relay the test can cut and block, standing in for an SSH drop.
/// It can also swallow chosen frame lines, as a connection that dies with
/// them in flight does.
struct Relay {
    path: std::path::PathBuf,
    open: Arc<AtomicBool>,
    cut: broadcast::Sender<()>,
    drops: Arc<Mutex<Drops>>,
}

#[derive(Clone, Copy)]
enum Toward {
    Host,
    Main,
}

/// Lines containing `host` or `main` going that way are swallowed, and kept
/// in `dropped`.
#[derive(Default)]
struct Drops {
    host: Option<&'static str>,
    main: Option<&'static str>,
    dropped: Vec<String>,
}

impl Relay {
    fn start(path: std::path::PathBuf, upstream: std::path::PathBuf) -> Self {
        let listener = UnixListener::bind(&path).unwrap();
        let open = Arc::new(AtomicBool::new(true));
        let (cut, _) = broadcast::channel(4);
        let drops = Arc::new(Mutex::new(Drops::default()));
        let (gate, cuts, all_drops) = (Arc::clone(&open), cut.clone(), Arc::clone(&drops));
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                if !gate.load(Ordering::SeqCst) {
                    continue;
                }
                let mut cut = cuts.subscribe();
                let server = UnixStream::connect(&upstream).await.unwrap();
                let ((from_main, to_main), (from_host, to_host)) =
                    (client.into_split(), server.into_split());
                let drops = Arc::clone(&all_drops);
                tokio::spawn(async move {
                    tokio::select! {
                        () = relay_lines(from_main, to_host, Toward::Host, Arc::clone(&drops)) => {}
                        () = relay_lines(from_host, to_main, Toward::Main, drops) => {}
                        _ = cut.recv() => {}
                    }
                });
            }
        });
        Self {
            path,
            open,
            cut,
            drops,
        }
    }

    /// Swallow lines toward `to` that contain `pattern`, until [`Self::sever`].
    fn swallow(&self, to: Toward, pattern: &'static str) {
        let mut drops = self.drops.lock().unwrap();
        match to {
            Toward::Host => drops.host = Some(pattern),
            Toward::Main => drops.main = Some(pattern),
        }
    }

    async fn wait_swallowed(&self, pattern: &str) {
        let deadline = Instant::now() + WAIT;
        while !self
            .drops
            .lock()
            .unwrap()
            .dropped
            .iter()
            .any(|line| line.contains(pattern))
        {
            assert!(Instant::now() < deadline, "no {pattern} line came by");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn sever(&self) {
        self.open.store(false, Ordering::SeqCst);
        let mut drops = self.drops.lock().unwrap();
        (drops.host, drops.main) = (None, None);
        let _ = self.cut.send(());
    }

    fn reopen(&self) {
        self.open.store(true, Ordering::SeqCst);
    }

    /// Cut the connection and wait for the link to make a new one.
    async fn reconnect(&self, link: &HostLink) {
        self.sever();
        wait_status(link, |s| matches!(s, LinkStatus::Connecting { .. })).await;
        self.reopen();
        link.wait_connected(WAIT).await.unwrap();
    }
}

async fn relay_lines(
    from: tokio::net::unix::OwnedReadHalf,
    mut to: tokio::net::unix::OwnedWriteHalf,
    toward: Toward,
    drops: Arc<Mutex<Drops>>,
) {
    let mut from = BufReader::new(from);
    let mut line = String::new();
    loop {
        line.clear();
        if matches!(from.read_line(&mut line).await, Ok(0) | Err(_)) {
            return;
        }
        {
            let mut drops = drops.lock().unwrap();
            let pattern = match toward {
                Toward::Host => drops.host,
                Toward::Main => drops.main,
            };
            if pattern.is_some_and(|p| line.contains(p)) {
                drops.dropped.push(line.clone());
                continue;
            }
        }
        if to.write_all(line.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// A signal the host handed to a connection that died before main read it
/// reaches main over the next connection, and a re-send of one main already
/// has (its confirmation lost too) does not reach it twice.
#[tokio::test(flavor = "multi_thread")]
async fn a_signal_lost_with_its_connection_reaches_main_once() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp);
    let mut host = TestDaemon::start(&paths).await;
    let relay = Relay::start(tmp.path().join("relay.sock"), paths.control_sock.clone());
    let session = SessionId::new();
    let (tx, mut signals) = mpsc::unbounded_channel();
    let link = HostLink::connect(
        "sub-1",
        session,
        Transport::Socket(relay.path.clone()),
        tx,
        HashSet::new(),
    );
    link.wait_connected(WAIT).await.unwrap();
    let panel = PanelId::new();
    let mut pty = link
        .spawn(panel, &sh("echo ready; exec cat"), 80, 24, false)
        .unwrap();
    let mut out = String::new();
    read_until(&mut pty, &mut out, "ready").await;

    relay.swallow(Toward::Main, "\"t\":\"signal\"");
    let signal = TurnSignal::now(session, panel, TurnKind::Stop, None, serde_json::json!({}));
    let mut post = UnixStream::connect(&paths.signal_sock).await.unwrap();
    let mut line = serde_json::to_string(&signal).unwrap();
    line.push('\n');
    post.write_all(line.as_bytes()).await.unwrap();
    post.shutdown().await.unwrap();
    relay.wait_swallowed("\"t\":\"signal\"").await;

    relay.sever();
    wait_status(&link, |s| matches!(s, LinkStatus::Connecting { .. })).await;
    relay.swallow(Toward::Host, "\"t\":\"signals_seen\"");
    relay.reopen();
    link.wait_connected(WAIT).await.unwrap();
    let got = tokio::time::timeout(WAIT, signals.recv())
        .await
        .expect("the signal never reached main")
        .unwrap();
    assert!(matches!(
        ingest(&got.line).unwrap(),
        SignalEvent::Turn(sig) if sig.panel_id == panel
    ));
    relay.wait_swallowed("\"t\":\"signals_seen\"").await;

    // The host sends the unconfirmed signal again on attach, ahead of the
    // echo below.
    relay.reconnect(&link).await;
    pty.write(b"ping\n").unwrap();
    read_until(&mut pty, &mut out, "ping\r\nping").await;
    assert!(signals.try_recv().is_err(), "the signal reached main twice");

    link.close(WAIT).await;
    host.stop().await;
}

/// Input written into a connection that died before the host read it
/// reaches the panel over the next connection. Input the host took before
/// the drop, and input written while down (queued and kept for re-sending
/// both), is typed once.
#[tokio::test(flavor = "multi_thread")]
async fn input_lost_with_its_connection_reaches_the_panel_once() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp);
    let mut host = TestDaemon::start(&paths).await;
    let relay = Relay::start(tmp.path().join("relay.sock"), paths.control_sock.clone());
    let (tx, _signals) = mpsc::unbounded_channel();
    let link = HostLink::connect(
        "sub-1",
        SessionId::new(),
        Transport::Socket(relay.path.clone()),
        tx,
        HashSet::new(),
    );
    link.wait_connected(WAIT).await.unwrap();
    let mut pty = link
        .spawn(
            PanelId::new(),
            &sh("echo ready; for _ in 1 2 3 4; do read x; echo got-$x; done"),
            80,
            24,
            false,
        )
        .unwrap();
    let mut out = String::new();
    read_until(&mut pty, &mut out, "ready").await;
    pty.write(b"a\n").unwrap();
    read_until(&mut pty, &mut out, "got-a").await;

    relay.swallow(Toward::Host, "\"t\":\"input\"");
    pty.write(b"b\n").unwrap();
    relay.wait_swallowed("\"t\":\"input\"").await;
    relay.sever();
    wait_status(&link, |s| matches!(s, LinkStatus::Connecting { .. })).await;
    pty.write(b"c\n").unwrap();
    relay.reopen();
    link.wait_connected(WAIT).await.unwrap();

    read_until(&mut pty, &mut out, "got-c").await;
    pty.write(b"d\n").unwrap();
    read_until(&mut pty, &mut out, "got-d").await;
    for word in ["got-a", "got-b", "got-c", "got-d"] {
        assert_eq!(out.matches(word).count(), 1, "{word} in {out:?}");
    }

    link.close(WAIT).await;
    host.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reconnect_replays_exactly_the_output_missed_while_the_link_was_down() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp);
    let mut host = TestDaemon::start(&paths).await;
    let relay = Relay::start(tmp.path().join("relay.sock"), paths.control_sock.clone());
    let (tx, _signals) = mpsc::unbounded_channel();
    let link = HostLink::connect(
        "sub-1",
        SessionId::new(),
        Transport::Socket(relay.path.clone()),
        tx,
        HashSet::new(),
    );
    link.wait_connected(WAIT).await.unwrap();

    let mut pty = link
        .spawn(
            PanelId::new(),
            &sh("echo start; sleep 1; echo during; read x; echo got-$x"),
            80,
            24,
            false,
        )
        .unwrap();
    let mut out = String::new();
    read_until(&mut pty, &mut out, "start").await;

    relay.sever();
    wait_status(&link, |s| matches!(s, LinkStatus::Connecting { .. })).await;
    // `during` is printed while nothing is connected.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!out.contains("during"));
    relay.reopen();
    link.wait_connected(WAIT).await.unwrap();

    read_until(&mut pty, &mut out, "during").await;
    pty.write(b"z\n").unwrap();
    read_until(&mut pty, &mut out, "got-z").await;
    for word in ["start", "during", "got-z"] {
        assert_eq!(out.matches(word).count(), 1, "{word} in {out:?}");
    }

    link.close(WAIT).await;
    host.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panel_signal_on_the_host_reaches_main() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp);
    let mut host = TestDaemon::start(&paths).await;
    let session = SessionId::new();
    let (link, mut signals) = link(&paths, session);
    link.wait_connected(WAIT).await.unwrap();
    let panel = PanelId::new();
    let mut pty = link
        .spawn(panel, &sh("echo ready; exec cat"), 80, 24, false)
        .unwrap();
    let mut out = String::new();
    read_until(&mut pty, &mut out, "ready").await;

    // What `caucus signal post` writes for a Stop hook in that panel.
    let signal = TurnSignal::now(session, panel, TurnKind::Stop, None, serde_json::json!({}));
    let mut post = UnixStream::connect(&paths.signal_sock).await.unwrap();
    let mut line = serde_json::to_string(&signal).unwrap();
    line.push('\n');
    post.write_all(line.as_bytes()).await.unwrap();
    post.shutdown().await.unwrap();

    let got = tokio::time::timeout(WAIT, signals.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.host, "sub-1");
    assert!(matches!(
        ingest(&got.line).unwrap(),
        SignalEvent::Turn(sig) if sig.panel_id == panel && sig.session_id == session
    ));

    link.close(WAIT).await;
    host.stop().await;
}

/// A daemon that restarted has none of the old panels; re-attaching one
/// reports it gone instead of leaving main waiting on it.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_host_reports_the_old_panels_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp);
    let mut host = TestDaemon::start(&paths).await;
    let (link, _signals) = link(&paths, SessionId::new());
    link.wait_connected(WAIT).await.unwrap();
    let mut pty = link
        .spawn(PanelId::new(), &sh("echo up; exec cat"), 80, 24, false)
        .unwrap();
    let mut out = String::new();
    read_until(&mut pty, &mut out, "up").await;

    host.stop().await;
    wait_status(&link, |s| matches!(s, LinkStatus::Connecting { .. })).await;
    let mut host = TestDaemon::start(&paths).await;
    link.wait_connected(WAIT).await.unwrap();
    let deadline = Instant::now() + WAIT;
    while pty.is_alive() {
        let _ = pty.read();
        assert!(
            Instant::now() < deadline,
            "the vanished panel still looks alive"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    link.close(WAIT).await;
    host.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_refuses_another_protocol_version() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp);
    let mut host = TestDaemon::start(&paths).await;
    let mut stream = UnixStream::connect(&paths.control_sock).await.unwrap();
    let hello = ClientFrame::Hello {
        proto: PROTO_VERSION + 1,
        session: SessionId::new(),
        host: "sub-1".into(),
    };
    stream.write_all(encode(&hello).as_bytes()).await.unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await.unwrap();
    assert!(matches!(
        serde_json::from_str::<DaemonFrame>(&line).unwrap(),
        DaemonFrame::Error { msg } if msg.contains("protocol")
    ));
    host.stop().await;
}

/// The welcome a fake host answers a hello with.
fn fake_welcome() -> String {
    encode(&DaemonFrame::Welcome(HostInfo {
        proto: PROTO_VERSION,
        version: "test".into(),
        hostname: "fake".into(),
        caucus_bin: "caucus".into(),
        signal_sock: "/nonexistent".into(),
        session_dir: "/nonexistent".into(),
        panels: Vec::new(),
    }))
}

/// A link to a fake host that answered hello and then reads nothing, with
/// one panel whose input has filled the link's outbound queue.
struct StalledHost {
    _tmp: tempfile::TempDir,
    link: HostLink,
    panel: PanelId,
    pty: RemotePty,
    read: BufReader<tokio::net::unix::OwnedReadHalf>,
    write: tokio::net::unix::OwnedWriteHalf,
}

impl StalledHost {
    async fn start() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("host.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let (tx, _signals) = mpsc::unbounded_channel();
        let link = HostLink::connect(
            "sub-1",
            SessionId::new(),
            Transport::Socket(sock),
            tx,
            HashSet::new(),
        );
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        read.read_line(&mut String::new()).await.unwrap();
        let welcome = format!("{}{}", fake_welcome(), fake_load(1));
        write.write_all(welcome.as_bytes()).await.unwrap();
        // Main reads the load only once it has re-sent what a new connection
        // carries first, while still unable to stall: a panel opened before
        // then would be re-sent with its input to a host that reads nothing.
        wait_load(&link, 1).await;

        let panel = PanelId::new();
        let mut pty = link.spawn(panel, &sh("cat"), 80, 24, false).unwrap();
        // Full and staying full: the writer drains into the socket until
        // the host's buffer is full too, so refill until a pause frees nothing.
        let chunk = vec![b'x'; 4096];
        let deadline = Instant::now() + WAIT;
        loop {
            let mut wrote = false;
            loop {
                match pty.write(&chunk) {
                    Ok(()) => wrote = true,
                    Err(PtyError::WriterFull) => break,
                    Err(err) => panic!("{err}"),
                }
                assert!(Instant::now() < deadline, "the queue never filled");
            }
            if !wrote {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self {
            _tmp: tmp,
            link,
            panel,
            pty,
            read,
            write,
        }
    }

    /// Read frames from main until `want` matches one.
    async fn wait_for(&mut self, want: impl Fn(&ClientFrame) -> bool, what: &str) {
        let arrives = async {
            let mut line = String::new();
            loop {
                line.clear();
                assert_ne!(
                    self.read.read_line(&mut line).await.unwrap(),
                    0,
                    "link closed"
                );
                if serde_json::from_str(&line).is_ok_and(|frame| want(&frame)) {
                    return;
                }
            }
        };
        tokio::time::timeout(WAIT, arrives)
            .await
            .unwrap_or_else(|_| panic!("{what} never reached the host"));
    }
}

/// A load report a fake host sends, told apart by `cpus`.
fn fake_load(cpus: usize) -> String {
    encode(&DaemonFrame::Load(super::HostLoad {
        load_avg: [0.0; 3],
        cpus,
        mem_available_mib: None,
        mem_total_mib: None,
    }))
}

/// Wait until `link` holds the load report [`fake_load`] made for `cpus`.
async fn wait_load(link: &HostLink, cpus: usize) {
    let deadline = Instant::now() + WAIT;
    while link.load().is_none_or(|load| load.cpus != cpus) {
        assert!(
            Instant::now() < deadline,
            "main never read load report {cpus}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_kill_made_while_the_queue_is_full_still_reaches_the_host() {
    let mut host = StalledHost::start().await;
    host.pty.kill().unwrap();
    let panel = host.panel;
    host.wait_for(
        |frame| matches!(frame, ClientFrame::Kill { panel: killed } if *killed == panel),
        "the kill",
    )
    .await;
}

/// A signal that arrives while the outbound queue is full is still
/// confirmed, so the host does not keep it, and send it again, for ever.
#[tokio::test(flavor = "multi_thread")]
async fn a_signal_seen_while_the_queue_is_full_is_still_confirmed() {
    let mut host = StalledHost::start().await;
    let panel = host.panel;
    let signal = encode(&DaemonFrame::Signal {
        panel: Some(panel),
        seq: 0,
        line: "{}".into(),
    });
    // The load report goes after the signal, so once main holds it, main
    // has dealt with the signal while the queue was still full.
    host.write
        .write_all(format!("{signal}{}", fake_load(2)).as_bytes())
        .await
        .unwrap();
    wait_load(&host.link, 2).await;
    host.wait_for(
        |frame| {
            matches!(frame, ClientFrame::SignalsSeen { panel: seen, next: 1 } if *seen == Some(panel))
        },
        "the confirmation",
    )
    .await;
}

/// A resumed main that adopts a panel numbers its input after what the host
/// took from the main before it, so its first keystrokes are typed.
#[tokio::test(flavor = "multi_thread")]
async fn an_adopted_panel_takes_input_after_what_it_took_before() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp);
    let mut host = TestDaemon::start(&paths).await;
    let session = SessionId::new();
    let (before, _signals) = link(&paths, session);
    before.wait_connected(WAIT).await.unwrap();
    let panel = PanelId::new();
    let mut pty = before
        .spawn(
            panel,
            &sh("echo ready; for _ in 1 2; do read x; echo got-$x; done"),
            80,
            24,
            false,
        )
        .unwrap();
    let mut out = String::new();
    read_until(&mut pty, &mut out, "ready").await;
    pty.write(b"a\n").unwrap();
    read_until(&mut pty, &mut out, "got-a").await;
    before.close(WAIT).await;

    let (after, _signals) = resumed_link(&paths, session, HashSet::from([panel]));
    let info = after.wait_connected(WAIT).await.unwrap();
    assert_eq!(info.panels.len(), 1);
    assert_eq!((info.panels[0].id, info.panels[0].input), (panel, 2));
    let mut pty = after.adopt(panel, 80, 24).unwrap();
    let mut out = String::new();
    read_until(&mut pty, &mut out, "got-a").await;
    pty.write(b"b\n").unwrap();
    read_until(&mut pty, &mut out, "got-b").await;

    after.close(WAIT).await;
    host.stop().await;
}

/// A panel adopted after its first output left the host's ring still holds
/// the terminal modes that output set: bracketed paste, which marks an agent
/// ready for a prompt, and the alternate screen.
#[tokio::test(flavor = "multi_thread")]
async fn an_adopted_panel_keeps_the_modes_its_dropped_output_set() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp);
    let mut host = TestDaemon::start(&paths).await;
    let session = SessionId::new();
    let (before, _signals) = link(&paths, session);
    before.wait_connected(WAIT).await.unwrap();
    let flooded = tmp.path().join("flooded");
    let panel = PanelId::new();
    let _unread = before
        .spawn(
            panel,
            &sh(&format!(
                "printf '\\033[?2004h\\033[?1049h'; head -c 9000000 /dev/zero | tr '\\0' x; \
                 touch {}; printf flooded; sleep 60",
                flooded.display()
            )),
            80,
            24,
            false,
        )
        .unwrap();
    let deadline = Instant::now() + WAIT;
    while !flooded.exists() {
        assert!(
            Instant::now() < deadline,
            "the panel never flooded its ring"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    before.close(WAIT).await;

    let (after, _signals) = resumed_link(&paths, session, HashSet::from([panel]));
    after.wait_connected(WAIT).await.unwrap();
    let mut pty = after.adopt(panel, 80, 24).unwrap();
    let mut grid = crate::term::grid::Grid::new(80, 24);
    let mut tail = Vec::new();
    let deadline = Instant::now() + WAIT;
    while !tail.ends_with(b"flooded") {
        assert!(
            Instant::now() < deadline,
            "the adopted panel never replayed its tail"
        );
        let bytes = pty.read().unwrap();
        grid.advance(&bytes);
        tail.extend(bytes);
        let keep = tail.len().saturating_sub(16);
        tail.drain(..keep);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(grid.bracketed_paste());
    assert!(grid.on_alt_screen());

    after.close(WAIT).await;
    host.stop().await;
}

/// Two host entries reaching one daemon are two links: neither's connection
/// replaces the other's, and each panel answers through its own.
#[tokio::test(flavor = "multi_thread")]
async fn two_hosts_on_one_daemon_stay_connected_side_by_side() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp);
    let mut host = TestDaemon::start(&paths).await;
    let session = SessionId::new();
    let (tx, _signals) = mpsc::unbounded_channel();
    let links = ["gpu-a", "gpu-b"].map(|name| {
        let transport = Transport::Socket(paths.control_sock.clone());
        HostLink::connect(name, session, transport, tx.clone(), HashSet::new())
    });
    for link in &links {
        link.wait_connected(WAIT).await.unwrap();
    }
    let mut ptys = Vec::new();
    for link in &links {
        let script = format!("read line; echo {}-$line; exec cat", link.name());
        ptys.push(
            link.spawn(PanelId::new(), &sh(&script), 80, 24, false)
                .unwrap(),
        );
    }

    let watch_until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < watch_until {
        for link in &links {
            assert!(
                matches!(link.status(), LinkStatus::Connected(_)),
                "{} lost its connection: {:?}",
                link.name(),
                link.status()
            );
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for (link, pty) in links.iter().zip(&mut ptys) {
        pty.write(b"ping\n").unwrap();
        read_until(pty, &mut String::new(), &format!("{}-ping", link.name())).await;
    }

    for link in &links {
        link.close(WAIT).await;
    }
    host.stop().await;
}

/// A host that hangs up right after every welcome is redialled ever more
/// slowly, not every MIN_BACKOFF.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_that_hangs_up_after_its_welcome_is_redialled_ever_more_slowly() {
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("host.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let dials = Arc::new(AtomicUsize::new(0));
    let host = tokio::spawn({
        let dials = Arc::clone(&dials);
        async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                dials.fetch_add(1, Ordering::SeqCst);
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                BufReader::new(read).read_line(&mut line).await.unwrap();
                write.write_all(fake_welcome().as_bytes()).await.unwrap();
            }
        }
    });
    let (tx, _signals) = mpsc::unbounded_channel();
    let link = HostLink::connect(
        "flaky",
        SessionId::new(),
        Transport::Socket(sock),
        tx,
        HashSet::new(),
    );
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let dials = dials.load(Ordering::SeqCst);
    // Backing off 250, 500, 1000 ms dials 3 times in 1.6 s; redialling
    // every 250 ms dials 7.
    assert!((2..=4).contains(&dials), "dialled {dials} times in 1.6 s");
    host.abort();
    link.close(WAIT).await;
}
