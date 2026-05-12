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
}

#[derive(Debug, Args)]
pub struct RoundStartArgs {
    pub session_id: String,
    /// Path to the round's agenda markdown.
    #[arg(long)]
    pub agenda_file: PathBuf,
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
    /// Per-role execute status (worktree path, derived_state).
    Status(ExecuteStatusArgs),
    /// Mark the agent finished; capture commit_provenance; queue cleanup.
    Finish(ExecuteFinishArgs),
    /// Mark the agent abandoned; queue worktree cleanup.
    Abandon(ExecuteAbandonArgs),
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
    /// Dump the manifest JSON for one agent.
    Show(AgentShowArgs),
    /// Send a raw line to the agent's pane (escape-quoted).
    Send(AgentSendArgs),
    /// Kill the agent's pane and mark it Killed.
    Kill(AgentKillArgs),
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
