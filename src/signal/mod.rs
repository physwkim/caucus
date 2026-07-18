//! Turn-completion signals (`docs/design.md` §7).
//!
//! When an agent finishes a turn, its Claude `Stop` hook posts a one-line JSON
//! [`TurnSignal`] to the caucus unix-domain socket. caucus reads it live —
//! no file sentinel, no polling.

pub mod post;
pub mod server;

use std::path::PathBuf;

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
    /// Lets the main worker judge most turns without scraping the terminal.
    pub last_message: Option<String>,
    /// Path of the agent's conversation transcript (JSONL), lifted from the
    /// hook payload's `transcript_path`. Lets the main worker read the whole
    /// conversation from disk instead of scraping the terminal. `None` for
    /// payloads that carry no such path (codex's notify JSON has no
    /// counterpart) — and for signal lines posted by an older `caucus` binary,
    /// hence the serde default.
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
    /// The raw Claude hook payload, retained verbatim for diagnostics.
    pub raw_hook_payload: serde_json::Value,
}

impl TurnSignal {
    /// Construct a signal stamped with `Utc::now()`. `transcript_path` is
    /// lifted from `raw_hook_payload` here, so a constructed signal can never
    /// disagree with its own payload about where the transcript lives.
    pub fn now(
        session_id: SessionId,
        panel_id: PanelId,
        kind: TurnKind,
        last_message: Option<String>,
        raw_hook_payload: serde_json::Value,
    ) -> Self {
        let transcript_path = extract_transcript_path(&raw_hook_payload);
        Self {
            session_id,
            panel_id,
            ts: Utc::now(),
            kind,
            last_message,
            transcript_path,
            raw_hook_payload,
        }
    }
}

/// Lift the conversation-transcript path out of a hook payload. Claude Code's
/// Stop hook documents the `transcript_path` field; codex's notify JSON has no
/// counterpart, so codex signals carry `None`.
fn extract_transcript_path(payload: &serde_json::Value) -> Option<PathBuf> {
    payload
        .get("transcript_path")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_lifts_transcript_path_from_payload() {
        let signal = TurnSignal::now(
            SessionId::new(),
            PanelId::new(),
            TurnKind::Stop,
            None,
            serde_json::json!({ "transcript_path": "/logs/conv.jsonl" }),
        );
        assert_eq!(
            signal.transcript_path.as_deref(),
            Some(std::path::Path::new("/logs/conv.jsonl"))
        );
    }

    #[test]
    fn now_without_transcript_path_is_none() {
        let signal = TurnSignal::now(
            SessionId::new(),
            PanelId::new(),
            TurnKind::Stop,
            None,
            serde_json::json!({ "last_message": "no path here" }),
        );
        assert_eq!(signal.transcript_path, None);
    }
}
