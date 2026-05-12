//! Parse `roles.toml`. The file structure mirrors `docs/design.md` §6:
//!
//! ```toml
//! [roles.architect]
//! description = "..."
//! allowed_tools = ["Read", "Glob", "Grep", "TodoWrite"]
//! permission_mode = "default"
//! system_prompt_template = "roles/architect.md"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::role::spec::{PermissionMode, RoleSpec};

/// On-disk representation of a `roles.toml` file. One entry per role,
/// keyed by role name (e.g. `[roles.reviewer]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RolesConfig {
    pub roles: BTreeMap<String, RoleEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleEntry {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default = "default_permission_mode")]
    pub permission_mode: PermissionMode,
    pub system_prompt_template: PathBuf,
    /// Optional per-role model override; mirrors `RoleSpec::model`.
    #[serde(default)]
    pub model: Option<String>,
}

const fn default_permission_mode() -> PermissionMode {
    PermissionMode::Default
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
    /// Read a single `roles.toml` from disk. Missing files yield an empty
    /// config (so global+project merge can short-circuit).
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

    /// Convert into a vector of `RoleSpec`, applying defaults.
    pub fn into_specs(self) -> Vec<RoleSpec> {
        self.roles
            .into_iter()
            .map(|(name, entry)| RoleSpec {
                name,
                description: entry.description,
                allowed_tools: entry.allowed_tools.into_iter().collect(),
                permission_mode: entry.permission_mode,
                system_prompt_template: entry.system_prompt_template,
                model: entry.model,
            })
            .collect()
    }

    /// Override `self` with entries from `other`. Roles defined in `other`
    /// fully replace the version in `self` (no field-level merge — keeps the
    /// semantics simple to reason about).
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

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn missing_file_yields_empty_config() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope.toml");
        let cfg = RolesConfig::load(&missing).unwrap();
        assert!(cfg.roles.is_empty());
    }

    #[test]
    fn parses_minimal_role() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            tmp.path(),
            "roles.toml",
            r#"
                [roles.reviewer]
                description = "read-only critic"
                allowed_tools = ["Read", "Glob", "Grep"]
                permission_mode = "default"
                system_prompt_template = "roles/reviewer.md"
            "#,
        );
        let cfg = RolesConfig::load(&path).unwrap();
        let specs = cfg.into_specs();
        assert_eq!(specs.len(), 1);
        let r = &specs[0];
        assert_eq!(r.name, "reviewer");
        assert_eq!(r.permission_mode, PermissionMode::Default);
        assert_eq!(r.allowed_tools_csv(), "Glob,Grep,Read");
        assert_eq!(r.system_prompt_template, PathBuf::from("roles/reviewer.md"));
    }

    #[test]
    fn project_overrides_global() {
        let tmp = TempDir::new().unwrap();
        let g_path = write(
            tmp.path(),
            "global.toml",
            r#"
                [roles.backend]
                allowed_tools = ["Read"]
                system_prompt_template = "g/backend.md"
            "#,
        );
        let p_path = write(
            tmp.path(),
            "project.toml",
            r#"
                [roles.backend]
                allowed_tools = ["Read", "Edit", "Write"]
                permission_mode = "acceptEdits"
                system_prompt_template = "p/backend.md"
            "#,
        );
        let global = RolesConfig::load(&g_path).unwrap();
        let project = RolesConfig::load(&p_path).unwrap();
        let merged = global.override_with(project);
        let specs = merged.into_specs();
        assert_eq!(specs.len(), 1);
        let r = &specs[0];
        assert_eq!(r.name, "backend");
        assert_eq!(r.permission_mode, PermissionMode::AcceptEdits);
        assert_eq!(r.allowed_tools_csv(), "Edit,Read,Write");
        assert_eq!(r.system_prompt_template, PathBuf::from("p/backend.md"));
    }

    #[test]
    fn rejects_invalid_permission_mode() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            tmp.path(),
            "roles.toml",
            r#"
                [roles.x]
                permission_mode = "nuke"
                system_prompt_template = "x.md"
            "#,
        );
        let err = RolesConfig::load(&path).unwrap_err();
        assert!(matches!(err, RolesError::Toml { .. }));
    }
}
