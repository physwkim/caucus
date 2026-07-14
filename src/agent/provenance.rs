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

/// Every 7–40 character ascii-hex run in `text`, in order — each one a token
/// that *could* be a SHA. A run longer than 40 characters yields its first 40.
///
/// All of them, not the first: a decimal number is also a run of hex digits, so
/// "processed 1048576 bytes, committed abc1234de" leads with a token that is not
/// a commit and follows with one that is. Stopping at the first candidate throws
/// away the real SHA whenever an agent's final message counts anything.
fn hex_runs(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut runs = Vec::new();
    let mut start = None;
    for i in 0..=bytes.len() {
        match (i < bytes.len() && bytes[i].is_ascii_hexdigit(), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let end = i.min(s + 40);
                if end - s >= 7 {
                    runs.push(&text[s..end]);
                }
                start = None;
            }
            _ => {}
        }
    }
    runs
}

/// Find the first 7–40 character ascii-hex run in `text`. Returns the SHA-like
/// string, or `None`.
pub fn extract_commit_sha(text: &str) -> Option<String> {
    hex_runs(text).first().map(|s| (*s).to_string())
}

/// The commit an agent left on its own branch, named in the final message of the
/// turn that just ended (`docs/design.md` §5). `None` when the message names no
/// such commit.
///
/// Two things must hold before caucus records provenance, and the second is what
/// makes it provenance at all:
///
/// 1. The token resolves to a real commit object (`git rev-parse`). Prose like
///    `deadbeef` fails here.
/// 2. That commit is **reachable from `branch`** — the panel's own lane branch.
///    A worktree shares its object database with the main checkout and with every
///    other panel's worktree, so *any* commit in the repository resolves in step
///    one, including another agent's. Without this check an agent that merely
///    *mentions* a commit — a sibling panel's, one it read out of `git log` —
///    would have it recorded as work it created, and then, because that commit is
///    not on its branch and never will be, retired a turn later as
///    [`SupersededBy::Unknown`]. Two false claims from one unverified join.
///
/// This is also the premise [`detect_supersession`] rests on: a recorded commit
/// was, at the moment it was recorded, on the branch. So a later "not on the
/// branch" is a real disappearance and not a commit that was never there.
///
/// Returns the *full 40-char* canonical SHA git resolved, so callers record the
/// unambiguous id.
pub fn extract_branch_commit(worktree: &Path, branch: &str, last_message: &str) -> Option<String> {
    hex_runs(last_message).into_iter().find_map(|candidate| {
        let sha = verify_commit(worktree, candidate)?;
        (reachable(worktree, &sha, branch) == Some(true)).then_some(sha)
    })
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

/// Decide whether `commit` — recorded earlier by [`extract_verified_commit`] —
/// is still the commit its `branch` holds (`docs/design.md` §8.2).
///
/// `None` means caucus makes no claim: the commit is still reachable from the
/// branch, or git could not answer (branch deleted, repo gone, git missing).
/// Only a definite disappearance yields `Some`, because the timeline records
/// facts and the absence of an event is not a claim that nothing happened.
///
/// `Some(..)` means the branch no longer contains the commit. An amend or a
/// rebase leaves the *patch* intact, so we look for it: the commit on the branch
/// whose `git patch-id` matches, among everything the branch gained since the
/// old commit's parent. That names a rebase exactly, however far back it landed
/// — the search is not depth-capped, because it does not cost per candidate:
/// [`patch_ids_since`] prices the whole set at three git processes. An amend that
/// rewrote the content has no matching patch anywhere, and yields
/// [`SupersededBy::Unknown`].
///
/// Blocking, on the same narrow gate as [`extract_verified_commit`] (the panel
/// owns a worktree *and* has a recorded commit), and every git call here is a
/// local object-database read.
pub fn detect_supersession(worktree: &Path, branch: &str, commit: &str) -> Option<SupersededBy> {
    // Only git's explicit "no" retires a commit. Still reachable, or git unable
    // to answer, are both "no claim" — recording "we cannot tell" as "it is gone"
    // is the one direction that puts a false claim on the timeline.
    if reachable(worktree, commit, branch) != Some(false) {
        return None;
    }
    // The old commit object survives the rewrite (git keeps it until gc), so
    // its patch and its parent are both still readable.
    let Some(want) = patch_id(worktree, commit) else {
        return Some(SupersededBy::Unknown);
    };
    match patch_ids_since(worktree, branch, commit)
        .into_iter()
        .find(|(patch, _)| *patch == want)
    {
        Some((_, replacement)) => Some(SupersededBy::Commit {
            commit: replacement,
        }),
        None => Some(SupersededBy::Unknown),
    }
}

/// The commit `branch` currently points at, or `None` if git cannot say.
///
/// Reachability from a branch can only change when the branch ref moves, so an
/// unchanged tip is proof that no recorded commit left the branch since the last
/// look. One process answers for every commit on the lane, which is why
/// [`crate::session::runtime::Multiplexer::record_commit_supersessions`] asks
/// this before asking [`detect_supersession`] anything.
pub fn branch_tip(worktree: &Path, branch: &str) -> Option<String> {
    if branch.is_empty() {
        return None;
    }
    verify_commit(worktree, branch)
}

/// Is `rev` reachable from `branch`? `Some(true)` / `Some(false)` when git
/// answers, `None` when it cannot — the branch does not exist, `repo` is not a
/// worktree, git is missing, or caucus never learned the panel's branch (`""`).
///
/// `merge-base --is-ancestor` exits 0 for yes, 1 for no, and anything else is a
/// failure to answer. The three stay distinct here because the two callers need
/// opposite defaults: recording provenance requires a definite yes, retiring a
/// commit requires a definite no, and "cannot tell" must satisfy neither.
fn reachable(repo: &Path, rev: &str, branch: &str) -> Option<bool> {
    if branch.is_empty() {
        return None;
    }
    let code = Command::new("git")
        .current_dir(repo)
        .args(["merge-base", "--is-ancestor", rev, branch])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?
        .code()?;
    match code {
        0 => Some(true),
        1 => Some(false),
        _ => None,
    }
}

/// `(patch_id, commit)` for every commit `branch` holds that is not an ancestor
/// of `commit`'s parent — i.e. everything that could have replaced it, newest
/// first. Empty when git cannot answer.
///
/// This is git's own patch-id pipeline, `rev-list | diff-tree --stdin -p |
/// patch-id --stable`, and it is why the search needs no depth cap: three
/// processes price the whole candidate set, not two per candidate. A merge
/// commit yields no patch under `diff-tree -p` and so simply does not appear —
/// correct, since a merge cannot be the rebase of a single commit.
fn patch_ids_since(repo: &Path, branch: &str, commit: &str) -> Vec<(String, String)> {
    let git = |args: &[&str], stdin: Stdio| {
        Command::new("git")
            .current_dir(repo)
            .args(args)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    };
    // `<commit>^@` is "all parents of <commit>", so this is every commit the
    // branch reached after the one that was rewritten away.
    let Ok(mut revs) = git(
        &["rev-list", branch, "--not", &format!("{commit}^@")],
        Stdio::null(),
    ) else {
        return Vec::new();
    };
    let out = revs
        .stdout
        .take()
        // `--root` so a rewritten *first* commit still produces a patch to
        // match; without it `diff-tree` prints nothing for a parentless commit
        // and the replacement is silently unnameable.
        .and_then(|revs| {
            git(
                &["diff-tree", "--stdin", "-p", "--no-color", "--root"],
                revs.into(),
            )
            .ok()
        })
        .and_then(|mut diffs| {
            let patches = diffs.stdout.take()?;
            let out = git(&["patch-id", "--stable"], patches.into())
                .ok()?
                .wait_with_output()
                .ok()?;
            let _ = diffs.wait();
            Some(out)
        });
    let _ = revs.wait();
    let Some(out) = out.filter(|o| o.status.success()) else {
        return Vec::new();
    };
    // One `<patch-id> <commit-sha>` line per commit that produced a patch.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (patch, sha) = line.split_once(' ')?;
            Some((patch.to_string(), sha.trim().to_string()))
        })
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
        let branch = branch_of(dir.path());
        let msg = format!("Implemented feature. Commit {} on branch.", &sha[..12]);
        let got = extract_branch_commit(dir.path(), &branch, &msg).unwrap();
        assert_eq!(got, sha);
    }

    #[test]
    fn rejects_a_bogus_sha() {
        let (dir, _sha) = repo_with_commit();
        let branch = branch_of(dir.path());
        // `deadbeef` is hex-shaped but not a commit in this repo.
        let got = extract_branch_commit(dir.path(), &branch, "see deadbeef please");
        assert!(got.is_none());
    }

    #[test]
    fn no_sha_in_message_yields_none() {
        let (dir, _sha) = repo_with_commit();
        let branch = branch_of(dir.path());
        assert!(extract_branch_commit(dir.path(), &branch, "nothing hex here").is_none());
    }

    /// A decimal number is a run of hex digits too, and agents count things.
    /// Stopping at the first candidate — `1048576` here — threw away the SHA
    /// that followed it and recorded no provenance at all.
    #[test]
    fn a_sha_is_found_past_a_candidate_that_is_not_a_commit() {
        let (dir, sha) = repo_with_commit();
        let branch = branch_of(dir.path());
        let msg = format!(
            "Processed 1048576 bytes across 30 files. Committed {}.",
            &sha[..12]
        );
        assert_eq!(
            extract_branch_commit(dir.path(), &branch, &msg).as_deref(),
            Some(sha.as_str()),
            "every hex run is a candidate, not just the first"
        );
    }

    /// A commit that exists but is not on this panel's branch is not this
    /// agent's work. Worktrees share one object database, so a sibling panel's
    /// commit resolves here just as well as our own — an agent that merely
    /// *mentions* one (read out of `git log`, quoted from another lane) would
    /// otherwise have it recorded as work it created, and then retired a turn
    /// later as superseded, because a commit that was never on the branch is
    /// trivially "no longer on the branch". Two false claims from one unverified
    /// join.
    #[test]
    fn a_commit_on_another_branch_is_not_this_agents_provenance() {
        let (dir, first) = repo_with_commit();
        let lane = branch_of(dir.path());

        // A sibling lane commits in the shared object database.
        git(dir.path(), &["checkout", "-q", "-b", "sibling"]);
        std::fs::write(dir.path().join("s.txt"), "sibling work").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "sibling work"]);
        let sibling = verify_commit(dir.path(), "HEAD").unwrap();
        git(dir.path(), &["checkout", "-q", &lane]);

        // git resolves it — the object is right there — but it is not ours.
        assert_eq!(
            verify_commit(dir.path(), &sibling).as_deref(),
            Some(sibling.as_str()),
            "the object database is shared, so the sibling's commit resolves"
        );
        let msg = format!("Reviewed the change in {}; nothing to fix.", &sibling[..12]);
        assert_eq!(
            extract_branch_commit(dir.path(), &lane, &msg),
            None,
            "a commit that is not on our branch is not our provenance"
        );

        // And our own commit still is.
        let msg = format!("Committed {}.", &first[..12]);
        assert_eq!(
            extract_branch_commit(dir.path(), &lane, &msg).as_deref(),
            Some(first.as_str())
        );
    }

    /// With no branch there is no join to verify, so there is nothing to record
    /// and nothing to retire — never a claim built on a branch caucus does not
    /// know.
    #[test]
    fn without_a_branch_nothing_is_recorded() {
        let (dir, sha) = repo_with_commit();
        let msg = format!("Committed {sha}.");
        assert_eq!(extract_branch_commit(dir.path(), "", &msg), None);
        assert_eq!(detect_supersession(dir.path(), "", &sha), None);
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

    /// Commit on a branch that is not `lane`, and return its sha — a sibling
    /// panel's work, sitting in the same object database that every worktree of
    /// this repository shares. Leaves `lane` checked out.
    pub(crate) fn commit_on_a_sibling_branch(dir: &Path, lane: &str) -> String {
        git(dir, &["checkout", "-q", "-b", "sibling-lane"]);
        std::fs::write(dir.join("sibling.txt"), "sibling work").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "sibling work"]);
        let sha = verify_commit(dir, "HEAD").unwrap();
        git(dir, &["checkout", "-q", lane]);
        sha
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

    /// The commit that replaced a rewritten one is named however deep it now
    /// sits. The search used to stop at 64 commits and report `Unknown` past
    /// that — a wrong answer dressed as an honest one. `patch_ids_since` prices
    /// the whole branch in three git processes, so there is no depth to cap:
    /// here the replacement sits 100 commits behind the tip and is still named.
    #[test]
    fn a_replacement_is_named_however_far_behind_the_tip_it_sits() {
        let (dir, sha) = repo_with_commit();
        let branch = branch_of(dir.path());
        amend_reword(dir.path(), "reworded");
        let replacement = verify_commit(dir.path(), "HEAD").unwrap();

        for i in 0..100 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), i.to_string()).unwrap();
            git(dir.path(), &["add", "."]);
            git(dir.path(), &["commit", "-q", "-m", &format!("later {i}")]);
        }

        assert_eq!(
            detect_supersession(dir.path(), &branch, &sha),
            Some(SupersededBy::Commit {
                commit: replacement
            })
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
