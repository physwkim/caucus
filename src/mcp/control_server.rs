//! Control-socket server — the main `caucus` process side of the control
//! socket (`docs/design.md` §0 #4, §9).
//!
//! The main process opens this socket alongside the turn-signal socket. A
//! tokio accept task reads one [`ControlCall`] per connection, decodes it into
//! a [`ControlRequest`] and pushes a [`ControlJob`] onto an mpsc channel the
//! [`crate::session::Multiplexer`] event loop drains each tick. The
//! multiplexer executes the request against live panels and answers through
//! the job's oneshot reply channel; this module writes the [`ControlReply`]
//! back on the connection. A call that does not decode is answered here and
//! never reaches the multiplexer.
//!
//! Most requests are answered on the tick they are drained. Two are not:
//! `spawn_role(worktree=true)`, whose `git worktree add` is run off the event
//! loop (`session::runtime::spawn_async`), and `wait` on a busy panel
//! (`session::runtime::wait`). Their oneshot is held and answered on a later
//! tick. The accept task simply awaits the oneshot, so the delay is
//! transparent to this module.
//!
//! `subscribe_events` keeps its connection: the accept task subscribes to the
//! [`EventSubscriber`] *before* queuing the call, so every event after the
//! snapshot the multiplexer answers with is already in its receiver, then
//! streams the events past the snapshot's `seq` until the client closes the
//! connection (or its write side) or falls behind the ring.
//!
//! Why route through the event loop rather than touch panels here: the
//! `Multiplexer` is `!Send` single-owner state (it owns PTYs, grids). Control
//! requests must execute on the same thread that pumps panels — Invariant
//! I-5's single-owner discipline. The mpsc + oneshot pair is the hand-off.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use super::events::{EVENT_RING, EventSubscriber, PanelEvent};
use super::protocol::{
    ControlCall, ControlEvent, ControlReply, ControlRequest, ControlResponse, ErrorCode,
};
use crate::line_io::{CappedLine, MAX_IPC_LINE_BYTES, read_capped_line};

/// One queued control request plus the channel its [`ControlResponse`] is
/// returned on. The accept task creates these; the multiplexer consumes them.
pub struct ControlJob {
    /// The request to execute against the live multiplexer.
    pub request: ControlRequest,
    /// Oneshot the multiplexer sends the response back on.
    pub reply: oneshot::Sender<ControlResponse>,
}

/// Errors from the control-socket server.
#[derive(Debug, Error)]
pub enum ControlServerError {
    #[error("control socket io ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Handle to the running control-socket server.
///
/// The accept loop owns the [`UnixListener`]; the multiplexer drains queued
/// [`ControlJob`]s via [`ControlServer::jobs`].
pub struct ControlServer {
    sock_path: PathBuf,
    rx: mpsc::UnboundedReceiver<ControlJob>,
}

impl ControlServer {
    /// Bind the control socket at `sock_path` and spawn the accept loop;
    /// `subscribe_events` connections subscribe through `events`.
    ///
    /// A stale socket file at the path is removed first. The accept loop runs
    /// for the lifetime of the process.
    pub fn bind(sock_path: &Path, events: EventSubscriber) -> Result<Self, ControlServerError> {
        let _ = std::fs::remove_file(sock_path);
        let listener = UnixListener::bind(sock_path).map_err(|source| ControlServerError::Io {
            path: sock_path.to_path_buf(),
            source,
        })?;
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(accept_loop(listener, tx, events));
        Ok(Self {
            sock_path: sock_path.to_path_buf(),
            rx,
        })
    }

    /// Path the control socket is bound to — injected into the main worker panel's
    /// MCP config as `caucus mcp-serve --control-sock <path>`.
    pub fn sock_path(&self) -> &Path {
        &self.sock_path
    }

    /// Receiver of queued control jobs. The multiplexer drains this each tick
    /// and answers each job through its oneshot reply.
    pub fn jobs(&mut self) -> &mut mpsc::UnboundedReceiver<ControlJob> {
        &mut self.rx
    }
}

impl Drop for ControlServer {
    /// Remove the bound socket file on shutdown. `bind` only clears a *stale*
    /// file at startup, so without this every `caucus` run left its
    /// `caucus-<id>-ctl.sock` behind in the temp dir — they accumulate by the
    /// hundreds. The accept-loop task is torn down with the tokio runtime at
    /// process exit; unlinking the path here just stops new connects.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

/// Accept connections forever, one task per connection.
async fn accept_loop(
    listener: UnixListener,
    tx: mpsc::UnboundedSender<ControlJob>,
    events: EventSubscriber,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                tokio::spawn(handle_connection(stream, tx.clone(), events.clone()));
            }
            Err(_) => {
                // Transient accept error (e.g. EMFILE): yield and retry.
                tokio::task::yield_now().await;
            }
        }
    }
}

/// Read one call line, queue it as a [`ControlJob`], await the multiplexer's
/// reply, write it back.
///
/// One call, one reply, then the connection closes — the control-socket
/// protocol is not pipelined. A `subscribe_events` reply is followed by its
/// event stream ([`stream_events`]).
async fn handle_connection(
    stream: UnixStream,
    tx: mpsc::UnboundedSender<ControlJob>,
    events: EventSubscriber,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Bounded read: a peer cannot OOM the listener with a newline-less flood
    // (`line_io`). Over-cap calls are answered with an error, not buffered.
    let mut line = String::new();
    let (id, request) = match read_capped_line(&mut reader, &mut line, MAX_IPC_LINE_BYTES).await {
        Ok(CappedLine::Eof) => return, // client closed without sending
        Ok(CappedLine::Line) => match decode(line.trim_end()) {
            Ok(call) => call,
            Err(reply) => {
                send_line(&mut write_half, &reply).await;
                return;
            }
        },
        Ok(CappedLine::TooLong) => {
            let reply = ControlReply::error(
                None,
                ErrorCode::InvalidRequest,
                format!("control call exceeds {MAX_IPC_LINE_BYTES} bytes"),
            );
            send_line(&mut write_half, &reply).await;
            return;
        }
        Err(err) => {
            warn!(error = %err, "control socket read failed");
            return;
        }
    };

    // Subscribe before the call is queued, so no event published between
    // the snapshot and this task reading the reply is missed.
    let receiver = matches!(request, ControlRequest::SubscribeEvents).then(|| events.subscribe());
    let response = match execute(request, &tx).await {
        Ok(response) => response,
        Err((code, message)) => {
            send_line(
                &mut write_half,
                &ControlReply::error(Some(id), code, message),
            )
            .await;
            return;
        }
    };
    let stream_from = match (&response, receiver) {
        (ControlResponse::Subscribed { seq, .. }, Some(receiver)) => Some((*seq, receiver)),
        _ => None,
    };
    let sent = send_line(
        &mut write_half,
        &ControlReply::response(Some(id.clone()), response),
    )
    .await;
    if let (true, Some((seq, receiver))) = (sent, stream_from) {
        stream_events(&id, seq, receiver, reader, &mut write_half).await;
    }
}

/// Decode `line` into its id and request, or the reply rejecting it.
fn decode(line: &str) -> Result<(String, ControlRequest), ControlReply> {
    let call = serde_json::from_str::<ControlCall>(line).map_err(|err| {
        ControlReply::error(
            salvage_id(line),
            ErrorCode::InvalidRequest,
            format!("malformed control call: {err}"),
        )
    })?;
    match ControlRequest::from_call(&call.method, &call.params) {
        Ok(request) => Ok((call.id, request)),
        Err(message) => Err(ControlReply::error(
            Some(call.id),
            ErrorCode::InvalidRequest,
            message,
        )),
    }
}

/// Run `request` through the multiplexer and await its response.
async fn execute(
    request: ControlRequest,
    tx: &mpsc::UnboundedSender<ControlJob>,
) -> Result<ControlResponse, (ErrorCode, &'static str)> {
    let (reply_tx, reply_rx) = oneshot::channel();
    let job = ControlJob {
        request,
        reply: reply_tx,
    };
    if tx.send(job).is_err() {
        return Err((
            ErrorCode::Unavailable,
            "caucus multiplexer is shutting down",
        ));
    }
    // A dropped reply channel means the multiplexer is gone.
    reply_rx.await.map_err(|_| {
        (
            ErrorCode::Unavailable,
            "caucus multiplexer dropped the request",
        )
    })
}

/// Write each event after `after` as a [`ControlEvent`] line until the
/// client closes the connection or its write side, a write fails, or the
/// subscriber lags past the ring — which is answered with
/// [`ErrorCode::EventsLost`]. Bytes the client sends meanwhile are ignored.
async fn stream_events(
    id: &str,
    after: u64,
    mut events: broadcast::Receiver<PanelEvent>,
    mut reader: impl AsyncRead + Unpin,
    write_half: &mut (impl AsyncWrite + Unpin),
) {
    let mut discard = [0u8; 512];
    loop {
        tokio::select! {
            event = events.recv() => match event {
                // Already in the snapshot.
                Ok(event) if event.seq() <= after => {}
                Ok(event) => {
                    let line = ControlEvent { id: id.to_string(), event };
                    if !send_line(write_half, &line).await {
                        return;
                    }
                }
                Err(RecvError::Lagged(missed)) => {
                    let reply = ControlReply::error(
                        Some(id.to_string()),
                        ErrorCode::EventsLost,
                        format!(
                            "subscriber fell {missed} events behind the {EVENT_RING}-event \
                             ring; subscribe again"
                        ),
                    );
                    send_line(write_half, &reply).await;
                    return;
                }
                Err(RecvError::Closed) => return,
            },
            read = reader.read(&mut discard) => {
                if matches!(read, Ok(0) | Err(_)) {
                    return;
                }
            }
        }
    }
}

/// The string `id` of a line that is JSON but not a valid call, so the
/// caller can still match the error to its request.
fn salvage_id(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    value.get("id")?.as_str().map(str::to_string)
}

/// Serialise `line` and write it newline-terminated. Logs a serialise or
/// write error and returns `false`; the caller then closes the connection.
async fn send_line(write_half: &mut (impl AsyncWrite + Unpin), line: &impl Serialize) -> bool {
    let mut out = match serde_json::to_string(line) {
        Ok(out) => out,
        Err(err) => {
            warn!(error = %err, "control reply serialise failed");
            return false;
        }
    };
    out.push('\n');
    if let Err(err) = write_half.write_all(out.as_bytes()).await {
        warn!(error = %err, "control socket write failed");
        return false;
    }
    write_half.flush().await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::control_client::call;
    use crate::mcp::events::{EventFeed, test_row};
    use crate::session::id::PanelId;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, Lines};
    use tokio::net::unix::OwnedReadHalf;

    /// Dropping the server removes its socket file, so it does not accumulate
    /// in the temp dir across runs.
    #[tokio::test]
    async fn drop_removes_the_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        {
            let _server = ControlServer::bind(&sock, EventFeed::new().subscriber()).unwrap();
            assert!(sock.exists(), "bind creates the socket");
        }
        assert!(!sock.exists(), "drop removes the socket");
    }

    /// A call written to the socket reaches the job channel decoded, and a
    /// reply sent on the job's oneshot makes it back to the client.
    #[tokio::test]
    async fn call_round_trips_through_job_channel() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        let mut server = ControlServer::bind(&sock, EventFeed::new().subscriber()).unwrap();

        let panel = PanelId::new();
        let client_sock = sock.clone();
        let client = tokio::spawn(async move {
            call(
                &client_sock,
                "t1",
                "ctrl_c",
                &serde_json::json!({"panel": panel.to_string()}),
            )
            .await
            .unwrap()
        });

        // Act as the multiplexer: drain one job and answer it.
        let job = server.jobs().recv().await.expect("job queued");
        assert_eq!(job.request, ControlRequest::CtrlC { panel });
        job.reply.send(ControlResponse::Ok).unwrap();

        assert_eq!(client.await.unwrap(), Ok(ControlResponse::Ok));
    }

    /// Write `line` raw and parse the one reply line.
    async fn raw_exchange(sock: &Path, line: &[u8]) -> ControlReply {
        let mut stream = UnixStream::connect(sock).await.unwrap();
        stream.write_all(line).await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut reply = String::new();
        reader.read_line(&mut reply).await.unwrap();
        serde_json::from_str(reply.trim_end()).unwrap()
    }

    /// A line that is not a call, or names an unknown method, is answered
    /// `invalid_request` and never reaches the job channel. The id is echoed
    /// whenever the line carried one.
    #[tokio::test]
    async fn an_invalid_call_is_rejected_without_a_job() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        let mut server = ControlServer::bind(&sock, EventFeed::new().subscriber()).unwrap();

        let reply = raw_exchange(&sock, b"this is not json\n").await;
        assert_eq!(reply.id, None);
        assert_eq!(
            reply.into_result().unwrap_err().code,
            ErrorCode::InvalidRequest
        );

        let reply = raw_exchange(&sock, b"{\"id\":\"q\",\"method\":\"nope\"}\n").await;
        assert_eq!(reply.id.as_deref(), Some("q"));
        let error = reply.into_result().unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("unknown method `nope`"));

        let reply = raw_exchange(&sock, b"{\"id\":\"q\",\"op\":\"list_panels\"}\n").await;
        assert_eq!(reply.id.as_deref(), Some("q"));
        assert_eq!(
            reply.into_result().unwrap_err().code,
            ErrorCode::InvalidRequest
        );

        assert!(server.jobs().try_recv().is_err());
    }

    /// Connect, send a `subscribe_events` call, and wait until the server
    /// has subscribed and queued it.
    async fn subscribe(
        sock: &Path,
        server: &mut ControlServer,
    ) -> (
        Lines<BufReader<OwnedReadHalf>>,
        tokio::net::unix::OwnedWriteHalf,
        ControlJob,
    ) {
        let (read_half, mut write_half) = UnixStream::connect(sock).await.unwrap().into_split();
        write_half
            .write_all(b"{\"id\":\"s\",\"method\":\"subscribe_events\"}\n")
            .await
            .unwrap();
        let job = server.jobs().recv().await.expect("job queued");
        assert_eq!(job.request, ControlRequest::SubscribeEvents);
        (BufReader::new(read_half).lines(), write_half, job)
    }

    async fn next_json(lines: &mut Lines<BufReader<OwnedReadHalf>>) -> Value {
        let line = lines.next_line().await.unwrap().expect("a line");
        serde_json::from_str(&line).unwrap()
    }

    /// The snapshot reply is followed by exactly the events after its seq,
    /// gap-free: an event published after the server subscribed but before
    /// the snapshot was taken is already in the snapshot and is not resent.
    /// Closing the client's write side ends the stream.
    #[tokio::test]
    async fn a_subscription_streams_the_events_after_its_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        let mut feed = EventFeed::new();
        let mut server = ControlServer::bind(&sock, feed.subscriber()).unwrap();
        let a = PanelId::new();
        feed.publish(vec![test_row(a, "idle")]);

        let (mut lines, write_half, job) = subscribe(&sock, &mut server).await;
        feed.publish(vec![test_row(a, "working")]);
        let (seq, panels) = feed.snapshot();
        assert_eq!(seq, 2);
        job.reply
            .send(ControlResponse::Subscribed { seq, panels })
            .unwrap();
        feed.publish(vec![test_row(a, "idle")]);
        feed.publish(vec![]);

        let reply = next_json(&mut lines).await;
        assert_eq!(reply["id"], "s");
        assert_eq!(reply["result"]["type"], "subscribed");
        assert_eq!(reply["result"]["seq"], 2);
        assert_eq!(reply["result"]["panels"][0]["state"], "working");
        let first = next_json(&mut lines).await;
        let second = next_json(&mut lines).await;
        assert_eq!(first["id"], "s");
        assert_eq!(
            [&first["event"]["seq"], &second["event"]["seq"]],
            [&json!(3), &json!(4)]
        );
        assert_eq!(first["event"]["type"], "panel_updated");
        assert_eq!(first["event"]["panel"]["state"], "idle");
        assert_eq!(
            second["event"],
            json!({"type": "panel_removed", "seq": 4, "panel_id": a.to_string()})
        );

        drop(write_half);
        assert_eq!(lines.next_line().await.unwrap(), None);
    }

    /// A subscriber more than the ring behind gets one `events_lost` error
    /// and the connection closes.
    #[tokio::test]
    async fn a_subscriber_past_the_ring_gets_events_lost() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        let mut feed = EventFeed::new();
        let mut server = ControlServer::bind(&sock, feed.subscriber()).unwrap();
        let a = PanelId::new();

        let (mut lines, _write_half, job) = subscribe(&sock, &mut server).await;
        let (seq, panels) = feed.snapshot();
        job.reply
            .send(ControlResponse::Subscribed { seq, panels })
            .unwrap();
        // No await: the connection task cannot drain while the ring overflows.
        for i in 0..=EVENT_RING {
            feed.publish(vec![test_row(
                a,
                if i % 2 == 0 { "idle" } else { "working" },
            )]);
        }

        assert_eq!(next_json(&mut lines).await["result"]["type"], "subscribed");
        let lost = next_json(&mut lines).await;
        assert_eq!(lost["id"], "s");
        assert_eq!(lost["error"]["code"], "events_lost");
        assert_eq!(lines.next_line().await.unwrap(), None);
    }
}
