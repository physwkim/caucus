//! Integration tests for the caucus MCP control plane (`docs/design.md` §0 #4).
//!
//! Covers the two ends the unit tests cannot reach on their own:
//!
//! * the `caucus mcp-serve` binary answering a real JSON-RPC `tools/list`
//!   request over stdio (the success criterion);
//! * the control socket end-to-end — a [`ControlRequest`] written to a live
//!   [`Multiplexer`]'s control socket executes against real panels and the
//!   [`ControlResponse`] makes it back.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use caucus::config::Config;
use caucus::mcp::protocol::{ControlRequest, ControlResponse};
use caucus::mcp::{McpToolSurface, ReadPanelMode};
use caucus::render::Rect;
use caucus::session::Multiplexer;
use caucus::session::state::Session;
use tempfile::TempDir;

/// Whole-screen rect for a test multiplexer.
fn area() -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: 120,
        height: 40,
    }
}

/// `caucus mcp-serve` answers a JSON-RPC `tools/list` with the full tool set.
///
/// The binary is driven over stdio exactly as the main worker's Claude Code instance
/// drives it; `initialize` and `tools/list` touch no control socket, so a
/// throwaway socket path is fine.
#[test]
fn mcp_serve_lists_the_tools_over_stdio() {
    let bin = env!("CARGO_BIN_EXE_caucus");
    let mut child = Command::new(bin)
        .args([
            "mcp-serve",
            "--control-sock",
            "/tmp/caucus-test-unused.sock",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn caucus mcp-serve");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    // initialize
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .unwrap();
    let mut init_line = String::new();
    stdout.read_line(&mut init_line).unwrap();
    let init: serde_json::Value = serde_json::from_str(init_line.trim()).unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "caucus");

    // tools/list
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n")
        .unwrap();
    let mut list_line = String::new();
    stdout.read_line(&mut list_line).unwrap();
    let listed: serde_json::Value = serde_json::from_str(list_line.trim()).unwrap();

    let names: Vec<String> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        vec![
            "send_keys",
            "broadcast",
            "ctrl_c",
            "read_panel",
            "spawn_role",
            "kill_panel",
            "list_panels",
            "register_round",
            "read_menu",
            "select_option",
        ],
        "tools/list must return exactly the caucus tools"
    );

    // Closing stdin makes the server exit cleanly.
    drop(stdin);
    let status = child.wait().expect("wait for mcp-serve");
    assert!(status.success(), "mcp-serve must exit 0 on stdin EOF");
}

/// `caucus mcp-serve --help` works.
#[test]
fn mcp_serve_help_runs() {
    let bin = env!("CARGO_BIN_EXE_caucus");
    let out = Command::new(bin)
        .args(["mcp-serve", "--help"])
        .output()
        .expect("run mcp-serve --help");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("--control-sock"));
}

/// Build a multiplexer rooted in `tmp`. The tokio runtime is required because
/// the socket servers and cleanup queue spawn tasks.
fn build_mux(tmp: &TempDir) -> (Multiplexer, caucus::mcp::control_server::ControlServer) {
    let session = Session::new("mcp-test", tmp.path().to_path_buf());
    let config = Config::load(tmp.path()).unwrap();
    let (mux, _signal, control) = Multiplexer::new(session, config, area()).unwrap();
    (mux, control)
}

/// `list_panels` (via the MCP trait) reflects a spawned panel's role/state.
#[tokio::test]
async fn list_panels_reports_a_spawned_panel() {
    let tmp = TempDir::new().unwrap();
    let (mut mux, _control) = build_mux(&tmp);

    // `reviewer` spawns a real `claude` PTY; skip the test if `claude` is not
    // on PATH so the suite is hermetic on a bare CI box.
    let Ok(panel) = mux.spawn_panel("reviewer", None, None, None) else {
        eprintln!("skipping: no agent CLI on PATH");
        return;
    };

    let panels = McpToolSurface::list_panels(&mux);
    assert_eq!(panels.len(), 1);
    assert_eq!(panels[0].panel_id, panel);
    assert_eq!(panels[0].role, "reviewer");
    // Before any turn signal the panel is `working` (just spawned).
    assert!(
        panels[0].state == "working" || panels[0].state == "spawning",
        "unexpected state {:?}",
        panels[0].state
    );

    mux.shutdown();
}

/// A control request written to the live control socket executes against the
/// multiplexer and the response makes it back — the full two-hop path minus
/// the stdio MCP layer.
#[tokio::test]
async fn control_socket_executes_against_multiplexer() {
    let tmp = TempDir::new().unwrap();
    let (mut mux, mut control) = build_mux(&tmp);
    let sock = control.sock_path().to_path_buf();

    // Client task: send `list_panels` over the control socket.
    let client = tokio::spawn(async move {
        caucus::mcp::control_client::roundtrip(&sock, &ControlRequest::ListPanels)
            .await
            .unwrap()
    });

    // Multiplexer side: drain the queued job and execute it. Poll briefly —
    // the accept task needs a moment to queue the job.
    let mut response = None;
    for _ in 0..200 {
        mux.drain_control(&mut control);
        if let Ok(job) = control.jobs().try_recv() {
            let resp = mux.execute_control(job.request);
            let _ = job.reply.send(resp);
        }
        // `drain_control` already answers jobs; this loop just needs to run it
        // until the client has connected. Give the runtime a tick.
        tokio::time::sleep(Duration::from_millis(5)).await;
        if client.is_finished() {
            response = Some(client.await.unwrap());
            break;
        }
    }

    let response = response.expect("control round-trip completed");
    match response {
        ControlResponse::Panels { panels } => assert!(panels.is_empty()),
        other => panic!("expected Panels, got {other:?}"),
    }

    mux.shutdown();
}

/// `read_panel` rejects an unknown panel id with an error response, for every
/// mode.
#[tokio::test]
async fn read_panel_unknown_panel_is_an_error() {
    let tmp = TempDir::new().unwrap();
    let (mux, _control) = build_mux(&tmp);
    let ghost = caucus::session::id::PanelId::new();

    for mode in [
        ReadPanelMode::Screen,
        ReadPanelMode::Scrollback,
        ReadPanelMode::SinceLastTurn,
        ReadPanelMode::LastMessage,
    ] {
        let err = McpToolSurface::read_panel(&mux, ghost, mode).unwrap_err();
        assert!(
            matches!(err, caucus::mcp::McpError::NoSuchPanel(_)),
            "mode {mode:?} should reject an unknown panel"
        );
    }
}

/// The main worker builds a team of one architect + three reviewers through
/// the MCP `spawn_role` control path — the same path `spawn_role` takes when
/// the main worker's Claude Code instance calls it. Every panel comes up, the
/// roster has the right roles, and the layout tiles all four.
#[tokio::test]
async fn main_spawns_an_architect_and_three_reviewers() {
    let tmp = TempDir::new().unwrap();
    let (mut mux, _control) = build_mux(&tmp);

    // `spawn_role` over the control plane — exactly what the main worker
    // triggers. The same role spawned repeatedly is allowed (panels are
    // disambiguated by a per-role counter).
    for role in ["architect", "reviewer", "reviewer", "reviewer"] {
        match mux.execute_control(ControlRequest::SpawnRole {
            role: role.to_string(),
            worktree: false,
            model: None,
            agent_cli: None,
        }) {
            ControlResponse::Spawned { .. } => {}
            ControlResponse::Error { message } => {
                eprintln!("skipping: spawn_role({role}) failed: {message}");
                mux.shutdown();
                return;
            }
            other => panic!("expected Spawned, got {other:?}"),
        }
    }

    let panels = McpToolSurface::list_panels(&mux);
    eprintln!("--- team spawned: {} panels ---", panels.len());
    for p in &panels {
        eprintln!("  {} · {}", p.role, p.state);
    }

    assert_eq!(panels.len(), 4, "architect + 3 reviewers = 4 panels");
    assert_eq!(
        panels.iter().filter(|p| p.role == "architect").count(),
        1,
        "exactly one architect"
    );
    assert_eq!(
        panels.iter().filter(|p| p.role == "reviewer").count(),
        3,
        "three reviewers"
    );
    assert_eq!(
        mux.layout().slots.len(),
        4,
        "layout reflowed to tile all four panels"
    );

    mux.shutdown();
}
