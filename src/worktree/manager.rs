//! `git worktree add` driver. Worktree directories live under
//! `<repo>/.caucus/worktrees/<session>-<role>/` and check out a fresh branch
//! named `caucus/<session>/<role>` off the current `HEAD` (or an explicit
//! base ref).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use thiserror::Error;
use tokio::process::Command;

use crate::session::id::SessionId;

#[derive(Debug, Clone)]
pub struct WorktreeRequest {
    pub repo_root: PathBuf,
    pub session_id: SessionId,
    pub role: String,
    /// Branch name to create. If `None`, defaults to `caucus/<session>/<role>`.
    pub branch: Option<String>,
    /// Base ref for the new branch. `None` means current `HEAD`.
    pub base_ref: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorktreeHandle {
    pub path: PathBuf,
    pub branch: String,
    pub repo_root: PathBuf,
}

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("worktree path already exists: {0}")]
    AlreadyExists(PathBuf),
    #[error("git command spawn ({command}): {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("git command failed ({command}, exit {code:?}): {stderr}")]
    NonZero {
        command: String,
        code: Option<i32>,
        stderr: String,
    },
}

impl WorktreeRequest {
    pub fn default_path(&self) -> PathBuf {
        self.repo_root
            .join(".caucus")
            .join("worktrees")
            .join(format!("{}-{}", self.session_id, self.role))
    }

    pub fn default_branch(&self) -> String {
        self.branch.clone().unwrap_or_else(|| {
            format!(
                "caucus/{session}/{role}",
                session = short_session(self.session_id),
                role = self.role
            )
        })
    }
}

fn short_session(id: SessionId) -> String {
    let s = id.to_string();
    // Use the trailing 8 chars — ULIDs are 26 chars; the last 8 are still
    // unique within a normal session count and keep the branch name short.
    s.chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

/// Create a worktree for the request. Errors out if the destination
/// directory already exists (caller must clean up first via
/// [`crate::worktree::cleanup`]).
pub async fn create(req: &WorktreeRequest) -> Result<WorktreeHandle, WorktreeError> {
    let path = req.default_path();
    if path.exists() {
        return Err(WorktreeError::AlreadyExists(path));
    }
    if let Some(parent) = path.parent() {
        if let Err(source) = std::fs::create_dir_all(parent) {
            return Err(WorktreeError::Spawn {
                command: format!("mkdir -p {}", parent.display()),
                source,
            });
        }
    }
    let branch = req.default_branch();

    let mut args = vec![
        "worktree".to_string(),
        "add".to_string(),
        "-b".to_string(),
        branch.clone(),
        path.display().to_string(),
    ];
    if let Some(base) = &req.base_ref {
        args.push(base.clone());
    }
    run_git(&req.repo_root, &args).await?;

    Ok(WorktreeHandle {
        path,
        branch,
        repo_root: req.repo_root.clone(),
    })
}

/// Run a git subcommand in `repo` and return stdout (trimmed). Errors include
/// stderr so the caller can classify them via `agent::lane_event::classify_failure`.
pub(crate) async fn run_git(repo: &Path, args: &[String]) -> Result<String, WorktreeError> {
    let label = format!("git {}", args.join(" "));
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|source| WorktreeError::Spawn {
            command: label.clone(),
            source,
        })?;
    if !output.status.success() {
        return Err(WorktreeError::NonZero {
            command: label,
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let mut s = String::from_utf8_lossy(&output.stdout).into_owned();
    if s.ends_with('\n') {
        s.pop();
    }
    Ok(s)
}

/// Convenience: `git rev-parse --abbrev-ref HEAD` inside `repo`.
pub async fn current_branch(repo: &Path) -> Result<String, WorktreeError> {
    run_git(
        repo,
        &[
            "rev-parse".to_string(),
            "--abbrev-ref".to_string(),
            "HEAD".to_string(),
        ],
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_path_layout() {
        let req = WorktreeRequest {
            repo_root: PathBuf::from("/repo"),
            session_id: SessionId::new(),
            role: "backend".into(),
            branch: None,
            base_ref: None,
        };
        let p = req.default_path();
        assert_eq!(p.parent().unwrap(), Path::new("/repo/.caucus/worktrees"));
        let name = p.file_name().unwrap().to_string_lossy();
        assert!(name.ends_with("-backend"));
    }

    #[test]
    fn default_branch_template_is_caucus_session_role() {
        let req = WorktreeRequest {
            repo_root: PathBuf::from("/repo"),
            session_id: SessionId::new(),
            role: "reviewer".into(),
            branch: None,
            base_ref: None,
        };
        let b = req.default_branch();
        assert!(b.starts_with("caucus/"));
        assert!(b.ends_with("/reviewer"));
        // Short-suffix length is 8 chars from the session id.
        let parts: Vec<_> = b.split('/').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1].len(), 8);
    }

    #[test]
    fn explicit_branch_wins() {
        let req = WorktreeRequest {
            repo_root: PathBuf::from("/repo"),
            session_id: SessionId::new(),
            role: "backend".into(),
            branch: Some("feature/x".into()),
            base_ref: None,
        };
        assert_eq!(req.default_branch(), "feature/x");
    }
}
