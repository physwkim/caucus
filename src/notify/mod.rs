//! Notify the CEO when agent state changes (SIGUSR2, or a stdout event line in
//! `caucus watch` mode).

pub mod signal;

pub use signal::{SignalError, Usr2Stream, graceful_shutdown};
