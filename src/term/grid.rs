//! vte-backed screen grid (`docs/design.md` §0 #3, §8.5).
//!
//! A panel's terminal screen: a cell matrix viewport plus a bounded scrollback
//! ring. Bytes flow in from the panel PTY; the grid is the [`vte::Perform`]
//! sink that interprets escape sequences into cell state.
//!
//! **Invariant** (`docs/design.md` §9.1): the grid is mutated only by PTY
//! bytes fed through [`Grid::advance`]. No module pokes cells directly.
//!
//! The real `Perform` implementation (~2-4k LOC, zellij `grid.rs` as a
//! line-by-line reference) is Phase 2; this skeleton compiles a stub.

use vte::{Params, Parser, Perform};

/// One terminal cell: a glyph plus its rendition attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// The displayed character. Space for an empty cell.
    pub ch: char,
    /// Packed SGR foreground colour index (0 = default). Phase 2 widens this.
    pub fg: u8,
    /// Packed SGR background colour index (0 = default).
    pub bg: u8,
    /// Bold / underline / reverse etc., packed as a bitflag byte.
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
}

impl Grid {
    /// Default scrollback depth (rows).
    pub const DEFAULT_SCROLLBACK: usize = 10_000;

    /// Build a blank grid sized `cols x rows`.
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            cols,
            rows,
            viewport: vec![Cell::default(); cols * rows],
            scrollback: std::collections::VecDeque::new(),
            scrollback_limit: Self::DEFAULT_SCROLLBACK,
            cursor: (0, 0),
            parser: Parser::new(),
        }
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

    /// Feed a chunk of PTY output bytes through the vte parser.
    ///
    /// The only sanctioned way to mutate grid state.
    pub(crate) fn advance(&mut self, bytes: &[u8]) {
        // `Parser` and the `Perform` sink can't both be `&mut self` at once,
        // so swap the parser out, drive it, swap it back.
        let mut parser = std::mem::take(&mut self.parser);
        parser.advance(self, bytes);
        self.parser = parser;
    }

    /// Resize the viewport. Phase 2 reflows wrapped lines; the skeleton just
    /// reallocates a blank matrix.
    pub(crate) fn resize(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        self.viewport = vec![Cell::default(); cols * rows];
        self.cursor = (0, 0);
    }
}

/// Stub `vte::Perform` implementation. Compiles and accepts the full callback
/// surface; the real cell mutations are Phase 2 (zellij `grid.rs` reference).
impl Perform for Grid {
    fn print(&mut self, _c: char) {
        // TODO(phase 2): write `_c` at the cursor, advance, wrap, scroll.
    }

    fn execute(&mut self, _byte: u8) {
        // TODO(phase 2): handle C0 controls (LF, CR, BS, HT, ...).
    }

    fn csi_dispatch(
        &mut self,
        _params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        _action: char,
    ) {
        // TODO(phase 2): cursor moves, erase, SGR rendition, scroll regions.
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        // TODO(phase 2): index, reverse index, charset selection.
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // TODO(phase 2): title, hyperlinks, clipboard.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
