//! codex's state directory (`$CODEX_HOME`, default `~/.codex`) and the one
//! fact caucus reads from it: whether a thread is the panel's own
//! conversation.
//!
//! codex runs its `notify` program for every thread it drives — the panel's
//! conversation, the subagents it spawns, and the ephemeral thread that
//! writes the conversation title mid-turn. The notify JSON carries only the
//! `thread-id`. codex's rollout record for that thread is what says which
//! kind it is, and unlike the terminal title it does not change with the
//! thread a human happens to be viewing.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// codex's state directory: `$CODEX_HOME` when set (mirroring codex itself),
/// otherwise `~/.codex`. `None` when neither `CODEX_HOME` nor `HOME` is set.
pub(crate) fn dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return Some(PathBuf::from(home));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex"))
}

/// Whether `thread_id` is a top-level conversation by codex's record under
/// `home`: its rollout `sessions/YYYY/MM/DD/rollout-*-<thread_id>.jsonl`
/// exists, and its `session_meta` names no parent thread and no subagent or
/// internal source.
///
/// codex writes a thread's rollout before the thread's first model call
/// (`ensure_rollout_materialized` awaits the write), so a persisted thread's
/// rollout is on disk before its first `notify`. The ephemeral title thread is
/// never persisted, so no rollout means not the panel's. A `home` with no
/// `sessions` store at all gives nothing to classify against, so the thread
/// counts.
pub(crate) fn is_top_level_thread(home: &Path, thread_id: &str) -> bool {
    let sessions = home.join("sessions");
    if !sessions.is_dir() {
        return true;
    }
    let Some(rollout) = find_rollout(&sessions, thread_id) else {
        return false;
    };
    let mut first = String::new();
    let read = File::open(&rollout).and_then(|f| BufReader::new(f).read_line(&mut first));
    let meta = match (read, serde_json::from_str::<Value>(&first)) {
        (Ok(_), Ok(line)) if line["type"] == "session_meta" => line["payload"].clone(),
        // A rollout exists, so the thread was persisted; a first line caucus
        // cannot read says nothing against it.
        _ => return true,
    };
    let sub_source = meta["source"]
        .as_object()
        .is_some_and(|s| s.contains_key("subagent") || s.contains_key("internal"));
    meta["parent_thread_id"].is_null() && !sub_source
}

/// The rollout file for `thread_id`, searching the newest date directories
/// first: a panel's thread is almost always from today.
fn find_rollout(sessions: &Path, thread_id: &str) -> Option<PathBuf> {
    if thread_id.is_empty() {
        return None;
    }
    let suffix = format!("-{thread_id}.jsonl");
    for year in newest_first(sessions) {
        for month in newest_first(&year) {
            for day in newest_first(&month) {
                let Ok(entries) = std::fs::read_dir(&day) else {
                    continue;
                };
                let found = entries.flatten().find(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(&suffix))
                });
                if let Some(entry) = found {
                    return Some(entry.path());
                }
            }
        }
    }
    None
}

/// The subdirectories of `dir`, by name descending (codex zero-pads them).
fn newest_first(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort_unstable_by(|a, b| b.cmp(a));
    dirs
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Write a rollout for `thread_id` under `home` on `day` (`YYYY/MM/DD`)
    /// whose first line is a `session_meta` with `meta` as its payload.
    pub(crate) fn write_rollout(home: &Path, day: &str, thread_id: &str, meta: Value) {
        let dir = home.join("sessions").join(day);
        std::fs::create_dir_all(&dir).unwrap();
        let line = serde_json::json!({ "type": "session_meta", "payload": meta });
        std::fs::write(
            dir.join(format!("rollout-2026-11-02T15-38-08-{thread_id}.jsonl")),
            format!("{line}\n"),
        )
        .unwrap();
    }

    const MAIN: &str = "01a19afa-c6fd-77f2-8aed-354a60dafed7";
    const SUB: &str = "01a19afb-0471-7dd2-811a-ba579d7520b5";

    #[test]
    fn a_user_thread_is_top_level() {
        let home = tempfile::tempdir().unwrap();
        write_rollout(
            home.path(),
            "2026/11/02",
            MAIN,
            serde_json::json!({ "id": MAIN, "source": "cli" }),
        );
        assert!(is_top_level_thread(home.path(), MAIN));
    }

    /// Shapes as codex 0.162 writes them: a spawned subagent names its parent
    /// and a `subagent` source; an internal thread names an `internal` source.
    #[test]
    fn subagent_and_internal_threads_are_not_top_level() {
        let home = tempfile::tempdir().unwrap();
        write_rollout(
            home.path(),
            "2026/11/02",
            SUB,
            serde_json::json!({
                "id": SUB,
                "parent_thread_id": MAIN,
                "source": { "subagent": { "thread_spawn": { "parent_thread_id": MAIN, "depth": 1 } } },
            }),
        );
        assert!(!is_top_level_thread(home.path(), SUB));

        let internal = "01a19afc-0000-7000-8000-000000000000";
        write_rollout(
            home.path(),
            "2026/11/02",
            internal,
            serde_json::json!({ "id": internal, "source": { "internal": "memory_consolidation" } }),
        );
        assert!(!is_top_level_thread(home.path(), internal));
    }

    /// The ephemeral title thread has no rollout.
    #[test]
    fn an_unpersisted_thread_is_not_top_level() {
        let home = tempfile::tempdir().unwrap();
        write_rollout(
            home.path(),
            "2026/11/02",
            MAIN,
            serde_json::json!({ "id": MAIN, "source": "cli" }),
        );
        assert!(!is_top_level_thread(
            home.path(),
            "01a19afa-e139-7dc0-b6a3-2c27b3bc476b"
        ));
        assert!(!is_top_level_thread(home.path(), ""));
    }

    /// A panel left running across midnight keeps a thread from an older day.
    #[test]
    fn a_thread_from_an_earlier_day_is_found() {
        let home = tempfile::tempdir().unwrap();
        write_rollout(
            home.path(),
            "2026/10/31",
            MAIN,
            serde_json::json!({ "id": MAIN, "source": "cli" }),
        );
        write_rollout(
            home.path(),
            "2026/11/02",
            SUB,
            serde_json::json!({ "id": SUB, "parent_thread_id": MAIN }),
        );
        assert!(is_top_level_thread(home.path(), MAIN));
    }

    #[test]
    fn without_a_sessions_store_every_thread_counts() {
        let home = tempfile::tempdir().unwrap();
        assert!(is_top_level_thread(home.path(), MAIN));
    }
}
