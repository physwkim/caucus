//! Persistent in-flight-round record — the durable shadow of the memory-only
//! `PendingRound` (`src/session/runtime/rounds.rs`).
//!
//! A `PendingRound` lives only in `Multiplexer::pending_rounds`, so a quit or
//! crash loses every in-flight round silently: on `caucus resume` the main
//! worker's claude conversation reloads still believing its `register_round`
//! call is live, yet caucus has no round to ever deliver — it would wait
//! forever, and the sub-agents' captured work would be gone with no trace.
//!
//! These records close that gap. The multiplexer writes a compact snapshot of
//! its pending rounds to `<session_root>/pending-rounds.json` whenever the set
//! changes; resume reads it to (a) preserve the captured work in
//! `dropped-rounds.log` and (b) tell the main worker the round was dropped so
//! it stops waiting and can re-issue it. The sub-agent *processes* cannot be
//! resumed mid-turn — they restart fresh — so a round is never silently
//! "continued"; it is surfaced and closed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::mcp::ReadPanelMode;

/// File name of the pending-rounds record under a session root.
pub const PENDING_ROUNDS_FILE: &str = "pending-rounds.json";

/// One panel's slice of an in-flight round, snapshotted for resume.
///
/// Keyed by position (the panel's index within the round), not by panel id:
/// panel ids are regenerated on resume, so the id cannot be the link. The role
/// label and the captured work are what survive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoundPanelRecord {
    /// Role label of the panel at snapshot time, or `(gone)` if its id had
    /// already vanished from the roster.
    pub role: String,
    /// Finished-turn outputs captured before the snapshot — the sub-agent's
    /// work product, preserved so resume can surface it instead of dropping it.
    #[serde(default)]
    pub captured: Vec<String>,
    /// Count of backlog tasks still queued (not yet run) at snapshot time.
    #[serde(default)]
    pub pending_backlog: usize,
}

/// One in-flight round, snapshotted for resume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingRoundRecord {
    /// One entry per round panel, in round order.
    #[serde(default)]
    pub panels: Vec<RoundPanelRecord>,
    /// How the round read each panel's result.
    pub read_mode: ReadPanelMode,
}

/// `<session_root>/pending-rounds.json`.
pub fn path(session_root: &Path) -> PathBuf {
    session_root.join(PENDING_ROUNDS_FILE)
}

/// Persist `rounds` to `<session_root>/pending-rounds.json`, or remove the file
/// when there are none — so resume sees a clean state once every round has been
/// delivered. Atomic write-to-temp + rename, mirroring `SessionRecord::write`.
pub fn write(session_root: &Path, rounds: &[PendingRoundRecord]) -> std::io::Result<()> {
    let path = path(session_root);
    if rounds.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        };
    }
    std::fs::create_dir_all(session_root)?;
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(rounds).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)
}

/// Read the persisted rounds. A missing or unparseable file yields an empty
/// list — a corrupt pending-rounds record is not worth aborting resume over;
/// the worst case is the same silent loss this feature otherwise prevents.
pub fn read(session_root: &Path) -> Vec<PendingRoundRecord> {
    let Ok(bytes) = std::fs::read(path(session_root)) else {
        return Vec::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Remove the pending-rounds file, ignoring a missing one.
pub fn clear(session_root: &Path) {
    let _ = std::fs::remove_file(path(session_root));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<PendingRoundRecord> {
        vec![PendingRoundRecord {
            panels: vec![
                RoundPanelRecord {
                    role: "backend".into(),
                    captured: vec!["task one output".into()],
                    pending_backlog: 2,
                },
                RoundPanelRecord {
                    role: "(gone)".into(),
                    captured: vec![],
                    pending_backlog: 0,
                },
            ],
            read_mode: ReadPanelMode::SinceLastTurn,
        }]
    }

    #[test]
    fn round_trips_through_json() {
        let recs = sample();
        let json = serde_json::to_string_pretty(&recs).unwrap();
        let back: Vec<PendingRoundRecord> = serde_json::from_str(&json).unwrap();
        assert_eq!(recs, back);
    }

    #[test]
    fn write_then_read_round_trips_on_disk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let recs = sample();
        write(tmp.path(), &recs).unwrap();
        assert!(path(tmp.path()).is_file());
        assert_eq!(read(tmp.path()), recs);
    }

    /// Writing an empty slice removes the file — resume must see a clean state
    /// once every round has been delivered, not a stale snapshot.
    #[test]
    fn writing_empty_removes_the_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        write(tmp.path(), &sample()).unwrap();
        assert!(path(tmp.path()).is_file());

        write(tmp.path(), &[]).unwrap();
        assert!(
            !path(tmp.path()).exists(),
            "empty write must delete the file"
        );
        assert!(read(tmp.path()).is_empty());
    }

    /// A missing or corrupt file reads as empty rather than panicking resume.
    #[test]
    fn missing_or_corrupt_reads_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(read(tmp.path()).is_empty(), "missing file → empty");

        std::fs::write(path(tmp.path()), b"{not json").unwrap();
        assert!(read(tmp.path()).is_empty(), "corrupt file → empty");
    }
}
