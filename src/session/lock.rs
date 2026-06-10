//! Single-instance lock on a session root (`docs/design.md` §3).
//!
//! A session's on-disk state — `session.json`, `pending-rounds.json`, per-panel
//! logs, and the worktrees under `sessions/<id>/` — has a single in-memory
//! owner (the [`crate::session::Multiplexer`]). Two caucus processes opening the
//! *same* session would race those files and double-drive the same worktrees, so
//! each acquires an exclusive advisory lock on the session root and holds it for
//! the process lifetime.
//!
//! The lock is an OS advisory lock (`flock`-style) on `<root>/lock`, released by
//! the kernel when the process exits — including a crash — so a dead caucus
//! never leaves a stale lock that blocks `caucus resume`.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// An exclusive advisory lock on a session root, held for as long as the value
/// is alive. Dropping it (or the process exiting) releases the lock.
#[derive(Debug)]
pub struct SessionLock {
    /// The locked file. Holding it open keeps the advisory lock alive; the
    /// handle is otherwise never read again. The leading underscore documents
    /// that it exists purely for its `Drop`.
    _file: File,
}

/// Why acquiring a [`SessionLock`] failed.
#[derive(Debug, Error)]
pub enum SessionLockError {
    /// Another live caucus process already holds the lock.
    #[error("session is already open in another caucus process (lock held: {lock_path})")]
    AlreadyRunning { lock_path: PathBuf },
    /// The lock file could not be opened or locked.
    #[error("session lock io ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl SessionLock {
    /// Acquire the exclusive lock on `<session_root>/lock`, failing fast with
    /// [`SessionLockError::AlreadyRunning`] when another live caucus already
    /// holds it. `session_root` must already exist.
    pub fn acquire(session_root: &Path) -> Result<Self, SessionLockError> {
        let path = session_root.join("lock");
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| SessionLockError::Io {
                path: path.clone(),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => {
                // Stamp our PID for a human inspecting a held lock — purely
                // diagnostic, so any failure here is ignored.
                let _ = file.set_len(0);
                let _ = (&file).write_all(format!("{}\n", std::process::id()).as_bytes());
                Ok(Self { _file: file })
            }
            Err(TryLockError::WouldBlock) => {
                Err(SessionLockError::AlreadyRunning { lock_path: path })
            }
            Err(TryLockError::Error(source)) => Err(SessionLockError::Io { path, source }),
        }
    }

    /// Whether a live caucus process currently holds the lock on `session_root`.
    ///
    /// A read-only probe for `caucus gc`, which must never prune a session
    /// another caucus is actively running. It opens the *existing* `<root>/lock`
    /// without creating it — a session with no lock file (never opened, or
    /// opened before this lock existed) reads as not held — and tries the
    /// advisory lock; a `WouldBlock` means a live owner holds it. The momentary
    /// lock we take on success is released immediately when `file` drops here.
    ///
    /// On any I/O ambiguity it returns `true`: gc fails safe toward *keeping* a
    /// session rather than risk deleting one that is running.
    pub fn is_held(session_root: &Path) -> bool {
        let path = session_root.join("lock");
        let file = match OpenOptions::new().write(true).open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
            // Can't even open the lock file — treat as held (fail safe).
            Err(_) => return true,
        };
        match file.try_lock() {
            // We acquired it, so no live owner held it; the lock releases as
            // `file` drops at the end of this scope.
            Ok(()) => false,
            Err(TryLockError::WouldBlock) => true,
            // Ambiguous lock error — fail safe toward "held".
            Err(TryLockError::Error(_)) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquire_succeeds_on_a_free_root() {
        let tmp = TempDir::new().unwrap();
        let lock = SessionLock::acquire(tmp.path());
        assert!(lock.is_ok(), "a free root locks cleanly: {lock:?}");
        // The lock file carries our PID for diagnostics.
        let body = std::fs::read_to_string(tmp.path().join("lock")).unwrap();
        assert_eq!(body.trim(), std::process::id().to_string());
    }

    #[test]
    fn second_acquire_while_held_is_refused() {
        let tmp = TempDir::new().unwrap();
        let _held = SessionLock::acquire(tmp.path()).unwrap();
        // A second acquire on the same root — a stand-in for a second caucus
        // process — must be refused, not block or succeed.
        match SessionLock::acquire(tmp.path()) {
            Err(SessionLockError::AlreadyRunning { .. }) => {}
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
    }

    #[test]
    fn lock_is_released_on_drop() {
        let tmp = TempDir::new().unwrap();
        {
            let _held = SessionLock::acquire(tmp.path()).unwrap();
        } // released here
        // Re-acquiring after the prior owner dropped must succeed — the model
        // for `caucus resume` after the original process exits.
        assert!(SessionLock::acquire(tmp.path()).is_ok());
    }

    #[test]
    fn is_held_reflects_a_live_owner() {
        let tmp = TempDir::new().unwrap();
        // No lock file yet → not held (a session gc may freely prune).
        assert!(!SessionLock::is_held(tmp.path()));
        {
            let _held = SessionLock::acquire(tmp.path()).unwrap();
            // While a live owner holds it, the probe reports held.
            assert!(SessionLock::is_held(tmp.path()));
        }
        // After the owner drops, the (now stale) lock file remains on disk but
        // the probe must report not-held — the model for gc pruning the state
        // of a session whose caucus has exited.
        assert!(tmp.path().join("lock").exists());
        assert!(!SessionLock::is_held(tmp.path()));
    }
}
