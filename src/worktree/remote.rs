//! A sub-host lane's branch moving between this repo and the host's checkout:
//! main's `HEAD` pushed there as the lane's base, the lane's commits fetched
//! back so main can review them here.
//!
//! git runs in a session of its own (`setsid`), with no terminal to prompt
//! on: an ssh password or host-key prompt fails the push or fetch instead of
//! writing over the TUI and waiting for input it can never get.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use tokio::sync::OwnedMutexGuard;

/// Create `branch` in the repo at `url` at this repo's `HEAD`. Refused if the
/// host already has `branch` somewhere else.
pub(crate) async fn push_head(repo: &Path, url: &OsStr, branch: &str) -> Result<(), String> {
    git(
        repo,
        [
            OsStr::new("push"),
            OsStr::new("--quiet"),
            OsStr::new("--no-verify"),
            OsStr::new("--"),
            url,
            OsStr::new(&format!("HEAD:refs/heads/{branch}")),
        ],
    )
    .await
}

/// A branch of this repo, held so that no other fetch into it runs
/// meanwhile: two at once fight over the ref, and the loser fails.
pub(crate) struct LocalBranch {
    repo: PathBuf,
    branch: String,
    _held: OwnedMutexGuard<()>,
}

impl LocalBranch {
    /// Hold `repo`'s `branch` once every earlier holder lets go.
    pub(crate) async fn hold(repo: &Path, branch: &str) -> Self {
        type Held = HashMap<(PathBuf, String), Weak<tokio::sync::Mutex<()>>>;
        static HELD: LazyLock<Mutex<Held>> = LazyLock::new(Mutex::default);
        let key = (repo.to_path_buf(), branch.to_string());
        let lock = {
            let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
            held.retain(|_, lock| lock.strong_count() > 0);
            match held.get(&key).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(tokio::sync::Mutex::new(()));
                    held.insert(key, Arc::downgrade(&lock));
                    lock
                }
            }
        };
        Self {
            repo: repo.to_path_buf(),
            branch: branch.to_string(),
            _held: lock.lock_owned().await,
        }
    }

    /// Set this branch to the host's branch of the same name, whatever it
    /// was here.
    pub(crate) async fn fetch(&self, url: &OsStr) -> Result<(), String> {
        let branch = &self.branch;
        git(
            &self.repo,
            [
                OsStr::new("fetch"),
                OsStr::new("--quiet"),
                OsStr::new("--no-tags"),
                OsStr::new("--no-write-fetch-head"),
                OsStr::new("--"),
                url,
                OsStr::new(&format!("+refs/heads/{branch}:refs/heads/{branch}")),
            ],
        )
        .await
    }
}

async fn git<'a>(repo: &Path, args: impl IntoIterator<Item = &'a OsStr>) -> Result<(), String> {
    let args: Vec<OsString> = args.into_iter().map(OsStr::to_os_string).collect();
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(&args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // SAFETY: `setsid` is async-signal-safe, which is all `pre_exec` requires.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = command
        .output()
        .await
        .map_err(|err| format!("run git: {err}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {} failed: {}",
            args[0].to_string_lossy(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::manager::run_git;

    fn git_init(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@example.com"],
            &["config", "user.name", "t"],
            &["commit", "-q", "--allow-empty", "-m", "root"],
        ] {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            run_git(dir, &args).unwrap();
        }
    }

    fn rev(dir: &Path, rev: &str) -> String {
        run_git(dir, &["rev-parse".into(), rev.into()])
            .unwrap()
            .trim()
            .to_string()
    }

    /// A url shaped like an option reaches git as the repository.
    #[tokio::test]
    async fn an_option_shaped_url_is_still_the_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let here = tmp.path().join("here");
        git_init(&here);
        let url = OsStr::new("-x");
        for err in [
            push_head(&here, url, "b").await.unwrap_err(),
            LocalBranch::hold(&here, "b")
                .await
                .fetch(url)
                .await
                .unwrap_err(),
        ] {
            assert!(!err.contains("unknown switch"), "{err}");
        }
    }

    #[tokio::test]
    async fn push_then_fetch_moves_a_branch_both_ways() {
        let tmp = tempfile::tempdir().unwrap();
        let (here, there) = (tmp.path().join("here"), tmp.path().join("there"));
        git_init(&here);
        git_init(&there);
        let url = there.as_os_str();

        push_head(&here, url, "caucus/x/lane-1").await.unwrap();
        assert_eq!(rev(&there, "caucus/x/lane-1"), rev(&here, "HEAD"));

        run_git(
            &there,
            &[
                "commit".into(),
                "-q".into(),
                "--allow-empty".into(),
                "-m".into(),
                "lane work".into(),
            ],
        )
        .unwrap();
        run_git(
            &there,
            &[
                "branch".into(),
                "-f".into(),
                "caucus/x/lane-1".into(),
                "main".into(),
            ],
        )
        .unwrap();
        let lane = LocalBranch::hold(&here, "caucus/x/lane-1").await;
        lane.fetch(url).await.unwrap();
        assert_eq!(rev(&here, "caucus/x/lane-1"), rev(&there, "main"));

        let err = LocalBranch::hold(&here, "caucus/x/absent")
            .await
            .fetch(url)
            .await
            .unwrap_err();
        assert!(err.starts_with("git fetch failed"), "{err}");
    }

    /// A lane's branch fetched twice at once, as when a panel goes while its
    /// last turn's fetch still runs and the disposer fetches again: both
    /// land, whichever starts first.
    #[tokio::test(flavor = "multi_thread")]
    async fn fetches_of_one_branch_at_once_both_land() {
        let tmp = tempfile::tempdir().unwrap();
        let (here, there) = (tmp.path().join("here"), tmp.path().join("there"));
        git_init(&here);
        git_init(&there);
        run_git(&here, &["checkout".into(), "-q".into(), "--detach".into()]).unwrap();
        let url = there.as_os_str();
        for turn in 0..10 {
            run_git(
                &there,
                &[
                    "commit".into(),
                    "-q".into(),
                    "--allow-empty".into(),
                    "-m".into(),
                    format!("turn {turn}"),
                ],
            )
            .unwrap();
            let fetch = || async { LocalBranch::hold(&here, "main").await.fetch(url).await };
            let (a, b) = tokio::join!(fetch(), fetch());
            a.unwrap();
            b.unwrap();
            assert_eq!(rev(&here, "refs/heads/main"), rev(&there, "main"));
        }
    }
}
