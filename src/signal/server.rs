//! Turn-signal socket server.
//!
//! **Invariant I-6** (`docs/design.md` §12): turn signals arriving on the
//! socket are parsed and applied to manifests *only* by `ingest`. The
//! `UnixListener` lives exclusively inside this module; no other module reads
//! the socket.

use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use super::{SignalEnvelope, SignalEvent, SignalReply, StopDirective};
use crate::line_io::{CappedLine, MAX_IPC_LINE_BYTES, read_capped_line};

/// How long the server holds a `wants_reply` connection open for the runtime's
/// [`StopDirective`] before answering [`SignalReply::Allow`] on its own — the
/// server half of the reply-timeout ladder (`docs/design.md` §7.6): below the
/// client's read timeout (`post::HOOK_REPLY_READ_TIMEOUT`, 2500ms) so the
/// normal fail-open path is the server's explicit allow line, not the client's
/// socket timeout; both far below Claude's hook timeout (600s default). The
/// runtime normally answers within one event-loop tick (4–20ms).
pub(crate) const HOOK_REPLY_WAIT: Duration = Duration::from_millis(1500);

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
/// [`SignalEnvelope`]s via [`SignalServer::signals`].
pub struct SignalServer {
    sock_path: PathBuf,
    rx: mpsc::UnboundedReceiver<SignalEnvelope>,
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

    /// Stream of parsed signal envelopes — turn signals and mid-turn notes,
    /// each paired with its reply slot (`Some` only for a `wants_reply` turn
    /// signal; dropping that sender answers the connection with allow).
    pub fn signals(&mut self) -> &mut mpsc::UnboundedReceiver<SignalEnvelope> {
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
async fn accept_loop(listener: UnixListener, tx: mpsc::UnboundedSender<SignalEnvelope>) {
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
/// [`ingest`] and forwarding the resulting events on `tx`.
///
/// A malformed line is dropped (the connection continues); a closed receiver
/// ends the connection early. A turn signal that asks for a reply
/// (`TurnSignal::wants_reply`) is forwarded with a oneshot sender, and the
/// connection is answered with one [`SignalReply`] line — the directive the
/// runtime sent, or allow when the sender is dropped or [`HOOK_REPLY_WAIT`]
/// expires. The server is the reply line's only writer, the same way `ingest`
/// is the request line's only reader (Invariant I-6).
async fn handle_connection(stream: UnixStream, tx: mpsc::UnboundedSender<SignalEnvelope>) {
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
                    Ok(event) => {
                        // An unbound signal always waits for a reply: its
                        // poster broadcasts to every live caucus socket and
                        // reads one line per socket to learn whether a deliver
                        // directive rides back (`docs/design.md` §7.8). The
                        // runtime answers a signal it does not own by dropping
                        // the sender, which replies allow immediately.
                        let wants_reply = matches!(
                            &event,
                            SignalEvent::Turn(sig) if sig.wants_reply
                        ) || matches!(&event, SignalEvent::Unbound(_));
                        if !wants_reply {
                            if tx.send((event, None)).is_err() {
                                // No consumer left; nothing more to do.
                                return;
                            }
                            continue;
                        }
                        let (reply_tx, reply_rx) = oneshot::channel();
                        if tx.send((event, Some(reply_tx))).is_err() {
                            // No consumer left. The client's own read timeout
                            // resolves its wait (fail-open to allow).
                            return;
                        }
                        let reply = match tokio::time::timeout(HOOK_REPLY_WAIT, reply_rx).await {
                            Ok(Ok(StopDirective::Deliver { reason })) => {
                                SignalReply::Deliver { reason }
                            }
                            // Sender dropped (every non-deliver path in the
                            // runtime) or the wait expired: allow.
                            Ok(Err(_)) | Err(_) => SignalReply::Allow,
                        };
                        let mut out =
                            serde_json::to_string(&reply).expect("SignalReply serialises");
                        out.push('\n');
                        if reader.get_mut().write_all(out.as_bytes()).await.is_err() {
                            // Client gone (it timed out and exited): its half
                            // of the fail-open ladder already resolved.
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

    /// Lifecycle lines — exactly what `caucus signal post --kind post-compact /
    /// session-start` serialises — ingest as `Lifecycle`, and a round-trip
    /// through the serializer parses back to the same kind. The untagged
    /// discrimination cannot confuse them with turn signals or notes:
    /// `post_compact` / `session_start` are outside both other vocabularies.
    #[test]
    fn ingest_parses_lifecycle_signal_lines() {
        use crate::signal::{CompactTrigger, LifecycleKind, LifecycleSignal};
        let session_id = SessionId::new();
        let panel_id = PanelId::new();

        let line = serde_json::json!({
            "session_id": session_id,
            "panel_id": panel_id,
            "ts": "2026-07-21T05:00:00Z",
            "kind": "post_compact",
            "trigger": "manual",
            "raw_hook_payload": { "trigger": "manual" }
        })
        .to_string();
        let SignalEvent::Lifecycle(sig) = ingest(&line).unwrap() else {
            panic!("a post_compact line must ingest as Lifecycle");
        };
        assert_eq!(
            sig.kind,
            LifecycleKind::PostCompact {
                trigger: CompactTrigger::Manual
            }
        );

        let line = serde_json::json!({
            "session_id": session_id,
            "panel_id": panel_id,
            "ts": "2026-07-21T05:00:01Z",
            "kind": "session_start",
            "source": "compact",
            "raw_hook_payload": {}
        })
        .to_string();
        let SignalEvent::Lifecycle(sig) = ingest(&line).unwrap() else {
            panic!("a session_start line must ingest as Lifecycle");
        };
        assert_eq!(
            sig.kind,
            LifecycleKind::SessionStart {
                source: "compact".into()
            }
        );

        // Round-trip: what `LifecycleSignal` serialises is what ingest parses.
        let posted = LifecycleSignal::now(
            session_id,
            panel_id,
            LifecycleKind::SessionStart {
                source: "clear".into(),
            },
            serde_json::Value::Null,
        );
        let line = serde_json::to_string(&posted).unwrap();
        let SignalEvent::Lifecycle(back) = ingest(&line).unwrap() else {
            panic!("a serialised LifecycleSignal must ingest as Lifecycle");
        };
        assert_eq!(back.kind, posted.kind);
        assert_eq!(back.panel_id, panel_id);
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

        let (event, reply) = server.signals().recv().await.expect("signal received");
        let SignalEvent::Turn(sig) = event else {
            panic!("expected a turn signal");
        };
        assert!(
            reply.is_none(),
            "a line without wants_reply carries no reply slot"
        );
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

        let (event, _) = server.signals().recv().await.expect("note received");
        let SignalEvent::Note(got) = event else {
            panic!("first line must arrive as Note");
        };
        assert_eq!(got.kind, NoteKind::Progress);
        assert_eq!(got.body, "halfway through the sweep");

        let (event, _) = server.signals().recv().await.expect("turn received");
        let SignalEvent::Turn(sig) = event else {
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

        let (event, _) = server.signals().recv().await.expect("signal received");
        let SignalEvent::Turn(sig) = event else {
            panic!("expected a turn signal");
        };
        assert_eq!(sig.kind, TurnKind::Error);
        assert_eq!(sig.session_id, session_id);
    }

    /// A `wants_reply` turn-signal line for the reply tests below.
    fn wants_reply_line(session_id: SessionId, panel_id: PanelId) -> String {
        serde_json::json!({
            "session_id": session_id,
            "panel_id": panel_id,
            "ts": "2026-07-18T09:00:00Z",
            "kind": "stop",
            "last_message": "main turn ended",
            "wants_reply": true,
            "raw_hook_payload": {}
        })
        .to_string()
    }

    /// Read one reply line off the client end of the socket.
    async fn read_reply_line(stream: &mut UnixStream) -> String {
        use tokio::io::AsyncBufReadExt;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        line
    }

    /// A `wants_reply` signal is forwarded with a live reply slot, and the
    /// runtime's `Deliver` directive comes back to the client as the deliver
    /// reply line.
    #[tokio::test]
    async fn server_replies_deliver_with_the_runtime_directive() {
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("caucus.sock");
        let mut server = SignalServer::bind(&sock).unwrap();

        let mut stream = UnixStream::connect(&sock).await.unwrap();
        let line = wants_reply_line(SessionId::new(), PanelId::new());
        stream.write_all(line.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();

        let (event, reply) = server.signals().recv().await.expect("signal received");
        assert!(matches!(event, SignalEvent::Turn(sig) if sig.wants_reply));
        let sender = reply.expect("a wants_reply signal carries a reply slot");
        sender
            .send(StopDirective::Deliver {
                reason: "round 7 complete".into(),
            })
            .expect("server holds the receiver");

        let got = read_reply_line(&mut stream).await;
        let parsed: SignalReply = serde_json::from_str(got.trim()).unwrap();
        let SignalReply::Deliver { reason } = parsed else {
            panic!("expected a deliver reply, got: {got}");
        };
        assert_eq!(reason, "round 7 complete");
    }

    /// Dropping the reply sender — the runtime's every non-deliver path — is
    /// answered with an explicit allow line, so the client exits without
    /// waiting out its read timeout.
    #[tokio::test]
    async fn server_replies_allow_when_the_sender_is_dropped() {
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("caucus.sock");
        let mut server = SignalServer::bind(&sock).unwrap();

        let mut stream = UnixStream::connect(&sock).await.unwrap();
        let line = wants_reply_line(SessionId::new(), PanelId::new());
        stream.write_all(line.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();

        let (_event, reply) = server.signals().recv().await.expect("signal received");
        drop(reply.expect("a wants_reply signal carries a reply slot"));

        let got = read_reply_line(&mut stream).await;
        assert!(
            matches!(serde_json::from_str(got.trim()), Ok(SignalReply::Allow)),
            "a dropped sender must answer allow, got: {got}"
        );
    }

    /// An unbound line always carries a reply slot — the discovering poster
    /// waits for one reply line per socket — and dropping it (a server that
    /// resolves the signal to no panel, or to a non-main panel) answers allow.
    #[tokio::test]
    async fn server_gives_unbound_signals_a_reply_slot() {
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("caucus.sock");
        let mut server = SignalServer::bind(&sock).unwrap();

        let mut stream = UnixStream::connect(&sock).await.unwrap();
        let unbound =
            crate::signal::UnboundSignal::now(TurnKind::Stop, None, serde_json::json!({}));
        let line = serde_json::to_string(&unbound).unwrap();
        stream.write_all(line.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();

        let (event, reply) = server.signals().recv().await.expect("signal received");
        assert!(matches!(event, SignalEvent::Unbound(_)));
        drop(reply.expect("an unbound signal always carries a reply slot"));

        let got = read_reply_line(&mut stream).await;
        assert!(
            matches!(serde_json::from_str(got.trim()), Ok(SignalReply::Allow)),
            "an unclaimed unbound signal must answer allow, got: {got}"
        );
    }

    /// A runtime that holds the sender without answering is cut off at
    /// [`HOOK_REPLY_WAIT`]: the server answers allow on its own. Paused tokio
    /// time auto-advances the wait, so the test does not sleep for real.
    #[tokio::test(start_paused = true)]
    async fn server_replies_allow_when_the_wait_times_out() {
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("caucus.sock");
        let mut server = SignalServer::bind(&sock).unwrap();

        let mut stream = UnixStream::connect(&sock).await.unwrap();
        let line = wants_reply_line(SessionId::new(), PanelId::new());
        stream.write_all(line.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();

        let (_event, reply) = server.signals().recv().await.expect("signal received");
        // Hold the sender: the runtime never answers.
        let _held = reply.expect("a wants_reply signal carries a reply slot");

        let got = read_reply_line(&mut stream).await;
        assert!(
            matches!(serde_json::from_str(got.trim()), Ok(SignalReply::Allow)),
            "an unanswered wait must time out to allow, got: {got}"
        );
    }
}
