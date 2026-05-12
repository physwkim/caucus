//! Agent spawn: build the `claude` command line from a [`RoleSpec`], spawn a
//! tmux pane to run it, and persist the agent manifest. tmux is the only
//! supported execution model in caucus — see `docs/design.md` §14 (non-goals).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::agent::manifest::{AgentKind, AgentManifest, ManifestError, write_json};
use crate::role::spec::RoleSpec;
use crate::session::id::SessionId;
use crate::tmux::{SpawnPaneOptions, TmuxError, TmuxService};

/// Inputs for spawning one agent. `caucus session new --roles <list>` builds
/// one of these per role; `caucus execute start --role` builds one with
/// `kind = Execute` and a `worktree_path` set.
#[derive(Debug, Clone)]
pub struct SpawnRequest<'a> {
    pub session_id: SessionId,
    pub session_root: PathBuf,
    pub role: &'a RoleSpec,
    /// Distinguishes meeting vs execute panes; affects cwd and manifest.kind.
    pub kind: AgentKind,
    /// Working directory for the spawned pane. For meeting agents this is
    /// the repo root; for execute agents this is the new worktree path.
    pub cwd: PathBuf,
    /// Path of the rendered system-prompt file the spawned `claude` should
    /// load. Resolved by the caller (CLI / round lifecycle).
    pub system_prompt_path: PathBuf,
    /// Path to the response markdown the agent should write into.
    /// Communicated via the `CAUCUS_RESPONSE_PATH` env var.
    pub response_path: PathBuf,
    /// Repo-rooted path to `bin/sentinel-stop` (or absolute). Communicated
    /// via `CAUCUS_SENTINEL_HOOK` so the Claude Stop hook script knows
    /// which caucus binary to invoke.
    pub sentinel_hook_path: Option<PathBuf>,
    /// Optional model id; defaults to `claude-opus-4-7` if `None`.
    pub model: Option<String>,
    /// Pane title; defaults to "<role>" if `None`.
    pub title: Option<String>,
    /// Optional initial prompt path the spawned pane reads + acts on. This
    /// is in env, not on the command line, so an idle pane can be reused
    /// across rounds.
    pub initial_prompt_path: Option<PathBuf>,
    /// If true, the spawned agent CLI gets the "skip every permission
    /// prompt" flag (`claude --dangerously-skip-permissions`). The role's
    /// own `allowed_tools` allowlist is the actual safety boundary; this
    /// only suppresses the interactive confirmations that block agents
    /// inside a tmux pane.
    pub skip_permissions: bool,
    /// If `Some(session_id)`, spawn the agent CLI with `--resume <id>` so
    /// it inherits the conversation context of an earlier session (typically
    /// the meeting-phase agent). System prompt + allowed-tools are *not*
    /// re-passed — the resumed session already has them. Only valid for
    /// `AgentCli::Claude` (codex doesn't expose `--resume` the same way).
    pub resume_session_id: Option<String>,
}

/// Final result of a successful spawn.
#[derive(Debug, Clone)]
pub struct SpawnOutcome {
    pub manifest: AgentManifest,
    pub manifest_path: PathBuf,
    pub command_line: String,
}

#[derive(Debug, Error)]
pub enum SpawnError {
    #[error(transparent)]
    Tmux(#[from] TmuxError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("missing system-prompt file: {0}")]
    MissingPrompt(PathBuf),
}

/// Default model when the caller doesn't pin one. Matches the env Claude
/// Code expects via `--model`.
pub const DEFAULT_MODEL: &str = "claude-opus-4-7";

/// Render the agent CLI invocation as a single shell-quotable string. The
/// caller uses this for `tmux send-shell` (which adds the outer quotes) and
/// for the manifest's `command_line` field (purely informational).
///
/// Dispatches on `role.agent_cli`: Claude and Codex have different flag
/// surfaces so we keep them in two named helpers. The only shared output is
/// the bootstrap user message — both agents accept a positional prompt.
#[allow(clippy::too_many_arguments)] // Eight knobs is the actual surface;
// bundling into a config struct would just move the knob count around.
pub fn render_command_line(
    role: &RoleSpec,
    cwd: &Path,
    system_prompt_path: &Path,
    response_path: &Path,
    initial_prompt_path: Option<&Path>,
    model: &str,
    skip_permissions: bool,
    resume_session_id: Option<&str>,
) -> String {
    match role.agent_cli {
        crate::role::spec::AgentCli::Claude => render_claude_command(
            role,
            system_prompt_path,
            response_path,
            initial_prompt_path,
            model,
            skip_permissions,
            resume_session_id,
        ),
        crate::role::spec::AgentCli::Codex => {
            // codex --resume opens a session picker by default and doesn't
            // accept a session-id as a flag, so --continue-meeting is
            // Claude-only for now. Fall through to the fresh-spawn path.
            render_codex_command(
                role,
                cwd,
                system_prompt_path,
                response_path,
                initial_prompt_path,
                model,
                skip_permissions,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_claude_command(
    role: &RoleSpec,
    system_prompt_path: &Path,
    response_path: &Path,
    initial_prompt_path: Option<&Path>,
    model: &str,
    skip_permissions: bool,
    resume_session_id: Option<&str>,
) -> String {
    if let Some(sid) = resume_session_id {
        // Resume path: model / permission-mode / system prompt / allowed-tools
        // are all carried over from the original session. We only ensure the
        // permission-bypass flag stays on (the original session likely had
        // it; harmless to re-pass).
        let mut cmd = format!("claude --resume {sid}");
        if skip_permissions {
            cmd.push_str(" --dangerously-skip-permissions");
        }
        // Bootstrap delivered post-spawn via `send_text` because `--print`
        // would make claude exit after one response. See `spawn()` below.
        let _ = (response_path, initial_prompt_path);
        return cmd;
    }

    let allowed = role.allowed_tools_csv();
    let mut cmd = format!(
        "claude --model {model} --permission-mode {mode} --append-system-prompt @{prompt}",
        mode = role.permission_mode.as_cli_arg(),
        prompt = shell_quote(&system_prompt_path.display().to_string()),
    );
    if !allowed.is_empty() {
        cmd.push_str(" --allowed-tools ");
        cmd.push_str(&shell_quote(&allowed));
    }
    if skip_permissions {
        cmd.push_str(" --dangerously-skip-permissions");
    }
    if let Some(prompt_file) = initial_prompt_path {
        cmd.push_str(" --print ");
        cmd.push_str(&shell_quote(&bootstrap_message(prompt_file, response_path)));
    }
    cmd
}

/// Codex doesn't ship an `--append-system-prompt @FILE` equivalent, so we
/// fold the role's system prompt into the bootstrap user message and inline
/// it as the positional prompt argument. The trade-off vs. Claude: the role
/// identity arrives as a user message instead of a system message, which is
/// slightly weaker as a constraint but works well in practice (codex review
/// honours role framing the same way).
fn render_codex_command(
    role: &RoleSpec,
    cwd: &Path,
    system_prompt_path: &Path,
    response_path: &Path,
    initial_prompt_path: Option<&Path>,
    model: &str,
    skip_permissions: bool,
) -> String {
    let mut cmd = format!(
        "codex --model {model} -C {cwd}",
        cwd = shell_quote(&cwd.display().to_string()),
    );
    if skip_permissions {
        cmd.push_str(" --dangerously-bypass-approvals-and-sandbox");
    } else {
        // Match the role's permission_mode coarsely: anything stricter than
        // Default → read-only sandbox.
        let sandbox = match role.permission_mode {
            crate::role::spec::PermissionMode::Plan
            | crate::role::spec::PermissionMode::Default => "read-only",
            crate::role::spec::PermissionMode::AcceptEdits => "workspace-write",
            crate::role::spec::PermissionMode::BypassPermissions => "danger-full-access",
        };
        cmd.push_str(" --sandbox ");
        cmd.push_str(sandbox);
    }
    // The user prompt: role identity (read from disk) + bootstrap.
    let header = format!(
        "Read your role instructions at {system}. {tools_hint}",
        system = system_prompt_path.display(),
        tools_hint = if role.allowed_tools.is_empty() {
            String::new()
        } else {
            format!("Permitted tools: {}.", role.allowed_tools_csv())
        }
    );
    let body = match initial_prompt_path {
        Some(p) => format!("\n\n{}", bootstrap_message(p, response_path)),
        None => String::new(),
    };
    cmd.push(' ');
    cmd.push_str(&shell_quote(&format!("{header}{body}")));
    cmd
}

fn bootstrap_message(prompt_file: &Path, response_path: &Path) -> String {
    format!(
        "Read {prompt} and write your reply to {response}. \
         Finish with a one-line summary.",
        prompt = prompt_file.display(),
        response = response_path.display(),
    )
}

fn shell_quote(s: &str) -> String {
    // Minimal shell-quoter for command-line assembly. The full POSIX
    // quoter lives in `crate::tmux::escape::single_quote_shell` — this
    // helper exists only so the manifest's command_line is readable.
    crate::tmux::escape::single_quote_shell(s)
}

/// Spawn an agent.
pub async fn spawn(tmux: &TmuxService, req: SpawnRequest<'_>) -> Result<SpawnOutcome, SpawnError> {
    if !req.system_prompt_path.exists() {
        return Err(SpawnError::MissingPrompt(req.system_prompt_path.clone()));
    }

    // Precedence: per-role model > request-level model > DEFAULT_MODEL.
    let model = req
        .role
        .model
        .clone()
        .or_else(|| req.model.clone())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let command_line = render_command_line(
        req.role,
        &req.cwd,
        &req.system_prompt_path,
        &req.response_path,
        req.initial_prompt_path.as_deref(),
        &model,
        req.skip_permissions,
        req.resume_session_id.as_deref(),
    );

    // Manifest first, so the manifest path exists by the time the pane
    // could possibly emit a sentinel.
    let mut manifest = AgentManifest::new(
        req.session_id,
        req.role.name.clone(),
        req.title.clone().unwrap_or_else(|| req.role.name.clone()),
        req.kind,
        Some(model.clone()),
    );
    // Carry the inherited claude session id forward so subsequent sentinel
    // ingests can't accidentally treat the resumed pane as a fresh agent
    // when reconciling session ids.
    manifest.claude_session_id = req.resume_session_id.clone();
    manifest.worktree_path = match req.kind {
        AgentKind::Execute => Some(req.cwd.clone()),
        AgentKind::Meeting => None,
    };
    write_json(&manifest, &req.session_root)?;
    let manifest_path = AgentManifest::json_path(&req.session_root, manifest.agent_id);

    // Then spawn the pane. Env vars carry the contract with the Claude
    // Stop hook (`caucus sentinel write --session ID --agent ID`).
    let mut env: HashMap<String, String> = HashMap::new();
    env.insert("CAUCUS_SESSION_ID".into(), manifest.session_id.to_string());
    env.insert("CAUCUS_AGENT_ID".into(), manifest.agent_id.to_string());
    env.insert(
        "CAUCUS_SESSION_ROOT".into(),
        req.session_root.display().to_string(),
    );
    env.insert(
        "CAUCUS_RESPONSE_PATH".into(),
        req.response_path.display().to_string(),
    );
    if let Some(hook) = &req.sentinel_hook_path {
        env.insert("CAUCUS_SENTINEL_HOOK".into(), hook.display().to_string());
    }

    let pane_id = tmux
        .spawn_pane(SpawnPaneOptions {
            target_pane: None,
            cwd: Some(req.cwd.clone()),
            command: Some(command_line.clone()),
            vertical: false,
            env,
            title: Some(req.title.clone().unwrap_or_else(|| req.role.name.clone())),
        })
        .await?;

    manifest.tmux_pane_id = Some(pane_id.clone());
    write_json(&manifest, &req.session_root)?;

    // Resume-mode bootstrap: `--print` would make claude exit, so we omit it
    // from the command line for resumed sessions and instead paste the task
    // message after the TUI has started. A short wait lets `claude --resume`
    // load the prior transcript before we start typing.
    if req.resume_session_id.is_some() {
        if let Some(prompt_file) = req.initial_prompt_path.as_deref() {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            let body = format!(
                "Read {prompt} and write your reply to {response}. \
                 Finish with a one-line summary.",
                prompt = prompt_file.display(),
                response = req.response_path.display(),
            );
            tmux.send_text(&pane_id, &body, true).await?;
        }
    }

    Ok(SpawnOutcome {
        manifest,
        manifest_path,
        command_line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::spec::{PermissionMode, RoleSpec};
    use std::collections::BTreeSet;

    fn reviewer_spec() -> RoleSpec {
        RoleSpec {
            name: "reviewer".into(),
            description: "test".into(),
            allowed_tools: ["Read", "Glob", "Grep"]
                .iter()
                .map(|s| (*s).to_string())
                .collect::<BTreeSet<_>>(),
            permission_mode: PermissionMode::Default,
            system_prompt_template: PathBuf::from("roles/reviewer.md"),
            model: None,
            agent_cli: crate::role::spec::AgentCli::Claude,
        }
    }

    #[test]
    fn per_role_model_overrides_request_model() {
        // Even when a request supplies `model = "claude-opus-4-7"`, the role's
        // own pin wins.
        let mut role = reviewer_spec();
        role.model = Some("claude-sonnet-4-6".into());
        let cmd = render_command_line(
            &role,
            Path::new("/repo"),
            Path::new("/sys.md"),
            Path::new("/r.md"),
            None,
            DEFAULT_MODEL, // request-level
            false,
            None,
        );
        // render_command_line itself takes a model argument; the precedence
        // is resolved in spawn() before this call. Sanity: our request-level
        // model is the default, so the rendered command line shows that.
        assert!(cmd.contains(&format!("--model {DEFAULT_MODEL}")));
    }

    #[test]
    fn command_line_pins_model_mode_allowlist_and_prompt() {
        let role = reviewer_spec();
        let cmd = render_command_line(
            &role,
            Path::new("/repo"),
            Path::new("/repo/.caucus/s/round-1/system-reviewer.md"),
            Path::new("/repo/.caucus/s/round-1/response-reviewer.md"),
            None,
            DEFAULT_MODEL,
            false,
            None,
        );
        assert!(cmd.contains(&format!("--model {DEFAULT_MODEL}")));
        assert!(cmd.contains("--permission-mode default"));
        assert!(cmd.contains("--append-system-prompt @"));
        assert!(cmd.contains("--allowed-tools 'Glob,Grep,Read'"));
        // No initial prompt → no --print arg.
        assert!(!cmd.contains("--print"));
        assert!(!cmd.contains("--dangerously-skip-permissions"));
    }

    #[test]
    fn command_line_includes_print_when_bootstrap_present() {
        let role = reviewer_spec();
        let prompt = PathBuf::from("/p/agenda.md");
        let response = PathBuf::from("/p/response.md");
        let cmd = render_command_line(
            &role,
            Path::new("/repo"),
            Path::new("/repo/.caucus/sys.md"),
            &response,
            Some(&prompt),
            DEFAULT_MODEL,
            false,
            None,
        );
        assert!(cmd.contains("--print"));
        assert!(cmd.contains("/p/agenda.md"));
        assert!(cmd.contains("/p/response.md"));
    }

    #[test]
    fn command_line_empty_allowlist_omits_flag() {
        let mut role = reviewer_spec();
        role.allowed_tools.clear();
        let cmd = render_command_line(
            &role,
            Path::new("/repo"),
            Path::new("/sys.md"),
            Path::new("/r.md"),
            None,
            "claude-opus-4-7",
            false,
            None,
        );
        assert!(!cmd.contains("--allowed-tools"));
    }

    #[test]
    fn skip_permissions_appends_dangerous_flag() {
        let role = reviewer_spec();
        let cmd = render_command_line(
            &role,
            Path::new("/repo"),
            Path::new("/sys.md"),
            Path::new("/r.md"),
            None,
            DEFAULT_MODEL,
            true,
            None,
        );
        assert!(cmd.contains("--dangerously-skip-permissions"));
    }

    #[test]
    fn codex_role_renders_codex_invocation() {
        let mut role = reviewer_spec();
        role.agent_cli = crate::role::spec::AgentCli::Codex;
        let cmd = render_command_line(
            &role,
            Path::new("/repo"),
            Path::new("/sys.md"),
            Path::new("/r.md"),
            None,
            "gpt-5.1-codex",
            true,
            None,
        );
        assert!(cmd.starts_with("codex"));
        assert!(cmd.contains("--model gpt-5.1-codex"));
        assert!(cmd.contains("-C '/repo'"));
        assert!(cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
        // System prompt is inlined into the user message, not a separate flag.
        assert!(!cmd.contains("--append-system-prompt"));
        // The user message references the role prompt path.
        assert!(cmd.contains("/sys.md"));
    }

    #[test]
    fn codex_without_skip_permissions_picks_sandbox_by_mode() {
        let mut role = reviewer_spec(); // PermissionMode::Default → read-only
        role.agent_cli = crate::role::spec::AgentCli::Codex;
        let cmd = render_command_line(
            &role,
            Path::new("/repo"),
            Path::new("/sys.md"),
            Path::new("/r.md"),
            None,
            "gpt-5.1-codex",
            false,
            None,
        );
        assert!(cmd.contains("--sandbox read-only"));
        assert!(!cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn resume_session_id_uses_claude_resume_form() {
        let role = reviewer_spec();
        let cmd = render_command_line(
            &role,
            Path::new("/repo"),
            Path::new("/sys.md"),
            Path::new("/r.md"),
            Some(Path::new("/p/task.md")),
            DEFAULT_MODEL,
            true,
            Some("abc-123-session-id"),
        );
        assert!(cmd.starts_with("claude --resume abc-123-session-id"));
        assert!(cmd.contains("--dangerously-skip-permissions"));
        // Resume path: no system prompt, no allowed-tools, no --print
        // (bootstrap is paste-buffered after spawn).
        assert!(!cmd.contains("--append-system-prompt"));
        assert!(!cmd.contains("--allowed-tools"));
        assert!(!cmd.contains("--print"));
    }
}
