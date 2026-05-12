//! caucus — collaboration swarm orchestrator over tmux + git worktree.
//!
//! See `docs/design.md` for the full specification. Each module here implements
//! one slice of the system and has a single owner for the resource it manages
//! (see `docs/design.md` §9.1).

pub mod agent;
pub mod cli;
pub mod config;
pub mod consensus;
pub mod doctor;
pub mod execute;
pub mod notify;
pub mod role;
pub mod round;
pub mod sentinel;
pub mod session;
pub mod status;
pub mod tmux;
pub mod worktree;

/// Compile-time package version, exposed for `caucus --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
