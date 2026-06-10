//! vte-backed screen grid (`docs/design.md` §0 #3, §8.5).
//!
//! A panel's terminal screen: a cell matrix viewport plus a bounded scrollback
//! ring. Bytes flow in from the panel PTY; the grid is the [`vte::Perform`]
//! sink that interprets escape sequences into cell state.
//!
//! **Invariant** (`docs/design.md` §9.1): the grid is mutated only by PTY
//! bytes fed through `Grid::advance`. No module pokes cells directly.
//!
//! Implemented against caucus's own [`Cell`] / [`Grid`] types, using
//! `zellij-server/src/panes/grid.rs` as a semantic reference (design.md §0 #3).
//! Scope of this implementation and the deliberate partials are documented at
//! [`Perform for Grid`].

use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

/// SGR attribute bitflags packed into [`Cell::attrs`].
pub mod attr {
    /// Bold / increased intensity (SGR 1).
    pub const BOLD: u8 = 1 << 0;
    /// Dim / decreased intensity (SGR 2).
    pub const DIM: u8 = 1 << 1;
    /// Italic (SGR 3).
    pub const ITALIC: u8 = 1 << 2;
    /// Underline (SGR 4).
    pub const UNDERLINE: u8 = 1 << 3;
    /// Reverse video (SGR 7).
    pub const REVERSE: u8 = 1 << 4;
    /// Concealed / hidden (SGR 8).
    pub const HIDDEN: u8 = 1 << 5;
    /// Crossed-out / strikethrough (SGR 9).
    pub const STRIKE: u8 = 1 << 6;
}

/// One terminal cell: a glyph plus its rendition attributes.
///
/// `Copy` is derived (additive to the skeleton) so row-shift operations can
/// use `slice::copy_within`; all fields are themselves `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// The displayed character. Space for an empty cell.
    ///
    /// `'\0'` marks the *trailing half* of a wide (East-Asian) glyph: the
    /// preceding cell holds the real character and occupies two columns.
    pub ch: char,
    /// Packed SGR foreground colour index (0 = default).
    ///
    /// Encoding: `0` default; `1..=8` the standard ANSI colours (SGR 30-37);
    /// `9..=16` the bright variants (SGR 90-97); `17..=255` a direct 256-colour
    /// palette index. The extended paths funnel through one encoding:
    /// `38;5;n` and true-colour (`38;2;r;g;b`, approximated to the nearest
    /// palette slot via `rgb_to_256`) are folded by `xterm_to_field` so a raw
    /// xterm index in `0..=16` maps onto the same `1..=16` ANSI slots the named
    /// path uses — otherwise the two meanings collide and dark text renders
    /// white (see `xterm_to_field`).
    pub fg: u8,
    /// Packed SGR background colour index (0 = default). Same encoding as
    /// [`Cell::fg`].
    pub bg: u8,
    /// Bold / underline / reverse etc., packed as a bitflag byte — see
    /// [`attr`].
    pub attrs: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: 0,
            bg: 0,
            attrs: 0,
        }
    }
}

impl Cell {
    /// A blank cell carrying the given rendition (used when erasing so that a
    /// set background colour is preserved, matching xterm behaviour).
    fn blank_with(pen: &Pen) -> Self {
        Self {
            ch: ' ',
            fg: pen.fg,
            bg: pen.bg,
            attrs: pen.attrs,
        }
    }
}

/// Current graphic rendition ("pen") applied to freshly printed cells.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Pen {
    fg: u8,
    bg: u8,
    attrs: u8,
}

/// A panel's parsed screen state: viewport cell matrix + bounded scrollback.
pub struct Grid {
    cols: usize,
    rows: usize,
    /// Visible viewport, row-major, `rows * cols` cells.
    viewport: Vec<Cell>,
    /// Bounded scrollback ring: rows that have scrolled off the top. The
    /// front is the oldest row; capacity is `scrollback_limit`.
    scrollback: std::collections::VecDeque<Vec<Cell>>,
    /// Maximum scrollback rows retained.
    scrollback_limit: usize,
    /// Cursor position, `(row, col)` within the viewport.
    cursor: (usize, usize),
    /// vte escape-sequence parser. Drives [`Perform`] callbacks on this grid.
    parser: Parser,
    /// Current graphic rendition for printed glyphs.
    pen: Pen,
    /// Scroll region, inclusive `(top, bottom)` row indices (DECSTBM). Defaults
    /// to the whole viewport.
    scroll_top: usize,
    scroll_bottom: usize,
    /// Deferred-wrap flag: set after printing into the last column so the next
    /// glyph wraps. Matches the xterm "pending wrap" state and avoids eagerly
    /// scrolling on a glyph that lands exactly on the right margin.
    wrap_pending: bool,
    /// Window title set via OSC 0/2 (stored, not rendered here).
    title: Option<String>,
    /// Last hyperlink URI set via OSC 8 (stored, not rendered here).
    hyperlink: Option<String>,
    /// Saved cursor state for `ESC 7` / `ESC 8` (DECSC/DECRC) and the
    /// `CSI s` / `CSI u` SCO variant. `None` until the first save.
    saved_cursor: Option<SavedCursor>,
    /// Primary-screen snapshot taken when the panel switched to the alternate
    /// screen (`CSI ?1049h` / `?1047h` / `?47h`). `Some` exactly while the alt
    /// screen is active; restored verbatim on the matching reset.
    alt_saved: Option<AltScreen>,
    /// Whether the agent has enabled bracketed-paste mode (`CSI ?2004h`). This
    /// does not affect the scraped cell grid — it is a queryable hint for the
    /// *input* path: `send_keys` frames a multi-byte prompt as a real
    /// bracketed paste so the agent does not absorb the submitting `\r` as a
    /// literal newline (`session::runtime::mcp::plan_delivery`).
    bracketed_paste: bool,
    /// Monotonic content-change counter, bumped on every `advance` that ingests
    /// bytes and on every `resize`. A consumer that derives something expensive
    /// from the grid text (the menu scan,
    /// `session::runtime::rounds::poll_round_selection_prompts`) caches its
    /// result against this value and recomputes only when it changes — so an
    /// idle panel is never re-scanned. Wraps; only equality across one tick
    /// matters, never the absolute value.
    generation: u64,
}

/// Cursor + pen state preserved by DECSC (`ESC 7`) / SCO save (`CSI s`).
#[derive(Debug, Clone)]
struct SavedCursor {
    cursor: (usize, usize),
    pen: Pen,
    wrap_pending: bool,
}

/// Primary-screen state preserved across an alternate-screen switch.
///
/// The alternate screen is a *separate*, always-cleared buffer (xterm `?1049`).
/// Entering it stashes the primary buffer here; the matching reset pops it
/// back so the panel that was scraping (e.g. a shell with its banner) is
/// restored exactly — no primary content bleeds through the alt screen, and no
/// alt content survives the switch back.
struct AltScreen {
    viewport: Vec<Cell>,
    cursor: (usize, usize),
    pen: Pen,
    scroll_top: usize,
    scroll_bottom: usize,
    wrap_pending: bool,
    /// Cursor saved at switch time (`?1049` saves the cursor; `?47`/`?1047`
    /// do not — but stashing it unconditionally is harmless and lets a single
    /// restore path serve all three).
    saved_cursor: Option<SavedCursor>,
}

impl Grid {
    /// Default scrollback depth (rows).
    pub const DEFAULT_SCROLLBACK: usize = 10_000;

    /// Hard upper bound on viewport dimensions. The grid allocates
    /// `cols * rows` cells, so an unbounded size would let a garbage-large
    /// `TIOCGWINSZ` report — a display glitch or a wake-time size query
    /// returning nonsense — make `resize` fill tens of gigabytes of blank
    /// cells and OOM-kill caucus. A real terminal is far smaller (a 4K display
    /// at a tiny font is ~1000 cols), so this only ever clamps nonsense, never
    /// a legitimate size. Enforced in both `new` and `resize`, the two
    /// cell-allocating entry points, so the bound holds by construction for
    /// every caller.
    pub const MAX_COLS: usize = 2000;
    pub const MAX_ROWS: usize = 1000;

    /// Build a blank grid sized `cols x rows`.
    pub fn new(cols: usize, rows: usize) -> Self {
        let rows = rows.clamp(1, Self::MAX_ROWS);
        let cols = cols.clamp(1, Self::MAX_COLS);
        Self {
            cols,
            rows,
            viewport: vec![Cell::default(); cols * rows],
            scrollback: std::collections::VecDeque::new(),
            scrollback_limit: Self::DEFAULT_SCROLLBACK,
            cursor: (0, 0),
            parser: Parser::new(),
            pen: Pen::default(),
            scroll_top: 0,
            scroll_bottom: rows - 1,
            wrap_pending: false,
            title: None,
            hyperlink: None,
            saved_cursor: None,
            alt_saved: None,
            bracketed_paste: false,
            generation: 0,
        }
    }

    /// The grid's content-change counter (see the `generation` field). Two reads
    /// that compare equal mean the grid did not ingest any bytes or resize in
    /// between, so any text derived from it is unchanged.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the panel is currently on the alternate screen (`?1049h` and
    /// friends). Diagnostic / test helper.
    pub fn on_alt_screen(&self) -> bool {
        self.alt_saved.is_some()
    }

    /// Whether the agent has enabled bracketed-paste mode (`CSI ?2004h`). The
    /// input path consults this to frame a programmatic prompt as a real
    /// bracketed paste; see `session::runtime::mcp::plan_delivery`.
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// Viewport dimensions, `(cols, rows)`.
    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    /// Cursor position, `(row, col)`.
    pub fn cursor(&self) -> (usize, usize) {
        self.cursor
    }

    /// Read a viewport cell. `None` if out of bounds.
    pub fn cell(&self, row: usize, col: usize) -> Option<&Cell> {
        if row < self.rows && col < self.cols {
            self.viewport.get(row * self.cols + col)
        } else {
            None
        }
    }

    /// Visible viewport, row-major.
    pub fn viewport(&self) -> &[Cell] {
        &self.viewport
    }

    /// Scrollback rows, oldest first.
    pub fn scrollback(&self) -> impl Iterator<Item = &Vec<Cell>> {
        self.scrollback.iter()
    }

    /// Current window title (OSC 0/2), if the panel set one.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Most recent hyperlink URI (OSC 8), if set.
    pub fn hyperlink(&self) -> Option<&str> {
        self.hyperlink.as_deref()
    }

    /// Collect a viewport row's characters into a `String` (test/diagnostic
    /// helper). Trailing-half wide-glyph cells are skipped.
    pub fn row_text(&self, row: usize) -> String {
        if row >= self.rows {
            return String::new();
        }
        let start = row * self.cols;
        self.viewport[start..start + self.cols]
            .iter()
            .filter(|c| c.ch != '\0')
            .map(|c| c.ch)
            .collect()
    }

    /// Feed a chunk of PTY output bytes through the vte parser.
    ///
    /// The only sanctioned way to mutate grid state.
    pub(crate) fn advance(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // Bytes arrived → the grid may have changed; invalidate generation-keyed
        // caches (the menu scan). Conservative: an escape sequence that renders
        // to a no-op still bumps, which only costs one redundant re-scan.
        self.generation = self.generation.wrapping_add(1);
        // `Parser` and the `Perform` sink can't both be `&mut self` at once,
        // so swap the parser out, drive it, swap it back.
        let mut parser = std::mem::take(&mut self.parser);
        parser.advance(self, bytes);
        self.parser = parser;
    }

    /// Resize the viewport.
    ///
    /// **Reflow policy** (deliberately simple — design.md §0 #3 leaves full
    /// wrapped-line reflow to a later pass): the top-left content anchor is
    /// preserved. Each existing viewport row is copied into the new matrix,
    /// truncated when narrower and space-padded when wider. When the row count
    /// shrinks, the rows scrolled off the *top* are pushed into scrollback so
    /// no output is silently lost; when it grows, blank rows are appended at
    /// the bottom. Hard-wrapped logical lines are **not** re-joined or
    /// re-split — a line that wrapped at the old width keeps its old break.
    pub(crate) fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.clamp(1, Self::MAX_COLS);
        let rows = rows.clamp(1, Self::MAX_ROWS);
        if cols == self.cols && rows == self.rows {
            return;
        }
        // A real resize reflows the rows, so any generation-keyed cache (the
        // menu scan) must recompute against the new layout.
        self.generation = self.generation.wrapping_add(1);

        // Existing rows as owned vectors.
        let old_rows: Vec<Vec<Cell>> = (0..self.rows)
            .map(|r| self.viewport[r * self.cols..(r + 1) * self.cols].to_vec())
            .collect();

        // If shrinking vertically, oldest rows spill into scrollback.
        let overflow = old_rows.len().saturating_sub(rows);
        for row in old_rows.iter().take(overflow) {
            let mut row = resize_row(row, cols);
            row.truncate(cols);
            self.push_scrollback(row);
        }

        let mut new_viewport = Vec::with_capacity(cols * rows);
        for row in old_rows.iter().skip(overflow) {
            new_viewport.extend(resize_row(row, cols));
        }
        // Pad with blank rows if the viewport grew taller.
        while new_viewport.len() < cols * rows {
            new_viewport.push(Cell::default());
        }

        // Reflow the stashed primary buffer too when resizing on the alt
        // screen, so leaving the alt screen restores a correctly-sized
        // viewport rather than a truncate/pad approximation.
        if let Some(alt) = self.alt_saved.as_mut() {
            let alt_old: Vec<Vec<Cell>> = (0..self.rows)
                .map(|r| alt.viewport[r * self.cols..(r + 1) * self.cols].to_vec())
                .collect();
            let alt_overflow = alt_old.len().saturating_sub(rows);
            let mut alt_new = Vec::with_capacity(cols * rows);
            for row in alt_old.iter().skip(alt_overflow) {
                alt_new.extend(resize_row(row, cols));
            }
            while alt_new.len() < cols * rows {
                alt_new.push(Cell::default());
            }
            alt.viewport = alt_new;
            alt.cursor = (
                alt.cursor.0.saturating_sub(alt_overflow).min(rows - 1),
                alt.cursor.1.min(cols - 1),
            );
            alt.scroll_top = 0;
            alt.scroll_bottom = rows - 1;
            alt.wrap_pending = false;
        }

        self.cols = cols;
        self.rows = rows;
        self.viewport = new_viewport;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.wrap_pending = false;
        // Clamp the cursor: it moves up by the same overflow it was pushed by.
        let cur_row = self.cursor.0.saturating_sub(overflow).min(rows - 1);
        let cur_col = self.cursor.1.min(cols - 1);
        self.cursor = (cur_row, cur_col);
    }

    // ----- internal cell-state machine -------------------------------------

    /// Index into `viewport` for `(row, col)`.
    #[inline]
    fn idx(&self, row: usize, col: usize) -> usize {
        row * self.cols + col
    }

    /// Push a row off the top into the bounded scrollback ring.
    fn push_scrollback(&mut self, row: Vec<Cell>) {
        if self.scrollback_limit == 0 {
            return;
        }
        if self.scrollback.len() == self.scrollback_limit {
            self.scrollback.pop_front();
        }
        self.scrollback.push_back(row);
    }

    /// Scroll the scroll-region up by `n` lines. The lines leaving the top of
    /// the region enter scrollback **only** when the region starts at row 0
    /// (a full-screen scroll); a partial region scroll discards them, matching
    /// xterm.
    ///
    /// The surviving rows shift up in a single `copy_within` (one memmove for
    /// the whole region, not `n` × per-row copies), and the leaving row is
    /// cloned **only** when it will actually be retained — a partial-region
    /// scroll (`top != 0`) or a disabled scrollback (`scrollback_limit == 0`)
    /// allocates nothing.
    fn scroll_up(&mut self, n: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        if top >= bottom {
            // Degenerate region: just blank the single line.
            self.clear_row(top);
            return;
        }
        let n = n.min(bottom - top + 1);
        if n == 0 {
            return;
        }
        // Capture the rows leaving the top into scrollback, oldest first — but
        // only when they will be kept (full-screen scroll, scrollback enabled).
        if top == 0 && self.scrollback_limit > 0 {
            for r in 0..n {
                let lo = self.idx(r, 0);
                let leaving = self.viewport[lo..lo + self.cols].to_vec();
                self.push_scrollback(leaving);
            }
        }
        // Shift the survivors up by `n` in one move, then blank the rows opened
        // at the bottom margin.
        let src = self.idx(top + n, 0);
        let end = self.idx(bottom + 1, 0);
        let dst = self.idx(top, 0);
        self.viewport.copy_within(src..end, dst);
        for r in (bottom + 1 - n)..(bottom + 1) {
            self.clear_row(r);
        }
    }

    /// Scroll the scroll-region down by `n` lines (RI at the top margin / SD).
    /// Rows pushed past the bottom margin are discarded; scroll-down never
    /// feeds scrollback. The shift is a single `copy_within`.
    fn scroll_down(&mut self, n: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        if top >= bottom {
            self.clear_row(top);
            return;
        }
        let n = n.min(bottom - top + 1);
        if n == 0 {
            return;
        }
        let src = self.idx(top, 0);
        let end = self.idx(bottom + 1 - n, 0);
        let dst = self.idx(top + n, 0);
        self.viewport.copy_within(src..end, dst);
        for r in top..(top + n) {
            self.clear_row(r);
        }
    }

    /// Blank an entire viewport row with the current pen's background.
    fn clear_row(&mut self, row: usize) {
        if row >= self.rows {
            return;
        }
        let blank = Cell::blank_with(&self.pen);
        let start = self.idx(row, 0);
        for c in &mut self.viewport[start..start + self.cols] {
            *c = blank;
        }
    }

    /// Advance the cursor one line down, scrolling the region when it would
    /// pass the bottom margin.
    fn line_feed(&mut self) {
        if self.cursor.0 == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor.0 + 1 < self.rows {
            self.cursor.0 += 1;
        }
        self.wrap_pending = false;
    }

    /// Write one glyph at the cursor, honouring a pending wrap and advancing.
    fn put_glyph(&mut self, c: char) {
        let width = c.width().unwrap_or(0);
        if width == 0 {
            // Combining / zero-width: drop it. (Full combining-mark merge into
            // the previous cell is left to a later pass — documented partial.)
            return;
        }

        // Resolve a deferred wrap from the previous glyph.
        if self.wrap_pending {
            self.cursor.1 = 0;
            self.line_feed();
        }

        // A wide glyph that cannot fit before the right margin wraps first.
        if width == 2 && self.cursor.1 + 1 >= self.cols {
            // Pad the last column so nothing splits across the wrap.
            let idx = self.idx(self.cursor.0, self.cursor.1);
            self.viewport[idx] = Cell::blank_with(&self.pen);
            self.cursor.1 = 0;
            self.line_feed();
        }

        let (row, col) = self.cursor;
        for clear_col in col..(col + width).min(self.cols) {
            self.clear_cell_for_write(row, clear_col);
        }
        let idx = self.idx(row, col);
        self.viewport[idx] = Cell {
            ch: c,
            fg: self.pen.fg,
            bg: self.pen.bg,
            attrs: self.pen.attrs,
        };
        if width == 2 && col + 1 < self.cols {
            // Trailing half marker.
            let trail = self.idx(row, col + 1);
            self.viewport[trail] = Cell {
                ch: '\0',
                fg: self.pen.fg,
                bg: self.pen.bg,
                attrs: self.pen.attrs,
            };
        }

        let advance = width.max(1);
        if col + advance >= self.cols {
            // Land on the right margin: defer the wrap until the next glyph.
            self.cursor.1 = self.cols - 1;
            self.wrap_pending = true;
        } else {
            self.cursor.1 = col + advance;
        }
    }

    /// Clear the cell at `(row, col)` before overwriting it, also clearing any
    /// adjacent half of a wide glyph that shares the cell. This keeps the
    /// invariant that a trailing `'\0'` marker never survives without its lead
    /// glyph (and a lead glyph never survives after its trailing half is
    /// overwritten), so rendering cannot shift later cells left by skipping an
    /// orphan marker.
    fn clear_cell_for_write(&mut self, row: usize, col: usize) {
        let blank = Cell::blank_with(&self.pen);
        let idx = self.idx(row, col);
        if self.viewport[idx].ch == '\0' && col > 0 {
            let lead = self.idx(row, col - 1);
            self.viewport[lead] = blank;
        }
        if col + 1 < self.cols {
            let trail = self.idx(row, col + 1);
            if self.viewport[trail].ch == '\0' {
                self.viewport[trail] = blank;
            }
        }
        self.viewport[idx] = blank;
    }

    /// Clamp `(row, col)` into the viewport and store as the cursor.
    fn move_to(&mut self, row: usize, col: usize) {
        self.cursor = (row.min(self.rows - 1), col.min(self.cols - 1));
        self.wrap_pending = false;
    }

    /// Apply one SGR parameter list to the pen.
    fn apply_sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.pen = Pen::default();
            return;
        }
        // Flatten into a single index stream so multi-arg colours
        // (`38;5;n`, `38;2;r;g;b`) can be consumed across iterator items.
        let flat: Vec<u16> = params.iter().flat_map(|p| p.iter().copied()).collect();
        let mut i = 0;
        while i < flat.len() {
            let p = flat[i];
            match p {
                0 => self.pen = Pen::default(),
                1 => self.pen.attrs |= attr::BOLD,
                2 => self.pen.attrs |= attr::DIM,
                3 => self.pen.attrs |= attr::ITALIC,
                4 => self.pen.attrs |= attr::UNDERLINE,
                7 => self.pen.attrs |= attr::REVERSE,
                8 => self.pen.attrs |= attr::HIDDEN,
                9 => self.pen.attrs |= attr::STRIKE,
                21 | 22 => self.pen.attrs &= !(attr::BOLD | attr::DIM),
                23 => self.pen.attrs &= !attr::ITALIC,
                24 => self.pen.attrs &= !attr::UNDERLINE,
                27 => self.pen.attrs &= !attr::REVERSE,
                28 => self.pen.attrs &= !attr::HIDDEN,
                29 => self.pen.attrs &= !attr::STRIKE,
                30..=37 => self.pen.fg = (p - 30 + 1) as u8,
                39 => self.pen.fg = 0,
                40..=47 => self.pen.bg = (p - 40 + 1) as u8,
                49 => self.pen.bg = 0,
                90..=97 => self.pen.fg = (p - 90 + 9) as u8,
                100..=107 => self.pen.bg = (p - 100 + 9) as u8,
                38 | 48 => {
                    let is_fg = p == 38;
                    // `38;5;n` (256-colour) or `38;2;r;g;b` (true-colour).
                    if let Some(&mode) = flat.get(i + 1) {
                        match mode {
                            5 => {
                                let n = flat.get(i + 2).copied().unwrap_or(0);
                                let v = xterm_to_field((n.min(255)) as u8);
                                if is_fg {
                                    self.pen.fg = v;
                                } else {
                                    self.pen.bg = v;
                                }
                                i += 2;
                            }
                            2 => {
                                let r = flat.get(i + 2).copied().unwrap_or(0);
                                let g = flat.get(i + 3).copied().unwrap_or(0);
                                let b = flat.get(i + 4).copied().unwrap_or(0);
                                let v = xterm_to_field(rgb_to_256(r as u8, g as u8, b as u8));
                                if is_fg {
                                    self.pen.fg = v;
                                } else {
                                    self.pen.bg = v;
                                }
                                i += 4;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// Erase in display (ED). `mode`: 0 cursor..end, 1 start..cursor, 2 all,
    /// 3 all + scrollback.
    fn erase_display(&mut self, mode: u16) {
        let blank = Cell::blank_with(&self.pen);
        let (cr, cc) = self.cursor;
        match mode {
            0 => {
                // Cursor to end of current row, then all rows below.
                let start = self.idx(cr, cc);
                let row_end = self.idx(cr, 0) + self.cols;
                for c in &mut self.viewport[start..row_end] {
                    *c = blank;
                }
                let below = self.idx(cr + 1, 0).min(self.viewport.len());
                for c in &mut self.viewport[below..] {
                    *c = blank;
                }
            }
            1 => {
                // Start of screen through cursor (inclusive).
                let above = self.idx(cr, 0);
                for c in &mut self.viewport[..above] {
                    *c = blank;
                }
                let end = self.idx(cr, cc) + 1;
                for c in &mut self.viewport[above..end] {
                    *c = blank;
                }
            }
            2 | 3 => {
                for c in &mut self.viewport {
                    *c = blank;
                }
                if mode == 3 {
                    self.scrollback.clear();
                }
            }
            _ => {}
        }
        self.wrap_pending = false;
    }

    /// Erase in line (EL). `mode`: 0 cursor..end, 1 start..cursor, 2 whole row.
    fn erase_line(&mut self, mode: u16) {
        let blank = Cell::blank_with(&self.pen);
        let (cr, cc) = self.cursor;
        let row_start = self.idx(cr, 0);
        let row_end = row_start + self.cols;
        let range = match mode {
            0 => self.idx(cr, cc)..row_end,
            1 => row_start..self.idx(cr, cc) + 1,
            2 => row_start..row_end,
            _ => return,
        };
        for c in &mut self.viewport[range] {
            *c = blank;
        }
        self.wrap_pending = false;
    }

    /// Insert `n` blank lines at the cursor row (IL), pushing lower lines down
    /// within the scroll region.
    fn insert_lines(&mut self, n: usize) {
        let cr = self.cursor.0;
        if cr < self.scroll_top || cr > self.scroll_bottom {
            return;
        }
        let n = n.min(self.scroll_bottom - cr + 1);
        for _ in 0..n {
            for r in (cr + 1..=self.scroll_bottom).rev() {
                let dst = self.idx(r, 0);
                let src = self.idx(r - 1, 0);
                self.viewport.copy_within(src..src + self.cols, dst);
            }
            self.clear_row(cr);
        }
        self.wrap_pending = false;
    }

    /// Delete `n` lines at the cursor row (DL), pulling lower lines up within
    /// the scroll region.
    fn delete_lines(&mut self, n: usize) {
        let cr = self.cursor.0;
        if cr < self.scroll_top || cr > self.scroll_bottom {
            return;
        }
        let n = n.min(self.scroll_bottom - cr + 1);
        for _ in 0..n {
            for r in cr..self.scroll_bottom {
                let dst = self.idx(r, 0);
                let src = self.idx(r + 1, 0);
                self.viewport.copy_within(src..src + self.cols, dst);
            }
            self.clear_row(self.scroll_bottom);
        }
        self.wrap_pending = false;
    }

    /// Insert `n` blank cells at the cursor (ICH), shifting the rest of the
    /// row right.
    fn insert_chars(&mut self, n: usize) {
        let (cr, cc) = self.cursor;
        let n = n.min(self.cols - cc);
        let row_start = self.idx(cr, 0);
        let row_end = row_start + self.cols;
        let cur = self.idx(cr, cc);
        self.viewport.copy_within(cur..row_end - n, cur + n);
        let blank = Cell::blank_with(&self.pen);
        for c in &mut self.viewport[cur..cur + n] {
            *c = blank;
        }
        self.wrap_pending = false;
    }

    /// Delete `n` cells at the cursor (DCH), shifting the rest of the row
    /// left and blanking the tail.
    fn delete_chars(&mut self, n: usize) {
        let (cr, cc) = self.cursor;
        let n = n.min(self.cols - cc);
        let row_start = self.idx(cr, 0);
        let row_end = row_start + self.cols;
        let cur = self.idx(cr, cc);
        self.viewport.copy_within(cur + n..row_end, cur);
        let blank = Cell::blank_with(&self.pen);
        for c in &mut self.viewport[row_end - n..row_end] {
            *c = blank;
        }
        self.wrap_pending = false;
    }

    /// Erase `n` cells at the cursor in place (ECH) — no shift.
    fn erase_chars(&mut self, n: usize) {
        let (cr, cc) = self.cursor;
        let n = n.min(self.cols - cc);
        let cur = self.idx(cr, cc);
        let blank = Cell::blank_with(&self.pen);
        for c in &mut self.viewport[cur..cur + n] {
            *c = blank;
        }
        self.wrap_pending = false;
    }

    /// DECSC (`ESC 7`) / SCO save (`CSI s`): stash cursor + pen.
    fn save_cursor(&mut self) {
        self.saved_cursor = Some(SavedCursor {
            cursor: self.cursor,
            pen: self.pen.clone(),
            wrap_pending: self.wrap_pending,
        });
    }

    /// DECRC (`ESC 8`) / SCO restore (`CSI u`): restore cursor + pen.
    ///
    /// With no prior save the VT spec homes the cursor and resets the pen;
    /// that matches xterm and keeps a stray restore from leaving the cursor
    /// at an arbitrary spot.
    fn restore_cursor(&mut self) {
        match self.saved_cursor.take() {
            Some(s) => {
                self.cursor = (s.cursor.0.min(self.rows - 1), s.cursor.1.min(self.cols - 1));
                self.pen = s.pen.clone();
                self.wrap_pending = s.wrap_pending;
                // Keep the save so a second restore re-applies it (xterm
                // DECRC does not consume the saved state).
                self.saved_cursor = Some(s);
            }
            None => {
                self.cursor = (0, 0);
                self.pen = Pen::default();
                self.wrap_pending = false;
            }
        }
    }

    /// Enter the alternate screen (`CSI ?1049h` / `?1047h` / `?47h`).
    ///
    /// Stashes the primary buffer and switches to a freshly **cleared** alt
    /// buffer — the root fix for banner bleed-through: a full-screen TUI's
    /// startup banner lives on the primary screen, and without a real alt
    /// buffer every alt-screen redraw was layered on top of it.
    ///
    /// The primary cursor/pen are always stashed in `AltScreen` and restored
    /// on exit — `?1049` mandates it and doing so for `?47`/`?1047` too is
    /// harmless for a scraped panel (a precise restore can only help).
    fn enter_alt_screen(&mut self) {
        if self.alt_saved.is_some() {
            // Already on the alt screen — a redundant set is a no-op so the
            // stashed primary buffer is never clobbered.
            return;
        }
        self.alt_saved = Some(AltScreen {
            viewport: std::mem::replace(
                &mut self.viewport,
                vec![Cell::default(); self.cols * self.rows],
            ),
            cursor: self.cursor,
            pen: std::mem::take(&mut self.pen),
            scroll_top: self.scroll_top,
            scroll_bottom: self.scroll_bottom,
            wrap_pending: self.wrap_pending,
            saved_cursor: self.saved_cursor.take(),
        });
        // Fresh alt screen: cleared buffer, home cursor, full-screen region.
        self.cursor = (0, 0);
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.wrap_pending = false;
    }

    /// Leave the alternate screen (`CSI ?1049l` / `?1047l` / `?47l`).
    ///
    /// Restores the primary buffer verbatim; the alt buffer is discarded so
    /// no alt content survives the switch back.
    fn leave_alt_screen(&mut self) {
        let Some(alt) = self.alt_saved.take() else {
            // Not on the alt screen — nothing to restore.
            return;
        };
        // The restored primary buffer may have been sized while on the alt
        // screen; clamp it to the current dimensions so a resize that
        // happened mid-alt cannot leave a mis-shaped viewport.
        let want = self.cols * self.rows;
        let mut viewport = alt.viewport;
        if viewport.len() != want {
            viewport.resize(want, Cell::default());
        }
        self.viewport = viewport;
        self.cursor = (
            alt.cursor.0.min(self.rows - 1),
            alt.cursor.1.min(self.cols - 1),
        );
        self.pen = alt.pen;
        self.scroll_top = alt.scroll_top.min(self.rows - 1);
        self.scroll_bottom = alt.scroll_bottom.min(self.rows - 1);
        if self.scroll_top >= self.scroll_bottom {
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
        }
        self.wrap_pending = alt.wrap_pending;
        self.saved_cursor = alt.saved_cursor;
    }

    /// Apply one DEC private mode set/reset (`CSI ? Pn h` / `l`).
    ///
    /// Modes that change cell state (the alternate screen) are acted on, and
    /// bracketed paste (`?2004`) is tracked as an input-framing hint (it does
    /// not change the scraped grid). Cursor-visibility (`?25`), mouse modes,
    /// synchronized output (`?2026`) etc. remain deliberately ignored — they
    /// do not affect the scraped grid.
    fn set_private_mode(&mut self, mode: u16, enable: bool) {
        match mode {
            // Alternate screen buffer. `?47` is the legacy bare switch;
            // `?1047` clears the alt buffer on exit; `?1049` additionally
            // saves/restores the cursor. caucus treats all three as a
            // save-primary / cleared-alt / restore-primary pair — the
            // distinctions only matter to applications that depend on alt
            // content persisting, which a scraped panel never does.
            47 | 1047 | 1049 => {
                if enable {
                    self.enter_alt_screen();
                } else {
                    self.leave_alt_screen();
                }
            }
            // Bracketed paste: tracked, not rendered. The input path frames a
            // programmatic prompt as a real paste when this is on so the
            // submitting `\r` is not absorbed into the prompt buffer.
            2004 => self.bracketed_paste = enable,
            _ => {}
        }
    }
}

/// First numeric parameter, defaulting to `default` when absent or zero-ish.
fn first_param(params: &Params, default: u16) -> u16 {
    match params.iter().next().and_then(|p| p.first().copied()) {
        Some(0) | None => default,
        Some(v) => v,
    }
}

/// Resize one logical row to `cols` columns: truncate when narrower, pad with
/// blank cells when wider.
fn resize_row(row: &[Cell], cols: usize) -> Vec<Cell> {
    let mut out: Vec<Cell> = row.iter().take(cols).cloned().collect();
    out.resize(cols, Cell::default());
    out
}

/// Map a 24-bit RGB triple to the closest xterm 256-colour palette index.
///
/// caucus's [`Cell`] colour fields are [`u8`] (skeleton constraint), so
/// true-colour output is approximated rather than stored verbatim. The 6×6×6
/// colour cube occupies indices 16..=231; pure greys snap to the 232..=255
/// grey ramp.
fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    // Grey ramp when the channels are near-equal.
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max.saturating_sub(min) < 8 {
        if r < 8 {
            return 16;
        }
        if r > 248 {
            return 231;
        }
        return 232 + ((r as u16 - 8) * 24 / 247) as u8;
    }
    let q = |v: u8| -> u16 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            ((v as u16 - 35) / 40).min(5)
        }
    };
    (16 + 36 * q(r) + 6 * q(g) + q(b)) as u8
}

/// Fold a raw xterm-256 palette index into the caucus [`Cell`] colour encoding
/// so the extended-colour paths (`38;5;n`, `38;2;r;g;b`) agree with the
/// SGR-named path (`30..=37` / `90..=97`).
///
/// Without this the field carried two incompatible meanings in `0..=16`: the
/// named path stores the 16 ANSI colours shifted up to `1..=16` (`0` = the
/// terminal default), while the extended path stored raw xterm indices. The
/// collision rendered dark text invisible — `rgb_to_256` returns `16` (the
/// colour cube's black corner) for near-black truecolour, and field value `16`
/// is [`crate::render`]'s *bright white*, so a dark glyph on a light diff
/// background drew white-on-white.
///
/// * xterm `0..=15` (the 16 ANSI colours) → caucus `1..=16`.
/// * xterm `16` (cube black) has no distinct caucus slot; it is visually black,
///   so it folds onto ANSI black (`1`) rather than the bright-white slot.
/// * xterm `17..=255` (extended cube + grey ramp) are stored verbatim and
///   rendered through `Color::Indexed`.
fn xterm_to_field(n: u8) -> u8 {
    match n {
        0..=15 => n + 1,
        16 => 1,
        _ => n,
    }
}

/// `vte::Perform` implementation: the parsed-op → cell-state machine.
///
/// **Fully implemented**
/// - `print` — glyph write, cursor advance, deferred right-margin wrap,
///   scroll-on-overflow into bounded scrollback, wide (CJK) glyphs.
/// - `execute` — `LF`/`VT`/`FF`, `CR`, `BS`, `HT` (8-col tab stops), `BEL`
///   (ignored), `NEL` via `\x85`.
/// - `csi_dispatch` — `CUU/CUD/CUF/CUB`, `CNL/CPL`, `CHA`, `VPA`, `CUP/HVP`,
///   `ED/EL`, `IL/DL`, `ICH/DCH/ECH`, `SU/SD`, `SGR`, `DECSTBM`.
/// - `esc_dispatch` — `IND`, `RI`, `NEL`, `RIS` (reset), `DECSC`/`DECRC`
///   (`ESC 7` / `ESC 8` cursor save/restore), charset designation (parsed
///   and ignored — see partials).
/// - `csi_dispatch` private modes — the alternate screen (`?1049` / `?1047`
///   / `?47`) is fully implemented: enter stashes the primary buffer and
///   switches to a cleared alt buffer, exit restores it. Cursor save/restore
///   via `CSI s` / `CSI u` is implemented.
/// - `osc_dispatch` — window title (OSC 0/2) and hyperlinks (OSC 8) stored on
///   the grid.
///
/// **Documented partials** (intentionally simplified for caucus's read-only
/// scraping use; design.md §0 #3 scopes the grid to ~2-4k LOC):
/// - Charset translation (DEC special graphics, G0/G1 shift-in/shift-out) is
///   *not* applied — designations are parsed and discarded. caucus reads agent
///   prose, which is UTF-8, so box-drawing glyph substitution is unnecessary.
/// - Combining marks / zero-width joiners are dropped rather than merged into
///   the base cell.
/// - DEC private modes other than the alternate screen and bracketed paste
///   (`?25` cursor visibility, `?2026` synchronized output, mouse modes) are
///   accepted and ignored — they do not affect the scraped cell grid. The
///   alternate screen *is* honoured (see `csi_dispatch` above); bracketed
///   paste (`?2004`) is *tracked* (queryable via [`Grid::bracketed_paste`])
///   as an input-framing hint, though it too does not change the grid.
/// - DCS strings (`hook`/`put`/`unhook`) are ignored.
impl Perform for Grid {
    fn print(&mut self, c: char) {
        self.put_glyph(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => {
                // BS — move left one column.
                if self.wrap_pending {
                    self.wrap_pending = false;
                } else if self.cursor.1 > 0 {
                    self.cursor.1 -= 1;
                }
            }
            0x09 => {
                // HT — next 8-column tab stop.
                let next = ((self.cursor.1 / 8) + 1) * 8;
                self.cursor.1 = next.min(self.cols - 1);
                self.wrap_pending = false;
            }
            0x0A..=0x0C => {
                // LF / VT / FF — line feed.
                self.line_feed();
            }
            0x0D => {
                // CR — column 0.
                self.cursor.1 = 0;
                self.wrap_pending = false;
            }
            0x85 => {
                // NEL — newline (CR + LF).
                self.cursor.1 = 0;
                self.line_feed();
            }
            _ => {
                // BEL (0x07) and other C0/C1 controls: nothing to render.
            }
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // DEC private sequences (`CSI ? Pn h` / `l`). Modes that change cell
        // state — chiefly the alternate screen — are acted on; the rest are
        // accepted and ignored (see [`Grid::set_private_mode`]).
        if intermediates.first() == Some(&b'?') {
            if matches!(action, 'h' | 'l') {
                let enable = action == 'h';
                for p in params.iter() {
                    for &mode in p {
                        self.set_private_mode(mode, enable);
                    }
                }
            }
            return;
        }
        // Non-standard private-marker sequences: a leading `<`, `>` or `=`
        // byte (vte exposes it as the first intermediate) marks a CSI that is
        // *not* a standard cell-affecting op. These cover the kitty keyboard
        // protocol (`CSI < u`, `CSI > 1 u`), `XTMODKEYS` (`CSI > 4 m`),
        // `XTVERSION` (`CSI > q`), DEC tertiary-DA (`CSI = c`) and similar
        // terminal-capability negotiation. Dispatching them to the standard
        // handlers is a bug: `CSI < u` would hit the SCO-restore (`u`) path
        // and home the cursor, and `CSI > 4 ; 2 m` would be misread as SGR.
        // They never change the scraped grid, so ignore them outright.
        if matches!(intermediates.first(), Some(b'<' | b'>' | b'=')) {
            return;
        }
        let n = first_param(params, 1) as usize;
        let (cr, cc) = self.cursor;
        match action {
            'A' => self.move_to(cr.saturating_sub(n), cc), // CUU
            'B' => self.move_to(cr + n, cc),               // CUD
            'C' => self.move_to(cr, cc + n),               // CUF
            'D' => self.move_to(cr, cc.saturating_sub(n)), // CUB
            'E' => self.move_to(cr + n, 0),                // CNL
            'F' => self.move_to(cr.saturating_sub(n), 0),  // CPL
            'G' | '`' => self.move_to(cr, n.saturating_sub(1)), // CHA / HPA
            'd' => self.move_to(n.saturating_sub(1), cc),  // VPA
            'H' | 'f' => {
                // CUP / HVP — both params 1-based, default 1.
                let row = first_param(params, 1) as usize;
                let col = params
                    .iter()
                    .nth(1)
                    .and_then(|p| p.first().copied())
                    .filter(|&v| v != 0)
                    .unwrap_or(1) as usize;
                self.move_to(row.saturating_sub(1), col.saturating_sub(1));
            }
            'J' => self.erase_display(first_param(params, 0)),
            'K' => self.erase_line(first_param(params, 0)),
            'L' => self.insert_lines(n),
            'M' => self.delete_lines(n),
            '@' => self.insert_chars(n),
            'P' => self.delete_chars(n),
            'X' => self.erase_chars(n),
            'S' => self.scroll_up(n),
            'T' => self.scroll_down(n),
            'm' => self.apply_sgr(params),
            'r' => {
                // DECSTBM — set top/bottom scroll margins.
                let top = first_param(params, 1) as usize;
                let bottom = params
                    .iter()
                    .nth(1)
                    .and_then(|p| p.first().copied())
                    .filter(|&v| v != 0)
                    .map(|v| v as usize)
                    .unwrap_or(self.rows);
                let top = top.saturating_sub(1).min(self.rows - 1);
                let bottom = bottom.saturating_sub(1).min(self.rows - 1);
                if top < bottom {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                    // DECSTBM homes the cursor.
                    self.move_to(0, 0);
                }
            }
            // `CSI s` is SCO save (SCOSC) when it carries no explicit
            // arguments; with two it is DECSLRM (set left/right margin),
            // which caucus does not implement and so ignores.
            's' if params.len() < 2 => self.save_cursor(),
            'u' => self.restore_cursor(), // SCO restore (SCORC)
            _ => {
                // CSI sequences not relevant to a scraped grid (DSR/DA query
                // replies, `CSI s` *with* params = DECSLRM left/right margin)
                // are intentionally ignored.
            }
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        // Charset designation: `ESC ( B`, `ESC ) 0`, etc. Parsed and ignored
        // (documented partial — caucus reads UTF-8 agent prose).
        if matches!(intermediates.first(), Some(b'(' | b')' | b'*' | b'+')) {
            return;
        }
        match byte {
            b'7' => self.save_cursor(),    // DECSC
            b'8' => self.restore_cursor(), // DECRC
            b'D' => self.line_feed(),      // IND
            b'E' => {
                // NEL — CR + LF.
                self.cursor.1 = 0;
                self.line_feed();
            }
            b'M' => {
                // RI — reverse index.
                if self.cursor.0 == self.scroll_top {
                    self.scroll_down(1);
                } else if self.cursor.0 > 0 {
                    self.cursor.0 -= 1;
                }
                self.wrap_pending = false;
            }
            b'c' => {
                // RIS — full reset.
                self.viewport = vec![Cell::default(); self.cols * self.rows];
                self.scrollback.clear();
                self.cursor = (0, 0);
                self.pen = Pen::default();
                self.scroll_top = 0;
                self.scroll_bottom = self.rows - 1;
                self.wrap_pending = false;
                self.title = None;
                self.hyperlink = None;
                self.saved_cursor = None;
                self.alt_saved = None;
                self.bracketed_paste = false;
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some(&code) = params.first() else {
            return;
        };
        match code {
            b"0" | b"2" => {
                // Set window/icon title.
                if let Some(&title) = params.get(1) {
                    self.title = Some(String::from_utf8_lossy(title).into_owned());
                }
            }
            b"8" => {
                // Hyperlink: `OSC 8 ; params ; URI ST`. params[2] is the URI;
                // an empty URI closes the link.
                match params.get(2) {
                    Some(uri) if !uri.is_empty() => {
                        self.hyperlink = Some(String::from_utf8_lossy(uri).into_owned());
                    }
                    _ => self.hyperlink = None,
                }
            }
            _ => {
                // OSC 4 (palette), 52 (clipboard), 10/11 (default colours):
                // not needed for a scraped grid.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(g: &Grid, row: usize, col: usize) -> char {
        g.cell(row, col).unwrap().ch
    }

    /// The content-change `generation` advances when bytes are ingested or the
    /// grid is resized, and stays put on a no-op (empty advance / same-size
    /// resize) — the equality the menu-scan cache relies on to skip an idle
    /// panel.
    #[test]
    fn generation_tracks_content_changes_only() {
        let mut g = Grid::new(20, 5);
        let g0 = g.generation();

        g.advance(b"");
        assert_eq!(g.generation(), g0, "an empty advance must not bump");

        g.advance(b"hello");
        let g1 = g.generation();
        assert!(g1 > g0, "ingesting bytes must bump the generation");

        g.resize(30, 6);
        let g2 = g.generation();
        assert!(g2 > g1, "a real resize must bump the generation");

        g.resize(30, 6);
        assert_eq!(g.generation(), g2, "a no-op resize must not bump");
    }

    #[test]
    fn new_grid_is_blank() {
        let g = Grid::new(80, 24);
        assert_eq!(g.size(), (80, 24));
        assert_eq!(g.cell(0, 0), Some(&Cell::default()));
        assert!(g.cell(24, 0).is_none());
    }

    #[test]
    fn advance_does_not_panic_on_escape_bytes() {
        let mut g = Grid::new(80, 24);
        g.advance(b"hello\x1b[31mworld\x1b[0m");
        assert_eq!(g.size(), (80, 24));
    }

    #[test]
    fn resize_changes_dimensions() {
        let mut g = Grid::new(80, 24);
        g.resize(100, 30);
        assert_eq!(g.size(), (100, 30));
    }

    #[test]
    fn print_writes_glyphs_and_advances_cursor() {
        let mut g = Grid::new(20, 5);
        g.advance(b"abc");
        assert_eq!(at(&g, 0, 0), 'a');
        assert_eq!(at(&g, 0, 1), 'b');
        assert_eq!(at(&g, 0, 2), 'c');
        assert_eq!(g.cursor(), (0, 3));
    }

    #[test]
    fn cr_lf_moves_cursor() {
        let mut g = Grid::new(20, 5);
        g.advance(b"ab\r\ncd");
        assert_eq!(g.row_text(0), "ab".to_string() + &" ".repeat(18));
        assert_eq!(at(&g, 1, 0), 'c');
        assert_eq!(at(&g, 1, 1), 'd');
        assert_eq!(g.cursor(), (1, 2));
    }

    #[test]
    fn backspace_and_tab() {
        let mut g = Grid::new(40, 3);
        g.advance(b"abc\x08X");
        // BS over 'c', overwrite with 'X'.
        assert_eq!(at(&g, 0, 2), 'X');
        let mut g2 = Grid::new(40, 3);
        g2.advance(b"a\tb");
        assert_eq!(g2.cursor().1, 9); // tab to col 8, then 'b' advances to 9.
        assert_eq!(at(&g2, 0, 8), 'b');
    }

    #[test]
    fn deferred_wrap_at_right_margin() {
        let mut g = Grid::new(3, 3);
        g.advance(b"abc");
        // After printing into the last column, cursor parks there pending wrap.
        assert_eq!(g.cursor(), (0, 2));
        g.advance(b"d");
        // Next glyph wraps to the new line.
        assert_eq!(at(&g, 1, 0), 'd');
        assert_eq!(g.cursor(), (1, 1));
    }

    #[test]
    fn scroll_pushes_top_row_into_scrollback() {
        let mut g = Grid::new(10, 2);
        g.advance(b"row0\r\nrow1\r\nrow2");
        // 2-row viewport: row0 scrolled off, viewport now row1/row2.
        assert_eq!(g.row_text(0).trim_end(), "row1");
        assert_eq!(g.row_text(1).trim_end(), "row2");
        let sb: Vec<_> = g.scrollback().collect();
        assert_eq!(sb.len(), 1);
        let text: String = sb[0].iter().map(|c| c.ch).collect();
        assert_eq!(text.trim_end(), "row0");
    }

    #[test]
    fn scrollback_ring_is_bounded() {
        // 2-row viewport so a line feed at the bottom actually scrolls.
        let mut g = Grid::new(4, 2);
        g.scrollback_limit = 3;
        for i in 0..10u8 {
            g.advance(&[b'0' + i]);
            g.advance(b"\r\n");
        }
        let sb: Vec<_> = g.scrollback().collect();
        assert_eq!(sb.len(), 3, "ring capped at 3");
        // Lines 0..=8 scroll off the top (each trailing \r\n past the first
        // line feeds at the bottom margin). The ring keeps the last 3: 6,7,8.
        let oldest: String = sb[0].iter().map(|c| c.ch).collect();
        assert_eq!(oldest.trim_end(), "6");
        let newest: String = sb[2].iter().map(|c| c.ch).collect();
        assert_eq!(newest.trim_end(), "8");
    }

    #[test]
    fn scroll_up_n_batches_and_captures_each_leaving_row() {
        // CSI 2 S scrolls the full-screen region up by two in one shot: the top
        // two rows enter scrollback oldest-first, the survivors shift up, and
        // the bottom two rows blank.
        let mut g = Grid::new(5, 4);
        g.advance(b"r0\r\nr1\r\nr2\r\nr3");
        g.advance(b"\x1b[1;1H\x1b[2S"); // home, then scroll up 2
        assert_eq!(g.row_text(0).trim_end(), "r2");
        assert_eq!(g.row_text(1).trim_end(), "r3");
        assert_eq!(g.row_text(2).trim(), "");
        assert_eq!(g.row_text(3).trim(), "");
        let sb: Vec<_> = g.scrollback().collect();
        assert_eq!(sb.len(), 2, "both leaving rows captured");
        let oldest: String = sb[0].iter().map(|c| c.ch).collect();
        let newest: String = sb[1].iter().map(|c| c.ch).collect();
        assert_eq!(oldest.trim_end(), "r0");
        assert_eq!(newest.trim_end(), "r1");
    }

    #[test]
    fn partial_region_scroll_up_does_not_feed_scrollback() {
        // A DECSTBM-bounded region that does not start at row 0 discards the
        // line leaving its top — and allocates no clone for it.
        let mut g = Grid::new(5, 4);
        g.advance(b"\x1b[2;4r"); // region rows 2..=4 (1-based) -> 1..=3
        g.advance(b"\x1b[2;1Hr1\r\nr2\r\nr3"); // fill the region
        g.advance(b"\x1b[4;1H\x1b[1S"); // cursor in region, scroll up 1
        assert!(
            g.scrollback().next().is_none(),
            "partial-region scroll keeps nothing in scrollback"
        );
    }

    #[test]
    fn scroll_down_n_batches() {
        let mut g = Grid::new(5, 4);
        g.advance(b"r0\r\nr1\r\nr2\r\nr3");
        g.advance(b"\x1b[1;1H\x1b[2T"); // home, scroll down 2
        assert_eq!(g.row_text(0).trim(), "");
        assert_eq!(g.row_text(1).trim(), "");
        assert_eq!(g.row_text(2).trim_end(), "r0");
        assert_eq!(g.row_text(3).trim_end(), "r1");
    }

    #[test]
    fn cup_positions_cursor_one_based() {
        let mut g = Grid::new(20, 10);
        g.advance(b"\x1b[3;5HX");
        // Row 3, col 5 (1-based) -> (2,4).
        assert_eq!(at(&g, 2, 4), 'X');
    }

    #[test]
    fn cursor_moves_cuu_cud_cuf_cub() {
        let mut g = Grid::new(20, 10);
        g.advance(b"\x1b[5;5H");
        g.advance(b"\x1b[2A"); // up 2
        assert_eq!(g.cursor(), (2, 4));
        g.advance(b"\x1b[3B"); // down 3
        assert_eq!(g.cursor(), (5, 4));
        g.advance(b"\x1b[2C"); // right 2
        assert_eq!(g.cursor(), (5, 6));
        g.advance(b"\x1b[4D"); // left 4
        assert_eq!(g.cursor(), (5, 2));
    }

    #[test]
    fn erase_line_modes() {
        let mut g = Grid::new(10, 2);
        g.advance(b"abcdefghij");
        g.advance(b"\x1b[1;5H"); // cursor at col 4 ('e')
        g.advance(b"\x1b[0K"); // erase cursor..end
        assert_eq!(g.row_text(0).trim_end(), "abcd");

        let mut g2 = Grid::new(10, 2);
        g2.advance(b"abcdefghij\x1b[1;5H\x1b[1K"); // erase start..cursor
        assert_eq!(g2.row_text(0), "     fghij");

        let mut g3 = Grid::new(10, 2);
        g3.advance(b"abcdefghij\x1b[2K"); // whole row
        assert_eq!(g3.row_text(0).trim(), "");
    }

    #[test]
    fn erase_display_modes() {
        let mut g = Grid::new(5, 3);
        g.advance(b"aaaaa\r\nbbbbb\r\nccccc");
        g.advance(b"\x1b[2;3H"); // row 1, col 2
        g.advance(b"\x1b[0J"); // cursor..end
        assert_eq!(g.row_text(0).trim_end(), "aaaaa");
        assert_eq!(g.row_text(1), "bb   ");
        assert_eq!(g.row_text(2).trim(), "");

        let mut g2 = Grid::new(5, 3);
        g2.advance(b"aaaaa\r\nbbbbb\r\nccccc\x1b[2J");
        for r in 0..3 {
            assert_eq!(g2.row_text(r).trim(), "");
        }
    }

    #[test]
    fn sgr_parses_into_cell_attrs() {
        let mut g = Grid::new(20, 3);
        g.advance(b"\x1b[1;4;31;42mX\x1b[0mY");
        let x = g.cell(0, 0).unwrap();
        assert_eq!(x.ch, 'X');
        assert_ne!(x.attrs & attr::BOLD, 0);
        assert_ne!(x.attrs & attr::UNDERLINE, 0);
        assert_eq!(x.fg, 2); // red = 31 -> index 2
        assert_eq!(x.bg, 3); // green bg = 42 -> index 3
        let y = g.cell(0, 1).unwrap();
        assert_eq!(y.ch, 'Y');
        assert_eq!(y.attrs, 0);
        assert_eq!(y.fg, 0);
        assert_eq!(y.bg, 0);
    }

    #[test]
    fn sgr_256_and_truecolor() {
        let mut g = Grid::new(20, 3);
        g.advance(b"\x1b[38;5;200mA");
        assert_eq!(g.cell(0, 0).unwrap().fg, 200);
        g.advance(b"\x1b[48;2;255;255;255mB");
        // Pure white truecolor snaps to 231 in the grey-ramp branch.
        assert_eq!(g.cell(0, 1).unwrap().bg, 231);
    }

    /// `xterm_to_field` folds raw xterm-256 indices into the caucus field
    /// encoding so the extended-colour paths agree with the SGR-named path.
    #[test]
    fn xterm_to_field_folds_the_low_palette_into_the_ansi_slots() {
        // The 16 ANSI colours shift up by one (matching `30..=37` / `90..=97`).
        assert_eq!(xterm_to_field(0), 1, "xterm black -> ANSI black slot");
        assert_eq!(xterm_to_field(7), 8);
        assert_eq!(xterm_to_field(8), 9, "xterm bright-black -> bright slot");
        assert_eq!(xterm_to_field(15), 16, "xterm bright-white -> bright white");
        // Cube black folds onto ANSI black, not the bright-white slot (16).
        assert_eq!(xterm_to_field(16), 1, "cube black must not become white");
        // The extended cube + grey ramp pass through verbatim.
        assert_eq!(xterm_to_field(17), 17);
        assert_eq!(xterm_to_field(231), 231);
        assert_eq!(xterm_to_field(254), 254);
    }

    /// Regression: dark text must not render white. A near-black true-colour
    /// foreground used to snap to xterm cube index 16 and land on the field
    /// value the renderer treats as bright white, so dark code on a light diff
    /// background drew white-on-white. It must now fold onto the ANSI black
    /// slot (`1`), and an explicit `38;5;0` black must stay black, not default.
    #[test]
    fn dark_extended_foreground_does_not_fold_to_white() {
        let mut g = Grid::new(20, 3);

        // Near-black true-colour (channels < 8 -> rgb_to_256 returns 16) must
        // not stay at field 16, which the renderer treats as bright white.
        g.advance(b"\x1b[38;2;5;5;5mA");
        let fg = g.cell(0, 0).unwrap().fg;
        assert_eq!(
            fg, 1,
            "near-black truecolor must fold to ANSI black, got {fg}"
        );

        // `38;5;16` (cube black) likewise.
        g.advance(b"\x1b[38;5;16mB");
        assert_eq!(g.cell(0, 1).unwrap().fg, 1);

        // `38;5;0` is an explicit black, distinct from the default (0).
        g.advance(b"\x1b[38;5;0mC");
        assert_eq!(
            g.cell(0, 2).unwrap().fg,
            1,
            "explicit xterm black must stay black, not the terminal default"
        );
    }

    #[test]
    fn sgr_bright_colors() {
        let mut g = Grid::new(20, 3);
        g.advance(b"\x1b[91mA"); // bright red
        assert_eq!(g.cell(0, 0).unwrap().fg, 10); // 91-90+9
    }

    #[test]
    fn wide_glyph_occupies_two_columns() {
        let mut g = Grid::new(10, 2);
        g.advance("한x".as_bytes());
        assert_eq!(at(&g, 0, 0), '한');
        assert_eq!(at(&g, 0, 1), '\0'); // trailing half
        assert_eq!(at(&g, 0, 2), 'x');
        assert_eq!(g.cursor(), (0, 3));
    }

    #[test]
    fn overwriting_wide_glyph_cells_clears_the_other_half() {
        let mut g = Grid::new(6, 2);
        g.advance("한Z".as_bytes());

        // Overwrite the lead cell with a narrow glyph. The trailing marker at
        // col 1 must be cleared; otherwise row rendering skips it and shifts
        // the later `Z` left by one column.
        g.advance(b"\rX");
        assert_eq!(at(&g, 0, 0), 'X');
        assert_eq!(at(&g, 0, 1), ' ');
        assert_eq!(at(&g, 0, 2), 'Z');
        assert_eq!(g.row_text(0), "X Z   ");

        // Overwrite a trailing-half cell. The lead glyph must be cleared too,
        // leaving the replacement at its requested column.
        let mut g = Grid::new(6, 2);
        g.advance("한".as_bytes());
        g.advance(b"\x1b[1;2HY");
        assert_eq!(at(&g, 0, 0), ' ');
        assert_eq!(at(&g, 0, 1), 'Y');
        assert_eq!(g.row_text(0), " Y    ");

        // A wide write that overlaps the lead cell of an existing wide glyph
        // must clear that glyph's trailing marker as well.
        let mut g = Grid::new(6, 2);
        g.advance("a한".as_bytes());
        g.advance(" \r界".as_bytes());
        assert_eq!(at(&g, 0, 0), '界');
        assert_eq!(at(&g, 0, 1), '\0');
        assert_eq!(at(&g, 0, 2), ' ');
        assert_eq!(g.row_text(0), "界    ");
    }

    #[test]
    fn scroll_region_decstbm() {
        let mut g = Grid::new(5, 5);
        // Restrict scrolling to rows 2..=4 (1-based).
        g.advance(b"\x1b[2;4r");
        // DECSTBM homes the cursor.
        assert_eq!(g.cursor(), (0, 0));
        g.advance(b"\x1b[2;1Hr1\r\n");
        g.advance(b"r2\r\n");
        g.advance(b"r3\r\n");
        g.advance(b"r4");
        // Region rows 1..=3; r1 was at row1, after scroll it should move up.
        assert_eq!(g.row_text(0).trim(), ""); // row 0 untouched
        assert_eq!(g.row_text(3).trim_end(), "r4");
    }

    #[test]
    fn insert_and_delete_lines() {
        let mut g = Grid::new(5, 4);
        g.advance(b"aaaaa\r\nbbbbb\r\nccccc\r\nddddd");
        g.advance(b"\x1b[2;1H\x1b[1L"); // insert 1 line at row 1
        assert_eq!(g.row_text(0).trim_end(), "aaaaa");
        assert_eq!(g.row_text(1).trim(), "");
        assert_eq!(g.row_text(2).trim_end(), "bbbbb");

        let mut g2 = Grid::new(5, 4);
        g2.advance(b"aaaaa\r\nbbbbb\r\nccccc\r\nddddd");
        g2.advance(b"\x1b[2;1H\x1b[1M"); // delete 1 line at row 1
        assert_eq!(g2.row_text(1).trim_end(), "ccccc");
        assert_eq!(g2.row_text(2).trim_end(), "ddddd");
    }

    #[test]
    fn insert_delete_erase_chars() {
        let mut g = Grid::new(10, 2);
        g.advance(b"abcdef\x1b[1;1H\x1b[2@"); // insert 2 blanks at col 0
        assert_eq!(g.row_text(0).trim_end(), "  abcdef");

        let mut g2 = Grid::new(10, 2);
        g2.advance(b"abcdef\x1b[1;1H\x1b[2P"); // delete 2 chars at col 0
        assert_eq!(g2.row_text(0).trim_end(), "cdef");

        let mut g3 = Grid::new(10, 2);
        g3.advance(b"abcdef\x1b[1;3H\x1b[2X"); // erase 2 chars at col 2
        assert_eq!(g3.row_text(0), "ab  ef    ");
    }

    #[test]
    fn reverse_index_at_top_scrolls_down() {
        let mut g = Grid::new(5, 3);
        g.advance(b"r0\r\nr1\r\nr2");
        g.advance(b"\x1b[1;1H"); // home
        g.advance(b"\x1bM"); // RI at top
        assert_eq!(g.row_text(0).trim(), ""); // blank inserted
        assert_eq!(g.row_text(1).trim_end(), "r0");
        assert_eq!(g.row_text(2).trim_end(), "r1");
    }

    #[test]
    fn esc_index_and_nel() {
        let mut g = Grid::new(10, 3);
        g.advance(b"ab\x1bD"); // IND: down, keep column
        assert_eq!(g.cursor(), (1, 2));
        g.advance(b"\x1bE"); // NEL: down + col 0
        assert_eq!(g.cursor(), (2, 0));
    }

    #[test]
    fn osc_title_and_hyperlink() {
        let mut g = Grid::new(20, 3);
        g.advance(b"\x1b]0;my title\x07");
        assert_eq!(g.title(), Some("my title"));
        g.advance(b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07");
        // Link closed by the trailing empty OSC 8.
        assert_eq!(g.hyperlink(), None);
        assert_eq!(at(&g, 0, 0), 'l');
    }

    #[test]
    fn ris_resets_grid() {
        let mut g = Grid::new(10, 3);
        g.advance(b"\x1b[31mhello\x1b]0;t\x07\x1b[?2004h");
        assert!(g.bracketed_paste());
        g.advance(b"\x1bc"); // RIS
        assert_eq!(at(&g, 0, 0), ' ');
        assert_eq!(g.cursor(), (0, 0));
        assert_eq!(g.title(), None);
        assert_eq!(g.cell(0, 0).unwrap().fg, 0);
        assert!(!g.bracketed_paste(), "RIS clears bracketed-paste mode");
    }

    #[test]
    fn resize_preserves_content_and_spills_on_shrink() {
        let mut g = Grid::new(10, 4);
        g.advance(b"row0\r\nrow1\r\nrow2\r\nrow3");
        g.resize(10, 2); // shrink to 2 rows
        assert_eq!(g.size(), (10, 2));
        // row0/row1 spilled into scrollback.
        let sb: Vec<_> = g.scrollback().collect();
        assert_eq!(sb.len(), 2);
        assert_eq!(g.row_text(0).trim_end(), "row2");
        assert_eq!(g.row_text(1).trim_end(), "row3");
    }

    #[test]
    fn resize_wider_pads_blank() {
        let mut g = Grid::new(5, 2);
        g.advance(b"abc");
        g.resize(10, 2);
        assert_eq!(g.size(), (10, 2));
        assert_eq!(g.row_text(0), "abc       ");
    }

    #[test]
    fn resize_clamps_dimensions_to_the_maximum() {
        // A garbage-large size report (a display glitch / wake-time
        // `TIOCGWINSZ` returning nonsense) must not make the grid allocate
        // `cols * rows` cells unbounded. The grid caps both dimensions, so the
        // allocation stays bounded — this test completes instantly instead of
        // OOM-zeroing tens of gigabytes of blank cells.
        let mut g = Grid::new(80, 24);
        g.resize(usize::MAX, usize::MAX);
        assert_eq!(g.size(), (Grid::MAX_COLS, Grid::MAX_ROWS));
    }

    #[test]
    fn new_clamps_dimensions_to_the_maximum() {
        // Same ceiling enforced at construction, so even a panel spawned while
        // a bogus size is in effect cannot allocate an unbounded grid.
        let g = Grid::new(usize::MAX, usize::MAX);
        assert_eq!(g.size(), (Grid::MAX_COLS, Grid::MAX_ROWS));
    }

    #[test]
    fn erase_keeps_background_pen() {
        let mut g = Grid::new(5, 2);
        g.advance(b"\x1b[42m\x1b[2K"); // green bg, erase line
        assert_eq!(g.cell(0, 0).unwrap().bg, 3); // green bg index
    }

    #[test]
    fn cosmetic_private_modes_do_not_disturb_the_grid() {
        // `?25` (cursor visibility) and `?2004` (bracketed paste) do not affect
        // the cell grid; they must not disturb glyph printing. `?2004` is still
        // *tracked* for the input path (asserted below) — just not rendered.
        let mut g = Grid::new(10, 3);
        g.advance(b"\x1b[?25l\x1b[?2004hX");
        assert_eq!(at(&g, 0, 0), 'X');
        assert!(!g.on_alt_screen());
    }

    #[test]
    fn bracketed_paste_mode_is_tracked() {
        let mut g = Grid::new(10, 3);
        assert!(!g.bracketed_paste(), "off by default");
        g.advance(b"\x1b[?2004h");
        assert!(g.bracketed_paste(), "?2004h enables it");
        g.advance(b"\x1b[?2004l");
        assert!(!g.bracketed_paste(), "?2004l disables it");
    }

    // ----- alternate screen (banner bleed-through regression) --------------

    #[test]
    fn alt_screen_enter_clears_and_hides_primary() {
        // Primary screen carries a "banner". Switching to the alt screen must
        // present a *cleared* buffer — the banner must not bleed through.
        let mut g = Grid::new(20, 4);
        g.advance(b"BANNER ONE\r\nBANNER TWO\r\n");
        g.advance(b"\x1b[?1049h"); // enter alt screen
        assert!(g.on_alt_screen());
        for r in 0..4 {
            assert_eq!(
                g.row_text(r).trim(),
                "",
                "alt screen row {r} must start blank"
            );
        }
        // Conversation drawn on the alt screen stands alone — no banner under.
        g.advance(b"CONVO ALPHA");
        assert_eq!(g.row_text(0).trim_end(), "CONVO ALPHA");
        assert!(!g.row_text(0).contains("BANNER"));
    }

    #[test]
    fn alt_screen_exit_restores_primary_verbatim() {
        let mut g = Grid::new(20, 4);
        g.advance(b"BANNER ONE\r\nBANNER TWO");
        g.advance(b"\x1b[?1049h"); // enter
        g.advance(b"\x1b[2J\x1b[HCONVO ALPHA\r\nCONVO BETA");
        g.advance(b"\x1b[?1049l"); // exit -> primary restored
        assert!(!g.on_alt_screen());
        assert_eq!(g.row_text(0).trim_end(), "BANNER ONE");
        assert_eq!(g.row_text(1).trim_end(), "BANNER TWO");
        // No alt-screen content survives the switch back.
        assert!(!g.row_text(0).contains("CONVO"));
        assert!(!g.row_text(1).contains("CONVO"));
    }

    #[test]
    fn alt_screen_no_superimposition_after_redraws() {
        // The reported corruption: a startup banner stays in the top rows
        // while live output is drawn ON TOP. With a real alt buffer the two
        // can never coexist. Craft banner -> alt enter -> several redraws.
        let mut g = Grid::new(30, 6);
        g.advance("\x1b[1;1HClaude Code v2.1.143\r\n".as_bytes());
        g.advance(b"  mascot-art-line\r\n");
        g.advance(b"\x1b[?1049h"); // full-screen TUI takes over
        // Three redraw frames, each homing + erasing + reprinting.
        for frame in ["frame-A", "frame-B", "frame-C"] {
            g.advance(b"\x1b[H\x1b[2J");
            g.advance(format!("live: {frame}").as_bytes());
        }
        // Only the last frame is visible; the banner is gone entirely.
        assert_eq!(g.row_text(0).trim_end(), "live: frame-C");
        for r in 0..6 {
            let line = g.row_text(r);
            assert!(
                !line.contains("Claude Code") && !line.contains("mascot"),
                "banner must not coexist with alt-screen content (row {r}: {line:?})"
            );
        }
    }

    #[test]
    fn alt_screen_legacy_modes_47_and_1047() {
        for mode in ["47", "1047"] {
            let mut g = Grid::new(12, 3);
            g.advance(b"primary");
            g.advance(format!("\x1b[?{mode}h").as_bytes());
            assert!(g.on_alt_screen(), "?{mode}h enters alt screen");
            assert_eq!(g.row_text(0).trim(), "", "?{mode}h clears the buffer");
            g.advance(format!("\x1b[?{mode}l").as_bytes());
            assert!(!g.on_alt_screen());
            assert_eq!(g.row_text(0).trim_end(), "primary");
        }
    }

    #[test]
    fn alt_screen_redundant_enter_keeps_primary_snapshot() {
        // A second `?1049h` while already on the alt screen must not clobber
        // the stashed primary buffer.
        let mut g = Grid::new(12, 3);
        g.advance(b"primary");
        g.advance(b"\x1b[?1049h");
        g.advance(b"alt-content");
        g.advance(b"\x1b[?1049h"); // redundant
        g.advance(b"\x1b[?1049l");
        assert_eq!(g.row_text(0).trim_end(), "primary");
    }

    #[test]
    fn alt_screen_resize_preserves_primary() {
        // A resize while on the alt screen must not corrupt the stashed
        // primary buffer it will be restored to.
        let mut g = Grid::new(20, 4);
        g.advance(b"BANNER ONE\r\nBANNER TWO");
        g.advance(b"\x1b[?1049h");
        g.advance(b"alt stuff");
        g.resize(30, 6);
        g.advance(b"\x1b[?1049l");
        assert_eq!(g.size(), (30, 6));
        assert_eq!(g.row_text(0).trim_end(), "BANNER ONE");
        assert_eq!(g.row_text(1).trim_end(), "BANNER TWO");
    }

    // ----- cursor save / restore (DECSC/DECRC, SCOSC/SCORC) ----------------

    #[test]
    fn esc_7_8_saves_and_restores_cursor() {
        let mut g = Grid::new(20, 4);
        g.advance(b"line0\r\n");
        g.advance(b"\x1b7"); // DECSC at (1,0)
        g.advance(b"line1\r\nline2\r\n");
        g.advance(b"\x1b8"); // DECRC -> back to (1,0)
        g.advance(b"X");
        assert_eq!(g.row_text(1).trim_end(), "Xine1");
        assert_eq!(g.row_text(2).trim_end(), "line2");
    }

    #[test]
    fn csi_s_u_saves_and_restores_cursor() {
        let mut g = Grid::new(20, 4);
        g.advance(b"\x1b[2;3H"); // (1,2)
        g.advance(b"\x1b[s"); // SCOSC
        g.advance(b"\x1b[4;6H"); // move away to (3,5)
        g.advance(b"\x1b[u"); // SCORC -> back to (1,2)
        g.advance(b"Z");
        assert_eq!(at(&g, 1, 2), 'Z');
    }

    #[test]
    fn cursor_save_preserves_pen() {
        let mut g = Grid::new(20, 3);
        g.advance(b"\x1b[31m\x1b7"); // red pen, save
        g.advance(b"\x1b[0m\x1b[2;1H"); // reset pen, move away
        g.advance(b"\x1b8X"); // restore: pen should be red again
        assert_eq!(g.cell(0, 0).unwrap().fg, 2, "restored pen is red");
    }

    #[test]
    fn cursor_restore_without_save_homes() {
        let mut g = Grid::new(20, 3);
        g.advance(b"\x1b[3;5H"); // (2,4)
        g.advance(b"\x1b8"); // DECRC with no prior save
        assert_eq!(g.cursor(), (0, 0));
    }

    #[test]
    fn cursor_save_survives_alt_screen_round_trip() {
        // DECSC on the primary screen, an alt-screen excursion, then DECRC
        // back on the primary must still restore the primary save point.
        let mut g = Grid::new(20, 4);
        g.advance(b"\x1b[2;3H\x1b7"); // save at (1,2) on primary
        g.advance(b"\x1b[?1049h"); // alt screen
        g.advance(b"\x1b[u"); // a restore inside alt must not see primary save
        g.advance(b"alt");
        g.advance(b"\x1b[?1049l"); // back to primary
        g.advance(b"\x1b8Q"); // DECRC -> (1,2)
        assert_eq!(at(&g, 1, 2), 'Q');
    }

    #[test]
    fn claude_code_capture_replays_without_corruption() {
        // A real `claude` (Claude Code v2.1.143) TUI byte stream captured
        // through a PTY. Claude's Ink renderer redraws on the *primary*
        // screen (no alt-screen), so this is a non-corruption baseline: the
        // grid must contain the latest frame and must NOT show the startup
        // banner ("Claude Code v2.1.143" mascot box) superimposed on the
        // live conversation.
        let bytes = std::fs::read("tests/fixtures/claude_code_tui.vt")
            .expect("claude_code_tui.vt fixture present");
        let mut g = Grid::new(80, 24);
        g.advance(&bytes);

        let screen: Vec<String> = (0..24).map(|r| g.row_text(r)).collect();
        let joined = screen.join("\n");
        // The captured session ends on the input prompt + status footer.
        assert!(
            joined.contains("for shortcuts"),
            "final-frame footer present:\n{joined}"
        );
        // The mascot banner ("▐▛███▜▌") must not coexist with the live
        // conversation footer — that pairing is exactly the corruption.
        let has_banner = joined.contains("▐▛███▜▌") || joined.contains("v2.1.143");
        assert!(
            !has_banner,
            "startup banner must have scrolled off / been overwritten:\n{joined}"
        );
    }

    #[test]
    fn private_marker_csi_does_not_affect_grid() {
        // The kitty-keyboard protocol (`CSI < u`, `CSI > 1 u`) and XTMODKEYS
        // (`CSI > 4 m`) carry a `<`/`>`/`=` private marker. None touch the
        // cell grid. Before the marker check they were mis-dispatched: `CSI
        // < u` hit the SCO-restore path (homing the cursor) and `CSI > 4 m`
        // was misread as SGR. They must now be inert.
        let mut g = Grid::new(20, 5);
        g.advance(b"\x1b[3;5HX"); // cursor lands at (2,5) after the glyph
        let before = g.cursor();
        g.advance(b"\x1b[<u\x1b[>1u\x1b[<1u"); // kitty keyboard push/pop/query
        assert_eq!(g.cursor(), before, "kitty `CSI <|> u` must not move cursor");
        g.advance(b"\x1b[>4;2m\x1b[>4mY"); // XTMODKEYS must not be read as SGR
        let y = g.cell(before.0, before.1).unwrap();
        assert_eq!(y.ch, 'Y');
        assert_eq!(y.attrs, 0, "`CSI > 4 m` must not set SGR attrs");
        assert_eq!(y.fg, 0);
    }

    /// Regression: a real `claude` (Claude Code v2.1.143) Ink-TUI byte stream
    /// captured live through a 150x58 PTY — startup then quit. Claude renders
    /// with relative cursor moves only (`CUU`/`CUD`/`CUF`, `CR`, `LF`, `EL`),
    /// no absolute positioning and no alt-screen. The grid must reproduce
    /// `tmux`'s rendering of the same bytes: the mascot banner on rows 0-2,
    /// the input box (rules + prompt) on rows 4-6, and the quit/resume
    /// messages on their own separate rows 7-11 — never collapsed or
    /// overlapping.
    ///
    /// The corruption this guards against: `CSI < u` (kitty keyboard) was
    /// mis-dispatched to the SCO cursor-restore handler, homing the cursor
    /// mid-frame, so every subsequent relative move was anchored at row 0 and
    /// the whole screen collapsed onto ~4 overlapping top rows.
    #[test]
    fn live_main_replay_matches_tmux() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/live-main-startup.raw"
        );
        let bytes = std::fs::read(path).expect("live-main-startup.raw fixture present");
        let mut g = Grid::new(150, 58);
        g.advance(&bytes);

        // Expected non-blank rows, verified against `tmux` (-x 150 -y 58)
        // rendering the identical byte stream. Each entry is (row, prefix):
        // a prefix match keeps the test robust to trailing-blank padding and
        // to the full-width rule's exact length.
        let expect: &[(usize, &str)] = &[
            (0, " ▐▛███▜▌   Claude Code v2.1.143"),
            (1, "▝▜█████▛▘  Opus 4.7 with xhigh effort · Claude Max"),
            (2, "  ▘▘ ▝▝    /Users/stevek"),
            (4, "──────────────────────────────────────────"),
            (5, "❯"),
            (6, "──────────────────────────────────────────"),
            (7, "  Press Ctrl-C again to exit"),
            (9, "Resume this session with:"),
            (10, "claude --resume ddfb5a48-a860-49db-885e-433eb5cb4872"),
            (11, "^C"),
        ];
        for &(row, prefix) in expect {
            let text = g.row_text(row);
            assert!(
                text.trim_end().starts_with(prefix.trim_end()),
                "row {row} mismatch\n  expected prefix: {prefix:?}\n  got:             {:?}",
                text.trim_end()
            );
        }
        // Rows 4 and 6 are full-width rules of '─'.
        for &row in &[4usize, 6] {
            let text = g.row_text(row);
            assert!(
                text.trim_end().chars().all(|c| c == '─') && text.trim_end().chars().count() > 100,
                "row {row} must be a full-width rule, got {:?}",
                text.trim_end()
            );
        }
        // Rows that must stay blank — proof nothing collapsed onto them.
        for &row in &[3usize, 8, 12] {
            assert!(
                g.row_text(row).trim().is_empty(),
                "row {row} must be blank, got {:?}",
                g.row_text(row)
            );
        }
        // The startup banner must NOT coexist on the same row as the quit
        // footer — that overlap is the exact corruption being guarded.
        assert!(
            !g.row_text(0).contains("resume") && !g.row_text(0).contains("^C"),
            "banner row 0 must not carry footer content: {:?}",
            g.row_text(0)
        );
    }

    #[test]
    fn korean_wide_chars_render_without_gaps() {
        let mut g = Grid::new(40, 5);
        g.advance("한글 입력".as_bytes());
        assert_eq!(g.row_text(0).trim_end(), "한글 입력");
        // Lead cell holds the glyph; the trailing cell is the '\0' marker.
        assert_eq!(g.cell(0, 0).unwrap().ch, '한');
        assert_eq!(g.cell(0, 1).unwrap().ch, '\0');
        assert_eq!(g.cell(0, 2).unwrap().ch, '글');
        assert_eq!(g.cell(0, 3).unwrap().ch, '\0');
        assert_eq!(g.cell(0, 4).unwrap().ch, ' ');
        assert_eq!(g.cell(0, 5).unwrap().ch, '입');
        assert_eq!(g.cursor(), (0, 9));
    }
}
