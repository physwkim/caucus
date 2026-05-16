//! Turn-completion signals (`docs/design.md` §7).
//!
//! When an agent finishes a turn, its Claude `Stop` hook posts a one-line JSON
//! [`TurnSignal`] to the caucus unix-domain socket. caucus reads it live —
//! no file sentinel, no polling.

pub mod post;
pub mod server;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::session::id::{PanelId, SessionId};

/// What kind of turn-completion event the signal carries (`docs/design.md`
/// §7.4). Serialised lowercase: `stop | tool_blocked | error`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnKind {
    /// Normal turn completion (Claude `Stop` hook).
    Stop,
    /// The agent stopped because a tool was blocked (e.g. permission prompt).
    ToolBlocked,
    /// The turn ended in an error.
    Error,
}

/// One turn-completion signal, posted by an agent's Stop hook to the caucus
/// socket. Schema mirrors `docs/design.md` §7.4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnSignal {
    pub session_id: SessionId,
    pub panel_id: PanelId,
    pub ts: DateTime<Utc>,
    pub kind: TurnKind,
    /// The agent's final assistant message, lifted from the hook payload.
    /// Lets the CEO judge most turns without scraping the terminal.
    pub last_message: Option<String>,
    /// The raw Claude hook payload, retained verbatim for diagnostics.
    pub raw_hook_payload: serde_json::Value,
}

impl TurnSignal {
    /// Construct a signal stamped with `Utc::now()`.
    pub fn now(
        session_id: SessionId,
        panel_id: PanelId,
        kind: TurnKind,
        last_message: Option<String>,
        raw_hook_payload: serde_json::Value,
    ) -> Self {
        Self {
            session_id,
            panel_id,
            ts: Utc::now(),
            kind,
            last_message,
            raw_hook_payload,
        }
    }
}
