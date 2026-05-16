//! In-memory lookup over the merged global+project role specs. Built once at
//! startup and shared read-only.

use std::collections::BTreeMap;

use thiserror::Error;
use tracing::warn;

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
    /// the same name override earlier ones (project overrides global).
    ///
    /// Final enforcement point for Invariant I-7 (`docs/design.md` §0 #13):
    /// the forbidden `Task` tool is stripped from every spec's `allowed_tools`
    /// before it lands in the registry, so no code path can spawn an agent
    /// with `Task` granted even if a spec slipped past the config loader.
    pub fn from_specs<I: IntoIterator<Item = RoleSpec>>(specs: I) -> Self {
        let mut by_name = BTreeMap::new();
        for mut spec in specs {
            if spec.allows_task() {
                warn!(
                    role = %spec.name,
                    "stripping forbidden `Task` tool from role allowlist \
                     (design.md §0 #13, Invariant I-7)"
                );
                spec.allowed_tools.retain(|t| t != "Task");
            }
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

    pub fn specs(&self) -> impl Iterator<Item = &RoleSpec> {
        self.by_name.values()
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
    use crate::role::spec::{AgentCli, RoleSpec};

    fn r(name: &str, tool: &str) -> RoleSpec {
        RoleSpec {
            name: name.into(),
            description: format!("{name} role"),
            allowed_tools: vec![tool.to_string()],
            permission_mode: "default".into(),
            system_prompt_template: format!("roles/{name}.md"),
            agent_cli: AgentCli::Claude,
            model: None,
        }
    }

    #[test]
    fn duplicate_names_last_wins() {
        let registry = RoleRegistry::from_specs(vec![r("dev", "Read"), r("dev", "Bash")]);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get("dev").unwrap().allowed_tools_csv(), "Bash");
    }

    #[test]
    fn unknown_yields_error() {
        let registry = RoleRegistry::from_specs(vec![r("dev", "Read")]);
        assert_eq!(registry.get("missing").unwrap_err().0, "missing");
    }

    #[test]
    fn from_specs_strips_task_tool() {
        let mut spec = r("rogue", "Read");
        spec.allowed_tools.push("Task".into());
        let registry = RoleRegistry::from_specs(vec![spec]);
        let got = registry.get("rogue").unwrap();
        assert!(
            !got.allows_task(),
            "registry must strip `Task` (Invariant I-7)"
        );
        assert_eq!(got.allowed_tools, vec!["Read"]);
    }
}
