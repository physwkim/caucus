//! Non-TUI subcommand dispatch (`docs/design.md` §10).
//!
//! `caucus` with no subcommand launches the multiplexer TUI. The subcommands
//! here are for bootstrap and hooks only — live control (`send_keys`,
//! `spawn_role`, ...) is exposed to the main worker over MCP, not the CLI.
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
use crate::role::spec::AgentCli;
use crate::session::id::{PanelId, SessionId};
use crate::signal::{NoteKind, TurnKind};

/// Exit code for an environment error (`docs/design.md` §10.1).
const EXIT_ENV_ERROR: u8 = 3;
/// Exit code for a user error — bad arguments (`docs/design.md` §10.1).
const EXIT_USER_ERROR: u8 = 2;

/// `caucus` — a terminal multiplexer for teams of AI coding agents.
#[derive(Debug, Parser)]
#[command(name = "caucus", version, about)]
pub struct Cli {
    /// Initial panel roster (comma-separated role names). Omit for a lone main
    /// worker panel. Only meaningful when launching the TUI.
    #[arg(long, value_delimiter = ',')]
    pub roles: Vec<String>,

    /// Backend CLI for the main worker panel: `claude` (default) or `codex`.
    /// Sub-agent backends are chosen per `spawn_role`, not by this flag. Only
    /// meaningful when launching the TUI.
    #[arg(long, value_enum)]
    pub agent_cli: Option<AgentCli>,

    /// caucus's reserved prefix key — `Ctrl-<letter>` selects caucus commands
    /// (panel focus, zoom, layout, ...). Resolution order: this flag /
    /// `CAUCUS_PREFIX`, then the `[settings] prefix` key
    /// (`~/.caucus/settings.toml` < `<repo>/.caucus/settings.toml`), then the
    /// default `a` (`Ctrl-A`) — which auto-dodges to `Ctrl-B` when launched
    /// inside a tmux whose own prefix is `Ctrl-A`
    /// ([`crate::input::effective_prefix`]). Accepts a bare letter or a
    /// `ctrl-`/`c-`/`^` form. Only meaningful when launching the TUI (fresh
    /// or `resume`).
    #[arg(long, env = "CAUCUS_PREFIX")]
    pub prefix: Option<PrefixKey>,

    /// A short human label for this session, shown in `caucus sessions` so the
    /// list distinguishes runs instead of reading "caucus session" for every
    /// one. Omit to default to the repository's directory name. Only meaningful
    /// when launching a fresh TUI (not `resume`, which keeps the saved topic).
    #[arg(long)]
    pub topic: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The caucus prefix key, parsed from `--prefix` / `CAUCUS_PREFIX`. caucus
/// reserves a `Ctrl-<letter>` chord; the wrapped value is that lowercase
/// letter (e.g. `'b'` for `Ctrl-B`). A leading `ctrl-`, `ctrl+`, `c-`, or `^`
/// is accepted and stripped, so `b`, `ctrl-b`, `C-b`, and `^b` all parse the
/// same. Only ASCII letters are accepted — they are the keys that map cleanly
/// to a `Ctrl` control code.
#[derive(Debug, Clone, Copy)]
pub struct PrefixKey(pub char);

impl FromStr for PrefixKey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lowered = s.trim().to_ascii_lowercase();
        let key = lowered
            .strip_prefix("ctrl-")
            .or_else(|| lowered.strip_prefix("ctrl+"))
            .or_else(|| lowered.strip_prefix("c-"))
            .or_else(|| lowered.strip_prefix('^'))
            .unwrap_or(&lowered);
        let mut chars = key.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_alphabetic() => Ok(PrefixKey(c)),
            _ => Err(format!(
                "prefix must be a single Ctrl+<letter> key, e.g. `b` or `ctrl-b`; got `{s}`"
            )),
        }
    }
}

/// Non-TUI subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create `.caucus/`, optionally install the machine-wide Stop hook.
    Init {
        /// Also merge the Claude `Stop` hook into `~/.claude/settings.json`.
        #[arg(long)]
        install_hook: bool,
    },
    /// Check caucus version, git + repo, agent CLIs, hook, role allowlists.
    Doctor,
    /// Turn-signal client + (future) related signal subcommands.
    #[command(subcommand)]
    Signal(SignalCommand),
    /// Inspect role definitions.
    #[command(subcommand)]
    Role(RoleCommand),
    /// Stdio MCP server for the main worker panel — forwards the fourteen
    /// caucus tools to the main process over the control socket
    /// (`docs/design.md` §0 #4). Spawned by the main worker's Claude Code
    /// instance, not by a human.
    McpServe {
        /// Path to the main caucus process's control socket.
        #[arg(long)]
        control_sock: PathBuf,
    },
    /// List resumable sessions persisted under `.caucus/sessions/`.
    Sessions {
        /// Output format.
        #[arg(long, value_enum, default_value = "text")]
        format: OutputFormat,
    },
    /// Relaunch the TUI restoring a previously-persisted session.
    Resume {
        /// Session id to resume (see `caucus sessions`).
        session_id: String,
    },
    /// Reclaim disk by pruning old, not-running sessions under
    /// `.caucus/sessions/` (resume records, panel logs, agent manifests).
    /// Dry-run unless `--prune`; git branches and worktrees are left untouched.
    Gc {
        /// Prune sessions older than this window: `30m`, `24h`, `7d`, `2w`
        /// (a bare number is days). Default `7d`.
        #[arg(long, default_value = "7d", value_parser = crate::gc::parse_retention)]
        older_than: chrono::Duration,
        /// Actually delete. Without it, gc only prints what it would remove.
        #[arg(long)]
        prune: bool,
    },
}

/// Output format for listing subcommands.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

/// `caucus signal ...` — the turn-signal hook client.
#[derive(Debug, Subcommand)]
pub enum SignalCommand {
    /// Post a turn signal to the caucus socket. Invoked by the Stop hook
    /// script, not by a human.
    Post {
        /// Path to the caucus turn-signal socket.
        #[arg(
            long,
            required_unless_present = "discover",
            conflicts_with = "discover"
        )]
        sock: Option<PathBuf>,
        /// Session id (`CAUCUS_SESSION_ID`).
        #[arg(
            long,
            required_unless_present = "discover",
            conflicts_with = "discover"
        )]
        session: Option<String>,
        /// Panel id (`CAUCUS_PANEL_ID`).
        #[arg(
            long,
            required_unless_present = "discover",
            conflicts_with = "discover"
        )]
        panel: Option<String>,
        /// No `CAUCUS_*` env (the conversation was re-hosted outside the
        /// panel's process tree): broadcast the payload as an unbound signal
        /// to every live caucus signal socket instead; each server decides
        /// from the payload whether it owns the conversation.
        #[arg(long)]
        discover: bool,
        /// Signal kind.
        #[arg(long, value_enum, default_value = "stop")]
        kind: SignalKindArg,
    },
    /// Post a mid-turn note — structured progress, an artifact reference, or a
    /// question — without ending the turn. Invoked by an agent from its own
    /// shell: the socket and ids default to the `CAUCUS_*` env caucus injects
    /// into every panel, so a panel agent just runs
    /// `caucus signal note --kind progress "half done, 3 files left"`.
    Note {
        /// Path to the caucus turn-signal socket.
        #[arg(long, env = "CAUCUS_SOCK")]
        sock: PathBuf,
        /// Session id (`CAUCUS_SESSION_ID`).
        #[arg(long, env = "CAUCUS_SESSION_ID")]
        session: String,
        /// Panel id (`CAUCUS_PANEL_ID`).
        #[arg(long, env = "CAUCUS_PANEL_ID")]
        panel: String,
        /// Note kind. `question` is forwarded to the main worker.
        #[arg(long, value_enum, default_value = "progress")]
        kind: NoteKindArg,
        /// The note body.
        body: String,
    },
    /// Post a turn signal from codex's `notify` program. codex has no `Stop`
    /// hook; it invokes this on `agent-turn-complete`, appending the event JSON
    /// as the final positional argument (not stdin, unlike the claude hook).
    /// Only an `agent-turn-complete` event posts a `Stop` signal; any other
    /// event is a no-op, so codex's other notifications never spuriously settle
    /// a panel. Registered via `-c notify=[...]` at spawn, not invoked by hand.
    CodexNotify {
        /// Path to the caucus turn-signal socket.
        #[arg(long)]
        sock: PathBuf,
        /// Session id (`CAUCUS_SESSION_ID`).
        #[arg(long)]
        session: String,
        /// Panel id (`CAUCUS_PANEL_ID`).
        #[arg(long)]
        panel: String,
        /// The notify event JSON codex appends as the final argument. Absent
        /// (or unparseable) is a no-op rather than an error.
        payload: Option<String>,
    },
}

/// CLI spelling of the signal kinds `caucus signal post` accepts: the
/// [`TurnKind`]s the Stop hook posts, plus the lifecycle hooks (`post-compact`,
/// `session-start`) whose payloads carry their own discriminant on stdin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SignalKindArg {
    Stop,
    ToolBlocked,
    Error,
    PostCompact,
    SessionStart,
}

impl SignalKindArg {
    /// The turn kind this argument names, or `None` for a lifecycle kind —
    /// which `run_signal` routes to `post::run_lifecycle` instead.
    fn turn_kind(self) -> Option<TurnKind> {
        match self {
            SignalKindArg::Stop => Some(TurnKind::Stop),
            SignalKindArg::ToolBlocked => Some(TurnKind::ToolBlocked),
            SignalKindArg::Error => Some(TurnKind::Error),
            SignalKindArg::PostCompact | SignalKindArg::SessionStart => None,
        }
    }

    /// The lifecycle hook this argument names, or `None` for a turn kind.
    fn lifecycle_hook(self) -> Option<crate::signal::post::LifecycleHook> {
        use crate::signal::post::LifecycleHook;
        match self {
            SignalKindArg::PostCompact => Some(LifecycleHook::PostCompact),
            SignalKindArg::SessionStart => Some(LifecycleHook::SessionStart),
            SignalKindArg::Stop | SignalKindArg::ToolBlocked | SignalKindArg::Error => None,
        }
    }
}

/// CLI spelling of [`NoteKind`].
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum NoteKindArg {
    Progress,
    Artifact,
    Question,
}

impl From<NoteKindArg> for NoteKind {
    fn from(arg: NoteKindArg) -> Self {
        match arg {
            NoteKindArg::Progress => NoteKind::Progress,
            NoteKindArg::Artifact => NoteKind::Artifact,
            NoteKindArg::Question => NoteKind::Question,
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
    // `None` = no --prefix/CAUCUS_PREFIX given; the TUI resolves it against
    // `[settings] prefix` and the tmux-aware default (`input::effective_prefix`).
    let prefix = cli.prefix.map(|p| p.0);
    match cli.command {
        None => run_tui(&cli.roles, cli.agent_cli, prefix, cli.topic),
        Some(Command::Init { install_hook }) => run_init(install_hook),
        Some(Command::Doctor) => run_doctor(),
        Some(Command::Signal(cmd)) => run_signal(cmd),
        Some(Command::Role(cmd)) => run_role(cmd),
        Some(Command::McpServe { control_sock }) => run_mcp_serve(&control_sock),
        Some(Command::Sessions { format }) => run_sessions(format),
        Some(Command::Resume { session_id }) => run_resume(&session_id, prefix),
        Some(Command::Gc { older_than, prune }) => run_gc(older_than, prune),
    }
}

/// `caucus mcp-serve --control-sock <path>` — run the stdio MCP server.
///
/// Blocks serving JSON-RPC over stdio until `stdin` reaches EOF (the parent
/// Claude Code instance closed the pipe). A control-socket I/O failure is a
/// per-call error surfaced as an MCP tool error, not a process exit.
fn run_mcp_serve(control_sock: &std::path::Path) -> Result<ExitCode> {
    crate::mcp::serve::run(control_sock).context("run caucus mcp-serve")?;
    Ok(ExitCode::SUCCESS)
}

/// The git repository caucus operates on — the current working directory.
fn repo_root() -> Result<PathBuf> {
    std::env::current_dir().context("determine current working directory")
}

/// Launch the full-screen multiplexer TUI (`docs/design.md` §0 #2).
///
/// Builds the session, spawns the main worker panel (on `main_cli`, default
/// claude) plus any `--roles`, and runs the ratatui event loop. When stdout is
/// not a tty, [`crate::tui::run`] fails cleanly with a message rather than
/// panicking.
fn run_tui(
    roles: &[String],
    main_cli: Option<AgentCli>,
    prefix: Option<char>,
    topic: Option<String>,
) -> Result<ExitCode> {
    let repo = repo_root()?;
    crate::tui::run(&repo, roles, main_cli, prefix, topic)?;
    Ok(ExitCode::SUCCESS)
}

/// `caucus init [--install-hook]` — create `.caucus/`, optionally write the
/// machine-wide turn-signal script and merge the Claude Stop hook.
fn run_init(install_hook: bool) -> Result<ExitCode> {
    let repo = repo_root()?;
    let outcome = crate::init::run(&repo, install_hook)?;
    eprintln!("caucus init:");
    eprintln!("  .caucus dir:   {}", outcome.caucus_dir.display());
    if let Some(script) = &outcome.hook_script {
        eprintln!("  hook script:   {}", script.display());
    }
    match &outcome.gitignore {
        crate::init::GitignoreOutcome::Updated {
            path,
            created: true,
        } => {
            eprintln!(
                "  .gitignore:    created {} ignoring .caucus/sessions/",
                path.display()
            );
        }
        crate::init::GitignoreOutcome::Updated {
            path,
            created: false,
        } => {
            eprintln!(
                "  .gitignore:    added .caucus/sessions/ to {}",
                path.display()
            );
        }
        crate::init::GitignoreOutcome::AlreadyIgnored { path } => {
            eprintln!(
                "  .gitignore:    session state already ignored in {} — no change",
                path.display()
            );
        }
    }
    match &outcome.hook_install {
        Some(crate::init::HookInstall::Merged {
            settings,
            backup,
            migrated,
        }) => {
            eprintln!("  Claude Stop hook merged: {}", settings.display());
            if *migrated {
                eprintln!("  migrated: removed a stale caucus Stop hook (e.g. sentinel-stop)");
            }
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
    let report = doctor::run(&repo, &config);

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
            discover,
            kind,
        } => {
            if discover {
                // No env, no identity: broadcast the payload as an unbound
                // signal; each live caucus server resolves ownership itself
                // (`docs/design.md` §7.8).
                //
                // Only a *turn* kind can ride the unbound path: `UnboundSignal`
                // carries a `TurnKind`, and panel resolution reads the
                // conversation's transcript lineage — which a lifecycle hook's
                // payload names just as well, but which no lifecycle consumer
                // is wired to yet (design §7.9, known bound). A lifecycle kind
                // here posts nothing rather than failing: a hook process must
                // never fail the panel it runs in.
                let Some(kind) = kind.turn_kind() else {
                    return Ok(ExitCode::SUCCESS);
                };
                crate::signal::post::run_discover(kind)?;
                return Ok(ExitCode::SUCCESS);
            }
            // clap enforces the three flags whenever --discover is absent.
            let (sock, session, panel) = (
                sock.expect("clap requires --sock without --discover"),
                session.expect("clap requires --session without --discover"),
                panel.expect("clap requires --panel without --discover"),
            );
            let session_id = SessionId::from_str(&session)
                .with_context(|| format!("invalid --session id '{session}'"))?;
            let panel_id = PanelId::from_str(&panel)
                .with_context(|| format!("invalid --panel id '{panel}'"))?;
            if let Some(hook) = kind.lifecycle_hook() {
                // A lifecycle hook (`PostCompact` / `SessionStart`) posts a
                // fire-and-forget lifecycle signal; its payload's own
                // `trigger` / `source` field is read from stdin.
                crate::signal::post::run_lifecycle(&sock, session_id, panel_id, hook)?;
                return Ok(ExitCode::SUCCESS);
            }
            let kind = kind.turn_kind().expect("non-lifecycle kind is a turn kind");
            // Reply capability is negotiated by env, never by flag
            // (`docs/design.md` §7.6): the hook script body stays fixed, and a
            // caucus that can answer injects `CAUCUS_HOOK_REPLY=1` into the
            // claude main panel alone — any other combination of binary ages
            // degrades to today's fire-and-forget post.
            let wants_reply = std::env::var("CAUCUS_HOOK_REPLY").as_deref() == Ok("1");
            crate::signal::post::run(&sock, session_id, panel_id, kind, wants_reply)?;
            Ok(ExitCode::SUCCESS)
        }
        SignalCommand::Note {
            sock,
            session,
            panel,
            kind,
            body,
        } => {
            let session_id = SessionId::from_str(&session)
                .with_context(|| format!("invalid --session id '{session}'"))?;
            let panel_id = PanelId::from_str(&panel)
                .with_context(|| format!("invalid --panel id '{panel}'"))?;
            crate::signal::post::run_note(&sock, session_id, panel_id, kind.into(), body)?;
            Ok(ExitCode::SUCCESS)
        }
        SignalCommand::CodexNotify {
            sock,
            session,
            panel,
            payload,
        } => {
            let session_id = SessionId::from_str(&session)
                .with_context(|| format!("invalid --session id '{session}'"))?;
            let panel_id = PanelId::from_str(&panel)
                .with_context(|| format!("invalid --panel id '{panel}'"))?;
            crate::signal::post::run_codex_notify(&sock, session_id, panel_id, payload.as_deref())?;
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

/// `caucus sessions [--format json]` — list resumable sessions.
///
/// Scans `<repo>/.caucus/sessions/*/session.json`, newest first. Text mode
/// prints id, topic, panel count, and age; JSON mode emits the records array.
fn run_sessions(format: OutputFormat) -> Result<ExitCode> {
    let repo = repo_root()?;
    let records = crate::session::record::discover(&repo);
    match format {
        OutputFormat::Json => {
            let json =
                serde_json::to_string_pretty(&records).context("serialise session records")?;
            println!("{json}");
        }
        OutputFormat::Text => {
            if records.is_empty() {
                eprintln!("caucus: no resumable sessions under .caucus/sessions/");
            } else {
                println!("{:<28} {:<10} {:<8} TOPIC", "SESSION", "AGE", "PANELS");
                let now = chrono::Utc::now();
                for rec in &records {
                    println!(
                        "{:<28} {:<10} {:<8} {}",
                        rec.id,
                        humanize_age(now - rec.created_at),
                        rec.panels.len(),
                        rec.topic,
                    );
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Compact human-readable age string for a `chrono` duration.
fn humanize_age(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// `caucus resume <session_id>` — relaunch the TUI restoring a session.
fn run_resume(session_id: &str, prefix: Option<char>) -> Result<ExitCode> {
    let repo = repo_root()?;
    let id = SessionId::from_str(session_id)
        .with_context(|| format!("invalid session id '{session_id}'"))?;
    crate::tui::run_resumed(&repo, id, prefix)?;
    Ok(ExitCode::SUCCESS)
}

/// `caucus gc [--older-than DUR] [--prune]` — prune old, not-running session
/// state under `.caucus/sessions/`.
///
/// Prints the plan and, only with `--prune`, removes the directories. A removal
/// failure exits `1` (unexpected failure, `docs/design.md` §10.1); a clean run,
/// a dry-run, and "nothing to prune" all exit `0`.
fn run_gc(older_than: chrono::Duration, prune: bool) -> Result<ExitCode> {
    let repo = repo_root()?;
    let plan = crate::gc::plan(&repo, older_than, chrono::Utc::now());

    if plan.prunable.is_empty() && plan.skipped_live.is_empty() {
        eprintln!(
            "caucus gc: no sessions older than {} under .caucus/sessions/",
            humanize_age(older_than)
        );
        return Ok(ExitCode::SUCCESS);
    }

    for id in &plan.skipped_live {
        eprintln!("  skip {id}  (running — lock held)");
    }
    for session in &plan.prunable {
        eprintln!(
            "  {} {:<6} {}",
            session.id,
            humanize_age(session.age),
            session.topic,
        );
    }

    if !prune {
        eprintln!(
            "\n{} session(s) would be pruned. Re-run with --prune to delete.",
            plan.prunable.len()
        );
        return Ok(ExitCode::SUCCESS);
    }

    let report = crate::gc::execute(&plan);
    eprintln!("\ncaucus gc: pruned {} session(s)", report.removed.len());
    for id in &report.raced {
        eprintln!("  kept {id}  (a caucus opened it before deletion)");
    }
    for (id, err) in &report.failed {
        eprintln!("  FAILED {id}: {err}");
    }
    if report.failed.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
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
    fn prefix_flag_defaults_to_unset() {
        // `None` = nothing explicit on the CLI/env — the TUI then resolves
        // `[settings] prefix` and the tmux-aware default `Ctrl-A`
        // (`input::effective_prefix`), so unset must survive parsing as-is.
        let cli = Cli::try_parse_from(["caucus"]).unwrap();
        assert!(cli.prefix.is_none());
    }

    #[test]
    fn prefix_flag_accepts_bare_letter_and_normalises_case() {
        assert_eq!(
            Cli::try_parse_from(["caucus", "--prefix", "B"])
                .unwrap()
                .prefix
                .unwrap()
                .0,
            'b'
        );
    }

    #[test]
    fn prefix_flag_accepts_ctrl_forms() {
        for spec in ["ctrl-b", "ctrl+b", "C-b", "^b", " b "] {
            assert_eq!(
                Cli::try_parse_from(["caucus", "--prefix", spec])
                    .unwrap()
                    .prefix
                    .unwrap()
                    .0,
                'b',
                "spec `{spec}` should parse to 'b'"
            );
        }
    }

    #[test]
    fn prefix_flag_rejects_non_letter_and_multi_char() {
        for bad in ["", "1", "esc", "ab", "ctrl-1"] {
            assert!(
                Cli::try_parse_from(["caucus", "--prefix", bad]).is_err(),
                "spec `{bad}` must be rejected"
            );
        }
    }

    #[test]
    fn signal_post_parses() {
        let cli = Cli::try_parse_from([
            "caucus",
            "signal",
            "post",
            "--sock",
            "/tmp/c.sock",
            "--session",
            "S",
            "--panel",
            "P",
            "--kind",
            "stop",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Signal(SignalCommand::Post { .. }))
        ));
    }

    /// `signal post --discover` parses with no identity flags at all, an
    /// identity flag alongside `--discover` conflicts, and omitting both
    /// `--discover` and the identity flags is an error — the clap wiring the
    /// dispatch's `expect`s rely on.
    #[test]
    fn signal_post_discover_parses_and_excludes_identity_flags() {
        let cli = Cli::try_parse_from(["caucus", "signal", "post", "--discover", "--kind", "stop"])
            .unwrap();
        match cli.command {
            Some(Command::Signal(SignalCommand::Post {
                sock,
                session,
                panel,
                discover,
                ..
            })) => {
                assert!(discover);
                assert!(sock.is_none() && session.is_none() && panel.is_none());
            }
            other => panic!("expected signal post, got {other:?}"),
        }

        assert!(
            Cli::try_parse_from([
                "caucus",
                "signal",
                "post",
                "--discover",
                "--sock",
                "/tmp/c.sock",
            ])
            .is_err(),
            "--sock conflicts with --discover"
        );
        assert!(
            Cli::try_parse_from(["caucus", "signal", "post", "--kind", "stop"]).is_err(),
            "without --discover the identity flags are required"
        );
    }

    /// The lifecycle kinds parse (`--kind post-compact` / `session-start` are
    /// what the shared hook script passes as its argument), and every kind maps
    /// to exactly one of the two dispatch paths: a turn kind or a lifecycle
    /// hook, never both, never neither — the invariant behind `run_signal`'s
    /// `expect` on the non-lifecycle branch.
    #[test]
    fn signal_post_kind_maps_to_exactly_one_dispatch_path() {
        use crate::signal::post::LifecycleHook;
        for (arg, spec) in [
            (SignalKindArg::Stop, "stop"),
            (SignalKindArg::ToolBlocked, "tool-blocked"),
            (SignalKindArg::Error, "error"),
            (SignalKindArg::PostCompact, "post-compact"),
            (SignalKindArg::SessionStart, "session-start"),
        ] {
            let cli = Cli::try_parse_from([
                "caucus",
                "signal",
                "post",
                "--sock",
                "/s",
                "--session",
                "S",
                "--panel",
                "P",
                "--kind",
                spec,
            ])
            .unwrap();
            let Some(Command::Signal(SignalCommand::Post { kind, .. })) = cli.command else {
                panic!("--kind {spec} must parse as signal post");
            };
            assert_eq!(kind, arg, "--kind {spec}");
            assert!(
                kind.turn_kind().is_some() != kind.lifecycle_hook().is_some(),
                "--kind {spec} must map to exactly one dispatch path"
            );
        }
        assert_eq!(
            SignalKindArg::PostCompact.lifecycle_hook(),
            Some(LifecycleHook::PostCompact)
        );
        assert_eq!(
            SignalKindArg::SessionStart.lifecycle_hook(),
            Some(LifecycleHook::SessionStart)
        );
    }

    /// `signal note` parses with explicit flags, and the kind defaults to
    /// `progress`. (The env-var defaults are clap `env = "CAUCUS_*"` wiring;
    /// exercising them here would mutate process-global env in a parallel test
    /// run, so only the flag form is parsed.)
    #[test]
    fn signal_note_parses_with_default_kind() {
        let cli = Cli::try_parse_from([
            "caucus",
            "signal",
            "note",
            "--sock",
            "/tmp/c.sock",
            "--session",
            "S",
            "--panel",
            "P",
            "wrote the draft",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Signal(SignalCommand::Note { kind, body, .. })) => {
                assert!(matches!(kind, NoteKindArg::Progress));
                assert_eq!(body, "wrote the draft");
            }
            other => panic!("expected signal note, got {other:?}"),
        }
    }

    #[test]
    fn mcp_serve_parses_control_sock() {
        let cli = Cli::try_parse_from([
            "caucus",
            "mcp-serve",
            "--control-sock",
            "/tmp/caucus-ctl.sock",
        ])
        .unwrap();
        match cli.command {
            Some(Command::McpServe { control_sock }) => {
                assert_eq!(control_sock, PathBuf::from("/tmp/caucus-ctl.sock"));
            }
            other => panic!("expected McpServe, got {other:?}"),
        }
    }

    #[test]
    fn roles_flag_splits_on_comma() {
        let cli = Cli::try_parse_from(["caucus", "--roles", "architect,backend"]).unwrap();
        assert_eq!(cli.roles, vec!["architect", "backend"]);
    }

    #[test]
    fn topic_flag_sets_the_session_label() {
        // Omitted → None (the repo directory name is applied downstream).
        let cli = Cli::try_parse_from(["caucus"]).unwrap();
        assert_eq!(cli.topic, None);
        // `--topic` carries a free-form label, spaces and all.
        let cli = Cli::try_parse_from(["caucus", "--topic", "auth refactor"]).unwrap();
        assert_eq!(cli.topic.as_deref(), Some("auth refactor"));
    }

    #[test]
    fn agent_cli_flag_selects_the_main_worker_backend() {
        // Omitted → None (the claude default is applied downstream).
        let cli = Cli::try_parse_from(["caucus"]).unwrap();
        assert_eq!(cli.agent_cli, None);
        // `--agent-cli codex` selects codex.
        let cli = Cli::try_parse_from(["caucus", "--agent-cli", "codex"]).unwrap();
        assert_eq!(cli.agent_cli, Some(AgentCli::Codex));
        // `--agent-cli claude` is accepted explicitly.
        let cli = Cli::try_parse_from(["caucus", "--agent-cli", "claude"]).unwrap();
        assert_eq!(cli.agent_cli, Some(AgentCli::Claude));
        // An unknown backend is rejected.
        assert!(Cli::try_parse_from(["caucus", "--agent-cli", "gemini"]).is_err());
    }

    #[test]
    fn sessions_subcommand_parses_with_format() {
        let cli = Cli::try_parse_from(["caucus", "sessions", "--format", "json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Sessions {
                format: OutputFormat::Json
            })
        ));
        // Default format is text.
        let cli = Cli::try_parse_from(["caucus", "sessions"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Sessions {
                format: OutputFormat::Text
            })
        ));
    }

    #[test]
    fn resume_subcommand_parses_session_id() {
        let cli = Cli::try_parse_from(["caucus", "resume", "01ABCXYZ"]).unwrap();
        match cli.command {
            Some(Command::Resume { session_id }) => assert_eq!(session_id, "01ABCXYZ"),
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    #[test]
    fn gc_subcommand_parses_with_defaults_and_overrides() {
        // Bare `gc`: 7-day window, dry-run.
        let cli = Cli::try_parse_from(["caucus", "gc"]).unwrap();
        match cli.command {
            Some(Command::Gc { older_than, prune }) => {
                assert_eq!(older_than, chrono::Duration::days(7));
                assert!(!prune);
            }
            other => panic!("expected Gc, got {other:?}"),
        }
        // `--older-than 2w --prune` parses the window and the delete flag.
        let cli = Cli::try_parse_from(["caucus", "gc", "--older-than", "2w", "--prune"]).unwrap();
        match cli.command {
            Some(Command::Gc { older_than, prune }) => {
                assert_eq!(older_than, chrono::Duration::weeks(2));
                assert!(prune);
            }
            other => panic!("expected Gc, got {other:?}"),
        }
        // A bad retention spec is a usage error.
        assert!(Cli::try_parse_from(["caucus", "gc", "--older-than", "7x"]).is_err());
    }

    /// `caucus sessions` discovers a written `session.json`: build a record
    /// under a temp `.caucus/sessions/<id>/`, then confirm `discover` (the
    /// listing's data source) returns it.
    #[test]
    fn sessions_listing_finds_a_written_record() {
        use crate::render::LayoutMode;
        use crate::role::spec::AgentCli;
        use crate::session::record::{PanelRecord, SessionRecord};

        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path();
        let record = SessionRecord {
            id: SessionId::new(),
            topic: "resume me".into(),
            repo_path: repo.to_path_buf(),
            created_at: chrono::Utc::now(),
            layout_mode: LayoutMode::Tiled,
            panels: vec![PanelRecord {
                role: "main".into(),
                agent_cli: AgentCli::Claude,
                model: None,
                order_index: 0,
                is_main: true,
                worktree_branch: None,
                claude_session_id: Some("conv-1".into()),
            }],
            role_counts: std::collections::HashMap::new(),
        };
        let root = repo
            .join(".caucus")
            .join("sessions")
            .join(record.id.to_string());
        record.write(&root).unwrap();

        let found = crate::session::record::discover(repo);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, record.id);
        assert_eq!(found[0].topic, "resume me");
        assert_eq!(found[0].panels.len(), 1);

        // `humanize_age` produces a compact string for a fresh record.
        let age = humanize_age(chrono::Utc::now() - record.created_at);
        assert!(age.ends_with('s') || age.ends_with('m'), "age: {age}");
    }
}
