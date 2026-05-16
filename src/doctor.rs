//! `caucus doctor` — environment + configuration health check
//! (`docs/design.md` §10).
//!
//! Checks: `git`, the agent CLIs (`claude` / `codex` / `gemini`), the Stop
//! hook installation, and every role's `allowed_tools` for the forbidden
//! `Task` tool (Invariant I-7).

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
/// Phase 2 fills in the `which`-style probes for `git`/`claude`/`codex`/
/// `gemini` and the hook-installation check. The role `Task` audit
/// (Invariant I-7) is implemented now since it needs no environment.
pub fn run(config: &Config) -> Report {
    let mut report = Report::default();

    // TODO(phase 2): probe `git`, `claude`, `codex`, `gemini` on PATH and
    // verify the Stop hook is installed in `~/.claude/settings.json`.

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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn clean_config_has_no_task_warnings() {
        let tmp = TempDir::new().unwrap();
        let config = Config::load(tmp.path()).unwrap();
        let report = run(&config);
        assert!(
            report
                .checks
                .iter()
                .all(|c| !c.name.starts_with("role:"))
        );
    }

    #[test]
    fn worst_of_empty_report_is_ok() {
        assert_eq!(Report::default().worst(), Severity::Ok);
    }
}
