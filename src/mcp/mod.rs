//! caucus MCP control plane — the interface caucus exposes to the main worker
//! (`docs/design.md` §0 #4, §9).
//!
//! The main worker (a Claude Code agent in one panel) drives every sub-agent
//! panel through fourteen MCP tools: `send_keys`, `send_key`, `broadcast`,
//! `ctrl_c`, `read_panel`, `spawn_role`, `kill_panel`, `restart_panel`,
//! `list_panels`, `register_round`, `round_status`, `cancel_round`,
//! `read_menu`, `select_option`.
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
    fn list_panels(&self) -> Vec<PanelSummary>;

    /// Read the interactive selection menu shown in a panel, if any — the
    /// question, its numbered options, and the highlighted one as readable
    /// text. Empty when no menu is on screen (`docs/design.md` §8.3).
    fn read_menu(&self, panel: PanelId) -> Result<String, McpError>;

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
                          round's fan-out. Equivalent to one send_keys per \
                          panel. Follow with register_round on the same \
                          panels, then end your turn; caucus delivers their \
                          assembled results when the round settles.",
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
            name: "register_round",
            description: "Register a round: caucus watches the named panels and, \
                          when they ALL settle (finish their turn — leave the \
                          'working' state) or fallback_secs elapses, delivers \
                          their assembled results to you as a new message. \
                          Returns immediately — after calling this, end your \
                          turn; caucus re-prompts you when the round completes. \
                          That push can only land while you are idle, so it \
                          arrives only AFTER your turn ends: do NOT sleep-poll \
                          list_panels waiting for it inside this turn — it will \
                          never come and you will wait forever. If you must stay \
                          in your turn, poll round_status instead: it hands you \
                          the assembled report itself once the round completes. \
                          Use `backlog` to keep \
                          a panel busy across several tasks: caucus feeds it the \
                          next queued task each time it goes idle, so an early \
                          finisher never sits idle, and the panel settles only \
                          once its queue drains. Use `selection_hints` to let \
                          caucus answer a panel's recurring direction/approach \
                          menus for you (by option-label keywords) without \
                          interrupting your turn — it only auto-answers when the \
                          keywords single out exactly one option, and escalates \
                          anything ambiguous to you as usual.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "panels": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Panel ids (ULIDs) in the round."
                    },
                    "read_mode": {
                        "type": "string",
                        "enum": ["last_message", "since_last_turn"],
                        "description": "What to read from each panel for the \
                                        delivered report (default last_message)."
                    },
                    "fallback_secs": {
                        "type": "integer",
                        "description": "Safety-net seconds: if the panels never \
                                        all settle, caucus delivers a partial \
                                        report after this (default 600, max 3600)."
                    },
                    "backlog": {
                        "type": "object",
                        "additionalProperties": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "description": "Optional per-panel follow-up queue, keyed \
                                        by panel id (ULID) → ordered list of task \
                                        prompts. caucus feeds a panel its next \
                                        task whenever it goes idle; the panel \
                                        settles only once its queue is empty. A \
                                        panel omitted here settles on its first \
                                        idle (one task)."
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
                                        this round's selection menus for you \
                                        instead of interrupting your turn. An \
                                        option qualifies when its label contains \
                                        (case-insensitively) a `prefer` keyword \
                                        (empty `prefer` = any option) and no \
                                        `avoid` keyword. caucus auto-selects ONLY \
                                        when exactly one option qualifies; zero or \
                                        several matches are escalated to you as a \
                                        normal blocked-panel notice. Each \
                                        auto-answer is noted in the delivered \
                                        round report. Omit to escalate every menu."
                    }
                },
                "required": ["panels"]
            }),
        },
        ToolDef {
            name: "round_status",
            description: "Check on a round you registered, by the round id \
                          register_round returned — and collect it once it is \
                          done. While the round is still running this reports \
                          each panel's state (working / draining backlog / \
                          settled / gone), its remaining backlog count, and the \
                          seconds left on the fallback deadline. Once the round \
                          COMPLETES (every panel settled, or the fallback \
                          deadline passed) this returns the assembled round \
                          report itself and completes the round — so a main \
                          worker that must stay inside its turn can still collect \
                          its results, instead of waiting on a caucus push that \
                          can only land after the turn ends. Ending your turn and \
                          letting caucus re-prompt you is still the cheaper path. \
                          A round is delivered exactly once: an id already \
                          collected (by either path), cancelled, or never \
                          registered is an error.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "round": {
                        "type": "string",
                        "description": "Round id (a ULID) from register_round."
                    }
                },
                "required": ["round"]
            }),
        },
        ToolDef {
            name: "cancel_round",
            description: "Cancel a round you registered, by the round id \
                          register_round returned, so caucus stops watching it \
                          and never delivers its report. The panels are left \
                          exactly as they are — work already running keeps \
                          running and any backlog simply stops being fed; this \
                          does not kill or interrupt them. A round id caucus is \
                          no longer watching is an error.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "round": {
                        "type": "string",
                        "description": "Round id (a ULID) from register_round."
                    }
                },
                "required": ["round"]
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
                "register_round",
                "round_status",
                "cancel_round",
                "read_menu",
                "select_option",
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
