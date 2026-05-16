//! `git worktree add` driver (`docs/design.md` §5).
//!
//! Worktree directories live under `<repo>/.caucus/worktrees/<session>-<role>-NN/`
//! and check out a fresh branch off the current `HEAD` (or an explicit base).
//!
//! **Invariant I-3** (`docs/design.md` §12): worktree *creation* is owned by
//! [`create`]; *deletion* goes through [`crate::worktree::cleanup`].

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use thiserror::Error;

use crate::session::id::SessionId;

/// A request to create one worktree for a role.
#[derive(Debug, Clone)]
pub struct WorktreeRequest {
    pub repo_root: PathBuf,
    pub session_id: SessionId,
    pub role: String,
    /// Branch to create. `None` defaults to `caucus/<session>/<role>`.
    pub branch: Option<String>,
    /// Base ref for the new branch. `None` means current `HEAD`.
    pub base_ref: Option<String>,
    /// Override the directory leaf name under `<repo>/.caucus/worktrees/`.
    pub name_override: Option<String>,
}

/// A created worktree.
#[derive(Debug, Clone)]
pub struct WorktreeHandle {
    pub path: PathBuf,
    pub branch: String,
    pub repo_root: PathBuf,
}

/// Errors from worktree creation.
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
    /// Destination directory under `<repo>/.caucus/worktrees/`.
    pub fn default_path(&self) -> PathBuf {
        let leaf = self
            .name_override
            .clone()
            .unwrap_or_else(|| format!("{}-{}", self.session_id, self.role));
        self.repo_root.join(".caucus").join("worktrees").join(leaf)
    }

    /// Branch name to create.
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
    s.chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

/// Single owner of worktree creation (Invariant I-3).
///
/// Errors out if the destination directory already exists (the caller must
/// clean up first via [`crate::worktree::cleanup`]).
///
/// Synchronous: `git worktree add` is a fast subprocess. The multiplexer
/// event loop calls this directly on its own thread — no async bridging, so
/// no nested-runtime hazard.
pub(crate) fn create(req: &WorktreeRequest) -> Result<WorktreeHandle, WorktreeError> {
    let path = req.default_path();
    if path.exists() {
        return Err(WorktreeError::AlreadyExists(path));
    }
    if let Some(parent) = path.parent()
        && let Err(source) = std::fs::create_dir_all(parent)
    {
        return Err(WorktreeError::Spawn {
            command: format!("mkdir -p {}", parent.display()),
            source,
        });
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
    run_git(&req.repo_root, &args)?;

    Ok(WorktreeHandle {
        path,
        branch,
        repo_root: req.repo_root.clone(),
    })
}

/// Run a git subcommand in `repo` and return trimmed stdout. Stderr is folded
/// into the error so the caller can classify it.
pub(crate) fn run_git(repo: &Path, args: &[String]) -> Result<String, WorktreeError> {
    let label = format!("git {}", args.join(" "));
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
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
            name_override: None,
        };
        let p = req.default_path();
        assert_eq!(p.parent().unwrap(), Path::new("/repo/.caucus/worktrees"));
        assert!(p.file_name().unwrap().to_string_lossy().ends_with("-backend"));
    }

    #[test]
    fn default_branch_template() {
        let req = WorktreeRequest {
            repo_root: PathBuf::from("/repo"),
            session_id: SessionId::new(),
            role: "reviewer".into(),
            branch: None,
            base_ref: None,
            name_override: None,
        };
        let b = req.default_branch();
        assert!(b.starts_with("caucus/"));
        assert!(b.ends_with("/reviewer"));
    }

    #[test]
    fn explicit_branch_wins() {
        let req = WorktreeRequest {
            repo_root: PathBuf::from("/repo"),
            session_id: SessionId::new(),
            role: "backend".into(),
            branch: Some("feature/x".into()),
            base_ref: None,
            name_override: None,
        };
        assert_eq!(req.default_branch(), "feature/x");
    }
}
