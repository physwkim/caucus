//! Control-socket wire protocol (`docs/design.md` §0 #4, §9).
//!
//! The main `caucus` process opens a *control socket* — a unix-domain socket
//! distinct from the turn-signal socket. Its clients are `caucus mcp-serve`
//! (the main worker's MCP server), `caucus ctl`, and any script that speaks
//! the framing below; the main process executes each call against the live
//! [`crate::session::Multiplexer`].
//!
//! Newline-delimited JSON, one call per connection:
//!
//! - call ([`ControlCall`]): `{"id": "<string>", "method": "<tool>",
//!   "params": {...}}`. `method` is an MCP tool name and `params` that tool's
//!   arguments, so every client shares one schema; [`ControlRequest::from_call`]
//!   decodes them.
//! - reply ([`ControlReply`]): `{"id": ..., "result": {"type": ..., ...}}` or
//!   `{"id": ..., "error": {"code": ..., "message": ...}}`.
//!
//! `subscribe_events` is the one method that keeps the connection open: its
//! `subscribed` result is followed by one [`ControlEvent`] line per panel
//! event ([`super::events`]) until the client closes the connection or falls
//! behind, which ends the stream with an [`ErrorCode::EventsLost`] error.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::role::spec::AgentCli;
use crate::session::id::PanelId;

use super::events::PanelEvent;
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

/// One control-socket call as it travels on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlCall {
    /// Caller-chosen id, echoed on the reply.
    pub id: String,
    /// An MCP tool name (`list_panels`, `send_keys`, …).
    pub method: String,
    /// That tool's arguments; absent or `null` reads as none.
    #[serde(default)]
    pub params: Value,
}

/// One decoded control call ([`ControlRequest::from_call`]), executed by
/// [`crate::session::Multiplexer::execute_control`].
#[derive(Debug, Clone, PartialEq)]
pub enum ControlRequest {
    /// Type `text` into a panel's PTY; append a newline when `enter`.
    SendKeys {
        panel: PanelId,
        text: String,
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
        worktree: bool,
        model: Option<String>,
        agent_cli: Option<AgentCli>,
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
    /// safety net: a worker still mid-turn whose screen has not changed for
    /// that long is reported as possibly stuck (no result yet, its last note,
    /// the tasks still queued), once per silence, and stays watched with its
    /// queue intact. `selection_hints` lets caucus auto-answer that
    /// worker's selection menus without interrupting the main worker — see
    /// [`SelectionPolicy`]; absent (default `None`) escalates every menu.
    ///
    /// Answered with a [`ControlResponse::Panel`] describing the resulting
    /// watch, or an error if the panel is unknown or is the main panel.
    QueueTasks {
        panel: PanelId,
        tasks: Vec<String>,
        read_mode: Option<ReadPanelMode>,
        fallback_secs: Option<u64>,
        selection_hints: Option<SelectionPolicy>,
    },
    /// Collect every watched worker that is ready right now. This is the
    /// **pull** half of delivery.
    ///
    /// Each worker that has settled is answered with its own report and its
    /// watch is spent; one whose stall alarm is due is answered with an
    /// interim notice and stays watched; the workers still running are
    /// reported as a status line instead — settled vs working vs draining
    /// backlog, the backlog remaining, and the seconds of screen silence left
    /// before a stall alarm — and stay watched. There is no barrier: the early
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
    /// waiting. The stall alarm does not break that livelock — it makes a
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
    /// Hold the reply until `panel` stops working: its `list_panels` state
    /// leaves `working` / `spawning`. `timeout_secs` defaults to
    /// [`DEFAULT_WAIT_SECS`] and may not exceed [`MAX_WAIT_SECS`]. Answered
    /// with the panel's summary ([`ControlResponse::Panels`]), or an error on
    /// timeout, for an unknown or vanished panel, or for the main panel.
    Wait {
        panel: PanelId,
        timeout_secs: Option<u64>,
    },
    /// Open a [`super::events`] subscription. Answered with
    /// [`ControlResponse::Subscribed`], the snapshot the events that follow
    /// apply to. Not an MCP tool: only the control socket can stream.
    SubscribeEvents,
}

impl ControlRequest {
    /// Decode one call: `method` is an MCP tool name and `args` its
    /// arguments, `null` reading as none. `Err` carries a human-readable
    /// message naming the bad argument.
    pub fn from_call(method: &str, args: &Value) -> Result<Self, String> {
        /// Pull a required panel-id argument and parse it.
        fn panel(args: &Value) -> Result<PanelId, String> {
            let raw = args
                .get("panel")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing string argument `panel`".to_string())?;
            raw.parse::<PanelId>()
                .map_err(|e| format!("invalid panel id `{raw}`: {e}"))
        }

        if !(args.is_null() || args.is_object()) {
            return Err("`params` must be an object".to_string());
        }

        match method {
            "send_keys" => {
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "missing string argument `text`".to_string())?
                    .to_string();
                let enter = args.get("enter").and_then(Value::as_bool).unwrap_or(false);
                Ok(Self::SendKeys {
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
                Ok(Self::SendKey {
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
                        let s = v.as_str().ok_or_else(|| {
                            "`panels` entries must be panel-id strings".to_string()
                        })?;
                        s.parse::<PanelId>()
                            .map_err(|e| format!("invalid panel id `{s}`: {e}"))
                    })
                    .collect::<Result<Vec<PanelId>, String>>()?;
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "missing string argument `text`".to_string())?
                    .to_string();
                let enter = args.get("enter").and_then(Value::as_bool).unwrap_or(false);
                Ok(Self::Broadcast {
                    panels,
                    text,
                    enter,
                })
            }
            "ctrl_c" => Ok(Self::CtrlC {
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
                Ok(Self::ReadPanel {
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
                Ok(Self::SpawnRole {
                    role,
                    worktree,
                    model,
                    agent_cli,
                    prompt,
                })
            }
            "kill_panel" => Ok(Self::KillPanel {
                panel: panel(args)?,
            }),
            "restart_panel" => Ok(Self::RestartPanel {
                panel: panel(args)?,
            }),
            "list_panels" => Ok(Self::ListPanels),
            "queue_tasks" => {
                let tasks = match args.get("tasks") {
                    Some(v) => {
                        let arr = v.as_array().ok_or_else(|| {
                            "`tasks` must be an array of task strings".to_string()
                        })?;
                        arr.iter()
                            .map(|t| {
                                t.as_str()
                                    .map(str::to_string)
                                    .ok_or_else(|| "`tasks` entries must be strings".to_string())
                            })
                            .collect::<Result<Vec<String>, String>>()?
                    }
                    None => Vec::new(),
                };
                let read_mode = match args.get("read_mode").and_then(Value::as_str) {
                    Some(raw) => Some(serde_json::from_value(json!(raw)).map_err(|_| {
                        format!(
                            "invalid read_mode `{raw}` \
                             (expected screen|scrollback|since_last_turn|last_message)"
                        )
                    })?),
                    None => None,
                };
                let fallback_secs = match args.get("fallback_secs") {
                    Some(v) => Some(v.as_u64().ok_or_else(|| {
                        "`fallback_secs` must be a non-negative integer".to_string()
                    })?),
                    None => None,
                };
                let selection_hints = match args.get("selection_hints") {
                    Some(v) => Some(
                        serde_json::from_value::<SelectionPolicy>(v.clone()).map_err(|e| {
                            format!(
                                "`selection_hints` must be an object with optional \
                                     `prefer` / `avoid` string arrays: {e}"
                            )
                        })?,
                    ),
                    None => None,
                };
                Ok(Self::QueueTasks {
                    panel: panel(args)?,
                    tasks,
                    read_mode,
                    fallback_secs,
                    selection_hints,
                })
            }
            "collect_workers" => Ok(Self::CollectWorkers),
            "stop_watching" => Ok(Self::StopWatching {
                panel: panel(args)?,
            }),
            "read_menu" => Ok(Self::ReadMenu {
                panel: panel(args)?,
            }),
            "select_option" => {
                let index = args
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| "missing integer argument `index`".to_string())?
                    as usize;
                Ok(Self::SelectOption {
                    panel: panel(args)?,
                    index,
                })
            }
            "wait" => {
                let timeout_secs = match args.get("timeout_secs") {
                    Some(v) => Some(v.as_u64().ok_or_else(|| {
                        "`timeout_secs` must be a non-negative integer".to_string()
                    })?),
                    None => None,
                };
                Ok(Self::Wait {
                    panel: panel(args)?,
                    timeout_secs,
                })
            }
            "subscribe_events" => Ok(Self::SubscribeEvents),
            other => Err(format!("unknown method `{other}`")),
        }
    }
}

/// [`ControlRequest::Wait`]'s timeout when the caller names none.
pub const DEFAULT_WAIT_SECS: u64 = 600;

/// The longest [`ControlRequest::Wait`] caucus accepts. The MCP clients'
/// per-call timeouts are set above it (`crate::mcp::serve::TOOL_TIMEOUT_SECS`)
/// so the client never abandons a wait caucus is still holding.
pub const MAX_WAIT_SECS: u64 = 3600;

/// The multiplexer's answer to a [`ControlRequest`]. On the wire it travels
/// inside a [`ControlReply`]: a success as `result`, [`Self::Error`] as an
/// `error` with code [`ErrorCode::Failed`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlResponse {
    /// `send_keys` / `ctrl_c` / `kill_panel` succeeded — no payload.
    Ok,
    /// `read_panel` succeeded; `text` is the captured slice.
    Panel { text: String },
    /// `spawn_role` succeeded; `panel` is the new panel id.
    Spawned { panel: PanelId },
    /// `list_panels` succeeded.
    Panels { panels: Vec<PanelSummary> },
    /// `subscribe_events` succeeded: `panels` are the rows as of event `seq`;
    /// the stream continues at `seq + 1`.
    Subscribed { seq: u64, panels: Vec<PanelSummary> },
    /// The tool call failed; `message` is human-readable. Never a `result`:
    /// [`ControlReply::response`] sends it as an error.
    #[serde(skip)]
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

/// One control-socket reply line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlReply {
    /// The call's `id`; `None` when the line carried no string `id`.
    pub id: Option<String>,
    #[serde(flatten)]
    body: ReplyBody,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ReplyBody {
    Result(ControlResponse),
    Error(ReplyError),
}

/// The `error` member of a failed [`ControlReply`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReplyError {
    pub code: ErrorCode,
    pub message: String,
}

/// Why a call failed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The line is not a valid call: not JSON, too long, no string `id`, an
    /// unknown `method`, or bad `params`. Nothing was executed.
    InvalidRequest,
    /// The multiplexer executed the call and it failed.
    Failed,
    /// The multiplexer is shutting down and did not answer.
    Unavailable,
    /// A `subscribe_events` subscriber fell more than
    /// [`super::events::EVENT_RING`] events behind. The stream ends; subscribe
    /// again for a fresh snapshot.
    EventsLost,
}

/// One streamed `subscribe_events` line: `{"id": ..., "event": {"type": ...,
/// "seq": ..., ...}}`, `id` being the subscribing call's.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlEvent {
    pub id: String,
    pub event: PanelEvent,
}

impl ControlReply {
    /// The reply carrying the multiplexer's `response`; a
    /// [`ControlResponse::Error`] becomes a [`ErrorCode::Failed`] error.
    pub fn response(id: Option<String>, response: ControlResponse) -> Self {
        let body = match response {
            ControlResponse::Error { message } => ReplyBody::Error(ReplyError {
                code: ErrorCode::Failed,
                message,
            }),
            response => ReplyBody::Result(response),
        };
        Self { id, body }
    }

    /// An error reply.
    pub fn error(id: Option<String>, code: ErrorCode, message: impl std::fmt::Display) -> Self {
        Self {
            id,
            body: ReplyBody::Error(ReplyError {
                code,
                message: message.to_string(),
            }),
        }
    }

    /// The result, or the error the server reported.
    pub fn into_result(self) -> Result<ControlResponse, ReplyError> {
        match self.body {
            ReplyBody::Result(response) => Ok(response),
            ReplyBody::Error(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_send_keys_request() {
        let id = PanelId::new();
        let req = ControlRequest::from_call(
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
            ControlRequest::from_call("send_key", &json!({"panel": id.to_string(), "key": "esc"}))
                .unwrap();
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
        let err =
            ControlRequest::from_call("send_key", &json!({"panel": id.to_string()})).unwrap_err();
        assert!(err.contains("missing string argument `key`"));
    }

    #[test]
    fn build_broadcast_request_with_enter() {
        let a = PanelId::new();
        let b = PanelId::new();
        let req = ControlRequest::from_call(
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
        let req = ControlRequest::from_call(
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
        let err = ControlRequest::from_call("broadcast", &json!({"text": "hi"})).unwrap_err();
        assert!(err.contains("missing array argument `panels`"));
    }

    #[test]
    fn build_broadcast_requires_text() {
        let a = PanelId::new();
        let err = ControlRequest::from_call("broadcast", &json!({"panels": [a.to_string()]}))
            .unwrap_err();
        assert!(err.contains("missing string argument `text`"));
    }

    #[test]
    fn build_broadcast_rejects_bad_panel_id() {
        let err = ControlRequest::from_call(
            "broadcast",
            &json!({"panels": ["not-a-ulid"], "text": "hi"}),
        )
        .unwrap_err();
        assert!(err.contains("invalid panel id"));
    }

    #[test]
    fn build_read_panel_request() {
        let id = PanelId::new();
        let req = ControlRequest::from_call(
            "read_panel",
            &json!({"panel": id.to_string(), "mode": "since_last_turn"}),
        )
        .unwrap();
        assert_eq!(
            req,
            ControlRequest::ReadPanel {
                panel: id,
                mode: ReadPanelMode::SinceLastTurn,
            }
        );
    }

    #[test]
    fn build_read_panel_turn_mode() {
        let id = PanelId::new();
        let req = ControlRequest::from_call(
            "read_panel",
            &json!({"panel": id.to_string(), "mode": "turn", "turn": 3}),
        )
        .unwrap();
        assert_eq!(
            req,
            ControlRequest::ReadPanel {
                panel: id,
                mode: ReadPanelMode::Turn(3),
            }
        );
    }

    #[test]
    fn build_read_panel_turn_mode_requires_an_index() {
        let id = PanelId::new();
        let err = ControlRequest::from_call(
            "read_panel",
            &json!({"panel": id.to_string(), "mode": "turn"}),
        )
        .unwrap_err();
        assert!(err.contains("requires a non-negative integer argument `turn`"));
    }

    #[test]
    fn build_read_panel_rejects_bad_mode() {
        let id = PanelId::new();
        let err = ControlRequest::from_call(
            "read_panel",
            &json!({"panel": id.to_string(), "mode": "everything"}),
        )
        .unwrap_err();
        assert!(err.contains("invalid mode"));
    }

    #[test]
    fn build_spawn_role_request_with_overrides() {
        let req = ControlRequest::from_call(
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
        let req = ControlRequest::from_call(
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
        let err = ControlRequest::from_call("ctrl_c", &json!({"panel": "not-a-ulid"})).unwrap_err();
        assert!(err.contains("invalid panel id"));
    }

    #[test]
    fn build_restart_panel_request() {
        let id = PanelId::new();
        let req =
            ControlRequest::from_call("restart_panel", &json!({"panel": id.to_string()})).unwrap();
        assert_eq!(req, ControlRequest::RestartPanel { panel: id });
    }

    #[test]
    fn build_subscribe_events_takes_no_args() {
        for params in [Value::Null, json!({})] {
            let req = ControlRequest::from_call("subscribe_events", &params).unwrap();
            assert_eq!(req, ControlRequest::SubscribeEvents);
        }
    }

    #[test]
    fn event_line_wire_shape() {
        let panel = PanelId::new();
        let line = ControlEvent {
            id: "s".into(),
            event: PanelEvent::PanelRemoved {
                seq: 4,
                panel_id: panel,
            },
        };
        assert_eq!(
            serde_json::to_value(&line).unwrap(),
            json!({"id": "s", "event": {"type": "panel_removed", "seq": 4, "panel_id": panel.to_string()}})
        );
        let lost = ControlReply::error(Some("s".into()), ErrorCode::EventsLost, "behind");
        assert_eq!(
            serde_json::to_value(&lost).unwrap(),
            json!({"id": "s", "error": {"code": "events_lost", "message": "behind"}})
        );
    }

    #[test]
    fn build_list_panels_takes_no_args() {
        let req = ControlRequest::from_call("list_panels", &json!({})).unwrap();
        assert_eq!(req, ControlRequest::ListPanels);
    }

    #[test]
    fn build_queue_tasks_request() {
        let a = PanelId::new();
        let req = ControlRequest::from_call(
            "queue_tasks",
            &json!({
                "panel": a.to_string(),
                "tasks": ["CA-SR-2", "CA-SR-3"],
                "read_mode": "since_last_turn",
                "fallback_secs": 90
            }),
        )
        .unwrap();
        assert_eq!(
            req,
            ControlRequest::QueueTasks {
                panel: a,
                tasks: vec!["CA-SR-2".to_string(), "CA-SR-3".to_string()],
                read_mode: Some(ReadPanelMode::SinceLastTurn),
                fallback_secs: Some(90),
                selection_hints: None,
            }
        );
    }

    #[test]
    fn build_queue_tasks_parses_selection_hints() {
        let a = PanelId::new();
        let req = ControlRequest::from_call(
            "queue_tasks",
            &json!({
                "panel": a.to_string(),
                "selection_hints": {
                    "prefer": ["structural", "at source"],
                    "avoid": ["broad refactor"]
                }
            }),
        )
        .unwrap();
        assert_eq!(
            req,
            ControlRequest::QueueTasks {
                panel: a,
                tasks: vec![],
                read_mode: None,
                fallback_secs: None,
                selection_hints: Some(SelectionPolicy {
                    prefer: vec!["structural".to_string(), "at source".to_string()],
                    avoid: vec!["broad refactor".to_string()],
                }),
            }
        );
    }

    #[test]
    fn build_queue_tasks_rejects_bad_selection_hints() {
        let a = PanelId::new();
        // `prefer` must be an array of strings, not a bare string.
        let err = ControlRequest::from_call(
            "queue_tasks",
            &json!({"panel": a.to_string(), "selection_hints": {"prefer": "structural"}}),
        )
        .unwrap_err();
        assert!(err.contains("selection_hints"), "got: {err}");
    }

    /// `tasks` and every option default when omitted: a bare panel id is a valid
    /// call (it just re-arms/keeps the watch with caucus's defaults).
    #[test]
    fn build_queue_tasks_defaults_optional_fields() {
        let a = PanelId::new();
        let req =
            ControlRequest::from_call("queue_tasks", &json!({"panel": a.to_string()})).unwrap();
        assert_eq!(
            req,
            ControlRequest::QueueTasks {
                panel: a,
                tasks: vec![],
                read_mode: None,
                fallback_secs: None,
                selection_hints: None,
            }
        );
    }

    #[test]
    fn build_queue_tasks_requires_a_panel() {
        let err = ControlRequest::from_call("queue_tasks", &json!({})).unwrap_err();
        assert!(
            err.contains("missing string argument `panel`"),
            "got: {err}"
        );
    }

    #[test]
    fn build_queue_tasks_rejects_non_string_tasks() {
        let a = PanelId::new();
        let err = ControlRequest::from_call(
            "queue_tasks",
            &json!({"panel": a.to_string(), "tasks": "not-an-array"}),
        )
        .unwrap_err();
        assert!(
            err.contains("`tasks` must be an array of task strings"),
            "got: {err}"
        );
        let err = ControlRequest::from_call(
            "queue_tasks",
            &json!({"panel": a.to_string(), "tasks": [1, 2]}),
        )
        .unwrap_err();
        assert!(
            err.contains("`tasks` entries must be strings"),
            "got: {err}"
        );
    }

    #[test]
    fn build_collect_workers_takes_no_args() {
        let req = ControlRequest::from_call("collect_workers", &json!({})).unwrap();
        assert_eq!(req, ControlRequest::CollectWorkers);
    }

    #[test]
    fn build_stop_watching_request() {
        let a = PanelId::new();
        let req =
            ControlRequest::from_call("stop_watching", &json!({"panel": a.to_string()})).unwrap();
        assert_eq!(req, ControlRequest::StopWatching { panel: a });
    }

    #[test]
    fn build_stop_watching_rejects_bad_panel_id() {
        let err = ControlRequest::from_call("stop_watching", &json!({"panel": "not-a-ulid"}))
            .unwrap_err();
        assert!(err.contains("invalid panel id"), "got: {err}");
    }

    #[test]
    fn build_wait_request() {
        let a = PanelId::new();
        let req = ControlRequest::from_call("wait", &json!({"panel": a.to_string()})).unwrap();
        assert_eq!(
            req,
            ControlRequest::Wait {
                panel: a,
                timeout_secs: None,
            }
        );
        let req =
            ControlRequest::from_call("wait", &json!({"panel": a.to_string(), "timeout_secs": 45}))
                .unwrap();
        assert_eq!(
            req,
            ControlRequest::Wait {
                panel: a,
                timeout_secs: Some(45),
            }
        );
        let err =
            ControlRequest::from_call("wait", &json!({"panel": a.to_string(), "timeout_secs": -1}))
                .unwrap_err();
        assert!(err.contains("timeout_secs"), "got: {err}");
    }

    #[test]
    fn build_read_menu_request() {
        let a = PanelId::new();
        let req = ControlRequest::from_call("read_menu", &json!({"panel": a.to_string()})).unwrap();
        assert_eq!(req, ControlRequest::ReadMenu { panel: a });
    }

    #[test]
    fn build_select_option_request() {
        let a = PanelId::new();
        let req = ControlRequest::from_call(
            "select_option",
            &json!({"panel": a.to_string(), "index": 2}),
        )
        .unwrap();
        assert_eq!(req, ControlRequest::SelectOption { panel: a, index: 2 });
    }

    #[test]
    fn build_select_option_requires_index() {
        let a = PanelId::new();
        let err = ControlRequest::from_call("select_option", &json!({"panel": a.to_string()}))
            .unwrap_err();
        assert!(err.contains("missing integer argument `index`"));
    }

    #[test]
    fn build_spawn_role_defaults_are_optional() {
        let req = ControlRequest::from_call("spawn_role", &json!({"role": "backend"})).unwrap();
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
    fn from_call_reads_null_params_as_no_arguments() {
        assert_eq!(
            ControlRequest::from_call("list_panels", &Value::Null).unwrap(),
            ControlRequest::ListPanels
        );
        let err = ControlRequest::from_call("ctrl_c", &Value::Null).unwrap_err();
        assert!(
            err.contains("missing string argument `panel`"),
            "got: {err}"
        );
    }

    #[test]
    fn from_call_rejects_non_object_params() {
        let err = ControlRequest::from_call("list_panels", &json!(["x"])).unwrap_err();
        assert!(err.contains("`params` must be an object"), "got: {err}");
    }

    #[test]
    fn from_call_rejects_an_unknown_method() {
        let err = ControlRequest::from_call("drop_tables", &json!({})).unwrap_err();
        assert!(err.contains("unknown method `drop_tables`"), "got: {err}");
    }

    #[test]
    fn call_params_default_to_null() {
        let call: ControlCall =
            serde_json::from_str(r#"{"id":"1","method":"list_panels"}"#).unwrap();
        assert_eq!(call.params, Value::Null);
        assert!(serde_json::from_str::<ControlCall>(r#"{"method":"list_panels"}"#).is_err());
    }

    #[test]
    fn reply_wire_shape() {
        let ok = ControlReply::response(Some("7".into()), ControlResponse::Ok);
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            json!({"id": "7", "result": {"type": "ok"}})
        );
        let failed = ControlReply::response(Some("7".into()), ControlResponse::error("boom"));
        assert_eq!(
            serde_json::to_value(&failed).unwrap(),
            json!({"id": "7", "error": {"code": "failed", "message": "boom"}})
        );
        let invalid = ControlReply::error(None, ErrorCode::InvalidRequest, "not json");
        assert_eq!(
            serde_json::to_value(&invalid).unwrap(),
            json!({"id": null, "error": {"code": "invalid_request", "message": "not json"}})
        );
    }

    #[test]
    fn reply_round_trips_into_its_result() {
        for response in [
            ControlResponse::Ok,
            ControlResponse::Panel {
                text: "hello".into(),
            },
            ControlResponse::Spawned {
                panel: PanelId::new(),
            },
            ControlResponse::Panels { panels: vec![] },
            ControlResponse::Subscribed {
                seq: 3,
                panels: vec![crate::mcp::events::test_row(PanelId::new(), "idle")],
            },
        ] {
            let line =
                serde_json::to_string(&ControlReply::response(Some("1".into()), response.clone()))
                    .unwrap();
            let back: ControlReply = serde_json::from_str(&line).unwrap();
            assert_eq!(back.into_result(), Ok(response));
        }
        let line = serde_json::to_string(&ControlReply::response(
            None,
            ControlResponse::error("no such panel"),
        ))
        .unwrap();
        let back: ControlReply = serde_json::from_str(&line).unwrap();
        assert_eq!(
            back.into_result(),
            Err(ReplyError {
                code: ErrorCode::Failed,
                message: "no such panel".into(),
            })
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
}
