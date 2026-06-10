//! `git worktree add` driver (`docs/design.md` §5).
//!
//! Worktree directories live under
//! `<repo>/.caucus/worktrees/<session>-<role-stem>-NN/` and check out a fresh
//! branch off the current `HEAD` (or an explicit base).
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
    /// Branch to create. `None` defaults to `caucus/<session>/<role-stem>`.
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
            .unwrap_or_else(|| format!("{}-{}", self.session_id, role_worktree_stem(&self.role)));
        self.repo_root.join(".caucus").join("worktrees").join(leaf)
    }

    /// Branch name to create.
    pub fn default_branch(&self) -> String {
        self.branch.clone().unwrap_or_else(|| {
            format!(
                "caucus/{session}/{role}",
                session = short_session(self.session_id),
                role = role_worktree_stem(&self.role)
            )
        })
    }
}

/// Filesystem/git-ref-safe slug for a free-form role label.
///
/// Role names are display labels and can contain spaces, punctuation, or
/// slashes. Worktree paths and branch components need a narrower alphabet, so
/// the worktree owner converts labels once at the path/ref boundary.
pub(crate) fn role_slug(role: &str) -> String {
    const MAX: usize = 48;
    let mut out = String::new();
    let mut last_dash = false;
    for ch in role.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if mapped == '-' {
            if !out.is_empty() && !last_dash {
                out.push('-');
                last_dash = true;
            }
        } else {
            out.push(mapped);
            last_dash = false;
        }
        if out.len() >= MAX {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "role".to_string()
    } else {
        out
    }
}

/// Collision-resistant path/ref stem for a role label. The slug keeps names
/// readable, and the stable hash keeps labels with the same slug distinct.
pub(crate) fn role_worktree_stem(role: &str) -> String {
    format!("{}-{:08x}", role_slug(role), stable_role_hash(role))
}

fn stable_role_hash(role: &str) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in role.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
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

/// Reconcile stale caucus-owned worktree state before a resume [`attach`].
///
/// An unclean exit (crash, `kill -9`) leaves a panel's worktree directory
/// *and* its `git worktree` registration in place, so a later `attach` of the
/// same `branch` to a fresh path fails with "branch already checked out". A
/// clean exit removes the directory but can leave a dangling registration.
/// This frees the branch for re-attach: `git worktree prune` drops
/// registrations whose directory is gone, then any worktree still checked out
/// on `branch` *under `<repo>/.caucus/worktrees/`* (i.e. caucus-owned, never a
/// user's own worktree) is force-removed.
///
/// Best-effort: failures are logged, not returned — a genuine problem
/// surfaces from the `attach` that follows.
pub(crate) fn reconcile_stale(repo_root: &Path, branch: &str) {
    // Drop registrations for directories that no longer exist (clean-exit case).
    let _ = run_git(repo_root, &["worktree".to_string(), "prune".to_string()]);

    // Free the branch if a *caucus-owned* stale worktree still holds it. A
    // checkout outside `.caucus/worktrees/` is the user's own and left alone.
    // Compare canonicalized paths: git reports the real path, while a plain
    // join keeps the symlinked form (e.g. macOS `/var` → `/private/var`), so a
    // naive `starts_with` would miss every caucus worktree on such systems.
    let caucus_dir = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf())
        .join(".caucus")
        .join("worktrees");
    if let Some(stale) = worktree_checkout_of(repo_root, branch)
        && stale
            .canonicalize()
            .unwrap_or_else(|_| stale.clone())
            .starts_with(&caucus_dir)
    {
        // The stale checkout may hold a crashed agent's *uncommitted* work, and
        // resume re-attaches `branch` to a different (`-resume`) path — so a bare
        // `git worktree remove --force` here would destroy that work with no
        // trace. Salvage it onto the branch first: a WIP commit rides to the
        // resume worktree (which re-checks out `branch`) and is trivially
        // reversible. Only a *clean* or *salvaged* worktree may be force-removed;
        // if the work cannot be salvaged, leave the directory in place rather
        // than discard it.
        if worktree_is_dirty(&stale)
            && let Err(err) = salvage_uncommitted_work(&stale, branch)
        {
            tracing::warn!(
                branch = %branch, path = %stale.display(), error = %format!("{err}"),
                "stale caucus worktree has uncommitted changes that could not be \
                 salvaged; leaving it in place rather than discarding the work"
            );
            return;
        }

        let args = vec![
            "worktree".to_string(),
            "remove".to_string(),
            "--force".to_string(),
            stale.display().to_string(),
        ];
        if let Err(err) = run_git(repo_root, &args) {
            tracing::warn!(
                branch = %branch, path = %stale.display(), error = %format!("{err}"),
                "failed to remove a stale caucus worktree on resume"
            );
        }
    }
}

/// Whether `worktree` has uncommitted changes — tracked edits/deletions or
/// non-ignored untracked files (`git status --porcelain` is non-empty).
///
/// An *unreadable* status is treated as dirty: when in doubt, preserve the
/// directory rather than risk force-removing live work.
fn worktree_is_dirty(worktree: &Path) -> bool {
    match run_git(worktree, &["status".to_string(), "--porcelain".to_string()]) {
        Ok(out) => !out.trim().is_empty(),
        Err(_) => true,
    }
}

/// Commit a crashed worktree's uncommitted changes onto its branch so they are
/// preserved across the resume re-attach instead of being discarded.
///
/// Stages everything `git status` reported (tracked changes, deletions, and
/// non-ignored untracked files; `.gitignore` is honoured, so build artefacts
/// are excluded) and commits it. A recovery identity is supplied via `-c` so
/// the commit cannot fail on a repo without a configured `user.name`/`email`,
/// and `--no-verify` skips hooks that have no business blocking crash recovery.
/// The commit is plainly labelled and reversible with `git reset HEAD^`.
fn salvage_uncommitted_work(worktree: &Path, branch: &str) -> Result<String, WorktreeError> {
    run_git(worktree, &["add".to_string(), "-A".to_string()])?;
    run_git(
        worktree,
        &[
            "-c".to_string(),
            "user.name=caucus".to_string(),
            "-c".to_string(),
            "user.email=caucus@localhost".to_string(),
            "commit".to_string(),
            "--no-verify".to_string(),
            "-m".to_string(),
            format!("caucus: recovered uncommitted work from a crashed worktree ({branch})"),
        ],
    )
}

/// The worktree directory currently checked out on `branch`, if any, parsed
/// from `git worktree list --porcelain`. Returns `None` when the branch is not
/// checked out anywhere (including when the branch no longer exists).
fn worktree_checkout_of(repo_root: &Path, branch: &str) -> Option<PathBuf> {
    let out = run_git(
        repo_root,
        &[
            "worktree".to_string(),
            "list".to_string(),
            "--porcelain".to_string(),
        ],
    )
    .ok()?;
    // Porcelain output is per-worktree blocks: a `worktree <path>` line followed
    // by (for a branch checkout) a `branch refs/heads/<name>` line.
    let target = format!("refs/heads/{branch}");
    let mut cur_path: Option<PathBuf> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            cur_path = Some(PathBuf::from(p));
        } else if line.strip_prefix("branch ") == Some(target.as_str()) {
            return cur_path;
        }
    }
    None
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

    /// A crash-survivor stale worktree (the branch still checked out at the
    /// prior directory) blocks a fresh `attach` of that branch with "already
    /// checked out". `reconcile_stale` force-removes the *caucus-owned* stale
    /// checkout so the re-attach succeeds — the path that keeps a resumed
    /// worktree panel isolated instead of silently dropping it into the repo
    /// root.
    #[test]
    fn reconcile_stale_frees_a_caucus_owned_branch_for_reattach() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(repo)
                .args(args)
                .output()
                .expect("run git")
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "caucus-test"]);
        git(&["commit", "--allow-empty", "-q", "-m", "init"]);

        // A worktree W1 on a fresh branch under .caucus/worktrees — the
        // survivor a crash would leave behind (dir + git registration intact).
        let req = WorktreeRequest {
            repo_root: repo.to_path_buf(),
            session_id: SessionId::new(),
            role: "backend".into(),
            branch: Some("caucus/test/backend-1".into()),
            base_ref: None,
            name_override: Some("s-backend-1".into()),
        };
        let w1 = create(&req).unwrap();
        let branch = w1.branch.clone();

        // A fresh attach of the same branch to a new path fails: the branch is
        // already checked out at W1.
        let p2 = repo
            .join(".caucus")
            .join("worktrees")
            .join("s-backend-1-resume");
        assert!(
            attach(repo, &p2, &branch).is_err(),
            "the branch is still checked out at the stale worktree"
        );

        // Reconcile frees the caucus-owned checkout, so the re-attach works.
        reconcile_stale(repo, &branch);
        let w2 = attach(repo, &p2, &branch).expect("re-attach after reconcile");
        assert_eq!(w2.branch, branch);
        assert!(p2.exists(), "the re-attached worktree directory exists");
    }

    /// A crashed worktree with *uncommitted* work must not be silently
    /// discarded on resume: `reconcile_stale` salvages the changes onto the
    /// branch (a reversible recovery commit) before removing the directory, so
    /// the work rides to the re-attached resume worktree instead of being lost
    /// to `--force`.
    #[test]
    fn reconcile_stale_salvages_uncommitted_work_before_removing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(repo)
                .args(args)
                .output()
                .expect("run git")
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "caucus-test"]);
        git(&["commit", "--allow-empty", "-q", "-m", "init"]);

        let req = WorktreeRequest {
            repo_root: repo.to_path_buf(),
            session_id: SessionId::new(),
            role: "backend".into(),
            branch: Some("caucus/test/backend-1".into()),
            base_ref: None,
            name_override: Some("s-backend-1".into()),
        };
        let w1 = create(&req).unwrap();
        let branch = w1.branch.clone();

        // The crashed agent left uncommitted work in its worktree.
        std::fs::write(w1.path.join("recovered.txt"), b"unsaved agent work").unwrap();
        assert!(worktree_is_dirty(&w1.path), "the stale worktree is dirty");

        reconcile_stale(repo, &branch);
        assert!(!w1.path.exists(), "the salvaged stale worktree is removed");

        // The work rode onto the branch: re-attaching it to the resume path
        // brings the recovered file back, instead of a clean checkout that
        // lost it.
        let p2 = repo
            .join(".caucus")
            .join("worktrees")
            .join("s-backend-1-resume");
        let w2 = attach(repo, &p2, &branch).expect("re-attach after reconcile");
        assert_eq!(w2.branch, branch);
        assert!(
            p2.join("recovered.txt").exists(),
            "the crashed agent's uncommitted work must survive on the branch"
        );
        assert_eq!(
            std::fs::read_to_string(p2.join("recovered.txt")).unwrap(),
            "unsaved agent work",
        );
    }

    /// `reconcile_stale` must not touch a checkout outside `.caucus/worktrees/`
    /// — that is the user's own worktree, not caucus's to remove.
    #[test]
    fn reconcile_stale_leaves_a_user_owned_worktree_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(repo)
                .args(args)
                .output()
                .expect("run git")
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "caucus-test"]);
        git(&["commit", "--allow-empty", "-q", "-m", "init"]);

        // A user worktree *outside* .caucus/worktrees on a branch.
        let user_wt = tmp.path().join("user-wt");
        let out = std::process::Command::new("git")
            .current_dir(repo)
            .args([
                "worktree",
                "add",
                "-b",
                "feature/keep-me",
                &user_wt.display().to_string(),
            ])
            .output()
            .expect("git worktree add");
        assert!(out.status.success(), "setup worktree add failed");

        reconcile_stale(repo, "feature/keep-me");
        assert!(
            user_wt.exists(),
            "a user-owned worktree must not be removed by reconcile"
        );
    }

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
        let file_name = p.file_name().unwrap().to_string_lossy();
        assert!(
            file_name.starts_with(&format!("{}-backend-", req.session_id)),
            "file name: {file_name}"
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
        assert!(b.contains("/reviewer-"), "branch: {b}");
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

    #[test]
    fn role_slug_sanitizes_free_form_labels() {
        assert_eq!(role_slug("Perf Profiler: QA/2"), "perf-profiler-qa-2");
        assert_eq!(role_slug(".hidden"), "hidden");
        assert_eq!(role_slug("!!!"), "role");
    }

    #[test]
    fn default_branch_uses_a_git_ref_safe_role_slug() {
        let req = WorktreeRequest {
            repo_root: PathBuf::from("/repo"),
            session_id: SessionId::new(),
            role: "Perf Profiler: QA/2".into(),
            branch: None,
            base_ref: None,
            name_override: None,
        };
        let branch = req.default_branch();
        assert!(branch.contains("/perf-profiler-qa-2-"), "branch: {branch}");
        assert!(
            std::process::Command::new("git")
                .args(["check-ref-format", &format!("refs/heads/{branch}")])
                .status()
                .unwrap()
                .success(),
            "branch must be accepted by git check-ref-format: {branch}"
        );
    }

    #[test]
    fn role_worktree_stem_distinguishes_slug_collisions() {
        assert_eq!(role_slug("a b"), role_slug("a:b"));
        assert_ne!(role_worktree_stem("a b"), role_worktree_stem("a:b"));
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
