//! `caucus host bridge` as `ssh` runs it: the real binary starts the daemon on
//! first use, a later bridge reaches the same daemon, and the daemon removes
//! its socket on `SIGTERM`.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use caucus::host::proto::{Bytes, ClientFrame, DaemonFrame, PROTO_VERSION, SpawnSpec};
use caucus::host::{HostLink, Transport};
use caucus::session::id::{PanelId, SessionId};

const WAIT: Duration = Duration::from_secs(10);

struct Bridge {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: mpsc::Receiver<DaemonFrame>,
}

impl Bridge {
    fn start(dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_caucus"))
            .args(["host", "bridge", "--state-dir"])
            .arg(dir.join("state"))
            .arg("--control-sock")
            .arg(dir.join("ctl").join("daemon.sock"))
            .arg("--signal-sock")
            .arg(dir.join("signal.sock"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, frames) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                // The bridge starts with an empty line (see `bridge::run`).
                if line.is_empty() {
                    continue;
                }
                let frame = serde_json::from_str(&line).unwrap();
                if tx.send(frame).is_err() {
                    return;
                }
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            frames,
        }
    }

    fn send(&mut self, frame: &ClientFrame) {
        let mut line = serde_json::to_string(frame).unwrap();
        line.push('\n');
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    fn next(&self) -> DaemonFrame {
        self.frames
            .recv_timeout(WAIT)
            .expect("a frame from the bridge")
    }

    /// The next frame other than the daemon's periodic load reports, which
    /// interleave with everything else.
    fn recv(&self) -> DaemonFrame {
        loop {
            match self.next() {
                DaemonFrame::Load(_) => {}
                frame => return frame,
            }
        }
    }

    /// Close stdin, as `ssh` does when main goes away; the bridge must exit.
    fn close(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "bridge exited with {status}");
                return;
            }
            assert!(Instant::now() < deadline, "bridge outlived its stdin");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Stops the daemon the bridge started, even when an assertion fails first:
/// it runs in its own session, so nothing else would.
struct DaemonGuard {
    lock: PathBuf,
}

impl DaemonGuard {
    fn pid(&self) -> libc::pid_t {
        std::fs::read_to_string(&self.lock)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(&self.lock)
            && let Ok(pid) = pid.trim().parse()
        {
            // SAFETY: sends a signal to the daemon this test started.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

fn hello(bridge: &mut Bridge, session: SessionId) -> caucus::host::HostInfo {
    bridge.send(&ClientFrame::Hello {
        proto: PROTO_VERSION,
        session,
        host: "sub-1".into(),
    });
    let info = match bridge.next() {
        DaemonFrame::Welcome(info) => info,
        other => panic!("expected welcome, got {other:?}"),
    };
    match bridge.next() {
        DaemonFrame::Load(load) => assert!(load.cpus >= 1),
        other => panic!("expected the load after the welcome, got {other:?}"),
    }
    info
}

#[test]
fn a_bridge_starts_the_daemon_once_and_runs_panels_behind_it() {
    let tmp = tempfile::tempdir().unwrap();
    let daemon = DaemonGuard {
        lock: tmp.path().join("state").join("daemon.lock"),
    };
    let control_sock = tmp.path().join("ctl").join("daemon.sock");
    let session = SessionId::new();

    let mut first = Bridge::start(tmp.path());
    let info = hello(&mut first, session);
    assert_eq!(info.proto, PROTO_VERSION);
    assert_eq!(info.signal_sock, tmp.path().join("signal.sock"));
    let pid = daemon.pid();

    let panel = PanelId::new();
    first.send(&ClientFrame::Spawn {
        panel,
        spec: SpawnSpec {
            program: "cat".into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            cols: 80,
            rows: 24,
            trust_codex_cwd: false,
        },
    });
    first.send(&ClientFrame::Input {
        panel,
        offset: 0,
        data: Bytes(b"hello-bridge\n".to_vec()),
    });
    let mut out = Vec::new();
    while !String::from_utf8_lossy(&out).contains("hello-bridge") {
        match first.recv() {
            DaemonFrame::Spawned { panel: p } => assert_eq!(p, panel),
            DaemonFrame::Output { panel: p, data, .. } if p == panel => out.extend(data.0),
            other => panic!("unexpected frame {other:?}"),
        }
    }
    first.send(&ClientFrame::Kill { panel });
    loop {
        match first.recv() {
            DaemonFrame::Output { .. } => {}
            DaemonFrame::Exited { panel: p, reason } => {
                assert_eq!(p, panel);
                assert_eq!(reason.as_deref(), Some("killed"));
                break;
            }
            other => panic!("unexpected frame {other:?}"),
        }
    }
    first.close();

    let mut second = Bridge::start(tmp.path());
    hello(&mut second, session);
    assert_eq!(daemon.pid(), pid, "the second bridge reused the daemon");
    second.close();

    drop(daemon);
    let deadline = Instant::now() + WAIT;
    while control_sock.exists() {
        assert!(Instant::now() < deadline, "SIGTERM did not stop the daemon");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A login profile that prints ahead of the bridge, a last line without its
/// newline included, does not keep main from connecting.
#[tokio::test(flavor = "multi_thread")]
async fn a_login_profile_printing_before_the_bridge_does_not_stop_the_link() {
    let tmp = tempfile::tempdir().unwrap();
    let _daemon = DaemonGuard {
        lock: tmp.path().join("state").join("daemon.lock"),
    };
    let profile_then_bridge = "printf 'Agent pid 1\\npartial'; \
        exec \"$0\" host bridge --state-dir \"$1\" --control-sock \"$2\" --signal-sock \"$3\"";
    let transport = Transport::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            profile_then_bridge.into(),
            env!("CARGO_BIN_EXE_caucus").into(),
            tmp.path().join("state").into(),
            tmp.path().join("ctl").join("daemon.sock").into(),
            tmp.path().join("signal.sock").into(),
        ],
    };
    let (tx, _signals) = tokio::sync::mpsc::unbounded_channel();
    let link = HostLink::connect("chatty", SessionId::new(), transport, tx);
    let connected = link.wait_connected(WAIT).await;
    link.close(WAIT).await;
    connected.unwrap();
}
