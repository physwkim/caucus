//! Non-TUI subcommand dispatch (`docs/design.md` §10).
//!
//! `caucus` with no subcommand launches the multiplexer TUI. The subcommands
//! here are for bootstrap and hooks only — live control (`send_keys`,
//! `spawn_role`, ...) is exposed to the CEO over MCP, not the CLI.
//!
//! Exit codes follow `docs/design.md` §10.1:
//! `0` ok · `2` user error · `3` environment error · `4` bad caucus state ·
//! `1` unexpected failure.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::signal::TurnKind;

/// `caucus` — a terminal multiplexer for teams of AI coding agents.
#[derive(Debug, Parser)]
#[command(name = "caucus", version, about)]
pub struct Cli {
    /// Initial panel roster (comma-separated role names). Omit for a lone CEO
    /// panel. Only meaningful when launching the TUI.
    #[arg(long, value_delimiter = ',')]
    pub roles: Vec<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Non-TUI subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create `.caucus/` + `bin/turn-signal`, optionally install the Stop hook.
    Init {
        /// Also merge the Claude `Stop` hook into `~/.claude/settings.json`.
        #[arg(long)]
        install_hook: bool,
    },
    /// Check git / agent CLIs / hook / role allowlists.
    Doctor,
    /// Turn-signal client + (future) related signal subcommands.
    #[command(subcommand)]
    Signal(SignalCommand),
    /// Inspect role definitions.
    #[command(subcommand)]
    Role(RoleCommand),
}

/// `caucus signal ...` — the turn-signal hook client.
#[derive(Debug, Subcommand)]
pub enum SignalCommand {
    /// Post a turn signal to the caucus socket. Invoked by the Stop hook
    /// script, not by a human.
    Post {
        /// Path to the caucus turn-signal socket.
        #[arg(long)]
        sock: PathBuf,
        /// Session id (`CAUCUS_SESSION_ID`).
        #[arg(long)]
        session: String,
        /// Panel id (`CAUCUS_PANEL_ID`).
        #[arg(long)]
        panel: String,
        /// Signal kind.
        #[arg(long, value_enum, default_value = "stop")]
        kind: SignalKindArg,
    },
}

/// CLI spelling of [`TurnKind`].
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SignalKindArg {
    Stop,
    ToolBlocked,
    Error,
}

impl From<SignalKindArg> for TurnKind {
    fn from(arg: SignalKindArg) -> Self {
        match arg {
            SignalKindArg::Stop => TurnKind::Stop,
            SignalKindArg::ToolBlocked => TurnKind::ToolBlocked,
            SignalKindArg::Error => TurnKind::Error,
        }
    }
}

/// `caucus role ...` — role inspection.
#[derive(Debug, Subcommand)]
pub enum RoleCommand {
    /// List known roles.
    List,
    /// Show one role's full spec.
    Show {
        /// Role name.
        name: String,
    },
}

/// Process entry point. Parses args and dispatches; returns the process exit
/// code per `docs/design.md` §10.1.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("caucus: {err:#}");
            ExitCode::from(1)
        }
    }
}

/// Dispatch a parsed [`Cli`]. No subcommand launches the TUI.
fn dispatch(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        None => run_tui(&cli.roles),
        Some(Command::Init { install_hook }) => run_init(install_hook),
        Some(Command::Doctor) => run_doctor(),
        Some(Command::Signal(cmd)) => run_signal(cmd),
        Some(Command::Role(cmd)) => run_role(cmd),
    }
}

/// Launch the full-screen multiplexer TUI.
fn run_tui(roles: &[String]) -> Result<ExitCode> {
    // TODO(phase 2): build the `Session`, spawn the CEO panel (+ `roles`),
    // run the ratatui event loop with PTY pumps and the MCP + signal servers.
    let _ = roles;
    println!("caucus TUI: TODO");
    Ok(ExitCode::SUCCESS)
}

/// `caucus init [--install-hook]`.
fn run_init(install_hook: bool) -> Result<ExitCode> {
    // TODO(phase 2): create `.caucus/` + `bin/turn-signal`; when
    // `install_hook`, merge the Stop hook into `~/.claude/settings.json`.
    let _ = install_hook;
    todo!("phase 2: caucus init")
}

/// `caucus doctor`.
fn run_doctor() -> Result<ExitCode> {
    // TODO(phase 2): load `Config`, run `doctor::run`, print the report,
    // map `Report::worst()` to an exit code.
    todo!("phase 2: caucus doctor")
}

/// `caucus signal ...`.
fn run_signal(cmd: SignalCommand) -> Result<ExitCode> {
    match cmd {
        SignalCommand::Post { .. } => {
            // TODO(phase 2): read the hook payload from stdin, build a
            // `TurnSignal`, connect to `sock`, write one JSON line.
            todo!("phase 2: caucus signal post")
        }
    }
}

/// `caucus role list | show <name>`.
fn run_role(cmd: RoleCommand) -> Result<ExitCode> {
    match cmd {
        RoleCommand::List => {
            // TODO(phase 2): load `Config`, print role names.
            todo!("phase 2: caucus role list")
        }
        RoleCommand::Show { .. } => {
            // TODO(phase 2): load `Config`, print the role spec.
            todo!("phase 2: caucus role show")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_subcommand_parses() {
        let cli = Cli::try_parse_from(["caucus"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn signal_post_parses() {
        let cli = Cli::try_parse_from([
            "caucus", "signal", "post", "--sock", "/tmp/c.sock", "--session", "S", "--panel", "P",
            "--kind", "stop",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Signal(SignalCommand::Post { .. }))
        ));
    }

    #[test]
    fn roles_flag_splits_on_comma() {
        let cli = Cli::try_parse_from(["caucus", "--roles", "architect,backend"]).unwrap();
        assert_eq!(cli.roles, vec!["architect", "backend"]);
    }
}
