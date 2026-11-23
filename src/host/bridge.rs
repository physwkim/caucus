//! `caucus host bridge` — what a main caucus runs on a sub-host over `ssh`.
//!
//! It joins its stdin/stdout to the daemon's control socket, starting the
//! daemon first when none answers. The daemon is started in its own session
//! (`setsid`), so the SSH session ending — which kills the bridge — leaves it
//! and its panels running.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::{HostPaths, private_dir};

/// How long a freshly started daemon has to open its control socket.
const DAEMON_START_WAIT: Duration = Duration::from_secs(5);

/// Install this user's turn-signal hook
/// ([`install_home_hook`](crate::init::install_home_hook)), then connect to
/// the daemon (starting it if needed) and copy bytes both ways until either
/// side closes.
pub fn run(paths: &HostPaths) -> Result<()> {
    // Panels here post their turn signals through this host's own Stop hook.
    // Without it they only go quiet, so a failure to install it is said, not
    // fatal.
    if let Err(err) = crate::init::install_home_hook() {
        eprintln!("caucus: cannot install the turn-signal hook: {err:#}");
    }
    // Ends a line the login profile printed without one, so the daemon's
    // first frame starts a line of its own.
    let mut stdout = io::stdout().lock();
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    let stream = connect_or_start(paths)?;
    let mut to_daemon = stream.try_clone().context("clone daemon socket")?;
    let mut from_daemon = stream;
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut to_daemon);
        // Main went away: tell the daemon this connection is done.
        let _ = to_daemon.shutdown(std::net::Shutdown::Write);
    });
    copy_flushing(&mut from_daemon, &mut stdout)?;
    Ok(())
}

/// Copy like [`io::copy`], flushing after every read: stdout is a pipe into
/// `ssh`, and a frame left in a buffer is a frame main waits on.
fn copy_flushing(from: &mut impl Read, to: &mut impl Write) -> io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        to.write_all(&buf[..n])?;
        to.flush()?;
    }
}

fn connect_or_start(paths: &HostPaths) -> Result<UnixStream> {
    paths
        .own_control_dir()
        .with_context(|| format!("prepare {}", paths.control_sock.display()))?;
    if let Ok(stream) = UnixStream::connect(&paths.control_sock) {
        return Ok(stream);
    }
    start_daemon(paths)?;
    let deadline = Instant::now() + DAEMON_START_WAIT;
    loop {
        match UnixStream::connect(&paths.control_sock) {
            Ok(stream) => return Ok(stream),
            Err(err) if Instant::now() >= deadline => {
                bail!(
                    "caucus host daemon did not open {} within {:?} ({err}); see {}",
                    paths.control_sock.display(),
                    DAEMON_START_WAIT,
                    paths.log_file().display()
                )
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Start `caucus host serve` detached from this process's session. A daemon
/// that loses the start race to another exits at once; the connect loop then
/// reaches the winner.
fn start_daemon(paths: &HostPaths) -> Result<()> {
    private_dir(&paths.state_dir)
        .with_context(|| format!("prepare {}", paths.state_dir.display()))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.log_file())
        .with_context(|| format!("open {}", paths.log_file().display()))?;
    let exe = std::env::current_exe().context("locate the caucus executable")?;
    let mut command = Command::new(exe);
    command
        .args(["host", "serve", "--state-dir"])
        .arg(&paths.state_dir)
        .arg("--control-sock")
        .arg(&paths.control_sock)
        .arg("--signal-sock")
        .arg(&paths.signal_sock)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // SAFETY: `setsid` is async-signal-safe, which is all `pre_exec` requires.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().context("start caucus host serve")?;
    // Reap it if it exits while the bridge still runs (the lost-race case).
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::*;

    /// A control socket reached through a symlink is not this user's daemon,
    /// even with something listening on it.
    #[test]
    fn a_control_socket_behind_a_symlink_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let theirs = tmp.path().join("theirs");
        std::fs::create_dir(&theirs).unwrap();
        let _listener = UnixListener::bind(theirs.join("daemon.sock")).unwrap();
        let dir = tmp.path().join("caucus-host");
        std::os::unix::fs::symlink(&theirs, &dir).unwrap();
        let paths = HostPaths::resolve(
            Some(tmp.path().join("state")),
            Some(dir.join("daemon.sock")),
            Some(tmp.path().join("signal.sock")),
        )
        .unwrap();
        let err = connect_or_start(&paths).unwrap_err();
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");
    }
}
