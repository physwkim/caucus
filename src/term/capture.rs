//! Turn-segmented output capture (`docs/design.md` §8.5).
//!
//! Panel PTY output scrolls fast; the main worker acts in discrete MCP calls,
//! not live. caucus captures every byte a panel emits, segmented by turn
//! boundary (`PromptDelivered` .. `TurnCompleted`), so `read_panel` can return
//! a whole turn's output without the main worker racing the screen.
//!
//! Memory ring + disk spill to
//! `.caucus/sessions/<id>/panels/<panel_id>.log`.

use std::io::Write;
use std::path::{Path, PathBuf};

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
    /// Append-only log file the oldest closed turns spill to. `None` until a
    /// caller wires up the panel's log path via [`OutputCapture::set_log_path`].
    log_path: Option<PathBuf>,
    /// Count of turns already written out to `log_path` — the spilled prefix
    /// of the panel's lifetime, kept so callers can tell how much history is
    /// on disk vs in memory.
    spilled_turns: usize,
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
            log_path: None,
            spilled_turns: 0,
        }
    }

    /// Point this capture at its append-only spill log
    /// (`.caucus/sessions/<id>/panels/<panel_id>.log`).
    ///
    /// Set by the panel/session owner once the panel id and session directory
    /// are known. Until set, over-limit turns are still evicted from memory but
    /// not persisted (see [`OutputCapture::end_turn`]).
    pub(crate) fn set_log_path(&mut self, path: impl Into<PathBuf>) {
        self.log_path = Some(path.into());
    }

    /// The configured spill-log path, if any.
    pub fn log_path(&self) -> Option<&Path> {
        self.log_path.as_deref()
    }

    /// Number of closed turns that have been spilled to disk and dropped from
    /// memory.
    pub fn spilled_turns(&self) -> usize {
        self.spilled_turns
    }

    /// Open a new turn (called when a `PromptDelivered` lane event is emitted).
    pub(crate) fn begin_turn(&mut self) {
        let index = self.next_index();
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
    ///
    /// When the in-memory closed-turn count exceeds `memory_turn_limit`, the
    /// oldest turns are appended to the panel's disk log and dropped from the
    /// in-memory ring. The on-disk log is the durable source of truth for
    /// scrolled-off history (design.md §8.5).
    pub(crate) fn end_turn(&mut self) {
        if let Some(mut turn) = self.open.take() {
            turn.completed_at = Some(Utc::now());
            self.turns.push(turn);
        }
        self.spill_over_limit();
    }

    /// Spill the oldest closed turns to disk while the in-memory count exceeds
    /// `memory_turn_limit`.
    ///
    /// Best-effort: an I/O error stops spilling for this call but leaves the
    /// turn in memory so no bytes are lost. A `read_panel` for that turn still
    /// succeeds from memory; the next `end_turn` retries the spill.
    fn spill_over_limit(&mut self) {
        while self.turns.len() > self.memory_turn_limit {
            // Borrow the oldest without removing it yet, so a write failure
            // does not drop unspilled bytes.
            let oldest = &self.turns[0];
            match self.log_path.as_deref() {
                Some(path) => {
                    if write_turn(path, oldest).is_err() {
                        // Leave it in memory; retry on the next end_turn.
                        break;
                    }
                }
                None => {
                    // No log configured: still evict to honour the memory
                    // bound, but history before the ring is then unrecoverable.
                }
            }
            self.turns.remove(0);
            self.spilled_turns += 1;
        }
    }

    /// Next turn index across spilled + in-memory + open turns.
    fn next_index(&self) -> usize {
        self.spilled_turns + self.turns.len() + usize::from(self.open.is_some())
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

    /// All closed turns currently held in memory, oldest first.
    ///
    /// Turns older than this window have been spilled to [`OutputCapture::log_path`];
    /// [`OutputCapture::spilled_turns`] reports how many.
    pub fn turns(&self) -> &[TurnSegment] {
        &self.turns
    }
}

/// Append one turn's raw bytes to the panel log, creating parent directories
/// as needed. The log is append-only: each call adds to the tail.
fn write_turn(path: &Path, turn: &TurnSegment) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(&turn.bytes)?;
    file.flush()?;
    Ok(())
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

    #[test]
    fn turn_indices_are_monotonic() {
        let mut cap = OutputCapture::new();
        for _ in 0..3 {
            cap.begin_turn();
            cap.end_turn();
        }
        let idx: Vec<_> = cap.turns().iter().map(|t| t.index).collect();
        assert_eq!(idx, vec![0, 1, 2]);
    }

    #[test]
    fn over_limit_turns_evict_from_memory_without_log() {
        let mut cap = OutputCapture::new();
        cap.memory_turn_limit = 2;
        for i in 0..5u8 {
            cap.begin_turn();
            cap.push(&[b'a' + i]);
            cap.end_turn();
        }
        // Only the limit is retained in memory.
        assert_eq!(cap.turns().len(), 2);
        assert_eq!(cap.spilled_turns(), 3);
        // Newest two turns retained.
        assert_eq!(cap.turns()[0].bytes, b"d");
        assert_eq!(cap.turns()[1].bytes, b"e");
        // Indices remain correct across the spill boundary.
        assert_eq!(cap.turns()[0].index, 3);
        assert_eq!(cap.turns()[1].index, 4);
    }

    #[test]
    fn over_limit_turns_spill_to_disk_log() {
        let dir = std::env::temp_dir().join(format!("caucus-capture-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = dir.join("panels").join("panel-1.log");

        let mut cap = OutputCapture::new();
        cap.memory_turn_limit = 2;
        cap.set_log_path(&log);
        assert_eq!(cap.log_path(), Some(log.as_path()));

        for i in 0..5u8 {
            cap.begin_turn();
            cap.push(&[b'0' + i]);
            cap.end_turn();
        }
        assert_eq!(cap.turns().len(), 2);
        assert_eq!(cap.spilled_turns(), 3);

        // Spilled turns 0,1,2 -> bytes "012" appended in order.
        let on_disk = std::fs::read(&log).unwrap();
        assert_eq!(on_disk, b"012");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spill_appends_across_calls() {
        let dir =
            std::env::temp_dir().join(format!("caucus-capture-append-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = dir.join("p.log");

        let mut cap = OutputCapture::new();
        cap.memory_turn_limit = 1;
        cap.set_log_path(&log);

        cap.begin_turn();
        cap.push(b"AA");
        cap.end_turn(); // turn 0 in memory
        cap.begin_turn();
        cap.push(b"BB");
        cap.end_turn(); // turn 0 spilled, turn 1 in memory
        assert_eq!(std::fs::read(&log).unwrap(), b"AA");

        cap.begin_turn();
        cap.push(b"CC");
        cap.end_turn(); // turn 1 spilled
        assert_eq!(std::fs::read(&log).unwrap(), b"AABB");
        assert_eq!(cap.spilled_turns(), 2);
        assert_eq!(cap.turns().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
