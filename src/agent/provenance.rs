//! Commit-provenance extraction. After an execute-phase agent finishes, we
//! scan its final message for the first 7–40 char hex token and pair it with
//! the worktree's current branch + path. This is the same heuristic
//! claw-code uses (`extract_commit_sha`, see
//! `docs/claw-code-analysis.md` §4.3) — it is cheap, correct in the common
//! case, and recoverable when wrong (the orchestrator can still query
//! `git log` later).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Provenance metadata recorded when an agent finishes an execute-phase task.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaneCommitProvenance {
    pub commit: String,
    pub branch: String,
    pub worktree: Option<PathBuf>,
    pub canonical_commit: Option<String>,
    pub superseded_by: Option<String>,
    pub lineage: Vec<String>,
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

#[cfg(test)]
mod tests {
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
    fn ignores_non_hex_garbage() {
        assert!(extract_commit_sha("zzzz xxxx yyyy").is_none());
    }

    #[test]
    fn returns_first_match() {
        let s = "committed deadbeef and then cafef00d more";
        // "deadbeef" (8 hex) — first run we accept.
        assert_eq!(extract_commit_sha(s).as_deref(), Some("deadbeef"));
    }

    #[test]
    fn caps_at_40_chars() {
        // 50 hex chars in a row — must take the first 40.
        let s = "0123456789abcdef0123456789abcdef0123456789abcdef00";
        let got = extract_commit_sha(s).unwrap();
        assert_eq!(got.len(), 40);
        assert!(s.starts_with(&got));
    }
}
