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
}
