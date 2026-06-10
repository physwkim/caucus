//! `caucus gc` — reclaim disk from stale session state (`docs/design.md` §10).
//!
//! caucus never deletes a session's on-disk state on its own: every run leaves
//! `<repo>/.caucus/sessions/<id>/` behind — the resume record, the per-panel
//! capture spill logs, the agent manifests, the pending-rounds file. Across many
//! runs that tree grows without bound. `gc` prunes the state of *old,
//! not-currently-running* sessions.
//!
//! Safety boundary: gc removes only caucus's own runtime state under
//! `.caucus/sessions/`. It never deletes git branches or worktrees — those can
//! hold un-merged agent commits, and are managed by the explicit panel-discard,
//! shutdown, and resume-reconcile paths instead. A session a live caucus is
//! holding open (its [`SessionLock`] is held) is always skipped.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};

use crate::session::id::SessionId;
use crate::session::lock::{SessionLock, SessionLockError};
use crate::session::record::{self, session_root};

/// One session eligible for pruning.
#[derive(Debug, Clone)]
pub struct PrunableSession {
    pub id: SessionId,
    /// `<repo>/.caucus/sessions/<id>/`.
    pub root: PathBuf,
    /// Free-form session topic, echoed in the plan so a human can recognise it.
    pub topic: String,
    /// How old the session is now — drives the "older than" decision and the
    /// human-readable plan listing.
    pub age: Duration,
}

/// What gc would do for a given retention window: the sessions it would prune,
/// and the live ones it is skipping.
#[derive(Debug)]
pub struct GcPlan {
    pub prunable: Vec<PrunableSession>,
    /// Sessions old enough to prune but skipped because a live caucus holds
    /// their lock.
    pub skipped_live: Vec<SessionId>,
}

/// What executing a [`GcPlan`] actually did.
#[derive(Debug, Default)]
pub struct GcReport {
    pub removed: Vec<SessionId>,
    /// `(id, error)` for a session whose directory removal failed.
    pub failed: Vec<(SessionId, String)>,
    /// Sessions that a caucus claimed between [`plan`] and [`execute`] — not an
    /// error, just left alone.
    pub raced: Vec<SessionId>,
}

/// Build the prune plan for `repo`: every discovered session at least
/// `older_than` old that no live caucus is holding open. `now` is injected so
/// the decision is testable.
pub fn plan(repo: &Path, older_than: Duration, now: DateTime<Utc>) -> GcPlan {
    let mut prunable = Vec::new();
    let mut skipped_live = Vec::new();
    for rec in record::discover(repo) {
        let root = session_root(repo, rec.id);
        let age = now - last_active(&root, &rec);
        if age < older_than {
            continue;
        }
        if SessionLock::is_held(&root) {
            skipped_live.push(rec.id);
            continue;
        }
        prunable.push(PrunableSession {
            id: rec.id,
            root,
            topic: rec.topic,
            age,
        });
    }
    GcPlan {
        prunable,
        skipped_live,
    }
}

/// A session's last-activity time: the mtime of its `session.json`.
///
/// `created_at` is stamped once at creation and never moves, so ageing by it
/// prunes a long-lived session that was *resumed* — and is therefore actively
/// in use — only yesterday. `session.json` is rewritten (atomic temp + rename)
/// on every roster change and once on every resume (`persist_record`), so its
/// mtime tracks real activity. Because the file is first written at creation,
/// its mtime is always at or after `created_at`; switching to it can only make
/// a session look *fresher*, never staler, so gc never prunes something the
/// `created_at` rule would have kept. Falls back to `created_at` when the mtime
/// cannot be read (e.g. a record write raced the stat).
fn last_active(session_root: &Path, rec: &record::SessionRecord) -> DateTime<Utc> {
    record::SessionRecord::path(session_root)
        .metadata()
        .and_then(|m| m.modified())
        .map(DateTime::<Utc>::from)
        .unwrap_or(rec.created_at)
}

/// Remove each prunable session's state directory.
///
/// Per session this re-acquires the [`SessionLock`] and holds it across the
/// directory removal, so a caucus that launched between [`plan`] and here is
/// detected (lock contended → recorded in [`GcReport::raced`]) rather than
/// having its state half-deleted underneath it. The guard's lock file is
/// unlinked along with the directory; the guard then drops, releasing it.
pub fn execute(plan: &GcPlan) -> GcReport {
    let mut report = GcReport::default();
    for session in &plan.prunable {
        match SessionLock::acquire(&session.root) {
            Ok(_guard) => match std::fs::remove_dir_all(&session.root) {
                Ok(()) => report.removed.push(session.id),
                Err(err) => report.failed.push((session.id, err.to_string())),
            },
            Err(SessionLockError::AlreadyRunning { .. }) => report.raced.push(session.id),
            Err(SessionLockError::Io { source, .. }) => {
                report.failed.push((session.id, source.to_string()));
            }
        }
    }
    report
}

/// Parse a retention window like `30m`, `24h`, `7d`, or `2w`. A bare integer is
/// read as days (`7` == `7d`). Wired as the `--older-than` value parser, so a
/// bad spec surfaces as a clap usage error.
pub fn parse_retention(spec: &str) -> Result<Duration, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("retention window must not be empty".to_string());
    }
    let (num, unit) = match spec.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => (&spec[..i], &spec[i..]),
        None => (spec, "d"), // all digits → days
    };
    let value: i64 = num
        .parse()
        .map_err(|_| format!("invalid retention number in `{spec}`"))?;
    if value < 0 {
        return Err(format!("retention window must not be negative: `{spec}`"));
    }
    match unit {
        "s" => Ok(Duration::seconds(value)),
        "m" => Ok(Duration::minutes(value)),
        "h" => Ok(Duration::hours(value)),
        "d" => Ok(Duration::days(value)),
        "w" => Ok(Duration::weeks(value)),
        other => Err(format!(
            "unknown retention unit `{other}` in `{spec}` (use s, m, h, d, or w)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::LayoutMode;
    use crate::session::record::SessionRecord;

    /// Write a session record `age` old under `repo/.caucus/sessions/<id>/`,
    /// returning its id. Both `created_at` and the `session.json` mtime — gc's
    /// activity signal — are backdated by `age`.
    fn write_session(repo: &Path, topic: &str, age: Duration) -> SessionId {
        let id = SessionId::new();
        let when = Utc::now() - age;
        let rec = SessionRecord {
            id,
            topic: topic.to_string(),
            repo_path: repo.to_path_buf(),
            created_at: when,
            layout_mode: LayoutMode::Tiled,
            panels: Vec::new(),
            role_counts: std::collections::HashMap::new(),
        };
        rec.write(&session_root(repo, id)).unwrap();
        set_record_mtime(repo, id, when);
        id
    }

    /// Backdate a session's `session.json` mtime — the activity timestamp gc
    /// ages by — to `when`, independent of the record's `created_at`.
    fn set_record_mtime(repo: &Path, id: SessionId, when: DateTime<Utc>) {
        let path = SessionRecord::path(&session_root(repo, id));
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(std::time::SystemTime::from(when))
            .unwrap();
    }

    #[test]
    fn plan_selects_only_old_unlocked_sessions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path();
        let old = write_session(repo, "old", Duration::days(30));
        let _fresh = write_session(repo, "fresh", Duration::hours(1));

        let plan = plan(repo, Duration::days(7), Utc::now());
        assert_eq!(
            plan.prunable.len(),
            1,
            "only the 30-day session is old enough"
        );
        assert_eq!(plan.prunable[0].id, old);
        assert!(plan.skipped_live.is_empty());
    }

    #[test]
    fn plan_ages_by_session_json_mtime_not_created_at() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path();
        // Created 30 days ago, but resumed (session.json rewritten) an hour ago.
        let id = write_session(repo, "long-lived", Duration::days(30));
        set_record_mtime(repo, id, Utc::now() - Duration::hours(1));

        let plan = plan(repo, Duration::days(7), Utc::now());
        assert!(
            plan.prunable.is_empty(),
            "a session resumed within the window is fresh by mtime, not stale by created_at"
        );
    }

    #[test]
    fn plan_skips_a_session_a_live_caucus_holds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path();
        let id = write_session(repo, "running", Duration::days(30));

        // Stand in for the live caucus owning this session.
        let _held = SessionLock::acquire(&session_root(repo, id)).unwrap();

        let plan = plan(repo, Duration::days(7), Utc::now());
        assert!(plan.prunable.is_empty(), "a held session is not prunable");
        assert_eq!(plan.skipped_live, vec![id]);
    }

    #[test]
    fn execute_removes_the_session_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path();
        let id = write_session(repo, "stale", Duration::days(30));
        let root = session_root(repo, id);
        assert!(root.is_dir());

        let plan = plan(repo, Duration::days(7), Utc::now());
        let report = execute(&plan);

        assert_eq!(report.removed, vec![id]);
        assert!(report.failed.is_empty());
        assert!(report.raced.is_empty());
        assert!(!root.exists(), "the session directory is gone");
    }

    #[test]
    fn execute_does_not_remove_a_session_claimed_after_planning() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path();
        let id = write_session(repo, "raced", Duration::days(30));
        let root = session_root(repo, id);

        // Plan while free, then a caucus grabs the lock before execute runs.
        let plan = plan(repo, Duration::days(7), Utc::now());
        let _claimed = SessionLock::acquire(&root).unwrap();

        let report = execute(&plan);
        assert_eq!(
            report.raced,
            vec![id],
            "the now-running session is left alone"
        );
        assert!(report.removed.is_empty());
        assert!(root.is_dir(), "its state directory survives");
    }

    #[test]
    fn parse_retention_handles_units_and_bare_days() {
        assert_eq!(parse_retention("30s").unwrap(), Duration::seconds(30));
        assert_eq!(parse_retention("15m").unwrap(), Duration::minutes(15));
        assert_eq!(parse_retention("24h").unwrap(), Duration::hours(24));
        assert_eq!(parse_retention("7d").unwrap(), Duration::days(7));
        assert_eq!(parse_retention("2w").unwrap(), Duration::weeks(2));
        // A bare integer is days.
        assert_eq!(parse_retention("10").unwrap(), Duration::days(10));
        // Whitespace is tolerated.
        assert_eq!(parse_retention("  3d ").unwrap(), Duration::days(3));
    }

    #[test]
    fn parse_retention_rejects_bad_specs() {
        for bad in ["", "d", "7x", "-5d", "1.5h", "abc"] {
            assert!(parse_retention(bad).is_err(), "`{bad}` must be rejected");
        }
    }
}
