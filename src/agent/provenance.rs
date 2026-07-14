//! Commit-provenance extraction. After an execute-phase agent finishes, we
//! scan its final turn-signal message for the first 7–40 char hex token and
//! pair it with the worktree's branch + path.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// Provenance metadata recorded when an agent creates a commit in its
/// worktree. Attached to a [`crate::agent::lane_event::LaneEventKind::CommitCreated`]
/// event.
///
/// Three fields the design once listed here are deliberately absent, for the
/// same reason `LaneEventKind` has no `Finished`: nothing could produce them.
///
/// - `canonical_commit` — the commit as it lands on the integration branch.
///   caucus never integrates: it runs `worktree add/remove`, never `merge`,
///   `rebase`, or `cherry-pick`. A human merges the lane branch outside the
///   session, possibly squashed, possibly after caucus has exited. There is no
///   moment at which caucus could observe the answer, so the field would be
///   `None` forever. If caucus ever owns the integration step, it comes back as
///   a `CommitIntegrated` event written by whoever performs the merge.
/// - `superseded_by` — the timeline is append-only, so a commit's fate cannot
///   be back-written into the `CommitCreated` event that announced it. It is a
///   later fact and therefore a later event:
///   [`crate::agent::lane_event::LaneEventKind::CommitSuperseded`].
/// - `lineage` — the chain of supersessions is the transitive closure of those
///   events. Storing it too would put one fact in two places, free to disagree.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaneCommitProvenance {
    pub commit: String,
    pub branch: String,
    pub worktree: Option<PathBuf>,
}

/// Find the first 7–40 character ascii-hex run in `text`. Returns the SHA-like
/// string, or `None`.
pub fn extract_commit_sha(text: &str) -> Option<String> {
    let mut run = String::new();
    for ch in text.chars() {
        if ch.is_ascii_hexdigit() {
            run.push(ch);
            if run.len() == 40 {
                return Some(run);
            }
        } else {
            if (7..=40).contains(&run.len()) {
                return Some(run);
            }
            run.clear();
        }
    }
    if (7..=40).contains(&run.len()) {
        Some(run)
    } else {
        None
    }
}

/// Scan a turn signal's `last_message` for the first SHA-like token and
/// confirm it names a real commit object in `repo` via `git rev-parse`
/// (`docs/design.md` §5).
///
/// Returns the *full 40-char* canonical SHA git resolved (so callers record
/// the unambiguous id), or `None` when the message carries no SHA-like token,
/// or the token does not resolve to a commit in `repo`. A `false`-positive
/// hex run from prose (e.g. `deadbeef` in a sentence) simply fails to resolve
/// and yields `None` rather than an error.
pub fn extract_verified_commit(repo: &Path, last_message: &str) -> Option<String> {
    let candidate = extract_commit_sha(last_message)?;
    verify_commit(repo, &candidate)
}

/// What replaced a recorded commit that its branch no longer contains
/// (`docs/design.md` §8.2). Payload of
/// [`crate::agent::lane_event::LaneEventKind::CommitSuperseded`].
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum SupersededBy {
    /// A commit now on the branch carries the same patch — a rebase, or an
    /// amend that touched only the message. Named by its full sha.
    Commit { commit: String },
    /// Nothing on the branch carries the old commit's patch: an amend that
    /// changed the content, or a commit the agent dropped. That the commit is
    /// gone is a fact; what stands in its place cannot be named, and caucus
    /// says so rather than guessing.
    Unknown,
}

/// How many commits ahead of the superseded one we search for its patch. A lane
/// branch is one agent's work, so its rewrite window is small; a rebase that
/// moved a commit further back than this is reported as
/// [`SupersededBy::Unknown`] rather than searched indefinitely on the UI loop.
const SUPERSESSION_SEARCH_DEPTH: &str = "64";

/// Decide whether `commit` — recorded earlier by [`extract_verified_commit`] —
/// is still the commit its `branch` holds (`docs/design.md` §8.2).
///
/// `None` means caucus makes no claim: the commit is still reachable from the
/// branch, or git could not answer (branch deleted, repo gone, git missing).
/// Only a definite disappearance yields `Some`, because the timeline records
/// facts and the absence of an event is not a claim that nothing happened.
///
/// `Some(..)` means the branch no longer contains the commit. An amend or a
/// rebase leaves the *patch* intact, so we look for it: the commit whose
/// `git patch-id` matches, among those the branch gained since the old commit's
/// parent. That names a rebase exactly. An amend that rewrote the content has
/// no matching patch anywhere, and yields [`SupersededBy::Unknown`].
///
/// Blocking, on the same narrow gate as [`extract_verified_commit`] (the panel
/// owns a worktree *and* has a recorded commit), and every git call here is a
/// local object-database read.
pub fn detect_supersession(worktree: &Path, branch: &str, commit: &str) -> Option<SupersededBy> {
    if branch.is_empty() || is_ancestor(worktree, commit, branch) {
        return None;
    }
    // The old commit object survives the rewrite (git keeps it until gc), so
    // its patch and its parent are both still readable.
    let Some(want) = patch_id(worktree, commit) else {
        return Some(SupersededBy::Unknown);
    };
    for candidate in commits_since(worktree, branch, commit) {
        if patch_id(worktree, &candidate).as_deref() == Some(want.as_str()) {
            return Some(SupersededBy::Commit { commit: candidate });
        }
    }
    Some(SupersededBy::Unknown)
}

/// `true` if `rev` is reachable from `branch`.
///
/// `merge-base --is-ancestor` exits 0 for yes and 1 for no; any other exit is
/// git failing to answer (unknown branch, no repo, deleted object), and so is a
/// spawn error. Both answer `true` — "we cannot tell" must never be recorded as
/// "the commit is gone", which is the one direction that puts a false claim on
/// the timeline.
fn is_ancestor(repo: &Path, rev: &str, branch: &str) -> bool {
    let status = Command::new("git")
        .current_dir(repo)
        .args(["merge-base", "--is-ancestor", rev, branch])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    !matches!(status.map(|st| st.code()), Ok(Some(1)))
}

/// The commits `branch` holds that are not ancestors of `commit`'s parent —
/// i.e. everything that could have replaced it — newest first, capped at
/// [`SUPERSESSION_SEARCH_DEPTH`].
fn commits_since(repo: &Path, branch: &str, commit: &str) -> Vec<String> {
    let output = Command::new("git")
        .current_dir(repo)
        .args([
            "rev-list",
            "-n",
            SUPERSESSION_SEARCH_DEPTH,
            branch,
            "--not",
            &format!("{commit}^@"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok();
    let Some(output) = output.filter(|o| o.status.success()) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// `git patch-id --stable` of a commit's diff: the identity of *what it
/// changed*, stable across the sha rewrites an amend or rebase performs.
/// `None` for a commit git cannot diff (gone, or a merge with no single patch).
fn patch_id(repo: &Path, rev: &str) -> Option<String> {
    let mut diff = Command::new("git")
        .current_dir(repo)
        .args(["diff-tree", "-p", "--no-color", "--root", rev])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let patch = diff.stdout.take()?;
    let out = Command::new("git")
        .current_dir(repo)
        .args(["patch-id", "--stable"])
        .stdin(Stdio::from(patch))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let _ = diff.wait();
    // `<patch-id> <commit-sha>`; an empty diff prints nothing.
    String::from_utf8(out.stdout)
        .ok()?
        .split_whitespace()
        .next()
        .map(str::to_string)
}

/// Resolve `rev` against `repo` with `git rev-parse --verify <rev>^{commit}`.
/// `Some(full_sha)` if it is a real commit, `None` otherwise (including when
/// `git` itself is missing or `repo` is not a worktree).
///
/// Blocking, and called from the turn-signal path on the UI loop
/// (`Multiplexer::handle_signal`). That is affordable because the gate above it
/// is narrow — the panel must own a worktree *and* its last message must carry a
/// hex run — and because `rev-parse` is a local object-database lookup, not a
/// network or index operation. Contrast `git worktree add`, which is slow enough
/// that caucus runs it off the loop.
pub fn verify_commit(repo: &Path, rev: &str) -> Option<String> {
    // The `^{commit}` peel rejects tokens that resolve to a tree/blob/tag —
    // only an actual commit object counts as provenance.
    let output = Command::new("git")
        .current_dir(repo)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn finds_short_sha() {
        assert_eq!(
            extract_commit_sha("see 0123456").as_deref(),
            Some("0123456")
        );
    }

    #[test]
    fn finds_full_sha() {
        let s = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(extract_commit_sha(s).as_deref(), Some(s));
    }

    #[test]
    fn ignores_short_runs() {
        assert!(extract_commit_sha("ab cd ef").is_none());
    }

    #[test]
    fn returns_first_match() {
        let s = "committed deadbeef and then cafef00d more";
        assert_eq!(extract_commit_sha(s).as_deref(), Some("deadbeef"));
    }

    #[test]
    fn caps_at_40_chars() {
        let s = "0123456789abcdef0123456789abcdef0123456789abcdef00";
        let got = extract_commit_sha(s).unwrap();
        assert_eq!(got.len(), 40);
        assert!(s.starts_with(&got));
    }

    /// Build a throwaway git repo with one commit; return `(dir, full_sha)`.
    pub(crate) fn repo_with_commit() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let st = Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(dir.path().join("f.txt"), "hi").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "first"]);
        let sha = verify_commit(dir.path(), "HEAD").unwrap();
        (dir, sha)
    }

    #[test]
    fn verifies_a_real_commit() {
        let (dir, sha) = repo_with_commit();
        let msg = format!("Implemented feature. Commit {} on branch.", &sha[..12]);
        let got = extract_verified_commit(dir.path(), &msg).unwrap();
        assert_eq!(got, sha);
    }

    #[test]
    fn rejects_a_bogus_sha() {
        let (dir, _sha) = repo_with_commit();
        // `deadbeef` is hex-shaped but not a commit in this repo.
        let got = extract_verified_commit(dir.path(), "see deadbeef please");
        assert!(got.is_none());
    }

    #[test]
    fn no_sha_in_message_yields_none() {
        let (dir, _sha) = repo_with_commit();
        assert!(extract_verified_commit(dir.path(), "nothing hex here").is_none());
    }

    /// Run a git command in `dir`, asserting it succeeded.
    fn git(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?} failed");
    }

    /// Reword `HEAD` in place: a new sha carrying the same patch, which is what
    /// an agent's `commit --amend` (and a rebase) leaves behind.
    pub(crate) fn amend_reword(dir: &Path, message: &str) {
        git(dir, &["commit", "-q", "--amend", "-m", message]);
    }

    /// The branch `dir` has checked out.
    pub(crate) fn branch_of(dir: &Path) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// A commit the branch still holds is live: caucus makes no claim about it.
    #[test]
    fn a_commit_still_on_its_branch_is_not_superseded() {
        let (dir, sha) = repo_with_commit();
        let branch = branch_of(dir.path());
        assert_eq!(detect_supersession(dir.path(), &branch, &sha), None);

        // Still live once the agent commits *on top* of it — a descendant does
        // not supersede its ancestor.
        std::fs::write(dir.path().join("g.txt"), "more").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "second"]);
        assert_eq!(detect_supersession(dir.path(), &branch, &sha), None);
    }

    /// `commit --amend -m` rewrites the sha but not the patch, so the commit
    /// that replaced it can be named. This is the rebase case too: same diff,
    /// new sha.
    #[test]
    fn an_amend_that_keeps_the_patch_names_the_commit_that_replaced_it() {
        let (dir, sha) = repo_with_commit();
        let branch = branch_of(dir.path());
        git(dir.path(), &["commit", "-q", "--amend", "-m", "reworded"]);
        let new_sha = verify_commit(dir.path(), "HEAD").unwrap();
        assert_ne!(new_sha, sha, "the amend produced a new sha");

        assert_eq!(
            detect_supersession(dir.path(), &branch, &sha),
            Some(SupersededBy::Commit { commit: new_sha }),
            "the commit carrying the same patch is named"
        );
    }

    /// An amend that changes the *content* leaves no commit with the old patch.
    /// Caucus records that the commit is gone and says it cannot name what
    /// stands in its place, rather than guessing at the nearest sha.
    #[test]
    fn an_amend_that_rewrites_the_content_supersedes_with_an_unnameable_commit() {
        let (dir, sha) = repo_with_commit();
        let branch = branch_of(dir.path());
        std::fs::write(dir.path().join("f.txt"), "different content entirely").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "--amend", "-m", "first"]);

        assert_eq!(
            detect_supersession(dir.path(), &branch, &sha),
            Some(SupersededBy::Unknown)
        );
    }

    /// A commit dropped outright (`reset --hard` back past it) is superseded by
    /// nothing at all — gone is still a fact worth recording.
    #[test]
    fn a_dropped_commit_is_superseded_by_nothing() {
        let (dir, first) = repo_with_commit();
        let branch = branch_of(dir.path());
        std::fs::write(dir.path().join("g.txt"), "more").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "second"]);
        let second = verify_commit(dir.path(), "HEAD").unwrap();
        git(dir.path(), &["reset", "-q", "--hard", &first]);

        assert_eq!(detect_supersession(dir.path(), &branch, &first), None);
        assert_eq!(
            detect_supersession(dir.path(), &branch, &second),
            Some(SupersededBy::Unknown)
        );
    }

    /// A branch git cannot resolve is not evidence that a commit vanished. The
    /// panel's branch is empty when caucus never recorded one; asking git about
    /// `""` must yield no claim, not a false supersession.
    #[test]
    fn an_unresolvable_branch_makes_no_claim() {
        let (dir, sha) = repo_with_commit();
        assert_eq!(detect_supersession(dir.path(), "", &sha), None);
        assert_eq!(
            detect_supersession(dir.path(), "no-such-branch", &sha),
            None
        );
    }
}
