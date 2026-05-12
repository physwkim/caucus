//! Implementation of every subcommand. Errors bubble up as `anyhow::Error`;
//! the top-level `run()` converts them to exit codes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::agent::manifest::AgentManifest;
use crate::cli::commands::*;
use crate::cli::exit;
use crate::cli::output::{OutputFormat, emit, note};
use crate::config::RegistryBuilder;
use crate::doctor::DoctorReport;
use crate::role::registry::RoleRegistry;
use crate::sentinel::{SentinelKind, write_sentinel};
use crate::session::id::{AgentId, SessionId};
use crate::session::record::{Session, list_sessions, read_session, write_session};
use crate::session::state::SessionState;
use crate::tmux::TmuxService;
use crate::worktree::cleanup::CleanupQueue;

/// Resolve `--repo` flag → absolute repo root.
pub fn resolve_repo(flag: Option<PathBuf>) -> Result<PathBuf> {
    let raw = match flag {
        Some(p) => p,
        None => std::env::current_dir().context("could not read cwd")?,
    };
    Ok(std::fs::canonicalize(&raw).unwrap_or(raw))
}

pub fn build_registry(repo: &Path) -> Result<RoleRegistry> {
    let mut b = RegistryBuilder::new().with_project_root(repo);
    if std::env::var_os("HOME").is_some() {
        b = b.with_global_default()?;
    }
    Ok(b.build()?)
}

pub fn parse_session_id(s: &str) -> Result<SessionId> {
    s.parse()
        .map_err(|err| anyhow!("invalid session id {s}: {err}"))
}

pub fn parse_agent_id(s: &str) -> Result<AgentId> {
    s.parse()
        .map_err(|err| anyhow!("invalid agent id {s}: {err}"))
}

// ---- init ----------------------------------------------------------------

pub fn init(repo: &Path, args: InitArgs) -> Result<()> {
    let caucus_dir = repo.join(".caucus");
    std::fs::create_dir_all(caucus_dir.join("bin"))?;
    std::fs::create_dir_all(caucus_dir.join("sessions"))?;
    std::fs::create_dir_all(caucus_dir.join("worktrees"))?;

    let hook_path = caucus_dir.join("bin").join("sentinel-stop");
    if hook_path.exists() && !args.force {
        note(&format!(
            "hook already present at {} (use --force to overwrite)",
            hook_path.display()
        ));
    } else {
        std::fs::write(&hook_path, hook_script())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = std::fs::metadata(&hook_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_path, perms)?;
        }
    }

    note(&format!("caucus initialised at {}", caucus_dir.display()));

    if args.install_hook {
        let report = crate::cli::hook_install::install_stop_hook(&hook_path)?;
        note(&format!(
            "claude settings updated ({:?}) at {}",
            report.action,
            report.settings_path.display()
        ));
        if let Some(bak) = &report.backup_path {
            note(&format!("backup written to {}", bak.display()));
        }
    } else {
        note("");
        note("Next step — install the Claude Stop hook in ~/.claude/settings.json:");
        note("");
        note("  {\"hooks\":{\"Stop\":[{\"matcher\":\"\",\"hooks\":[{\"type\":\"command\",");
        note(&format!(
            "    \"command\":\"{}\"}}]}}]}}",
            hook_path.display()
        ));
        note("");
        note("Re-run `caucus init --install-hook` to do this automatically.");
    }
    note("Add `.caucus/` to your .gitignore. caucus state should never be tracked.");
    Ok(())
}

fn hook_script() -> &'static str {
    r#"#!/bin/sh
# caucus Stop hook. Installed globally in ~/.claude/settings.json, so the
# Claude Code harness fires it for EVERY claude session — not just caucus
# panes. When the pane wasn't spawned by caucus the env vars below are
# unset; we exit 0 silently in that case so the user doesn't see a
# spurious "Stop hook error" on every unrelated session.
#
# When CAUCUS_SESSION_ID + CAUCUS_AGENT_ID are present (caucus spawned this
# pane via tmux split-window with `-e`), we forward the Stop event to
# `caucus sentinel write`. Stdin carries the full hook payload from
# Claude Code (including the agent's claude session_id); caucus reads it
# via read_stdin_json() and stores it on the sentinel.
set -e
if [ -z "$CAUCUS_SESSION_ID" ] || [ -z "$CAUCUS_AGENT_ID" ]; then
  exit 0
fi
exec caucus sentinel write \
  --session "$CAUCUS_SESSION_ID" \
  --agent "$CAUCUS_AGENT_ID" \
  --kind stop
"#
}

// ---- doctor --------------------------------------------------------------

pub fn doctor(repo: &Path, format: OutputFormat) -> Result<()> {
    let report = crate::doctor::run(repo);
    emit(format, &report, || render_doctor_text(&report));
    if !report.is_healthy() {
        return Err(anyhow!("doctor reports unhealthy environment"));
    }
    Ok(())
}

fn render_doctor_text(r: &DoctorReport) -> String {
    let mut s = String::new();
    use std::fmt::Write as _;
    for check in &r.checks {
        let _ = writeln!(
            s,
            "{} {}: {}",
            if check.ok { "✓" } else { "✗" },
            check.name,
            check.detail
        );
    }
    if !r.is_healthy() {
        let _ = writeln!(
            s,
            "\nOne or more checks failed. See --format json for details."
        );
    }
    s
}

// ---- session --------------------------------------------------------------

pub async fn session_new(repo: &Path, format: OutputFormat, args: SessionNewArgs) -> Result<()> {
    let registry = build_registry(repo)?;
    for role in &args.roles {
        if !registry.contains(role) {
            return Err(anyhow!(
                "unknown role {role}. Available: {}",
                registry.names().collect::<Vec<_>>().join(", ")
            ));
        }
    }

    let mut session = Session::new(
        repo.to_path_buf(),
        args.topic,
        args.roles.clone(),
        args.max_rounds,
    );
    write_session(&session)?;

    // Move into MeetingInProgress immediately — the meeting starts as soon
    // as the panes are alive.
    session.transition(SessionState::MeetingInProgress)?;

    let tmux = TmuxService::new();
    let hook_path = repo.join(".caucus").join("bin").join("sentinel-stop");

    for role_name in &args.roles {
        let role = registry.get(role_name).map_err(|err| anyhow!("{err}"))?;
        let role_template = resolve_role_template(repo, &role.system_prompt_template);
        let agents_dir = session.session_root.join("agents");
        std::fs::create_dir_all(&agents_dir)?;
        let manifest = crate::agent::manifest::AgentManifest::new(
            session.id,
            role.name.clone(),
            role.name.clone(),
            crate::agent::manifest::AgentKind::Meeting,
            args.model.clone(),
        );
        let agent_id = manifest.agent_id;
        crate::agent::manifest::write_json(&manifest, &session.session_root)?;

        // No round yet — pane is alive idle waiting for `caucus round start`.
        // We still need a system-prompt file for it; we write it under
        // execute-style layout so it survives across rounds.
        let bootstrap_dir = session.session_root.join("agents-prompts");
        std::fs::create_dir_all(&bootstrap_dir)?;
        let sys_path = bootstrap_dir.join(format!("{role_name}.system.md"));
        std::fs::copy(&role_template, &sys_path)?;
        let response_path = bootstrap_dir.join(format!("{role_name}.response.md"));
        if !response_path.exists() {
            std::fs::write(&response_path, "")?;
        }

        let outcome = crate::agent::spawn::spawn(
            &tmux,
            crate::agent::spawn::SpawnRequest {
                session_id: session.id,
                session_root: session.session_root.clone(),
                role,
                kind: crate::agent::manifest::AgentKind::Meeting,
                cwd: repo.to_path_buf(),
                system_prompt_path: sys_path,
                response_path,
                sentinel_hook_path: Some(hook_path.clone()),
                model: args.model.clone(),
                title: Some(role.name.clone()),
                initial_prompt_path: None,
                skip_permissions: !args.require_permissions,
                resume_session_id: None,
                placement: args.placement.to_tmux(),
            },
        )
        .await?;

        let _ = agent_id; // Already persisted in the manifest above.
        session.register_agent(role_name, outcome.manifest.agent_id);
    }

    // Re-balance the window after all role panes are spawned — but only
    // when placement=split. With `placement=window` each role has its
    // own tab with a single pane, so there's nothing for select-layout
    // to balance and calling it would just resize the CEO's lone pane
    // unhelpfully.
    if !args.placement.is_single_pane_per_window() {
        tmux.apply_layout(args.layout.as_tmux_name(), session.agents.len() + 1, None)
            .await
            .ok();
    }

    write_session(&session)?;

    let json = serde_json::json!({
        "session_id": session.id.to_string(),
        "state": session.state,
        "roles": session.roles,
        "agents": session.agents.iter().map(|(role, id)| {
            serde_json::json!({"role": role, "agent_id": id.to_string()})
        }).collect::<Vec<_>>(),
        "session_root": session.session_root,
    });
    emit(format, &json, || {
        format!(
            "session {} started in state {:?} with roles {}",
            session.id,
            session.state,
            session.roles.join(", ")
        )
    });
    Ok(())
}

fn resolve_role_template(repo: &Path, template_path: &Path) -> PathBuf {
    if template_path.is_absolute() {
        return template_path.to_path_buf();
    }
    // Prefer the project's own roles/ dir; fall back to the caucus install
    // root via CARGO_MANIFEST_DIR when running from cargo, otherwise to the
    // executable's parent.
    let project_relative = repo.join(template_path);
    if project_relative.exists() {
        return project_relative;
    }
    if let Some(manifest_dir) = option_env!("CARGO_MANIFEST_DIR") {
        let candidate = Path::new(manifest_dir).join(template_path);
        if candidate.exists() {
            return candidate;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let candidate = parent.join(template_path);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    project_relative
}

pub fn session_list(repo: &Path, format: OutputFormat) -> Result<()> {
    let ids = list_sessions(repo)?;
    let view: Vec<_> = ids
        .iter()
        .filter_map(|id| read_session(repo, *id).ok())
        .map(|s| {
            serde_json::json!({
                "session_id": s.id.to_string(),
                "topic": s.topic,
                "state": s.state,
                "current_round": s.current_round,
                "max_rounds": s.max_rounds,
            })
        })
        .collect();
    emit(format, &view, || {
        if view.is_empty() {
            "no sessions".into()
        } else {
            view.iter()
                .map(|v| {
                    format!(
                        "{} [{}] round {}/{}: {}",
                        v["session_id"].as_str().unwrap_or(""),
                        v["state"].as_str().unwrap_or(""),
                        v["current_round"],
                        v["max_rounds"],
                        v["topic"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    Ok(())
}

pub fn session_show(repo: &Path, format: OutputFormat, args: SessionShowArgs) -> Result<()> {
    let id = parse_session_id(&args.session_id)?;
    let session = read_session(repo, id)?;
    emit(format, &session, || {
        format!(
            "session {} state={:?} round {}/{} roles=[{}]\nagents:\n{}",
            session.id,
            session.state,
            session.current_round,
            session.max_rounds,
            session.roles.join(", "),
            session
                .agents
                .iter()
                .map(|(r, a)| format!("  {r} {a}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    });
    Ok(())
}

pub fn session_converge(
    repo: &Path,
    format: OutputFormat,
    args: SessionConvergeArgs,
) -> Result<()> {
    let id = parse_session_id(&args.session_id)?;
    let mut session = read_session(repo, id)?;
    let decision = std::fs::read_to_string(&args.decision_file)
        .with_context(|| format!("reading {}", args.decision_file.display()))?;
    std::fs::write(session.session_root.join("decision.md"), &decision)?;
    session.transition(SessionState::MeetingConverged)?;
    write_session(&session)?;
    emit(
        format,
        &serde_json::json!({"session_id": id.to_string(), "state": session.state}),
        || format!("session {id} converged"),
    );
    Ok(())
}

pub async fn session_deadlock(
    repo: &Path,
    format: OutputFormat,
    args: SessionDeadlockArgs,
) -> Result<()> {
    let id = parse_session_id(&args.session_id)?;
    let mut session = read_session(repo, id)?;
    session.transition(SessionState::MeetingDeadlocked)?;
    write_session(&session)?;

    if args.escalate {
        let signal_path = session.session_root.join("escalated.signal");
        let body = serde_json::json!({
            "session_id": id.to_string(),
            "topic": session.topic,
            "ts": chrono::Utc::now(),
            "reason": "deadlock_escalated",
            "max_rounds": session.max_rounds,
            "current_round": session.current_round,
        });
        std::fs::write(&signal_path, serde_json::to_vec_pretty(&body)?)?;
        // Move to Abandoned — escalation means caucus's own loop is done;
        // a human (or some other tool reading escalated.signal) takes over.
        session.transition(SessionState::Abandoned)?;
        write_session(&session)?;
        emit(
            format,
            &serde_json::json!({
                "session_id": id.to_string(),
                "state": session.state,
                "escalation_signal": signal_path,
                "policy": "escalate",
            }),
            || {
                format!(
                    "session {id} deadlocked; escalation signal at {}",
                    signal_path.display()
                )
            },
        );
        return Ok(());
    }

    if args.explore {
        return session_deadlock_explore(repo, format, session, args).await;
    }

    emit(
        format,
        &serde_json::json!({
            "session_id": id.to_string(),
            "state": session.state,
            "policy": "manual",
        }),
        || format!("session {id} marked deadlocked (no policy flag — manual follow-up)"),
    );
    Ok(())
}

async fn session_deadlock_explore(
    repo: &Path,
    format: OutputFormat,
    mut session: Session,
    args: SessionDeadlockArgs,
) -> Result<()> {
    // Bridge: deadlocked → executing (legal per state machine §3).
    session.transition(SessionState::Executing)?;
    let registry = build_registry(repo)?;
    let tmux = TmuxService::new();

    let mut spawned = Vec::new();
    let mut skipped = Vec::new();
    let role_names = session.roles.clone();
    for role_name in &role_names {
        let role = match registry.get(role_name) {
            Ok(r) => r,
            Err(err) => {
                skipped.push(serde_json::json!({"role": role_name, "reason": err.to_string()}));
                continue;
            }
        };
        // Use the role's last non-empty round response as the explore task.
        let last_response = find_last_response(&session, role_name);
        let Some(task_source) = last_response else {
            skipped.push(serde_json::json!({
                "role": role_name,
                "reason": "no prior response to use as task.md",
            }));
            continue;
        };
        let role_template = resolve_role_template(repo, &role.system_prompt_template);
        let outcome = crate::execute::start(
            &tmux,
            crate::execute::ExecuteStartRequest {
                session_id: session.id,
                repo_root: repo.to_path_buf(),
                session_root: session.session_root.clone(),
                role,
                role_template_path: role_template,
                task_source: task_source.clone(),
                model: args.model.clone(),
                title: Some(format!("{role_name}-explore")),
                base_ref: args.base_ref.clone(),
                sentinel_hook_path: Some(repo.join(".caucus").join("bin").join("sentinel-stop")),
                skip_permissions: true,
                resume_session_id: None,
                placement: crate::tmux::Placement::SplitCurrent,
            },
        )
        .await?;
        session.register_agent(role_name, outcome.agent.agent_id);
        spawned.push(serde_json::json!({
            "role": role_name,
            "agent_id": outcome.agent.agent_id.to_string(),
            "worktree_path": outcome.worktree.path,
            "branch": outcome.worktree.branch,
            "task_source": task_source,
        }));
    }
    write_session(&session)?;

    emit(
        format,
        &serde_json::json!({
            "session_id": session.id.to_string(),
            "state": session.state,
            "policy": "explore",
            "spawned": spawned,
            "skipped": skipped,
        }),
        || {
            format!(
                "explore-on-deadlock: {} role(s) spawned, {} skipped",
                spawned.len(),
                skipped.len()
            )
        },
    );
    Ok(())
}

/// Walk round-NN/response-<role>.md from the highest round down, returning
/// the first non-empty path. Used by `--explore` to seed each explore agent
/// with that role's last argument.
fn find_last_response(session: &Session, role: &str) -> Option<PathBuf> {
    for round in (1..=session.current_round).rev() {
        let layout = crate::round::RoundLayout::new(session.session_root.clone(), round);
        let path = layout.response_path(role);
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() > 0 {
                return Some(path);
            }
        }
    }
    None
}

pub async fn session_kill(repo: &Path, format: OutputFormat, args: SessionKillArgs) -> Result<()> {
    let id = parse_session_id(&args.session_id)?;
    let mut session = read_session(repo, id)?;
    let tmux = TmuxService::new();
    for (_role, agent_id) in &session.agents {
        if let Ok(manifest) = crate::agent::manifest::read_json(&session.session_root, *agent_id) {
            if let Some(pane) = manifest.tmux_pane_id {
                let _ = tmux.kill_pane(&pane).await;
            }
        }
    }
    let (queue, _h) = CleanupQueue::spawn();
    let worktrees: Vec<_> = session
        .agents
        .iter()
        .filter_map(|(_, id)| crate::agent::manifest::read_json(&session.session_root, *id).ok())
        .filter_map(|m| m.worktree_path)
        .collect();
    let _ = queue.enqueue(crate::worktree::cleanup::CleanupJob {
        repo_root: repo.to_path_buf(),
        worktree_paths: worktrees,
        branches_to_delete: vec![],
        done: None,
    });
    // Pick a legal terminal transition. We accept either Abandoned or
    // staying-in-place if already terminal.
    if !session.state.is_terminal() {
        // Bridge through MeetingDeadlocked if we're still mid-meeting.
        if matches!(session.state, SessionState::MeetingInProgress) {
            session.transition(SessionState::MeetingDeadlocked)?;
        }
        session.transition(SessionState::Abandoned).ok();
    }
    write_session(&session)?;
    emit(
        format,
        &serde_json::json!({"session_id": id.to_string(), "state": session.state}),
        || format!("session {id} killed (state={:?})", session.state),
    );
    Ok(())
}

// ---- round ---------------------------------------------------------------

pub async fn round_start(repo: &Path, format: OutputFormat, args: RoundStartArgs) -> Result<()> {
    let id = parse_session_id(&args.session_id)?;
    let mut session = read_session(repo, id)?;
    let round = session.advance_round()?;
    let layout = crate::round::RoundLayout::new(session.session_root.clone(), round);
    let registry = build_registry(repo)?;

    let role_templates: Vec<(String, PathBuf)> = session
        .roles
        .iter()
        .map(|role| {
            registry
                .get(role)
                .map(|spec| {
                    (
                        role.clone(),
                        resolve_role_template(repo, &spec.system_prompt_template),
                    )
                })
                .map_err(|err| anyhow!("{err}"))
        })
        .collect::<Result<Vec<_>>>()?;

    crate::round::prepare_round(&layout, &role_templates, &args.agenda_file)?;

    let tmux = TmuxService::new();
    for (role, agent_id) in &session.agents {
        let manifest = crate::agent::manifest::read_json(&session.session_root, *agent_id)?;
        if let Some(pane) = manifest.tmux_pane_id.clone() {
            crate::round::nudge_role(&tmux, &pane, &layout, role).await?;
        }
    }

    write_session(&session)?;
    emit(
        format,
        &serde_json::json!({
            "session_id": id.to_string(),
            "round": round,
            "roles": session.roles,
        }),
        || format!("round {round} started for session {id}"),
    );
    Ok(())
}

pub fn round_status(repo: &Path, format: OutputFormat, args: RoundStatusArgs) -> Result<()> {
    let id = parse_session_id(&args.session_id)?;
    let session = read_session(repo, id)?;
    if session.current_round == 0 {
        return Err(anyhow!("no round has been started yet"));
    }
    let layout =
        crate::round::RoundLayout::new(session.session_root.clone(), session.current_round);
    let status =
        crate::round::round_status(&layout, &session.agents).context("collecting round status")?;
    emit(format, &status, || {
        let header = format!(
            "round {}/{}: all_complete={}",
            status.round_number, session.max_rounds, status.all_responses_complete
        );
        let body: String = status
            .roles
            .iter()
            .map(|r| {
                format!(
                    "  {} [{:?}] response_bytes={:?} sentinel={}",
                    r.role, r.derived_state, r.response_bytes, r.sentinel_present
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("{header}\n{body}")
    });
    Ok(())
}

pub async fn round_next(repo: &Path, format: OutputFormat, args: RoundNextArgs) -> Result<()> {
    round_start(
        repo,
        format,
        RoundStartArgs {
            session_id: args.session_id,
            agenda_file: args.agenda_file,
        },
    )
    .await
}

// ---- execute -------------------------------------------------------------

pub async fn execute_start(
    repo: &Path,
    format: OutputFormat,
    args: ExecuteStartCliArgs,
) -> Result<()> {
    let id = parse_session_id(&args.session_id)?;
    let mut session = read_session(repo, id)?;
    if matches!(session.state, SessionState::MeetingConverged) {
        session.transition(SessionState::Executing)?;
    }
    let registry = build_registry(repo)?;
    let role = registry.get(&args.role).map_err(|err| anyhow!("{err}"))?;
    let role_template = resolve_role_template(repo, &role.system_prompt_template);
    let tmux = TmuxService::new();

    // --continue-meeting: look up the meeting-phase agent for this role,
    // read its captured claude_session_id, kill its pane (claude refuses to
    // resume a live session from a second process), and pass the id into
    // the spawn so the execute agent inherits the meeting transcript.
    let resume_session_id = if args.continue_meeting {
        let meeting_agent_id = lookup_meeting_agent(&session, &args.role)?;
        let meeting_manifest =
            crate::agent::manifest::read_json(&session.session_root, meeting_agent_id)?;
        let sid = meeting_manifest.claude_session_id.clone().ok_or_else(|| {
            anyhow!(
                "meeting agent {meeting_agent_id} has no captured claude_session_id — \
                     wait until at least one Stop hook has fired (run a round) before retrying \
                     with --continue-meeting"
            )
        })?;
        if let Some(pane) = meeting_manifest.tmux_pane_id.clone() {
            let _ = tmux.kill_pane(&pane).await;
        }
        Some(sid)
    } else {
        None
    };

    let outcome = crate::execute::start(
        &tmux,
        crate::execute::ExecuteStartRequest {
            session_id: session.id,
            repo_root: repo.to_path_buf(),
            session_root: session.session_root.clone(),
            role,
            role_template_path: role_template,
            task_source: args.task_file,
            model: args.model,
            title: Some(format!("{}-execute", args.role)),
            base_ref: args.base_ref,
            sentinel_hook_path: Some(repo.join(".caucus").join("bin").join("sentinel-stop")),
            skip_permissions: !args.require_permissions,
            resume_session_id,
            placement: args.placement.to_tmux(),
        },
    )
    .await?;
    session.register_agent(&args.role, outcome.agent.agent_id);
    // Rebalance like session_new — but only when split-placement is in use.
    if !args.placement.is_single_pane_per_window() {
        let total_panes = session.agents.len() + 1;
        tmux.apply_layout(args.layout.as_tmux_name(), total_panes, None)
            .await
            .ok();
    }
    write_session(&session)?;
    emit(
        format,
        &serde_json::json!({
            "session_id": id.to_string(),
            "agent_id": outcome.agent.agent_id.to_string(),
            "worktree_path": outcome.worktree.path,
            "branch": outcome.worktree.branch,
        }),
        || {
            format!(
                "execute agent {} started in worktree {} (branch {})",
                outcome.agent.agent_id,
                outcome.worktree.path.display(),
                outcome.worktree.branch
            )
        },
    );
    Ok(())
}

pub fn execute_status(repo: &Path, format: OutputFormat, args: ExecuteStatusArgs) -> Result<()> {
    let id = parse_session_id(&args.session_id)?;
    let session = read_session(repo, id)?;
    let mut entries = Vec::new();
    for (role, agent_id) in &session.agents {
        if let Ok(m) = crate::agent::manifest::read_json(&session.session_root, *agent_id) {
            if matches!(m.kind, crate::agent::manifest::AgentKind::Execute) {
                entries.push(serde_json::json!({
                    "role": role,
                    "agent_id": agent_id.to_string(),
                    "derived_state": m.derived_state,
                    "worktree_path": m.worktree_path,
                    "tmux_pane_id": m.tmux_pane_id,
                }));
            }
        }
    }
    emit(format, &entries, || {
        if entries.is_empty() {
            "no execute agents".into()
        } else {
            entries
                .iter()
                .map(|e| {
                    format!(
                        "{} {:?} {} → {}",
                        e["role"], e["derived_state"], e["agent_id"], e["worktree_path"]
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    Ok(())
}

pub async fn execute_finish(
    repo: &Path,
    format: OutputFormat,
    args: ExecuteFinishArgs,
) -> Result<()> {
    let id = parse_session_id(&args.session_id)?;
    let session = read_session(repo, id)?;
    let agent_id = lookup_execute_agent(&session, &args.role)?;
    let tmux = TmuxService::new();
    let (queue, _h) = CleanupQueue::spawn();
    let outcome = crate::execute::finish(&tmux, &queue, &session.session_root, agent_id).await?;
    emit(
        format,
        &serde_json::json!({
            "agent_id": agent_id.to_string(),
            "commit_provenance": outcome.provenance,
            "cleanup": {
                "removed": outcome.cleanup.removed_worktrees,
                "failed": outcome.cleanup.failed_worktrees,
            },
        }),
        || {
            format!(
                "execute {} finished. commit={:?}",
                agent_id,
                outcome.provenance.as_ref().map(|p| p.commit.clone())
            )
        },
    );
    Ok(())
}

pub async fn execute_abandon(
    repo: &Path,
    format: OutputFormat,
    args: ExecuteAbandonArgs,
) -> Result<()> {
    let id = parse_session_id(&args.session_id)?;
    let session = read_session(repo, id)?;
    let agent_id = lookup_execute_agent(&session, &args.role)?;
    let tmux = TmuxService::new();
    let (queue, _h) = CleanupQueue::spawn();
    let outcome = crate::execute::abandon(&tmux, &queue, &session.session_root, agent_id).await?;
    emit(
        format,
        &serde_json::json!({
            "agent_id": agent_id.to_string(),
            "state": outcome.manifest.derived_state,
            "cleanup": {
                "removed": outcome.cleanup.removed_worktrees,
            },
        }),
        || format!("execute {agent_id} abandoned"),
    );
    Ok(())
}

pub async fn execute_pipeline(
    repo: &Path,
    format: OutputFormat,
    args: ExecutePipelineCliArgs,
) -> Result<()> {
    let id = parse_session_id(&args.session_id)?;
    let mut session = read_session(repo, id)?;
    if matches!(session.state, SessionState::MeetingConverged) {
        session.transition(SessionState::Executing)?;
    }
    let registry = build_registry(repo)?;
    let tmux = TmuxService::new();

    let repo_path = repo.to_path_buf();
    let resolver: Box<dyn Fn(&Path) -> std::path::PathBuf> =
        Box::new(move |template| resolve_role_template(&repo_path, template));

    let outcome = crate::execute::pipeline_run(
        &tmux,
        crate::execute::PipelineRequest {
            session_id: session.id,
            repo_root: repo.to_path_buf(),
            session_root: session.session_root.clone(),
            registry: &registry,
            role_template_resolver: &resolver,
            plan_role: args.plan.as_deref(),
            implement_role: &args.implement,
            review_role: args.review.as_deref(),
            task_source: args.task_file,
            model: args.model,
            base_ref: args.base_ref,
            sentinel_hook_path: Some(repo.join(".caucus").join("bin").join("sentinel-stop")),
            skip_permissions: !args.require_permissions,
            retry_on_block: args.retry_on_block,
            step_timeout: std::time::Duration::from_secs(args.step_timeout_secs),
            placement: args.placement.to_tmux(),
        },
    )
    .await
    .map_err(|err| anyhow!("pipeline failed: {err}"))?;

    // Register every spawned agent under the session so `session show` /
    // `agent list` continue to surface them.
    for step in [
        outcome.plan.as_ref(),
        outcome.implement.as_ref(),
        outcome.review.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        session.register_agent(&step.role, step.agent_id);
    }
    write_session(&session)?;

    emit(
        format,
        &serde_json::json!({
            "session_id": id.to_string(),
            "pipeline_number": outcome.pipeline_number,
            "worktree_path": outcome.worktree_path,
            "worktree_branch": outcome.worktree_branch,
            "status": outcome.status,
            "attempts": outcome.attempts,
            "plan": outcome.plan,
            "implement": outcome.implement,
            "review": outcome.review,
        }),
        || {
            format!(
                "pipeline #{} done — status={:?}, attempts={}, worktree={}",
                outcome.pipeline_number,
                outcome.status,
                outcome.attempts,
                outcome.worktree_path.display()
            )
        },
    );
    Ok(())
}

/// Find the most recent Meeting-kind agent for `role` in this session.
/// Used by `caucus execute start --continue-meeting`.
fn lookup_meeting_agent(session: &Session, role: &str) -> Result<AgentId> {
    for (r, id) in session.agents.iter().rev() {
        if r != role {
            continue;
        }
        if let Ok(m) = crate::agent::manifest::read_json(&session.session_root, *id) {
            if matches!(m.kind, crate::agent::manifest::AgentKind::Meeting) {
                return Ok(*id);
            }
        }
    }
    Err(anyhow!(
        "no meeting-phase agent for role {role} in this session"
    ))
}

fn lookup_execute_agent(session: &Session, role: &str) -> Result<AgentId> {
    for (r, id) in session.agents.iter().rev() {
        if r != role {
            continue;
        }
        if let Ok(m) = crate::agent::manifest::read_json(&session.session_root, *id) {
            if matches!(m.kind, crate::agent::manifest::AgentKind::Execute) {
                return Ok(*id);
            }
        }
    }
    Err(anyhow!("no execute agent for role {role}"))
}

/// Exit-code gate for CEO polling loops. Returns 0 when the session is in a
/// terminal state, 1 when it's still active. With `--format json`, also
/// prints `{session_id, state, terminal}` to stdout so the CEO can read both
/// the bit and the underlying state in one call. With `--format text`
/// (default), stdout is silent — only the exit code matters.
pub fn session_is_terminal(
    repo: &Path,
    format: OutputFormat,
    args: SessionIsTerminalArgs,
) -> Result<u8> {
    use crate::session::record::SessionRecordError;
    let id = parse_session_id(&args.session_id)?;
    let (state, terminal, kind) = match read_session(repo, id) {
        Ok(s) => {
            let t = s.state.is_terminal();
            (Some(s.state), t, if t { "terminal" } else { "active" })
        }
        // ENOENT on the session.json → session was never created or was
        // already cleaned up. From a polling perspective that's strictly
        // *more* terminal than `Abandoned`; report exit 0 so wakeup loops
        // self-stop instead of treating "gone" the same as "active" (1).
        Err(SessionRecordError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            (None, true, "missing")
        }
        Err(other) => return Err(other.into()),
    };
    if matches!(format, OutputFormat::Json) {
        emit(
            format,
            &serde_json::json!({
                "session_id": id.to_string(),
                "state": state,
                "terminal": terminal,
                "kind": kind,
            }),
            String::new,
        );
    }
    Ok(if terminal { 0 } else { 1 })
}

pub async fn session_relayout(
    repo: &Path,
    format: OutputFormat,
    args: SessionRelayoutArgs,
) -> Result<()> {
    let id = parse_session_id(&args.session_id)?;
    let session = read_session(repo, id)?;
    let tmux = TmuxService::new();
    // Count live role panes + the operator's pane. The session manifest
    // is the source of truth for active agents; if any have been killed,
    // their tmux_pane_id is still recorded but `pane_exists` would say no.
    // For layout purposes we count manifests — the worst case is a
    // slightly over-counted layout, which tmux just absorbs.
    let pane_count = session.agents.len() + 1;
    tmux.apply_layout(args.layout.as_tmux_name(), pane_count, None)
        .await?;
    emit(
        format,
        &serde_json::json!({
            "session_id": id.to_string(),
            "layout": args.layout.as_tmux_name().unwrap_or("auto"),
            "pane_count": pane_count,
        }),
        || {
            format!(
                "relayout applied ({:?}, {pane_count} panes counted)",
                args.layout
            )
        },
    );
    Ok(())
}

pub fn session_transcript(
    repo: &Path,
    format: OutputFormat,
    args: SessionTranscriptArgs,
) -> Result<()> {
    let id = parse_session_id(&args.session_id)?;
    let session = read_session(repo, id)?;
    let last = session.current_round.max(1);
    let path = crate::round::transcript::assemble(
        &session.session_root,
        last,
        &session.roles,
        &session.topic,
    )?;
    emit(
        format,
        &serde_json::json!({
            "session_id": id.to_string(),
            "transcript_path": path,
            "rounds": last,
        }),
        || format!("transcript written to {}", path.display()),
    );
    Ok(())
}

// ---- agent ---------------------------------------------------------------

pub fn agent_list(repo: &Path, format: OutputFormat, args: AgentListArgs) -> Result<()> {
    let id = parse_session_id(&args.session_id)?;
    let session = read_session(repo, id)?;
    let mut entries = Vec::new();
    for (role, agent_id) in &session.agents {
        let Ok(manifest) = crate::agent::manifest::read_json(&session.session_root, *agent_id)
        else {
            continue;
        };
        let keep = match args.kind {
            AgentKindFilter::All => true,
            AgentKindFilter::Meeting => {
                matches!(manifest.kind, crate::agent::manifest::AgentKind::Meeting)
            }
            AgentKindFilter::Execute => {
                matches!(manifest.kind, crate::agent::manifest::AgentKind::Execute)
            }
        };
        if !keep {
            continue;
        }
        entries.push(serde_json::json!({
            "role": role,
            "agent_id": agent_id.to_string(),
            "kind": manifest.kind,
            "status": manifest.status,
            "derived_state": manifest.derived_state,
            "tmux_pane_id": manifest.tmux_pane_id,
            "worktree_path": manifest.worktree_path,
        }));
    }
    emit(format, &entries, || {
        if entries.is_empty() {
            "no agents".into()
        } else {
            entries
                .iter()
                .map(|e| {
                    format!(
                        "{:<10} {} kind={} state={}",
                        e["role"].as_str().unwrap_or(""),
                        e["agent_id"].as_str().unwrap_or(""),
                        e["kind"],
                        e["derived_state"],
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    Ok(())
}

pub fn agent_show(repo: &Path, format: OutputFormat, args: AgentShowArgs) -> Result<()> {
    let session_id = parse_session_id(&args.session)?;
    let session = read_session(repo, session_id)?;
    let agent_id = parse_agent_id(&args.agent_id)?;
    let manifest = crate::agent::manifest::read_json(&session.session_root, agent_id)?;
    emit(format, &manifest, || {
        format!(
            "agent {} ({}) state={:?}",
            manifest.agent_id, manifest.role, manifest.derived_state
        )
    });
    Ok(())
}

pub async fn agent_send(_repo: &Path, args: AgentSendArgs) -> Result<()> {
    let session_id = parse_session_id(&args.session)?;
    let agent_id = parse_agent_id(&args.agent_id)?;
    // We rely on `--repo` (already resolved by caller) to find the manifest.
    // But this function only needs the manifest's pane id, so read by id+session.
    let manifest_path = crate::agent::manifest::AgentManifest::json_path(
        // Resolve session_root indirectly via the global lookup.
        &resolve_session_root_from_id(_repo, session_id)?,
        agent_id,
    );
    let manifest: AgentManifest = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
    let pane = manifest
        .tmux_pane_id
        .ok_or_else(|| anyhow!("agent has no tmux pane attached"))?;
    let tmux = TmuxService::new();
    tmux.send_text(&pane, &args.message, true).await?;
    note(&format!("sent {} bytes to pane {pane}", args.message.len()));
    Ok(())
}

fn resolve_session_root_from_id(repo: &Path, id: SessionId) -> Result<PathBuf> {
    let s = read_session(repo, id)?;
    Ok(s.session_root)
}

pub async fn agent_kill(repo: &Path, format: OutputFormat, args: AgentKillArgs) -> Result<()> {
    let session_id = parse_session_id(&args.session)?;
    let session = read_session(repo, session_id)?;
    let agent_id = parse_agent_id(&args.agent_id)?;
    let mut manifest = crate::agent::manifest::read_json(&session.session_root, agent_id)?;
    let tmux = TmuxService::new();
    if let Some(pane) = manifest.tmux_pane_id.clone() {
        let _ = tmux.kill_pane(&pane).await;
    }
    let killed_sentinel = crate::sentinel::Sentinel::new(
        session_id,
        agent_id,
        SentinelKind::Killed,
        Some("killed by orchestrator".into()),
        None,
    );
    write_sentinel(&session.session_root, &killed_sentinel)?;
    manifest.status = crate::agent::derive_state::RawStatus::Failed;
    manifest.derived_state = crate::agent::derive_state::DerivedState::TrulyIdle;
    manifest.completed_at = Some(chrono::Utc::now());
    manifest.error = Some("killed by orchestrator".into());
    crate::agent::manifest::write_json(&manifest, &session.session_root)?;
    emit(
        format,
        &serde_json::json!({"agent_id": agent_id.to_string()}),
        || format!("agent {agent_id} killed"),
    );
    Ok(())
}

// ---- role ----------------------------------------------------------------

pub fn role_list(repo: &Path, format: OutputFormat) -> Result<()> {
    let registry = build_registry(repo)?;
    let entries: Vec<_> = registry
        .names()
        .map(|name| {
            let spec = registry.get(name).unwrap();
            serde_json::json!({
                "name": name,
                "description": spec.description,
                "permission_mode": spec.permission_mode,
                "allowed_tools": spec.allowed_tools_csv(),
                "system_prompt_template": spec.system_prompt_template,
            })
        })
        .collect();
    emit(format, &entries, || {
        entries
            .iter()
            .map(|e| {
                format!(
                    "{}: {} [{}] mode={}",
                    e["name"], e["description"], e["allowed_tools"], e["permission_mode"]
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    Ok(())
}

pub fn role_show(repo: &Path, format: OutputFormat, args: RoleShowArgs) -> Result<()> {
    let registry = build_registry(repo)?;
    let spec = registry
        .get(&args.name)
        .map_err(|err| anyhow!("{err}"))?
        .clone();
    let body =
        std::fs::read_to_string(resolve_role_template(repo, &spec.system_prompt_template)).ok();
    emit(
        format,
        &serde_json::json!({"spec": spec, "prompt_body": body}),
        || {
            format!(
                "role {}\ndescription: {}\nmode: {:?}\nallowed_tools: {}\ntemplate: {}",
                spec.name,
                spec.description,
                spec.permission_mode,
                spec.allowed_tools_csv(),
                spec.system_prompt_template.display(),
            )
        },
    );
    Ok(())
}

// ---- sentinel ------------------------------------------------------------

pub fn sentinel_write(_repo: &Path, format: OutputFormat, args: SentinelWriteArgs) -> Result<()> {
    let session_id = parse_session_id(&args.session)?;
    let agent_id = parse_agent_id(&args.agent)?;
    // Find session_root via the repo (passed by the env or --repo).
    let repo = std::env::var("CAUCUS_SESSION_ROOT").ok().map(PathBuf::from);
    let session_root = if let Some(root) = repo.clone() {
        root
    } else {
        // Fall back to scanning .caucus/sessions/<id>/ under the current
        // working directory.
        let cwd = std::env::current_dir().context("could not read cwd")?;
        cwd.join(".caucus")
            .join("sessions")
            .join(session_id.to_string())
    };
    std::fs::create_dir_all(session_root.join("agents"))?;
    let raw = read_stdin_json();
    let sentinel = crate::sentinel::Sentinel::new(
        session_id,
        agent_id,
        args.kind.into(),
        args.last_message,
        raw,
    );
    let path = write_sentinel(&session_root, &sentinel)?;
    emit(format, &serde_json::json!({"sentinel_path": path}), || {
        format!("sentinel written to {}", path.display())
    });
    Ok(())
}

fn read_stdin_json() -> Option<serde_json::Value> {
    use std::io::Read as _;
    use std::time::Duration;
    // If stdin is a TTY, skip — Claude only pipes via stdin during a hook.
    let mut buf = Vec::new();
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    // 32 KiB cap; hook payloads are small.
    let _ = lock.read_to_end(&mut buf);
    let _ = Duration::from_millis(0); // touch import; tokio not used here.
    if buf.is_empty() {
        return None;
    }
    serde_json::from_slice(&buf).ok()
}

// ---- watch ---------------------------------------------------------------

pub async fn watch(repo: &Path, args: WatchArgs) -> Result<()> {
    let id = parse_session_id(&args.session_id)?;
    let session = read_session(repo, id)?;
    let agents_dir = session.session_root.join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    let (watcher, mut rx) = crate::sentinel::watch(&agents_dir)?;
    let _w = watcher;

    let started_at = chrono::Utc::now();
    let startup = serde_json::json!({
        "kind": "started",
        "session_id": id.to_string(),
        "agents_dir": agents_dir,
        "ts": started_at,
    });
    println!("{startup}");
    note(&format!(
        "watching {} (Ctrl-C to stop)",
        agents_dir.display()
    ));

    // Pane-hint poller fan-in. The reverse index (pane_id → (role,
    // agent_id)) is built once at startup; pane assignments do not change
    // during a session (manifest.tmux_pane_id is stamped at spawn).
    let tmux = TmuxService::new();
    let (hint_tx, mut hint_rx) = tokio::sync::mpsc::unbounded_channel::<(
        String,
        AgentId,
        String,
        crate::status::HintUpdate,
    )>();
    let mut pane_index: std::collections::HashMap<String, (String, AgentId)> =
        std::collections::HashMap::new();
    for (role, agent_id) in &session.agents {
        let manifest = match crate::agent::manifest::read_json(&session.session_root, *agent_id) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let Some(pane) = manifest.tmux_pane_id.clone() else {
            continue;
        };
        pane_index.insert(pane.clone(), (role.clone(), *agent_id));
        let mut poller_rx = crate::status::spawn_poller(
            tmux.clone(),
            pane.clone(),
            std::time::Duration::from_secs(2),
            30,
        );
        let role = role.clone();
        let agent_id = *agent_id;
        let pane_for_task = pane.clone();
        let hint_tx = hint_tx.clone();
        tokio::spawn(async move {
            while let Some(update) = poller_rx.recv().await {
                if hint_tx
                    .send((role.clone(), agent_id, pane_for_task.clone(), update))
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    drop(hint_tx); // close the channel once all forwarder tasks drop their clone

    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat.tick().await; // immediate tick is consumed silently.

    let mut usr2 = crate::notify::Usr2Stream::new().ok();
    let usr2_label = if usr2.is_some() { "yes" } else { "no" };
    tracing::debug!(usr2 = usr2_label, "watch loop ready");

    let mut last_round_complete_emitted: Option<u32> = None;

    loop {
        let usr2_recv = async {
            match usr2.as_mut() {
                Some(s) => s.recv().await,
                None => std::future::pending::<Option<()>>().await,
            }
        };
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                let line = serde_json::json!({"kind": "stopped", "reason": "sigint"});
                println!("{line}");
                return Ok(());
            }
            _ = heartbeat.tick() => {
                let line = serde_json::json!({
                    "kind": "heartbeat",
                    "session_id": id.to_string(),
                    "ts": chrono::Utc::now(),
                });
                println!("{line}");
            }
            _ = usr2_recv => {
                let line = serde_json::json!({
                    "kind": "wake",
                    "source": "sigusr2",
                    "ts": chrono::Utc::now(),
                });
                println!("{line}");
            }
            event = rx.recv() => {
                match event {
                    None => {
                        // Watcher torn down; exit cleanly.
                        let line = serde_json::json!({"kind": "stopped", "reason": "watcher_closed"});
                        println!("{line}");
                        return Ok(());
                    }
                    Some(crate::sentinel::WatchEvent::Sentinel { path, sentinel }) => {
                        let line = serde_json::json!({
                            "kind": "sentinel",
                            "path": path,
                            "agent_id": sentinel.agent_id.to_string(),
                            "sentinel_kind": sentinel.kind,
                            "ts": sentinel.ts,
                        });
                        println!("{line}");
                        let _ = crate::round::record_sentinel(&session.session_root, &sentinel);
                        emit_round_progress(repo, id, &mut last_round_complete_emitted);
                    }
                    Some(crate::sentinel::WatchEvent::ParseDeferred { path, reason }) => {
                        let line = serde_json::json!({
                            "kind": "parse_deferred",
                            "path": path,
                            "reason": reason,
                        });
                        println!("{line}");
                    }
                    Some(crate::sentinel::WatchEvent::WatcherError { message }) => {
                        let line = serde_json::json!({
                            "kind": "watcher_error",
                            "message": message,
                        });
                        println!("{line}");
                    }
                    Some(other) => {
                        // Watcher does not synthesise pane_hint/pane_gone/
                        // round_progress/round_complete — those originate
                        // from the watch loop itself. Defensive: ignore.
                        tracing::debug!(?other, "unexpected synthesised event on watcher rx");
                    }
                }
            }
            hint = hint_rx.recv() => {
                let Some((role, agent_id, pane, update)) = hint else {
                    // All poller forwarders dropped; nothing more to do
                    // on this channel — keep looping for sentinels.
                    continue;
                };
                let ts = chrono::Utc::now();
                if update.current.is_none() {
                    let line = serde_json::json!({
                        "kind": "pane_gone",
                        "role": role,
                        "agent_id": agent_id.to_string(),
                        "pane": pane,
                        "ts": ts,
                    });
                    println!("{line}");
                    let _ = crate::round::record_pane_gone(
                        &session.session_root,
                        agent_id,
                        pane.clone(),
                    );
                } else {
                    let line = serde_json::json!({
                        "kind": "pane_hint",
                        "role": role,
                        "agent_id": agent_id.to_string(),
                        "pane": pane,
                        "previous": update.previous,
                        "current": update.current,
                        "ts": ts,
                    });
                    println!("{line}");
                    let _ = crate::round::record_pane_hint(
                        &session.session_root,
                        agent_id,
                        update.current,
                    );
                }
                emit_round_progress(repo, id, &mut last_round_complete_emitted);
            }
        }
    }
}

/// Compute the current round's response-collection snapshot and emit a
/// `round_progress` JSON line on stdout. On the first false→true
/// transition of `all_responses_complete` for a given round number,
/// additionally emit a `round_complete` line (idempotency latched by
/// `last_emitted`).
fn emit_round_progress(repo: &Path, session_id: SessionId, last_emitted: &mut Option<u32>) {
    let session = match read_session(repo, session_id) {
        Ok(s) => s,
        Err(err) => {
            tracing::debug!(%err, "watch: read_session failed during emit_round_progress");
            return;
        }
    };
    if session.current_round == 0 {
        return;
    }
    let layout =
        crate::round::RoundLayout::new(session.session_root.clone(), session.current_round);
    let status = match crate::round::round_status(&layout, &session.agents) {
        Ok(s) => s,
        Err(err) => {
            tracing::debug!(%err, "watch: round_status failed");
            return;
        }
    };
    let total = status.roles.len() as u32;
    let completed = status
        .roles
        .iter()
        .filter(|r| r.response_bytes.unwrap_or(0) > 0)
        .count() as u32;
    let mut states: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for r in &status.roles {
        let key = serde_json::to_value(r.derived_state)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "unknown".to_string());
        *states.entry(key).or_insert(0) += 1;
    }
    let now = chrono::Utc::now();
    let progress = serde_json::json!({
        "kind": "round_progress",
        "session_id": session_id.to_string(),
        "round_number": status.round_number,
        "completed": completed,
        "total": total,
        "states": states,
        "ts": now,
    });
    println!("{progress}");
    if status.all_responses_complete && *last_emitted != Some(status.round_number) {
        let complete = serde_json::json!({
            "kind": "round_complete",
            "session_id": session_id.to_string(),
            "round_number": status.round_number,
            "ts": now,
        });
        println!("{complete}");
        *last_emitted = Some(status.round_number);
    }
}

// ---- top-level dispatch --------------------------------------------------

pub async fn dispatch(cli: Cli) -> Result<u8> {
    let repo = resolve_repo(cli.repo)?;
    let format = cli.format;
    match cli.command {
        Command::Init(args) => init(&repo, args)?,
        Command::Doctor => doctor(&repo, format)?,
        Command::Session(SessionArgs { action }) => match action {
            SessionAction::New(args) => session_new(&repo, format, args).await?,
            SessionAction::List => session_list(&repo, format)?,
            SessionAction::Show(args) => session_show(&repo, format, args)?,
            SessionAction::Converge(args) => session_converge(&repo, format, args)?,
            SessionAction::Deadlock(args) => session_deadlock(&repo, format, args).await?,
            SessionAction::Kill(args) => session_kill(&repo, format, args).await?,
            SessionAction::Transcript(args) => session_transcript(&repo, format, args)?,
            SessionAction::IsTerminal(args) => return session_is_terminal(&repo, format, args),
            SessionAction::Relayout(args) => session_relayout(&repo, format, args).await?,
        },
        Command::Round(RoundArgs { action }) => match action {
            RoundAction::Start(args) => round_start(&repo, format, args).await?,
            RoundAction::Status(args) => round_status(&repo, format, args)?,
            RoundAction::Next(args) => round_next(&repo, format, args).await?,
        },
        Command::Execute(ExecuteArgs { action }) => match action {
            ExecuteAction::Start(args) => execute_start(&repo, format, args).await?,
            ExecuteAction::Pipeline(args) => execute_pipeline(&repo, format, args).await?,
            ExecuteAction::Status(args) => execute_status(&repo, format, args)?,
            ExecuteAction::Finish(args) => execute_finish(&repo, format, args).await?,
            ExecuteAction::Abandon(args) => execute_abandon(&repo, format, args).await?,
        },
        Command::Agent(AgentArgs { action }) => match action {
            AgentAction::List(args) => agent_list(&repo, format, args)?,
            AgentAction::Show(args) => agent_show(&repo, format, args)?,
            AgentAction::Send(args) => agent_send(&repo, args).await?,
            AgentAction::Kill(args) => agent_kill(&repo, format, args).await?,
        },
        Command::Role(RoleArgs { action }) => match action {
            RoleAction::List => role_list(&repo, format)?,
            RoleAction::Show(args) => role_show(&repo, format, args)?,
        },
        Command::Sentinel(SentinelArgs { action }) => match action {
            SentinelAction::Write(args) => sentinel_write(&repo, format, args)?,
        },
        Command::Watch(args) => watch(&repo, args).await?,
        Command::Ceo(CeoArgs { action }) => ceo(&repo, format, action)?,
    }
    Ok(exit::OK)
}

fn ceo(repo: &Path, format: OutputFormat, action: CeoAction) -> Result<()> {
    match action {
        CeoAction::Enable => {
            let report = crate::cli::ceo_brief::enable(repo)?;
            emit(
                format,
                &serde_json::json!({
                    "commands_dir": report.commands_dir,
                    "on_path": report.on_path,
                    "off_path": report.off_path,
                    "action": format!("{:?}", report.action),
                    "enabled": report.enabled,
                }),
                || {
                    format!(
                        "CEO slash commands installed in {} ({:?}).\n\
                         In your Claude Code session type `/caucus-ceo` to activate, \
                         `/caucus-ceo-off` to deactivate. No restart needed.",
                        report.commands_dir.display(),
                        report.action
                    )
                },
            );
        }
        CeoAction::Disable => {
            let report = crate::cli::ceo_brief::disable(repo)?;
            emit(
                format,
                &serde_json::json!({
                    "commands_dir": report.commands_dir,
                    "on_path": report.on_path,
                    "off_path": report.off_path,
                    "action": format!("{:?}", report.action),
                    "enabled": report.enabled,
                }),
                || {
                    format!(
                        "CEO slash commands removed from {} ({:?}). \
                         Already-running sessions keep whichever mode you last toggled.",
                        report.commands_dir.display(),
                        report.action
                    )
                },
            );
        }
        CeoAction::Status => {
            let on = crate::cli::ceo_brief::status(repo)?;
            emit(format, &serde_json::json!({"enabled": on}), || {
                if on {
                    "CEO slash commands are installed (`/caucus-ceo` / `/caucus-ceo-off`)".into()
                } else {
                    "CEO slash commands are NOT installed — run `caucus ceo enable`".into()
                }
            });
        }
        CeoAction::Show => {
            print!("{}", crate::cli::ceo_brief::CEO_ON_BODY);
        }
    }
    Ok(())
}
