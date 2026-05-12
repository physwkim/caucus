//! Watch a session directory for new/changed sentinel files. Built on the
//! `notify` crate (inotify on Linux, FSEvents on macOS, ReadDirectoryChangesW
//! on Windows). Each detected change is parsed and forwarded over a tokio
//! channel as a `WatchEvent`.
//!
//! This module is **read-only** with respect to sentinel files (Invariant I-5
//! in `docs/design.md` §12). Writes go through `super::writer`.

use std::path::{Path, PathBuf};

use notify::{Event, EventKind, RecursiveMode, Watcher};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::writer::{Sentinel, SentinelError};

/// One event surfaced to the orchestrator.
#[derive(Debug)]
pub enum WatchEvent {
    /// A sentinel JSON was successfully read.
    Sentinel { path: PathBuf, sentinel: Sentinel },
    /// A change was detected but the file could not be parsed (yet).
    /// Almost always transient: the writer is mid-`rename`. The caller may
    /// re-poll the path once.
    ParseDeferred { path: PathBuf, reason: String },
    /// The underlying watcher itself errored. Watcher is still alive.
    WatcherError { message: String },
}

#[derive(Debug, Error)]
pub enum WatcherError {
    #[error("notify start failed: {0}")]
    Start(#[from] notify::Error),
    #[error("watch path does not exist: {0}")]
    MissingPath(PathBuf),
    #[error(transparent)]
    Sentinel(#[from] SentinelError),
}

/// Handle to a running watcher. Dropping it stops the underlying notify
/// thread. Events are received on the paired `mpsc::UnboundedReceiver`.
pub struct SentinelWatcher {
    _inner: notify::RecommendedWatcher,
}

/// Start a watcher rooted at `agents_dir` (which is normally
/// `<session_root>/agents/`).
pub fn watch(
    agents_dir: &Path,
) -> Result<(SentinelWatcher, mpsc::UnboundedReceiver<WatchEvent>), WatcherError> {
    if !agents_dir.exists() {
        return Err(WatcherError::MissingPath(agents_dir.to_path_buf()));
    }

    let (tx, rx) = mpsc::unbounded_channel::<WatchEvent>();

    let tx_for_cb = tx.clone();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<Event, notify::Error>| match res {
            Ok(event) => handle_event(&tx_for_cb, event),
            Err(err) => {
                let _ = tx_for_cb.send(WatchEvent::WatcherError {
                    message: err.to_string(),
                });
            }
        })?;

    watcher.watch(agents_dir, RecursiveMode::NonRecursive)?;
    Ok((SentinelWatcher { _inner: watcher }, rx))
}

fn handle_event(tx: &mpsc::UnboundedSender<WatchEvent>, event: Event) {
    if !is_relevant(event.kind) {
        return;
    }
    for path in event.paths {
        if !looks_like_sentinel(&path) {
            continue;
        }
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Sentinel>(&bytes) {
                Ok(sentinel) => {
                    let _ = tx.send(WatchEvent::Sentinel { path, sentinel });
                }
                Err(err) => {
                    debug!(?path, %err, "sentinel parse failed (likely mid-write)");
                    let _ = tx.send(WatchEvent::ParseDeferred {
                        path,
                        reason: err.to_string(),
                    });
                }
            },
            Err(err) => {
                // ENOENT is normal: rename completed *before* we read.
                if err.kind() != std::io::ErrorKind::NotFound {
                    warn!(?path, %err, "sentinel read failed");
                }
            }
        }
    }
}

fn is_relevant(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any
    )
}

fn looks_like_sentinel(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.ends_with(".sentinel.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sentinel::writer::{Sentinel, SentinelKind, write_sentinel};
    use crate::session::id::{AgentId, SessionId};
    use tempfile::TempDir;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn watcher_picks_up_write() {
        let tmp = TempDir::new().unwrap();
        let agents = tmp.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();

        let (_w, mut rx) = watch(&agents).unwrap();

        let session = SessionId::new();
        let agent = AgentId::new();
        let sentinel = Sentinel::new(session, agent, SentinelKind::Stop, Some("ok".into()), None);
        write_sentinel(tmp.path(), &sentinel).unwrap();

        // Allow up to 2s for notify to flush; on macOS FSEvents can coalesce
        // events with a small delay.
        let event = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("watcher should fire")
            .expect("channel still open");
        match event {
            WatchEvent::Sentinel { sentinel: s, .. } => {
                assert_eq!(s.agent_id, agent);
                assert_eq!(s.kind, SentinelKind::Stop);
            }
            WatchEvent::ParseDeferred { reason, .. } => {
                // On some platforms we may observe an intermediate write
                // first; that's allowed as long as a final parse succeeds.
                let final_event = timeout(Duration::from_secs(2), rx.recv())
                    .await
                    .expect("watcher should re-fire after rename")
                    .expect("channel still open");
                assert!(
                    matches!(final_event, WatchEvent::Sentinel { .. }),
                    "expected eventual Sentinel after ParseDeferred({reason})"
                );
            }
            WatchEvent::WatcherError { message } => panic!("watcher errored: {message}"),
        }
    }

    #[tokio::test]
    async fn missing_path_returns_error() {
        let tmp = TempDir::new().unwrap();
        let bad = tmp.path().join("nope");
        let err = watch(&bad).err().expect("should error");
        assert!(matches!(err, WatcherError::MissingPath(_)));
    }

    #[test]
    fn looks_like_sentinel_is_strict() {
        assert!(looks_like_sentinel(Path::new("a.sentinel.json")));
        assert!(!looks_like_sentinel(Path::new("a.sentinel.json.tmp")));
        assert!(!looks_like_sentinel(Path::new("manifest.json")));
        assert!(!looks_like_sentinel(Path::new("README.md")));
    }
}
