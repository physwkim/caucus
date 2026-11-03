//! Control-socket client — the `caucus mcp-serve` side of the control socket.
//!
//! `mcp-serve` is a thin stdio MCP server: every MCP `tools/call` it receives
//! is forwarded as a [`ControlCall`] whose `method` and `params` are the tool
//! name and arguments, undecoded — the main process decodes them
//! ([`super::protocol::ControlRequest::from_call`]), so the MCP tools and other
//! control-socket clients share one schema.
//!
//! [`ControlClient`] implements [`crate::mcp::jsonrpc::ToolHandler`] so the
//! hand-rolled [`crate::mcp::jsonrpc::McpDispatch`] can drive it directly.
//! Each call is a fresh connect / write-line / read-line / close — every MCP
//! tool is one call, one reply per connection (`protocol` module doc).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::jsonrpc::{ToolHandler, ToolOutcome};
use super::protocol::{ControlCall, ControlReply, ControlResponse, ReplyError};

/// The `id` mcp-serve puts on its calls. One call per connection leaves
/// nothing to correlate, so it only names the caller.
const MCP_CALL_ID: &str = "mcp-serve";

/// A client of the main process's control socket.
pub struct ControlClient {
    sock_path: PathBuf,
}

impl ControlClient {
    /// Build a client for the control socket at `sock_path`.
    pub fn new(sock_path: impl Into<PathBuf>) -> Self {
        Self {
            sock_path: sock_path.into(),
        }
    }
}

/// One control-socket call on a fresh connection. The outer `Err` is a
/// transport failure; the inner one is the error the server replied with.
pub async fn call(
    sock_path: &Path,
    id: &str,
    method: &str,
    params: &Value,
) -> Result<Result<ControlResponse, ReplyError>> {
    let stream = UnixStream::connect(sock_path)
        .await
        .with_context(|| format!("connect to caucus control socket {}", sock_path.display()))?;
    let (read_half, mut write_half) = stream.into_split();

    let call = ControlCall {
        id: id.to_string(),
        method: method.to_string(),
        params: params.clone(),
    };
    let mut line = serde_json::to_string(&call).context("serialise control call")?;
    line.push('\n');
    write_half
        .write_all(line.as_bytes())
        .await
        .context("write control call")?;
    write_half.flush().await.ok();

    let mut reader = BufReader::new(read_half);
    let mut reply_line = String::new();
    let n = reader
        .read_line(&mut reply_line)
        .await
        .context("read control reply")?;
    if n == 0 {
        anyhow::bail!("caucus control socket closed without a reply");
    }
    let reply: ControlReply = serde_json::from_str(reply_line.trim_end())
        .with_context(|| format!("parse control reply: {}", reply_line.trim_end()))?;
    Ok(reply.into_result())
}

/// Render a [`ControlResponse`] as the textual MCP tool result.
fn render_response(resp: ControlResponse) -> ToolOutcome {
    match resp {
        ControlResponse::Ok => ToolOutcome::Ok("ok".to_string()),
        ControlResponse::Panel { text } => ToolOutcome::Ok(text),
        ControlResponse::Spawned { panel } => ToolOutcome::Ok(panel.to_string()),
        ControlResponse::Panels { panels } => match serde_json::to_string_pretty(&panels) {
            Ok(text) => ToolOutcome::Ok(text),
            Err(err) => ToolOutcome::Err(format!("serialise panel list: {err}")),
        },
        // mcp-serve lists no `subscribe_events` tool, so this is not reached.
        ControlResponse::Subscribed { .. } => {
            ToolOutcome::Err("subscribe_events streams; it is not an MCP tool".to_string())
        }
        ControlResponse::Error { message } => ToolOutcome::Err(message),
    }
}

impl ToolHandler for ControlClient {
    async fn call(&self, name: &str, args: &Value) -> ToolOutcome {
        match call(&self.sock_path, MCP_CALL_ID, name, args).await {
            Ok(Ok(response)) => render_response(response),
            Ok(Err(error)) => ToolOutcome::Err(error.message),
            Err(err) => ToolOutcome::Err(format!("caucus control socket: {err:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::id::PanelId;

    #[test]
    fn render_spawned_response_is_panel_id() {
        let id = PanelId::new();
        match render_response(ControlResponse::Spawned { panel: id }) {
            ToolOutcome::Ok(text) => assert_eq!(text, id.to_string()),
            ToolOutcome::Err(_) => panic!("expected Ok"),
        }
    }

    /// An error reply reaches the main worker as a tool error carrying the
    /// server's message.
    #[tokio::test]
    async fn a_failed_call_is_a_tool_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        let mut server = crate::mcp::control_server::ControlServer::bind(
            &sock,
            crate::mcp::events::EventFeed::new().subscriber(),
        )
        .unwrap();
        let client = ControlClient::new(&sock);
        let panel = PanelId::new();
        let call = tokio::spawn(async move {
            client
                .call("ctrl_c", &serde_json::json!({"panel": panel.to_string()}))
                .await
        });
        let job = server.jobs().recv().await.expect("job queued");
        job.reply
            .send(ControlResponse::error("no such panel"))
            .unwrap();
        match call.await.unwrap() {
            ToolOutcome::Err(msg) => assert_eq!(msg, "no such panel"),
            ToolOutcome::Ok(text) => panic!("expected Err, got {text}"),
        }
    }
}
