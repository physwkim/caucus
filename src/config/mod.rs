//! Configuration: global (`~/.caucus/`) + project (`<repo>/.caucus/`) merge.
//! See `docs/design.md` §0 #7, §6.
//!
//! Layered lookup (later layers override earlier):
//!
//! 1. Embedded defaults (6 built-in roles) — see [`embedded_defaults`].
//! 2. `~/.caucus/roles.toml` — user-global overrides.
//! 3. `<repo>/.caucus/roles.toml` — project overrides (highest precedence).

pub mod roles;

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::role::registry::RoleRegistry;
use crate::role::spec::{AgentCli, RoleSpec};

use roles::{RolesConfig, RolesError};

/// Errors from building the merged configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Roles(#[from] RolesError),
    #[error("home directory not found")]
    NoHome,
}

/// The fully merged caucus configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Merged role registry (embedded defaults + global + project).
    pub roles: RoleRegistry,
    /// `~/.caucus/` — the global config directory, if `$HOME` was set.
    pub global_dir: Option<PathBuf>,
    /// `<repo>/.caucus/` — the project config directory.
    pub project_dir: PathBuf,
}

impl Config {
    /// Load and merge configuration for a project rooted at `repo`.
    ///
    /// Layers: embedded defaults < `~/.caucus/roles.toml` < `<repo>/.caucus/roles.toml`.
    pub fn load(repo: &Path) -> Result<Self, ConfigError> {
        let global_dir = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".caucus"));
        let project_dir = repo.join(".caucus");

        let mut merged = RolesConfig::from(embedded_defaults());
        if let Some(dir) = &global_dir {
            merged = merged.override_with(RolesConfig::load(&dir.join("roles.toml"))?);
        }
        merged = merged.override_with(RolesConfig::load(&project_dir.join("roles.toml"))?);

        Ok(Self {
            roles: RoleRegistry::from_specs(merged.into_specs()),
            global_dir,
            project_dir,
        })
    }
}

/// The six role specs shipped with caucus (`docs/design.md` §13). Bottom layer
/// of the override stack, so caucus always knows the standard roles even with
/// no config files present.
pub fn embedded_defaults() -> Vec<RoleSpec> {
    fn role(
        name: &str,
        desc: &str,
        mode: &str,
        tools: &[&str],
        cli: AgentCli,
        model: Option<&str>,
    ) -> RoleSpec {
        RoleSpec {
            name: name.into(),
            description: desc.into(),
            allowed_tools: tools.iter().map(|t| (*t).to_string()).collect(),
            permission_mode: mode.into(),
            system_prompt_template: format!("roles/{name}.md"),
            agent_cli: cli,
            model: model.map(str::to_string),
        }
    }
    vec![
        role(
            "architect",
            "Designs the approach, decomposes tasks, no code edits.",
            "plan",
            &["Read", "Glob", "Grep", "WebFetch", "WebSearch", "TodoWrite"],
            AgentCli::Claude,
            Some("opus"),
        ),
        role(
            "backend",
            "Implements changes. Full file edit + bash.",
            "acceptEdits",
            &["Read", "Glob", "Grep", "Edit", "Write", "Bash", "TodoWrite"],
            AgentCli::Claude,
            Some("sonnet"),
        ),
        role(
            "reviewer",
            "Reads only. Critiques approach and code.",
            "default",
            &["Read", "Glob", "Grep", "Bash"],
            AgentCli::Claude,
            Some("opus"),
        ),
        role(
            "qa",
            "Runs tests. Reports failures.",
            "default",
            &["Read", "Glob", "Grep", "Bash"],
            AgentCli::Claude,
            Some("haiku"),
        ),
        role(
            "scribe",
            "Compiles the meeting transcript. No external sync.",
            "acceptEdits",
            &["Read", "Glob", "Grep", "Edit", "Write"],
            AgentCli::Claude,
            Some("haiku"),
        ),
        role(
            "serious-reviewer",
            "Adversarial second-opinion reviewer running on codex.",
            "default",
            &["Read", "Glob", "Grep", "Bash"],
            AgentCli::Codex,
            None,
        ),
    ]
}

/// Build a `RolesConfig` from a slice of specs, so the embedded defaults can
/// participate in the same override pipeline.
impl From<Vec<RoleSpec>> for RolesConfig {
    fn from(specs: Vec<RoleSpec>) -> Self {
        use roles::RoleEntry;
        let mut roles = std::collections::BTreeMap::new();
        for s in specs {
            roles.insert(
                s.name,
                RoleEntry {
                    description: s.description,
                    allowed_tools: s.allowed_tools,
                    permission_mode: s.permission_mode,
                    system_prompt_template: s.system_prompt_template,
                    agent_cli: s.agent_cli,
                    model: s.model,
                },
            );
        }
        Self { roles }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn embedded_defaults_contain_six_roles() {
        let names: Vec<_> = embedded_defaults().into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "architect",
                "backend",
                "reviewer",
                "qa",
                "scribe",
                "serious-reviewer",
            ]
        );
    }

    #[test]
    fn serious_reviewer_uses_codex() {
        let specs = embedded_defaults();
        let sr = specs.iter().find(|s| s.name == "serious-reviewer").unwrap();
        assert_eq!(sr.agent_cli, AgentCli::Codex);
    }

    #[test]
    fn config_falls_back_to_defaults_when_no_files() {
        let tmp = TempDir::new().unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert!(cfg.roles.contains("reviewer"));
        assert!(cfg.roles.contains("backend"));
    }
}
