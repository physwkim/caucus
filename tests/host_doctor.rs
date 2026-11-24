//! `caucus host doctor` as a main's `caucus doctor` runs it over `ssh`, and a
//! main's doctor asking a socket host's daemon: one report, checked in the
//! environment the daemon's panels inherit.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use caucus::config::Config;
use caucus::config::hosts::{HostConfig, HostReach};
use caucus::doctor::{HostReport, Severity};
use caucus::host::HostPaths;
use caucus::host::proto::PROTO_VERSION;

#[test]
fn host_doctor_reports_this_host_on_one_line() {
    // No daemon under this home, so the process checks itself.
    let home = short_home();
    let tmp = tempfile::tempdir().unwrap();
    let report = host_doctor(home.path(), tmp.path());
    assert_eq!(report.proto, PROTO_VERSION);
    assert_eq!(report.version, env!("CARGO_PKG_VERSION"));
    // A home with no Claude settings has no hook to run.
    let hook = report
        .checks
        .iter()
        .find(|c| c.name == "claude-stop-hook")
        .unwrap();
    assert_eq!(hook.severity, Severity::Warn, "{}", hook.detail);
}

#[test]
fn host_doctor_reports_the_running_daemon_in_its_environment() {
    let home = short_home();
    let tmp = tempfile::tempdir().unwrap();
    let no_git = tempfile::tempdir().unwrap();
    let _daemon = Daemon::start(
        home.path(),
        &default_control_sock(home.path()),
        no_git.path(),
    );
    let report = host_doctor(home.path(), tmp.path());
    let git = report.checks.iter().find(|c| c.name == "git").unwrap();
    assert_eq!(git.severity, Severity::Error, "{}", git.detail);
}

#[test]
fn doctor_asks_a_socket_host_daemon_for_its_checks() {
    let home = tempfile::tempdir().unwrap();
    let no_git = tempfile::tempdir().unwrap();
    let sock = home.path().join("ctl").join("daemon.sock");
    let _daemon = Daemon::start(home.path(), &sock, no_git.path());
    let repo = tempfile::tempdir().unwrap();
    let mut config = Config::load(repo.path()).unwrap();
    config.hosts = vec![HostConfig {
        name: "box".into(),
        reach: HostReach::Socket(sock),
        dir: repo.path().join("box"),
    }];
    let report = caucus::doctor::run(repo.path(), &config);
    let host = report.checks.iter().find(|c| c.name == "host:box").unwrap();
    assert_eq!(host.severity, Severity::Ok, "{}", host.detail);
    let git = report
        .checks
        .iter()
        .find(|c| c.name == "host:box:git")
        .unwrap();
    assert_eq!(git.severity, Severity::Error, "{}", git.detail);
}

#[test]
fn doctor_reports_a_socket_host_without_a_daemon() {
    let repo = tempfile::tempdir().unwrap();
    let mut config = Config::load(repo.path()).unwrap();
    config.hosts = vec![HostConfig {
        name: "box".into(),
        reach: HostReach::Socket(repo.path().join("daemon.sock")),
        dir: repo.path().join("box"),
    }];
    let report = caucus::doctor::run(repo.path(), &config);
    let host: Vec<_> = report
        .checks
        .iter()
        .filter(|c| c.name.starts_with("host:box"))
        .collect();
    assert_eq!(host.len(), 1, "{host:?}");
    assert_eq!(host[0].severity, Severity::Warn, "{}", host[0].detail);
    assert!(host[0].detail.contains("no daemon"), "{}", host[0].detail);
}

/// A temp dir under `/tmp` to serve as a home: under a macOS `$TMPDIR`, the
/// default sockets beneath it run past what a socket address holds.
fn short_home() -> tempfile::TempDir {
    tempfile::Builder::new().tempdir_in("/tmp").unwrap()
}

/// Run `caucus host doctor` with `home` and `tmp` as its `$HOME` and temp
/// dir, and read its report.
fn host_doctor(home: &Path, tmp: &Path) -> HostReport {
    let out = Command::new(env!("CARGO_BIN_EXE_caucus"))
        .args(["host", "doctor"])
        .env("HOME", home)
        .env("TMPDIR", tmp)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    serde_json::from_str(stdout.lines().last().unwrap()).unwrap()
}

/// The control socket a bridge or `caucus host doctor` uses under `home`.
fn default_control_sock(home: &Path) -> PathBuf {
    HostPaths::default_state_dir(home).join("daemon.sock")
}

/// A `caucus host serve` with `path` as its whole `PATH`; killed on drop.
struct Daemon(Child);

impl Daemon {
    fn start(home: &Path, control_sock: &Path, path: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_caucus"))
            .args(["host", "serve", "--state-dir"])
            .arg(home.join("state"))
            .arg("--control-sock")
            .arg(control_sock)
            .arg("--signal-sock")
            .arg(home.join("signal.sock"))
            .env("HOME", home)
            .env("PATH", path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let daemon = Self(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while UnixStream::connect(control_sock).is_err() {
            assert!(Instant::now() < deadline, "daemon never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
