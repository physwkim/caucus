//! caucus MCP control plane — the interface caucus exposes to the main worker
//! (`docs/design.md` §0 #4, §9).
//!
//! The main worker (a Claude Code agent in one panel) drives every sub-agent
//! panel through fifteen MCP tools: `send_keys`, `send_key`, `broadcast`,
//! `ctrl_c`, `read_panel`, `spawn_role`, `kill_panel`, `restart_panel`,
//! `list_panels`, `queue_tasks`, `collect_workers`, `stop_watching`,
//! `read_menu`, `select_option`, `wait`.
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
//! / `tools/call`, fourteen tools — so [`jsonrpc`] implements exactly that, with
//! a pure dispatch core. See that module's header for the rationale.

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
    /// A specific past turn by its absolute 0-based index (`since_last_turn`
    /// only ever returns the most recent). Readable while the turn is still in
    /// the in-memory ring; an older turn that has spilled to disk is reported
    /// as such (the disk log concatenates turns without a per-turn boundary).
    Turn(usize),
}

/// One panel's status row, returned by `list_panels`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PanelSummary {
    pub panel_id: PanelId,
    pub role: String,
    /// Derived state as its canonical `snake_case` name (`working` / `idle` /
    /// `awaiting_selection` / `blocked_*` / `exited`), from
    /// [`crate::agent::derive_state::DerivedState::as_str`].
    pub state: String,
    pub agent_cli: AgentCli,
    /// Filesystem path of the panel's dedicated git worktree, if it was
    /// spawned with one (`spawn_role(worktree=true)`); `None` for a panel that
    /// shares the main repo checkout. This is the directory the sub-agent's
    /// commits land in.
    pub worktree_path: Option<String>,
    /// Git branch the panel's worktree checked out, if any — the branch name
    /// is generated internally at spawn, so this is the only way the main
    /// worker learns it to merge/diff the sub-agent's work. `None` without a
    /// worktree.
    pub branch: Option<String>,
    /// Model override the panel runs under (`spawn_role(model=...)`), if one
    /// was set; `None` when the panel uses the backend's default model.
    pub model: Option<String>,
    /// Path of the agent's conversation transcript (JSONL), once a turn
    /// signal has carried one (`docs/design.md` §7.4). The main worker reads
    /// the file directly for the full conversation — richer than any screen
    /// capture. `None` before the first turn signal and for backends whose
    /// payloads carry no such path (codex).
    pub transcript_path: Option<String>,
    /// The panel's most recent mid-turn note (`caucus signal note`), rendered
    /// `[kind] body` — how a still-`working` panel reports progress, names an
    /// artifact, or asks a question without ending its turn. `None` until the
    /// panel posts one; the full history is on the manifest's lane events.
    pub last_note: Option<String>,
    /// The most recent desktop-notification text the panel's process emitted
    /// in-band (OSC 9 / 99 / 777, `docs/design.md` §7.7) — an attention
    /// signal from tools with no hook channel. `None` until one is seen; the
    /// full history is on the manifest's lane events.
    pub last_notification: Option<String>,
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
    /// Type keys into a panel's PTY (the live task-delivery mechanism, `docs/design.md`
    /// §4). When `enter` is set, a trailing newline is appended.
    fn send_keys(&mut self, panel: PanelId, text: &str, enter: bool) -> Result<(), McpError>;

    /// Send a single raw key to a panel's PTY, named as
    /// [`crate::input::parse_key_name`] parses it (`esc`, `up`, `ctrl-c`,
    /// `f5`, …). The escape hatch for keys `send_keys` text cannot express; no
    /// turn/`Working` bookkeeping is done.
    fn send_key(&mut self, panel: PanelId, key: &str) -> Result<(), McpError>;

    /// Send `Ctrl-C` (interrupt) to a panel's PTY.
    fn ctrl_c(&mut self, panel: PanelId) -> Result<(), McpError>;

    /// Read a panel's captured output in the requested `mode` (`docs/design.md`
    /// §8.5).
    fn read_panel(&self, panel: PanelId, mode: ReadPanelMode) -> Result<String, McpError>;

    /// Spawn a new panel for `role`. `role` is a free-form label: a known
    /// preset reuses that preset's tool allowlist and permission mode, any
    /// other name is an ad-hoc role on the generic `worker` defaults.
    /// `worktree` requests an execute-phase worktree; `model`/`agent_cli` are
    /// main worker overrides; `prompt`, when set, is the role's system prompt
    /// (replacing the preset's template) (`docs/design.md` §5, §6).
    fn spawn_role(
        &mut self,
        role: &str,
        worktree: bool,
        model: Option<&str>,
        agent_cli: Option<AgentCli>,
        prompt: Option<&str>,
    ) -> Result<PanelId, McpError>;

    /// Kill a panel; its worktree (if any) is enqueued for cleanup.
    fn kill_panel(&mut self, panel: PanelId) -> Result<(), McpError>;

    /// Restart a sub-agent panel in place: tear it down and spawn a fresh agent
    /// that resumes the same conversation in the same worktree, under the same
    /// role / model / backend. Returns the NEW panel id. The main worker panel
    /// cannot be restarted.
    fn restart_panel(&mut self, panel: PanelId) -> Result<PanelId, McpError>;

    /// List every live panel with its derived state.
    fn list_panels(&mut self) -> Vec<PanelSummary>;

    /// Read the interactive selection menu shown in a panel, if any — the
    /// question, its numbered options, and the highlighted one as readable
    /// text. Empty when no menu is on screen (`docs/design.md` §8.3).
    fn read_menu(&mut self, panel: PanelId) -> Result<String, McpError>;

    /// Answer a panel's selection menu by picking option `index` (the displayed
    /// 1-based number): caucus navigates the chooser to that option and presses
    /// Enter (`docs/design.md` §8.3).
    fn select_option(&mut self, panel: PanelId, index: usize) -> Result<(), McpError>;
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
            name: "send_key",
            description: "Send ONE raw key to a panel — the escape hatch for keys \
                          send_keys text cannot express: 'esc' (dismiss a prompt), \
                          arrows ('up'/'down'/'left'/'right'), 'tab', 'enter', \
                          control chords ('ctrl-c', 'ctrl-d'), 'alt-*', and \
                          function keys ('f1'..'f12'). Names are case-insensitive \
                          with '-' or '+' joining modifiers (ctrl / alt / shift), \
                          e.g. 'ctrl-shift-left'. Unlike send_keys this does no \
                          turn bookkeeping — use send_keys(enter=true) to deliver \
                          a prompt as a turn.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "panel": panel_prop(),
                    "key": {
                        "type": "string",
                        "description": "Key name, e.g. 'esc', 'up', 'ctrl-c', \
                                        'alt-enter', 'f5'."
                    }
                },
                "required": ["panel", "key"]
            }),
        },
        ToolDef {
            name: "broadcast",
            description: "Send the same text to several panels at once — a \
                          fan-out. Equivalent to one send_keys per panel. \
                          caucus watches every panel it prompts, so just end \
                          your turn afterwards: it delivers each worker's \
                          result to you as that worker finishes, one at a \
                          time — you do not wait for the slowest.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "panels": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Panel ids (ULIDs) to send the text to."
                    },
                    "text": { "type": "string", "description": "Text to type into every panel." },
                    "enter": {
                        "type": "boolean",
                        "description": "Append a newline (submit the line) in every panel.",
                        "default": false
                    }
                },
                "required": ["panels", "text"]
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
                          message from its turn signal), 'turn' (a specific past \
                          turn by its 0-based index in the `turn` arg — \
                          since_last_turn only ever returns the latest; an old \
                          turn that has scrolled out of memory reports as such).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "panel": panel_prop(),
                    "mode": {
                        "type": "string",
                        "enum": [
                            "screen", "scrollback", "since_last_turn",
                            "last_message", "turn"
                        ],
                        "description": "Which output slice to return."
                    },
                    "turn": {
                        "type": "integer",
                        "description": "Absolute 0-based turn index — required \
                                        when mode is 'turn', ignored otherwise."
                    }
                },
                "required": ["panel", "mode"]
            }),
        },
        ToolDef {
            name: "spawn_role",
            description: "Spawn a new sub-agent panel. You are NOT limited to a \
                          fixed roster: 'role' is a free-form label. A known \
                          preset (worker, architect, backend, reviewer, qa, \
                          scribe, serious-reviewer) reuses that preset's tool \
                          allowlist + permission mode; any other label is an \
                          ad-hoc role built on the generic worker defaults. To \
                          invent a role, pass any label plus 'prompt' — that text \
                          becomes the agent's system prompt (the role's \
                          instructions), so you define the sub-agent's job, model, \
                          and backend yourself. worktree=true gives a dedicated \
                          git worktree; model and agent_cli override the defaults \
                          (the 'prompt' reaches claude via --append-system-prompt \
                          and codex via -c instructions=...). caucus itself \
                          appends a question contract to every sub-agent prompt \
                          (ask in plain text and end the turn — AskUserQuestion \
                          is disabled on claude sub-agents), so your 'prompt' \
                          never needs to restate that.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "role": {
                        "type": "string",
                        "description": "Free-form role label. A preset name \
                                        (worker / architect / backend / reviewer / \
                                        qa / scribe / serious-reviewer) reuses its \
                                        tools + permission mode; any other name is \
                                        a custom role on the worker defaults."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Inline system prompt — the role's \
                                        instructions. When set it replaces the \
                                        preset's prompt template, letting you \
                                        define an ad-hoc role on the fly. Omit to \
                                        use a preset's built-in prompt."
                    },
                    "worktree": {
                        "type": "boolean",
                        "description": "Create a dedicated git worktree for the panel.",
                        "default": false
                    },
                    "model": { "type": "string", "description": "Model override." },
                    "agent_cli": {
                        "type": "string",
                        "enum": ["claude", "codex"],
                        "description": "Backend CLI override."
                    }
                },
                "required": ["role"]
            }),
        },
        ToolDef {
            name: "kill_panel",
            description: "Kill a panel: terminate its agent process and enqueue \
                          any worktree for cleanup. Uncommitted work in that \
                          worktree is committed onto its branch first, so it \
                          survives the removal — recover it with \
                          `git show <branch>`, revert it with `git reset HEAD^`. \
                          An idle panel is reusable, not a leak: hand it the \
                          next sub-task with send_keys rather than killing it to \
                          tidy the roster. Kill when a worktree panel's next task \
                          belongs on a different branch, when the roster exceeds \
                          the work in flight, or when a panel is wedged (prefer \
                          restart_panel there, which keeps the worktree).",
            input_schema: json!({
                "type": "object",
                "properties": { "panel": panel_prop() },
                "required": ["panel"]
            }),
        },
        ToolDef {
            name: "restart_panel",
            description: "Restart a wedged sub-agent panel in place: terminate \
                          its current agent process and spawn a fresh one that \
                          RESUMES the same conversation in the same worktree \
                          (branch, commits, and uncommitted changes preserved), \
                          under the same role / model / backend. Returns the NEW \
                          panel id — re-target later calls at it. Use this when a \
                          panel hangs, OOMs, or its CLI crashes and you want its \
                          context back, instead of kill_panel + spawn_role (which \
                          loses the worktree and the session). The main worker \
                          panel cannot be restarted.",
            input_schema: json!({
                "type": "object",
                "properties": { "panel": panel_prop() },
                "required": ["panel"]
            }),
        },
        ToolDef {
            name: "list_panels",
            description: "List every live panel with its role and derived state \
                          (working / idle / blocked_* / exited). Each row also \
                          carries the panel's worktree_path and branch (where a \
                          worktree sub-agent's commits land — the branch name is \
                          generated internally, so this is how you find it to \
                          merge/diff) and its model override, all null for a \
                          panel without them.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "queue_tasks",
            description: "Queue follow-up tasks on ONE worker, and tune how \
                          caucus watches it. You do NOT need this to hear back \
                          from a worker: caucus watches every panel it hands a \
                          prompt to, and re-prompts you with that worker's \
                          result the moment it finishes — send_keys is enough. \
                          Use this when you already know the worker's next few \
                          tasks and do not want it idling between them: caucus \
                          feeds it the next queued task each time it goes idle, \
                          and delivers its result to you only once the queue \
                          drains. Pass no `tasks` to set the options alone. \
                          Each delivery names the panel it came from; workers \
                          reach you one at a time, as each finishes — never \
                          batched, never in lockstep, so act on each one as it \
                          lands instead of waiting for the others. That push can \
                          only land while you are idle, so it arrives only AFTER \
                          your turn ends: do NOT sleep-poll list_panels waiting \
                          for it inside this turn — it will never come and you \
                          will wait forever. If you must stay in your turn, call \
                          collect_workers instead.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "panel": panel_prop(),
                    "tasks": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Ordered follow-up task prompts, appended \
                                        to this worker's queue. caucus feeds the \
                                        next one whenever the panel goes idle; \
                                        the worker's result reaches you only once \
                                        the queue is empty. Omit to set the \
                                        options below without queueing work."
                    },
                    "read_mode": {
                        "type": "string",
                        "enum": ["last_message", "since_last_turn"],
                        "description": "What to read from this worker for the \
                                        delivered report (default last_message)."
                    },
                    "fallback_secs": {
                        "type": "integer",
                        "description": "Safety-net seconds: a worker still \
                                        mid-turn after this is reported to you \
                                        anyway as an interim notice (no result \
                                        yet, its last note, tasks still queued); \
                                        it stays watched with its queue intact \
                                        and the window restarts (default 600, \
                                        max 3600). Set from now."
                    },
                    "selection_hints": {
                        "type": "object",
                        "properties": {
                            "prefer": {
                                "type": "array",
                                "items": { "type": "string" }
                            },
                            "avoid": {
                                "type": "array",
                                "items": { "type": "string" }
                            }
                        },
                        "description": "Optional keyword hints so caucus answers \
                                        this worker's selection menus for you \
                                        instead of interrupting your turn. An \
                                        option qualifies when its label contains \
                                        (case-insensitively) a `prefer` keyword \
                                        (empty `prefer` = any option) and no \
                                        `avoid` keyword. caucus auto-selects ONLY \
                                        when exactly one option qualifies; zero or \
                                        several matches are escalated to you as a \
                                        normal blocked-panel notice. Each \
                                        auto-answer is noted in that worker's \
                                        delivered report. Omit to escalate every menu."
                    }
                },
                "required": ["panel"]
            }),
        },
        ToolDef {
            name: "collect_workers",
            description: "Collect every worker that is ready RIGHT NOW. Takes no \
                          arguments — caucus watches every panel you have handed \
                          work to, so this covers all of them. Returns the report \
                          of each worker that has finished (or passed its \
                          fallback deadline) and has not reached you yet; the \
                          ones still running are reported as a status line \
                          instead (working / draining backlog / settled, backlog \
                          remaining, seconds left on the fallback deadline) and \
                          stay watched, so call again later to collect them. \
                          There is no barrier — this hands you the early \
                          finishers while the slow workers are still going. This \
                          is the in-turn path: a main worker that must stay \
                          inside its turn can still collect results, instead of \
                          waiting on a caucus push that can only land after the \
                          turn ends. Ending your turn and letting caucus \
                          re-prompt you per worker is still the cheaper path. \
                          Each worker reaches you exactly once across both paths.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "stop_watching",
            description: "Stop watching one worker: caucus drops any queued \
                          backlog for it and will not deliver its result to you. \
                          The panel is left exactly as it is — work already \
                          running keeps running; this does not kill or interrupt \
                          it. Not permanent: the next prompt you send that panel \
                          starts watching it again. A panel caucus is not \
                          watching is an error.",
            input_schema: json!({
                "type": "object",
                "properties": { "panel": panel_prop() },
                "required": ["panel"]
            }),
        },
        ToolDef {
            name: "read_menu",
            description: "Read the interactive selection menu currently shown in \
                          a panel — an AskUserQuestion-style chooser. Returns the \
                          question, the numbered options, and which is \
                          highlighted. Empty if no menu is on screen. Use this \
                          when a panel reads 'awaiting_selection' (or caucus tells \
                          you it is waiting) to see the choices before answering \
                          with select_option.",
            input_schema: json!({
                "type": "object",
                "properties": { "panel": panel_prop() },
                "required": ["panel"]
            }),
        },
        ToolDef {
            name: "select_option",
            description: "Answer a panel's selection menu: pick option number \
                          'index' (the 1-based number shown by read_menu) and \
                          caucus navigates the chooser there and presses Enter. \
                          To answer in free text instead, select the menu's \
                          'type something' option this way, then send_keys your \
                          text.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "panel": panel_prop(),
                    "index": {
                        "type": "integer",
                        "description": "Option number to pick (1-based, as shown)."
                    }
                },
                "required": ["panel", "index"]
            }),
        },
        ToolDef {
            name: "wait",
            description: "Block until one panel stops working — its list_panels \
                          state leaves 'working' / 'spawning' (idle, exited, or \
                          stopped on a prompt) — and return its list_panels \
                          entry. Use it only when your next step inside this turn \
                          needs that panel's result; otherwise end your turn and \
                          let caucus push the result to you. A watched worker \
                          is still delivered afterwards: call collect_workers to \
                          take its report now. Errors on timeout, if the panel \
                          is killed or restarted while you wait, and for your own \
                          panel.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "panel": panel_prop(),
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": protocol::MAX_WAIT_SECS,
                        "default": protocol::DEFAULT_WAIT_SECS,
                        "description": "Give up after this many seconds."
                    }
                },
                "required": ["panel"]
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
                "send_key",
                "broadcast",
                "ctrl_c",
                "read_panel",
                "spawn_role",
                "kill_panel",
                "restart_panel",
                "list_panels",
                "queue_tasks",
                "collect_workers",
                "stop_watching",
                "read_menu",
                "select_option",
                "wait",
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
