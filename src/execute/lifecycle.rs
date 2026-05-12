//! Execute-phase lifecycle. After meeting consensus, the CEO calls
//! `caucus execute start --session ID --role backend --task-file PATH`.
//! caucus then:
//!
//! 1. Creates a fresh worktree at `<repo>/.caucus/worktrees/<session>-<role>/`
//!    via `crate::worktree::manager::create`.
//! 2. Renders the role's system prompt at
//!    `<session_root>/execute/<role>/system.md` and copies the task into
//!    `<session_root>/execute/<role>/task.md`.
//! 3. Spawns an Execute-kind agent via `crate::agent::spawn::spawn`, cwd =
//!    the worktree, with the task path passed as the initial bootstrap
//!    prompt.
//! 4. On finish, records `commit_provenance` extracted from the agent's
//!    final message and enqueues worktree cleanup.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::sync::oneshot;

use crate::agent::manifest::{AgentKind, AgentManifest, ManifestError, read_json, write_json};
use crate::agent::provenance::{LaneCommitProvenance, extract_commit_sha};
use crate::agent::spawn::{SpawnError, SpawnRequest, spawn};
use crate::role::spec::RoleSpec;
use crate::session::id::{AgentId, SessionId};
use crate::tmux::TmuxService;
use crate::worktree::cleanup::{CleanupJob, CleanupQueue, CleanupSummary};
use crate::worktree::manager::{
    WorktreeError, WorktreeHandle, WorktreeRequest, create as create_worktree, current_branch,
};

#[derive(Debug, Error)]
pub enum ExecuteError {
    #[error("execute io ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Worktree(#[from] WorktreeError),
    #[error(transparent)]
    Spawn(#[from] SpawnError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("cleanup queue closed")]
    CleanupQueueClosed,
}

/// Inputs for `caucus execute start`.
#[derive(Debug, Clone)]
pub struct ExecuteStartRequest<'a> {
    pub session_id: SessionId,
    pub repo_root: PathBuf,
    pub session_root: PathBuf,
    pub role: &'a RoleSpec,
    pub role_template_path: PathBuf,
    /// Markdown file describing the task. Copied into the execute dir.
    pub task_source: PathBuf,
    /// Optional model + title overrides.
    pub model: Option<String>,
    pub title: Option<String>,
    /// Optional override for the worktree base ref.
    pub base_ref: Option<String>,
    /// Optional sentinel hook path forwarded into spawn().
    pub sentinel_hook_path: Option<PathBuf>,
    /// Pass `--dangerously-skip-permissions` to the spawned agent CLI. Default
    /// `true` from the CLI (opt out via `--require-permissions`).
    pub skip_permissions: bool,
    /// If Some, spawn the agent as `claude --resume <session_id>` so the
    /// execute-phase pane inherits the meeting-phase conversation.
    pub resume_session_id: Option<String>,
}

/// Files materialised under `<session_root>/execute/<role>/`.
#[derive(Debug, Clone)]
pub struct ExecuteLayout {
    pub session_root: PathBuf,
    pub role: String,
}

impl ExecuteLayout {
    pub fn dir(&self) -> PathBuf {
        self.session_root.join("execute").join(&self.role)
    }
    pub fn system_prompt_path(&self) -> PathBuf {
        self.dir().join("system.md")
    }
    pub fn task_path(&self) -> PathBuf {
        self.dir().join("task.md")
    }
    pub fn response_path(&self) -> PathBuf {
        self.dir().join("response.md")
    }
}

/// Outcome of `start()`.
#[derive(Debug, Clone)]
pub struct ExecuteStartOutcome {
    pub agent: AgentManifest,
    pub worktree: WorktreeHandle,
}

/// Start an execute-phase agent in its own worktree.
pub async fn start(
    tmux: &TmuxService,
    req: ExecuteStartRequest<'_>,
) -> Result<ExecuteStartOutcome, ExecuteError> {
    let layout = ExecuteLayout {
        session_root: req.session_root.clone(),
        role: req.role.name.clone(),
    };
    let dir = layout.dir();
    std::fs::create_dir_all(&dir).map_err(|source| ExecuteError::Io {
        path: dir.clone(),
        source,
    })?;
    std::fs::copy(&req.task_source, layout.task_path()).map_err(|source| ExecuteError::Io {
        path: layout.task_path(),
        source,
    })?;
    let prompt_body =
        std::fs::read_to_string(&req.role_template_path).map_err(|source| ExecuteError::Io {
            path: req.role_template_path.clone(),
            source,
        })?;
    std::fs::write(layout.system_prompt_path(), prompt_body).map_err(|source| {
        ExecuteError::Io {
            path: layout.system_prompt_path(),
            source,
        }
    })?;

    let worktree = create_worktree(&WorktreeRequest {
        repo_root: req.repo_root.clone(),
        session_id: req.session_id,
        role: req.role.name.clone(),
        branch: None,
        base_ref: req.base_ref.clone(),
    })
    .await?;

    let outcome = spawn(
        tmux,
        SpawnRequest {
            session_id: req.session_id,
            session_root: req.session_root.clone(),
            role: req.role,
            kind: AgentKind::Execute,
            cwd: worktree.path.clone(),
            system_prompt_path: layout.system_prompt_path(),
            response_path: layout.response_path(),
            sentinel_hook_path: req.sentinel_hook_path,
            model: req.model,
            title: req.title,
            initial_prompt_path: Some(layout.task_path()),
            skip_permissions: req.skip_permissions,
            resume_session_id: req.resume_session_id,
        },
    )
    .await?;

    Ok(ExecuteStartOutcome {
        agent: outcome.manifest,
        worktree,
    })
}

/// Finalise an execute agent: read its response, extract commit provenance,
/// enqueue cleanup. The session-level orchestrator decides whether to merge
/// the worktree's branch; that happens *outside* caucus (the CEO calls `git
/// merge` itself).
pub async fn finish(
    tmux: &TmuxService,
    queue: &CleanupQueue,
    session_root: &Path,
    agent_id: AgentId,
) -> Result<FinishOutcome, ExecuteError> {
    let mut manifest = read_json(session_root, agent_id)?;

    let layout = ExecuteLayout {
        session_root: session_root.to_path_buf(),
        role: manifest.role.clone(),
    };
    let response_text = std::fs::read_to_string(layout.response_path()).unwrap_or_default();

    let provenance = if let Some(commit) = extract_commit_sha(&response_text) {
        let worktree = manifest
            .worktree_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));
        let branch = current_branch(&worktree)
            .await
            .unwrap_or_else(|_| "unknown".into());
        Some(LaneCommitProvenance {
            commit: commit.clone(),
            branch,
            worktree: manifest.worktree_path.clone(),
            canonical_commit: Some(commit.clone()),
            superseded_by: None,
            lineage: vec![commit],
        })
    } else {
        None
    };

    if let Some(ref p) = provenance {
        manifest
            .lane_events
            .push(crate::agent::lane_event::LaneEvent::CommitCreated {
                ts: chrono::Utc::now(),
                provenance: p.clone(),
            });
    }
    if let Some(pane) = manifest.tmux_pane_id.clone() {
        // Best-effort kill; missing pane is fine.
        let _ = tmux.kill_pane(&pane).await;
    }
    write_json(&manifest, session_root)?;

    let (tx, rx) = oneshot::channel();
    let job = CleanupJob {
        repo_root: manifest
            .worktree_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(".")),
        worktree_paths: manifest.worktree_path.iter().cloned().collect(),
        branches_to_delete: vec![],
        done: Some(tx),
    };
    // Wedge: cleanup needs the *repo root*, not the worktree path. Fix:
    // the CleanupJob.repo_root must be the original repo, recoverable from
    // the manifest. We persist worktree_path under the repo's .caucus dir,
    // so the repo root is `worktree_path → ../../../..` (worktrees/SESSION-ROLE → .caucus/worktrees → .caucus → repo).
    let mut job = job;
    if let Some(wt) = manifest.worktree_path.clone() {
        if let Some(repo_root) = wt.ancestors().nth(3).map(|p| p.to_path_buf()) {
            job.repo_root = repo_root;
        }
    }
    queue
        .enqueue(job)
        .map_err(|_| ExecuteError::CleanupQueueClosed)?;
    let summary = rx.await.unwrap_or_default();

    Ok(FinishOutcome {
        manifest,
        provenance,
        cleanup: summary,
    })
}

/// Abandon an execute agent: kill its pane, queue worktree cleanup, mark
/// the manifest as failed. Does not capture commit_provenance.
pub async fn abandon(
    tmux: &TmuxService,
    queue: &CleanupQueue,
    session_root: &Path,
    agent_id: AgentId,
) -> Result<AbandonOutcome, ExecuteError> {
    let mut manifest = read_json(session_root, agent_id)?;
    if let Some(pane) = manifest.tmux_pane_id.clone() {
        let _ = tmux.kill_pane(&pane).await;
    }
    manifest.status = crate::agent::derive_state::RawStatus::Failed;
    manifest.completed_at = Some(chrono::Utc::now());
    manifest.derived_state = crate::agent::derive_state::DerivedState::TrulyIdle;
    manifest.error = Some("abandoned by orchestrator".into());
    manifest
        .lane_events
        .push(crate::agent::lane_event::LaneEvent::Failed {
            ts: chrono::Utc::now(),
            blocker: crate::agent::lane_event::LaneEventBlocker {
                failure_class: crate::agent::lane_event::LaneFailureClass::Unknown,
                detail: "abandoned by orchestrator".into(),
            },
        });
    write_json(&manifest, session_root)?;

    let (tx, rx) = oneshot::channel();
    let mut job = CleanupJob {
        repo_root: PathBuf::from("."),
        worktree_paths: manifest.worktree_path.iter().cloned().collect(),
        branches_to_delete: vec![],
        done: Some(tx),
    };
    if let Some(wt) = manifest.worktree_path.clone() {
        if let Some(repo_root) = wt.ancestors().nth(3).map(|p| p.to_path_buf()) {
            job.repo_root = repo_root;
        }
    }
    queue
        .enqueue(job)
        .map_err(|_| ExecuteError::CleanupQueueClosed)?;
    let summary = rx.await.unwrap_or_default();
    Ok(AbandonOutcome {
        manifest,
        cleanup: summary,
    })
}

#[derive(Debug)]
pub struct FinishOutcome {
    pub manifest: AgentManifest,
    pub provenance: Option<LaneCommitProvenance>,
    pub cleanup: CleanupSummary,
}

#[derive(Debug)]
pub struct AbandonOutcome {
    pub manifest: AgentManifest,
    pub cleanup: CleanupSummary,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_layout_paths() {
        let layout = ExecuteLayout {
            session_root: PathBuf::from("/repo/.caucus/sessions/S"),
            role: "backend".into(),
        };
        assert_eq!(
            layout.dir(),
            Path::new("/repo/.caucus/sessions/S/execute/backend")
        );
        assert_eq!(
            layout.system_prompt_path(),
            Path::new("/repo/.caucus/sessions/S/execute/backend/system.md")
        );
        assert_eq!(
            layout.response_path(),
            Path::new("/repo/.caucus/sessions/S/execute/backend/response.md")
        );
    }

    #[test]
    fn worktree_path_to_repo_root() {
        // worktree at /repo/.caucus/worktrees/<id>-role
        let wt = PathBuf::from("/repo/.caucus/worktrees/01HXX-backend");
        let repo = wt.ancestors().nth(3).unwrap();
        assert_eq!(repo, Path::new("/repo"));
    }
}
