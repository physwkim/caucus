//! Environment health check (`caucus doctor`). Reports tmux/git/claude
//! availability plus role-config status.

use std::path::Path;
use std::process::Command;

use serde::Serialize;

/// One probe result.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Aggregate report for `caucus doctor`.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub checks: Vec<CheckResult>,
}

impl DoctorReport {
    pub fn is_healthy(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }
}

/// Run every probe and assemble a `DoctorReport`.
pub fn run(repo: &Path) -> DoctorReport {
    let checks = vec![
        probe_binary("tmux", &["-V"]),
        probe_binary("git", &["--version"]),
        probe_binary("claude", &["--version"]),
        probe_caucus_dir(repo),
        probe_hook_script(repo),
        probe_hook_registered(repo),
        probe_roles(repo),
    ];
    DoctorReport { checks }
}

fn probe_hook_registered(repo: &Path) -> CheckResult {
    let Some(home) = std::env::var_os("HOME") else {
        return CheckResult {
            name: "hook registered".into(),
            ok: false,
            detail: "HOME not set".into(),
        };
    };
    let path = std::path::PathBuf::from(home)
        .join(".claude")
        .join("settings.json");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(err) => {
            return CheckResult {
                name: "hook registered".into(),
                ok: false,
                detail: format!(
                    "{} unreadable: {err} — run `caucus init --install-hook`",
                    path.display()
                ),
            };
        }
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(err) => {
            return CheckResult {
                name: "hook registered".into(),
                ok: false,
                detail: format!("{} parse failed: {err}", path.display()),
            };
        }
    };
    let needle = repo
        .join(".caucus")
        .join("bin")
        .join("sentinel-stop")
        .display()
        .to_string();
    let stops = value
        .pointer("/hooks/Stop")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let found = stops.iter().any(|block| {
        block
            .get("hooks")
            .and_then(|v| v.as_array())
            .map(|hooks| {
                hooks
                    .iter()
                    .any(|h| h.get("command").and_then(|c| c.as_str()) == Some(needle.as_str()))
            })
            .unwrap_or(false)
    });
    CheckResult {
        name: "hook registered".into(),
        ok: found,
        detail: if found {
            format!("Stop hook at {needle} present in {}", path.display())
        } else {
            format!(
                "{} has no Stop hook pointing at {needle} — run `caucus init --install-hook`",
                path.display()
            )
        },
    }
}

fn probe_binary(name: &str, args: &[&str]) -> CheckResult {
    match Command::new(name).args(args).output() {
        Ok(out) if out.status.success() => CheckResult {
            name: name.into(),
            ok: true,
            detail: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        },
        Ok(out) => CheckResult {
            name: name.into(),
            ok: false,
            detail: format!(
                "exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        },
        Err(err) => CheckResult {
            name: name.into(),
            ok: false,
            detail: format!("not found on PATH: {err}"),
        },
    }
}

fn probe_caucus_dir(repo: &Path) -> CheckResult {
    let dir = repo.join(".caucus");
    if dir.exists() {
        CheckResult {
            name: ".caucus dir".into(),
            ok: true,
            detail: dir.display().to_string(),
        }
    } else {
        CheckResult {
            name: ".caucus dir".into(),
            ok: false,
            detail: format!("missing — run `caucus init` in {}", repo.display()),
        }
    }
}

fn probe_hook_script(repo: &Path) -> CheckResult {
    let path = repo.join(".caucus").join("bin").join("sentinel-stop");
    if path.exists() {
        CheckResult {
            name: "sentinel hook".into(),
            ok: true,
            detail: path.display().to_string(),
        }
    } else {
        CheckResult {
            name: "sentinel hook".into(),
            ok: false,
            detail: format!("missing at {} — run `caucus init`", path.display()),
        }
    }
}

fn probe_roles(repo: &Path) -> CheckResult {
    match crate::config::RegistryBuilder::new()
        .with_project_root(repo)
        .build()
    {
        Ok(registry) => CheckResult {
            name: "roles".into(),
            ok: !registry.is_empty(),
            detail: format!(
                "{} role(s): {}",
                registry.len(),
                registry.names().collect::<Vec<_>>().join(", ")
            ),
        },
        Err(err) => CheckResult {
            name: "roles".into(),
            ok: false,
            detail: err.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn report_is_unhealthy_on_missing_caucus_dir() {
        let tmp = TempDir::new().unwrap();
        let report = run(tmp.path());
        // .caucus dir + hook will both fail; everything else is environmental.
        assert!(!report.is_healthy());
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == ".caucus dir" && !c.ok)
        );
    }

    #[test]
    fn embedded_roles_are_always_present() {
        let tmp = TempDir::new().unwrap();
        let report = run(tmp.path());
        let roles = report.checks.iter().find(|c| c.name == "roles").unwrap();
        assert!(roles.ok);
        assert!(roles.detail.contains("reviewer"));
    }
}
