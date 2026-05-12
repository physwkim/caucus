//! Clap-driven CLI dispatch.

use std::process::ExitCode;

use clap::Parser;

pub mod ceo_brief;
pub mod commands;
pub mod dispatch;
pub mod exit;
pub mod hook_install;
pub mod output;

use commands::Cli;

/// Entry point invoked by `main`.
pub fn run() -> ExitCode {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("CAUCUS_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();

    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("caucus: failed to build tokio runtime: {err}");
            return exit::code(exit::ENVIRONMENT_ERROR);
        }
    };
    match runtime.block_on(dispatch::dispatch(cli)) {
        Ok(c) => exit::code(c),
        Err(err) => {
            eprintln!("caucus: {err:#}");
            exit::code(map_error_to_code(&err))
        }
    }
}

fn map_error_to_code(err: &anyhow::Error) -> u8 {
    let msg = err.to_string().to_lowercase();
    if msg.contains("unknown role")
        || msg.contains("invalid session id")
        || msg.contains("invalid agent id")
        || msg.contains("no round has been started")
        || msg.contains("not started yet")
    {
        return exit::USER_ERROR;
    }
    if msg.contains("tmux") || msg.contains("git") {
        return exit::ENVIRONMENT_ERROR;
    }
    if msg.contains("manifest") || msg.contains("session json") {
        return exit::STATE_ERROR;
    }
    exit::GENERIC_FAILURE
}
