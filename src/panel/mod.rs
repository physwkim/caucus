//! Panel: one cell of the caucus screen — a PTY + a vte grid + a render area.
//! tmux/zellij's "pane". See `docs/design.md` §2, §3.

pub mod lifecycle;

pub use crate::session::id::PanelId;
pub use lifecycle::{IllegalTransition, Panel, PanelError, PanelState};
