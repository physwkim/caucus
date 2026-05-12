//! Global + project configuration loading (`~/.caucus/`, `<repo>/.caucus/`).
//!
//! Layered lookup (later entries override earlier):
//!
//! 1. Embedded defaults (5 built-in roles) — see [`embedded_defaults`].
//! 2. `~/.caucus/roles.toml` — user-global overrides.
//! 3. `<repo>/.caucus/roles.toml` — project overrides.
//!
//! See `docs/design.md` §6.

pub mod roles;

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::role::registry::RoleRegistry;
use crate::role::spec::{PermissionMode, RoleSpec};

use roles::{RolesConfig, RolesError};

/// Errors from building a [`RoleRegistry`] from disk.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Roles(#[from] RolesError),
    #[error("home directory not found")]
    NoHome,
}

/// Builder collecting the path of each layered roles file.
#[derive(Debug, Default)]
pub struct RegistryBuilder {
    global_path: Option<PathBuf>,
    project_path: Option<PathBuf>,
}

impl RegistryBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Path to `~/.caucus/roles.toml`. Missing file is fine.
    pub fn with_global_default(mut self) -> Result<Self, ConfigError> {
        let home = std::env::var_os("HOME").ok_or(ConfigError::NoHome)?;
        let path = PathBuf::from(home).join(".caucus").join("roles.toml");
        self.global_path = Some(path);
        Ok(self)
    }

    /// Path to `<repo>/.caucus/roles.toml`. Missing file is fine.
    pub fn with_project_root(mut self, repo: &Path) -> Self {
        self.project_path = Some(repo.join(".caucus").join("roles.toml"));
        self
    }

    pub fn build(self) -> Result<RoleRegistry, ConfigError> {
        let mut merged = RolesConfig::from(embedded_defaults());
        if let Some(p) = self.global_path {
            merged = merged.override_with(RolesConfig::load(&p)?);
        }
        if let Some(p) = self.project_path {
            merged = merged.override_with(RolesConfig::load(&p)?);
        }
        Ok(RoleRegistry::from_specs(merged.into_specs()))
    }
}

/// The five role specs that ship with caucus. Used as the bottom layer of
/// the override stack so that `caucus` always knows about `architect`,
/// `backend`, `reviewer`, `qa`, `scribe` even with no config files present.
pub fn embedded_defaults() -> Vec<RoleSpec> {
    use crate::role::spec::AgentCli;
    // Cost-tier baseline. Per the precedence rule in
    // `agent::spawn::resolve_model`, an operator's `--model` flag still
    // overrides any of these; they're just the role-typed *default*.
    //
    // - Opus when the role's job is "find subtle issues" or "decide the
    //   shape of the work" (architect, reviewer): the reasoning quality
    //   gap is meaningful.
    // - Sonnet when the role's job is "execute a defined plan" (backend):
    //   Sonnet codes well at a fraction of Opus's cost.
    // - Haiku when the role's job is mechanical (qa runs tests + classifies
    //   failures; scribe stitches markdown). No deep reasoning needed.
    const OPUS: &str = "claude-opus-4-7";
    const SONNET: &str = "claude-sonnet-4-6";
    const HAIKU: &str = "claude-haiku-4-5";

    fn role(
        name: &str,
        desc: &str,
        mode: PermissionMode,
        tools: &[&str],
        model: Option<&str>,
    ) -> RoleSpec {
        RoleSpec {
            name: name.into(),
            description: desc.into(),
            allowed_tools: tools.iter().map(|t| (*t).to_string()).collect(),
            permission_mode: mode,
            system_prompt_template: PathBuf::from(format!("roles/{name}.md")),
            model: model.map(|s| s.to_string()),
            agent_cli: AgentCli::Claude,
        }
    }
    let claude_roles = [
        role(
            "architect",
            "Designs the approach, decomposes tasks, no code edits.",
            PermissionMode::Plan,
            &["Read", "Glob", "Grep", "WebFetch", "WebSearch", "TodoWrite"],
            Some(OPUS),
        ),
        role(
            "backend",
            "Implements changes. Full file edit + bash.",
            PermissionMode::AcceptEdits,
            &["Read", "Glob", "Grep", "Edit", "Write", "Bash", "TodoWrite"],
            Some(SONNET),
        ),
        role(
            "reviewer",
            "Reads only. Critiques approach and code.",
            PermissionMode::Default,
            &["Read", "Glob", "Grep", "Bash"],
            Some(OPUS),
        ),
        role(
            "qa",
            "Runs tests. Reports failures.",
            PermissionMode::Default,
            &["Read", "Glob", "Grep", "Bash"],
            Some(HAIKU),
        ),
        role(
            "scribe",
            "Compiles final meeting transcript. No external sync.",
            PermissionMode::AcceptEdits,
            &["Read", "Glob", "Grep", "Edit", "Write"],
            Some(HAIKU),
        ),
    ];

    // serious-reviewer runs on codex instead of claude — used as an
    // adversarial second opinion when Claude review stalls or rubber-stamps.
    // No `model` on the Rust side; codex picks its own default unless the
    // operator overrides via roles.toml.
    let mut codex_reviewer = role(
        "serious-reviewer",
        "Adversarial second-opinion reviewer running on codex.",
        PermissionMode::Default,
        &["Read", "Glob", "Grep", "Bash"],
        None,
    );
    codex_reviewer.agent_cli = AgentCli::Codex;

    let mut out = Vec::with_capacity(claude_roles.len() + 1);
    out.extend(claude_roles);
    out.push(codex_reviewer);
    out
}

/// Convenience: build a `RolesConfig` from a slice of specs (so the embedded
/// defaults can participate in the same override pipeline).
impl From<Vec<RoleSpec>> for RolesConfig {
    fn from(specs: Vec<RoleSpec>) -> Self {
        use roles::RoleEntry;
        let mut roles = std::collections::BTreeMap::new();
        for s in specs {
            roles.insert(
                s.name,
                RoleEntry {
                    description: s.description,
                    allowed_tools: s.allowed_tools.into_iter().collect(),
                    permission_mode: s.permission_mode,
                    system_prompt_template: s.system_prompt_template,
                    model: s.model,
                    agent_cli: s.agent_cli,
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
    fn embedded_defaults_contain_all_six_roles() {
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
        use crate::role::spec::AgentCli;
        let specs = embedded_defaults();
        let sr = specs.iter().find(|s| s.name == "serious-reviewer").unwrap();
        assert_eq!(sr.agent_cli, AgentCli::Codex);
    }

    #[test]
    fn embedded_model_tiers_match_cost_optimization() {
        // Lock in the cost-tier baseline so a future refactor can't silently
        // re-default everything to Opus.
        let specs = embedded_defaults();
        let model = |name: &str| {
            specs
                .iter()
                .find(|s| s.name == name)
                .and_then(|s| s.model.clone())
        };
        assert_eq!(model("architect").as_deref(), Some("claude-opus-4-7"));
        assert_eq!(model("backend").as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(model("reviewer").as_deref(), Some("claude-opus-4-7"));
        assert_eq!(model("qa").as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(model("scribe").as_deref(), Some("claude-haiku-4-5"));
        // serious-reviewer is codex; no Claude model is meaningful.
        assert_eq!(model("serious-reviewer"), None);
    }

    #[test]
    fn registry_falls_back_to_defaults_when_no_files() {
        let tmp = TempDir::new().unwrap(); // empty repo
        let registry = RegistryBuilder::new()
            .with_project_root(tmp.path())
            .build()
            .unwrap();
        assert!(registry.contains("reviewer"));
        assert!(registry.contains("backend"));
    }

    #[test]
    fn project_override_changes_permission_mode() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".caucus");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("roles.toml"),
            r#"
                [roles.reviewer]
                allowed_tools = ["Read"]
                permission_mode = "plan"
                system_prompt_template = "roles/reviewer.md"
            "#,
        )
        .unwrap();
        let registry = RegistryBuilder::new()
            .with_project_root(tmp.path())
            .build()
            .unwrap();
        let r = registry.get("reviewer").unwrap();
        assert_eq!(r.permission_mode, PermissionMode::Plan);
        assert_eq!(r.allowed_tools_csv(), "Read");
    }
}
