//! Turn-segmented output capture (`docs/design.md` §8.5).
//!
//! Panel PTY output scrolls fast; the main worker acts in discrete MCP calls,
//! not live. caucus captures every byte a panel emits, segmented by turn
//! boundary (`PromptDelivered` .. `TurnCompleted`), so `read_panel` can return
//! a whole turn's output without the main worker racing the screen.
//!
//! Memory ring + disk spill to
//! `.caucus/sessions/<id>/panels/<panel_id>.log`.

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

/// Memoized render of the most-recent turn, keyed so a cache hit is exact.
/// The key is `(turn index, byte length, cols)`: within one turn the byte
/// length only grows (or shrinks on a head trim), so it discriminates every
/// state of that turn; across turns the index differs, so two different turns
/// that happen to share a byte length never collide.
struct RenderCache {
    turn_index: Option<usize>,
    byte_len: usize,
    cols: usize,
    text: String,
}

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
    /// In-memory byte cap for a single still-open turn. Closed turns are bounded
    /// by count (`memory_turn_limit`, spilling to disk); the *open* turn has no
    /// such boundary until it closes, so a panel firehosing output mid-turn
    /// could otherwise grow it without limit. See [`OutputCapture::push`].
    open_turn_byte_limit: usize,
    /// Memoized render of the most-recent turn for
    /// [`OutputCapture::rendered_since_last_turn`]. `RefCell` so the read path
    /// stays `&self` (it shares the `read_panel` immutable-borrow signature)
    /// while still filling the cache on a miss.
    last_render: RefCell<Option<RenderCache>>,
}

impl OutputCapture {
    /// Default number of closed turns kept in memory.
    pub const DEFAULT_TURN_LIMIT: usize = 64;

    /// Default in-memory byte cap for a single open turn (4 MiB). Sized above
    /// what [`crate::session`] renders from a turn (`rendered_capture_text`
    /// replays into a grid keeping 50 rows + 10_000 scrollback rows), so the
    /// dropped head was already scrolled out of any `read_panel` result.
    pub const DEFAULT_OPEN_TURN_BYTES: usize = 4 << 20;

    /// Build an empty capture.
    pub fn new() -> Self {
        Self {
            turns: Vec::new(),
            open: None,
            memory_turn_limit: Self::DEFAULT_TURN_LIMIT,
            log_path: None,
            spilled_turns: 0,
            open_turn_byte_limit: Self::DEFAULT_OPEN_TURN_BYTES,
            last_render: RefCell::new(None),
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
        if self.open.is_some() {
            return;
        }
        let index = self.next_index();
        self.open = Some(TurnSegment {
            index,
            started_at: Utc::now(),
            completed_at: None,
            bytes: Vec::new(),
        });
    }

    /// Append PTY output bytes to the currently open turn.
    ///
    /// A single open turn is bounded by `open_turn_byte_limit`: the buffer is
    /// allowed to reach 2× the cap, then the oldest half is dropped back to the
    /// cap. Letting it overshoot before trimming amortizes the drain's memmove
    /// to O(1) per byte while holding memory to 2× the cap — a panel firehosing
    /// output before its turn closes can no longer grow it without bound. The
    /// retained tail is exactly what `read_panel(since_last_turn)` renders (the
    /// recent screen + scrollback); the dropped head was already scrolled past
    /// the grid replay's window. A turn that closes under the cap is untouched.
    ///
    /// Trade-off: the dropped head of an over-cap turn is also absent from the
    /// disk-log spill on close (the open turn is not streamed to disk). The cap
    /// is sized so this only affects turns far larger than any rendered view.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if let Some(turn) = self.open.as_mut() {
            turn.bytes.extend_from_slice(bytes);
            let cap = self.open_turn_byte_limit;
            if turn.bytes.len() > cap.saturating_mul(2) {
                let drop = turn.bytes.len() - cap;
                turn.bytes.drain(..drop);
            }
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
            // Spill to disk when a log is configured; a write failure stops
            // this call and leaves the turn in memory (no bytes lost — the
            // next end_turn retries). With no log configured the turn is still
            // evicted below to honour the memory bound, though history before
            // the ring is then unrecoverable.
            if let Some(path) = self.log_path.as_deref()
                && write_turn(path, oldest).is_err()
            {
                break;
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

    /// Render of the most-recent turn (the `since_last_turn` `read_panel` mode),
    /// memoized so repeated reads of the same turn do not re-replay the whole
    /// byte buffer through a throwaway grid. `render` does the actual replay
    /// (`session`'s `rendered_capture_text`); it runs only on a cache miss —
    /// the turn grew, its head was trimmed, the active turn changed, or `cols`
    /// changed (see [`RenderCache`]).
    pub fn rendered_since_last_turn(
        &self,
        cols: usize,
        render: impl FnOnce(&[u8], usize) -> String,
    ) -> String {
        let bytes = self.since_last_turn();
        let turn_index = self
            .open
            .as_ref()
            .map(|t| t.index)
            .or_else(|| self.turns.last().map(|t| t.index));

        if let Some(c) = self.last_render.borrow().as_ref()
            && c.turn_index == turn_index
            && c.byte_len == bytes.len()
            && c.cols == cols
        {
            return c.text.clone();
        }

        let text = render(bytes, cols);
        *self.last_render.borrow_mut() = Some(RenderCache {
            turn_index,
            byte_len: bytes.len(),
            cols,
            text: text.clone(),
        });
        text
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
    fn begin_turn_while_open_preserves_the_active_segment() {
        let mut cap = OutputCapture::new();
        cap.begin_turn();
        cap.push(b"before interactive reply");

        // A second submitted input can arrive while the panel is still in the
        // same coarse Working turn (for example answering an in-turn
        // selection prompt). It must not discard the bytes already captured
        // for that turn.
        cap.begin_turn();
        cap.push(b" after reply");
        cap.end_turn();

        assert_eq!(cap.turns().len(), 1);
        assert_eq!(
            cap.turns()[0].bytes,
            b"before interactive reply after reply"
        );
        assert_eq!(cap.turns()[0].index, 0);
    }

    #[test]
    fn empty_capture_has_no_turn_output() {
        assert!(OutputCapture::new().since_last_turn().is_empty());
    }

    #[test]
    fn open_turn_buffer_is_bounded() {
        // A turn that keeps emitting before it closes cannot grow the buffer
        // without limit: it is held to at most 2× the cap.
        let mut cap = OutputCapture::new();
        cap.open_turn_byte_limit = 100;
        cap.begin_turn();
        for _ in 0..50 {
            cap.push(&[b'x'; 20]); // 1000 bytes total, well past 2× the cap
        }
        let held = cap.since_last_turn().len();
        assert!(held <= 200, "open turn capped at 2× the limit, got {held}");
        assert!(held >= 100, "keeps at least the cap, got {held}");
        assert!(cap.since_last_turn().iter().all(|&b| b == b'x'));
    }

    #[test]
    fn open_turn_trim_keeps_the_tail_not_the_head() {
        // Trimming drops the OLDEST bytes — read_panel renders the recent tail.
        let mut cap = OutputCapture::new();
        cap.open_turn_byte_limit = 10;
        cap.begin_turn();
        cap.push(&[b'A'; 10]);
        cap.push(&[b'B'; 15]); // total 25 > 20 → trim to the last 10
        let tail = cap.since_last_turn();
        assert_eq!(tail.len(), 10);
        assert!(
            tail.iter().all(|&b| b == b'B'),
            "the recent tail is kept and the head dropped"
        );
    }

    #[test]
    fn since_last_turn_render_is_cached_until_content_changes() {
        let mut cap = OutputCapture::new();
        cap.begin_turn();
        cap.push(b"hello");

        let calls = std::cell::Cell::new(0u32);
        let render = |b: &[u8], _c: usize| -> String {
            calls.set(calls.get() + 1);
            String::from_utf8_lossy(b).into_owned()
        };

        assert_eq!(cap.rendered_since_last_turn(80, render), "hello");
        assert_eq!(cap.rendered_since_last_turn(80, render), "hello");
        assert_eq!(calls.get(), 1, "the second read is served from the cache");

        // Growth invalidates the cache.
        cap.push(b" world");
        assert_eq!(cap.rendered_since_last_turn(80, render), "hello world");
        assert_eq!(calls.get(), 2, "new bytes re-render");

        // A different width invalidates too.
        let _ = cap.rendered_since_last_turn(40, render);
        assert_eq!(calls.get(), 3, "a new cols re-renders");
    }

    #[test]
    fn cached_render_does_not_leak_across_turns_of_equal_length() {
        let mut cap = OutputCapture::new();
        let render = |b: &[u8], _c: usize| String::from_utf8_lossy(b).into_owned();

        cap.begin_turn();
        cap.push(b"AAAAA"); // 5 bytes
        assert_eq!(cap.rendered_since_last_turn(80, render), "AAAAA");
        cap.end_turn();

        // A new turn of the SAME byte length but different content must not
        // serve the previous turn's cached render — the turn index discriminates.
        cap.begin_turn();
        cap.push(b"BBBBB"); // also 5 bytes
        assert_eq!(cap.rendered_since_last_turn(80, render), "BBBBB");
    }

    #[test]
    fn small_open_turn_is_not_trimmed() {
        // A turn that stays under the cap keeps every byte through to close.
        let mut cap = OutputCapture::new();
        cap.open_turn_byte_limit = 1000;
        cap.begin_turn();
        cap.push(b"short output");
        cap.end_turn();
        assert_eq!(cap.turns()[0].bytes, b"short output");
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
