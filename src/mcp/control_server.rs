//! Control-socket server — the main `caucus` process side of the control
//! socket (`docs/design.md` §0 #4, §9).
//!
//! The main process opens this socket alongside the turn-signal socket. A
//! tokio accept task reads one [`ControlRequest`] per connection and pushes a
//! [`ControlJob`] onto an mpsc channel the [`crate::session::Multiplexer`]
//! event loop drains each tick. The multiplexer executes the request against
//! live panels and answers through the job's oneshot reply channel; this
//! module writes the [`ControlResponse`] back on the connection.
//!
//! Why route through the event loop rather than touch panels here: the
//! `Multiplexer` is `!Send` single-owner state (it owns PTYs, grids). Control
//! requests must execute on the same thread that pumps panels — Invariant
//! I-5's single-owner discipline. The mpsc + oneshot pair is the hand-off.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use super::protocol::{ControlRequest, ControlResponse};

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
    /// Bind the control socket at `sock_path` and spawn the accept loop.
    ///
    /// A stale socket file at the path is removed first. The accept loop runs
    /// for the lifetime of the process.
    pub fn bind(sock_path: &Path) -> Result<Self, ControlServerError> {
        let _ = std::fs::remove_file(sock_path);
        let listener = UnixListener::bind(sock_path).map_err(|source| ControlServerError::Io {
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

    /// Path the control socket is bound to — injected into the CEO panel's
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

/// Accept connections forever, one task per connection.
async fn accept_loop(listener: UnixListener, tx: mpsc::UnboundedSender<ControlJob>) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let tx = tx.clone();
                tokio::spawn(handle_connection(stream, tx));
            }
            Err(_) => {
                // Transient accept error (e.g. EMFILE): yield and retry.
                tokio::task::yield_now().await;
            }
        }
    }
}

/// Read one request line, queue it as a [`ControlJob`], await the multiplexer's
/// reply, write it back.
///
/// One request, one response, then the connection closes — the control-socket
/// protocol is not pipelined.
async fn handle_connection(stream: UnixStream, tx: mpsc::UnboundedSender<ControlJob>) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) => return, // client closed without sending
        Ok(_) => {}
        Err(err) => {
            warn!(error = %err, "control socket read failed");
            return;
        }
    }

    let response = match serde_json::from_str::<ControlRequest>(line.trim_end()) {
        Ok(request) => {
            let (reply_tx, reply_rx) = oneshot::channel();
            let job = ControlJob {
                request,
                reply: reply_tx,
            };
            if tx.send(job).is_err() {
                ControlResponse::error("caucus multiplexer is shutting down")
            } else {
                // Await the multiplexer's answer. A dropped reply channel
                // (multiplexer gone) surfaces as an error response.
                match reply_rx.await {
                    Ok(resp) => resp,
                    Err(_) => ControlResponse::error("caucus multiplexer dropped the request"),
                }
            }
        }
        Err(err) => ControlResponse::error(format!("malformed control request: {err}")),
    };

    let mut out = match serde_json::to_string(&response) {
        Ok(out) => out,
        Err(err) => {
            warn!(error = %err, "control response serialise failed");
            return;
        }
    };
    out.push('\n');
    if let Err(err) = write_half.write_all(out.as_bytes()).await {
        warn!(error = %err, "control socket write failed");
    }
    let _ = write_half.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::control_client::roundtrip;
    use crate::session::id::PanelId;

    /// A request written to the socket reaches the job channel, and a reply
    /// sent on the job's oneshot makes it back to the client.
    #[tokio::test]
    async fn request_round_trips_through_job_channel() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        let mut server = ControlServer::bind(&sock).unwrap();

        let panel = PanelId::new();
        let client_sock = sock.clone();
        let client = tokio::spawn(async move {
            roundtrip(&client_sock, &ControlRequest::CtrlC { panel })
                .await
                .unwrap()
        });

        // Act as the multiplexer: drain one job and answer it.
        let job = server.jobs().recv().await.expect("job queued");
        assert_eq!(job.request, ControlRequest::CtrlC { panel });
        job.reply.send(ControlResponse::Ok).unwrap();

        assert_eq!(client.await.unwrap(), ControlResponse::Ok);
    }

    /// A malformed request line yields an error response and never reaches the
    /// job channel.
    #[tokio::test]
    async fn malformed_request_is_rejected_without_a_job() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        let mut server = ControlServer::bind(&sock).unwrap();

        let mut stream = UnixStream::connect(&sock).await.unwrap();
        stream.write_all(b"this is not json\n").await.unwrap();

        let mut reader = BufReader::new(stream);
        let mut resp = String::new();
        reader.read_line(&mut resp).await.unwrap();
        let parsed: ControlResponse = serde_json::from_str(resp.trim_end()).unwrap();
        assert!(matches!(parsed, ControlResponse::Error { .. }));

        // No job was queued for a malformed request.
        assert!(server.jobs().try_recv().is_err());
    }
}
