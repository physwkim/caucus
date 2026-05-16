//! Turn-segmented output capture (`docs/design.md` §8.5).
//!
//! Panel PTY output scrolls fast; the CEO acts in discrete MCP calls, not
//! live. caucus captures every byte a panel emits, segmented by turn boundary
//! (`PromptDelivered` .. `TurnCompleted`), so `read_panel` can return a whole
//! turn's output without the CEO racing the screen.
//!
//! Memory ring + disk spill to
//! `.caucus/sessions/<id>/panels/<panel_id>.log`.

use chrono::{DateTime, Utc};

/// One captured turn: all output bytes between a `PromptDelivered` and the
/// matching `TurnCompleted`.
#[derive(Debug, Clone)]
pub struct TurnSegment {
    /// 0-based turn index within the panel's lifetime.
    pub index: usize,
    /// When the prompt that opened this turn was delivered.
    pub started_at: DateTime<Utc>,
    /// When the turn signal closed this turn. `None` while still open.
    pub completed_at: Option<DateTime<Utc>>,
    /// Raw PTY output bytes for this turn.
    pub bytes: Vec<u8>,
}

/// Append-only, turn-segmented capture of one panel's output.
pub struct OutputCapture {
    /// Closed turns, oldest first. Bounded; oldest spill to disk.
    turns: Vec<TurnSegment>,
    /// The currently open turn, if a prompt has been delivered and not yet
    /// completed.
    open: Option<TurnSegment>,
    /// Max in-memory closed turns before older ones spill to disk.
    memory_turn_limit: usize,
}

impl OutputCapture {
    /// Default number of closed turns kept in memory.
    pub const DEFAULT_TURN_LIMIT: usize = 64;

    /// Build an empty capture.
    pub fn new() -> Self {
        Self {
            turns: Vec::new(),
            open: None,
            memory_turn_limit: Self::DEFAULT_TURN_LIMIT,
        }
    }

    /// Open a new turn (called when a `PromptDelivered` lane event is emitted).
    pub(crate) fn begin_turn(&mut self) {
        let index = self.turns.len() + usize::from(self.open.is_some());
        self.open = Some(TurnSegment {
            index,
            started_at: Utc::now(),
            completed_at: None,
            bytes: Vec::new(),
        });
    }

    /// Append PTY output bytes to the currently open turn.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if let Some(turn) = self.open.as_mut() {
            turn.bytes.extend_from_slice(bytes);
        }
    }

    /// Close the open turn (called on `TurnCompleted`).
    pub(crate) fn end_turn(&mut self) {
        if let Some(mut turn) = self.open.take() {
            turn.completed_at = Some(Utc::now());
            self.turns.push(turn);
            // TODO(phase 2): spill the oldest turn to disk when over limit.
            let _ = self.memory_turn_limit;
        }
    }

    /// Output of the most recent turn (open or closed) — backs the
    /// `since_last_turn` `read_panel` mode.
    pub fn since_last_turn(&self) -> &[u8] {
        if let Some(turn) = &self.open {
            &turn.bytes
        } else if let Some(turn) = self.turns.last() {
            &turn.bytes
        } else {
            &[]
        }
    }

    /// All closed turns, oldest first.
    pub fn turns(&self) -> &[TurnSegment] {
        &self.turns
    }
}

impl Default for OutputCapture {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_lifecycle_captures_output() {
        let mut cap = OutputCapture::new();
        cap.begin_turn();
        cap.push(b"working...");
        assert_eq!(cap.since_last_turn(), b"working...");
        cap.end_turn();
        assert_eq!(cap.turns().len(), 1);
        assert_eq!(cap.since_last_turn(), b"working...");
    }

    #[test]
    fn empty_capture_has_no_turn_output() {
        assert!(OutputCapture::new().since_last_turn().is_empty());
    }
}
