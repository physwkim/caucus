//! caucus MCP control plane — the interface caucus exposes to the main worker
//! (`docs/design.md` §0 #4, §9).
//!
//! The main worker (a Claude Code agent in one panel) drives every sub-agent
//! panel through seven MCP tools: `send_keys`, `ctrl_c`, `read_panel`,
//! `spawn_role`, `kill_panel`, `list_panels`, `wait_for_panels`.
//!
//! ## Architecture
//!
//! Two processes, two hops:
//!
//! 1. **`caucus mcp-serve`** ([`serve`]) — a thin stdio MCP server the main
//!    worker's Claude Code instance spawns. It speaks JSON-RPC 2.0 over stdio
//!    ([`jsonrpc`]) and forwards each tool call as a [`protocol::ControlRequest`]
//!    over the *control socket* ([`control_client`]).
//! 2. **The main `caucus` process** owns the control socket
//!    ([`control_server`]); its accept task queues each request as a
//!    [`control_server::ControlJob`] for the [`crate::session::Multiplexer`]
//!    event loop, which executes it against live panels (Invariant I-5's
//!    single-owner discipline) and answers through the job's oneshot.
//!
//! ## MCP transport: hand-rolled, not `rmcp`
//!
//! `rmcp` (1.7.0) resolves cleanly but its server surface is macro-driven and
//! its transport runs an internal loop that resists deterministic unit
//! testing. The MCP slice caucus needs is small — `initialize` / `tools/list`
//! / `tools/call`, seven tools — so [`jsonrpc`] implements exactly that, with a
//! pure dispatch core. See that module's header for the rationale.

pub mod control_client;
pub mod control_server;
pub mod jsonrpc;
pub mod protocol;
pub mod serve;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::role::spec::AgentCli;
use crate::session::id::PanelId;

use jsonrpc::ToolDef;

/// Which slice of a panel's captured output `read_panel` should return
/// (`docs/design.md` §8.5).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadPanelMode {
    /// The currently visible grid viewport.
    Screen,
    /// The whole scrollback buffer.
    Scrollback,
    /// All output since the last `PromptDelivered` — "what this agent just did".
    SinceLastTurn,
    /// Only the agent's final message, as carried by the turn signal.
    LastMessage,
}

/// One panel's status row, returned by `list_panels`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PanelSummary {
    pub panel_id: PanelId,
    pub role: String,
    /// Derived state, lower-cased (`working` / `idle` / `blocked_*` / `exited`).
    pub state: String,
    pub agent_cli: AgentCli,
}

/// Errors surfaced by MCP tool calls.
#[derive(Debug, Error)]
pub enum McpError {
    #[error("no such panel: {0}")]
    NoSuchPanel(PanelId),
    #[error("mcp tool failed: {0}")]
    Tool(String),
}

/// The tool surface caucus exposes to the main worker over MCP.
///
/// Implemented by [`crate::session::Multiplexer`]: the live panel registry is
/// the real backing store. The control-socket server routes each
/// [`protocol::ControlRequest`] into one of these methods.
pub trait McpToolSurface {
    /// Type keys into a panel's PTY (the live round mechanism, `docs/design.md`
    /// §4). When `enter` is set, a trailing newline is appended.
    fn send_keys(&mut self, panel: PanelId, text: &str, enter: bool) -> Result<(), McpError>;

    /// Send `Ctrl-C` (interrupt) to a panel's PTY.
    fn ctrl_c(&mut self, panel: PanelId) -> Result<(), McpError>;

    /// Read a panel's captured output in the requested `mode` (`docs/design.md`
    /// §8.5).
    fn read_panel(&self, panel: PanelId, mode: ReadPanelMode) -> Result<String, McpError>;

    /// Spawn a new panel for `role`. `worktree` requests an execute-phase
    /// worktree; `model`/`agent_cli` are main worker overrides (`docs/design.md` §5).
    fn spawn_role(
        &mut self,
        role: &str,
        worktree: bool,
        model: Option<&str>,
        agent_cli: Option<AgentCli>,
    ) -> Result<PanelId, McpError>;

    /// Kill a panel; its worktree (if any) is enqueued for cleanup.
    fn kill_panel(&mut self, panel: PanelId) -> Result<(), McpError>;

    /// List every live panel with its derived state.
    fn list_panels(&self) -> Vec<PanelSummary>;
}

/// The MCP tools caucus exposes to the main worker (`docs/design.md` §0 #4).
///
/// One catalogue, shared by [`jsonrpc::McpDispatch`] (the `tools/list`
/// response) and the control-socket request decoder ([`control_client`]).
pub fn tool_catalogue() -> Vec<ToolDef> {
    /// JSON-Schema for a required panel-id string argument.
    fn panel_prop() -> Value {
        json!({ "type": "string", "description": "Target panel id (a ULID)." })
    }
    vec![
        ToolDef {
            name: "send_keys",
            description: "Type text into a panel's terminal. With enter=true a \
                          newline is appended — the live way to deliver a prompt \
                          or a slash command (/compact, /clear) to that agent.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "panel": panel_prop(),
                    "text": { "type": "string", "description": "Text to type." },
                    "enter": {
                        "type": "boolean",
                        "description": "Append a newline (submit the line).",
                        "default": false
                    }
                },
                "required": ["panel", "text"]
            }),
        },
        ToolDef {
            name: "ctrl_c",
            description: "Send Ctrl-C (interrupt) to a panel's terminal — stop a \
                          runaway turn or cancel a prompt.",
            input_schema: json!({
                "type": "object",
                "properties": { "panel": panel_prop() },
                "required": ["panel"]
            }),
        },
        ToolDef {
            name: "read_panel",
            description: "Read a panel's captured output. mode: 'screen' (visible \
                          grid), 'scrollback' (full scrollback), 'since_last_turn' \
                          (everything since the last prompt — the whole turn, no \
                          racing the screen), 'last_message' (the agent's final \
                          message from its turn signal).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "panel": panel_prop(),
                    "mode": {
                        "type": "string",
                        "enum": ["screen", "scrollback", "since_last_turn", "last_message"],
                        "description": "Which output slice to return."
                    }
                },
                "required": ["panel", "mode"]
            }),
        },
        ToolDef {
            name: "spawn_role",
            description: "Spawn a new panel running the given role. worktree=true \
                          gives the new agent a dedicated git worktree as its cwd. \
                          model and agent_cli override the role defaults.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "role": { "type": "string", "description": "Role name (e.g. backend)." },
                    "worktree": {
                        "type": "boolean",
                        "description": "Create a dedicated git worktree for the panel.",
                        "default": false
                    },
                    "model": { "type": "string", "description": "Model override." },
                    "agent_cli": {
                        "type": "string",
                        "enum": ["claude", "codex", "gemini"],
                        "description": "Backend CLI override."
                    }
                },
                "required": ["role"]
            }),
        },
        ToolDef {
            name: "kill_panel",
            description: "Kill a panel: terminate its agent process and enqueue \
                          any worktree for cleanup.",
            input_schema: json!({
                "type": "object",
                "properties": { "panel": panel_prop() },
                "required": ["panel"]
            }),
        },
        ToolDef {
            name: "list_panels",
            description: "List every live panel with its role and derived state \
                          (working / idle / blocked_* / exited).",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "wait_for_panels",
            description: "Block until the named panels all settle (finish their \
                          turn — leave the 'working' state) or timeout_secs \
                          elapses (default 600). Returns each panel's final role \
                          + state. Use this instead of sleep-polling list_panels.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "panels": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Panel ids (ULIDs) to wait on."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Max seconds to block before returning \
                                        (default 600, max 3600)."
                    }
                },
                "required": ["panels"]
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_panel_mode_serde_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&ReadPanelMode::SinceLastTurn).unwrap(),
            "\"since_last_turn\""
        );
    }

    #[test]
    fn tool_catalogue_has_every_tool() {
        let names: Vec<&str> = tool_catalogue().iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "send_keys",
                "ctrl_c",
                "read_panel",
                "spawn_role",
                "kill_panel",
                "list_panels",
                "wait_for_panels",
            ]
        );
    }

    #[test]
    fn every_tool_has_an_object_schema() {
        for tool in tool_catalogue() {
            assert_eq!(
                tool.input_schema["type"], "object",
                "tool {} schema must be an object",
                tool.name
            );
        }
    }
}
