//! Clap derive-API definitions for every subcommand. The dispatch table
//! lives in `super::mod::run`; this module is the typed source of truth for
//! what `caucus --help` documents.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use super::output::OutputFormat;

/// Top-level CLI.
#[derive(Debug, Parser)]
#[command(
    name = "caucus",
    version,
    about = "Collaboration swarm orchestrator over tmux + git worktree"
)]
pub struct Cli {
    /// Output format (default: text).
    #[arg(long, value_enum, default_value_t = OutputFormat::Text, global = true)]
    pub format: OutputFormat,

    /// Override the repo root caucus operates against. Defaults to CWD.
    #[arg(long, global = true)]
    pub repo: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create `.caucus/`, write `bin/sentinel-stop`, and print the next
    /// step the user needs to take (hook installation).
    Init(InitArgs),

    /// Run environment health checks (tmux/git/claude/hook + role config).
    Doctor,

    /// Inspect or work with sessions.
    Session(SessionArgs),

    /// Start / advance / inspect a round.
    Round(RoundArgs),

    /// Drive the execute phase.
    Execute(ExecuteArgs),

    /// Inspect or signal individual agents.
    Agent(AgentArgs),

    /// List or inspect available roles.
    Role(RoleArgs),

    /// Sentinel writer — invoked by the Claude Stop hook script.
    Sentinel(SentinelArgs),

    /// Foreground watcher: stream events for one session to stdout.
    Watch(WatchArgs),

    /// Toggle the CAUCUS CEO orientation block in `<repo>/CLAUDE.md`.
    Ceo(CeoArgs),

    /// Fully autonomous run: session new → round → converge → pipeline,
    /// no human in the loop. v1 hard-codes roles, agenda, decision, and
    /// retry policy; later versions will synthesise each via `claude --print`.
    Auto(AutoArgs),
}

#[derive(Debug, Args)]
pub struct AutoArgs {
    /// Free-form task description. v1 copies this text verbatim into both
    /// the agenda (meeting input) and the decision (pipeline input). Later
    /// versions will let an LLM rewrite each.
    pub task: String,
    /// Comma-separated role names. When omitted, caucus shells out to
    /// `claude --print` with the task text and the registry's role list,
    /// then parses the response into a role roster. Pass this flag
    /// explicitly to skip synthesis and pin the roster (e.g.
    /// `--roles architect,backend,reviewer`).
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    pub roles: Option<Vec<String>>,
    /// Meeting agenda timeout (seconds). caucus waits up to this long for
    /// every meeting agent's first sentinel before giving up. Default 1800.
    #[arg(long, default_value_t = 1800)]
    pub round_timeout_secs: u64,
    /// Per-pipeline-step sentinel timeout (seconds). Default 1800.
    #[arg(long, default_value_t = 1800)]
    pub step_timeout_secs: u64,
    /// Retry budget for the reviewer's BLOCK verdict. Default 1.
    #[arg(long, default_value_t = 1)]
    pub retry_on_block: u32,
    /// Pane placement. Default `window` — auto runs are unattended, so
    /// per-role tabs read more cleanly than split panes when you come
    /// back to inspect.
    #[arg(long, value_enum, default_value_t = PlacementMode::Window)]
    pub placement: PlacementMode,
    /// Model override forwarded to every spawned agent. Per-role defaults
    /// still apply when this is unset.
    #[arg(long)]
    pub model: Option<String>,
    /// Optional base ref for the pipeline worktree.
    #[arg(long)]
    pub base_ref: Option<String>,
}

#[derive(Debug, Args)]
pub struct CeoArgs {
    #[command(subcommand)]
    pub action: CeoAction,
}

#[derive(Debug, Subcommand)]
pub enum CeoAction {
    /// Add (or refresh) the CEO block in this repo's CLAUDE.md.
    Enable,
    /// Remove the CEO block from CLAUDE.md (preserves the rest of the file).
    Disable,
    /// Report whether the CEO block is currently installed.
    Status,
    /// Print the CEO orientation prompt to stdout. No side effects.
    Show,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Overwrite existing `.caucus/bin/sentinel-stop` if present.
    #[arg(long)]
    pub force: bool,
    /// Also merge the Stop hook into `~/.claude/settings.json` (with a
    /// timestamped `.bak` backup if the file already exists).
    #[arg(long)]
    pub install_hook: bool,
}

#[derive(Debug, Args)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub action: SessionAction,
}

#[derive(Debug, Subcommand)]
pub enum SessionAction {
    /// Create a new session and spawn one meeting pane per role.
    New(SessionNewArgs),
    /// List all sessions under the repo.
    List,
    /// Show one session's full state (state machine + roster + last events).
    Show(SessionShowArgs),
    /// Lock in a decision and transition Meeting* → MeetingConverged.
    Converge(SessionConvergeArgs),
    /// Transition Meeting* → MeetingDeadlocked.
    Deadlock(SessionDeadlockArgs),
    /// Kill every pane, enqueue worktree cleanup, mark Abandoned.
    Kill(SessionKillArgs),
    /// Render `<session>/transcript.md` from all rounds.
    Transcript(SessionTranscriptArgs),
    /// Exit 0 if the session is in a terminal state (Merged | Abandoned),
    /// 1 if still active. Cheap polling-gate for CEO wakeup loops.
    IsTerminal(SessionIsTerminalArgs),
    /// Re-balance pane layout for an existing session (no spawn). Useful
    /// after a terminal resize or after the operator manually rearranges.
    Relayout(SessionRelayoutArgs),
}

#[derive(Debug, Args)]
pub struct SessionRelayoutArgs {
    /// Session id — only needed for log/metadata, not for the tmux call
    /// (the layout applies to the current window).
    pub session_id: String,
    /// Layout preset. Default `auto` picks even-horizontal for 2 panes,
    /// tiled for 3+.
    #[arg(long, value_enum, default_value_t = LayoutPreset::Auto)]
    pub layout: LayoutPreset,
}

#[derive(Debug, Args)]
pub struct SessionTranscriptArgs {
    pub session_id: String,
}

#[derive(Debug, Args)]
pub struct SessionIsTerminalArgs {
    pub session_id: String,
}

#[derive(Debug, Args)]
pub struct SessionNewArgs {
    /// Topic title (e.g. "write_loop refactor").
    #[arg(long)]
    pub topic: String,
    /// Comma-separated role names from your roles registry.
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    pub roles: Vec<String>,
    /// Maximum number of meeting rounds (default: 5).
    #[arg(long, default_value_t = 5)]
    pub max_rounds: u32,
    /// Optional model id (forwarded to each spawned `claude`).
    #[arg(long)]
    pub model: Option<String>,
    /// Opt back into the agent CLI's permission prompts. Default behaviour
    /// is to pass `--dangerously-skip-permissions` to every sub-agent on the
    /// assumption that the role's `allowed_tools` list is the real safety
    /// boundary — interactive prompts only freeze the pane.
    #[arg(long)]
    pub require_permissions: bool,
    /// Pane layout applied after all role panes are spawned. Default `auto`
    /// picks `even-horizontal` for 2 panes and `tiled` for 3+. Ignored when
    /// `--placement window`.
    #[arg(long, value_enum, default_value_t = LayoutPreset::Auto)]
    pub layout: LayoutPreset,
    /// Where each role's pane lives. `split` (default) shares the current
    /// window; `window` opens a new tab per role.
    #[arg(long, value_enum, default_value_t = PlacementMode::Split)]
    pub placement: PlacementMode,
}

/// Where each role's pane lives in the tmux session.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, clap::ValueEnum)]
pub enum PlacementMode {
    /// `tmux split-window` in the current window. The window ends up with
    /// one pane per role (default; existing behaviour).
    #[default]
    Split,
    /// `tmux new-window` per role — each role gets its own tab, full
    /// width. The current window stays clean. Recommended once role count
    /// passes ~3.
    Window,
}

impl PlacementMode {
    /// Convert to the internal tmux placement enum.
    pub fn to_tmux(self) -> crate::tmux::Placement {
        match self {
            Self::Split => crate::tmux::Placement::SplitCurrent,
            Self::Window => crate::tmux::Placement::NewWindow,
        }
    }

    /// Does the placement produce one pane per window (so `select-layout`
    /// is irrelevant)?
    pub fn is_single_pane_per_window(self) -> bool {
        matches!(self, Self::Window)
    }
}

/// tmux `select-layout` presets caucus surfaces to operators. `Auto` lets
/// caucus pick based on pane count (even-horizontal for 2, tiled for 3+).
#[derive(Debug, Clone, Copy, Eq, PartialEq, clap::ValueEnum)]
pub enum LayoutPreset {
    Auto,
    Tiled,
    EvenHorizontal,
    EvenVertical,
    MainHorizontal,
    MainVertical,
}

impl LayoutPreset {
    /// Resolve to the literal tmux layout name. `Auto` is resolved
    /// elsewhere (it depends on pane count).
    pub fn as_tmux_name(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Tiled => Some("tiled"),
            Self::EvenHorizontal => Some("even-horizontal"),
            Self::EvenVertical => Some("even-vertical"),
            Self::MainHorizontal => Some("main-horizontal"),
            Self::MainVertical => Some("main-vertical"),
        }
    }
}

#[derive(Debug, Args)]
pub struct SessionShowArgs {
    pub session_id: String,
}

#[derive(Debug, Args)]
pub struct SessionConvergeArgs {
    pub session_id: String,
    /// Path to the decision markdown. Copied into `<session>/decision.md`.
    #[arg(long)]
    pub decision_file: PathBuf,
}

#[derive(Debug, Args)]
pub struct SessionDeadlockArgs {
    pub session_id: String,
    /// On deadlock, emit an `escalated.signal` and an `escalation` event so
    /// a human can take over. Implies session goes to `Abandoned` after
    /// the signal is written.
    #[arg(long, conflicts_with = "explore")]
    pub escalate: bool,
    /// On deadlock, spawn one execute agent per role using each role's last
    /// round response as its task.md — the "try every option in parallel"
    /// branch (dmux's multi-select philosophy). Session transitions to
    /// `Executing`.
    #[arg(long)]
    pub explore: bool,
    /// When --explore is set, an optional base ref for the new worktrees.
    #[arg(long, requires = "explore")]
    pub base_ref: Option<String>,
    /// When --explore is set, an optional model override.
    #[arg(long, requires = "explore")]
    pub model: Option<String>,
}

#[derive(Debug, Args)]
pub struct SessionKillArgs {
    pub session_id: String,
}

#[derive(Debug, Args)]
pub struct RoundArgs {
    #[command(subcommand)]
    pub action: RoundAction,
}

#[derive(Debug, Subcommand)]
pub enum RoundAction {
    /// Start round 1 of a session (or the next round if one is in progress).
    Start(RoundStartArgs),
    /// Report response collection status for the current round.
    Status(RoundStatusArgs),
    /// Begin a new round, re-using the existing meeting panes.
    Next(RoundNextArgs),
    /// Block until the target round's response files are all non-empty
    /// (or `--timeout-secs` elapses, or the session reaches a terminal
    /// state). Cheap polling-gate for CEO wakeup loops; companion to
    /// `caucus session is-terminal`.
    ///
    /// Exit codes:
    ///   0  — round completed (all response files non-empty)
    ///   1  — timeout, ctrl-c, or watcher closed unexpectedly
    ///   2  — user error (no such session, future round, no round started)
    ///   3  — session reached a terminal state before completion
    Wait(RoundWaitArgs),
}

#[derive(Debug, Args)]
pub struct RoundStartArgs {
    pub session_id: String,
    /// Path to the round's agenda markdown.
    #[arg(long)]
    pub agenda_file: PathBuf,
    /// Architect-led round: nudge this role first, wait for its sentinel,
    /// then write a follower brief that quotes its response and nudges
    /// the other roles. Without this flag every role gets the same agenda
    /// in parallel.
    #[arg(long)]
    pub lead: Option<String>,
    /// Sentinel timeout (seconds) for the lead. Only meaningful with
    /// `--lead`. Default 1800 (30 min).
    #[arg(long, default_value_t = 1800)]
    pub lead_timeout_secs: u64,
}

#[derive(Debug, Args)]
pub struct RoundStatusArgs {
    pub session_id: String,
}

#[derive(Debug, Args)]
pub struct RoundNextArgs {
    pub session_id: String,
    #[arg(long)]
    pub agenda_file: PathBuf,
    /// See `caucus round start --lead`.
    #[arg(long)]
    pub lead: Option<String>,
    /// See `caucus round start --lead-timeout-secs`.
    #[arg(long, default_value_t = 1800)]
    pub lead_timeout_secs: u64,
}

#[derive(Debug, Args)]
pub struct RoundWaitArgs {
    pub session_id: String,
    /// Round to wait on. Defaults to `session.current_round`. Errors out
    /// (exit 2) if greater than `current_round` — the round must already
    /// have been started.
    #[arg(long)]
    pub round: Option<u32>,
    /// Maximum wait in seconds. `0` means wait forever — only valid when
    /// explicitly set; the default is bounded.
    #[arg(long, default_value_t = 1800)]
    pub timeout_secs: u64,
}

#[derive(Debug, Args)]
pub struct ExecuteArgs {
    #[command(subcommand)]
    pub action: ExecuteAction,
}

#[derive(Debug, Subcommand)]
pub enum ExecuteAction {
    /// Spawn an execute-phase agent for one role in a new worktree.
    Start(ExecuteStartCliArgs),
    /// Plan → implement → review pipeline over a shared worktree.
    Pipeline(ExecutePipelineCliArgs),
    /// Per-role execute status (worktree path, derived_state).
    Status(ExecuteStatusArgs),
    /// Mark the agent finished; capture commit_provenance; queue cleanup.
    Finish(ExecuteFinishArgs),
    /// Mark the agent abandoned; queue worktree cleanup.
    Abandon(ExecuteAbandonArgs),
}

#[derive(Debug, Args)]
pub struct ExecutePipelineCliArgs {
    pub session_id: String,
    /// Markdown file the first step consumes as `task.md`.
    #[arg(long)]
    pub task_file: PathBuf,
    /// Optional planner role — runs first, its `response.md` becomes the
    /// implementer's task. Omit to feed `--task-file` straight to the
    /// implementer.
    #[arg(long)]
    pub plan: Option<String>,
    /// Implementer role — required. Writes code in the shared worktree.
    #[arg(long)]
    pub implement: String,
    /// Optional reviewer role — runs after implement and decides
    /// `APPROVE` vs `BLOCK`.
    #[arg(long)]
    pub review: Option<String>,
    /// Retry budget: when the reviewer says BLOCK, regenerate plan → impl
    /// up to N more times before declaring `Blocked`. Default 0 = no retry.
    #[arg(long, default_value_t = 0)]
    pub retry_on_block: u32,
    /// Per-step sentinel timeout in seconds.
    #[arg(long, default_value_t = 1800)]
    pub step_timeout_secs: u64,
    #[arg(long)]
    pub base_ref: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub require_permissions: bool,
    /// Where the pipeline's role panes live. Default `split`.
    #[arg(long, value_enum, default_value_t = PlacementMode::Split)]
    pub placement: PlacementMode,
    /// Resume each step's meeting-phase claude session in the shared
    /// worktree (plan ← meeting architect, impl ← meeting backend,
    /// review ← meeting reviewer). Cuts the "fresh context per step"
    /// cost — see `caucus execute start --continue-meeting` for the
    /// single-role version. Kills the corresponding meeting panes before
    /// spawning the pipeline's first step; on retry, kills the previous
    /// attempt's panes before re-resuming. Requires every pipeline role
    /// to have a meeting agent with a captured claude_session_id.
    #[arg(long)]
    pub continue_meeting: bool,
}

#[derive(Debug, Args)]
pub struct ExecuteStartCliArgs {
    pub session_id: String,
    #[arg(long)]
    pub role: String,
    #[arg(long)]
    pub task_file: PathBuf,
    #[arg(long)]
    pub base_ref: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    /// Opt back into the agent CLI's permission prompts (see
    /// `caucus session new --require-permissions`).
    #[arg(long)]
    pub require_permissions: bool,
    /// Pane layout applied after the execute pane is spawned. Default `auto`.
    /// Ignored when `--placement window`.
    #[arg(long, value_enum, default_value_t = LayoutPreset::Auto)]
    pub layout: LayoutPreset,
    /// Where the execute pane lives. Default `split` (same window).
    #[arg(long, value_enum, default_value_t = PlacementMode::Split)]
    pub placement: PlacementMode,
    /// Resume the meeting-phase agent's Claude session in the new worktree
    /// instead of starting a fresh one. Kills the meeting pane (claude
    /// refuses two concurrent resumes of the same session id). Requires the
    /// meeting agent to have produced at least one sentinel so caucus
    /// captured its claude session id. Claude-only — codex roles ignore
    /// this flag and always start a fresh process.
    #[arg(long)]
    pub continue_meeting: bool,
}

#[derive(Debug, Args)]
pub struct ExecuteStatusArgs {
    pub session_id: String,
}

#[derive(Debug, Args)]
pub struct ExecuteFinishArgs {
    pub session_id: String,
    #[arg(long)]
    pub role: String,
}

#[derive(Debug, Args)]
pub struct ExecuteAbandonArgs {
    pub session_id: String,
    #[arg(long)]
    pub role: String,
}

#[derive(Debug, Args)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub action: AgentAction,
}

#[derive(Debug, Subcommand)]
pub enum AgentAction {
    /// List agents in a session, optionally filtered by kind.
    List(AgentListArgs),
    /// Dump the manifest JSON for one agent.
    Show(AgentShowArgs),
    /// Send a raw line to the agent's pane (escape-quoted).
    Send(AgentSendArgs),
    /// Kill the agent's pane and mark it Killed.
    Kill(AgentKillArgs),
}

#[derive(Debug, Args)]
pub struct AgentListArgs {
    pub session_id: String,
    /// Filter by agent kind. Default: all.
    #[arg(long, value_enum, default_value_t = AgentKindFilter::All)]
    pub kind: AgentKindFilter,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum AgentKindFilter {
    All,
    Meeting,
    Execute,
}

#[derive(Debug, Args)]
pub struct AgentShowArgs {
    pub agent_id: String,
    /// Session id is required because manifests live under the session root.
    #[arg(long)]
    pub session: String,
}

#[derive(Debug, Args)]
pub struct AgentSendArgs {
    pub agent_id: String,
    #[arg(long)]
    pub session: String,
    pub message: String,
}

#[derive(Debug, Args)]
pub struct AgentKillArgs {
    pub agent_id: String,
    #[arg(long)]
    pub session: String,
}

#[derive(Debug, Args)]
pub struct RoleArgs {
    #[command(subcommand)]
    pub action: RoleAction,
}

#[derive(Debug, Subcommand)]
pub enum RoleAction {
    /// List the merged role registry (embedded defaults + global + project).
    List,
    /// Show one role's spec (name, tools, permission mode, template path).
    Show(RoleShowArgs),
}

#[derive(Debug, Args)]
pub struct RoleShowArgs {
    pub name: String,
}

#[derive(Debug, Args)]
pub struct SentinelArgs {
    #[command(subcommand)]
    pub action: SentinelAction,
}

#[derive(Debug, Subcommand)]
pub enum SentinelAction {
    /// Write a sentinel file. Invoked by the Claude Stop hook script via
    /// `bin/sentinel-stop`.
    Write(SentinelWriteArgs),
}

#[derive(Debug, Args)]
pub struct SentinelWriteArgs {
    #[arg(long)]
    pub session: String,
    #[arg(long)]
    pub agent: String,
    #[arg(long, value_enum, default_value = "stop")]
    pub kind: SentinelKindArg,
    /// One-line summary the hook script extracted. Omit if not available.
    #[arg(long)]
    pub last_message: Option<String>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SentinelKindArg {
    Stop,
    ToolBlocked,
    Error,
    Killed,
}

impl From<SentinelKindArg> for crate::sentinel::SentinelKind {
    fn from(arg: SentinelKindArg) -> Self {
        use crate::sentinel::SentinelKind::*;
        match arg {
            SentinelKindArg::Stop => Stop,
            SentinelKindArg::ToolBlocked => ToolBlocked,
            SentinelKindArg::Error => Error,
            SentinelKindArg::Killed => Killed,
        }
    }
}

#[derive(Debug, Args)]
pub struct WatchArgs {
    pub session_id: String,
}
