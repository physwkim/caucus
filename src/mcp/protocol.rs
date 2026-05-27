//! Control-socket wire protocol (`docs/design.md` §0 #4, §9).
//!
//! The main `caucus` process opens a *control socket* — a unix-domain socket
//! distinct from the turn-signal socket. The thin `caucus mcp-serve` process
//! (spawned by the main worker's Claude Code instance) connects to it and forwards
//! each MCP tool call as one [`ControlRequest`]; the main process executes it
//! against the live [`crate::session::Multiplexer`] and writes back one
//! [`ControlResponse`].
//!
//! The framing is newline-delimited JSON: exactly one request and one response
//! per connection (request/response, no pipelining), which keeps the
//! [`crate::mcp::control_server`] event-loop integration trivial — one queued
//! op, one reply.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::role::spec::AgentCli;
use crate::session::id::PanelId;

use super::{PanelSummary, ReadPanelMode};

/// One control-socket request: an MCP tool call forwarded from `mcp-serve`.
///
/// `snake_case` tag mirrors the MCP tool names exposed to the main worker so a wire
/// dump is self-describing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlRequest {
    /// Type `text` into a panel's PTY; append a newline when `enter`.
    SendKeys {
        panel: PanelId,
        text: String,
        #[serde(default)]
        enter: bool,
    },
    /// Type the same `text` into several panels' PTYs at once — a round's
    /// fan-out. Equivalent to one [`ControlRequest::SendKeys`] per panel;
    /// executed synchronously like `SendKeys`, not deferred.
    Broadcast {
        panels: Vec<PanelId>,
        text: String,
        #[serde(default)]
        enter: bool,
    },
    /// Send `Ctrl-C` (0x03) to a panel's PTY.
    CtrlC { panel: PanelId },
    /// Read a panel's captured output in `mode`.
    ReadPanel { panel: PanelId, mode: ReadPanelMode },
    /// Spawn a new panel for `role`. `role` is a free-form label — a known
    /// preset name (`worker`, `reviewer`, …) reuses that preset's tool
    /// allowlist and permission mode, any other name is an ad-hoc role built on
    /// the generic `worker` defaults. `worktree` requests an execute-phase
    /// worktree; `model` / `agent_cli` are main worker overrides; `prompt` is an
    /// inline system prompt that, when set, *is* the role's instructions
    /// (replacing the preset's prompt template) — the mechanism by which the
    /// main worker invents a role on the fly.
    SpawnRole {
        role: String,
        #[serde(default)]
        worktree: bool,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        agent_cli: Option<AgentCli>,
        #[serde(default)]
        prompt: Option<String>,
    },
    /// Kill a panel; its worktree (if any) is enqueued for cleanup.
    KillPanel { panel: PanelId },
    /// List every live panel with role + derived state.
    ListPanels,
    /// Register a *round*: caucus watches `panels` and, once they have all
    /// settled (left `Working`/`Spawning`) — or `fallback_secs` elapses —
    /// assembles their results and injects them into the main worker's panel
    /// as a fresh turn. Answered *immediately* with a snapshot of the named
    /// panels: the round runs in the background, so the main worker ends its
    /// turn and is re-prompted by caucus on completion (no blocking, no
    /// timeout-shaped wait). `read_mode` selects what each panel's result is
    /// read as on delivery (default `last_message`).
    ///
    /// `backlog` is an optional per-panel task queue keyed by panel id: while
    /// the round runs, a panel that goes idle with tasks still queued is fed
    /// its next task (so an early finisher keeps working instead of idling
    /// until the barrier); the panel settles for the round only once it is
    /// idle *and* its queue is empty. A panel absent from `backlog` settles on
    /// its first idle, the original one-task-per-panel behaviour.
    /// (`crate::session::runtime::Multiplexer::poll_pending_rounds`).
    RegisterRound {
        panels: Vec<PanelId>,
        #[serde(default)]
        read_mode: Option<ReadPanelMode>,
        #[serde(default)]
        fallback_secs: Option<u64>,
        #[serde(default)]
        backlog: Option<HashMap<PanelId, Vec<String>>>,
    },
    /// Read the interactive selection menu shown in a panel (if any) as
    /// readable text. Answered with a [`ControlResponse::Panel`].
    ReadMenu { panel: PanelId },
    /// Pick option `index` (the displayed 1-based number) in a panel's
    /// selection menu: caucus navigates the chooser there and presses Enter.
    SelectOption { panel: PanelId, index: usize },
}

/// One control-socket response — the result of executing a [`ControlRequest`]
/// against the live multiplexer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ControlResponse {
    /// `send_keys` / `ctrl_c` / `kill_panel` succeeded — no payload.
    Ok,
    /// `read_panel` succeeded; `text` is the captured slice.
    Panel { text: String },
    /// `spawn_role` succeeded; `panel` is the new panel id.
    Spawned { panel: PanelId },
    /// `list_panels` succeeded.
    Panels { panels: Vec<PanelSummary> },
    /// The tool call failed; `message` is human-readable.
    Error { message: String },
}

impl ControlResponse {
    /// Build an error response from any displayable error.
    pub fn error(message: impl std::fmt::Display) -> Self {
        Self::Error {
            message: message.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_keys_round_trips() {
        let req = ControlRequest::SendKeys {
            panel: PanelId::new(),
            text: "/compact".into(),
            enter: true,
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"op\":\"send_keys\""));
        let back: ControlRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn broadcast_round_trips() {
        let req = ControlRequest::Broadcast {
            panels: vec![PanelId::new(), PanelId::new()],
            text: "the agenda".into(),
            enter: true,
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"op\":\"broadcast\""));
        let back: ControlRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn broadcast_enter_defaults_to_false() {
        // `enter` defaults to false when omitted from the wire form.
        let id = PanelId::new();
        let req: ControlRequest = serde_json::from_str(&format!(
            r#"{{"op":"broadcast","panels":["{id}"],"text":"hi"}}"#
        ))
        .unwrap();
        assert_eq!(
            req,
            ControlRequest::Broadcast {
                panels: vec![id],
                text: "hi".into(),
                enter: false,
            }
        );
    }

    #[test]
    fn read_panel_carries_mode() {
        let req = ControlRequest::ReadPanel {
            panel: PanelId::new(),
            mode: ReadPanelMode::SinceLastTurn,
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("since_last_turn"));
        let back: ControlRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn spawn_role_defaults_are_optional() {
        // A minimal `spawn_role` request with only `role` parses (serde
        // defaults fill worktree/model/agent_cli/prompt).
        let req: ControlRequest =
            serde_json::from_str(r#"{"op":"spawn_role","role":"backend"}"#).unwrap();
        assert_eq!(
            req,
            ControlRequest::SpawnRole {
                role: "backend".into(),
                worktree: false,
                model: None,
                agent_cli: None,
                prompt: None,
            }
        );
    }

    #[test]
    fn spawn_role_carries_an_inline_prompt() {
        // A free-form role: an arbitrary label plus the inline `prompt` that
        // becomes its system prompt.
        let req: ControlRequest = serde_json::from_str(
            r#"{"op":"spawn_role","role":"perf-profiler","prompt":"You profile hot paths."}"#,
        )
        .unwrap();
        assert_eq!(
            req,
            ControlRequest::SpawnRole {
                role: "perf-profiler".into(),
                worktree: false,
                model: None,
                agent_cli: None,
                prompt: Some("You profile hot paths.".into()),
            }
        );
    }

    #[test]
    fn register_round_round_trips() {
        let panel = PanelId::new();
        let req = ControlRequest::RegisterRound {
            panels: vec![panel, PanelId::new()],
            read_mode: Some(ReadPanelMode::SinceLastTurn),
            fallback_secs: Some(120),
            backlog: Some(HashMap::from([(
                panel,
                vec!["CA-SR-2".to_string(), "CA-SR-3".to_string()],
            )])),
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"op\":\"register_round\""));
        let back: ControlRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn register_round_optional_fields_default_to_none() {
        // `read_mode`, `fallback_secs`, and `backlog` default to None when omitted.
        let id = PanelId::new();
        let req: ControlRequest =
            serde_json::from_str(&format!(r#"{{"op":"register_round","panels":["{id}"]}}"#))
                .unwrap();
        assert_eq!(
            req,
            ControlRequest::RegisterRound {
                panels: vec![id],
                read_mode: None,
                fallback_secs: None,
                backlog: None,
            }
        );
    }

    #[test]
    fn read_menu_round_trips() {
        let req = ControlRequest::ReadMenu {
            panel: PanelId::new(),
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"op\":\"read_menu\""));
        let back: ControlRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn select_option_carries_index() {
        let req = ControlRequest::SelectOption {
            panel: PanelId::new(),
            index: 3,
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"op\":\"select_option\""));
        assert!(line.contains("\"index\":3"));
        let back: ControlRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn response_variants_round_trip() {
        for resp in [
            ControlResponse::Ok,
            ControlResponse::Panel {
                text: "hello".into(),
            },
            ControlResponse::Spawned {
                panel: PanelId::new(),
            },
            ControlResponse::Panels { panels: vec![] },
            ControlResponse::error("boom"),
        ] {
            let line = serde_json::to_string(&resp).unwrap();
            let back: ControlResponse = serde_json::from_str(&line).unwrap();
            assert_eq!(resp, back);
        }
    }
}
