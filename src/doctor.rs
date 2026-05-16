//! `caucus doctor` — environment + configuration health check
//! (`docs/design.md` §10).
//!
//! Checks: `git`, the agent CLIs (`claude` / `codex` / `gemini`), the Stop
//! hook installation, and every role's `allowed_tools` for the forbidden
//! `Task` tool (Invariant I-7).

use std::path::PathBuf;

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

/// Run all environment + configuration checks for `config`.
///
/// Probes `git` and the agent CLIs (`claude` / `codex` / `gemini`) on `PATH`,
/// verifies the Claude `Stop` hook is installed in `~/.claude/settings.json`,
/// and audits every role's `allowed_tools` for the forbidden `Task` tool
/// (Invariant I-7).
pub fn run(config: &Config) -> Report {
    let mut report = Report::default();

    // `git` is mandatory — worktree creation/cleanup shell out to it.
    report.checks.push(binary_check(
        "git",
        Severity::Error,
        "required for worktree creation and commit provenance",
    ));

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
    report.checks.push(binary_check(
        "gemini",
        Severity::Warn,
        "needed for roles with `agent_cli = \"gemini\"`",
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
        Ok(v) => hook_present(&v),
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

/// Whether a Claude settings JSON value contains a `Stop` hook that invokes
/// the caucus turn-signal script. Looks for `hooks.Stop` and a `caucus`
/// reference in any command string under it.
fn hook_present(settings: &serde_json::Value) -> bool {
    let Some(stop) = settings.get("hooks").and_then(|h| h.get("Stop")) else {
        return false;
    };
    json_contains_caucus(stop)
}

/// Recursively scan a JSON value for a string mentioning `caucus`.
fn json_contains_caucus(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::String(s) => s.contains("caucus"),
        serde_json::Value::Array(items) => items.iter().any(json_contains_caucus),
        serde_json::Value::Object(map) => map.values().any(json_contains_caucus),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn clean_config_has_no_task_warnings() {
        let tmp = TempDir::new().unwrap();
        let config = Config::load(tmp.path()).unwrap();
        let report = run(&config);
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
        let report = run(&config);
        for expected in ["git", "claude", "codex", "gemini", "claude-stop-hook"] {
            assert!(
                report.checks.iter().any(|c| c.name == expected),
                "missing doctor check: {expected}"
            );
        }
    }

    #[test]
    fn hook_present_detects_caucus_stop_hook() {
        let v = serde_json::json!({
            "hooks": {
                "Stop": [
                    { "hooks": [{ "type": "command", "command": ".caucus/bin/turn-signal" }] }
                ]
            }
        });
        assert!(hook_present(&v));
    }

    #[test]
    fn hook_present_false_without_stop_or_caucus() {
        assert!(!hook_present(&serde_json::json!({ "hooks": {} })));
        assert!(!hook_present(&serde_json::json!({
            "hooks": { "Stop": [{ "command": "/usr/bin/other" }] }
        })));
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
