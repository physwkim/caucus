//! `caucus doctor` — environment + configuration health check
//! (`docs/design.md` §10).
//!
//! Checks: the running caucus version + `caucus` on `PATH` (and that the two
//! builds agree — the hook chain runs the PATH binary), `git`, that the cwd
//! is a git repository, the agent CLIs (`claude` / `codex`), the Stop hook
//! chain (present in settings, its command runnable on this machine, and a
//! live end-to-end signal delivery), and every role's `allowed_tools` for the
//! forbidden `Task` tool (Invariant I-7).
//!
//! Each sub-host in `hosts.toml` has its daemon run the machine checks, in
//! the environment its panels inherit: a socket host's daemon is asked over
//! its socket, an ssh host's by `caucus host doctor` there, which runs them
//! itself when no daemon runs yet, in the login shell a bridge would start
//! one from. The host's protocol version is compared with this caucus's.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::config::hosts::{HostConfig, HostReach};
use crate::host::proto::{ClientFrame, DaemonFrame, PROTO_VERSION, encode};
use crate::host::{HostPaths, ssh_args};

/// Severity of a single check result.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Everything is fine.
    Ok,
    /// Non-fatal — caucus runs, but something is degraded.
    Warn,
    /// Fatal — caucus cannot operate correctly.
    Error,
}

/// The outcome of one doctor check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    /// Short name, e.g. `git` or `role:reviewer`.
    pub name: String,
    pub severity: Severity,
    /// Human-readable detail.
    pub detail: String,
}

/// The full doctor report.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// Worst severity across all checks.
    pub fn worst(&self) -> Severity {
        self.checks
            .iter()
            .map(|c| c.severity)
            .max_by_key(|s| match s {
                Severity::Ok => 0,
                Severity::Warn => 1,
                Severity::Error => 2,
            })
            .unwrap_or(Severity::Ok)
    }
}

/// Run all environment + configuration checks for the project rooted at `repo`.
///
/// Reports the running caucus version (that `caucus` is on `PATH` for the
/// turn-signal hook, and that the PATH build's version matches the running
/// one), probes `git` and the agent CLIs (`claude` / `codex`) on
/// `PATH`, verifies the Claude `Stop` hook is installed in
/// `~/.claude/settings.json`, confirms `repo` is a git repository, audits
/// every role's `allowed_tools` for the forbidden `Task` tool (Invariant
/// I-7), and checks every sub-host.
pub fn run(repo: &Path, config: &Config) -> Report {
    let mut report = Report {
        checks: machine_checks(),
    };

    // The cwd must actually be a git repository, or git's shell-outs fail
    // later, confusingly, at the first role spawn.
    report.checks.push(git_repo_check(repo));

    // Role allowlist audit — Invariant I-7: no role may grant `Task`.
    for spec in config.roles.specs() {
        if spec.allows_task() {
            report.checks.push(Check {
                name: format!("role:{}", spec.name),
                severity: Severity::Warn,
                detail: format!(
                    "role '{}' grants the forbidden `Task` tool; nested sub-agents \
                     are invisible to caucus (design.md §0 #13)",
                    spec.name
                ),
            });
        }
    }

    report.checks.extend(sub_host_checks(&config.hosts));
    report
}

/// The checks of this machine as a place panels run: here, and on a sub-host
/// through [`host_report`].
fn machine_checks() -> Vec<Check> {
    let mut checks = vec![
        // Identify the running build first — and confirm `caucus` is on PATH,
        // since the turn-signal hook script execs it by bare name.
        caucus_check(),
        // `git` is mandatory — worktree creation/cleanup shell out to it.
        binary_check(
            "git",
            Severity::Error,
            "required for worktree creation and commit provenance",
        ),
        // The agent CLIs: a missing one is a warning, not fatal — a session
        // may only use a subset (e.g. claude-only). `caucus` itself still runs.
        binary_check(
            "claude",
            Severity::Warn,
            "the default agent backend; required unless every role overrides it",
        ),
        binary_check(
            "codex",
            Severity::Warn,
            "needed for roles with `agent_cli = \"codex\"` (e.g. serious-reviewer)",
        ),
    ];
    // Claude Stop hook — turn-completion signals depend on it (§7). Presence
    // in settings.json, then the hook command's existence on *this* machine,
    // then a live end-to-end delivery test.
    checks.extend(stop_hook_checks());
    checks
}

/// A sub-host daemon's answer to a doctor request, which `caucus host
/// doctor` prints for a main's `caucus doctor`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostReport {
    pub proto: u32,
    pub version: String,
    pub checks: Vec<Check>,
}

/// This machine's checks as a sub-host.
pub fn host_report() -> HostReport {
    HostReport {
        proto: PROTO_VERSION,
        version: env!("CARGO_PKG_VERSION").to_string(),
        checks: machine_checks(),
    }
}

/// How long a sub-host has to answer: an ssh login, then the hook's live
/// self-test there.
const SUB_HOST_WAIT: Duration = Duration::from_secs(20);

/// Every sub-host's checks, the hosts asked side by side.
fn sub_host_checks(hosts: &[HostConfig]) -> Vec<Check> {
    std::thread::scope(|scope| {
        let asked: Vec<_> = hosts
            .iter()
            .map(|host| scope.spawn(move || sub_host_check(host)))
            .collect();
        asked
            .into_iter()
            .flat_map(|asked| asked.join().expect("sub-host check panicked"))
            .collect()
    })
}

fn sub_host_check(host: &HostConfig) -> Vec<Check> {
    let report = match &host.reach {
        HostReach::Socket(sock) => UnixStream::connect(sock)
            .map_err(|e| format!("not checked: no daemon at {}: {e}", sock.display()))
            .and_then(ask_daemon),
        HostReach::Ssh { dest, caucus } => {
            let mut command = Command::new("ssh");
            command.args(ssh_args(dest, caucus, "host doctor"));
            output_within(&mut command, SUB_HOST_WAIT)
                .map_err(|e| format!("not checked: {e}"))
                .and_then(printed_report)
        }
    };
    sub_host_report(&host.name, report)
}

/// What `caucus host doctor` prints: the report of the daemon a bridge
/// reaches through `paths`, or with none running this process's, the binary
/// and environment the bridge would start one with.
pub fn host_doctor(paths: &HostPaths) -> Result<HostReport, String> {
    match UnixStream::connect(&paths.control_sock) {
        Ok(stream) => ask_daemon(stream),
        Err(_) => Ok(host_report()),
    }
}

/// Send a doctor request on `stream`, a connection to a daemon's control
/// socket, and read the daemon's report.
fn ask_daemon(mut stream: UnixStream) -> Result<HostReport, String> {
    let failed = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
            format!("the daemon gave no report within {SUB_HOST_WAIT:?}")
        }
        _ => format!("doctor request to the daemon failed: {e}"),
    };
    stream
        .set_read_timeout(Some(SUB_HOST_WAIT))
        .map_err(failed)?;
    stream
        .set_write_timeout(Some(SUB_HOST_WAIT))
        .map_err(failed)?;
    stream
        .write_all(encode(&ClientFrame::Doctor).as_bytes())
        .map_err(failed)?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(failed)?;
    match serde_json::from_str::<DaemonFrame>(&line) {
        Ok(DaemonFrame::Report(report)) => Ok(report),
        Ok(DaemonFrame::Error { msg }) => {
            Err(format!("the daemon refused a doctor request: {msg}"))
        }
        Ok(other) => Err(format!(
            "the daemon answered a doctor request with {other:?}"
        )),
        Err(_) if line.is_empty() => Err("the daemon closed without a report".into()),
        Err(e) => Err(format!("unreadable report from the daemon: {e}")),
    }
}

/// Run `command` to its end, or kill it once `wait` has passed.
fn output_within(command: &mut Command, wait: Duration) -> Result<Output, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {:?}: {e}", command.get_program()))?;
    let stdout = child.stdout.take().map(read_apart);
    let stderr = child.stderr.take().map(read_apart);
    let deadline = Instant::now() + wait;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no answer within {wait:?}"));
            }
            Err(e) => return Err(format!("cannot wait for {:?}: {e}", command.get_program())),
        }
    };
    Ok(Output {
        status,
        stdout: read_back(stdout),
        stderr: read_back(stderr),
    })
}

/// Read a child's `pipe` to its end on a thread of its own, so the child
/// never stalls on a full pipe while it is waited for.
fn read_apart(mut pipe: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = pipe.read_to_end(&mut out);
        out
    })
}

/// What [`read_apart`] read.
fn read_back(pipe: Option<std::thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    pipe.and_then(|pipe| pipe.join().ok()).unwrap_or_default()
}

/// The report `caucus host doctor` printed in `out`, after anything the host
/// user's login profile printed ahead of it.
fn printed_report(out: Output) -> Result<HostReport, String> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Some(report) = stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<HostReport>(line).ok())
    {
        return Ok(report);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let why = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output");
    Err(format!(
        "`caucus host doctor` there did not report ({}): {why}",
        out.status
    ))
}

/// `host`'s daemon's report: a check of the host itself, then the host's own
/// checks named under it; or why there is none.
fn sub_host_report(host: &str, report: Result<HostReport, String>) -> Vec<Check> {
    let name = format!("host:{host}");
    let report = match report {
        Ok(report) => report,
        Err(detail) => {
            return vec![Check {
                name,
                severity: Severity::Warn,
                detail,
            }];
        }
    };
    let itself = if report.proto == PROTO_VERSION {
        Check {
            name: name.clone(),
            severity: Severity::Ok,
            detail: format!("caucus v{} there, protocol {PROTO_VERSION}", report.version),
        }
    } else {
        Check {
            name: name.clone(),
            severity: Severity::Warn,
            detail: format!(
                "caucus v{} there speaks protocol {}, this caucus {PROTO_VERSION}, so the \
                 host will not connect; install a matching caucus there and stop its old \
                 daemon",
                report.version, report.proto
            ),
        }
    };
    std::iter::once(itself)
        .chain(report.checks.into_iter().map(|check| Check {
            name: format!("{name}:{}", check.name),
            ..check
        }))
        .collect()
}

/// Whether `bin` is resolvable on `PATH`. A bare `which`-style probe walking
/// `$PATH` entries — no extra process spawn.
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// Build a check for one expected binary on `PATH`.
fn binary_check(bin: &str, missing_severity: Severity, why: &str) -> Check {
    match which(bin) {
        Some(p) => Check {
            name: bin.to_string(),
            severity: Severity::Ok,
            detail: format!("found at {}", p.display()),
        },
        None => Check {
            name: bin.to_string(),
            severity: missing_severity,
            detail: format!("`{bin}` not found on PATH — {why}"),
        },
    }
}

/// Report the running caucus version and confirm a `caucus` binary is on
/// `PATH`. The turn-signal hook script execs bare `caucus signal post`
/// (`crate::init`), so a caucus that is not on `PATH` — run via `cargo run` or
/// an absolute path — leaves turn-completion signals dead even though the TUI
/// itself works. The version is surfaced so `caucus doctor` output pins the
/// exact build for bug reports and upgrade confirmation.
fn caucus_check() -> Check {
    let on_path = which("caucus");
    let path_version = on_path.as_deref().and_then(path_binary_version);
    caucus_check_from(
        env!("CARGO_PKG_VERSION"),
        on_path.as_deref(),
        path_version.as_deref(),
    )
}

/// The pure body of [`caucus_check`]: running version vs what `PATH` resolves.
/// A version mismatch is the one *silent* degradation in the signal chain —
/// the stale PATH binary still posts signals (the live selftest passes), but
/// it speaks an older wire, so newer capabilities (e.g. the `wants_reply`
/// hook-reply of §7.6) quietly degrade to fire-and-forget.
fn caucus_check_from(running: &str, on_path: Option<&Path>, path_version: Option<&str>) -> Check {
    let name = "caucus".to_string();
    match on_path {
        Some(p) => match path_version {
            Some(found) if found != running => Check {
                name,
                severity: Severity::Warn,
                detail: format!(
                    "v{running} running, but `caucus` on PATH ({}) is v{found} — the \
                     turn-signal hook execs the PATH binary, so signals go through \
                     the other build and newer wire features degrade silently; \
                     reinstall caucus so PATH matches",
                    p.display()
                ),
            },
            // Same version, or a binary whose version cannot be read (a
            // wrapper script, say) — no mismatch provable. Whether it works
            // at all is the live selftest's verdict, not this check's.
            _ => Check {
                name,
                severity: Severity::Ok,
                detail: format!("v{running} (on PATH at {})", p.display()),
            },
        },
        None => Check {
            name,
            severity: Severity::Warn,
            detail: format!(
                "v{running} running, but `caucus` is not on PATH — the turn-signal \
                 hook (`exec caucus signal post`) cannot run; install caucus on PATH"
            ),
        },
    }
}

/// The version of the caucus binary at `path`, via `<path> --version` (clap
/// prints `caucus <semver>`). `None` when it cannot be run or prints nothing
/// version-shaped — then [`caucus_check_from`] stays quiet and the live
/// selftest is the arbiter of chain health.
fn path_binary_version(path: &Path) -> Option<String> {
    let out = std::process::Command::new(path)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_version_output(&String::from_utf8_lossy(&out.stdout))
}

/// The version token out of `--version` output: the last whitespace token of
/// the first line (`caucus 0.9.1` → `0.9.1`), required to start with a digit
/// so arbitrary wrapper-script chatter does not read as a mismatch.
fn parse_version_output(text: &str) -> Option<String> {
    text.lines()
        .next()?
        .split_whitespace()
        .last()
        .filter(|v| v.starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_string)
}

/// Check that `repo` is inside a git work tree. caucus's per-panel isolation
/// creates git worktrees and its provenance commits shell out to git, so a
/// non-repo cwd is fatal — it fails later, confusingly, at the first spawn.
/// Asks git itself (`git -C <repo> rev-parse --is-inside-work-tree`) so the
/// answer matches what caucus's own worktree shell-outs will see; if git cannot
/// be run at all the `git` binary check above already carries the Error, so
/// this degrades to a Warn rather than double-reporting it as fatal.
fn git_repo_check(repo: &Path) -> Check {
    let name = "git-repo".to_string();
    match std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
    {
        Ok(out)
            if out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true" =>
        {
            Check {
                name,
                severity: Severity::Ok,
                detail: format!("{} is inside a git work tree", repo.display()),
            }
        }
        Ok(_) => Check {
            name,
            severity: Severity::Error,
            detail: format!(
                "{} is not a git repository — worktree isolation and provenance \
                 commits will fail; run `git init` or cd into a repo",
                repo.display()
            ),
        },
        Err(err) => Check {
            name,
            severity: Severity::Warn,
            detail: format!("could not run `git rev-parse` (is git installed?): {err}"),
        },
    }
}

/// Check the whole turn-signal delivery chain, not just its configuration.
///
/// Three layered checks: the `Stop` hook entry exists in
/// `~/.claude/settings.json`; the hook *command* actually resolves on this
/// machine (a synced `settings.json` can carry another machine's absolute
/// `turn-signal` path — the presence check alone false-OKs while every
/// worker panel sits at `working` forever because no signal ever lands);
/// and a live end-to-end delivery ([`signal_selftest`]). Without a working
/// chain, turn-completion signals never reach the socket (§7) and rounds
/// never settle.
fn stop_hook_checks() -> Vec<Check> {
    let name = "claude-stop-hook".to_string();
    let Some(home) = std::env::var_os("HOME") else {
        return vec![Check {
            name,
            severity: Severity::Warn,
            detail: "$HOME unset — cannot locate ~/.claude/settings.json".into(),
        }];
    };
    let settings = PathBuf::from(home).join(".claude").join("settings.json");
    let text = match std::fs::read_to_string(&settings) {
        Ok(t) => t,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return vec![Check {
                name,
                severity: Severity::Warn,
                detail: format!(
                    "{} not found — run `caucus init --install-hook`",
                    settings.display()
                ),
            }];
        }
        Err(err) => {
            return vec![Check {
                name,
                severity: Severity::Warn,
                detail: format!("cannot read {}: {err}", settings.display()),
            }];
        }
    };
    let settings_value = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => v,
        Err(err) => {
            return vec![Check {
                name,
                severity: Severity::Warn,
                detail: format!("{} is not valid JSON: {err}", settings.display()),
            }];
        }
    };
    let commands = crate::hook::caucus_hook_commands(&settings_value, "Stop");
    let Some(command) = commands.first() else {
        return vec![Check {
            name,
            severity: Severity::Warn,
            detail: "no `Stop` hook in ~/.claude/settings.json — run \
                     `caucus init --install-hook`"
                .into(),
        }];
    };
    let program = hook_program(command);
    if hook_program_resolves(program).is_none() {
        // Two ways to get here: a `settings.json` synced from another machine,
        // or a legacy per-project hook (`<repo>/.caucus/bin/turn-signal`) whose
        // project was deleted or superseded. Re-installing fixes both, and now
        // writes the script machine-wide so it cannot recur.
        return vec![Check {
            name,
            severity: Severity::Warn,
            detail: format!(
                "Stop hook is configured but its command `{program}` does not \
                 exist on this machine (a settings.json synced from another \
                 machine, or a legacy per-project hook whose project is gone) \
                 — run `caucus init --install-hook`; until then every panel \
                 stays `working` forever"
            ),
        }];
    }
    vec![
        Check {
            name,
            severity: Severity::Ok,
            detail: format!("Stop hook present and `{program}` is runnable"),
        },
        signal_selftest(command),
        lifecycle_hooks_check(&settings_value),
    ]
}

/// Presence check for the lifecycle hook events (`PostCompact`, `SessionStart`).
/// These close the `working` state for local slash commands (`/compact`,
/// `/clear`), which run no agent turn and therefore fire no `Stop` hook; a
/// panel given one on a machine without them wedges in `working` until its
/// next real prompt.
fn lifecycle_hooks_check(settings: &serde_json::Value) -> Check {
    let name = "claude-lifecycle-hooks".to_string();
    let missing: Vec<&str> = crate::hook::HOOK_EVENTS
        .iter()
        .filter(|(event, _)| *event != "Stop")
        .filter(|(event, _)| crate::hook::caucus_hook_commands(settings, event).is_empty())
        .map(|(event, _)| *event)
        .collect();
    if missing.is_empty() {
        Check {
            name,
            severity: Severity::Ok,
            detail: "PostCompact + SessionStart hooks present".into(),
        }
    } else {
        Check {
            name,
            severity: Severity::Warn,
            detail: format!(
                "no `{}` hook in ~/.claude/settings.json — run `caucus init \
                 --install-hook`; until then a panel given `/compact` or \
                 `/clear` stays `working` until its next real prompt",
                missing.join("` / `")
            ),
        }
    }
}

/// The program a hook command starts with — its first whitespace token.
/// caucus's own installs are a bare absolute script path, so this is exact
/// for them; for a `caucus signal post ...` style command it yields the bare
/// name to resolve via `PATH`.
fn hook_program(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or(command)
}

/// Resolve a hook program the way the shell that runs the hook will: a path
/// (contains `/`) must itself be executable; a bare name is looked up on
/// `PATH`.
fn hook_program_resolves(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        is_executable(&path).then_some(path)
    } else {
        which(program)
    }
}

/// How long the self-test waits for the hook to deliver a signal before
/// declaring the chain dead. The hook is one local exec plus one unix-socket
/// connect; a healthy chain lands in milliseconds.
const SELFTEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);

/// End-to-end turn-signal delivery test: bind a throwaway socket, run the
/// hook command exactly as Claude Code would (through `sh -c`, with the
/// `CAUCUS_*` env a real panel spawn injects, a JSON payload on stdin), and
/// require bytes to arrive on the socket within `SELFTEST_DEADLINE`.
///
/// This is the check that actually mirrors production: it fails when the
/// hook script is missing or not executable, when `caucus` does not resolve
/// inside the hook's environment, or when the temp dir cannot host a
/// connectable socket — each of which silently strands every panel at
/// `working` while all the static checks look fine.
pub fn signal_selftest(hook_command: &str) -> Check {
    static SELFTEST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = "turn-signal".to_string();
    let seq = SELFTEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let sock =
        std::env::temp_dir().join(format!("caucus-doctor-{}-{seq}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let result = run_selftest(hook_command, &sock);
    let _ = std::fs::remove_file(&sock);
    match result {
        Ok(()) => Check {
            name,
            severity: Severity::Ok,
            detail: "hook delivered a live signal end-to-end (hook → socket)".into(),
        },
        Err(detail) => Check {
            name,
            severity: Severity::Warn,
            detail: format!(
                "{detail} — turn-completion signals are not arriving; every \
                 panel will sit at `working` forever"
            ),
        },
    }
}

/// The fallible body of [`signal_selftest`], separated so the socket file is
/// cleaned up on every exit path of the caller.
fn run_selftest(hook_command: &str, sock: &Path) -> Result<(), String> {
    use std::io::{Read, Write};

    let listener = std::os::unix::net::UnixListener::bind(sock)
        .map_err(|e| format!("cannot bind a test socket at {}: {e}", sock.display()))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("cannot poll the test socket: {e}"))?;

    // Run the command exactly as Claude Code runs hook commands: through the
    // shell, payload JSON on stdin, `CAUCUS_*` env as a panel spawn injects.
    // The ids must be ULID-shaped — `caucus signal post` parses them before
    // posting, and rejecting the test ids would fail the healthy path. A
    // fixed all-zero ULID cannot collide with a real session (real ids carry
    // a current timestamp).
    const SELFTEST_ID: &str = "00000000000000000000000000";
    let mut child = std::process::Command::new("sh")
        .args(["-c", hook_command])
        .env("CAUCUS_SOCK", sock)
        .env("CAUCUS_SESSION_ID", SELFTEST_ID)
        .env("CAUCUS_PANEL_ID", SELFTEST_ID)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run the hook command: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"{}");
    }
    let stderr = child.stderr.take().map(read_apart);

    // Once the hook process has exited, any successful post's connect is
    // already queued on the listener — a short grace is enough to drain it,
    // so a hook that exits without posting fails fast instead of eating the
    // whole deadline.
    const EXIT_GRACE: std::time::Duration = std::time::Duration::from_millis(250);
    let deadline = std::time::Instant::now() + SELFTEST_DEADLINE;
    let mut exited_at: Option<std::time::Instant> = None;
    let received = loop {
        match listener.accept() {
            Ok((mut conn, _)) => {
                let _ = conn.set_read_timeout(Some(SELFTEST_DEADLINE));
                let mut buf = [0u8; 1];
                // Retry on EINTR rather than reporting a false "hook did not
                // post": a signal interrupting this probe read is not evidence
                // the hook stayed silent. Same handling as the production reply
                // read in `signal::post`.
                let got = loop {
                    match conn.read(&mut buf) {
                        Ok(n) => break n > 0,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break false,
                    }
                };
                break got;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let now = std::time::Instant::now();
                if now >= deadline {
                    break false;
                }
                if exited_at.is_none() && matches!(child.try_wait(), Ok(Some(_))) {
                    exited_at = Some(now);
                }
                if exited_at.is_some_and(|t| now >= t + EXIT_GRACE) {
                    break false;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("test socket accept failed: {e}"));
            }
        }
    };

    if received {
        let _ = child.wait();
        return Ok(());
    }
    // Nothing arrived: collect the hook's stderr for the diagnosis.
    let _ = child.kill();
    child
        .wait()
        .map_err(|e| format!("hook did not deliver and could not be reaped: {e}"))?;
    let stderr = read_back(stderr);
    let stderr = String::from_utf8_lossy(&stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        Err(format!(
            "hook ran but no signal arrived within {SELFTEST_DEADLINE:?}"
        ))
    } else {
        Err(format!("hook failed: {stderr}"))
    }
}

// Hook detection lives in `crate::hook` — the single owner shared with
// `crate::init`, so neither drifts back to a loose "mentions caucus" match.

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn clean_config_has_no_task_warnings() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config::load(tmp.path()).unwrap();
        // The global hosts.toml would have the test reach real hosts.
        config.hosts.clear();
        let report = run(tmp.path(), &config);
        assert!(report.checks.iter().all(|c| !c.name.starts_with("role:")));
    }

    #[test]
    fn worst_of_empty_report_is_ok() {
        assert_eq!(Report::default().worst(), Severity::Ok);
    }

    #[test]
    fn hook_program_is_the_first_token() {
        assert_eq!(hook_program("/a/b/turn-signal"), "/a/b/turn-signal");
        assert_eq!(hook_program("caucus signal post --kind stop"), "caucus");
        assert_eq!(hook_program(""), "");
    }

    #[test]
    fn hook_program_resolves_rejects_a_path_missing_on_this_machine() {
        // The synced-settings.json failure shape: an absolute path from
        // another machine. Presence in settings must not imply runnability.
        assert!(hook_program_resolves("/no/such/machine/.caucus/bin/turn-signal").is_none());
    }

    #[test]
    fn hook_program_resolves_accepts_an_executable_path() {
        assert!(hook_program_resolves("/bin/sh").is_some());
    }

    #[test]
    fn selftest_warns_when_the_hook_command_cannot_run() {
        let check = signal_selftest("/no/such/machine/.caucus/bin/turn-signal");
        assert_eq!(check.severity, Severity::Warn, "detail: {}", check.detail);
        assert!(check.detail.contains("working"), "detail: {}", check.detail);
    }

    #[test]
    fn selftest_warns_when_the_hook_exits_without_posting() {
        // A hook that runs fine but never touches the socket — e.g. the real
        // turn-signal script when $CAUCUS_SOCK's caucus binary is absent.
        let check = signal_selftest("exit 0");
        assert_eq!(check.severity, Severity::Warn, "detail: {}", check.detail);
    }

    /// A hook that writes more to stderr than a pipe holds still exits, so
    /// it is diagnosed without waiting out the deadline.
    #[test]
    fn a_hook_writing_past_a_pipe_is_diagnosed_once_it_exits() {
        let started = Instant::now();
        let check = signal_selftest("yes e | head -c 200000 >&2");
        assert_eq!(check.severity, Severity::Warn, "detail: {}", check.detail);
        assert!(
            started.elapsed() < SELFTEST_DEADLINE,
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn run_includes_binary_and_hook_checks() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config::load(tmp.path()).unwrap();
        config.hosts = vec![HostConfig {
            name: "box".into(),
            reach: HostReach::Socket(tmp.path().join("daemon.sock")),
            dir: tmp.path().join("box"),
        }];
        let report = run(tmp.path(), &config);
        for expected in [
            "caucus",
            "git",
            "git-repo",
            "claude",
            "codex",
            "claude-stop-hook",
            "host:box",
        ] {
            assert!(
                report.checks.iter().any(|c| c.name == expected),
                "missing doctor check: {expected}"
            );
        }
    }

    #[test]
    fn caucus_check_reports_the_running_version() {
        // Regardless of whether `caucus` is on PATH, the detail pins the
        // running build's version so `caucus doctor` output is self-dating.
        let check = caucus_check();
        assert_eq!(check.name, "caucus");
        assert!(
            check.detail.contains(env!("CARGO_PKG_VERSION")),
            "version not surfaced: {}",
            check.detail
        );
    }

    #[test]
    fn caucus_check_warns_when_the_path_binary_version_differs() {
        let check = caucus_check_from("0.9.1", Some(Path::new("/usr/bin/caucus")), Some("0.8.0"));
        assert_eq!(check.severity, Severity::Warn, "{}", check.detail);
        for needle in ["0.9.1", "0.8.0", "/usr/bin/caucus"] {
            assert!(check.detail.contains(needle), "detail: {}", check.detail);
        }
    }

    #[test]
    fn caucus_check_is_ok_when_the_path_binary_version_matches() {
        let check = caucus_check_from("0.9.1", Some(Path::new("/usr/bin/caucus")), Some("0.9.1"));
        assert_eq!(check.severity, Severity::Ok, "{}", check.detail);
    }

    #[test]
    fn caucus_check_is_ok_when_the_path_binary_version_is_unreadable() {
        // A wrapper script with no readable version: no mismatch provable, so
        // no warning — the live selftest decides whether the chain works.
        let check = caucus_check_from("0.9.1", Some(Path::new("/usr/bin/caucus")), None);
        assert_eq!(check.severity, Severity::Ok, "{}", check.detail);
    }

    #[test]
    fn caucus_check_warns_when_not_on_path() {
        let check = caucus_check_from("0.9.1", None, None);
        assert_eq!(check.severity, Severity::Warn, "{}", check.detail);
        assert!(check.detail.contains("not on PATH"), "{}", check.detail);
    }

    #[test]
    fn parse_version_output_takes_the_first_line_version_token() {
        assert_eq!(
            parse_version_output("caucus 0.9.1\n").as_deref(),
            Some("0.9.1")
        );
        assert_eq!(parse_version_output("0.9.1").as_deref(), Some("0.9.1"));
        assert_eq!(parse_version_output("not a version"), None);
        assert_eq!(parse_version_output(""), None);
    }

    #[cfg(unix)]
    #[test]
    fn path_binary_version_reads_a_binary_version() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let bin = tmp.path().join("caucus");
        std::fs::write(&bin, "#!/bin/sh\necho 'caucus 99.0.0'\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(path_binary_version(&bin).as_deref(), Some("99.0.0"));
    }

    #[cfg(unix)]
    #[test]
    fn path_binary_version_is_none_for_a_failing_binary() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let bin = tmp.path().join("caucus");
        std::fs::write(&bin, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(path_binary_version(&bin), None);
    }

    #[test]
    fn git_repo_check_flags_a_non_repo() {
        if which("git").is_none() {
            return; // no git → the `git` binary check carries the Error instead.
        }
        // A bare TempDir is not inside any git work tree.
        let tmp = TempDir::new().unwrap();
        let check = git_repo_check(tmp.path());
        assert_eq!(check.name, "git-repo");
        assert_eq!(
            check.severity,
            Severity::Error,
            "a non-repo cwd is fatal: {}",
            check.detail
        );
    }

    #[test]
    fn git_repo_check_accepts_a_real_repo() {
        if which("git").is_none() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .arg("init")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git init failed in test setup");
        let check = git_repo_check(tmp.path());
        assert_eq!(check.severity, Severity::Ok, "{}", check.detail);
    }

    fn answer(stdout: &str, stderr: &str, code: i32) -> Result<Output, String> {
        use std::os::unix::process::ExitStatusExt;
        Ok(Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.into(),
            stderr: stderr.into(),
        })
    }

    fn report_line(proto: u32) -> String {
        serde_json::to_string(&HostReport {
            proto,
            version: "9.9.9".into(),
            checks: vec![
                Check {
                    name: "caucus".into(),
                    severity: Severity::Ok,
                    detail: "v9.9.9".into(),
                },
                Check {
                    name: "claude-stop-hook".into(),
                    severity: Severity::Warn,
                    detail: "no `Stop` hook".into(),
                },
            ],
        })
        .unwrap()
    }

    fn summary(checks: &[Check]) -> Vec<(&str, Severity)> {
        checks
            .iter()
            .map(|c| (c.name.as_str(), c.severity))
            .collect()
    }

    #[test]
    fn a_sub_host_report_after_profile_output_names_its_checks_under_the_host() {
        let stdout = format!("welcome to build-1\n\n{}\n", report_line(PROTO_VERSION));
        let checks = sub_host_report("build-1", answer(&stdout, "", 0).and_then(printed_report));
        assert_eq!(
            summary(&checks),
            [
                ("host:build-1", Severity::Ok),
                ("host:build-1:caucus", Severity::Ok),
                ("host:build-1:claude-stop-hook", Severity::Warn),
            ]
        );
        assert!(checks[0].detail.contains("v9.9.9"), "{}", checks[0].detail);
    }

    #[test]
    fn a_sub_host_on_another_protocol_is_a_warning() {
        let checks = sub_host_report(
            "build-1",
            answer(&report_line(PROTO_VERSION + 1), "", 0).and_then(printed_report),
        );
        assert_eq!(checks[0].severity, Severity::Warn);
        assert!(
            checks[0].detail.contains("protocol"),
            "{}",
            checks[0].detail
        );
    }

    #[test]
    fn a_sub_host_without_a_report_says_why() {
        let checks = sub_host_report(
            "old",
            answer(
                "",
                "error: unrecognized subcommand 'doctor'\n\nFor more information, try '--help'.\n",
                2,
            )
            .and_then(printed_report),
        );
        assert_eq!(summary(&checks), [("host:old", Severity::Warn)]);
        assert!(checks[0].detail.contains("--help"), "{}", checks[0].detail);
        let checks = sub_host_report("gone", Err("no answer within 20s".into()));
        assert_eq!(summary(&checks), [("host:gone", Severity::Warn)]);
    }

    #[test]
    fn a_command_past_its_wait_is_killed() {
        let started = Instant::now();
        let out = output_within(Command::new("sleep").arg("10"), Duration::from_millis(100));
        assert!(out.is_err_and(|e| e.contains("within")));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// More output than a pipe holds is read as it comes, not taken for a
    /// command that never ends.
    #[test]
    fn a_command_writing_past_a_pipe_still_answers() {
        let out = output_within(
            Command::new("sh").args(["-c", "yes o | head -c 200000; yes e | head -c 200000 >&2"]),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!((out.stdout.len(), out.stderr.len()), (200_000, 200_000));
    }

    #[test]
    fn task_role_produces_a_warn_check() {
        // Build a config whose project roles.toml grants Task; the loader
        // strips it, so feed the registry a raw spec instead and audit it
        // by hand against the same predicate `run` uses.
        use crate::role::spec::{AgentCli, RoleSpec};
        let spec = RoleSpec {
            name: "rogue".into(),
            description: "d".into(),
            allowed_tools: vec!["Read".into(), "Task".into()],
            permission_mode: "default".into(),
            system_prompt_template: "roles/rogue.md".into(),
            agent_cli: AgentCli::Claude,
            model: None,
        };
        // `RoleSpec::allows_task` is the predicate `run` uses for the audit.
        assert!(spec.allows_task());
    }
}
