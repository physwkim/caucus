//! Parse `roles.toml` (`docs/design.md` §6):
//!
//! ```toml
//! [roles.architect]
//! description = "..."
//! allowed_tools = ["Read", "Glob", "Grep", "TodoWrite"]
//! permission_mode = "plan"
//! system_prompt_template = "roles/architect.md"
//! agent_cli = "claude"
//! model = "opus"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

use crate::role::spec::{AgentCli, RoleSpec};

/// On-disk representation of a `roles.toml` file. One entry per role.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RolesConfig {
    pub roles: BTreeMap<String, RoleEntry>,
}

/// One `[roles.<name>]` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleEntry {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default = "default_permission_mode")]
    pub permission_mode: String,
    pub system_prompt_template: String,
    #[serde(default)]
    pub agent_cli: AgentCli,
    #[serde(default)]
    pub model: Option<String>,
}

fn default_permission_mode() -> String {
    "default".to_string()
}

/// Drop the `Task` tool from a role's allowlist (Invariant I-7), warning once
/// per offending role. Returns the filtered list.
fn strip_task(role: &str, tools: Vec<String>) -> Vec<String> {
    if tools.iter().any(|t| t == "Task") {
        warn!(
            role,
            "stripping forbidden `Task` tool from role allowlist — nested \
             in-session sub-agents are invisible to caucus (design.md §0 #13, \
             Invariant I-7)"
        );
        tools.into_iter().filter(|t| t != "Task").collect()
    } else {
        tools
    }
}

/// Errors from loading `roles.toml`.
#[derive(Debug, Error)]
pub enum RolesError {
    #[error("roles io ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("roles toml ({path}): {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

impl RolesConfig {
    /// Read one `roles.toml`. A missing file yields an empty config so the
    /// global+project merge can short-circuit.
    pub fn load(path: &Path) -> Result<Self, RolesError> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|source| RolesError::Toml {
                path: path.to_owned(),
                source,
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(RolesError::Io {
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// Convert into [`RoleSpec`]s.
    ///
    /// The forbidden `Task` tool (`docs/design.md` §0 #13, Invariant I-7) is
    /// stripped from every role's `allowed_tools` here, with a `warn!` per
    /// offending role — a nested in-session sub-agent would be invisible to
    /// caucus, so it never reaches a spawned agent's allowlist.
    pub fn into_specs(self) -> Vec<RoleSpec> {
        self.roles
            .into_iter()
            .map(|(name, entry)| {
                let allowed_tools = strip_task(&name, entry.allowed_tools);
                RoleSpec {
                    name,
                    description: entry.description,
                    allowed_tools,
                    permission_mode: entry.permission_mode,
                    system_prompt_template: entry.system_prompt_template,
                    agent_cli: entry.agent_cli,
                    model: entry.model,
                }
            })
            .collect()
    }

    /// Override `self` with entries from `other` — roles defined in `other`
    /// fully replace the version in `self` (project beats global).
    pub fn override_with(mut self, other: Self) -> Self {
        for (name, entry) in other.roles {
            self.roles.insert(name, entry);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_file_yields_empty_config() {
        let tmp = TempDir::new().unwrap();
        let cfg = RolesConfig::load(&tmp.path().join("nope.toml")).unwrap();
        assert!(cfg.roles.is_empty());
    }

    #[test]
    fn parses_minimal_role() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("roles.toml");
        std::fs::write(
            &path,
            r#"
                [roles.reviewer]
                description = "read-only critic"
                allowed_tools = ["Read", "Glob", "Grep"]
                permission_mode = "default"
                system_prompt_template = "roles/reviewer.md"
            "#,
        )
        .unwrap();
        let specs = RolesConfig::load(&path).unwrap().into_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "reviewer");
        assert_eq!(specs[0].allowed_tools_csv(), "Read,Glob,Grep");
    }

    #[test]
    fn project_overrides_global() {
        let global = RolesConfig {
            roles: BTreeMap::from([(
                "backend".to_string(),
                RoleEntry {
                    description: "g".into(),
                    allowed_tools: vec!["Read".into()],
                    permission_mode: "default".into(),
                    system_prompt_template: "g/backend.md".into(),
                    agent_cli: AgentCli::Claude,
                    model: None,
                },
            )]),
        };
        let project = RolesConfig {
            roles: BTreeMap::from([(
                "backend".to_string(),
                RoleEntry {
                    description: "p".into(),
                    allowed_tools: vec!["Read".into(), "Edit".into()],
                    permission_mode: "acceptEdits".into(),
                    system_prompt_template: "p/backend.md".into(),
                    agent_cli: AgentCli::Claude,
                    model: None,
                },
            )]),
        };
        let merged = global.override_with(project);
        let specs = merged.into_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].permission_mode, "acceptEdits");
    }

    #[test]
    fn task_tool_is_stripped_on_load() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("roles.toml");
        std::fs::write(
            &path,
            r#"
                [roles.rogue]
                description = "tries to nest"
                allowed_tools = ["Read", "Task", "Grep"]
                permission_mode = "default"
                system_prompt_template = "roles/rogue.md"
            "#,
        )
        .unwrap();
        let specs = RolesConfig::load(&path).unwrap().into_specs();
        assert_eq!(specs.len(), 1);
        assert!(
            !specs[0].allows_task(),
            "Task must be stripped from a loaded role (Invariant I-7)"
        );
        assert_eq!(specs[0].allowed_tools, vec!["Read", "Grep"]);
    }
}
