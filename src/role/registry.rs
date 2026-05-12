//! In-memory lookup over the merged global+project role specs. Built once at
//! CLI startup and shared read-only.

use std::collections::BTreeMap;

use thiserror::Error;

use super::spec::RoleSpec;

/// Read-only role lookup keyed by role name.
#[derive(Debug, Clone, Default)]
pub struct RoleRegistry {
    by_name: BTreeMap<String, RoleSpec>,
}

#[derive(Debug, Error)]
#[error("unknown role: {0}")]
pub struct UnknownRole(pub String);

impl RoleRegistry {
    /// Build from an arbitrary collection of role specs. Later entries with
    /// the same name override earlier ones (mirrors `RolesConfig::override_with`).
    pub fn from_specs<I: IntoIterator<Item = RoleSpec>>(specs: I) -> Self {
        let mut by_name = BTreeMap::new();
        for spec in specs {
            by_name.insert(spec.name.clone(), spec);
        }
        Self { by_name }
    }

    pub fn get(&self, name: &str) -> Result<&RoleSpec, UnknownRole> {
        self.by_name
            .get(name)
            .ok_or_else(|| UnknownRole(name.to_owned()))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::spec::PermissionMode;
    use std::path::PathBuf;

    fn r(name: &str, tool: &str) -> RoleSpec {
        RoleSpec {
            name: name.into(),
            description: format!("{name} role"),
            allowed_tools: [tool.to_string()].into_iter().collect(),
            permission_mode: PermissionMode::Default,
            system_prompt_template: PathBuf::from(format!("roles/{name}.md")),
            model: None,
        }
    }

    #[test]
    fn duplicate_names_last_wins() {
        let registry = RoleRegistry::from_specs(vec![r("dev", "Read"), r("dev", "Bash")]);
        assert_eq!(registry.len(), 1);
        let spec = registry.get("dev").unwrap();
        assert_eq!(spec.allowed_tools_csv(), "Bash");
    }

    #[test]
    fn unknown_yields_error() {
        let registry = RoleRegistry::from_specs(vec![r("dev", "Read")]);
        let err = registry.get("missing").unwrap_err();
        assert_eq!(err.0, "missing");
    }
}
