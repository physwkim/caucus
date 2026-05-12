//! Role specification: name, allowed tools, permission mode, prompt template
//! location. Mirrors claw-code's per-type tool allowlist (see
//! `docs/claw-code-analysis.md` §3).

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Claude CLI `--permission-mode` values that caucus knows about. The
/// serde representation matches the exact strings the `claude` binary
/// accepts, so a `roles.toml` value can be copy-pasted from `claude --help`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum PermissionMode {
    /// `--permission-mode default` — Claude asks before any write/bash.
    #[serde(rename = "default")]
    Default,
    /// `--permission-mode acceptEdits` — write/edit are auto-approved.
    /// Bash still prompts.
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    /// `--permission-mode plan` — read-only planning mode.
    #[serde(rename = "plan")]
    Plan,
    /// `--permission-mode bypassPermissions` — skip every prompt. Dangerous;
    /// reserve for sandboxed roles.
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
}

impl PermissionMode {
    pub fn as_cli_arg(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

/// Which agent CLI runs in the pane for a given role.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentCli {
    /// `claude` (Claude Code). Default.
    #[default]
    Claude,
    /// `codex` (OpenAI Codex CLI). Useful as a "serious reviewer" when
    /// Claude gets stuck — see README "Mixing agent CLIs" section.
    Codex,
}

impl AgentCli {
    /// Binary name to invoke. Stays stable; both binaries are expected on
    /// `PATH`.
    pub fn binary(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// Static specification for a role.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleSpec {
    pub name: String,
    pub description: String,
    pub allowed_tools: BTreeSet<String>,
    pub permission_mode: PermissionMode,
    /// Path (relative to the caucus install / repo root) of the system-prompt
    /// markdown file for this role. Resolved at spawn time.
    pub system_prompt_template: PathBuf,
    /// Optional model override for this role. If `None`, the spawning code
    /// uses the request-level model (or [`crate::agent::spawn::DEFAULT_MODEL`]).
    /// Useful for cost control: e.g. `architect = sonnet`, `backend = opus`.
    #[serde(default)]
    pub model: Option<String>,
    /// Which agent CLI to spawn for this role. Defaults to Claude. Set to
    /// `codex` for roles where you want OpenAI Codex as a second opinion.
    #[serde(default)]
    pub agent_cli: AgentCli,
}

impl RoleSpec {
    /// Render `--allowed-tools` as the comma-separated string Claude CLI
    /// accepts.
    pub fn allowed_tools_csv(&self) -> String {
        let mut iter = self.allowed_tools.iter();
        let mut s = match iter.next() {
            Some(first) => first.clone(),
            None => return String::new(),
        };
        for tool in iter {
            s.push(',');
            s.push_str(tool);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str, tools: &[&str], mode: PermissionMode) -> RoleSpec {
        RoleSpec {
            name: name.into(),
            description: format!("test role {name}"),
            allowed_tools: tools.iter().map(|t| (*t).to_string()).collect(),
            permission_mode: mode,
            system_prompt_template: PathBuf::from(format!("roles/{name}.md")),
            model: None,
            agent_cli: AgentCli::Claude,
        }
    }

    #[test]
    fn csv_in_btree_order() {
        let s = sample(
            "reviewer",
            &["Grep", "Glob", "Read"],
            PermissionMode::Default,
        );
        // BTreeSet sorts alphabetically.
        assert_eq!(s.allowed_tools_csv(), "Glob,Grep,Read");
    }

    #[test]
    fn empty_allowlist_yields_empty_csv() {
        let s = sample("noop", &[], PermissionMode::Plan);
        assert_eq!(s.allowed_tools_csv(), "");
    }

    #[test]
    fn permission_mode_cli_args() {
        assert_eq!(PermissionMode::Default.as_cli_arg(), "default");
        assert_eq!(PermissionMode::AcceptEdits.as_cli_arg(), "acceptEdits");
        assert_eq!(PermissionMode::Plan.as_cli_arg(), "plan");
        assert_eq!(
            PermissionMode::BypassPermissions.as_cli_arg(),
            "bypassPermissions"
        );
    }
}
