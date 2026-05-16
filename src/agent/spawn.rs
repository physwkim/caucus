//! Agent spawn: turn a [`RoleSpec`] into a new panel running the backend CLI
//! plus a fresh [`AgentManifest`]. See `docs/design.md` §5.

use std::path::PathBuf;

use thiserror::Error;

use crate::role::spec::{AgentCli, RoleSpec};
use crate::session::id::{PanelId, SessionId};

use super::manifest::AgentManifest;

/// A request to spawn one agent into a new panel.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub session_id: SessionId,
    /// Role to instantiate.
    pub role: RoleSpec,
    /// CEO-chosen agent name (e.g. `reviewer-r1`).
    pub agent_name: String,
    /// CEO override for the backend CLI. `None` uses the role's `agent_cli`
    /// (`docs/design.md` §0 #9).
    pub agent_cli_override: Option<AgentCli>,
    /// CEO override for the model. `None` uses the role's `model`.
    pub model_override: Option<String>,
    /// Worktree to use as cwd, if this is an execute-phase agent.
    pub worktree_path: Option<PathBuf>,
}

impl SpawnRequest {
    /// Effective backend CLI (override beats role default).
    pub fn effective_cli(&self) -> AgentCli {
        self.agent_cli_override.unwrap_or(self.role.agent_cli)
    }

    /// Effective model (override beats role default).
    pub fn effective_model(&self) -> Option<String> {
        self.model_override
            .clone()
            .or_else(|| self.role.model.clone())
    }
}

/// Errors from spawning an agent.
#[derive(Debug, Error)]
pub enum SpawnError {
    #[error("agent spawn: {0}")]
    Spawn(String),
}

/// Outcome of a successful spawn.
#[derive(Debug)]
pub struct SpawnOutcome {
    pub panel_id: PanelId,
    pub manifest: AgentManifest,
}

/// Spawn an agent for `request`: build the manifest, allocate a panel, launch
/// the backend CLI process in that panel's PTY.
pub(crate) fn spawn(request: &SpawnRequest) -> Result<SpawnOutcome, SpawnError> {
    // TODO(phase 2): allocate a panel via `panel::lifecycle::spawn`, launch
    // `request.effective_cli().binary()` in its PTY with CAUCUS_* env injected
    // (`docs/design.md` §7.1), then persist the manifest via
    // `agent::manifest::write`.
    let panel_id = PanelId::new();
    let manifest = AgentManifest::new(
        request.session_id,
        panel_id,
        request.role.name.clone(),
        request.agent_name.clone(),
        request.effective_cli(),
        request.effective_model(),
    );
    Ok(SpawnOutcome { panel_id, manifest })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role() -> RoleSpec {
        RoleSpec {
            name: "reviewer".into(),
            description: "r".into(),
            allowed_tools: vec!["Read".into()],
            permission_mode: "default".into(),
            system_prompt_template: "roles/reviewer.md".into(),
            agent_cli: AgentCli::Claude,
            model: Some("opus".into()),
        }
    }

    #[test]
    fn override_beats_role_default() {
        let req = SpawnRequest {
            session_id: SessionId::new(),
            role: role(),
            agent_name: "reviewer-r1".into(),
            agent_cli_override: Some(AgentCli::Gemini),
            model_override: Some("flash".into()),
            worktree_path: None,
        };
        assert_eq!(req.effective_cli(), AgentCli::Gemini);
        assert_eq!(req.effective_model().as_deref(), Some("flash"));
    }

    #[test]
    fn falls_back_to_role_default() {
        let req = SpawnRequest {
            session_id: SessionId::new(),
            role: role(),
            agent_name: "reviewer-r1".into(),
            agent_cli_override: None,
            model_override: None,
            worktree_path: None,
        };
        assert_eq!(req.effective_cli(), AgentCli::Claude);
        assert_eq!(req.effective_model().as_deref(), Some("opus"));
    }
}
