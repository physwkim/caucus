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
# caucus Stop hook. Invoked by Claude Code when a turn ends in a pane spawned
# by `caucus`. CAUCUS_SESSION_ID and CAUCUS_AGENT_ID are injected into the
# pane's env at spawn time. The hook receives the full hook payload on stdin;
# we forward it verbatim as `--raw` if you teach caucus to record it.
set -e
: "${CAUCUS_SESSION_ID:?CAUCUS_SESSION_ID not set — this pane was not spawned by caucus}"
: "${CAUCUS_AGENT_ID:?CAUCUS_AGENT_ID not set}"
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
            },
        )
        .await?;

        let _ = agent_id; // Already persisted in the manifest above.
        session.register_agent(role_name, outcome.manifest.agent_id);
    }

    // Re-balance the window after all role panes are spawned. Otherwise the
    // last role ends up in a 12.5% slice because each split-window halves
    // the current pane. `--layout` (default `auto`) picks even-horizontal
    // for 2 panes and tiled for 3+ panes (a 2D grid that uses both row and
    // column splits). Includes the CEO's own pane in the count.
    tmux.apply_layout(args.layout.as_tmux_name(), session.agents.len() + 1, None)
        .await
        .ok();

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
        },
    )
    .await?;
    session.register_agent(&args.role, outcome.agent.agent_id);
    // Rebalance like session_new — execute panes accumulate too.
    let total_panes = session.agents.len() + 1;
    tmux.apply_layout(args.layout.as_tmux_name(), total_panes, None)
        .await
        .ok();
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

    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat.tick().await; // immediate tick is consumed silently.

    let mut usr2 = crate::notify::Usr2Stream::new().ok();
    let usr2_label = if usr2.is_some() { "yes" } else { "no" };
    tracing::debug!(usr2 = usr2_label, "watch loop ready");

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
                }
            }
        }
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
