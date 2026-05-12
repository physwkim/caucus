//! Sentinel files written by Claude's `Stop` hook. The writer is the only path
//! to a sentinel file inside caucus; the watcher is read-only.
//!
//! See `docs/design.md` §7.

pub mod watcher;
pub mod writer;

pub use watcher::{SentinelWatcher, WatchEvent, WatcherError, watch};
pub use writer::{
    Sentinel, SentinelError, SentinelKind, read_sentinel, sentinel_path, write_sentinel,
};
