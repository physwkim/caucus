//! Role specification: name, description, allowed tools, permission mode,
//! prompt template, agent CLI, model. See `docs/design.md` §6.

use serde::{Deserialize, Serialize};

/// Which agent CLI runs in the panel for a given role (`docs/design.md` §0 #9).
/// Serialised lowercase so `roles.toml` reads `agent_cli = "claude"`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentCli {
    /// `claude` (Claude Code). Default when `agent_cli` is omitted.
    #[default]
    Claude,
    /// `codex` (OpenAI Codex CLI). Useful as an adversarial second opinion.
    Codex,
    /// `gemini` (Google Gemini CLI).
    Gemini,
}

impl AgentCli {
    /// Binary name to invoke. Both/all binaries are expected on `PATH`.
    pub fn binary(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }
}

/// Static specification for a role (`docs/design.md` §6 / §9).
///
/// `permission_mode` is kept as a free-form `String` matching the exact
/// `--permission-mode` value the backend CLI accepts (e.g. `default`,
/// `acceptEdits`, `plan`, `bypassPermissions`), so a `roles.toml` value can be
/// copy-pasted straight from the CLI's help text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleSpec {
    pub name: String,
    pub description: String,
    /// Tool allowlist. `Task` MUST NOT appear here (`docs/design.md` §0 #13,
    /// Invariant I-7) — `caucus doctor` warns if it does.
    pub allowed_tools: Vec<String>,
    /// `--permission-mode` value passed to the backend CLI verbatim.
    pub permission_mode: String,
    /// Path (relative to the caucus install / repo root) of the system-prompt
    /// markdown file for this role. Resolved at spawn time.
    pub system_prompt_template: String,
    /// Which agent CLI to spawn for this role. Defaults to Claude.
    #[serde(default)]
    pub agent_cli: AgentCli,
    /// Optional model override. `None` means the CLI's own default tier.
    #[serde(default)]
    pub model: Option<String>,
}

impl RoleSpec {
    /// Render `allowed_tools` as the comma-separated string the CLIs accept.
    pub fn allowed_tools_csv(&self) -> String {
        self.allowed_tools.join(",")
    }

    /// Whether this role's allowlist contains the forbidden `Task` tool
    /// (Invariant I-7). `caucus doctor` surfaces this.
    pub fn allows_task(&self) -> bool {
        self.allowed_tools.iter().any(|t| t == "Task")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str, tools: &[&str]) -> RoleSpec {
        RoleSpec {
            name: name.into(),
            description: format!("test role {name}"),
            allowed_tools: tools.iter().map(|t| (*t).to_string()).collect(),
            permission_mode: "default".into(),
            system_prompt_template: format!("roles/{name}.md"),
            agent_cli: AgentCli::Claude,
            model: None,
        }
    }

    #[test]
    fn csv_preserves_order() {
        let s = sample("reviewer", &["Read", "Glob", "Grep"]);
        assert_eq!(s.allowed_tools_csv(), "Read,Glob,Grep");
    }

    #[test]
    fn detects_forbidden_task_tool() {
        assert!(sample("x", &["Read", "Task"]).allows_task());
        assert!(!sample("x", &["Read", "Grep"]).allows_task());
    }

    #[test]
    fn agent_cli_serde_is_lowercase() {
        assert_eq!(serde_json::to_string(&AgentCli::Gemini).unwrap(), "\"gemini\"");
    }
}
