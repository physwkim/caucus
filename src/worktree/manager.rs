//! `git worktree add` driver (`docs/design.md` §5).
//!
//! Worktree directories live under `<repo>/.caucus/worktrees/<session>-<role>-NN/`
//! and check out a fresh branch off the current `HEAD` (or an explicit base).
//!
//! **Invariant I-3** (`docs/design.md` §12): worktree *creation* is owned by
//! `create`; *deletion* goes through [`crate::worktree::cleanup`].

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

/// Re-add a worktree on an **existing** branch — the resume path
/// (`docs/design.md` §5). Unlike [`create`], this runs `git worktree add`
/// *without* `-b`: the branch already exists (it persisted across the prior
/// caucus shutdown, holding the agent's commits) and only its working
/// directory needs recreating.
///
/// Errors out if `path` already exists. Synchronous, like [`create`].
pub(crate) fn attach(
    repo_root: &Path,
    path: &Path,
    branch: &str,
) -> Result<WorktreeHandle, WorktreeError> {
    if path.exists() {
        return Err(WorktreeError::AlreadyExists(path.to_path_buf()));
    }
    if let Some(parent) = path.parent()
        && let Err(source) = std::fs::create_dir_all(parent)
    {
        return Err(WorktreeError::Spawn {
            command: format!("mkdir -p {}", parent.display()),
            source,
        });
    }
    let args = vec![
        "worktree".to_string(),
        "add".to_string(),
        path.display().to_string(),
        branch.to_string(),
    ];
    run_git(repo_root, &args)?;

    Ok(WorktreeHandle {
        path: path.to_path_buf(),
        branch: branch.to_string(),
        repo_root: repo_root.to_path_buf(),
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
        assert!(
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-backend")
        );
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

    /// `attach` re-adds a worktree on an *existing* branch — the resume path.
    /// Mirrors `cleanup::create_then_cleanup_a_real_worktree`: a hermetic temp
    /// git repo, create a branch via `create`, drop the directory, then
    /// re-attach a fresh worktree on the same branch.
    #[test]
    fn attach_re_adds_a_worktree_on_an_existing_branch() {
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(repo.path())
                .args(args)
                .output()
                .expect("run git")
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "caucus-test"]);
        git(&["commit", "--allow-empty", "-q", "-m", "init"]);

        // Create a worktree (which creates the branch), then remove just the
        // directory — simulating a prior caucus shutdown that cleaned the
        // worktree dir but kept the branch.
        let req = WorktreeRequest {
            repo_root: repo.path().to_path_buf(),
            session_id: SessionId::new(),
            role: "backend".into(),
            branch: Some("caucus/resume-test/backend".into()),
            base_ref: None,
            name_override: Some("ts-backend-1".into()),
        };
        let created = create(&req).expect("git worktree add -b");
        let branch = created.branch.clone();
        run_git(
            repo.path(),
            &[
                "worktree".into(),
                "remove".into(),
                "--force".into(),
                created.path.display().to_string(),
            ],
        )
        .expect("git worktree remove");
        assert!(!created.path.exists(), "worktree directory removed");

        // Re-attach a fresh worktree on the persisted branch.
        let attach_path = repo
            .path()
            .join(".caucus")
            .join("worktrees")
            .join("ts-backend-1-resume");
        let handle = attach(repo.path(), &attach_path, &branch).expect("git worktree add");
        assert_eq!(handle.branch, branch);
        assert!(handle.path.is_dir(), "re-attached worktree directory");
        assert!(
            handle.path.join(".git").exists(),
            "worktree .git marker present"
        );

        // Attaching onto an existing path is rejected.
        let err = attach(repo.path(), &attach_path, &branch);
        assert!(matches!(err, Err(WorktreeError::AlreadyExists(_))));
    }

    /// Attaching to a branch that does not exist fails — the resume path
    /// classifies this and spawns the panel fresh instead.
    #[test]
    fn attach_fails_when_branch_is_gone() {
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(repo.path())
                .args(args)
                .output()
                .expect("run git")
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "caucus-test"]);
        git(&["commit", "--allow-empty", "-q", "-m", "init"]);

        let path = repo.path().join(".caucus").join("worktrees").join("gone");
        let err = attach(repo.path(), &path, "caucus/no-such-branch");
        assert!(
            matches!(err, Err(WorktreeError::NonZero { .. })),
            "attaching a missing branch must fail: {err:?}"
        );
    }
}
