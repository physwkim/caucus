//! Hand-rolled minimal MCP server — JSON-RPC 2.0 over stdio (`docs/design.md`
//! §0 #4).
//!
//! **Why hand-rolled, not `rmcp`.** `rmcp` (1.7.0) resolves cleanly, but its
//! server surface is macro-driven (`#[tool_router]` / `#[tool]`) and its
//! transport drives an internal event loop that is awkward to exercise from a
//! deterministic unit test. The MCP slice caucus needs is small — three
//! methods (`initialize`, `tools/list`, `tools/call`) and a handful of tools — so this
//! module implements just that. The protocol core is a *pure* function,
//! [`McpDispatch::route`], which makes a `tools/list` round-trip and every
//! tool call testable without spawning a process or a transport.
//!
//! The stdio loop ([`serve_stdio`]) is the thin shell: read a line, parse a
//! [`Request`], route it, write the [`Response`] line — a `tools/call` on its
//! own task. Newline-delimited JSON, one object per line — the framing every
//! MCP stdio client uses.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::line_io::{CappedLine, MAX_IPC_LINE_BYTES, read_capped_line};

/// JSON-RPC protocol version string carried in every message.
const JSONRPC_VERSION: &str = "2.0";

/// MCP protocol revision caucus speaks. Reported in the `initialize` result.
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// One JSON-RPC request read off the wire.
#[derive(Debug, Deserialize)]
pub struct Request {
    /// Always `"2.0"`; not validated strictly — a non-2.0 client is rare and
    /// the error would be opaque anyway.
    #[allow(dead_code)]
    pub jsonrpc: Option<String>,
    /// Request id. Absent for notifications (no response is written).
    pub id: Option<Value>,
    /// Method name (`initialize`, `tools/list`, `tools/call`, ...).
    pub method: String,
    /// Method params.
    #[serde(default)]
    pub params: Value,
}

/// One JSON-RPC response written to the wire.
#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// JSON-RPC error object.
#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl Response {
    /// A success response carrying `result`.
    fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    /// An error response (`code`, `message`).
    fn err(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// One MCP tool definition, as returned by `tools/list`.
#[derive(Debug, Clone)]
pub struct ToolDef {
    /// Tool name (`send_keys`, `read_panel`, ...).
    pub name: &'static str,
    /// One-line human description.
    pub description: &'static str,
    /// JSON-Schema for the tool's arguments.
    pub input_schema: Value,
}

/// What a [`ToolHandler`] hands back: either a text result or an error string.
///
/// A tool error is reported to the MCP client as a `tools/call` result with
/// `isError: true` (the MCP convention) rather than a JSON-RPC error, so the
/// main worker's model sees the failure text in-band.
pub enum ToolOutcome {
    /// Tool succeeded; the string is the textual result.
    Ok(String),
    /// Tool failed; the string is the error message.
    Err(String),
}

/// A backend that executes one named tool call. Implemented by the
/// control-socket client ([`crate::mcp::serve`]) for the live process, and by
/// a test double in unit tests — so [`McpDispatch`] is exercised end-to-end
/// without a socket.
///
/// Calls run concurrently ([`serve_stdio`]), and a call whose request is
/// cancelled is dropped mid-await.
pub trait ToolHandler: Send + Sync + 'static {
    /// Execute tool `name` with JSON `args`.
    fn call(&self, name: &str, args: &Value) -> impl Future<Output = ToolOutcome> + Send;
}

/// What [`McpDispatch::route`] decided for one request.
pub enum Routed {
    /// Answer now: everything except a valid `tools/call`.
    Reply(Response),
    /// A validated `tools/call`, to be run on the handler.
    Call(ToolCall),
    /// `notifications/cancelled` for the request with this id.
    Cancel(Value),
    /// Any other notification: write nothing.
    Ignore,
}

/// A validated `tools/call` awaiting its handler.
pub struct ToolCall {
    id: Value,
    name: String,
    args: Value,
}

impl ToolCall {
    /// Run the call on `handler` and wrap the outcome in the MCP content
    /// envelope.
    async fn run<H: ToolHandler>(self, handler: &H) -> Response {
        let (text, is_error) = match handler.call(&self.name, &self.args).await {
            ToolOutcome::Ok(text) => (text, false),
            ToolOutcome::Err(text) => (text, true),
        };
        Response::ok(
            self.id,
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": is_error,
            }),
        )
    }
}

/// The pure MCP protocol core: routes a parsed [`Request`] to a method and
/// produces a [`Response`]. Holds the tool catalogue and a [`ToolHandler`].
pub struct McpDispatch<H> {
    tools: Vec<ToolDef>,
    handler: Arc<H>,
}

impl<H: ToolHandler> McpDispatch<H> {
    /// Build a dispatcher serving `tools`, backed by `handler`.
    pub fn new(tools: Vec<ToolDef>, handler: H) -> Self {
        Self {
            tools,
            handler: Arc::new(handler),
        }
    }

    /// Route one request without running any tool.
    pub fn route(&self, req: Request) -> Routed {
        // Notifications (no id) get no response — `notifications/initialized`
        // is the common one.
        let Some(id) = req.id else {
            return match req.method.as_str() {
                "notifications/cancelled" => req
                    .params
                    .get("requestId")
                    .cloned()
                    .map_or(Routed::Ignore, Routed::Cancel),
                _ => Routed::Ignore,
            };
        };

        Routed::Reply(match req.method.as_str() {
            "initialize" => Response::ok(id, self.initialize_result()),
            "tools/list" => Response::ok(id, self.tools_list_result()),
            "tools/call" => return self.tools_call(id, &req.params),
            "ping" => Response::ok(id, json!({})),
            other => Response::err(id, -32601, format!("method not found: {other}")),
        })
    }

    /// Route and run one request inline. Returns `None` for a notification.
    #[cfg(test)]
    pub async fn handle(&self, req: Request) -> Option<Response> {
        match self.route(req) {
            Routed::Reply(resp) => Some(resp),
            Routed::Call(call) => Some(call.run(&*self.handler).await),
            Routed::Cancel(_) | Routed::Ignore => None,
        }
    }

    /// `initialize` result — server capabilities + identity.
    fn initialize_result(&self) -> Value {
        json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "caucus",
                "version": crate::VERSION,
            },
        })
    }

    /// `tools/list` result — the full tool catalogue.
    fn tools_list_result(&self) -> Value {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                })
            })
            .collect();
        json!({ "tools": tools })
    }

    /// `tools/call` — validate the tool name; the call itself runs later.
    fn tools_call(&self, id: Value, params: &Value) -> Routed {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Routed::Reply(Response::err(id, -32602, "tools/call: missing `name`"));
        };
        if !self.tools.iter().any(|t| t.name == name) {
            return Routed::Reply(Response::err(id, -32602, format!("unknown tool: {name}")));
        }
        Routed::Call(ToolCall {
            id,
            name: name.to_string(),
            args: params.get("arguments").cloned().unwrap_or(json!({})),
        })
    }

    /// Tool catalogue (read-only) — exposed for tests.
    #[cfg(test)]
    pub fn tools(&self) -> &[ToolDef] {
        &self.tools
    }
}

/// Run the MCP server over stdio: read newline-delimited JSON-RPC requests
/// from `stdin`, dispatch each, write the response line to `stdout`.
///
/// Returns when `stdin` reaches EOF (the parent MCP client closed the pipe).
pub async fn serve_stdio<H: ToolHandler>(dispatch: McpDispatch<H>) -> anyhow::Result<()> {
    serve(dispatch, tokio::io::stdin(), tokio::io::stdout()).await
}

/// [`serve_stdio`] over any byte streams.
///
/// Each `tools/call` runs on its own task, so a long call (`wait`) does not
/// hold up the ones behind it; responses are written in completion order
/// through one writer task. A cancelled request's task is aborted and never
/// answered. Calls still running at EOF are aborted too: the client is gone.
async fn serve<H, R, W>(dispatch: McpDispatch<H>, input: R, output: W) -> anyhow::Result<()>
where
    H: ToolHandler,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let writer = tokio::spawn(write_responses(output, out_rx));
    // Keyed by the request id's JSON text, so `1` and `"1"` stay distinct.
    let mut in_flight: HashMap<String, AbortHandle> = HashMap::new();
    let mut reader = BufReader::new(input);
    let mut line = String::new();

    loop {
        // Bounded read: a client cannot OOM the server with a newline-less
        // flood (`line_io`).
        let routed = match read_capped_line(&mut reader, &mut line, MAX_IPC_LINE_BYTES).await? {
            CappedLine::Eof => break,
            CappedLine::TooLong => {
                // An over-cap request leaves the stream desynchronised; emit a
                // parse error and stop rather than resume into the leftover.
                let _ = out_tx.send(Response::err(
                    Value::Null,
                    -32700,
                    format!("request exceeds {MAX_IPC_LINE_BYTES} bytes"),
                ));
                break;
            }
            CappedLine::Line if line.trim().is_empty() => continue,
            CappedLine::Line => match serde_json::from_str::<Request>(&line) {
                Ok(req) => dispatch.route(req),
                // Parse error: reply with a JSON-RPC parse error (null id).
                Err(err) => Routed::Reply(Response::err(
                    Value::Null,
                    -32700,
                    format!("parse error: {err}"),
                )),
            },
        };
        match routed {
            Routed::Reply(resp) => {
                if out_tx.send(resp).is_err() {
                    break; // writer failed; its error is returned below
                }
            }
            Routed::Call(call) => {
                in_flight.retain(|_, task| !task.is_finished());
                let key = call.id.to_string();
                let handler = Arc::clone(&dispatch.handler);
                let out_tx = out_tx.clone();
                let task = tokio::spawn(async move {
                    let _ = out_tx.send(call.run(&*handler).await);
                });
                in_flight.insert(key, task.abort_handle());
            }
            Routed::Cancel(id) => {
                if let Some(task) = in_flight.remove(&id.to_string()) {
                    task.abort();
                }
            }
            Routed::Ignore => {}
        }
    }

    for task in in_flight.values() {
        task.abort();
    }
    drop(out_tx);
    writer.await.context("mcp-serve response writer")?
}

/// Write each response as one newline-terminated line, in arrival order,
/// until every sender is gone.
async fn write_responses<W: AsyncWrite + Unpin>(
    mut output: W,
    mut responses: mpsc::UnboundedReceiver<Response>,
) -> anyhow::Result<()> {
    while let Some(response) = responses.recv().await {
        let mut out = serde_json::to_string(&response)?;
        out.push('\n');
        output.write_all(out.as_bytes()).await?;
        output.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test ToolHandler: echoes the call back as text, or errors when the
    /// tool name starts with `bad`.
    struct EchoHandler;
    impl ToolHandler for EchoHandler {
        async fn call(&self, name: &str, args: &Value) -> ToolOutcome {
            if name.starts_with("bad") {
                ToolOutcome::Err(format!("{name} refused"))
            } else {
                ToolOutcome::Ok(format!("{name}({args})"))
            }
        }
    }

    fn tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "good",
                description: "ok tool",
                input_schema: json!({"type": "object"}),
            },
            ToolDef {
                name: "bad",
                description: "failing tool",
                input_schema: json!({"type": "object"}),
            },
        ]
    }

    fn req(method: &str, params: Value) -> Request {
        Request {
            jsonrpc: Some("2.0".into()),
            id: Some(json!(1)),
            method: method.into(),
            params,
        }
    }

    #[tokio::test]
    async fn initialize_reports_server_info() {
        let d = McpDispatch::new(tools(), EchoHandler);
        let resp = d.handle(req("initialize", json!({}))).await.unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["serverInfo"]["name"], "caucus");
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn tools_list_returns_catalogue() {
        let d = McpDispatch::new(tools(), EchoHandler);
        let resp = d.handle(req("tools/list", json!({}))).await.unwrap();
        let listed = resp.result.unwrap();
        let names: Vec<&str> = listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["good", "bad"]);
    }

    #[tokio::test]
    async fn tools_call_dispatches_to_handler() {
        let d = McpDispatch::new(tools(), EchoHandler);
        let resp = d
            .handle(req(
                "tools/call",
                json!({"name": "good", "arguments": {"x": 1}}),
            ))
            .await
            .unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], false);
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("good(")
        );
    }

    #[tokio::test]
    async fn tool_error_is_in_band_is_error() {
        let d = McpDispatch::new(tools(), EchoHandler);
        let resp = d
            .handle(req("tools/call", json!({"name": "bad", "arguments": {}})))
            .await
            .unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("refused")
        );
    }

    #[tokio::test]
    async fn unknown_tool_is_invalid_params() {
        let d = McpDispatch::new(tools(), EchoHandler);
        let resp = d
            .handle(req("tools/call", json!({"name": "nope"})))
            .await
            .unwrap();
        assert_eq!(resp.error.unwrap().code, -32602);
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let d = McpDispatch::new(tools(), EchoHandler);
        let resp = d.handle(req("frobnicate", json!({}))).await.unwrap();
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[tokio::test]
    async fn notification_without_id_yields_no_response() {
        let d = McpDispatch::new(tools(), EchoHandler);
        let notif = Request {
            jsonrpc: Some("2.0".into()),
            id: None,
            method: "notifications/initialized".into(),
            params: json!({}),
        };
        assert!(d.handle(notif).await.is_none());
    }

    /// `hold` reports its start, parks until released, and reports its drop;
    /// every other tool answers at once.
    struct GateHandler {
        release: Arc<tokio::sync::Notify>,
        started: mpsc::UnboundedSender<()>,
        dropped: mpsc::UnboundedSender<()>,
    }

    /// Signals `dropped` when the `hold` future is dropped, finished or not.
    struct DropSignal(mpsc::UnboundedSender<()>);
    impl Drop for DropSignal {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    impl ToolHandler for GateHandler {
        async fn call(&self, name: &str, _args: &Value) -> ToolOutcome {
            if name == "hold" {
                let _signal = DropSignal(self.dropped.clone());
                let _ = self.started.send(());
                self.release.notified().await;
            }
            ToolOutcome::Ok(name.to_string())
        }
    }

    /// A dispatcher served on in-memory pipes, seen from the client side.
    struct Gate {
        client: tokio::io::DuplexStream,
        lines: tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
        release: Arc<tokio::sync::Notify>,
        started: mpsc::UnboundedReceiver<()>,
        dropped: mpsc::UnboundedReceiver<()>,
        server: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    /// Bound a test await, so a regression fails instead of hanging.
    async fn within<T>(fut: impl Future<Output = T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(5), fut)
            .await
            .expect("timed out")
    }

    impl Gate {
        fn serve() -> Self {
            use tokio::io::AsyncBufReadExt;
            let (client, server_in) = tokio::io::duplex(4096);
            let (server_out, client_out) = tokio::io::duplex(4096);
            let release = Arc::new(tokio::sync::Notify::new());
            let (started_tx, started) = mpsc::unbounded_channel();
            let (dropped_tx, dropped) = mpsc::unbounded_channel();
            let tools = ["hold", "good"]
                .into_iter()
                .map(|name| ToolDef {
                    name,
                    description: "",
                    input_schema: json!({"type": "object"}),
                })
                .collect();
            let handler = GateHandler {
                release: Arc::clone(&release),
                started: started_tx,
                dropped: dropped_tx,
            };
            let server = tokio::spawn(serve(
                McpDispatch::new(tools, handler),
                server_in,
                server_out,
            ));
            Self {
                client,
                lines: BufReader::new(client_out).lines(),
                release,
                started,
                dropped,
                server,
            }
        }

        async fn send(&mut self, msg: Value) {
            let mut line = msg.to_string();
            line.push('\n');
            self.client.write_all(line.as_bytes()).await.unwrap();
        }

        async fn call(&mut self, id: u64, name: &str) {
            self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                             "params": {"name": name, "arguments": {}}}))
                .await;
        }

        async fn next_id(&mut self) -> Value {
            let line = within(self.lines.next_line())
                .await
                .unwrap()
                .expect("a response");
            serde_json::from_str::<Value>(&line).unwrap()["id"].clone()
        }

        /// Close the client's input, await a clean server exit, and assert
        /// nothing further was written.
        async fn finish(mut self) {
            drop(self.client);
            within(self.server).await.unwrap().unwrap();
            assert!(self.lines.next_line().await.unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn a_parked_call_does_not_hold_up_later_ones() {
        let mut gate = Gate::serve();
        gate.call(1, "hold").await;
        within(gate.started.recv()).await.unwrap();
        gate.call(2, "good").await;
        assert_eq!(gate.next_id().await, json!(2));
        gate.release.notify_one();
        assert_eq!(gate.next_id().await, json!(1));
        gate.finish().await;
    }

    #[tokio::test]
    async fn a_cancelled_call_is_dropped_and_never_answered() {
        let mut gate = Gate::serve();
        gate.call(1, "hold").await;
        within(gate.started.recv()).await.unwrap();
        gate.send(
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                         "params": {"requestId": 1, "reason": "user"}}),
        )
        .await;
        within(gate.dropped.recv()).await.unwrap();
        gate.call(2, "good").await;
        assert_eq!(gate.next_id().await, json!(2));
        gate.finish().await;
    }

    #[tokio::test]
    async fn eof_aborts_a_parked_call() {
        let mut gate = Gate::serve();
        gate.call(1, "hold").await;
        within(gate.started.recv()).await.unwrap();
        let Gate {
            client,
            mut lines,
            mut dropped,
            server,
            ..
        } = gate;
        drop(client);
        within(server).await.unwrap().unwrap();
        within(dropped.recv()).await.unwrap();
        assert!(lines.next_line().await.unwrap().is_none());
    }
}
