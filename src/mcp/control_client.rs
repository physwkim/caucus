//! Control-socket client — the `caucus mcp-serve` side of the control socket.
//!
//! `mcp-serve` is a thin stdio MCP server; every MCP `tools/call` it receives
//! is translated into a [`ControlRequest`], shipped over the control socket to
//! the main `caucus` process, and the [`ControlResponse`] is turned back into
//! an MCP tool result.
//!
//! [`ControlClient`] implements [`crate::mcp::jsonrpc::ToolHandler`] so the
//! hand-rolled [`crate::mcp::jsonrpc::McpDispatch`] can drive it directly.
//! Each call is a fresh connect / write-line / read-line / close — the
//! control-socket protocol is strictly one request, one response per
//! connection (`protocol` module doc).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::role::spec::AgentCli;
use crate::session::id::PanelId;

use super::ReadPanelMode;
use super::jsonrpc::{ToolHandler, ToolOutcome};
use super::protocol::{ControlRequest, ControlResponse};

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

/// Blocking control-socket round-trip — used by `mcp-serve`'s synchronous
/// [`ToolHandler::call`].
///
/// `ToolHandler::call` is invoked from inside `mcp-serve`'s tokio runtime, so
/// it cannot drive an async future — `Handle::block_on` and `block_in_place`
/// both panic, as neither leaves the runtime context. Instead the round-trip
/// uses plain blocking `std` sockets: one connect / write-line / read-line /
/// close, no runtime involved. `mcp-serve` handles one MCP request at a time,
/// so briefly blocking the worker is harmless.
fn roundtrip_blocking(sock_path: &Path, req: &ControlRequest) -> Result<ControlResponse> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect(sock_path)
        .with_context(|| format!("connect to caucus control socket {}", sock_path.display()))?;

    let mut line = serde_json::to_string(req).context("serialise control request")?;
    line.push('\n');
    (&stream)
        .write_all(line.as_bytes())
        .context("write control request")?;
    (&stream).flush().ok();

    let mut reader = BufReader::new(&stream);
    let mut resp_line = String::new();
    let n = reader
        .read_line(&mut resp_line)
        .context("read control response")?;
    if n == 0 {
        anyhow::bail!("caucus control socket closed without a response");
    }
    serde_json::from_str(resp_line.trim_end())
        .with_context(|| format!("parse control response: {}", resp_line.trim_end()))
}

/// One control-socket request/response round-trip on a fresh connection.
pub async fn roundtrip(sock_path: &Path, req: &ControlRequest) -> Result<ControlResponse> {
    let stream = UnixStream::connect(sock_path)
        .await
        .with_context(|| format!("connect to caucus control socket {}", sock_path.display()))?;
    let (read_half, mut write_half) = stream.into_split();

    let mut line = serde_json::to_string(req).context("serialise control request")?;
    line.push('\n');
    write_half
        .write_all(line.as_bytes())
        .await
        .context("write control request")?;
    write_half.flush().await.ok();

    let mut reader = BufReader::new(read_half);
    let mut resp_line = String::new();
    let n = reader
        .read_line(&mut resp_line)
        .await
        .context("read control response")?;
    if n == 0 {
        anyhow::bail!("caucus control socket closed without a response");
    }
    serde_json::from_str(resp_line.trim_end())
        .with_context(|| format!("parse control response: {}", resp_line.trim_end()))
}

/// Translate one MCP tool call into a [`ControlRequest`].
///
/// Returns `Err` with a human-readable message when an argument is missing or
/// malformed — the caller surfaces that as an `isError` tool result.
fn build_request(name: &str, args: &Value) -> std::result::Result<ControlRequest, String> {
    /// Pull a required panel-id argument and parse it.
    fn panel(args: &Value) -> std::result::Result<PanelId, String> {
        let raw = args
            .get("panel")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing string argument `panel`".to_string())?;
        raw.parse::<PanelId>()
            .map_err(|e| format!("invalid panel id `{raw}`: {e}"))
    }

    match name {
        "send_keys" => {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing string argument `text`".to_string())?
                .to_string();
            let enter = args.get("enter").and_then(Value::as_bool).unwrap_or(false);
            Ok(ControlRequest::SendKeys {
                panel: panel(args)?,
                text,
                enter,
            })
        }
        "send_key" => {
            let key = args
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing string argument `key`".to_string())?
                .to_string();
            Ok(ControlRequest::SendKey {
                panel: panel(args)?,
                key,
            })
        }
        "broadcast" => {
            let raw = args
                .get("panels")
                .and_then(Value::as_array)
                .ok_or_else(|| "missing array argument `panels`".to_string())?;
            let panels = raw
                .iter()
                .map(|v| {
                    let s = v
                        .as_str()
                        .ok_or_else(|| "`panels` entries must be panel-id strings".to_string())?;
                    s.parse::<PanelId>()
                        .map_err(|e| format!("invalid panel id `{s}`: {e}"))
                })
                .collect::<std::result::Result<Vec<PanelId>, String>>()?;
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing string argument `text`".to_string())?
                .to_string();
            let enter = args.get("enter").and_then(Value::as_bool).unwrap_or(false);
            Ok(ControlRequest::Broadcast {
                panels,
                text,
                enter,
            })
        }
        "ctrl_c" => Ok(ControlRequest::CtrlC {
            panel: panel(args)?,
        }),
        "read_panel" => {
            let mode_raw = args
                .get("mode")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing string argument `mode`".to_string())?;
            // `turn` carries its index in a sibling integer arg rather than in
            // the mode string, so the mode stays a plain enum value.
            let mode = if mode_raw == "turn" {
                let n = args.get("turn").and_then(Value::as_u64).ok_or_else(|| {
                    "mode `turn` requires a non-negative integer argument `turn`".to_string()
                })? as usize;
                ReadPanelMode::Turn(n)
            } else {
                serde_json::from_value(json!(mode_raw)).map_err(|_| {
                    format!(
                        "invalid mode `{mode_raw}` \
                         (expected screen|scrollback|since_last_turn|last_message|turn)"
                    )
                })?
            };
            Ok(ControlRequest::ReadPanel {
                panel: panel(args)?,
                mode,
            })
        }
        "spawn_role" => {
            let role = args
                .get("role")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing string argument `role`".to_string())?
                .to_string();
            let worktree = args
                .get("worktree")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let model = args
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
            let agent_cli = match args.get("agent_cli").and_then(Value::as_str) {
                Some(raw) => Some(
                    serde_json::from_value::<AgentCli>(json!(raw))
                        .map_err(|_| format!("invalid agent_cli `{raw}`"))?,
                ),
                None => None,
            };
            let prompt = args
                .get("prompt")
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok(ControlRequest::SpawnRole {
                role,
                worktree,
                model,
                agent_cli,
                prompt,
            })
        }
        "kill_panel" => Ok(ControlRequest::KillPanel {
            panel: panel(args)?,
        }),
        "list_panels" => Ok(ControlRequest::ListPanels),
        "register_round" => {
            let raw = args
                .get("panels")
                .and_then(Value::as_array)
                .ok_or_else(|| "missing array argument `panels`".to_string())?;
            let panels = raw
                .iter()
                .map(|v| {
                    let s = v
                        .as_str()
                        .ok_or_else(|| "`panels` entries must be panel-id strings".to_string())?;
                    s.parse::<PanelId>()
                        .map_err(|e| format!("invalid panel id `{s}`: {e}"))
                })
                .collect::<std::result::Result<Vec<PanelId>, String>>()?;
            let read_mode = match args.get("read_mode").and_then(Value::as_str) {
                Some(raw) => Some(serde_json::from_value(json!(raw)).map_err(|_| {
                    format!(
                        "invalid read_mode `{raw}` \
                         (expected screen|scrollback|since_last_turn|last_message)"
                    )
                })?),
                None => None,
            };
            let fallback_secs =
                match args.get("fallback_secs") {
                    Some(v) => Some(v.as_u64().ok_or_else(|| {
                        "`fallback_secs` must be a non-negative integer".to_string()
                    })?),
                    None => None,
                };
            let backlog = match args.get("backlog") {
                Some(v) => {
                    let obj = v.as_object().ok_or_else(|| {
                        "`backlog` must be an object mapping panel id → array of task strings"
                            .to_string()
                    })?;
                    let mut map = std::collections::HashMap::with_capacity(obj.len());
                    for (key, tasks) in obj {
                        let id = key
                            .parse::<PanelId>()
                            .map_err(|e| format!("invalid backlog panel id `{key}`: {e}"))?;
                        let arr = tasks.as_array().ok_or_else(|| {
                            format!("`backlog[{key}]` must be an array of task strings")
                        })?;
                        let tasks = arr
                            .iter()
                            .map(|t| {
                                t.as_str().map(str::to_string).ok_or_else(|| {
                                    format!("`backlog[{key}]` entries must be strings")
                                })
                            })
                            .collect::<std::result::Result<Vec<String>, String>>()?;
                        map.insert(id, tasks);
                    }
                    Some(map)
                }
                None => None,
            };
            Ok(ControlRequest::RegisterRound {
                panels,
                read_mode,
                fallback_secs,
                backlog,
            })
        }
        "read_menu" => Ok(ControlRequest::ReadMenu {
            panel: panel(args)?,
        }),
        "select_option" => {
            let index = args
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| "missing integer argument `index`".to_string())?
                as usize;
            Ok(ControlRequest::SelectOption {
                panel: panel(args)?,
                index,
            })
        }
        other => Err(format!("unknown tool: {other}")),
    }
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
        ControlResponse::Error { message } => ToolOutcome::Err(message),
    }
}

impl ToolHandler for ControlClient {
    fn call(&mut self, name: &str, args: &Value) -> ToolOutcome {
        let req = match build_request(name, args) {
            Ok(req) => req,
            Err(msg) => return ToolOutcome::Err(msg),
        };
        // Blocking std-socket round-trip — no runtime gymnastics
        // (see `roundtrip_blocking`).
        match roundtrip_blocking(&self.sock_path, &req) {
            Ok(resp) => render_response(resp),
            Err(err) => ToolOutcome::Err(format!("caucus control socket: {err:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_send_keys_request() {
        let id = PanelId::new();
        let req = build_request(
            "send_keys",
            &json!({"panel": id.to_string(), "text": "/clear", "enter": true}),
        )
        .unwrap();
        assert_eq!(
            req,
            ControlRequest::SendKeys {
                panel: id,
                text: "/clear".into(),
                enter: true,
            }
        );
    }

    #[test]
    fn build_send_key_request() {
        let id = PanelId::new();
        let req =
            build_request("send_key", &json!({"panel": id.to_string(), "key": "esc"})).unwrap();
        assert_eq!(
            req,
            ControlRequest::SendKey {
                panel: id,
                key: "esc".into(),
            }
        );
    }

    #[test]
    fn build_send_key_requires_key() {
        let id = PanelId::new();
        let err = build_request("send_key", &json!({"panel": id.to_string()})).unwrap_err();
        assert!(err.contains("missing string argument `key`"));
    }

    #[test]
    fn build_broadcast_request_with_enter() {
        let a = PanelId::new();
        let b = PanelId::new();
        let req = build_request(
            "broadcast",
            &json!({"panels": [a.to_string(), b.to_string()], "text": "the agenda", "enter": true}),
        )
        .unwrap();
        assert_eq!(
            req,
            ControlRequest::Broadcast {
                panels: vec![a, b],
                text: "the agenda".into(),
                enter: true,
            }
        );
    }

    #[test]
    fn build_broadcast_request_defaults_enter() {
        let a = PanelId::new();
        let req = build_request(
            "broadcast",
            &json!({"panels": [a.to_string()], "text": "hi"}),
        )
        .unwrap();
        assert_eq!(
            req,
            ControlRequest::Broadcast {
                panels: vec![a],
                text: "hi".into(),
                enter: false,
            }
        );
    }

    #[test]
    fn build_broadcast_requires_panels_array() {
        let err = build_request("broadcast", &json!({"text": "hi"})).unwrap_err();
        assert!(err.contains("missing array argument `panels`"));
    }

    #[test]
    fn build_broadcast_requires_text() {
        let a = PanelId::new();
        let err = build_request("broadcast", &json!({"panels": [a.to_string()]})).unwrap_err();
        assert!(err.contains("missing string argument `text`"));
    }

    #[test]
    fn build_broadcast_rejects_bad_panel_id() {
        let err = build_request(
            "broadcast",
            &json!({"panels": ["not-a-ulid"], "text": "hi"}),
        )
        .unwrap_err();
        assert!(err.contains("invalid panel id"));
    }

    #[test]
    fn build_read_panel_request() {
        let id = PanelId::new();
        let req = build_request(
            "read_panel",
            &json!({"panel": id.to_string(), "mode": "since_last_turn"}),
        )
        .unwrap();
        assert_eq!(
            req,
            ControlRequest::ReadPanel {
                panel: id,
                mode: super::super::ReadPanelMode::SinceLastTurn,
            }
        );
    }

    #[test]
    fn build_read_panel_turn_mode() {
        let id = PanelId::new();
        let req = build_request(
            "read_panel",
            &json!({"panel": id.to_string(), "mode": "turn", "turn": 3}),
        )
        .unwrap();
        assert_eq!(
            req,
            ControlRequest::ReadPanel {
                panel: id,
                mode: super::super::ReadPanelMode::Turn(3),
            }
        );
    }

    #[test]
    fn build_read_panel_turn_mode_requires_an_index() {
        let id = PanelId::new();
        let err = build_request(
            "read_panel",
            &json!({"panel": id.to_string(), "mode": "turn"}),
        )
        .unwrap_err();
        assert!(err.contains("requires a non-negative integer argument `turn`"));
    }

    #[test]
    fn build_read_panel_rejects_bad_mode() {
        let id = PanelId::new();
        let err = build_request(
            "read_panel",
            &json!({"panel": id.to_string(), "mode": "everything"}),
        )
        .unwrap_err();
        assert!(err.contains("invalid mode"));
    }

    #[test]
    fn build_spawn_role_request_with_overrides() {
        let req = build_request(
            "spawn_role",
            &json!({"role": "backend", "worktree": true, "agent_cli": "codex"}),
        )
        .unwrap();
        assert_eq!(
            req,
            ControlRequest::SpawnRole {
                role: "backend".into(),
                worktree: true,
                model: None,
                agent_cli: Some(AgentCli::Codex),
                prompt: None,
            }
        );
    }

    /// A free-form role: an arbitrary label plus the inline `prompt` that
    /// becomes its system prompt is decoded off the MCP tool args.
    #[test]
    fn build_spawn_role_request_with_inline_prompt() {
        let req = build_request(
            "spawn_role",
            &json!({
                "role": "perf-profiler",
                "model": "opus",
                "agent_cli": "codex",
                "prompt": "You profile hot paths and report findings."
            }),
        )
        .unwrap();
        assert_eq!(
            req,
            ControlRequest::SpawnRole {
                role: "perf-profiler".into(),
                worktree: false,
                model: Some("opus".into()),
                agent_cli: Some(AgentCli::Codex),
                prompt: Some("You profile hot paths and report findings.".into()),
            }
        );
    }

    #[test]
    fn build_request_rejects_bad_panel_id() {
        let err = build_request("ctrl_c", &json!({"panel": "not-a-ulid"})).unwrap_err();
        assert!(err.contains("invalid panel id"));
    }

    #[test]
    fn build_list_panels_takes_no_args() {
        let req = build_request("list_panels", &json!({})).unwrap();
        assert_eq!(req, ControlRequest::ListPanels);
    }

    #[test]
    fn build_register_round_request() {
        let a = PanelId::new();
        let b = PanelId::new();
        let req = build_request(
            "register_round",
            &json!({
                "panels": [a.to_string(), b.to_string()],
                "read_mode": "since_last_turn",
                "fallback_secs": 90
            }),
        )
        .unwrap();
        assert_eq!(
            req,
            ControlRequest::RegisterRound {
                panels: vec![a, b],
                read_mode: Some(super::super::ReadPanelMode::SinceLastTurn),
                fallback_secs: Some(90),
                backlog: None,
            }
        );
    }

    #[test]
    fn build_register_round_defaults_optional_fields() {
        let a = PanelId::new();
        let req = build_request("register_round", &json!({"panels": [a.to_string()]})).unwrap();
        assert_eq!(
            req,
            ControlRequest::RegisterRound {
                panels: vec![a],
                read_mode: None,
                fallback_secs: None,
                backlog: None,
            }
        );
    }

    #[test]
    fn build_register_round_parses_backlog() {
        let a = PanelId::new();
        let req = build_request(
            "register_round",
            &json!({
                "panels": [a.to_string()],
                "backlog": { a.to_string(): ["CA-SR-2", "CA-SR-3"] }
            }),
        )
        .unwrap();
        assert_eq!(
            req,
            ControlRequest::RegisterRound {
                panels: vec![a],
                read_mode: None,
                fallback_secs: None,
                backlog: Some(std::collections::HashMap::from([(
                    a,
                    vec!["CA-SR-2".to_string(), "CA-SR-3".to_string()],
                )])),
            }
        );
    }

    #[test]
    fn build_register_round_requires_panels_array() {
        let err = build_request("register_round", &json!({})).unwrap_err();
        assert!(err.contains("missing array argument `panels`"));
    }

    #[test]
    fn build_register_round_rejects_bad_panel_id() {
        let err = build_request("register_round", &json!({"panels": ["not-a-ulid"]})).unwrap_err();
        assert!(err.contains("invalid panel id"));
    }

    #[test]
    fn build_register_round_rejects_bad_backlog() {
        let a = PanelId::new();
        // A backlog whose entries are not string arrays is rejected.
        let err = build_request(
            "register_round",
            &json!({"panels": [a.to_string()], "backlog": {a.to_string(): "not-an-array"}}),
        )
        .unwrap_err();
        assert!(
            err.contains("must be an array of task strings"),
            "got: {err}"
        );
        // A backlog keyed by a non-ulid is rejected.
        let err = build_request(
            "register_round",
            &json!({"panels": [a.to_string()], "backlog": {"not-a-ulid": ["x"]}}),
        )
        .unwrap_err();
        assert!(err.contains("invalid backlog panel id"), "got: {err}");
    }

    #[test]
    fn build_read_menu_request() {
        let a = PanelId::new();
        let req = build_request("read_menu", &json!({"panel": a.to_string()})).unwrap();
        assert_eq!(req, ControlRequest::ReadMenu { panel: a });
    }

    #[test]
    fn build_select_option_request() {
        let a = PanelId::new();
        let req = build_request(
            "select_option",
            &json!({"panel": a.to_string(), "index": 2}),
        )
        .unwrap();
        assert_eq!(req, ControlRequest::SelectOption { panel: a, index: 2 });
    }

    #[test]
    fn build_select_option_requires_index() {
        let a = PanelId::new();
        let err = build_request("select_option", &json!({"panel": a.to_string()})).unwrap_err();
        assert!(err.contains("missing integer argument `index`"));
    }

    #[test]
    fn render_spawned_response_is_panel_id() {
        let id = PanelId::new();
        match render_response(ControlResponse::Spawned { panel: id }) {
            ToolOutcome::Ok(text) => assert_eq!(text, id.to_string()),
            ToolOutcome::Err(_) => panic!("expected Ok"),
        }
    }

    #[test]
    fn render_error_response_is_tool_error() {
        match render_response(ControlResponse::error("no such panel")) {
            ToolOutcome::Err(msg) => assert_eq!(msg, "no such panel"),
            ToolOutcome::Ok(_) => panic!("expected Err"),
        }
    }
}
