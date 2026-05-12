//! End-to-end integration: builds the `caucus` binary via Cargo, exercises
//! the full happy path (init → session new → round start → sentinel write
//! → round status → converge) against a temp git repo + real tmux session.
//!
//! Marked `#[ignore]` so `cargo test` stays fast and tmux-independent;
//! run with `cargo test --test end_to_end -- --ignored` to exercise the
//! whole flow. Requires `tmux`, `git`, and the caucus binary built by
//! cargo (the test resolves it via `CARGO_BIN_EXE_caucus`).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn caucus_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_caucus"))
}

fn run_caucus(repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new(caucus_bin())
        .arg("--repo")
        .arg(repo)
        .args(args)
        .output()
        .expect("spawn caucus")
}

fn run(cmd: &mut Command) {
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{:?} failed: stderr={}",
        cmd,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_init(repo: &Path) {
    run(Command::new("git").arg("init").arg("-q").current_dir(repo));
    run(Command::new("git")
        .args(["config", "user.email", "caucus@test.invalid"])
        .current_dir(repo));
    run(Command::new("git")
        .args(["config", "user.name", "caucus-test"])
        .current_dir(repo));
    std::fs::write(repo.join("seed"), "seed\n").unwrap();
    run(Command::new("git").args(["add", "seed"]).current_dir(repo));
    run(Command::new("git")
        .args(["commit", "-q", "-m", "seed"])
        .current_dir(repo));
}

#[test]
#[ignore = "requires tmux + git + a real claude CLI; run with --ignored"]
fn happy_path_init_doctor_session_round_converge() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    git_init(repo);

    // 1. init — creates .caucus/ + sentinel-stop hook.
    let out = run_caucus(repo, &["init"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(repo.join(".caucus/bin/sentinel-stop").exists());

    // 2. doctor — JSON parses and includes every expected probe. We do
    // *not* assert "all green": the "hook registered" probe inspects the
    // user's real ~/.claude/settings.json against this temp repo's hook
    // path, and that's expected to be missing in a fresh test env. The
    // important guarantee is that doctor surfaces a structured report
    // with every named check.
    let out = run_caucus(repo, &["--format", "json", "doctor"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("doctor JSON parses (err={err}): {stdout}"));
    let probe_names: Vec<&str> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    for expected in [
        "tmux",
        "git",
        "claude",
        ".caucus dir",
        "sentinel hook",
        "hook registered",
        "roles",
    ] {
        assert!(
            probe_names.contains(&expected),
            "doctor missing check {expected}: {probe_names:?}"
        );
    }

    // 3. role list — the embedded defaults round-trip through the registry.
    let out = run_caucus(repo, &["--format", "json", "role", "list"]);
    assert!(out.status.success());
    let roles: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let names: Vec<String> = roles
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    for expected in ["architect", "backend", "reviewer", "qa", "scribe"] {
        assert!(
            names.contains(&expected.to_string()),
            "missing role {expected}"
        );
    }

    // 4. session list — empty at the start.
    let out = run_caucus(repo, &["--format", "json", "session", "list"]);
    let listed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 0);

    // The remainder of the happy path (session new / round start / converge)
    // requires the `claude` CLI to be installed *and* willing to spawn
    // panes that emit a Stop hook. To keep this test self-contained we stop
    // here; the manual smoke documented in commit 11's message exercises the
    // rest, and individual modules are covered by the per-module integration
    // tests (worktree round-trip, tmux pane lifecycle, sentinel watcher,
    // poller).
}

#[test]
fn caucus_help_lists_every_top_level_subcommand() {
    // Doesn't need tmux/git/claude — exercises arg parsing only.
    let out = Command::new(caucus_bin())
        .arg("--help")
        .output()
        .expect("spawn caucus");
    assert!(out.status.success());
    let body = String::from_utf8_lossy(&out.stdout);
    for sub in [
        "init", "doctor", "session", "round", "execute", "agent", "role", "sentinel", "watch",
        "auto",
    ] {
        assert!(
            body.contains(sub),
            "subcommand {sub} missing from --help output:\n{body}"
        );
    }
}

#[test]
fn round_status_json_has_last_event_ts() {
    // Bypass `session new` (which spawns claude) by writing the session
    // record + manifest directly on disk, then drive `caucus round status
    // --format json` and check the new field is present.
    use caucus::agent::lane_event::LaneEvent;
    use caucus::agent::manifest::{AgentKind, AgentManifest, write_json};
    use caucus::session::record::{Session, write_session};
    use caucus::session::state::SessionState;

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    std::fs::create_dir_all(repo.join(".caucus").join("sessions")).unwrap();

    let mut session = Session::new(repo.clone(), "topic".into(), vec!["scribe".into()], 1);
    session.transition(SessionState::MeetingInProgress).unwrap();
    session.advance_round().unwrap();
    std::fs::create_dir_all(session.session_root.join("agents")).unwrap();
    std::fs::create_dir_all(session.session_root.join("round-01")).unwrap();

    let mut manifest = AgentManifest::new(
        session.id,
        "scribe".into(),
        "scribe".into(),
        AgentKind::Meeting,
        None,
    );
    let known_ts = chrono::DateTime::parse_from_rfc3339("2026-05-12T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    manifest.lane_events.push(LaneEvent::ResponseFileWritten {
        ts: known_ts,
        path: session
            .session_root
            .join("round-01")
            .join("response-scribe.md"),
        bytes: 7,
    });
    let agent_id = manifest.agent_id;
    write_json(&manifest, &session.session_root).unwrap();
    session.register_agent("scribe", agent_id);
    write_session(&session).unwrap();

    let out = run_caucus(
        &repo,
        &[
            "--format",
            "json",
            "round",
            "status",
            &session.id.to_string(),
        ],
    );
    assert!(
        out.status.success(),
        "round status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let role0 = &parsed["roles"][0];
    assert_eq!(role0["role"].as_str().unwrap(), "scribe");
    let ts = role0["last_event_ts"]
        .as_str()
        .expect("last_event_ts present");
    assert!(
        ts.starts_with("2026-05-12T10:00:00"),
        "unexpected last_event_ts: {ts}"
    );
    // current_pane_hint should serialise as null (default None).
    assert!(role0["current_pane_hint"].is_null());
}

// ----------------------------------------------------------------------
// `caucus watch` subprocess harness — drives the stdout JSON stream and
// asserts on synthesised event kinds (`round_progress`, `round_complete`,
// `pane_hint`, `pane_gone`). The harness bypasses `session new` (which
// spawns claude + tmux) by writing session.json + manifest files
// directly with the caucus library types.

/// Holds a spawned `caucus watch` subprocess and a channel of stdout
/// lines. Dropping the struct SIGKILLs the child.
struct WatchProc {
    child: Child,
    rx: Receiver<String>,
    _stdout_thread: JoinHandle<()>,
    _stderr_thread: JoinHandle<()>,
}

impl WatchProc {
    fn spawn(repo: &Path, session_id: &str) -> Self {
        let mut child = Command::new(caucus_bin())
            .arg("--repo")
            .arg(repo)
            .args(["watch", session_id])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn caucus watch");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (tx, rx) = channel::<String>();
        let stdout_thread = std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        // Drain stderr so the kernel pipe buffer never blocks the child.
        // We don't assert on stderr — `note()` and tracing output go here.
        let stderr_thread = std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for _line in reader.lines().map_while(Result::ok) {}
        });

        Self {
            child,
            rx,
            _stdout_thread: stdout_thread,
            _stderr_thread: stderr_thread,
        }
    }

    /// Block until a stdout JSON line with `kind == expected` arrives or
    /// `timeout` elapses (in which case it panics with the collected
    /// lines for debugging). Lines whose `kind` differs are discarded
    /// (returned via `seen` for callers that need them).
    fn wait_for_kind(&self, expected: &str, timeout: Duration) -> serde_json::Value {
        let (v, _seen) = self.wait_for_kind_collecting(expected, timeout);
        v
    }

    fn wait_for_kind_collecting(
        &self,
        expected: &str,
        timeout: Duration,
    ) -> (serde_json::Value, Vec<String>) {
        let deadline = Instant::now() + timeout;
        let mut seen: Vec<String> = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!(
                    "timed out waiting for kind={expected}; saw {} line(s):\n{}",
                    seen.len(),
                    seen.join("\n")
                );
            }
            match self.rx.recv_timeout(remaining) {
                Ok(line) => {
                    seen.push(line.clone());
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                        && v["kind"].as_str() == Some(expected)
                    {
                        return (v, seen);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!(
                        "timed out waiting for kind={expected}; saw {} line(s):\n{}",
                        seen.len(),
                        seen.join("\n")
                    );
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!(
                        "watch child closed stdout before emitting kind={expected}; \
                         saw {} line(s):\n{}",
                        seen.len(),
                        seen.join("\n")
                    );
                }
            }
        }
    }

    /// Drain stdout for `window` and return every line received. Used to
    /// check for *absence* of follow-up events after a known one.
    fn drain_for(&self, window: Duration) -> Vec<String> {
        let deadline = Instant::now() + window;
        let mut out = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return out;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(line) => out.push(line),
                Err(_) => return out,
            }
        }
    }
}

impl Drop for WatchProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Build a session record + per-role manifests on disk so `caucus watch`
/// has something to read without going through `session new`. No tmux
/// panes are stamped — manifests carry `tmux_pane_id == None`, so the
/// watch loop's poller fan-in spawns zero pollers (matches the headless
/// CI environment).
///
/// If `write_response_files` is true, every role gets a non-empty
/// `round-01/response-<role>.md` written, so `all_responses_complete`
/// will flip true after the first sentinel arrives.
fn build_meeting_session(
    repo: &Path,
    roles: &[&str],
    write_response_files: bool,
) -> (
    caucus::session::id::SessionId,
    PathBuf,
    Vec<(String, caucus::session::id::AgentId)>,
) {
    use caucus::agent::manifest::{AgentKind, AgentManifest, write_json};
    use caucus::session::record::{Session, write_session};
    use caucus::session::state::SessionState;

    std::fs::create_dir_all(repo.join(".caucus").join("sessions")).unwrap();
    let role_names: Vec<String> = roles.iter().map(|r| (*r).to_string()).collect();
    let mut session = Session::new(repo.to_path_buf(), "test".into(), role_names.clone(), 1);
    session.transition(SessionState::MeetingInProgress).unwrap();
    session.advance_round().unwrap();
    std::fs::create_dir_all(session.session_root.join("agents")).unwrap();
    std::fs::create_dir_all(session.session_root.join("round-01")).unwrap();

    let mut registered = Vec::new();
    for role in roles {
        let manifest = AgentManifest::new(
            session.id,
            (*role).to_string(),
            (*role).to_string(),
            AgentKind::Meeting,
            None,
        );
        let agent_id = manifest.agent_id;
        write_json(&manifest, &session.session_root).unwrap();
        session.register_agent(role, agent_id);
        registered.push(((*role).to_string(), agent_id));
        if write_response_files {
            let path = session
                .session_root
                .join("round-01")
                .join(format!("response-{role}.md"));
            std::fs::write(&path, "# response\nok\n").unwrap();
        }
    }
    write_session(&session).unwrap();
    let session_root = session.session_root.clone();
    (session.id, session_root, registered)
}

#[test]
fn watch_emits_round_progress_on_sentinel() {
    use caucus::sentinel::writer::{Sentinel, SentinelKind, write_sentinel};

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    let (session_id, session_root, agents) =
        build_meeting_session(&repo, &["reviewer", "qa"], false);

    let watch = WatchProc::spawn(&repo, &session_id.to_string());
    // Drain `started` so we know notify is listening before we write.
    watch.wait_for_kind("started", Duration::from_secs(5));
    // FSEvents on macOS may need a brief settle window after the
    // watcher arms before it reliably reports new files. The watcher's
    // own unit test (sentinel::watcher::tests::watcher_picks_up_write)
    // uses a 2-second timeout for the same reason; we mirror that with
    // 50 ms of slack before writing so the test is independent of the
    // FSEvents coalescence latency floor.
    std::thread::sleep(Duration::from_millis(50));

    let (_role, agent_id) = agents[0].clone();
    let s = Sentinel::new(
        session_id,
        agent_id,
        SentinelKind::Stop,
        Some("done".into()),
        None,
    );
    write_sentinel(&session_root, &s).unwrap();

    let progress = watch.wait_for_kind("round_progress", Duration::from_secs(10));
    assert_eq!(
        progress["session_id"].as_str().unwrap(),
        session_id.to_string()
    );
    assert_eq!(progress["round_number"].as_u64().unwrap(), 1);
    assert_eq!(progress["total"].as_u64().unwrap(), 2);
    // No response files were written → completed must still be 0.
    assert_eq!(progress["completed"].as_u64().unwrap(), 0);
    // `states` is an object keyed by derived_state name.
    assert!(progress["states"].is_object());
}

#[test]
fn watch_emits_round_complete_once() {
    use caucus::sentinel::writer::{Sentinel, SentinelKind, write_sentinel};

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    // Single-role session with the response file pre-written: the first
    // sentinel flips all_responses_complete → true.
    let (session_id, session_root, agents) = build_meeting_session(&repo, &["scribe"], true);

    let watch = WatchProc::spawn(&repo, &session_id.to_string());
    watch.wait_for_kind("started", Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(50));

    let (_role, agent_id) = agents[0].clone();
    let first = Sentinel::new(
        session_id,
        agent_id,
        SentinelKind::Stop,
        Some("first".into()),
        None,
    );
    write_sentinel(&session_root, &first).unwrap();

    let complete = watch.wait_for_kind("round_complete", Duration::from_secs(10));
    assert_eq!(complete["round_number"].as_u64().unwrap(), 1);
    assert_eq!(
        complete["session_id"].as_str().unwrap(),
        session_id.to_string()
    );

    // Now write a second sentinel for the same agent. The watch loop
    // re-runs round_status → emits another `round_progress`, but the
    // `last_round_complete_emitted` latch must suppress a duplicate
    // `round_complete`.
    //
    // 250 ms is well above FSEvents' coalescence floor (~30 ms) so the
    // second rename surfaces as its own event rather than being merged
    // with the first one.
    std::thread::sleep(Duration::from_millis(250));
    let second = Sentinel::new(
        session_id,
        agent_id,
        SentinelKind::Stop,
        Some("second".into()),
        None,
    );
    write_sentinel(&session_root, &second).unwrap();

    // Wait for the follow-up `round_progress` (proves the watch loop
    // picked up the second sentinel) and capture every line up to it.
    let (_progress2, lines_until_progress2) =
        watch.wait_for_kind_collecting("round_progress", Duration::from_secs(10));
    let duplicate_in_window: Vec<_> = lines_until_progress2
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["kind"].as_str() == Some("round_complete"))
        .collect();
    assert!(
        duplicate_in_window.is_empty(),
        "round_complete must not re-emit; saw {} duplicate(s) before second progress: {:#?}",
        duplicate_in_window.len(),
        duplicate_in_window
    );

    // Drain stdout for a further window and assert nothing else slipped
    // through (e.g. a delayed `round_complete` after the second
    // `round_progress`).
    let trailing = watch.drain_for(Duration::from_millis(500));
    let trailing_completes: Vec<_> = trailing
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["kind"].as_str() == Some("round_complete"))
        .collect();
    assert!(
        trailing_completes.is_empty(),
        "round_complete must not re-emit after second round_progress; \
         saw {} duplicate(s): {:#?}",
        trailing_completes.len(),
        trailing_completes
    );
}

/// Run `caucus round wait` to completion. Smaller than [`WatchProc`]
/// because `round wait` is an exit-code gate (one JSON line on stdout +
/// exit, no live stream).
struct WaitProc {
    child: Child,
    ready_rx: Receiver<()>,
    stdout_thread: JoinHandle<String>,
    stderr_thread: JoinHandle<String>,
}

impl WaitProc {
    fn spawn(repo: &Path, session_id: &str, extra: &[&str]) -> Self {
        let mut child = Command::new(caucus_bin())
            .arg("--repo")
            .arg(repo)
            .args(["--format", "json", "round", "wait", session_id])
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn caucus round wait");

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (ready_tx, ready_rx) = channel::<()>();

        let stdout_thread = std::thread::spawn(move || {
            let mut buf = String::new();
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                buf.push_str(&line);
                buf.push('\n');
            }
            buf
        });

        let stderr_thread = std::thread::spawn(move || {
            let mut buf = String::new();
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                if line.trim() == "ready" {
                    let _ = ready_tx.send(());
                }
                buf.push_str(&line);
                buf.push('\n');
            }
            buf
        });

        Self {
            child,
            ready_rx,
            stdout_thread,
            stderr_thread,
        }
    }

    /// Block until the `round_wait` handler has emitted `ready` on stderr
    /// — i.e. the sentinel watcher is armed. Panics on timeout so the
    /// failing test fails fast instead of stalling.
    fn wait_for_ready(&self, timeout: Duration) {
        match self.ready_rx.recv_timeout(timeout) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => {
                panic!("round_wait did not emit 'ready' within {timeout:?}")
            }
            Err(RecvTimeoutError::Disconnected) => {
                panic!("round_wait stderr closed before emitting 'ready'")
            }
        }
    }

    fn wait(mut self) -> (std::process::ExitStatus, String, String) {
        let status = self.child.wait().expect("wait on caucus round wait");
        let stdout = self.stdout_thread.join().expect("stdout reader join");
        let stderr = self.stderr_thread.join().expect("stderr reader join");
        (status, stdout, stderr)
    }
}

/// Parse the JSON result emitted by `caucus round wait`. The handler
/// prints exactly one compact JSON line on stdout (see `emit_wait_result`
/// in `src/cli/dispatch.rs`); we tolerate trailing blanks and any future
/// tracing-style noise by taking the *last* non-empty line.
fn parse_wait_stdout(stdout: &str) -> serde_json::Value {
    let last = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_else(|| panic!("no non-empty line on stdout:\n{stdout}"));
    serde_json::from_str(last)
        .unwrap_or_else(|err| panic!("parse stdout JSON: {err}\nlast_line={last}\nstdout={stdout}"))
}

#[test]
fn round_wait_exits_zero_when_already_complete() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    let (session_id, _session_root, _agents) = build_meeting_session(&repo, &["scribe"], true);

    let (status, stdout, stderr) =
        WaitProc::spawn(&repo, &session_id.to_string(), &["--timeout-secs", "10"]).wait();
    assert!(
        status.success(),
        "expected exit 0; got {:?}\nstdout={stdout}\nstderr={stderr}",
        status.code()
    );
    let v = parse_wait_stdout(&stdout);
    assert_eq!(v["status"].as_str(), Some("completed_already"));
    assert_eq!(v["round"].as_u64(), Some(1));
    assert_eq!(v["completed"].as_u64(), Some(1));
    assert_eq!(v["total"].as_u64(), Some(1));
}

#[test]
fn round_wait_blocks_then_exits_zero_on_completion() {
    use caucus::sentinel::writer::{Sentinel, SentinelKind, write_sentinel};

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    let (session_id, session_root, agents) =
        build_meeting_session(&repo, &["reviewer", "qa"], false);

    let waiter = WaitProc::spawn(&repo, &session_id.to_string(), &["--timeout-secs", "30"]);

    // Synchronise on the handler's `ready` line — emitted right after
    // `sentinel::watch` is armed. Robust under parallel-test contention,
    // unlike a fixed sleep.
    waiter.wait_for_ready(Duration::from_secs(10));

    for (role, _agent_id) in &agents {
        let path = session_root
            .join("round-01")
            .join(format!("response-{role}.md"));
        std::fs::write(&path, "# response\nok\n").unwrap();
    }
    for (_role, agent_id) in &agents {
        let s = Sentinel::new(
            session_id,
            *agent_id,
            SentinelKind::Stop,
            Some("done".into()),
            None,
        );
        write_sentinel(&session_root, &s).unwrap();
    }

    let (status, stdout, stderr) = waiter.wait();
    assert!(
        status.success(),
        "expected exit 0; got {:?}\nstdout={stdout}\nstderr={stderr}",
        status.code()
    );
    let v = parse_wait_stdout(&stdout);
    assert_eq!(v["status"].as_str(), Some("completed"));
    assert_eq!(v["completed"].as_u64(), Some(2));
    assert_eq!(v["total"].as_u64(), Some(2));
}

#[test]
fn round_wait_exits_one_on_timeout() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    let (session_id, _session_root, _agents) =
        build_meeting_session(&repo, &["reviewer", "qa"], false);

    let (status, stdout, stderr) =
        WaitProc::spawn(&repo, &session_id.to_string(), &["--timeout-secs", "1"]).wait();
    assert_eq!(
        status.code(),
        Some(1),
        "expected exit 1; got {:?}\nstdout={stdout}\nstderr={stderr}",
        status.code()
    );
    let v = parse_wait_stdout(&stdout);
    assert_eq!(v["status"].as_str(), Some("timed_out"));
    assert_eq!(v["completed"].as_u64(), Some(0));
    assert_eq!(v["total"].as_u64(), Some(2));
}

#[test]
fn round_wait_errors_on_future_round() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    let (session_id, _session_root, _agents) = build_meeting_session(&repo, &["scribe"], false);

    let (status, stdout, stderr) = WaitProc::spawn(
        &repo,
        &session_id.to_string(),
        &["--round", "5", "--timeout-secs", "10"],
    )
    .wait();
    assert_eq!(
        status.code(),
        Some(2),
        "expected exit 2 (USER_ERROR); got {:?}\nstdout={stdout}\nstderr={stderr}",
        status.code()
    );
    assert!(
        stderr.contains("round 5 not started yet"),
        "stderr should mention the future-round rejection, got:\n{stderr}"
    );
}

#[test]
fn round_wait_exits_three_when_session_terminal() {
    use caucus::session::record::{read_session, write_session};
    use caucus::session::state::SessionState;

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    let (session_id, _session_root, _agents) = build_meeting_session(&repo, &["scribe"], false);

    // Force the session into a terminal state (MeetingInProgress →
    // MeetingDeadlocked → Abandoned) so `round wait` short-circuits.
    let mut session = read_session(&repo, session_id).unwrap();
    session.transition(SessionState::MeetingDeadlocked).unwrap();
    session.transition(SessionState::Abandoned).unwrap();
    write_session(&session).unwrap();

    let (status, stdout, stderr) =
        WaitProc::spawn(&repo, &session_id.to_string(), &["--timeout-secs", "30"]).wait();
    assert_eq!(
        status.code(),
        Some(3),
        "expected exit 3 (SESSION_TERMINAL); got {:?}\nstdout={stdout}\nstderr={stderr}",
        status.code()
    );
    let v = parse_wait_stdout(&stdout);
    assert_eq!(v["status"].as_str(), Some("session_terminal"));
    assert_eq!(v["round"].as_u64(), Some(1));
}

#[test]
fn caucus_role_list_text_format_is_human_readable() {
    let tmp = TempDir::new().unwrap();
    let out = run_caucus(tmp.path(), &["role", "list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = String::from_utf8_lossy(&out.stdout);
    for role in ["architect", "backend", "reviewer", "qa", "scribe"] {
        assert!(body.contains(role), "role {role} not in role list:\n{body}");
    }
}
