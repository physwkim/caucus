//! `caucus mcp-serve --control-sock <path>` — the thin stdio MCP server
//! (`docs/design.md` §0 #4).
//!
//! This process is spawned by the main worker panel's Claude Code instance (caucus
//! writes an MCP config registering it — see [`crate::mcp::serve::mcp_config_json`]).
//! It exposes the six caucus tools over stdio JSON-RPC and forwards each call
//! to the main `caucus` process over the control socket.
//!
//! It owns no panels and no PTYs — all state lives in the main process. This
//! keeps the main worker's MCP server crash-isolated from the multiplexer.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::control_client::ControlClient;
use super::jsonrpc::{McpDispatch, serve_stdio};
use super::tool_catalogue;

/// Run `caucus mcp-serve`: serve the six caucus tools over stdio, forwarding
/// each call to the control socket at `control_sock`.
///
/// Builds its own multi-thread tokio runtime — the synchronous tool-handler
/// path ([`ControlClient`]) blocks on the control-socket round-trip, which a
/// current-thread runtime would deadlock.
pub fn run(control_sock: &Path) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("start tokio runtime for mcp-serve")?;

    let client = ControlClient::new(control_sock);
    let dispatch = McpDispatch::new(tool_catalogue(), client);

    runtime.block_on(async move { serve_stdio(dispatch).await })
}

/// The MCP-config JSON registering the caucus MCP server for a Claude Code
/// instance — written into the main worker panel's worktree/cwd as `.mcp.json`
/// (`docs/design.md` §0 #4, #5).
///
/// Claude Code reads `.mcp.json` from its cwd: it registers an MCP server
/// `caucus` whose command is `caucus mcp-serve --control-sock <path>`, so the
/// main worker's claude can call `send_keys` / `read_panel` / ... on the sub-agent panels.
///
/// `caucus_bin` is the absolute path to the running `caucus` executable so the
/// spawned server is the exact same build.
pub fn mcp_config_json(caucus_bin: &Path, control_sock: &Path) -> Value {
    json!({
        "mcpServers": {
            "caucus": {
                "command": caucus_bin.display().to_string(),
                "args": [
                    "mcp-serve",
                    "--control-sock",
                    control_sock.display().to_string(),
                ],
            }
        }
    })
}

/// Write the caucus MCP config to `<dir>/.mcp.json`, returning the file path.
///
/// Called when caucus spawns the main worker panel: the main worker panel's claude picks the
/// file up from its cwd and gains the caucus tool surface.
pub fn write_mcp_config(dir: &Path, caucus_bin: &Path, control_sock: &Path) -> Result<std::path::PathBuf> {
    let path = dir.join(".mcp.json");
    let body = serde_json::to_vec_pretty(&mcp_config_json(caucus_bin, control_sock))
        .context("serialise .mcp.json")?;
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_config_registers_caucus_server() {
        let cfg = mcp_config_json(
            Path::new("/usr/local/bin/caucus"),
            Path::new("/tmp/caucus-ctl.sock"),
        );
        let server = &cfg["mcpServers"]["caucus"];
        assert_eq!(server["command"], "/usr/local/bin/caucus");
        let args: Vec<&str> = server["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert_eq!(
            args,
            vec!["mcp-serve", "--control-sock", "/tmp/caucus-ctl.sock"]
        );
    }

    #[test]
    fn write_mcp_config_creates_dot_mcp_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_mcp_config(
            dir.path(),
            Path::new("/bin/caucus"),
            Path::new("/tmp/ctl.sock"),
        )
        .unwrap();
        assert_eq!(path, dir.path().join(".mcp.json"));
        let written: Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(written["mcpServers"]["caucus"].is_object());
    }
}
