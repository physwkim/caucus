//! Turn-signal socket server.
//!
//! **Invariant I-6** (`docs/design.md` §12): turn signals arriving on the
//! socket are parsed and applied to manifests *only* by [`ingest`]. The
//! `UnixListener` lives exclusively inside this module; no other module reads
//! the socket.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
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
    rx: mpsc::UnboundedReceiver<TurnSignal>,
}

impl SignalServer {
    /// Bind the unix-domain socket at `sock_path` and spawn the accept loop.
    ///
    /// Path shape: `<repo>/.caucus/sessions/<session_id>/caucus.sock`.
    ///
    /// The spawned accept loop holds the `UnixListener` for the lifetime of
    /// the process; closing the returned [`SignalServer`]'s receiver does not
    /// stop it. Per Invariant I-6 the listener never leaves this module.
    pub fn bind(sock_path: &Path) -> Result<Self, SignalServerError> {
        // Remove any stale socket, then bind.
        let _ = std::fs::remove_file(sock_path);
        let listener = UnixListener::bind(sock_path).map_err(|source| SignalServerError::Io {
            path: sock_path.to_path_buf(),
            source,
        })?;
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(accept_loop(listener, tx));
        Ok(Self {
            sock_path: sock_path.to_path_buf(),
            rx,
        })
    }

    /// Path the server is bound to.
    pub fn sock_path(&self) -> &Path {
        &self.sock_path
    }

    /// Stream of parsed turn signals. Consumers call `ingest` per signal.
    pub fn signals(&mut self) -> &mut mpsc::UnboundedReceiver<TurnSignal> {
        &mut self.rx
    }
}

/// Accept connections forever, handling each on its own task so a slow or
/// stalled client never blocks other agents' turn signals.
///
/// Runs until the listener errors unrecoverably or every receiver for `tx`
/// is dropped; the listener is owned here and nowhere else (Invariant I-6).
async fn accept_loop(listener: UnixListener, tx: mpsc::UnboundedSender<TurnSignal>) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let tx = tx.clone();
                tokio::spawn(handle_connection(stream, tx));
            }
            Err(_) => {
                // Transient accept errors (e.g. EMFILE) are non-fatal: a
                // future connection may still succeed. Yield to avoid a hot
                // spin, then keep listening.
                tokio::task::yield_now().await;
            }
        }
    }
}

/// Read newline-delimited JSON from one connection, parsing each line via
/// [`ingest`] and forwarding the resulting [`TurnSignal`]s on `tx`.
///
/// A malformed line is dropped (the connection continues); a closed receiver
/// ends the connection early.
async fn handle_connection(stream: UnixStream, tx: mpsc::UnboundedSender<TurnSignal>) {
    let mut lines = BufReader::new(stream).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match ingest(&line) {
                    Ok(signal) => {
                        if tx.send(signal).is_err() {
                            // No consumer left; nothing more to do.
                            return;
                        }
                    }
                    Err(_) => {
                        // Malformed line: skip it, keep reading the stream.
                        continue;
                    }
                }
            }
            // Clean EOF or a read error: the client is done with us.
            Ok(None) | Err(_) => return,
        }
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

    /// A raw newline-delimited JSON write reaches the server's channel.
    #[tokio::test]
    async fn server_forwards_a_raw_socket_line() {
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("caucus.sock");
        let mut server = SignalServer::bind(&sock).unwrap();

        let session_id = SessionId::new();
        let panel_id = PanelId::new();
        let line = serde_json::json!({
            "session_id": session_id,
            "panel_id": panel_id,
            "ts": "2026-05-16T14:23:01Z",
            "kind": "stop",
            "last_message": "raw write",
            "raw_hook_payload": {}
        })
        .to_string();

        let mut stream = UnixStream::connect(&sock).await.unwrap();
        stream.write_all(line.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        stream.shutdown().await.unwrap();

        let sig = server.signals().recv().await.expect("signal received");
        assert_eq!(sig.session_id, session_id);
        assert_eq!(sig.panel_id, panel_id);
        assert_eq!(sig.kind, TurnKind::Stop);
        assert_eq!(sig.last_message.as_deref(), Some("raw write"));
    }

    /// A malformed line is skipped without killing the connection: a valid
    /// line on the same stream still arrives.
    #[tokio::test]
    async fn server_skips_malformed_line_and_keeps_reading() {
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("caucus.sock");
        let mut server = SignalServer::bind(&sock).unwrap();

        let session_id = SessionId::new();
        let panel_id = PanelId::new();
        let good = serde_json::json!({
            "session_id": session_id,
            "panel_id": panel_id,
            "ts": "2026-05-16T14:23:01Z",
            "kind": "error",
            "last_message": null,
            "raw_hook_payload": {}
        })
        .to_string();

        let mut stream = UnixStream::connect(&sock).await.unwrap();
        stream.write_all(b"not json\n").await.unwrap();
        stream.write_all(good.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        stream.shutdown().await.unwrap();

        let sig = server.signals().recv().await.expect("signal received");
        assert_eq!(sig.kind, TurnKind::Error);
        assert_eq!(sig.session_id, session_id);
    }
}
