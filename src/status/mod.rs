//! Fallback settle detection when the Claude `Stop` hook is not installed.
//! Regex-based pane-screen hints + a low-rate poller, modelled on dmux's
//! `PaneWorker` (see `docs/dmux-analysis.md` §2).

pub mod pane_hint;
pub mod poller;

pub use pane_hint::classify;
pub use poller::{HintUpdate, spawn_poller};
