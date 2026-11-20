//! A daemon and a link in one process, over real sockets and real PTYs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use super::proto::{ClientFrame, DaemonFrame, PROTO_VERSION, encode};
use super::{HostLink, HostPaths, LinkStatus, RemotePty, RemoteSignal, Transport, daemon};
use crate::pty::PtyCommand;
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
    let (tx, rx) = mpsc::unbounded_channel();
    let link = HostLink::connect(
        "sub-1",
        session,
        Transport::Socket(paths.control_sock.clone()),
        tx,
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
struct Relay {
    path: std::path::PathBuf,
    open: Arc<AtomicBool>,
    cut: broadcast::Sender<()>,
}

impl Relay {
    fn start(path: std::path::PathBuf, upstream: std::path::PathBuf) -> Self {
        let listener = UnixListener::bind(&path).unwrap();
        let open = Arc::new(AtomicBool::new(true));
        let (cut, _) = broadcast::channel(4);
        let (gate, cuts) = (Arc::clone(&open), cut.clone());
        tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                if !gate.load(Ordering::SeqCst) {
                    continue;
                }
                let mut cut = cuts.subscribe();
                let mut server = UnixStream::connect(&upstream).await.unwrap();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut client, &mut server) => {}
                        _ = cut.recv() => {}
                    }
                });
            }
        });
        Self { path, open, cut }
    }

    fn sever(&self) {
        self.open.store(false, Ordering::SeqCst);
        let _ = self.cut.send(());
    }

    fn reopen(&self) {
        self.open.store(true, Ordering::SeqCst);
    }
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
