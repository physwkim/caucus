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
use caucus::host::{HostLink, HostPaths, LinkError, RemoteShell, Transport};
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
    let link = HostLink::connect(
        "chatty",
        SessionId::new(),
        transport,
        tx,
        Default::default(),
        None,
    );
    let connected = link.wait_connected(WAIT).await;
    link.close(WAIT).await;
    connected.unwrap();
}

/// A "host" reached by running sh locally with `home` as its user's home.
fn local_shell(home: &Path) -> RemoteShell {
    RemoteShell {
        program: "env".into(),
        args: vec![
            format!("HOME={}", home.display()).into(),
            "sh".into(),
            "-c".into(),
        ],
    }
}

/// The daemon a managed bridge starts under `home`.
fn managed_daemon(home: &Path) -> DaemonGuard {
    DaemonGuard {
        lock: HostPaths::default_state_dir(home).join("daemon.lock"),
    }
}

fn managed_link(home: &Path, image: &Path) -> HostLink {
    let transport = Transport::Managed {
        shell: local_shell(home),
        image: image.to_path_buf(),
    };
    let (tx, _signals) = tokio::sync::mpsc::unbounded_channel();
    HostLink::connect(
        "managed",
        SessionId::new(),
        transport,
        tx,
        Default::default(),
        None,
    )
}

fn managed_copy(home: &Path) -> PathBuf {
    home.join(".caucus").join("bin").join("caucus")
}

/// An executable script at `path` running `body`.
fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Connect to the managed host `home` with this build, then check the host
/// runs the copy main put there.
async fn connects_through_the_copy(home: &Path) {
    let _daemon = managed_daemon(home);
    let image = Path::new(env!("CARGO_BIN_EXE_caucus"));
    let link = managed_link(home, image);
    let connected = link.wait_connected(WAIT).await;
    link.close(WAIT).await;
    let info = connected.unwrap();
    let copy = managed_copy(home);
    assert_eq!(
        info.caucus_bin.canonicalize().unwrap(),
        copy.canonicalize().unwrap(),
        "the daemon runs the copy"
    );
    assert!(
        std::fs::read(&copy).unwrap() == std::fs::read(image).unwrap(),
        "the copy is this build"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_without_caucus_gets_this_build_and_connects() {
    let tmp = tempfile::tempdir().unwrap();
    connects_through_the_copy(tmp.path()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn another_build_on_the_host_is_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    script(
        &managed_copy(tmp.path()),
        "echo older-build; echo ran >>\"$HOME/older-ran\"",
    );
    connects_through_the_copy(tmp.path()).await;
    assert!(
        tmp.path().join("older-ran").exists(),
        "the older copy was asked which build it is"
    );
}

/// The link parks with `reason`, and `runs` stops growing: nothing is
/// copied again.
async fn parks(link: &HostLink, reason: &str, runs: &Path) {
    let err = link
        .wait_connected(Duration::from_secs(3))
        .await
        .unwrap_err();
    let LinkError::Timeout { last_error, .. } = err else {
        panic!("expected a timeout, got {err:?}");
    };
    let last_error = last_error.unwrap_or_default();
    assert!(last_error.contains(reason), "{last_error}");
    let count = || {
        std::fs::read_to_string(runs)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let before = count();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(count(), before, "the link stopped trying");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_that_does_not_run_there_is_copied_once() {
    let tmp = tempfile::tempdir().unwrap();
    let image = tmp.path().join("image");
    script(
        &image,
        "echo ran >>\"$HOME/runs\"; echo 'cannot execute binary file' >&2; exit 126",
    );
    let link = managed_link(tmp.path(), &image);
    parks(
        &link,
        "cannot execute binary file",
        &tmp.path().join("runs"),
    )
    .await;
    link.close(WAIT).await;
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("runs")).unwrap(),
        "ran\n",
        "only the check after the one copy ran it"
    );
    assert!(
        !managed_copy(tmp.path()).exists(),
        "nothing was put in place"
    );
}

/// A copy that lands but does not answer to its build stops the link after
/// one copy, rather than copying it forever.
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_that_disowns_its_build_is_copied_once() {
    let tmp = tempfile::tempdir().unwrap();
    let image = tmp.path().join("image");
    script(&image, "echo ran >>\"$HOME/runs\"; echo not-this-build");
    let link = managed_link(tmp.path(), &image);
    parks(&link, "still does not run it", &tmp.path().join("runs")).await;
    link.close(WAIT).await;
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("runs")).unwrap(),
        "ran\nran\n",
        "the install's check and the one connect after it"
    );
}
