//! Clap-driven CLI dispatch. Filled in at commit 11.

use std::process::ExitCode;

/// Entry point invoked by `main`. Stub until the CLI is wired in commit 11.
pub fn run() -> ExitCode {
    eprintln!(
        "caucus {} — CLI not yet wired (see docs/design.md §10)",
        crate::VERSION
    );
    ExitCode::from(0)
}
