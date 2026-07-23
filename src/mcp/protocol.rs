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

use serde::{Deserialize, Serialize};

use crate::role::spec::AgentCli;
use crate::session::id::PanelId;

use super::{PanelSummary, ReadPanelMode};

/// Main-supplied hints for auto-answering a watched worker's selection menu
/// without disturbing the main worker. Attached to a worker's watch via
/// [`ControlRequest::QueueTasks`] and consulted by
/// [`crate::session::Multiplexer::poll_round_blocked_panels`] when a watched
/// panel stops mid-turn on an `AskUserQuestion`-style chooser.
///
/// An option *qualifies* when its label contains (ASCII-case-insensitively) at
/// least one `prefer` keyword (an empty `prefer` passes every option) **and**
/// none of the `avoid` keywords. caucus auto-selects an option **only when
/// exactly one qualifies** — zero or several qualifying options fall back to
/// the existing "ask the main worker" notice. This keeps the narrowed
/// invariant *"caucus never auto-answers a prompt the main worker did not
/// pre-authorize"*: the main worker pre-authorizes by naming the keywords, and
/// caucus acts only when they single out one option.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionPolicy {
    /// Keywords that *select* an option (case-insensitive substring of its
    /// label). Empty means "every option passes the positive test", so `avoid`
    /// alone decides — useful for "pick whatever, just not these".
    #[serde(default)]
    pub prefer: Vec<String>,
    /// Keywords that *veto* an option (case-insensitive substring of its
    /// label). An option whose label matches any of these never qualifies.
    #[serde(default)]
    pub avoid: Vec<String>,
}

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
    /// Send a single raw key to a panel's PTY — the escape hatch for keys
    /// `SendKeys` text cannot express (`esc`, arrows, `ctrl-*`, `tab`, function
    /// keys). `key` is a human-readable key name decoded by
    /// [`crate::input::parse_key_name`]. Unlike `SendKeys` with `enter`, this
    /// does no turn/`Working` bookkeeping — it is a bare keystroke.
    SendKey { panel: PanelId, key: String },
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
    /// Restart a sub-agent panel in place: tear it down and respawn a fresh
    /// agent resuming the same conversation in the same worktree. Answered with
    /// the NEW panel id ([`ControlResponse::Spawned`]). The main worker panel
    /// cannot be restarted.
    RestartPanel { panel: PanelId },
    /// List every live panel with role + derived state.
    ListPanels,
    /// Queue follow-up tasks on one worker, and set how caucus watches it.
    ///
    /// caucus already watches every panel it hands a prompt to, so this is not
    /// how a worker becomes visible — it is how the main worker keeps one busy
    /// across several tasks and tunes the watch. `tasks` is an ordered queue:
    /// the panel is fed its next task each time it goes idle (so an early
    /// finisher keeps working instead of reporting a half-done assignment), and
    /// it settles — i.e. its result is delivered — only once it is idle *and*
    /// the queue is empty. An empty `tasks` sets the options alone.
    ///
    /// `read_mode` selects what the worker's result is read as, captured the
    /// instant it settles (default `last_message`). `fallback_secs` is the
    /// safety net: a worker that never settles is delivered anyway after it,
    /// marked still-working. `selection_hints` lets caucus auto-answer that
    /// worker's selection menus without interrupting the main worker — see
    /// [`SelectionPolicy`]; absent (default `None`) escalates every menu.
    ///
    /// Answered with a [`ControlResponse::Panel`] describing the resulting
    /// watch, or an error if the panel is unknown or is the main panel.
    QueueTasks {
        panel: PanelId,
        #[serde(default)]
        tasks: Vec<String>,
        #[serde(default)]
        read_mode: Option<ReadPanelMode>,
        #[serde(default)]
        fallback_secs: Option<u64>,
        #[serde(default)]
        selection_hints: Option<SelectionPolicy>,
    },
    /// Collect every watched worker that is ready right now. This is the
    /// **pull** half of delivery.
    ///
    /// Each worker that has settled (or passed its fallback deadline) is
    /// answered with its own report and its watch is spent; the workers still
    /// running are reported as a status line instead — settled vs working vs
    /// draining backlog, the backlog remaining, and the seconds left until the
    /// fallback deadline — and stay watched. There is no barrier: the early
    /// finishers come back while the slow ones are still working. Each status is
    /// read from the settle latch, so a worker that finished and was later
    /// re-woken still reads `settled`.
    ///
    /// Both halves exist because the push
    /// (`crate::session::runtime::Multiplexer::poll_worker_watches`) types the
    /// report into the main panel and so can only fire while that panel is
    /// *idle* — i.e. after the main worker ends its turn. A main worker that
    /// stays in its turn polling is `Working` throughout, so the push can never
    /// land: main waits for its workers and the workers wait for main to stop
    /// waiting. The fallback deadline does not break that livelock — it makes a
    /// worker *due*, not *deliverable*. Handing the report to the poller does.
    ///
    /// Answered with a [`ControlResponse::Panel`] (text). A worker is delivered
    /// exactly once across both halves, because delivering it removes its watch.
    CollectWorkers,
    /// Stop watching one worker: caucus drops its watch, stops feeding it any
    /// queued backlog, and never delivers its result. The panel keeps running —
    /// only the watch is dropped, and a later prompt re-arms one.
    /// Answered with [`ControlResponse::Ok`], or an error if it is not watched.
    StopWatching { panel: PanelId },
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
    fn send_key_round_trips() {
        let req = ControlRequest::SendKey {
            panel: PanelId::new(),
            key: "ctrl-c".into(),
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"op\":\"send_key\""));
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
    fn read_panel_turn_mode_round_trips() {
        // The `turn` mode carries its index; the wire form tags it as a map
        // variant, distinct from the string-valued unit modes.
        let req = ControlRequest::ReadPanel {
            panel: PanelId::new(),
            mode: ReadPanelMode::Turn(7),
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"turn\":7"));
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
    fn queue_tasks_round_trips() {
        let req = ControlRequest::QueueTasks {
            panel: PanelId::new(),
            tasks: vec!["CA-SR-2".to_string(), "CA-SR-3".to_string()],
            read_mode: Some(ReadPanelMode::SinceLastTurn),
            fallback_secs: Some(120),
            selection_hints: Some(SelectionPolicy {
                prefer: vec!["structural".to_string()],
                avoid: vec!["broad refactor".to_string()],
            }),
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"op\":\"queue_tasks\""));
        let back: ControlRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn queue_tasks_optional_fields_default() {
        // `tasks` defaults to empty and the three option fields to None.
        let id = PanelId::new();
        let req: ControlRequest =
            serde_json::from_str(&format!(r#"{{"op":"queue_tasks","panel":"{id}"}}"#)).unwrap();
        assert_eq!(
            req,
            ControlRequest::QueueTasks {
                panel: id,
                tasks: vec![],
                read_mode: None,
                fallback_secs: None,
                selection_hints: None,
            }
        );
    }

    #[test]
    fn selection_policy_defaults_empty_keyword_lists() {
        // Either keyword list may be omitted; each defaults to an empty Vec.
        let policy: SelectionPolicy = serde_json::from_str(r#"{"prefer":["structural"]}"#).unwrap();
        assert_eq!(policy.prefer, vec!["structural".to_string()]);
        assert!(policy.avoid.is_empty());
        // And a full round-trip preserves both.
        let full = SelectionPolicy {
            prefer: vec!["structural".to_string(), "at source".to_string()],
            avoid: vec!["rewrite".to_string()],
        };
        let line = serde_json::to_string(&full).unwrap();
        assert_eq!(
            serde_json::from_str::<SelectionPolicy>(&line).unwrap(),
            full
        );
    }

    #[test]
    fn restart_panel_round_trips() {
        let req = ControlRequest::RestartPanel {
            panel: PanelId::new(),
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"op\":\"restart_panel\""));
        let back: ControlRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn collect_workers_and_stop_watching_round_trip() {
        for req in [
            ControlRequest::CollectWorkers,
            ControlRequest::StopWatching {
                panel: PanelId::new(),
            },
        ] {
            let line = serde_json::to_string(&req).unwrap();
            let back: ControlRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(req, back);
        }
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
