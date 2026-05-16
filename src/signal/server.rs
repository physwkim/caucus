//! Turn-signal socket server.
//!
//! **Invariant I-6** (`docs/design.md` §12): turn signals arriving on the
//! socket are parsed and applied to manifests *only* by [`ingest`]. The
//! `UnixListener` lives exclusively inside this module; no other module reads
//! the socket.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::net::UnixListener;
use tokio::sync::mpsc;

use super::TurnSignal;

/// Errors from the turn-signal server.
#[derive(Debug, Error)]
pub enum SignalServerError {
    #[error("signal socket io ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("signal payload json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Handle to the running turn-signal server.
///
/// The server owns the `UnixListener`; consumers receive parsed
/// [`TurnSignal`]s via [`SignalServer::signals`].
pub struct SignalServer {
    sock_path: PathBuf,
    /// The bound listener. The accept loop reading it is wired in phase 2.
    _listener: UnixListener,
    rx: mpsc::UnboundedReceiver<TurnSignal>,
}

impl SignalServer {
    /// Bind the unix-domain socket at `sock_path` and spawn the accept loop.
    ///
    /// Path shape: `<repo>/.caucus/sessions/<session_id>/caucus.sock`.
    pub fn bind(sock_path: &Path) -> Result<Self, SignalServerError> {
        // Remove any stale socket, then bind. The accept loop — forwarding
        // each connection's payload through `ingest` — is wired in phase 2.
        let _ = std::fs::remove_file(sock_path);
        let listener = UnixListener::bind(sock_path).map_err(|source| SignalServerError::Io {
            path: sock_path.to_path_buf(),
            source,
        })?;
        // TODO(phase 2): `tokio::spawn` the accept loop over `listener`,
        // calling `ingest` per line and forwarding parsed signals on `tx`.
        let (_tx, rx) = mpsc::unbounded_channel();
        Ok(Self {
            sock_path: sock_path.to_path_buf(),
            _listener: listener,
            rx,
        })
    }

    /// Path the server is (or will be) bound to.
    pub fn sock_path(&self) -> &Path {
        &self.sock_path
    }

    /// Stream of parsed turn signals. Consumers call `ingest` per signal.
    pub fn signals(&mut self) -> &mut mpsc::UnboundedReceiver<TurnSignal> {
        &mut self.rx
    }
}

/// Single owner of turn-signal ingestion (Invariant I-6).
///
/// Parses one line of socket JSON into a [`TurnSignal`] and is the only path
/// by which a turn signal reaches the rest of caucus (manifest append,
/// derived-state recompute).
pub(crate) fn ingest(line: &str) -> Result<TurnSignal, SignalServerError> {
    let signal: TurnSignal = serde_json::from_str(line)?;
    // TODO(phase 2): append a `TurnCompleted` LaneEvent to the panel manifest
    // and recompute derived_state via `agent::derive_state`.
    Ok(signal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::id::{PanelId, SessionId};
    use crate::signal::TurnKind;

    #[test]
    fn ingest_parses_a_turn_signal_line() {
        let session_id = SessionId::new();
        let panel_id = PanelId::new();
        let line = serde_json::json!({
            "session_id": session_id,
            "panel_id": panel_id,
            "ts": "2026-05-16T14:23:01Z",
            "kind": "stop",
            "last_message": "done",
            "raw_hook_payload": {}
        })
        .to_string();
        let sig = ingest(&line).unwrap();
        assert_eq!(sig.kind, TurnKind::Stop);
        assert_eq!(sig.last_message.as_deref(), Some("done"));
    }
}
