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

/// The caucus MCP server registration for the main worker panel: the absolute
/// `caucus` binary and the control socket it serves on. The main worker drives
/// the sub-agent panels through this server's tools (`spawn_role`, `list_panels`,
/// ...). claude registers it via the written `.mcp.json` (`mcp_config_path`,
/// `--mcp-config`); codex has no such flag, so its argv injects
/// `-c mcp_servers.caucus.{command,args}` built from this. `None` for every
/// non-main panel — a sub-agent is not an orchestrator and must not receive the
/// caucus tool surface.
#[derive(Debug, Clone)]
pub struct CaucusMcp {
    pub caucus_bin: PathBuf,
    pub control_sock: PathBuf,
}

/// codex turn-completion wiring for one panel.
///
/// codex has no equivalent of claude's `Stop` hook (the turn-signal mechanism
/// in `docs/design.md` §7), so without this a codex panel never reports its
/// turn as done: caucus would keep it `Working` forever and a registered round
/// would settle only at its fallback deadline. codex *does* invoke a `notify`
/// program on `agent-turn-complete`, passing the event JSON as a trailing
/// argument (verified empirically — `-c notify=[...]` is honoured in the
/// interactive TUI mode caucus drives over a PTY). caucus registers
/// `caucus signal codex-notify` as that program, baking this panel's signal
/// socket, session, and panel id into the argv so the short-lived notify
/// process can post the *same* `TurnSignal{Stop}` the claude Stop hook posts —
/// landing both backends on one turn-completion owner (`handle_signal`). Set
/// for every codex panel (`build_command`); unused by the claude backend.
#[derive(Debug, Clone)]
pub struct CodexNotify {
    pub caucus_bin: PathBuf,
    pub signal_sock: PathBuf,
    pub session_id: SessionId,
    pub panel_id: PanelId,
}

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
    /// Session-scoped storage root (`<repo>/.caucus/sessions/<id>/`,
    /// `Session::root_dir`). Injected into the agent as `CAUCUS_SESSION_DIR`:
    /// a shared path every panel can reach — even a `worktree=true` panel,
    /// whose cwd is an isolated worktree — for handoff artifacts (e.g. a
    /// review doc passed between a reviewer and a fixer). Empty `PathBuf` means
    /// "unset" (test/default), which omits the var (mirrors `repo_root`).
    pub session_dir: PathBuf,
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
    /// conversation. Honoured only by the `claude` backend — codex has no
    /// standard resume flag, so for it the id is ignored and the agent spawns
    /// fresh.
    pub resume_session_id: Option<String>,
    /// The role's system-prompt text — either an inline prompt from a free-form
    /// `spawn_role` call or the text resolved from `role.system_prompt_template`
    /// (`crate::role::prompt::resolve`). `None` when the role configures no
    /// prompt. Injected via `claude --append-system-prompt` and via codex
    /// `-c instructions=<text>` (`build_command`). For a sub-agent panel,
    /// `build_command` appends the caucus question contract
    /// ([`crate::role::prompt::SUBAGENT_QUESTION_CONTRACT`]) to this text —
    /// or injects it alone when `None`.
    pub system_prompt: Option<String>,
    /// caucus MCP server registration — set only for the main worker panel (the
    /// orchestrator). The codex backend consumes it via `-c mcp_servers.caucus.*`;
    /// the claude backend uses `mcp_config_path` instead. `None` for sub-agents.
    pub caucus_mcp: Option<CaucusMcp>,
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
            session_dir: PathBuf::new(),
            sock_path: None,
            skip_permissions: false,
            mcp_config_path: None,
            resume_session_id: None,
            system_prompt: None,
            caucus_mcp: None,
        }
    }
}

impl SpawnRequest {
    /// Whether this request spawns the **main worker** (orchestrator) panel —
    /// exactly the panel that receives the caucus MCP registration
    /// (`caucus_mcp`, set only by `spawn_main_panel_resume`). Every other
    /// panel is a sub-agent, which gates the sub-agent-only spawn policy:
    /// the question contract and the `AskUserQuestion` disallow
    /// (`build_command`, `claude_args`).
    pub fn is_main(&self) -> bool {
        self.caucus_mcp.is_some()
    }

    /// Effective backend CLI (override beats role default).
    pub fn effective_cli(&self) -> AgentCli {
        self.agent_cli_override.unwrap_or(self.role.agent_cli)
    }

    /// Effective model. An explicit `model_override` always wins. Otherwise the
    /// role's model applies — *unless* a CLI override switched the backend away
    /// from the role's native `agent_cli`: a model id tuned for one backend
    /// (e.g. a claude `opus`) is invalid for another (codex rejects claude ids),
    /// so fall back to the new CLI's own default tier rather than leaking the
    /// native model. This is what lets `--agent-cli codex` (or a codex
    /// `spawn_role` override) reuse a claude-default role without erroring.
    pub fn effective_model(&self) -> Option<String> {
        if let Some(m) = &self.model_override {
            return Some(m.clone());
        }
        match self.agent_cli_override {
            Some(cli) if cli != self.role.agent_cli => None,
            _ => self.role.model.clone(),
        }
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
/// Branches on the effective [`AgentCli`]: `claude` / `codex` have distinct
/// flag surfaces. The `CAUCUS_*` env vars (§7.1) are injected so the
/// agent's turn-signal hook can post back to the caucus socket.
pub(crate) fn build_command(request: &SpawnRequest, panel_id: PanelId) -> PtyCommand {
    let cli = request.effective_cli();
    let model = request.effective_model();

    // Every sub-agent prompt carries the caucus question contract (ask in
    // plain text and end the turn — `role::prompt::SUBAGENT_QUESTION_CONTRACT`):
    // no human sits at a sub-agent's panel, so an interactive chooser stalls it
    // `Working` with no turn signal (§8.3). Appended here, the single spawn
    // path, so it covers preset roles, free-form inline prompts, and roles
    // with no prompt, on both backends. The main worker is exempt — its panel
    // is the one the human actually watches.
    let system_prompt = if request.is_main() {
        request.system_prompt.clone()
    } else {
        Some(match &request.system_prompt {
            Some(p) => format!("{p}\n\n{}", crate::role::prompt::SUBAGENT_QUESTION_CONTRACT),
            None => crate::role::prompt::SUBAGENT_QUESTION_CONTRACT.to_string(),
        })
    };

    let args: Vec<OsString> = match cli {
        AgentCli::Claude => claude_args(
            &request.role,
            model.as_deref(),
            request.skip_permissions,
            request.mcp_config_path.as_deref(),
            request.resume_session_id.as_deref(),
            system_prompt.as_deref(),
            request.is_main(),
        ),
        // codex has no standard resume flag — `resume_session_id` is
        // intentionally ignored and the agent spawns fresh. It also has no
        // system-prompt *flag*, but its `-c instructions=<text>` config override
        // sets the agent's base instructions, so the role prompt is injected
        // that way (`codex_args`).
        AgentCli::Codex => {
            // Every codex panel needs turn-completion wiring (codex has no
            // Stop hook): register `caucus signal codex-notify` as codex's
            // `notify` program, baking in this panel's signal socket / session
            // / panel id. Skipped only when no signal socket is wired (unit
            // tests) — then the panel relies on the round fallback as before.
            // The notify program is this exact running caucus binary
            // (`current_exe`, the same path `tui::caucus_bin` resolves).
            let notify = request.sock_path.as_ref().map(|sock| CodexNotify {
                caucus_bin: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("caucus")),
                signal_sock: sock.clone(),
                session_id: request.session_id,
                panel_id,
            });
            codex_args(
                &request.role,
                model.as_deref(),
                request.skip_permissions,
                system_prompt.as_deref(),
                request.caucus_mcp.as_ref(),
                notify.as_ref(),
            )
        }
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
    // A guaranteed-shared path for inter-panel handoff artifacts, reachable
    // even from an isolated worktree cwd. Empty means "unset" (test/default).
    if !request.session_dir.as_os_str().is_empty() {
        env.insert(
            "CAUCUS_SESSION_DIR".to_string(),
            request.session_dir.display().to_string(),
        );
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

/// `claude` argv: `--model`, `--permission-mode`, `--allowedTools`,
/// `--append-system-prompt <text>` for the role's system prompt, optionally
/// `--dangerously-skip-permissions`, `--mcp-config <path>` for the main
/// worker panel (so its claude loads the caucus MCP server),
/// `--disallowedTools AskUserQuestion` for every sub-agent panel, and
/// `--resume <id>` on the resume launch path.
fn claude_args(
    role: &RoleSpec,
    model: Option<&str>,
    skip_permissions: bool,
    mcp_config: Option<&std::path::Path>,
    resume_session_id: Option<&str>,
    system_prompt: Option<&str>,
    is_main: bool,
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
    // Append the role's guidance to claude's default system prompt (the role
    // .md is "claw-code constraints + role-specific", `design.md` §6.1).
    if let Some(prompt) = system_prompt {
        args.push("--append-system-prompt".into());
        args.push(prompt.into());
    }
    args.push("--permission-mode".into());
    args.push(role.permission_mode.as_str().into());
    let tools = role.allowed_tools_csv();
    if !tools.is_empty() {
        args.push("--allowedTools".into());
        args.push(tools.into());
    }
    // No human sits at a sub-agent's panel: an AskUserQuestion chooser would
    // stall it `Working` with no turn signal (§8.3). Disallow the tool; the
    // question contract (`build_command`) has the model ask in plain text and
    // end its turn instead, so the round report carries the question to the
    // main worker. The main worker keeps the tool — the human answers its
    // menus directly.
    if !is_main {
        args.push("--disallowedTools".into());
        args.push("AskUserQuestion".into());
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
/// ids), the role's system prompt as `-c instructions=<text>` (codex's base
/// instructions config override — it has no `--append-system-prompt` flag),
/// the caucus MCP server as `-c mcp_servers.caucus.*` for the main worker panel
/// (codex has no `--mcp-config`), the turn-completion `notify` program as
/// `-c notify=[...]` (codex's stand-in for claude's missing Stop hook —
/// [`CodexNotify`]), and either `--dangerously-bypass-approvals-and-sandbox` or
/// a `--sandbox` level coarsely matching the role's permission mode.
///
/// The instructions value is passed raw (no TOML quoting): codex parses a
/// `-c` value as TOML and falls back to the literal string when that fails, so
/// a multi-line markdown role prompt lands verbatim as the agent's base
/// instructions.
fn codex_args(
    role: &RoleSpec,
    model: Option<&str>,
    skip_permissions: bool,
    system_prompt: Option<&str>,
    caucus_mcp: Option<&CaucusMcp>,
    notify: Option<&CodexNotify>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    if let Some(prompt) = system_prompt {
        args.push("-c".into());
        let mut kv = OsString::from("instructions=");
        kv.push(prompt);
        args.push(kv);
    }
    // The main worker drives sub-agents through the caucus MCP server. codex
    // has no `--mcp-config`; register it via config override, which codex
    // *does* honour (verified: codex starts the server and discovers its tools,
    // unlike the trust gate that ignores `-c`). The command path is quoted as a
    // TOML string; the args are a TOML string array.
    if let Some(mcp) = caucus_mcp {
        args.push("-c".into());
        let mut command = OsString::from("mcp_servers.caucus.command=\"");
        command.push(mcp.caucus_bin.as_os_str());
        command.push("\"");
        args.push(command);

        args.push("-c".into());
        let mut server_args =
            OsString::from("mcp_servers.caucus.args=[\"mcp-serve\", \"--control-sock\", \"");
        server_args.push(mcp.control_sock.as_os_str());
        server_args.push("\"]");
        args.push(server_args);
    }
    // Turn-completion signalling: codex invokes this `notify` program on
    // `agent-turn-complete` with the event JSON appended as a final argument.
    // `caucus signal codex-notify` parses that JSON and posts the same
    // `TurnSignal{Stop}` claude's Stop hook posts, so a codex panel settles to
    // `Idle` the moment its turn ends (see [`CodexNotify`]). The program path
    // and args are TOML strings; codex parses the `-c` value as a TOML array.
    if let Some(n) = notify {
        args.push("-c".into());
        let mut v = OsString::from("notify=[\"");
        v.push(n.caucus_bin.as_os_str());
        v.push("\",\"signal\",\"codex-notify\",\"--sock\",\"");
        v.push(n.signal_sock.as_os_str());
        v.push("\",\"--session\",\"");
        v.push(n.session_id.to_string());
        v.push("\",\"--panel\",\"");
        v.push(n.panel_id.to_string());
        v.push("\"]");
        args.push(v);
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
    use crate::role::prompt::SUBAGENT_QUESTION_CONTRACT;

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
            agent_cli_override: Some(AgentCli::Codex),
            model_override: Some("o3".into()),
            ..SpawnRequest::default()
        };
        assert_eq!(req.effective_cli(), AgentCli::Codex);
        assert_eq!(req.effective_model().as_deref(), Some("o3"));
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

    /// A CLI override that switches the backend away from the role's native
    /// `agent_cli` drops the role's (native-tuned) model: a claude `opus` id
    /// must not leak onto a codex invocation. This is what makes
    /// `--agent-cli codex` reuse the claude-default `main` role without codex
    /// rejecting the model.
    #[test]
    fn override_to_a_different_backend_drops_the_roles_native_model() {
        let req = SpawnRequest {
            role: role(), // claude + opus
            agent_name: "main-1".into(),
            agent_cli_override: Some(AgentCli::Codex),
            ..SpawnRequest::default()
        };
        assert_eq!(req.effective_cli(), AgentCli::Codex);
        assert_eq!(
            req.effective_model(),
            None,
            "claude model must not leak onto a codex override"
        );
    }

    /// An override matching the role's native backend keeps the role model.
    #[test]
    fn override_to_the_same_backend_keeps_the_role_model() {
        let req = SpawnRequest {
            role: role(), // claude + opus
            agent_name: "x".into(),
            agent_cli_override: Some(AgentCli::Claude),
            ..SpawnRequest::default()
        };
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

    /// A sub-agent's resolved role prompt is injected with the caucus question
    /// contract appended — ask in plain text and end the turn, never through
    /// an interactive chooser (`build_command`).
    #[test]
    fn claude_argv_injects_resolved_system_prompt_with_question_contract() {
        let req = SpawnRequest {
            role: role(),
            agent_name: "reviewer-r1".into(),
            system_prompt: Some("You are a reviewer.".into()),
            ..SpawnRequest::default()
        };
        let args = args_of(&build_command(&req, PanelId::new()));
        let expected = format!("You are a reviewer.\n\n{SUBAGENT_QUESTION_CONTRACT}");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--append-system-prompt" && w[1] == expected),
            "args: {args:?}"
        );
    }

    /// A sub-agent whose role configures no prompt still gets the question
    /// contract — the contract matters most exactly when the role gives no
    /// guidance of its own.
    #[test]
    fn claude_subagent_without_role_prompt_still_gets_the_contract() {
        let req = SpawnRequest {
            role: role(),
            agent_name: "reviewer-r1".into(),
            ..SpawnRequest::default()
        };
        let args = args_of(&build_command(&req, PanelId::new()));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--append-system-prompt" && w[1] == SUBAGENT_QUESTION_CONTRACT),
            "args: {args:?}"
        );
    }

    /// The main worker's prompt is passed through untouched — no contract. Its
    /// panel is the one the human actually watches, so it may ask directly.
    #[test]
    fn claude_main_prompt_is_not_rewritten() {
        let req = SpawnRequest {
            role: role(),
            agent_name: "main-1".into(),
            system_prompt: Some("You are the main worker.".into()),
            caucus_mcp: Some(CaucusMcp {
                caucus_bin: PathBuf::from("/usr/local/bin/caucus"),
                control_sock: PathBuf::from("/repo/.caucus/sessions/S1/control.sock"),
            }),
            ..SpawnRequest::default()
        };
        let args = args_of(&build_command(&req, PanelId::new()));
        assert!(
            args.windows(2)
                .any(|w| w == ["--append-system-prompt", "You are the main worker."]),
            "args: {args:?}"
        );
    }

    /// A main worker with no role prompt carries no `--append-system-prompt`
    /// at all — the contract is sub-agent-only.
    #[test]
    fn claude_main_argv_omits_system_prompt_when_unset() {
        let req = SpawnRequest {
            role: role(),
            agent_name: "main-1".into(),
            caucus_mcp: Some(CaucusMcp {
                caucus_bin: PathBuf::from("/usr/local/bin/caucus"),
                control_sock: PathBuf::from("/repo/.caucus/sessions/S1/control.sock"),
            }),
            ..SpawnRequest::default()
        };
        let args = args_of(&build_command(&req, PanelId::new()));
        assert!(!args.contains(&"--append-system-prompt".to_string()));
    }

    /// Every claude sub-agent disallows `AskUserQuestion`: nothing answers an
    /// interactive chooser in a sub-agent panel, so the menu would stall it
    /// `Working` with no turn signal. The question contract has it ask in
    /// plain text instead.
    #[test]
    fn claude_subagent_argv_disallows_ask_user_question() {
        let req = SpawnRequest {
            role: role(),
            agent_name: "reviewer-r1".into(),
            ..SpawnRequest::default()
        };
        let args = args_of(&build_command(&req, PanelId::new()));
        assert!(
            args.windows(2)
                .any(|w| w == ["--disallowedTools", "AskUserQuestion"]),
            "args: {args:?}"
        );
    }

    /// The main worker keeps `AskUserQuestion` — the human answers its menus
    /// directly in the TUI.
    #[test]
    fn claude_main_argv_keeps_ask_user_question() {
        let req = SpawnRequest {
            role: role(),
            agent_name: "main-1".into(),
            caucus_mcp: Some(CaucusMcp {
                caucus_bin: PathBuf::from("/usr/local/bin/caucus"),
                control_sock: PathBuf::from("/repo/.caucus/sessions/S1/control.sock"),
            }),
            ..SpawnRequest::default()
        };
        let args = args_of(&build_command(&req, PanelId::new()));
        assert!(!args.contains(&"--disallowedTools".to_string()));
    }

    /// codex has no `--append-system-prompt` flag, so a resolved role prompt is
    /// injected via the `-c instructions=<text>` config override instead — the
    /// value passed raw (no TOML quoting) as one argv element. A codex
    /// sub-agent's prompt carries the question contract too: the contract is
    /// backend-neutral (codex has no `--disallowedTools`, so the prompt is its
    /// only enforcement).
    #[test]
    fn codex_argv_injects_system_prompt_as_instructions_config() {
        let mut r = role();
        r.agent_cli = AgentCli::Codex;
        r.model = None;
        let req = SpawnRequest {
            role: r,
            agent_name: "sr".into(),
            system_prompt: Some("ROLE GUIDANCE\nsecond line".into()),
            ..SpawnRequest::default()
        };
        let args = args_of(&build_command(&req, PanelId::new()));
        // No claude flag leaks onto a codex invocation.
        assert!(!args.contains(&"--append-system-prompt".to_string()));
        assert!(!args.contains(&"--disallowedTools".to_string()));
        // The prompt rides as `-c instructions=<raw text>`, contract appended.
        let expected =
            format!("instructions=ROLE GUIDANCE\nsecond line\n\n{SUBAGENT_QUESTION_CONTRACT}");
        assert!(
            args.windows(2).any(|w| w[0] == "-c" && w[1] == expected),
            "codex argv must carry the role prompt via -c instructions=: {args:?}"
        );
    }

    /// With no role prompt, a codex **main worker** carries no
    /// `-c instructions=` override. (A codex sub-agent always carries one —
    /// the question contract.)
    #[test]
    fn codex_main_argv_omits_instructions_when_no_system_prompt() {
        let mut r = role();
        r.agent_cli = AgentCli::Codex;
        r.model = None;
        let req = SpawnRequest {
            role: r,
            agent_name: "main-1".into(),
            caucus_mcp: Some(CaucusMcp {
                caucus_bin: PathBuf::from("/usr/local/bin/caucus"),
                control_sock: PathBuf::from("/repo/.caucus/sessions/S1/control.sock"),
            }),
            ..SpawnRequest::default()
        };
        let args = args_of(&build_command(&req, PanelId::new()));
        assert!(!args.iter().any(|a| a.starts_with("instructions=")));
    }

    /// A codex sub-agent with no role prompt still gets the question contract
    /// as its `-c instructions=` override.
    #[test]
    fn codex_subagent_without_role_prompt_still_gets_the_contract() {
        let mut r = role();
        r.agent_cli = AgentCli::Codex;
        r.model = None;
        let req = SpawnRequest {
            role: r,
            agent_name: "sr".into(),
            ..SpawnRequest::default()
        };
        let args = args_of(&build_command(&req, PanelId::new()));
        let expected = format!("instructions={SUBAGENT_QUESTION_CONTRACT}");
        assert!(
            args.windows(2).any(|w| w[0] == "-c" && w[1] == expected),
            "codex argv must carry the contract via -c instructions=: {args:?}"
        );
    }

    /// A codex main worker panel (codex backend + `caucus_mcp` set) registers
    /// the caucus MCP server via `-c mcp_servers.caucus.{command,args}` — codex
    /// has no `--mcp-config`, so this is how it gains the tool surface to drive
    /// sub-agents.
    #[test]
    fn codex_argv_registers_the_caucus_mcp_server() {
        let mut r = role();
        r.agent_cli = AgentCli::Codex;
        r.model = None;
        let req = SpawnRequest {
            role: r,
            agent_name: "main-1".into(),
            caucus_mcp: Some(CaucusMcp {
                caucus_bin: PathBuf::from("/usr/local/bin/caucus"),
                control_sock: PathBuf::from("/repo/.caucus/sessions/S1/control.sock"),
            }),
            ..SpawnRequest::default()
        };
        let args = args_of(&build_command(&req, PanelId::new()));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c"
                    && w[1] == "mcp_servers.caucus.command=\"/usr/local/bin/caucus\""),
            "codex must register the caucus server command: {args:?}"
        );
        assert!(
            args.windows(2).any(|w| w[0] == "-c"
                && w[1]
                    == "mcp_servers.caucus.args=[\"mcp-serve\", \"--control-sock\", \
                        \"/repo/.caucus/sessions/S1/control.sock\"]"),
            "codex must register the caucus server args: {args:?}"
        );
        // No claude-only MCP flag leaks onto a codex invocation.
        assert!(!args.contains(&"--mcp-config".to_string()));
    }

    /// A codex sub-agent (no `caucus_mcp`) carries no MCP registration — only
    /// the main worker is an orchestrator.
    #[test]
    fn codex_argv_omits_mcp_when_not_the_main_panel() {
        let mut r = role();
        r.agent_cli = AgentCli::Codex;
        r.model = None;
        let req = SpawnRequest {
            role: r,
            agent_name: "reviewer-r1".into(),
            ..SpawnRequest::default()
        };
        let args = args_of(&build_command(&req, PanelId::new()));
        assert!(!args.iter().any(|a| a.starts_with("mcp_servers.caucus")));
    }

    /// Every codex panel gets turn-completion wiring: codex has no Stop hook,
    /// so caucus registers `caucus signal codex-notify` as codex's `notify`
    /// program via `-c notify=[...]`, baking in the panel's signal socket,
    /// session, and panel id. This is what lets a codex panel settle to `Idle`
    /// when its turn ends instead of hanging `Working` until the round fallback.
    #[test]
    fn codex_argv_registers_the_turn_completion_notify() {
        let mut r = role();
        r.agent_cli = AgentCli::Codex;
        r.model = None;
        let req = SpawnRequest {
            role: r,
            agent_name: "reviewer-r1".into(),
            sock_path: Some(PathBuf::from("/repo/.caucus/sessions/S1/caucus.sock")),
            ..SpawnRequest::default()
        };
        let panel = PanelId::new();
        let args = args_of(&build_command(&req, panel));
        let notify = args
            .windows(2)
            .find(|w| w[0] == "-c" && w[1].starts_with("notify=["))
            .map(|w| w[1].clone())
            .expect("codex argv must register a notify program");
        assert!(
            notify.contains("\",\"signal\",\"codex-notify\","),
            "notify must invoke `caucus signal codex-notify`: {notify}"
        );
        assert!(
            notify.contains("\"--sock\",\"/repo/.caucus/sessions/S1/caucus.sock\""),
            "notify must carry the signal socket: {notify}"
        );
        assert!(
            notify.contains(&format!("\"--session\",\"{}\"", req.session_id)),
            "notify must carry the session id: {notify}"
        );
        assert!(
            notify.contains(&format!("\"--panel\",\"{panel}\"")),
            "notify must carry this panel's id: {notify}"
        );
    }

    /// With no signal socket wired (the default/unit-test `SpawnRequest`), codex
    /// carries no `notify` registration — the panel falls back to the round
    /// fallback timer, exactly as before this wiring existed.
    #[test]
    fn codex_argv_omits_notify_without_a_signal_socket() {
        let mut r = role();
        r.agent_cli = AgentCli::Codex;
        r.model = None;
        let req = SpawnRequest {
            role: r,
            agent_name: "reviewer-r1".into(),
            ..SpawnRequest::default()
        };
        let args = args_of(&build_command(&req, PanelId::new()));
        assert!(!args.iter().any(|a| a.starts_with("notify=[")));
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

    /// codex has no standard resume flag — a set `resume_session_id` must be
    /// ignored, not leaked onto its argv.
    #[test]
    fn codex_ignores_resume_session_id() {
        let mut r = role();
        r.agent_cli = AgentCli::Codex;
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
            "codex must ignore resume_session_id: {args:?}"
        );
        assert!(!args.contains(&"conv-xyz".to_string()));
    }

    #[test]
    fn caucus_env_is_injected() {
        let sock = PathBuf::from("/tmp/caucus.sock");
        let req = SpawnRequest {
            role: role(),
            agent_name: "x".into(),
            sock_path: Some(sock.clone()),
            session_dir: PathBuf::from("/repo/.caucus/sessions/S1"),
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
        assert_eq!(
            cmd.env.get("CAUCUS_SESSION_DIR").map(String::as_str),
            Some("/repo/.caucus/sessions/S1")
        );
    }

    #[test]
    fn caucus_session_dir_omitted_when_unset() {
        // Empty session_dir (test/default) must not inject the var, mirroring
        // how an empty repo_root leaves the cwd unset.
        let req = SpawnRequest {
            role: role(),
            agent_name: "x".into(),
            ..SpawnRequest::default()
        };
        let cmd = build_command(&req, PanelId::new());
        assert!(!cmd.env.contains_key("CAUCUS_SESSION_DIR"));
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
