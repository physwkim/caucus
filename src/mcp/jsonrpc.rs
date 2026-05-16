//! Hand-rolled minimal MCP server — JSON-RPC 2.0 over stdio (`docs/design.md`
//! §0 #4).
//!
//! **Why hand-rolled, not `rmcp`.** `rmcp` (1.7.0) resolves cleanly, but its
//! server surface is macro-driven (`#[tool_router]` / `#[tool]`) and its
//! transport drives an internal event loop that is awkward to exercise from a
//! deterministic unit test. The MCP slice caucus needs is small — three
//! methods (`initialize`, `tools/list`, `tools/call`) and a handful of tools — so this
//! module implements just that. The protocol core is a *pure* function,
//! [`McpDispatch::handle`], which makes a `tools/list` round-trip and every
//! tool call testable without spawning a process or a transport.
//!
//! The stdio loop ([`serve_stdio`]) is the thin shell: read a line, parse a
//! [`Request`], dispatch, write the [`Response`] line. Newline-delimited JSON,
//! one object per line — the framing every MCP stdio client uses.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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
pub trait ToolHandler {
    /// Execute tool `name` with JSON `args`.
    fn call(&mut self, name: &str, args: &Value) -> ToolOutcome;
}

/// The pure MCP protocol core: routes a parsed [`Request`] to a method and
/// produces a [`Response`]. Holds the tool catalogue and a [`ToolHandler`].
pub struct McpDispatch<H> {
    tools: Vec<ToolDef>,
    handler: H,
}

impl<H: ToolHandler> McpDispatch<H> {
    /// Build a dispatcher serving `tools`, backed by `handler`.
    pub fn new(tools: Vec<ToolDef>, handler: H) -> Self {
        Self { tools, handler }
    }

    /// Handle one request. Returns `None` for a notification (no `id`), in
    /// which case the caller writes nothing.
    pub fn handle(&mut self, req: Request) -> Option<Response> {
        // Notifications (no id) get no response — `notifications/initialized`
        // is the common one.
        let id = req.id.clone()?;

        let resp = match req.method.as_str() {
            "initialize" => Response::ok(id, self.initialize_result()),
            "tools/list" => Response::ok(id, self.tools_list_result()),
            "tools/call" => self.tools_call(id, &req.params),
            "ping" => Response::ok(id, json!({})),
            other => Response::err(id, -32601, format!("method not found: {other}")),
        };
        Some(resp)
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

    /// `tools/call` — dispatch to the [`ToolHandler`], wrap the outcome in the
    /// MCP content envelope.
    fn tools_call(&mut self, id: Value, params: &Value) -> Response {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Response::err(id, -32602, "tools/call: missing `name`");
        };
        if !self.tools.iter().any(|t| t.name == name) {
            return Response::err(id, -32602, format!("unknown tool: {name}"));
        }
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        let (text, is_error) = match self.handler.call(name, &args) {
            ToolOutcome::Ok(text) => (text, false),
            ToolOutcome::Err(text) => (text, true),
        };
        Response::ok(
            id,
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": is_error,
            }),
        )
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
pub async fn serve_stdio<H: ToolHandler>(mut dispatch: McpDispatch<H>) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => dispatch.handle(req),
            Err(err) => {
                // Parse error: reply with a JSON-RPC parse error (null id).
                Some(Response::err(Value::Null, -32700, format!("parse error: {err}")))
            }
        };
        if let Some(response) = response {
            let mut out = serde_json::to_string(&response)?;
            out.push('\n');
            stdout.write_all(out.as_bytes()).await?;
            stdout.flush().await?;
        }
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
        fn call(&mut self, name: &str, args: &Value) -> ToolOutcome {
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

    #[test]
    fn initialize_reports_server_info() {
        let mut d = McpDispatch::new(tools(), EchoHandler);
        let resp = d.handle(req("initialize", json!({}))).unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["serverInfo"]["name"], "caucus");
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_returns_catalogue() {
        let mut d = McpDispatch::new(tools(), EchoHandler);
        let resp = d.handle(req("tools/list", json!({}))).unwrap();
        let listed = resp.result.unwrap();
        let names: Vec<&str> = listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["good", "bad"]);
    }

    #[test]
    fn tools_call_dispatches_to_handler() {
        let mut d = McpDispatch::new(tools(), EchoHandler);
        let resp = d
            .handle(req(
                "tools/call",
                json!({"name": "good", "arguments": {"x": 1}}),
            ))
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

    #[test]
    fn tool_error_is_in_band_is_error() {
        let mut d = McpDispatch::new(tools(), EchoHandler);
        let resp = d
            .handle(req("tools/call", json!({"name": "bad", "arguments": {}})))
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

    #[test]
    fn unknown_tool_is_invalid_params() {
        let mut d = McpDispatch::new(tools(), EchoHandler);
        let resp = d
            .handle(req("tools/call", json!({"name": "nope"})))
            .unwrap();
        assert_eq!(resp.error.unwrap().code, -32602);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let mut d = McpDispatch::new(tools(), EchoHandler);
        let resp = d.handle(req("frobnicate", json!({}))).unwrap();
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[test]
    fn notification_without_id_yields_no_response() {
        let mut d = McpDispatch::new(tools(), EchoHandler);
        let notif = Request {
            jsonrpc: Some("2.0".into()),
            id: None,
            method: "notifications/initialized".into(),
            params: json!({}),
        };
        assert!(d.handle(notif).is_none());
    }
}
