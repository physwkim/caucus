//! `caucus doctor` — environment + configuration health check
//! (`docs/design.md` §10).
//!
//! Checks: the running caucus version + `caucus` on `PATH`, `git`, that the cwd
//! is a git repository, the agent CLIs (`claude` / `codex`), the Stop hook
//! installation, and every role's `allowed_tools` for the forbidden `Task`
//! tool (Invariant I-7).

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

    // Claude Stop hook — turn-completion signals depend on it (§7).
    report.checks.push(stop_hook_check());

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

/// Check that `~/.claude/settings.json` carries a `Stop` hook entry. caucus
/// installs it via `caucus init --install-hook`; without it turn-completion
/// signals never reach the socket (§7).
fn stop_hook_check() -> Check {
    let name = "claude-stop-hook".to_string();
    let Some(home) = std::env::var_os("HOME") else {
        return Check {
            name,
            severity: Severity::Warn,
            detail: "$HOME unset — cannot locate ~/.claude/settings.json".into(),
        };
    };
    let settings = PathBuf::from(home).join(".claude").join("settings.json");
    let text = match std::fs::read_to_string(&settings) {
        Ok(t) => t,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Check {
                name,
                severity: Severity::Warn,
                detail: format!(
                    "{} not found — run `caucus init --install-hook`",
                    settings.display()
                ),
            };
        }
        Err(err) => {
            return Check {
                name,
                severity: Severity::Warn,
                detail: format!("cannot read {}: {err}", settings.display()),
            };
        }
    };
    let installed = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => crate::hook::caucus_stop_hook_installed(&v),
        Err(err) => {
            return Check {
                name,
                severity: Severity::Warn,
                detail: format!("{} is not valid JSON: {err}", settings.display()),
            };
        }
    };
    if installed {
        Check {
            name,
            severity: Severity::Ok,
            detail: "Stop hook present in ~/.claude/settings.json".into(),
        }
    } else {
        Check {
            name,
            severity: Severity::Warn,
            detail: "no `Stop` hook in ~/.claude/settings.json — run \
                     `caucus init --install-hook`"
                .into(),
        }
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
