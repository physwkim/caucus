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
    let session = create_session_with_meeting_agents(repo, &args).await?;
    let json = serde_json::json!({
        "session_id": session.id.to_string(),
        "state": session.state,
        "roles": session.roles,
        "agents": session.agents.iter().map(|(role, id)| {
            serde_json::json!({"role": role, "agent_id": id.to_string()})
        }).collect::<Vec<_>>(),
        "session_root": session.session_root,
        "next_action": format!(
            "Write a SHORT round-1 agenda to a temp file (one paragraph stating the topic + what each role should produce). \
             Then call `caucus round start {}` with that agenda. Do NOT read source files yourself — that's the architect/reviewer's job.",
            session.id
        ),
        "next_command_suggestion": format!(
            "caucus round start {} --agenda-file <path-to-agenda.md> --format json",
            session.id
        ),
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

/// Inner helper: validate roles, write the session JSON, transition to
/// `MeetingInProgress`, and spawn one meeting pane per role. Returns the
/// freshly-written `Session` so callers (`session_new`, `auto`) can chain
/// further work without re-parsing it from disk.
pub(crate) async fn create_session_with_meeting_agents(
    repo: &Path,
    args: &SessionNewArgs,
) -> Result<Session> {
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
        args.topic.clone(),
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

    Ok(session)
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
    let decision_path = session.session_root.join("decision.md");
    emit(
        format,
        &serde_json::json!({
            "session_id": id.to_string(),
            "state": session.state,
            "decision_path": decision_path,
            "next_action": format!(
                "Decision locked. Run the execute pipeline to chain plan → impl → review automatically: \
                 `caucus execute pipeline {id} --task-file {dec} --plan architect --implement backend --review reviewer`. \
                 Prefer pipeline over manual `execute start` unless you need step-by-step control.",
                dec = decision_path.display(),
            ),
            "next_command_suggestion": format!(
                "caucus execute pipeline {id} --task-file {dec} --plan architect --implement backend --review reviewer --format json",
                dec = decision_path.display(),
            ),
        }),
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
            "next_action": format!(
                "Session is deadlocked. Pick a policy: re-run `caucus session deadlock {id} --escalate` \
                 (writes escalated.signal then Abandoned) or `--explore` (one execute agent per role \
                 in parallel worktrees). If you genuinely want to leave it stuck, do nothing.",
            ),
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

    // Validate --lead role belongs to this session before doing any tmux work.
    if let Some(lead) = &args.lead {
        if !session.agents.iter().any(|(role, _)| role == lead) {
            return Err(anyhow!(
                "--lead role `{lead}` is not in this session (available: {})",
                session
                    .roles
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    let payload = if let Some(lead_role) = args.lead.clone() {
        // Architect-led flow: nudge lead first, wait for sentinel, then
        // compose follower briefs and nudge the rest.
        let lead_outcome = run_lead_phase(
            &tmux,
            &session,
            &layout,
            &lead_role,
            std::time::Duration::from_secs(args.lead_timeout_secs),
        )
        .await?;

        let mut followers = Vec::new();
        for (role, agent_id) in &session.agents {
            if role == &lead_role {
                continue;
            }
            let manifest = crate::agent::manifest::read_json(&session.session_root, *agent_id)?;
            let pane = match manifest.tmux_pane_id.clone() {
                Some(p) => p,
                None => continue,
            };
            let brief_path = crate::round::write_follower_brief(&layout, role, &lead_role)
                .map_err(|err| anyhow!("write follower brief: {err}"))?;
            crate::round::nudge_pane_with_brief(
                &tmux,
                &pane,
                &brief_path,
                &layout.response_path(role),
            )
            .await
            .map_err(|err| anyhow!("nudge follower {role}: {err}"))?;
            followers.push(serde_json::json!({
                "role": role,
                "agent_id": agent_id.to_string(),
                "brief_path": brief_path,
            }));
        }

        serde_json::json!({
            "session_id": id.to_string(),
            "round": round,
            "roles": session.roles,
            "mode": "lead",
            "lead": {
                "role": lead_role,
                "agent_id": lead_outcome.agent_id.to_string(),
                "response_path": lead_outcome.response_path,
            },
            "followers": followers,
            "next_action": format!(
                "Lead ({lead_role}) finished. Follower briefs are sent. Schedule a wakeup ~5 minutes out. \
                 On wake: FIRST `caucus session is-terminal {id}` (exit 0 → stop). Then `caucus round \
                 status {id} --format json` until `all_responses_complete` is true. Read each follower's \
                 response.md to see how they reacted to the lead's proposal."
            ),
            "polling_hint": serde_json::json!({
                "first_check": format!("caucus session is-terminal {id}"),
                "status_check": format!("caucus round status {id} --format json"),
                "ready_when": ".all_responses_complete == true",
            }),
        })
    } else {
        // Parallel flow: every role gets the same agenda at once.
        for (role, agent_id) in &session.agents {
            let manifest = crate::agent::manifest::read_json(&session.session_root, *agent_id)?;
            if let Some(pane) = manifest.tmux_pane_id.clone() {
                crate::round::nudge_role(&tmux, &pane, &layout, role).await?;
            }
        }
        serde_json::json!({
            "session_id": id.to_string(),
            "round": round,
            "roles": session.roles,
            "mode": "parallel",
            "next_action": format!(
                "Roles are working. Schedule a wakeup ~5 minutes out. On wake, FIRST run \
                 `caucus session is-terminal {id}` — exit 0 means stop polling. Otherwise check \
                 `caucus round status {id} --format json` and read response files when \
                 `all_responses_complete` is true. Do NOT read project source files yourself."
            ),
            "polling_hint": serde_json::json!({
                "first_check": format!("caucus session is-terminal {id}"),
                "status_check": format!("caucus round status {id} --format json"),
                "ready_when": ".all_responses_complete == true",
            }),
        })
    };

    write_session(&session)?;
    emit(format, &payload, || {
        format!("round {round} started for session {id}")
    });
    Ok(())
}

#[derive(Debug)]
struct LeadOutcome {
    agent_id: crate::session::id::AgentId,
    response_path: PathBuf,
}

/// Nudge the lead role's pane with the standard round agenda, then await
/// its Stop sentinel. Returns the lead's agent_id + response path once we've
/// confirmed the response file is non-empty.
async fn run_lead_phase(
    tmux: &TmuxService,
    session: &Session,
    layout: &crate::round::RoundLayout,
    lead_role: &str,
    timeout: std::time::Duration,
) -> Result<LeadOutcome> {
    let (lead_agent_id, lead_pane) = session
        .agents
        .iter()
        .find(|(r, _)| r == lead_role)
        .map(|(_, id)| *id)
        .and_then(|id| {
            crate::agent::manifest::read_json(&session.session_root, id)
                .ok()
                .and_then(|m| m.tmux_pane_id.clone().map(|p| (id, p)))
        })
        .ok_or_else(|| anyhow!("lead role {lead_role} has no live tmux pane"))?;

    let agents_dir = session.session_root.join("agents");
    std::fs::create_dir_all(&agents_dir)
        .with_context(|| format!("creating {} for sentinel watcher", agents_dir.display()))?;
    let (watcher, mut rx) = crate::sentinel::watch(&agents_dir)
        .map_err(|err| anyhow!("sentinel::watch failed: {err}"))?;
    let _watcher_guard = watcher;

    crate::round::nudge_role(tmux, &lead_pane, layout, lead_role)
        .await
        .map_err(|err| anyhow!("nudge lead {lead_role}: {err}"))?;

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(anyhow!(
                "lead {lead_role} did not produce a sentinel within {}s",
                timeout.as_secs()
            ));
        }
        let event = tokio::time::timeout(remaining, rx.recv())
            .await
            .map_err(|_| {
                anyhow!(
                    "lead {lead_role} did not produce a sentinel within {}s",
                    timeout.as_secs()
                )
            })?;
        match event {
            Some(crate::sentinel::WatchEvent::Sentinel { sentinel, .. })
                if sentinel.agent_id == lead_agent_id =>
            {
                // Ingest the sentinel into the manifest so follow-up
                // `round status` / `session show` reflect it.
                let _ = crate::round::record_sentinel(&session.session_root, &sentinel);
                break;
            }
            Some(_) => continue,
            None => return Err(anyhow!("sentinel watcher closed before lead responded")),
        }
    }

    let response_path = layout.response_path(lead_role);
    let response_size = std::fs::metadata(&response_path)
        .map(|m| m.len())
        .unwrap_or(0);
    if response_size == 0 {
        return Err(anyhow!(
            "lead {lead_role} produced an empty response at {} — aborting before nudging followers",
            response_path.display()
        ));
    }
    Ok(LeadOutcome {
        agent_id: lead_agent_id,
        response_path,
    })
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
    let mut payload = serde_json::to_value(&status).unwrap_or_else(|_| serde_json::json!({}));
    let next_action = if status.all_responses_complete {
        format!(
            "All response files are populated. Read each `.caucus/sessions/{id}/round-{rn:02}/response-<role>.md`, \
             synthesize, and decide:\n\
             - Consensus reached → write decision.md and run `caucus session converge {id} --decision-file <path>`.\n\
             - Need another round → write next agenda and run `caucus round next {id} --agenda-file <path>`.\n\
             - Stuck → `caucus session deadlock {id}` then pick --escalate or --explore.",
            rn = status.round_number
        )
    } else {
        format!(
            "Some roles haven't responded yet. Schedule another wakeup (~5 min). First call \
             `caucus session is-terminal {id}` on wake to bail out cleanly if the session was killed."
        )
    };
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("next_action".into(), serde_json::Value::String(next_action));
    }
    emit(format, &payload, || {
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
            lead: args.lead,
            lead_timeout_secs: args.lead_timeout_secs,
        },
    )
    .await
}

/// Exit-code gate for CEO wakeup loops on a single round. Returns
/// `Ok(exit::OK)` (0) once every role's response file is non-empty,
/// `Ok(exit::GENERIC_FAILURE)` (1) on timeout / ctrl-c / watcher tear-down
/// **or any infrastructure error** (read_session / round_status /
/// sentinel::watch / fs::create_dir_all), and `Ok(exit::SESSION_TERMINAL)`
/// (3) when the session reached a terminal state before the round could
/// complete. Only user-error paths (`parse_session_id`, invalid round
/// argument) return `Err` so the dispatch layer can map them to
/// `exit::USER_ERROR` (2). Infrastructure errors are *not* propagated as
/// `Err` because `map_error_to_code` may pattern-match them onto
/// `exit::ENVIRONMENT_ERROR` (3), which numerically collides with
/// `exit::SESSION_TERMINAL` and would make those two outcomes
/// indistinguishable to a CEO wakeup loop.
///
/// After arming the notify watcher the handler writes the single line
/// `ready` to stderr — integration tests synchronise on it to avoid the
/// FSEvents arm-race under parallel test execution. Stdout is reserved
/// for the JSON result (R3), so the marker stays on stderr.
///
/// Read-only: no `record_sentinel`, no `write_json`, no `transition`. Uses
/// `sentinel::watch` purely as a wake-up source — each notify event
/// triggers a fresh `round::round_status` re-read off disk.
pub async fn round_wait(repo: &Path, format: OutputFormat, args: RoundWaitArgs) -> Result<u8> {
    // User-error path — keep `?` / `bail` so the dispatch layer maps to
    // exit code 2 via `map_error_to_code` substring rules.
    let id = parse_session_id(&args.session_id)?;

    let session = match read_session(repo, id) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("caucus round wait: read_session failed: {err:#}");
            return Ok(exit::GENERIC_FAILURE);
        }
    };

    let target_round = args.round.unwrap_or(session.current_round);
    if target_round == 0 {
        return Err(anyhow!("no round has been started yet"));
    }
    if target_round > session.current_round {
        return Err(anyhow!(
            "round {target_round} not started yet (current={})",
            session.current_round
        ));
    }

    let layout = crate::round::RoundLayout::new(session.session_root.clone(), target_round);
    let initial = match crate::round::round_status(&layout, &session.agents) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("caucus round wait: round_status failed: {err:#}");
            return Ok(exit::GENERIC_FAILURE);
        }
    };

    // Terminal-session short-circuit: the round can never complete on its
    // own past this point. Exit 3 before installing the watcher.
    if session.state.is_terminal() {
        emit_wait_result(format, id, target_round, "session_terminal", &initial);
        return Ok(exit::SESSION_TERMINAL);
    }

    // Pre-read guard: already-complete round → exit 0 without arming
    // notify (cheap idempotent re-call).
    if initial.all_responses_complete {
        emit_wait_result(format, id, target_round, "completed_already", &initial);
        return Ok(exit::OK);
    }

    let agents_dir = session.session_root.join("agents");
    if let Err(err) = std::fs::create_dir_all(&agents_dir) {
        eprintln!(
            "caucus round wait: create_dir_all({}) failed: {err}",
            agents_dir.display()
        );
        return Ok(exit::GENERIC_FAILURE);
    }
    let (_w, mut rx) = match crate::sentinel::watch(&agents_dir) {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("caucus round wait: sentinel::watch failed: {err}");
            return Ok(exit::GENERIC_FAILURE);
        }
    };

    // Watcher armed; signal readiness on stderr so integration tests can
    // synchronise on it instead of sleeping. Stdout stays clean for the
    // JSON result (parsed by `parse_wait_stdout`).
    eprintln!("ready");

    // `--timeout-secs 0` → wait forever (only ctrl-c / completion / session
    // terminal can break the loop). std::future::pending keeps the timeout
    // arm from ever firing.
    let timeout_fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
        if args.timeout_secs == 0 {
            Box::pin(std::future::pending())
        } else {
            Box::pin(tokio::time::sleep(std::time::Duration::from_secs(
                args.timeout_secs,
            )))
        };
    tokio::pin!(timeout_fut);

    loop {
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                let s = crate::round::round_status(&layout, &session.agents)
                    .unwrap_or_else(|_| initial.clone());
                emit_wait_result(format, id, target_round, "interrupted", &s);
                return Ok(exit::GENERIC_FAILURE);
            }
            _ = &mut timeout_fut => {
                let s = crate::round::round_status(&layout, &session.agents)
                    .unwrap_or_else(|_| initial.clone());
                emit_wait_result(format, id, target_round, "timed_out", &s);
                return Ok(exit::GENERIC_FAILURE);
            }
            event = rx.recv() => {
                match event {
                    None => {
                        let s = crate::round::round_status(&layout, &session.agents)
                            .unwrap_or_else(|_| initial.clone());
                        emit_wait_result(format, id, target_round, "watcher_closed", &s);
                        return Ok(exit::GENERIC_FAILURE);
                    }
                    Some(_) => {
                        // Re-read the session record so a converge /
                        // deadlock / kill that landed mid-wait is observed.
                        if let Ok(refreshed) = read_session(repo, id) {
                            if refreshed.state.is_terminal() {
                                let s = crate::round::round_status(&layout, &session.agents)
                                    .unwrap_or_else(|_| initial.clone());
                                emit_wait_result(
                                    format,
                                    id,
                                    target_round,
                                    "session_terminal",
                                    &s,
                                );
                                return Ok(exit::SESSION_TERMINAL);
                            }
                        }
                        let s = match crate::round::round_status(&layout, &session.agents) {
                            Ok(s) => s,
                            Err(err) => {
                                eprintln!(
                                    "caucus round wait: round_status failed mid-wait: {err:#}"
                                );
                                return Ok(exit::GENERIC_FAILURE);
                            }
                        };
                        if s.all_responses_complete {
                            emit_wait_result(format, id, target_round, "completed", &s);
                            return Ok(exit::OK);
                        }
                    }
                }
            }
        }
    }
}

/// Emit the single-line JSON result for `caucus round wait` on stdout.
///
/// Bypasses [`emit`]'s pretty-printer so stdout contains exactly one line
/// — robust against future tracing leakage and easy for the integration
/// tests' "last non-empty line" parser to consume.
fn emit_wait_result(
    format: OutputFormat,
    session_id: SessionId,
    round: u32,
    status: &str,
    s: &crate::round::RoundStatus,
) {
    if !matches!(format, OutputFormat::Json) {
        return;
    }
    let total = s.roles.len() as u32;
    let completed = s
        .roles
        .iter()
        .filter(|r| r.response_bytes.unwrap_or(0) > 0)
        .count() as u32;
    let line = serde_json::json!({
        "session_id": session_id.to_string(),
        "round": round,
        "status": status,
        "completed": completed,
        "total": total,
    });
    println!("{line}");
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
    let role_for_hint = args.role.clone();
    emit(
        format,
        &serde_json::json!({
            "session_id": id.to_string(),
            "agent_id": outcome.agent.agent_id.to_string(),
            "worktree_path": outcome.worktree.path,
            "branch": outcome.worktree.branch,
            "next_action": format!(
                "Execute agent is working. Schedule a wakeup (~10 min — implementation takes longer than meeting). \
                 On wake: FIRST `caucus session is-terminal {id}` (exit 0 → stop). Otherwise check \
                 `caucus execute status {id} --format json` until the agent's derived_state is \
                 `finished_cleanable` or blocked. Then `caucus execute finish {id} --role {role}` \
                 (captures commit_provenance + queues worktree cleanup). Don't forget the reviewer \
                 step — consider re-running with `caucus execute pipeline` if you want auto-review.",
                role = role_for_hint
            ),
            "polling_hint": serde_json::json!({
                "first_check": format!("caucus session is-terminal {id}"),
                "status_check": format!("caucus execute status {id} --format json"),
                "ready_when": ".[].derived_state == \"finished_cleanable\""
            }),
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
) -> Result<crate::execute::PipelineOutcome> {
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

    // --continue-meeting: collect per-role claude session ids from each
    // pipeline role's meeting agent and kill those panes so the pipeline's
    // first step can resume the session without conflict. Validated
    // up-front — any missing meeting agent or unrecorded session_id is a
    // user error before the pipeline burns any state.
    let mut resume_by_role: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    if args.continue_meeting {
        let mut pipeline_roles: Vec<&str> = Vec::new();
        if let Some(plan) = args.plan.as_deref() {
            pipeline_roles.push(plan);
        }
        pipeline_roles.push(args.implement.as_str());
        if let Some(review) = args.review.as_deref() {
            pipeline_roles.push(review);
        }
        for role in pipeline_roles {
            if resume_by_role.contains_key(role) {
                continue;
            }
            let meeting_agent_id = lookup_meeting_agent(&session, role)?;
            let manifest =
                crate::agent::manifest::read_json(&session.session_root, meeting_agent_id)?;
            let sid = manifest.claude_session_id.clone().ok_or_else(|| {
                anyhow!(
                    "meeting agent for role {role} ({meeting_agent_id}) has no captured \
                     claude_session_id — wait until at least one Stop hook has fired \
                     (run a round) before retrying with --continue-meeting"
                )
            })?;
            if let Some(pane) = manifest.tmux_pane_id.clone() {
                let _ = tmux.kill_pane(&pane).await;
            }
            resume_by_role.insert(role.to_string(), sid);
        }
    }

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
            resume_by_role,
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

    let next_action = match &outcome.status {
        crate::execute::PipelineStatus::Approved => format!(
            "Reviewer approved. The implementation is at `{}` (branch `{}`). Inspect the diff \
             yourself or trust the review. To merge, switch to your main branch in the repo and run \
             `git merge {}` (caucus deliberately does NOT auto-merge). Then sync Notion / kodex if \
             you do that.",
            outcome.worktree_path.display(),
            outcome.worktree_branch,
            outcome.worktree_branch
        ),
        crate::execute::PipelineStatus::NoReviewer => format!(
            "Implementation done without a review step. Read \
             `<session_root>/pipeline-{:02}/attempt-{:02}/implement/response.md` and decide: \
             merge `{}` directly, or re-run with `--review reviewer`.",
            outcome.pipeline_number, outcome.attempts, outcome.worktree_branch
        ),
        crate::execute::PipelineStatus::Blocked { attempts } => format!(
            "Reviewer flagged BLOCK after {attempts} attempt(s). Read the review at \
             `<session_root>/pipeline-{pn:02}/attempt-{att:02}/review/response.md`. Then choose: \
             (a) re-run pipeline with a bigger `--retry-on-block`, (b) edit decision.md and re-run, \
             (c) `caucus execute abandon` the worktree, or (d) escalate to a human.",
            attempts = attempts,
            pn = outcome.pipeline_number,
            att = outcome.attempts
        ),
        crate::execute::PipelineStatus::StepFailed { step } => format!(
            "Pipeline aborted at step {step:?}. Inspect that step's sentinel + response.md under \
             `<session_root>/pipeline-{:02}/attempt-{:02}/`. Decide whether to retry the pipeline \
             or abandon the worktree (`caucus execute abandon`).",
            outcome.pipeline_number, outcome.attempts
        ),
    };
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
            "next_action": next_action,
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
    Ok(outcome)
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
        let next_action = if terminal {
            "Session is terminal. Stop polling. If you have post-merge work \
             (Notion / kodex / git push), run it now once and exit the wakeup loop."
        } else {
            "Session still active. Continue the wakeup loop — check `caucus round status` \
             or `caucus execute status` per the most recent next_action you received."
        };
        emit(
            format,
            &serde_json::json!({
                "session_id": id.to_string(),
                "state": state,
                "terminal": terminal,
                "kind": kind,
                "next_action": next_action,
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
            RoundAction::Wait(args) => return round_wait(&repo, format, args).await,
        },
        Command::Execute(ExecuteArgs { action }) => match action {
            ExecuteAction::Start(args) => execute_start(&repo, format, args).await?,
            ExecuteAction::Pipeline(args) => {
                execute_pipeline(&repo, format, args).await?;
            }
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
        Command::Auto(args) => auto(&repo, format, args).await?,
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

/// `caucus auto`: end-to-end spine. v1 hardcodes the agenda, the decision
/// (= task text), and the pipeline shape; later versions replace each with
/// `claude --print` synthesis. The current value is the *spine* — it
/// proves caucus can drive a session from start to PR-ready without a
/// human in the loop, and it's the substrate every v2 decision-point edit
/// will plug into.
pub async fn auto(repo: &Path, format: OutputFormat, args: AutoArgs) -> Result<()> {
    let hook_path = repo.join(".caucus").join("bin").join("sentinel-stop");
    if !hook_path.exists() {
        return Err(anyhow!(
            "caucus is not initialised in {}. Run `caucus init --install-hook` \
             first, then re-run `caucus auto`. Without the Stop hook the meeting \
             agents produce no sentinels and `caucus auto` would block forever \
             on round_wait.",
            repo.display()
        ));
    }
    let roles_source = if args.roles.is_some() {
        "explicit"
    } else {
        "synthesized"
    };
    let roles = match args.roles.clone() {
        Some(rs) if !rs.is_empty() => rs,
        Some(_) => return Err(anyhow!("--roles passed but empty")),
        None => synthesize_roles(repo, &args.task, args.model.as_deref()).await?,
    };
    emit(
        format,
        &serde_json::json!({
            "auto_step": "roles_picked",
            "roles": roles,
            "source": roles_source,
        }),
        || format!("auto: roles ({roles_source}) = {roles:?}"),
    );

    let session_args = SessionNewArgs {
        topic: args.task.clone(),
        roles: roles.clone(),
        max_rounds: 5,
        model: args.model.clone(),
        require_permissions: false,
        layout: LayoutPreset::Auto,
        placement: args.placement,
    };
    let session = create_session_with_meeting_agents(repo, &session_args).await?;
    let session_id = session.id;
    let session_root = session.session_root.clone();
    emit(
        format,
        &serde_json::json!({
            "auto_step": "session_new",
            "session_id": session_id.to_string(),
            "roles": roles,
        }),
        || format!("auto: session {session_id} created"),
    );

    let agenda_source: &'static str;
    let agenda_body = match args.agenda_file.as_ref() {
        Some(path) => {
            agenda_source = "file";
            std::fs::read_to_string(path).map_err(|e| {
                anyhow!("auto: failed to read --agenda-file {}: {e}", path.display())
            })?
        }
        None => {
            agenda_source = "synthesized";
            synthesize_agenda(&args.task, &roles, args.model.as_deref()).await?
        }
    };
    let agenda_path = session_root.join("auto-agenda.md");
    std::fs::write(&agenda_path, &agenda_body)?;
    emit(
        format,
        &serde_json::json!({
            "auto_step": "agenda_composed",
            "agenda_path": agenda_path,
            "source": agenda_source,
            "bytes": agenda_body.len(),
        }),
        || {
            format!(
                "auto: agenda ({agenda_source}) at {}",
                agenda_path.display()
            )
        },
    );

    let lead = roles.iter().find(|r| r.as_str() == "architect").cloned();
    let round_args = RoundStartArgs {
        session_id: session_id.to_string(),
        agenda_file: agenda_path,
        lead,
        lead_timeout_secs: args.round_timeout_secs,
    };
    round_start(repo, format, round_args).await?;

    let wait_args = RoundWaitArgs {
        session_id: session_id.to_string(),
        round: None,
        timeout_secs: args.round_timeout_secs,
    };
    let rc = round_wait(repo, format, wait_args).await?;
    if rc != 0 {
        return Err(anyhow!(
            "auto: round_wait exit={rc} (1=timeout, 2=user_error, 3=session terminal). \
             Inspect session {session_id} and re-drive manually."
        ));
    }

    let decision_source: &'static str;
    let decision_path = match args.decision_file.as_ref() {
        Some(path) => {
            decision_source = "file";
            let dst = session_root.join("auto-decision.md");
            std::fs::copy(path, &dst).map_err(|e| {
                anyhow!(
                    "auto: failed to copy --decision-file {}: {e}",
                    path.display()
                )
            })?;
            dst
        }
        None => {
            decision_source = "synthesized";
            let responses = read_round_responses(&session_root, 1, &roles)?;
            let body =
                synthesize_decision(&args.task, &roles, &responses, args.model.as_deref()).await?;
            let dst = session_root.join("auto-decision.md");
            std::fs::write(&dst, body)?;
            dst
        }
    };
    emit(
        format,
        &serde_json::json!({
            "auto_step": "decision_composed",
            "decision_path": decision_path,
            "source": decision_source,
        }),
        || {
            format!(
                "auto: decision ({decision_source}) at {}",
                decision_path.display()
            )
        },
    );

    let converge_args = SessionConvergeArgs {
        session_id: session_id.to_string(),
        decision_file: decision_path,
    };
    session_converge(repo, format, converge_args)?;

    let (plan_role, impl_role, review_role) = pick_pipeline_roles(&roles)?;

    let retry_source: &'static str;
    let retry_on_block = match args.retry_on_block {
        Some(n) => {
            retry_source = "explicit";
            n
        }
        None => {
            retry_source = "synthesized";
            synthesize_retry_budget(&args.task, args.model.as_deref()).await?
        }
    };
    emit(
        format,
        &serde_json::json!({
            "auto_step": "retry_budget_picked",
            "retry_on_block": retry_on_block,
            "source": retry_source,
        }),
        || format!("auto: retry_on_block ({retry_source}) = {retry_on_block}"),
    );

    let pipeline_args = ExecutePipelineCliArgs {
        session_id: session_id.to_string(),
        task_file: session_root.join("decision.md"),
        plan: plan_role,
        implement: impl_role,
        review: review_role,
        retry_on_block,
        step_timeout_secs: args.step_timeout_secs,
        base_ref: args.base_ref,
        model: args.model,
        require_permissions: false,
        placement: args.placement,
        continue_meeting: true,
    };
    let pipeline_outcome = execute_pipeline(repo, format, pipeline_args).await?;

    // --merge-on-approve: opt-in. Only acts on `Approved`; everything else
    // (NoReviewer, Blocked, StepFailed) preserves the existing "human
    // decides" behaviour with a clear surface in the auto/complete payload.
    let merge_report = if args.merge_on_approve
        && matches!(
            pipeline_outcome.status,
            crate::execute::PipelineStatus::Approved
        ) {
        match merge_branch_into_head(repo, &pipeline_outcome.worktree_branch).await {
            Ok(into) => serde_json::json!({
                "attempted": true,
                "merged": true,
                "into_branch": into,
                "from_branch": pipeline_outcome.worktree_branch,
            }),
            Err(err) => serde_json::json!({
                "attempted": true,
                "merged": false,
                "from_branch": pipeline_outcome.worktree_branch,
                "error": err.to_string(),
            }),
        }
    } else {
        serde_json::json!({ "attempted": false })
    };

    emit(
        format,
        &serde_json::json!({
            "auto": "complete",
            "session_id": session_id.to_string(),
            "pipeline_status": pipeline_outcome.status,
            "worktree_branch": pipeline_outcome.worktree_branch,
            "merge": merge_report,
            "next_action":
                "auto run done. Read the final pipeline status above. If \
                 `approved` and you did NOT pass --merge-on-approve, merge the \
                 worktree branch yourself. If `blocked` or `step_failed`, open \
                 the session and either re-pipeline with more retry budget, \
                 escalate to a human, or `caucus execute abandon`.",
        }),
        || format!("auto run complete for session {session_id}"),
    );
    Ok(())
}

/// Run `git merge --no-ff --no-edit <branch>` in the repo. Returns the
/// name of the branch we merged INTO so callers can log it. Refuses to
/// merge from a detached HEAD; on conflict, returns an error and leaves
/// the merge state in place for the user to resolve / abort.
async fn merge_branch_into_head(repo: &Path, branch: &str) -> Result<String> {
    let repo_str = repo.to_string_lossy();
    let head_out = tokio::process::Command::new("git")
        .args(["-C", &repo_str, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .await
        .map_err(|e| anyhow!("auto: git rev-parse failed to launch: {e}"))?;
    if !head_out.status.success() {
        return Err(anyhow!(
            "auto: `git rev-parse --abbrev-ref HEAD` exited non-zero: {}",
            String::from_utf8_lossy(&head_out.stderr).trim()
        ));
    }
    let head = String::from_utf8_lossy(&head_out.stdout).trim().to_string();
    if head == "HEAD" || head.is_empty() {
        return Err(anyhow!(
            "auto --merge-on-approve: repo at {} is in detached HEAD; \
             refusing to merge. Check out a branch first.",
            repo.display()
        ));
    }
    let merge_out = tokio::process::Command::new("git")
        .args(["-C", &repo_str, "merge", "--no-ff", "--no-edit", branch])
        .output()
        .await
        .map_err(|e| anyhow!("auto: git merge failed to launch: {e}"))?;
    if !merge_out.status.success() {
        return Err(anyhow!(
            "auto --merge-on-approve: `git merge --no-ff --no-edit {branch}` \
             exited non-zero. stderr: {}\n\
             The merge state is left in place at {}. Run `git -C {} merge --abort` \
             to back out, or resolve conflicts and commit yourself.",
            String::from_utf8_lossy(&merge_out.stderr).trim(),
            repo.display(),
            repo.display()
        ));
    }
    Ok(head)
}

/// Shell out to `claude --print` once. Single owner of every auto-mode
/// synthesis call: builds the command, captures stdout, surfaces stderr on
/// non-zero exit. `purpose` is what we tell the user in error messages
/// (`role synthesis`, `agenda synthesis`, ...) so failures point at the
/// specific decision point that broke.
async fn claude_print(prompt: &str, model: Option<&str>, purpose: &str) -> Result<String> {
    let mut cmd = tokio::process::Command::new("claude");
    cmd.arg("--print").arg(prompt);
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }
    let output = cmd
        .output()
        .await
        .map_err(|e| anyhow!("auto: `claude --print` for {purpose} failed to launch: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "auto: `claude --print` for {purpose} exited non-zero ({}). stderr: {}",
            output.status,
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Synthesize a role roster from the task text. Strict: must pick from the
/// registry's actual names so downstream `session new` / `execute pipeline`
/// don't surprise us with an unknown role.
async fn synthesize_roles(repo: &Path, task: &str, model: Option<&str>) -> Result<Vec<String>> {
    let registry = build_registry(repo)?;
    let available: Vec<String> = registry.names().map(|s| s.to_string()).collect();
    if available.is_empty() {
        return Err(anyhow!(
            "auto: role registry is empty; no roles to synthesize from"
        ));
    }
    let prompt = format!(
        "You are a router for caucus, an agent orchestrator. Pick 1-5 roles \
         from this exact list for the task below.\n\n\
         Available roles:\n{available}\n\n\
         Always include at least one role that writes code (typically \
         `backend`) and at least one role that reviews (typically `reviewer`). \
         When the task is design-heavy or ambiguous, include `architect`. \
         Add `qa` only if the task explicitly involves test coverage.\n\n\
         Output ONLY a comma-separated list of role names from the list \
         above, no other text, no markdown.\n\n\
         Task:\n{task}",
        available = available
            .iter()
            .map(|n| format!("- {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
        task = task.trim()
    );
    let stdout = claude_print(&prompt, model, "role synthesis").await?;
    parse_role_list(&stdout, &available)
        .map_err(|e| anyhow!("{e}\nRe-run with `--roles <...>` to skip synthesis."))
}

/// Synthesize a round-1 agenda body from the task and the picked roles.
/// Unlike role synthesis, the body is free-form markdown — we only check
/// that it is non-empty.
async fn synthesize_agenda(task: &str, roles: &[String], model: Option<&str>) -> Result<String> {
    let prompt = format!(
        "You are composing a round-1 meeting agenda for caucus, an agent \
         orchestrator. Every role's agent will receive this agenda \
         simultaneously and respond from their role's angle.\n\n\
         Compose a SHORT (under 300 words) markdown agenda that:\n\
         1. States the task in one or two sentences.\n\
         2. Lists what each role should produce (one bullet per role).\n\
         3. Reminds every role to write their response to `response.md` \
            and stop.\n\n\
         Roles in the meeting: {roles}\n\n\
         Output ONLY the markdown agenda. No preamble, no explanation, no \
         code fences.\n\n\
         Task:\n{task}",
        roles = roles.join(", "),
        task = task.trim()
    );
    let stdout = claude_print(&prompt, model, "agenda synthesis").await?;
    let body = stdout.trim().to_string();
    if body.is_empty() {
        return Err(anyhow!(
            "auto: claude returned an empty agenda body. \
             Re-run with `--agenda-file <path>` to skip synthesis."
        ));
    }
    Ok(body)
}

/// Synthesize the meeting's decision from the round-1 responses. The output
/// becomes `decision.md` for `caucus session converge` and from there the
/// task that `execute pipeline` runs against. Errors when the response is
/// empty so silent fallback never sneaks in.
async fn synthesize_decision(
    task: &str,
    roles: &[String],
    responses: &[(String, String)],
    model: Option<&str>,
) -> Result<String> {
    let mut responses_block = String::new();
    for (role, body) in responses {
        responses_block.push_str(&format!("## {role}\n\n{}\n\n", body.trim()));
    }
    let prompt = format!(
        "You are the orchestrator for caucus. Compose `decision.md` for an \
         implementer to act on, based on the task and each role's round-1 \
         response below.\n\n\
         Requirements:\n\
         - Under 400 words.\n\
         - Markdown. Start with `# Decision`.\n\
         - State what to do concretely: file paths, behaviours, acceptance \
           criteria.\n\
         - When responses disagree, pick the option you find most defensible \
           and say so in one line; do NOT punt to the implementer.\n\
         - If round-1 responses are clearly insufficient, add one line at the \
           top: `Note: round-1 outputs are thin — implementer should escalate \
           if blocked.`\n\n\
         Output ONLY the markdown decision. No preamble, no code fences \
         wrapping the whole thing.\n\n\
         Roles in the meeting: {role_list}\n\n\
         Task:\n{task}\n\n\
         Round-1 responses:\n{responses_block}",
        role_list = roles.join(", "),
        task = task.trim()
    );
    let stdout = claude_print(&prompt, model, "decision synthesis").await?;
    let body = stdout.trim().to_string();
    if body.is_empty() {
        return Err(anyhow!(
            "auto: claude returned an empty decision. \
             Re-run with `--decision-file <path>` to skip synthesis."
        ));
    }
    Ok(body)
}

/// Read every role's response file for the given round. Returns
/// `(role, body)` pairs in the same order as `roles`. Missing files error
/// (round_wait should have already gated on non-empty responses; a missing
/// file at this point is a state-corruption bug, not a quiet skip).
/// Ask claude how many in-pipeline retries make sense for this task. The
/// pipeline's own `--retry-on-block N` triggers when the reviewer flags
/// BLOCK; each retry re-plans → re-implements with the review findings
/// folded in. We bound the synthesis to [0, 3] so a runaway estimate
/// can't burn through a worktree's worth of agent runs.
async fn synthesize_retry_budget(task: &str, model: Option<&str>) -> Result<u32> {
    let prompt = format!(
        "Predict how many implementation retry attempts make sense before \
         declaring a caucus task too hard. A retry happens when the reviewer \
         flags the implementer's code BLOCK; the next retry re-plans and \
         re-implements with the review findings folded in.\n\n\
         Output ONLY a single integer between 0 and 3, no other text.\n\
         - 0: trivial change (mechanical refactor, doc-only fix).\n\
         - 1: standard task (one bug fix, one feature, moderate complexity).\n\
         - 2: complex (multi-file feature, subtle invariants, design choices).\n\
         - 3: very complex (cross-crate change, performance work, race conditions).\n\n\
         Task:\n{task}",
        task = task.trim()
    );
    let stdout = claude_print(&prompt, model, "retry-budget synthesis").await?;
    parse_retry_budget(&stdout)
        .map_err(|e| anyhow!("{e}\nRe-run with `--retry-on-block <0..=3>` to skip synthesis."))
}

/// Extract the first integer in [0, 3] from the model's response. Tolerates
/// prose framing ("I think 2 attempts is right") and trailing newlines.
fn parse_retry_budget(stdout: &str) -> Result<u32> {
    let mut current = String::new();
    for c in stdout.chars() {
        if c.is_ascii_digit() {
            current.push(c);
        } else if !current.is_empty() {
            if let Ok(n) = current.parse::<u32>() {
                if n <= 3 {
                    return Ok(n);
                }
            }
            current.clear();
        }
    }
    if !current.is_empty() {
        if let Ok(n) = current.parse::<u32>() {
            if n <= 3 {
                return Ok(n);
            }
        }
    }
    Err(anyhow!(
        "auto: claude returned no integer in [0, 3]. Got: {stdout:?}"
    ))
}

fn read_round_responses(
    session_root: &Path,
    round: u32,
    roles: &[String],
) -> Result<Vec<(String, String)>> {
    let layout = crate::round::RoundLayout::new(session_root.to_path_buf(), round);
    let mut out = Vec::with_capacity(roles.len());
    for role in roles {
        let path = layout.response_path(role);
        let body = std::fs::read_to_string(&path).with_context(|| {
            format!("read round-{round} response for {role}: {}", path.display())
        })?;
        out.push((role.clone(), body));
    }
    Ok(out)
}

/// Robust parser for `claude --print` output. Tokenises on any
/// non-identifier character, lower-cases, and keeps tokens that match an
/// available role name (preserving order, deduped). Tolerates bullet
/// lists, prose framing, and trailing newlines that the model sometimes
/// emits despite "no other text" instructions.
fn parse_role_list(stdout: &str, available: &[String]) -> Result<Vec<String>> {
    let lowered: Vec<String> = available.iter().map(|a| a.to_lowercase()).collect();
    let s_lower = stdout.to_lowercase();
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for tok in s_lower.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-') {
        if tok.is_empty() {
            continue;
        }
        if let Some(i) = lowered.iter().position(|l| l == tok) {
            let canonical = available[i].clone();
            if seen.insert(canonical.clone()) {
                found.push(canonical);
            }
        }
    }
    if found.is_empty() {
        return Err(anyhow!(
            "auto: claude returned no recognisable role names. \
             Available: {available:?}. Got: {stdout:?}. \
             Re-run with `--roles <...>` to skip synthesis."
        ));
    }
    Ok(found)
}

/// Map the meeting roster onto the plan / implement / review slots that
/// `caucus execute pipeline` expects. v1 honours canonical names first and
/// falls back to positional assignment for non-canonical names. Plan and
/// review are optional; implement is required.
fn pick_pipeline_roles(roles: &[String]) -> Result<(Option<String>, String, Option<String>)> {
    let plan = roles.iter().find(|r| r.as_str() == "architect").cloned();
    let review = roles.iter().find(|r| r.as_str() == "reviewer").cloned();
    let impl_role = roles
        .iter()
        .find(|r| r.as_str() == "backend")
        .cloned()
        .or_else(|| {
            roles
                .iter()
                .find(|r| r.as_str() != "architect" && r.as_str() != "reviewer")
                .cloned()
        })
        .ok_or_else(|| {
            anyhow!(
                "--roles must include an implementer role (backend, or any \
                 non-architect/non-reviewer role). Got: {roles:?}"
            )
        })?;
    Ok((plan, impl_role, review))
}

#[cfg(test)]
mod auto_tests {
    use super::*;

    fn v(roles: &[&str]) -> Vec<String> {
        roles.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_three_roles_map_to_canonical_pipeline_slots() {
        let (plan, impl_, review) =
            pick_pipeline_roles(&v(&["architect", "backend", "reviewer"])).unwrap();
        assert_eq!(plan.as_deref(), Some("architect"));
        assert_eq!(impl_, "backend");
        assert_eq!(review.as_deref(), Some("reviewer"));
    }

    #[test]
    fn missing_architect_makes_plan_none() {
        let (plan, impl_, review) = pick_pipeline_roles(&v(&["backend", "reviewer"])).unwrap();
        assert!(plan.is_none());
        assert_eq!(impl_, "backend");
        assert_eq!(review.as_deref(), Some("reviewer"));
    }

    #[test]
    fn non_canonical_role_fills_implementer_slot_when_backend_absent() {
        let (plan, impl_, review) =
            pick_pipeline_roles(&v(&["architect", "scribe", "reviewer"])).unwrap();
        assert_eq!(plan.as_deref(), Some("architect"));
        assert_eq!(impl_, "scribe");
        assert_eq!(review.as_deref(), Some("reviewer"));
    }

    #[test]
    fn architect_only_errors_without_an_implementer() {
        let err = pick_pipeline_roles(&v(&["architect"])).unwrap_err();
        assert!(
            err.to_string().contains("implementer"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn reviewer_only_errors_without_an_implementer() {
        let err = pick_pipeline_roles(&v(&["reviewer"])).unwrap_err();
        assert!(
            err.to_string().contains("implementer"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_role_list_accepts_canonical_comma_form() {
        let avail = v(&["architect", "backend", "reviewer", "qa", "scribe"]);
        let got = parse_role_list("architect, backend, reviewer", &avail).unwrap();
        assert_eq!(got, v(&["architect", "backend", "reviewer"]));
    }

    #[test]
    fn parse_role_list_ignores_prose_around_bullet_list() {
        let avail = v(&["architect", "backend", "reviewer", "qa", "scribe"]);
        let raw = "Here are the picks:\n\n\
                   - architect\n- backend\n- reviewer\n\n\
                   Feel free to add more if needed.\n";
        let got = parse_role_list(raw, &avail).unwrap();
        assert_eq!(got, v(&["architect", "backend", "reviewer"]));
    }

    #[test]
    fn parse_role_list_dedupes_while_preserving_first_occurrence_order() {
        let avail = v(&["architect", "backend", "reviewer"]);
        let got = parse_role_list("backend, architect, backend, reviewer", &avail).unwrap();
        assert_eq!(got, v(&["backend", "architect", "reviewer"]));
    }

    #[test]
    fn parse_role_list_is_case_insensitive() {
        let avail = v(&["architect", "backend", "reviewer"]);
        let got = parse_role_list("Architect, BACKEND, Reviewer", &avail).unwrap();
        assert_eq!(got, v(&["architect", "backend", "reviewer"]));
    }

    #[test]
    fn parse_role_list_rejects_no_recognised_names() {
        let avail = v(&["architect", "backend", "reviewer"]);
        let err = parse_role_list("frontend, security, ops", &avail).unwrap_err();
        assert!(err.to_string().contains("no recognisable role names"));
    }

    #[test]
    fn parse_role_list_filters_out_unknown_alongside_known() {
        let avail = v(&["architect", "backend", "reviewer"]);
        // Unknown names are silently dropped; known names are kept.
        let got = parse_role_list("architect, security, backend, reviewer", &avail).unwrap();
        assert_eq!(got, v(&["architect", "backend", "reviewer"]));
    }

    #[test]
    fn read_round_responses_returns_each_roles_body_in_input_order() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let round_dir = tmp.path().join("round-01");
        fs::create_dir_all(&round_dir).unwrap();
        fs::write(round_dir.join("response-architect.md"), "plan: do X").unwrap();
        fs::write(
            round_dir.join("response-backend.md"),
            "i'll write the patch",
        )
        .unwrap();
        fs::write(
            round_dir.join("response-reviewer.md"),
            "watch for regressions in Y",
        )
        .unwrap();

        let roles = v(&["backend", "architect", "reviewer"]); // out of disk order
        let got = read_round_responses(tmp.path(), 1, &roles).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].0, "backend");
        assert_eq!(got[0].1, "i'll write the patch");
        assert_eq!(got[1].0, "architect");
        assert_eq!(got[1].1, "plan: do X");
        assert_eq!(got[2].0, "reviewer");
    }

    #[test]
    fn parse_retry_budget_bare_integer() {
        assert_eq!(parse_retry_budget("1").unwrap(), 1);
        assert_eq!(parse_retry_budget("0").unwrap(), 0);
        assert_eq!(parse_retry_budget("3").unwrap(), 3);
    }

    #[test]
    fn parse_retry_budget_handles_trailing_whitespace() {
        assert_eq!(parse_retry_budget("2\n").unwrap(), 2);
        assert_eq!(parse_retry_budget("  1  \n").unwrap(), 1);
    }

    #[test]
    fn parse_retry_budget_extracts_from_prose() {
        // Models occasionally hedge despite "ONLY an integer" instructions.
        assert_eq!(
            parse_retry_budget("I think 2 attempts is right").unwrap(),
            2
        );
        assert_eq!(parse_retry_budget("Estimated retries: 1").unwrap(), 1);
    }

    #[test]
    fn parse_retry_budget_skips_out_of_range_integers() {
        // 7 is out of range, then "2" is the first valid hit.
        assert_eq!(parse_retry_budget("7 is too many, try 2").unwrap(), 2);
    }

    #[test]
    fn parse_retry_budget_rejects_no_digits() {
        let err = parse_retry_budget("no clue").unwrap_err();
        assert!(err.to_string().contains("no integer in [0, 3]"));
    }

    #[test]
    fn parse_retry_budget_rejects_only_out_of_range_digits() {
        let err = parse_retry_budget("9, 8, 7").unwrap_err();
        assert!(err.to_string().contains("no integer in [0, 3]"));
    }

    #[test]
    fn read_round_responses_errors_when_a_role_response_is_missing() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let round_dir = tmp.path().join("round-01");
        fs::create_dir_all(&round_dir).unwrap();
        fs::write(round_dir.join("response-architect.md"), "plan").unwrap();
        // backend file deliberately missing

        let roles = v(&["architect", "backend"]);
        let err = read_round_responses(tmp.path(), 1, &roles).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("backend"), "unexpected error chain: {s}");
    }
}
