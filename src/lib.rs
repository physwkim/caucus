//! caucus — a terminal multiplexer for teams of AI coding agents.
//!
//! See `docs/design.md` for the full specification. Each module owns one
//! slice of the system, with a single owner for the resource it manages
//! (`docs/design.md` §9.1).

pub mod agent;
pub mod cli;
pub mod config;
pub mod doctor;
pub mod gc;
pub mod hook;
pub mod init;
pub mod input;
pub mod line_io;
pub mod mcp;
pub mod panel;
pub mod pty;
pub mod render;
pub mod role;
pub mod session;
pub mod signal;
pub mod term;
pub mod tui;
pub mod worktree;

/// Coordinates subprocess-heavy tests so they do not flake under CI load.
#[cfg(test)]
mod test_serial;

/// Compile-time package version, exposed for `caucus --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
