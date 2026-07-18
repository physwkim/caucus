//! Turn-signal socket server.
//!
//! **Invariant I-6** (`docs/design.md` §12): turn signals arriving on the
//! socket are parsed and applied to manifests *only* by `ingest`. The
//! `UnixListener` lives exclusively inside this module; no other module reads
//! the socket.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tracing::warn;

use super::SignalEvent;
use crate::line_io::{CappedLine, MAX_IPC_LINE_BYTES, read_capped_line};

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
/// [`SignalEvent`]s via [`SignalServer::signals`].
pub struct SignalServer {
    sock_path: PathBuf,
    rx: mpsc::UnboundedReceiver<SignalEvent>,
}

impl SignalServer {
    /// Bind the unix-domain socket at `sock_path` and spawn the accept loop.
    ///
    /// `sock_path` is caller-supplied: `runtime::socket_path` picks a
    /// `SUN_LEN`-safe name in the system temp dir (a `docs/design.md` §7.1
    /// session-dir path easily overruns the ~104-byte OS cap) and conveys it to
    /// agents via the `CAUCUS_SOCK` env var — the location is an internal
    /// detail, not a fixed path shape.
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

    /// Stream of parsed signal events — turn signals and mid-turn notes.
    pub fn signals(&mut self) -> &mut mpsc::UnboundedReceiver<SignalEvent> {
        &mut self.rx
    }
}

impl Drop for SignalServer {
    /// Remove the bound socket file on shutdown. As with the control socket,
    /// `bind` only clears a stale file at startup, so each run otherwise left
    /// its `caucus-<id>.sock` behind in the temp dir. The accept-loop task is
    /// torn down with the tokio runtime at process exit.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

/// Accept connections forever, handling each on its own task so a slow or
/// stalled client never blocks other agents' turn signals.
///
/// Runs until the listener errors unrecoverably or every receiver for `tx`
/// is dropped; the listener is owned here and nowhere else (Invariant I-6).
async fn accept_loop(listener: UnixListener, tx: mpsc::UnboundedSender<SignalEvent>) {
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
/// [`ingest`] and forwarding the resulting [`SignalEvent`]s on `tx`.
///
/// A malformed line is dropped (the connection continues); a closed receiver
/// ends the connection early.
async fn handle_connection(stream: UnixStream, tx: mpsc::UnboundedSender<SignalEvent>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        // Bounded read: a peer cannot OOM the listener with a newline-less
        // flood (`line_io`). A line over the cap desyncs the stream, so we stop.
        match read_capped_line(&mut reader, &mut line, MAX_IPC_LINE_BYTES).await {
            Ok(CappedLine::Line) => {
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
            Ok(CappedLine::TooLong) => {
                warn!("turn-signal line exceeded {MAX_IPC_LINE_BYTES} bytes; closing connection");
                return;
            }
            // Clean EOF or a read error: the client is done with us.
            Ok(CappedLine::Eof) | Err(_) => return,
        }
    }
}

/// Single owner of signal ingestion (Invariant I-6).
///
/// Parses one line of socket JSON into a [`SignalEvent`] — a turn signal or a
/// mid-turn note — and is the only path by which either reaches the rest of
/// caucus.
///
/// Ingestion is parse-only by design: applying an event — appending a lane
/// event and (for a turn signal) recomputing `derived_state` — requires the
/// per-panel [`crate::agent::AgentManifest`], whose single owner is the
/// [`crate::session::Multiplexer`] (Invariant I-2). The server forwards the
/// parsed event; `Multiplexer::handle_signal` / `Multiplexer::handle_note`
/// apply it through the manifest owners. Splitting it this way keeps the
/// socket listener free of any manifest dependency.
pub(crate) fn ingest(line: &str) -> Result<SignalEvent, SignalServerError> {
    let event: SignalEvent = serde_json::from_str(line)?;
    Ok(event)
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
        let SignalEvent::Turn(sig) = ingest(&line).unwrap() else {
            panic!("a turn-signal line must ingest as Turn");
        };
        assert_eq!(sig.kind, TurnKind::Stop);
        assert_eq!(sig.last_message.as_deref(), Some("done"));
        // The line predates `transcript_path` (an old `caucus` binary posted
        // it): the field defaults to None rather than failing the parse.
        assert_eq!(sig.transcript_path, None);
    }

    /// A mid-turn note line ingests as `Note` — the untagged discrimination
    /// cannot confuse it with a turn signal (disjoint `kind` vocabularies,
    /// `body` vs `raw_hook_payload`).
    #[test]
    fn ingest_parses_an_agent_note_line() {
        use crate::signal::NoteKind;
        let session_id = SessionId::new();
        let panel_id = PanelId::new();
        let line = serde_json::json!({
            "session_id": session_id,
            "panel_id": panel_id,
            "ts": "2026-07-18T09:00:00Z",
            "kind": "question",
            "body": "which API version should the new endpoint target?"
        })
        .to_string();
        let SignalEvent::Note(note) = ingest(&line).unwrap() else {
            panic!("a note line must ingest as Note");
        };
        assert_eq!(note.kind, NoteKind::Question);
        assert_eq!(
            note.body,
            "which API version should the new endpoint target?"
        );
    }

    /// Dropping the server removes its socket file, so it does not accumulate
    /// in the temp dir across runs.
    #[tokio::test]
    async fn drop_removes_the_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("caucus.sock");
        {
            let _server = SignalServer::bind(&sock).unwrap();
            assert!(sock.exists(), "bind creates the socket");
        }
        assert!(!sock.exists(), "drop removes the socket");
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

        let SignalEvent::Turn(sig) = server.signals().recv().await.expect("signal received") else {
            panic!("expected a turn signal");
        };
        assert_eq!(sig.session_id, session_id);
        assert_eq!(sig.panel_id, panel_id);
        assert_eq!(sig.kind, TurnKind::Stop);
        assert_eq!(sig.last_message.as_deref(), Some("raw write"));
    }

    /// One connection interleaving a note line, a malformed line, and a turn
    /// line delivers `Note` then `Turn` in order — the mixed stream a live
    /// session produces when a sub posts mid-turn notes while another's Stop
    /// hook fires on the same socket.
    #[tokio::test]
    async fn server_forwards_interleaved_note_and_turn_lines() {
        use crate::signal::NoteKind;
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("caucus.sock");
        let mut server = SignalServer::bind(&sock).unwrap();

        let session_id = SessionId::new();
        let panel_id = PanelId::new();
        let note = serde_json::json!({
            "session_id": session_id,
            "panel_id": panel_id,
            "ts": "2026-07-18T09:00:00Z",
            "kind": "progress",
            "body": "halfway through the sweep"
        })
        .to_string();
        let turn = serde_json::json!({
            "session_id": session_id,
            "panel_id": panel_id,
            "ts": "2026-07-18T09:01:00Z",
            "kind": "stop",
            "last_message": "done",
            "raw_hook_payload": {}
        })
        .to_string();

        let mut stream = UnixStream::connect(&sock).await.unwrap();
        let payload = format!("{note}\nnot json\n{turn}\n");
        stream.write_all(payload.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();

        let SignalEvent::Note(got) = server.signals().recv().await.expect("note received") else {
            panic!("first line must arrive as Note");
        };
        assert_eq!(got.kind, NoteKind::Progress);
        assert_eq!(got.body, "halfway through the sweep");

        let SignalEvent::Turn(sig) = server.signals().recv().await.expect("turn received") else {
            panic!("last line must arrive as Turn");
        };
        assert_eq!(sig.kind, TurnKind::Stop);
        assert_eq!(sig.last_message.as_deref(), Some("done"));
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

        let SignalEvent::Turn(sig) = server.signals().recv().await.expect("signal received") else {
            panic!("expected a turn signal");
        };
        assert_eq!(sig.kind, TurnKind::Error);
        assert_eq!(sig.session_id, session_id);
    }
}
