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
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::doctor::{self, Severity};
use crate::session::id::{PanelId, SessionId};
use crate::signal::TurnKind;

/// Exit code for an environment error (`docs/design.md` §10.1).
const EXIT_ENV_ERROR: u8 = 3;
/// Exit code for a user error — bad arguments (`docs/design.md` §10.1).
const EXIT_USER_ERROR: u8 = 2;

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

/// The git repository caucus operates on — the current working directory.
fn repo_root() -> Result<PathBuf> {
    std::env::current_dir().context("determine current working directory")
}

/// Launch the full-screen multiplexer TUI (`docs/design.md` §0 #2).
///
/// Builds the session, spawns the CEO panel plus any `--roles`, and runs the
/// ratatui event loop. When stdout is not a tty, [`crate::tui::run`] fails
/// cleanly with a message rather than panicking.
fn run_tui(roles: &[String]) -> Result<ExitCode> {
    let repo = repo_root()?;
    crate::tui::run(&repo, roles)?;
    Ok(ExitCode::SUCCESS)
}

/// `caucus init [--install-hook]` — create `.caucus/` + `bin/turn-signal`,
/// optionally merge the Claude Stop hook.
fn run_init(install_hook: bool) -> Result<ExitCode> {
    let repo = repo_root()?;
    let outcome = crate::init::run(&repo, install_hook)?;
    eprintln!("caucus init:");
    eprintln!("  .caucus dir:   {}", outcome.caucus_dir.display());
    eprintln!("  hook script:   {}", outcome.hook_script.display());
    match &outcome.hook_install {
        Some(crate::init::HookInstall::Merged { settings, backup }) => {
            eprintln!("  Claude Stop hook merged: {}", settings.display());
            if let Some(bak) = backup {
                eprintln!("  prior settings backed up: {}", bak.display());
            }
        }
        Some(crate::init::HookInstall::AlreadyPresent { settings }) => {
            eprintln!(
                "  Claude Stop hook already present in {} — no change",
                settings.display()
            );
        }
        None => {
            eprintln!("  (Stop hook not installed — re-run with --install-hook)");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `caucus doctor` — environment + configuration health check.
///
/// Exit code maps from the worst check severity (`docs/design.md` §10.1):
/// any `Error` → `3` (environment error); otherwise `0`.
fn run_doctor() -> Result<ExitCode> {
    let repo = repo_root()?;
    let config = Config::load(&repo).context("load caucus configuration")?;
    let report = doctor::run(&config);

    eprintln!("caucus doctor — {} check(s):", report.checks.len());
    for check in &report.checks {
        let marker = match check.severity {
            Severity::Ok => "ok  ",
            Severity::Warn => "warn",
            Severity::Error => "ERR ",
        };
        eprintln!("  [{}] {}: {}", marker, check.name, check.detail);
    }

    let code = match report.worst() {
        Severity::Error => EXIT_ENV_ERROR,
        Severity::Ok | Severity::Warn => 0,
    };
    Ok(ExitCode::from(code))
}

/// `caucus signal post ...` — the turn-signal hook client.
fn run_signal(cmd: SignalCommand) -> Result<ExitCode> {
    match cmd {
        SignalCommand::Post {
            sock,
            session,
            panel,
            kind,
        } => {
            let session_id = SessionId::from_str(&session)
                .with_context(|| format!("invalid --session id '{session}'"))?;
            let panel_id = PanelId::from_str(&panel)
                .with_context(|| format!("invalid --panel id '{panel}'"))?;
            crate::signal::post::run(&sock, session_id, panel_id, kind.into())?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// `caucus role list | show <name>` — role inspection.
fn run_role(cmd: RoleCommand) -> Result<ExitCode> {
    let repo = repo_root()?;
    let config = Config::load(&repo).context("load caucus configuration")?;
    match cmd {
        RoleCommand::List => {
            for spec in config.roles.specs() {
                println!("{:<18} {}", spec.name, spec.description);
            }
            Ok(ExitCode::SUCCESS)
        }
        RoleCommand::Show { name } => match config.roles.get(&name) {
            Ok(spec) => {
                println!("name:            {}", spec.name);
                println!("description:     {}", spec.description);
                println!("agent_cli:       {}", spec.agent_cli.binary());
                println!(
                    "model:           {}",
                    spec.model.as_deref().unwrap_or("(CLI default)")
                );
                println!("permission_mode: {}", spec.permission_mode);
                println!("allowed_tools:   {}", spec.allowed_tools_csv());
                println!("prompt_template: {}", spec.system_prompt_template);
                Ok(ExitCode::SUCCESS)
            }
            Err(err) => {
                eprintln!("caucus: {err}");
                Ok(ExitCode::from(EXIT_USER_ERROR))
            }
        },
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
