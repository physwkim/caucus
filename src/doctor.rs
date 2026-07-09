//! `caucus doctor` — environment + configuration health check
//! (`docs/design.md` §10).
//!
//! Checks: the running caucus version + `caucus` on `PATH`, `git`, that the cwd
//! is a git repository, the agent CLIs (`claude` / `codex`), the Stop hook
//! chain (present in settings, its command runnable on this machine, and a
//! live end-to-end signal delivery), and every role's `allowed_tools` for the
//! forbidden `Task` tool (Invariant I-7).

use std::path::{Path, PathBuf};

use crate::config::Config;

/// Severity of a single check result.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Severity {
    /// Everything is fine.
    Ok,
    /// Non-fatal — caucus runs, but something is degraded.
    Warn,
    /// Fatal — caucus cannot operate correctly.
    Error,
}

/// The outcome of one doctor check.
#[derive(Debug, Clone)]
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
/// Reports the running caucus version (and that `caucus` is on `PATH` for the
/// turn-signal hook), probes `git` and the agent CLIs (`claude` / `codex`) on
/// `PATH`, confirms `repo` is a git repository, verifies the Claude `Stop` hook
/// is installed in `~/.claude/settings.json`, and audits every role's
/// `allowed_tools` for the forbidden `Task` tool (Invariant I-7).
pub fn run(repo: &Path, config: &Config) -> Report {
    let mut report = Report::default();

    // Identify the running build first — and confirm `caucus` is on PATH, since
    // the turn-signal hook script execs it by bare name.
    report.checks.push(caucus_check());

    // `git` is mandatory — worktree creation/cleanup shell out to it.
    report.checks.push(binary_check(
        "git",
        Severity::Error,
        "required for worktree creation and commit provenance",
    ));

    // ...and the cwd must actually be a git repository, or those shell-outs
    // fail later, confusingly, at the first role spawn.
    report.checks.push(git_repo_check(repo));

    // The agent CLIs: a missing one is a warning, not fatal — a session may
    // only use a subset (e.g. claude-only). `caucus` itself still runs.
    report.checks.push(binary_check(
        "claude",
        Severity::Warn,
        "the default agent backend; required unless every role overrides it",
    ));
    report.checks.push(binary_check(
        "codex",
        Severity::Warn,
        "needed for roles with `agent_cli = \"codex\"` (e.g. serious-reviewer)",
    ));

    // Claude Stop hook — turn-completion signals depend on it (§7). Presence
    // in settings.json, then the hook command's existence on *this* machine,
    // then a live end-to-end delivery test.
    report.checks.extend(stop_hook_checks());

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

    report
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
    let version = env!("CARGO_PKG_VERSION");
    match which("caucus") {
        Some(p) => Check {
            name: "caucus".into(),
            severity: Severity::Ok,
            detail: format!("v{version} (on PATH at {})", p.display()),
        },
        None => Check {
            name: "caucus".into(),
            severity: Severity::Warn,
            detail: format!(
                "v{version} running, but `caucus` is not on PATH — the turn-signal \
                 hook (`exec caucus signal post`) cannot run; install caucus on PATH"
            ),
        },
    }
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
    let commands = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => crate::hook::caucus_stop_hook_commands(&v),
        Err(err) => {
            return vec![Check {
                name,
                severity: Severity::Warn,
                detail: format!("{} is not valid JSON: {err}", settings.display()),
            }];
        }
    };
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
    ]
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
/// require bytes to arrive on the socket within [`SELFTEST_DEADLINE`].
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
                break conn.read(&mut buf).map(|n| n > 0).unwrap_or(false);
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
    let output = child
        .wait_with_output()
        .map_err(|e| format!("hook did not deliver and could not be reaped: {e}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
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
        let config = Config::load(tmp.path()).unwrap();
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

    #[test]
    fn run_includes_binary_and_hook_checks() {
        let tmp = TempDir::new().unwrap();
        let config = Config::load(tmp.path()).unwrap();
        let report = run(tmp.path(), &config);
        for expected in [
            "caucus",
            "git",
            "git-repo",
            "claude",
            "codex",
            "claude-stop-hook",
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
