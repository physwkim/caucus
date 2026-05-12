//! Pipeline executor: plan → implement → review chain over a shared
//! worktree. The CEO calls `caucus execute pipeline` and caucus runs each
//! step, waiting for the previous role's sentinel before starting the next.
//! Optional `--retry-on-block N` loops plan→impl when the reviewer flags
//! the implementation as BLOCK.
//!
//! This is caucus's answer to claw-code's "OmO" layer (see
//! `docs/claw-code-analysis.md` and PHILOSOPHY.md §3): the agent CLI is
//! single-shot; the planning/execution/review/retry loop belongs in the
//! orchestrator above it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::Utc;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::agent::derive_state::DerivedState;
use crate::agent::manifest::{AgentKind, write_json};
use crate::agent::provenance::{LaneCommitProvenance, extract_commit_sha};
use crate::agent::spawn::{SpawnRequest, spawn};
use crate::role::registry::RoleRegistry;
use crate::role::spec::RoleSpec;
use crate::sentinel::{WatchEvent, WatcherError};
use crate::session::id::{AgentId, SessionId};
use crate::tmux::TmuxService;
use crate::worktree::manager::{
    WorktreeError, WorktreeRequest, create as create_worktree, current_branch,
};

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("pipeline io ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Worktree(#[from] WorktreeError),
    #[error(transparent)]
    Watcher(#[from] WatcherError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
    #[error("sentinel timeout after {seconds}s waiting for agent {agent_id}")]
    SentinelTimeout { agent_id: AgentId, seconds: u64 },
    #[error("watcher channel closed before sentinel arrived")]
    WatcherClosed,
}

/// One step of the pipeline.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Plan,
    Implement,
    Review,
}

impl StepKind {
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Implement => "implement",
            Self::Review => "review",
        }
    }
}

/// What we record per step in the pipeline output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepOutcome {
    pub kind: StepKind,
    pub role: String,
    pub agent_id: AgentId,
    pub response_path: PathBuf,
    pub response_excerpt: String,
    pub derived_state: DerivedState,
    pub sentinel_kind: String,
    pub commit_provenance: Option<LaneCommitProvenance>,
    /// tmux pane id of the spawned execute-phase agent. Used by the
    /// `--continue-meeting` retry path to kill the previous attempt's pane
    /// before re-resuming the same claude session id. `None` if the spawn
    /// completed without a recorded pane (defensive — shouldn't happen).
    #[serde(default)]
    pub tmux_pane_id: Option<String>,
}

/// Terminal status of a pipeline run.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStatus {
    /// All steps ran, review (if present) returned no BLOCK token.
    Approved,
    /// No reviewer in the pipeline — implementation completed, status
    /// surfaces that the CEO must read the impl response itself.
    NoReviewer,
    /// Review returned `Recommendation: BLOCK` after `attempts` rounds.
    Blocked { attempts: u32 },
    /// A step's sentinel said `failed` / `tool_blocked` / `error`.
    StepFailed { step: StepKind },
}

/// Inputs for [`run`]. Not `Debug` because the role-template resolver is a
/// trait object; the field exists so the CLI can pass its own resolution
/// logic (it knows about repo / install paths) without `pipeline.rs`
/// growing path-resolution rules.
pub struct PipelineRequest<'a> {
    pub session_id: SessionId,
    pub repo_root: PathBuf,
    pub session_root: PathBuf,
    pub registry: &'a RoleRegistry,
    pub role_template_resolver: &'a dyn Fn(&Path) -> PathBuf,

    pub plan_role: Option<&'a str>,
    pub implement_role: &'a str,
    pub review_role: Option<&'a str>,

    pub task_source: PathBuf,
    pub model: Option<String>,
    pub base_ref: Option<String>,
    pub sentinel_hook_path: Option<PathBuf>,
    pub skip_permissions: bool,
    pub retry_on_block: u32,
    pub step_timeout: Duration,
    pub placement: crate::tmux::Placement,
    /// Per-role claude `--resume` session ids. When a step's role appears in
    /// this map, the step's spawn passes `resume_session_id = Some(...)`
    /// instead of starting a fresh `claude` process. Populated by the CLI
    /// from each role's meeting-agent manifest when `--continue-meeting`
    /// is set. Empty map = fresh-context behaviour (unchanged).
    pub resume_by_role: std::collections::BTreeMap<String, String>,
}

/// Final result of a pipeline run.
#[derive(Debug, Clone, Serialize)]
pub struct PipelineOutcome {
    pub pipeline_number: u32,
    pub worktree_path: PathBuf,
    pub worktree_branch: String,
    pub plan: Option<StepOutcome>,
    pub implement: Option<StepOutcome>,
    pub review: Option<StepOutcome>,
    pub status: PipelineStatus,
    pub attempts: u32,
}

static BLOCK_PATTERN: Lazy<Regex> = Lazy::new(|| {
    // Matches `Recommendation: BLOCK`, `recommendation: block`, etc. Comes
    // straight from `roles/reviewer.md` which already standardises this
    // line.
    Regex::new(r"(?im)^recommendation\s*:\s*block\b").unwrap()
});

/// Detect a BLOCK verdict in a reviewer response. Public for unit tests.
pub fn response_is_blocked(text: &str) -> bool {
    BLOCK_PATTERN.is_match(text)
}

/// Find the next unused pipeline number under `<session_root>/pipeline-NN/`.
/// 1-indexed; caps at 99 (well past any realistic retry count).
pub fn allocate_pipeline_number(session_root: &Path) -> Result<u32, PipelineError> {
    for n in 1u32..=99 {
        let dir = session_root.join(format!("pipeline-{n:02}"));
        if !dir.exists() {
            return Ok(n);
        }
    }
    Err(PipelineError::Other(anyhow!(
        "more than 99 pipelines under {} — clean up before retrying",
        session_root.display()
    )))
}

/// Run a pipeline. The orchestrator is fully sequential: each step is
/// spawned, its sentinel awaited, its response captured, and the result
/// fed into the next step.
pub async fn run(
    tmux: &TmuxService,
    req: PipelineRequest<'_>,
) -> Result<PipelineOutcome, PipelineError> {
    let pipeline_number = allocate_pipeline_number(&req.session_root)?;
    let pipeline_dir = req
        .session_root
        .join(format!("pipeline-{pipeline_number:02}"));
    std::fs::create_dir_all(&pipeline_dir).map_err(|source| PipelineError::Io {
        path: pipeline_dir.clone(),
        source,
    })?;

    // Shared worktree for every step. Implementer writes code here; planner
    // and reviewer read it.
    let worktree = create_worktree(&WorktreeRequest {
        repo_root: req.repo_root.clone(),
        session_id: req.session_id,
        role: req.implement_role.to_string(),
        branch: None,
        base_ref: req.base_ref.clone(),
        name_override: Some(format!("{}-pipeline-{pipeline_number:02}", req.session_id)),
    })
    .await?;

    // Start the sentinel watcher once for the whole pipeline.
    let agents_dir = req.session_root.join("agents");
    std::fs::create_dir_all(&agents_dir).map_err(|source| PipelineError::Io {
        path: agents_dir.clone(),
        source,
    })?;
    let (watcher, mut rx) = crate::sentinel::watch(&agents_dir)?;
    let _watcher_guard = watcher;

    let plan_spec = req
        .plan_role
        .map(|name| resolve_role(req.registry, name))
        .transpose()?;
    let impl_spec = resolve_role(req.registry, req.implement_role)?;
    let review_spec = req
        .review_role
        .map(|name| resolve_role(req.registry, name))
        .transpose()?;

    let mut attempts = 0u32;
    let mut current_task: PathBuf = req.task_source.clone();
    let mut last_review: Option<StepOutcome> = None;
    let mut last_implement: Option<StepOutcome> = None;
    // Reassigned each iteration; carry-forward isn't part of the contract,
    // but the variable's existence keeps the outcome assembly readable.
    let mut last_plan: Option<StepOutcome>;
    // Pane ids spawned by the previous attempt — killed at the top of the
    // next attempt under `--continue-meeting` so the same claude session
    // id can be re-resumed in a fresh pane. Empty without resume mode.
    let mut previous_attempt_panes: Vec<String> = Vec::new();

    loop {
        attempts += 1;
        if attempts > 1 && !req.resume_by_role.is_empty() {
            for pane in previous_attempt_panes.drain(..) {
                let _ = tmux.kill_pane(&pane).await;
            }
        }
        let mut current_attempt_panes: Vec<String> = Vec::new();

        // ---- Plan step (optional) -----------------------------------
        let plan_outcome = if let Some(role) = plan_spec.as_ref() {
            let template = (req.role_template_resolver)(&role.system_prompt_template);
            let outcome = run_step(
                tmux,
                &mut rx,
                StepArgs {
                    kind: StepKind::Plan,
                    session_id: req.session_id,
                    session_root: &req.session_root,
                    pipeline_dir: &pipeline_dir,
                    role,
                    role_template: template,
                    cwd: &worktree.path,
                    task_source: &current_task,
                    model: req.model.clone(),
                    skip_permissions: req.skip_permissions,
                    sentinel_hook: req.sentinel_hook_path.clone(),
                    attempt: attempts,
                    pipeline_number,
                    placement: req.placement,
                    resume_session_id: req.resume_by_role.get(&role.name).cloned(),
                },
                req.step_timeout,
            )
            .await?;
            if let Some(pane) = outcome.tmux_pane_id.as_ref() {
                current_attempt_panes.push(pane.clone());
            }
            if outcome.sentinel_kind != "stop" {
                return Ok(PipelineOutcome {
                    pipeline_number,
                    worktree_path: worktree.path.clone(),
                    worktree_branch: worktree.branch.clone(),
                    plan: Some(outcome),
                    implement: last_implement,
                    review: last_review,
                    status: PipelineStatus::StepFailed {
                        step: StepKind::Plan,
                    },
                    attempts,
                });
            }
            current_task = outcome.response_path.clone();
            Some(outcome)
        } else {
            None
        };
        last_plan = plan_outcome.clone();

        // ---- Implement step (required) ------------------------------
        let template = (req.role_template_resolver)(&impl_spec.system_prompt_template);
        let implement_outcome = run_step(
            tmux,
            &mut rx,
            StepArgs {
                kind: StepKind::Implement,
                session_id: req.session_id,
                session_root: &req.session_root,
                pipeline_dir: &pipeline_dir,
                role: impl_spec,
                role_template: template,
                cwd: &worktree.path,
                task_source: &current_task,
                model: req.model.clone(),
                skip_permissions: req.skip_permissions,
                sentinel_hook: req.sentinel_hook_path.clone(),
                attempt: attempts,
                pipeline_number,
                placement: req.placement,
                resume_session_id: req.resume_by_role.get(&impl_spec.name).cloned(),
            },
            req.step_timeout,
        )
        .await?;
        if let Some(pane) = implement_outcome.tmux_pane_id.as_ref() {
            current_attempt_panes.push(pane.clone());
        }
        last_implement = Some(implement_outcome.clone());
        if implement_outcome.sentinel_kind != "stop" {
            return Ok(PipelineOutcome {
                pipeline_number,
                worktree_path: worktree.path.clone(),
                worktree_branch: worktree.branch.clone(),
                plan: last_plan,
                implement: last_implement,
                review: last_review,
                status: PipelineStatus::StepFailed {
                    step: StepKind::Implement,
                },
                attempts,
            });
        }

        // ---- Review step (optional) ---------------------------------
        let review_outcome = if let Some(role) = review_spec.as_ref() {
            let template = (req.role_template_resolver)(&role.system_prompt_template);
            // Compose review task: point reviewer at the worktree and the
            // implementer's response.
            let review_task = compose_review_brief(
                &pipeline_dir,
                attempts,
                &worktree.path,
                &implement_outcome.response_path,
            )?;
            let outcome = run_step(
                tmux,
                &mut rx,
                StepArgs {
                    kind: StepKind::Review,
                    session_id: req.session_id,
                    session_root: &req.session_root,
                    pipeline_dir: &pipeline_dir,
                    role,
                    role_template: template,
                    cwd: &worktree.path,
                    task_source: &review_task,
                    model: req.model.clone(),
                    skip_permissions: req.skip_permissions,
                    sentinel_hook: req.sentinel_hook_path.clone(),
                    attempt: attempts,
                    pipeline_number,
                    placement: req.placement,
                    resume_session_id: req.resume_by_role.get(&role.name).cloned(),
                },
                req.step_timeout,
            )
            .await?;
            if let Some(pane) = outcome.tmux_pane_id.as_ref() {
                current_attempt_panes.push(pane.clone());
            }
            Some(outcome)
        } else {
            None
        };
        last_review = review_outcome.clone();
        previous_attempt_panes = current_attempt_panes;

        // ---- Decide ----------------------------------------------------
        let Some(review) = review_outcome.as_ref() else {
            return Ok(PipelineOutcome {
                pipeline_number,
                worktree_path: worktree.path.clone(),
                worktree_branch: worktree.branch.clone(),
                plan: last_plan,
                implement: last_implement,
                review: None,
                status: PipelineStatus::NoReviewer,
                attempts,
            });
        };
        if review.sentinel_kind != "stop" {
            return Ok(PipelineOutcome {
                pipeline_number,
                worktree_path: worktree.path.clone(),
                worktree_branch: worktree.branch.clone(),
                plan: last_plan,
                implement: last_implement,
                review: last_review,
                status: PipelineStatus::StepFailed {
                    step: StepKind::Review,
                },
                attempts,
            });
        }

        let review_body = std::fs::read_to_string(&review.response_path).unwrap_or_default();
        if !response_is_blocked(&review_body) {
            return Ok(PipelineOutcome {
                pipeline_number,
                worktree_path: worktree.path.clone(),
                worktree_branch: worktree.branch.clone(),
                plan: last_plan,
                implement: last_implement,
                review: last_review,
                status: PipelineStatus::Approved,
                attempts,
            });
        }

        // BLOCK: retry if budget remains. The next iteration uses the
        // review findings as the task — the planner (or implementer if no
        // planner) reads them and revises.
        if attempts > req.retry_on_block {
            return Ok(PipelineOutcome {
                pipeline_number,
                worktree_path: worktree.path.clone(),
                worktree_branch: worktree.branch.clone(),
                plan: last_plan,
                implement: last_implement,
                review: last_review,
                status: PipelineStatus::Blocked { attempts },
                attempts,
            });
        }
        let retry_task = compose_retry_brief(
            &pipeline_dir,
            attempts,
            &req.task_source,
            &review.response_path,
        )?;
        current_task = retry_task;
    }
}

struct StepArgs<'a> {
    kind: StepKind,
    session_id: SessionId,
    session_root: &'a Path,
    pipeline_dir: &'a Path,
    role: &'a RoleSpec,
    role_template: PathBuf,
    cwd: &'a Path,
    task_source: &'a Path,
    model: Option<String>,
    skip_permissions: bool,
    sentinel_hook: Option<PathBuf>,
    attempt: u32,
    pipeline_number: u32,
    placement: crate::tmux::Placement,
    /// When `Some`, the step's spawn passes `claude --resume <id>` instead
    /// of a fresh process. Populated from `PipelineRequest::resume_by_role`
    /// for the step's role.
    resume_session_id: Option<String>,
}

async fn run_step(
    tmux: &TmuxService,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<WatchEvent>,
    args: StepArgs<'_>,
    timeout: Duration,
) -> Result<StepOutcome, PipelineError> {
    let step_dir = args
        .pipeline_dir
        .join(format!("attempt-{:02}", args.attempt))
        .join(args.kind.dir_name());
    std::fs::create_dir_all(&step_dir).map_err(|source| PipelineError::Io {
        path: step_dir.clone(),
        source,
    })?;
    let task_dst = step_dir.join("task.md");
    std::fs::copy(args.task_source, &task_dst).map_err(|source| PipelineError::Io {
        path: task_dst.clone(),
        source,
    })?;
    let sys_dst = step_dir.join("system.md");
    std::fs::copy(&args.role_template, &sys_dst).map_err(|source| PipelineError::Io {
        path: sys_dst.clone(),
        source,
    })?;
    let response_path = step_dir.join("response.md");
    if !response_path.exists() {
        std::fs::write(&response_path, "").map_err(|source| PipelineError::Io {
            path: response_path.clone(),
            source,
        })?;
    }

    let outcome = spawn(
        tmux,
        SpawnRequest {
            session_id: args.session_id,
            session_root: args.session_root.to_path_buf(),
            role: args.role,
            kind: AgentKind::Execute,
            cwd: args.cwd.to_path_buf(),
            system_prompt_path: sys_dst,
            response_path: response_path.clone(),
            sentinel_hook_path: args.sentinel_hook,
            model: args.model,
            title: Some(format!(
                "p{:02}-a{:02}-{}-{}",
                args.pipeline_number,
                args.attempt,
                args.kind.dir_name(),
                args.role.name
            )),
            initial_prompt_path: Some(task_dst),
            skip_permissions: args.skip_permissions,
            placement: args.placement,
            resume_session_id: args.resume_session_id.clone(),
        },
    )
    .await
    .map_err(|e| PipelineError::Other(anyhow!("spawn {:?}: {e}", args.kind)))?;

    let agent_id = outcome.manifest.agent_id;
    let tmux_pane_id = outcome.manifest.tmux_pane_id.clone();
    let sentinel = await_sentinel_for(rx, agent_id, timeout).await?;
    let manifest = crate::round::record_sentinel(args.session_root, &sentinel)
        .map_err(|e| PipelineError::Other(anyhow!("record sentinel: {e}")))?;

    let response_text = std::fs::read_to_string(&response_path).unwrap_or_default();
    let excerpt = excerpt(&response_text, 800);

    let commit_provenance = if matches!(args.kind, StepKind::Implement) {
        match extract_commit_sha(&response_text) {
            Some(commit) => {
                let branch = current_branch(args.cwd).await.ok();
                Some(LaneCommitProvenance {
                    commit: commit.clone(),
                    branch: branch.unwrap_or_else(|| "unknown".into()),
                    worktree: Some(args.cwd.to_path_buf()),
                    canonical_commit: Some(commit.clone()),
                    superseded_by: None,
                    lineage: vec![commit],
                })
            }
            None => None,
        }
    } else {
        None
    };

    if let Some(ref prov) = commit_provenance {
        let mut m = manifest.clone();
        m.lane_events
            .push(crate::agent::lane_event::LaneEvent::CommitCreated {
                ts: Utc::now(),
                provenance: prov.clone(),
            });
        let _ = write_json(&m, args.session_root);
    }

    Ok(StepOutcome {
        kind: args.kind,
        role: args.role.name.clone(),
        agent_id,
        response_path,
        response_excerpt: excerpt,
        derived_state: manifest.derived_state,
        sentinel_kind: format!("{:?}", sentinel.kind).to_lowercase(),
        commit_provenance,
        tmux_pane_id,
    })
}

async fn await_sentinel_for(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<WatchEvent>,
    agent_id: AgentId,
    timeout: Duration,
) -> Result<crate::sentinel::Sentinel, PipelineError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(PipelineError::SentinelTimeout {
                agent_id,
                seconds: timeout.as_secs(),
            });
        }
        let event = tokio::time::timeout(remaining, rx.recv())
            .await
            .map_err(|_| PipelineError::SentinelTimeout {
                agent_id,
                seconds: timeout.as_secs(),
            })?;
        match event {
            Some(WatchEvent::Sentinel { sentinel, .. }) if sentinel.agent_id == agent_id => {
                return Ok(sentinel);
            }
            // Sentinels for other agents (e.g. the meeting agents that
            // outlasted us) are ignored — not our concern in this loop.
            Some(_) => continue,
            None => return Err(PipelineError::WatcherClosed),
        }
    }
}

fn excerpt(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn compose_review_brief(
    pipeline_dir: &Path,
    attempt: u32,
    worktree_path: &Path,
    impl_response_path: &Path,
) -> Result<PathBuf, PipelineError> {
    let body = format!(
        "# Review brief\n\n\
         The implementer just finished. Review their work and write your \
         findings to your `response.md` (the orchestrator will read it).\n\n\
         - Worktree: `{worktree}`\n\
         - Implementer's response: `{impl_response}`\n\n\
         Inspect with `git -C {worktree} diff HEAD~..HEAD` (or similar). End \
         your response with `Recommendation: APPROVE` or `Recommendation: \
         BLOCK` on its own line — `BLOCK` triggers another pipeline pass if \
         `--retry-on-block` is set.\n",
        worktree = worktree_path.display(),
        impl_response = impl_response_path.display(),
    );
    let path = pipeline_dir
        .join(format!("attempt-{attempt:02}"))
        .join("review-brief.md");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| PipelineError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&path, body).map_err(|source| PipelineError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

fn compose_retry_brief(
    pipeline_dir: &Path,
    attempt: u32,
    original_task: &Path,
    review_findings: &Path,
) -> Result<PathBuf, PipelineError> {
    let original = std::fs::read_to_string(original_task).unwrap_or_default();
    let review = std::fs::read_to_string(review_findings).unwrap_or_default();
    let body = format!(
        "# Retry brief (attempt {next})\n\n\
         The previous pass was BLOCKed by review. Fold the review's findings \
         into a revised plan, then re-implement.\n\n\
         ## Original task\n\n{original}\n\n\
         ## Review findings (attempt {attempt})\n\n{review}\n",
        next = attempt + 1,
    );
    let path = pipeline_dir.join(format!("retry-{attempt:02}.md"));
    std::fs::write(&path, body).map_err(|source| PipelineError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

fn resolve_role<'a>(registry: &'a RoleRegistry, name: &str) -> Result<&'a RoleSpec, PipelineError> {
    registry
        .get(name)
        .map_err(|err| PipelineError::Other(anyhow!("{err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn block_regex_matches_canonical_recommendation() {
        assert!(response_is_blocked("Recommendation: BLOCK"));
        assert!(response_is_blocked("Findings ok.\n\nRecommendation: block"));
        assert!(response_is_blocked("recommendation : BLOCK now"));
    }

    #[test]
    fn block_regex_ignores_approval_and_inline_mentions() {
        assert!(!response_is_blocked("Recommendation: APPROVE"));
        assert!(!response_is_blocked("This change wouldn't block anything."));
        assert!(!response_is_blocked(
            "Recommendation: request_changes\n(could be a block in spirit but not literal)"
        ));
    }

    #[test]
    fn allocate_returns_one_for_empty_session_root() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(allocate_pipeline_number(tmp.path()).unwrap(), 1);
    }

    #[test]
    fn allocate_skips_existing_pipelines() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("pipeline-01")).unwrap();
        std::fs::create_dir_all(tmp.path().join("pipeline-02")).unwrap();
        assert_eq!(allocate_pipeline_number(tmp.path()).unwrap(), 3);
    }

    #[test]
    fn compose_retry_brief_concatenates_original_and_findings() {
        let tmp = TempDir::new().unwrap();
        let task = tmp.path().join("task.md");
        std::fs::write(&task, "implement X").unwrap();
        let review = tmp.path().join("review.md");
        std::fs::write(&review, "Missing tests.\nRecommendation: BLOCK").unwrap();
        let out = compose_retry_brief(tmp.path(), 1, &task, &review).unwrap();
        let body = std::fs::read_to_string(out).unwrap();
        assert!(body.contains("implement X"));
        assert!(body.contains("Missing tests"));
        assert!(body.contains("attempt 2"));
    }

    #[test]
    fn compose_review_brief_references_worktree_and_impl() {
        let tmp = TempDir::new().unwrap();
        let impl_response = tmp.path().join("impl.md");
        std::fs::write(&impl_response, "I committed deadbeef").unwrap();
        let out = compose_review_brief(tmp.path(), 1, Path::new("/wt"), &impl_response).unwrap();
        let body = std::fs::read_to_string(out).unwrap();
        assert!(body.contains("/wt"));
        assert!(body.contains(impl_response.display().to_string().as_str()));
        assert!(body.contains("Recommendation: APPROVE"));
    }
}
