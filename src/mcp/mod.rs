//! caucus MCP server — the control interface caucus exposes to the CEO
//! (`docs/design.md` §0 #4, §9).
//!
//! The CEO (a Claude Code agent in one panel) drives every other panel
//! through these tools: `send_keys`, `ctrl_c`, `read_panel`, `spawn_role`,
//! `kill_panel`, `list_panels`.
//!
//! **Implementation note.** `rmcp` (the official Rust MCP SDK) resolves
//! cleanly on crates.io (v1.7.0), but wiring its macro-driven server/transport
//! surface is Phase 2 work and would not add value to a compiling skeleton.
//! This module defines the tool surface as a plain [`McpToolSurface`] trait so
//! parallel agents have a stable contract; the transport is stubbed.
//
// TODO: wire rmcp — add `rmcp` back to Cargo.toml and implement
// `McpToolSurface` behind an `rmcp` `ServerHandler` in Phase 2.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::role::spec::AgentCli;
use crate::session::id::PanelId;

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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// The tool surface caucus exposes to the CEO over MCP.
///
/// Phase 2 implements this against the live panel registry and serves it via
/// `rmcp`. Defining it as a trait now gives parallel agents a stable contract.
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
    /// worktree; `model`/`agent_cli` are CEO overrides (`docs/design.md` §5).
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

/// Placeholder MCP server. Phase 2 replaces this with an `rmcp`-backed server
/// bound to the live panel registry.
pub struct McpServer {
    // TODO(phase 2): hold a handle to the panel registry and the rmcp server.
}

impl McpServer {
    /// Construct the (stub) MCP server.
    pub fn new() -> Self {
        Self {}
    }

    /// Serve MCP requests. Phase 2 wires the rmcp transport.
    pub async fn serve(&self) -> Result<(), McpError> {
        // TODO(phase 2): wire rmcp — bind the transport, register the six
        // tools, dispatch into a `McpToolSurface`.
        todo!("phase 2: wire rmcp MCP server")
    }
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
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
}
