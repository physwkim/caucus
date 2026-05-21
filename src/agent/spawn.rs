//! Agent spawn: turn a [`RoleSpec`] into a new panel running the backend CLI
//! plus a fresh [`AgentManifest`]. See `docs/design.md` §5.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

use thiserror::Error;

use crate::pty::PtyCommand;
use crate::role::spec::{AgentCli, RoleSpec};
use crate::session::id::{PanelId, SessionId};

use super::manifest::AgentManifest;

/// A request to spawn one agent into a new panel.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub session_id: SessionId,
    /// Role to instantiate.
    pub role: RoleSpec,
    /// Main-worker-chosen agent name (e.g. `reviewer-r1`).
    pub agent_name: String,
    /// Main worker override for the backend CLI. `None` uses the role's
    /// `agent_cli` (`docs/design.md` §0 #9).
    pub agent_cli_override: Option<AgentCli>,
    /// Main worker override for the model. `None` uses the role's `model`.
    pub model_override: Option<String>,
    /// Worktree to use as cwd, if this is an execute-phase agent.
    pub worktree_path: Option<PathBuf>,
    /// Session repo root — the directory caucus was launched in
    /// (`std::env::current_dir`). Used as the panel's cwd when it has no
    /// worktree, so a non-worktree panel (the main worker, and any
    /// `worktree=false` sub-agent) runs in the repo the user started caucus in
    /// rather than `$HOME`. Empty `PathBuf` means "unset" (test/default), which
    /// leaves the cwd unspecified.
    pub repo_root: PathBuf,
    /// Path to the caucus turn-signal socket for this session
    /// (`docs/design.md` §7.1). Injected into the agent as `CAUCUS_SOCK`.
    /// `None` when no socket is wired (e.g. unit tests).
    #[allow(clippy::struct_field_names)]
    pub sock_path: Option<PathBuf>,
    /// When `true`, pass the backend CLI's "skip every permission prompt"
    /// flag. The role's `allowed_tools` allowlist remains the real safety
    /// boundary; this only suppresses interactive confirmations that would
    /// otherwise stall an agent inside a non-interactive panel.
    pub skip_permissions: bool,
    /// Path to an MCP-config JSON to register with the backend CLI
    /// (`docs/design.md` §0 #4). Set for the main worker panel so its Claude Code
    /// instance loads the caucus MCP server; `None` for every other panel.
    /// Honoured only by the `claude` backend (`--mcp-config`).
    pub mcp_config_path: Option<PathBuf>,
    /// Claude Code conversation id to resume (`claude --resume <id>`). Set on
    /// the resume launch path so a relaunched agent continues its prior
    /// conversation. Honoured only by the `claude` backend — codex/gemini have
    /// no standard resume flag, so for those it is ignored and the agent
    /// spawns fresh.
    pub resume_session_id: Option<String>,
}

impl Default for SpawnRequest {
    fn default() -> Self {
        Self {
            session_id: SessionId::new(),
            role: RoleSpec {
                name: String::new(),
                description: String::new(),
                allowed_tools: Vec::new(),
                permission_mode: "default".into(),
                system_prompt_template: String::new(),
                agent_cli: AgentCli::Claude,
                model: None,
            },
            agent_name: String::new(),
            agent_cli_override: None,
            model_override: None,
            worktree_path: None,
            repo_root: PathBuf::new(),
            sock_path: None,
            skip_permissions: false,
            mcp_config_path: None,
            resume_session_id: None,
        }
    }
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
    /// The backend CLI command the panel owner launches in the new PTY.
    pub command: PtyCommand,
}

/// Build the [`PtyCommand`] that launches the backend CLI for `request`
/// into the panel identified by `panel_id` (`docs/design.md` §5, §7.1).
///
/// Branches on the effective [`AgentCli`]: `claude` / `codex` / `gemini` have
/// distinct flag surfaces. The `CAUCUS_*` env vars (§7.1) are injected so the
/// agent's turn-signal hook can post back to the caucus socket.
pub(crate) fn build_command(request: &SpawnRequest, panel_id: PanelId) -> PtyCommand {
    let cli = request.effective_cli();
    let model = request.effective_model();

    let args: Vec<OsString> = match cli {
        AgentCli::Claude => claude_args(
            &request.role,
            model.as_deref(),
            request.skip_permissions,
            request.mcp_config_path.as_deref(),
            request.resume_session_id.as_deref(),
        ),
        // codex/gemini have no standard resume flag — `resume_session_id` is
        // intentionally ignored for them and the agent spawns fresh.
        AgentCli::Codex => codex_args(&request.role, model.as_deref(), request.skip_permissions),
        AgentCli::Gemini => gemini_args(&request.role, model.as_deref(), request.skip_permissions),
    };

    let mut env: HashMap<String, String> = HashMap::new();
    env.insert(
        "CAUCUS_SESSION_ID".to_string(),
        request.session_id.to_string(),
    );
    env.insert("CAUCUS_PANEL_ID".to_string(), panel_id.to_string());
    if let Some(sock) = &request.sock_path {
        env.insert("CAUCUS_SOCK".to_string(), sock.display().to_string());
    }

    // A panel runs in its worktree if it has one, otherwise the session repo
    // root (the launch dir). Leaving this `None` is not equivalent to "inherit
    // caucus's cwd" — portable-pty resolves an unset cwd to `$HOME`, which is
    // why a non-worktree panel would otherwise start in the home directory.
    let cwd = request.worktree_path.clone().or_else(|| {
        let root = &request.repo_root;
        (!root.as_os_str().is_empty()).then(|| root.clone())
    });

    PtyCommand {
        program: OsString::from(cli.binary()),
        args,
        cwd,
        env,
    }
}

/// `claude` argv: `--model`, `--permission-mode`, `--allowedTools`, optionally
/// `--dangerously-skip-permissions`, `--mcp-config <path>` for the main
/// worker panel (so its claude loads the caucus MCP server), and
/// `--resume <id>` on the resume launch path.
fn claude_args(
    role: &RoleSpec,
    model: Option<&str>,
    skip_permissions: bool,
    mcp_config: Option<&std::path::Path>,
    resume_session_id: Option<&str>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    if let Some(id) = resume_session_id {
        args.push("--resume".into());
        args.push(id.into());
    }
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args.push("--permission-mode".into());
    args.push(role.permission_mode.as_str().into());
    let tools = role.allowed_tools_csv();
    if !tools.is_empty() {
        args.push("--allowedTools".into());
        args.push(tools.into());
    }
    if skip_permissions {
        args.push("--dangerously-skip-permissions".into());
    }
    if let Some(path) = mcp_config {
        args.push("--mcp-config".into());
        args.push(path.into());
    }
    args
}

/// `codex` argv: `--model` (omitted when unset — codex rejects claude model
/// ids), and either `--dangerously-bypass-approvals-and-sandbox` or a
/// `--sandbox` level coarsely matching the role's permission mode.
fn codex_args(role: &RoleSpec, model: Option<&str>, skip_permissions: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    if skip_permissions {
        args.push("--dangerously-bypass-approvals-and-sandbox".into());
    } else {
        // Map the role's permission mode onto a codex sandbox level.
        let sandbox = match role.permission_mode.as_str() {
            "acceptEdits" => "workspace-write",
            "bypassPermissions" => "danger-full-access",
            // `default` / `plan` / anything stricter → read-only.
            _ => "read-only",
        };
        args.push("--sandbox".into());
        args.push(sandbox.into());
    }
    args
}

/// `gemini` argv: `--model`, and `--yolo` for the skip-permissions flag.
fn gemini_args(role: &RoleSpec, model: Option<&str>, skip_permissions: bool) -> Vec<OsString> {
    let _ = role;
    let mut args: Vec<OsString> = Vec::new();
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    if skip_permissions {
        // gemini's "approve everything" flag.
        args.push("--yolo".into());
    }
    args
}

/// Spawn an agent for `request`: build the manifest, allocate a panel id and
/// the backend CLI command, hand both back to the caller.
///
/// The panel allocation and PTY launch are owned by `panel::lifecycle::spawn`
/// (Invariant I-5); this function produces the [`PtyCommand`] and the fresh
/// [`AgentManifest`] that the panel owner consumes.
pub(crate) fn spawn(request: &SpawnRequest) -> Result<SpawnOutcome, SpawnError> {
    let panel_id = PanelId::new();
    let command = build_command(request, panel_id);
    let manifest = AgentManifest::new(
        request.session_id,
        panel_id,
        request.role.name.clone(),
        request.agent_name.clone(),
        request.effective_cli(),
        request.effective_model(),
    );
    Ok(SpawnOutcome {
        panel_id,
        manifest,
        command,
    })
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
            role: role(),
            agent_name: "reviewer-r1".into(),
            agent_cli_override: Some(AgentCli::Gemini),
            model_override: Some("flash".into()),
            ..SpawnRequest::default()
        };
        assert_eq!(req.effective_cli(), AgentCli::Gemini);
        assert_eq!(req.effective_model().as_deref(), Some("flash"));
    }

    #[test]
    fn falls_back_to_role_default() {
        let req = SpawnRequest {
            role: role(),
            agent_name: "reviewer-r1".into(),
            ..SpawnRequest::default()
        };
        assert_eq!(req.effective_cli(), AgentCli::Claude);
        assert_eq!(req.effective_model().as_deref(), Some("opus"));
    }

    fn args_of(cmd: &PtyCommand) -> Vec<String> {
        cmd.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn claude_argv_has_model_mode_and_tools() {
        let mut r = role();
        r.allowed_tools = vec!["Read".into(), "Grep".into()];
        r.permission_mode = "plan".into();
        let req = SpawnRequest {
            role: r,
            agent_name: "reviewer-r1".into(),
            ..SpawnRequest::default()
        };
        let cmd = build_command(&req, PanelId::new());
        assert_eq!(cmd.program, OsString::from("claude"));
        let args = args_of(&cmd);
        assert!(args.windows(2).any(|w| w == ["--model", "opus"]));
        assert!(args.windows(2).any(|w| w == ["--permission-mode", "plan"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--allowedTools", "Read,Grep"])
        );
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn claude_skip_permissions_flag() {
        let req = SpawnRequest {
            role: role(),
            agent_name: "x".into(),
            skip_permissions: true,
            ..SpawnRequest::default()
        };
        let cmd = build_command(&req, PanelId::new());
        assert!(args_of(&cmd).contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn codex_argv_uses_sandbox_for_permission_mode() {
        let mut r = role();
        r.agent_cli = AgentCli::Codex;
        r.permission_mode = "acceptEdits".into();
        r.model = None;
        let req = SpawnRequest {
            role: r,
            agent_name: "sr".into(),
            ..SpawnRequest::default()
        };
        let cmd = build_command(&req, PanelId::new());
        assert_eq!(cmd.program, OsString::from("codex"));
        let args = args_of(&cmd);
        assert!(
            args.windows(2)
                .any(|w| w == ["--sandbox", "workspace-write"])
        );
        // No claude model id leaks into a codex invocation.
        assert!(!args.contains(&"--model".to_string()));
    }

    #[test]
    fn codex_bypass_flag_when_skip_permissions() {
        let mut r = role();
        r.agent_cli = AgentCli::Codex;
        r.model = None;
        let req = SpawnRequest {
            role: r,
            agent_name: "sr".into(),
            skip_permissions: true,
            ..SpawnRequest::default()
        };
        let cmd = build_command(&req, PanelId::new());
        let args = args_of(&cmd);
        assert!(args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert!(!args.contains(&"--sandbox".to_string()));
    }

    #[test]
    fn gemini_argv_uses_yolo_for_skip_permissions() {
        let mut r = role();
        r.agent_cli = AgentCli::Gemini;
        r.model = Some("flash".into());
        let req = SpawnRequest {
            role: r,
            agent_name: "g".into(),
            skip_permissions: true,
            ..SpawnRequest::default()
        };
        let cmd = build_command(&req, PanelId::new());
        assert_eq!(cmd.program, OsString::from("gemini"));
        let args = args_of(&cmd);
        assert!(args.windows(2).any(|w| w == ["--model", "flash"]));
        assert!(args.contains(&"--yolo".to_string()));
    }

    #[test]
    fn claude_argv_includes_mcp_config_when_set() {
        let req = SpawnRequest {
            role: role(),
            agent_name: "main-1".into(),
            mcp_config_path: Some(PathBuf::from("/tmp/caucus/.mcp.json")),
            ..SpawnRequest::default()
        };
        let cmd = build_command(&req, PanelId::new());
        let args = args_of(&cmd);
        assert!(
            args.windows(2)
                .any(|w| w == ["--mcp-config", "/tmp/caucus/.mcp.json"])
        );
    }

    #[test]
    fn claude_argv_omits_mcp_config_by_default() {
        let req = SpawnRequest {
            role: role(),
            agent_name: "backend-1".into(),
            ..SpawnRequest::default()
        };
        let cmd = build_command(&req, PanelId::new());
        assert!(!args_of(&cmd).contains(&"--mcp-config".to_string()));
    }

    #[test]
    fn claude_argv_includes_resume_when_set() {
        let req = SpawnRequest {
            role: role(),
            agent_name: "reviewer-r1".into(),
            resume_session_id: Some("conv-9f3a".into()),
            ..SpawnRequest::default()
        };
        let cmd = build_command(&req, PanelId::new());
        let args = args_of(&cmd);
        assert!(
            args.windows(2).any(|w| w == ["--resume", "conv-9f3a"]),
            "claude argv must carry --resume <id>: {args:?}"
        );
    }

    #[test]
    fn claude_argv_omits_resume_by_default() {
        let req = SpawnRequest {
            role: role(),
            agent_name: "reviewer-r1".into(),
            ..SpawnRequest::default()
        };
        let cmd = build_command(&req, PanelId::new());
        assert!(!args_of(&cmd).contains(&"--resume".to_string()));
    }

    /// codex/gemini have no standard resume flag — a set `resume_session_id`
    /// must be ignored, not leaked onto their argv.
    #[test]
    fn codex_and_gemini_ignore_resume_session_id() {
        for cli in [AgentCli::Codex, AgentCli::Gemini] {
            let mut r = role();
            r.agent_cli = cli;
            r.model = None;
            let req = SpawnRequest {
                role: r,
                agent_name: "x".into(),
                resume_session_id: Some("conv-xyz".into()),
                ..SpawnRequest::default()
            };
            let cmd = build_command(&req, PanelId::new());
            let args = args_of(&cmd);
            assert!(
                !args.contains(&"--resume".to_string()),
                "{cli:?} must ignore resume_session_id: {args:?}"
            );
            assert!(!args.contains(&"conv-xyz".to_string()));
        }
    }

    #[test]
    fn caucus_env_is_injected() {
        let sock = PathBuf::from("/tmp/caucus.sock");
        let req = SpawnRequest {
            role: role(),
            agent_name: "x".into(),
            sock_path: Some(sock.clone()),
            ..SpawnRequest::default()
        };
        let panel_id = PanelId::new();
        let cmd = build_command(&req, panel_id);
        assert_eq!(
            cmd.env.get("CAUCUS_PANEL_ID").map(String::as_str),
            Some(panel_id.to_string().as_str())
        );
        assert_eq!(
            cmd.env.get("CAUCUS_SESSION_ID").map(String::as_str),
            Some(req.session_id.to_string().as_str())
        );
        assert_eq!(
            cmd.env.get("CAUCUS_SOCK").map(String::as_str),
            Some("/tmp/caucus.sock")
        );
    }

    #[test]
    fn worktree_path_becomes_command_cwd() {
        let wt = PathBuf::from("/repo/.caucus/worktrees/s-backend-1");
        let req = SpawnRequest {
            role: role(),
            agent_name: "x".into(),
            worktree_path: Some(wt.clone()),
            repo_root: PathBuf::from("/repo"),
            ..SpawnRequest::default()
        };
        let cmd = build_command(&req, PanelId::new());
        // A worktree wins over the repo root.
        assert_eq!(cmd.cwd.as_deref(), Some(wt.as_path()));
    }

    #[test]
    fn repo_root_becomes_cwd_without_a_worktree() {
        // No worktree (the main worker, or a worktree=false sub-agent): the cwd
        // must be the session repo root, not `None` — an unset cwd makes
        // portable-pty spawn the child in `$HOME`.
        let repo = PathBuf::from("/repo");
        let req = SpawnRequest {
            role: role(),
            agent_name: "x".into(),
            worktree_path: None,
            repo_root: repo.clone(),
            ..SpawnRequest::default()
        };
        let cmd = build_command(&req, PanelId::new());
        assert_eq!(cmd.cwd.as_deref(), Some(repo.as_path()));
    }

    #[test]
    fn empty_repo_root_leaves_cwd_unspecified() {
        // The default/test boundary: no worktree and an empty repo root leaves
        // the cwd `None` rather than an empty path.
        let req = SpawnRequest {
            role: role(),
            agent_name: "x".into(),
            worktree_path: None,
            repo_root: PathBuf::new(),
            ..SpawnRequest::default()
        };
        let cmd = build_command(&req, PanelId::new());
        assert_eq!(cmd.cwd, None);
    }

    #[test]
    fn spawn_yields_consistent_panel_id() {
        let req = SpawnRequest {
            role: role(),
            agent_name: "x".into(),
            ..SpawnRequest::default()
        };
        let outcome = spawn(&req).unwrap();
        assert_eq!(outcome.panel_id, outcome.manifest.panel_id);
        assert_eq!(
            outcome
                .command
                .env
                .get("CAUCUS_PANEL_ID")
                .map(String::as_str),
            Some(outcome.panel_id.to_string().as_str())
        );
    }
}
