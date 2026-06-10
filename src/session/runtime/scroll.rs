use super::*;
use crate::input::CopyMotion;

/// Open scrollback-pager state (`Ctrl-A [`): a *frozen* snapshot of one
/// panel's rendered scrollback plus the current scroll offset.
///
/// The panel keeps running underneath; the pager shows this snapshot until
/// closed (tmux copy-mode behavior — new output appears only after exit).
/// Built by [`Multiplexer::enter_scroll`]; fields are `pub(crate)` so the
/// `render` layer can window the lines without a getter per field.
pub(crate) struct ScrollState {
    /// Role of the snapshotted panel, for the pager header.
    pub(crate) role: String,
    /// Rendered scrollback + live viewport, one entry per line, oldest first.
    pub(crate) lines: Vec<String>,
    /// Index of the topmost visible line — `0` is the oldest.
    pub(crate) offset: usize,
    /// Visible body height in rows (the page step), set at entry. Also the
    /// clamp window: the maximum offset is `lines.len() - page`.
    pub(crate) page: usize,
    /// Incremental `/` search over `lines` (`docs/design.md` §1).
    pub(crate) search: PagerSearch,
    /// Line-selection copy mode over `lines` (`v`; `docs/design.md` §1).
    pub(crate) copy: CopyMode,
}

/// Copy-mode state inside the pager (`v` then move + `y`).
///
/// A vim visual-line selection: `anchor` is the line where the selection began
/// (fixed), `cursor` is the moving end. The selected range is the inclusive
/// span between them ([`CopyMode::selection`]). Both index into [`ScrollState`]
/// `lines` and are meaningful only while `active`.
#[derive(Default)]
pub(crate) struct CopyMode {
    /// Whether a line selection is in progress.
    pub(crate) active: bool,
    /// The fixed end of the selection, set when copy mode opened.
    pub(crate) anchor: usize,
    /// The moving end of the selection — the cursor the user steers.
    pub(crate) cursor: usize,
}

impl CopyMode {
    /// The inclusive selected line range `[lo, hi]`, ordering the anchor and
    /// cursor so it holds whichever way the cursor was moved. The single
    /// definition of the selection span, shared by the yank and the renderer.
    pub(crate) fn selection(&self) -> (usize, usize) {
        (self.anchor.min(self.cursor), self.anchor.max(self.cursor))
    }
}

/// Incremental-search state inside the pager (`/` then `n`/`N`).
///
/// Two distinct strings by design: `input` is the live `/` line being typed,
/// `query` is the committed term that `matches` / `n` / `N` act on. Keeping them
/// separate means cancelling an edit (`Esc`) leaves the prior committed search
/// intact instead of half-overwriting it.
#[derive(Default)]
pub(crate) struct PagerSearch {
    /// Whether the `/` input line is open — keystrokes edit `input`.
    pub(crate) editing: bool,
    /// The `/` line being typed; committed into `query` on Enter.
    pub(crate) input: String,
    /// The committed search term (empty = no active search).
    pub(crate) query: String,
    /// Line indices (into `lines`) containing `query`, ascending.
    pub(crate) matches: Vec<usize>,
    /// Index into `matches` of the current match (meaningless when empty).
    pub(crate) current: usize,
}

impl ScrollState {
    /// Single construction site for a pager snapshot — always starts with no
    /// active search.
    pub(crate) fn new(role: String, lines: Vec<String>, offset: usize, page: usize) -> Self {
        Self {
            role,
            lines,
            offset,
            page,
            search: PagerSearch::default(),
            copy: CopyMode::default(),
        }
    }

    /// Recompute `matches` (line indices containing `query`, case-insensitive)
    /// and reset `current` to the first.
    fn recompute_matches(&mut self) {
        self.search.matches.clear();
        self.search.current = 0;
        if self.search.query.is_empty() {
            return;
        }
        let needle = self.search.query.to_lowercase();
        for (i, line) in self.lines.iter().enumerate() {
            if line.to_lowercase().contains(&needle) {
                self.search.matches.push(i);
            }
        }
    }

    /// Scroll so the current match line sits at the window top, clamped to the
    /// last page so it is always visible.
    fn jump_to_current_match(&mut self) {
        if let Some(&line) = self.search.matches.get(self.search.current) {
            let max = self.lines.len().saturating_sub(self.page);
            self.offset = line.min(max);
        }
    }

    /// Step `delta` matches with wraparound, then scroll to the new current.
    fn step_match(&mut self, delta: isize) {
        let n = self.search.matches.len();
        if n == 0 {
            return;
        }
        self.search.current =
            (self.search.current as isize + delta).rem_euclid(n as isize) as usize;
        self.jump_to_current_match();
    }

    /// Scroll the window so the copy-mode cursor stays visible: pull the top
    /// down to the cursor when it rose above the view, push it up when the
    /// cursor fell below, then clamp to the last page. No-op when the cursor is
    /// already on screen.
    fn scroll_to_cursor(&mut self) {
        let page = self.page.max(1);
        if self.copy.cursor < self.offset {
            self.offset = self.copy.cursor;
        } else if self.copy.cursor >= self.offset + page {
            self.offset = self.copy.cursor + 1 - page;
        }
        let max = self.lines.len().saturating_sub(self.page);
        self.offset = self.offset.min(max);
    }

    /// Move the copy-mode cursor per `motion` (clamped to the buffer), then
    /// scroll it into view. A no-op when copy mode is inactive or the buffer is
    /// empty.
    fn copy_move(&mut self, motion: CopyMotion) {
        if !self.copy.active || self.lines.is_empty() {
            return;
        }
        let last = (self.lines.len() - 1) as isize;
        let page = self.page.max(1) as isize;
        let cur = self.copy.cursor as isize;
        let target = match motion {
            CopyMotion::Up => cur - 1,
            CopyMotion::Down => cur + 1,
            CopyMotion::PageUp => cur - page,
            CopyMotion::PageDown => cur + page,
            CopyMotion::Top => 0,
            CopyMotion::Bottom => last,
        };
        self.copy.cursor = target.clamp(0, last) as usize;
        self.scroll_to_cursor();
    }
}

impl Multiplexer {
    /// The open scrollback pager, if any — for the draw layer (`tui::draw`).
    /// `pub(crate)`: [`ScrollState`] is an internal type, consumed only in-crate.
    pub(crate) fn scroll_state(&self) -> Option<&ScrollState> {
        self.scroll.as_ref()
    }

    /// Open the scrollback pager on the focused panel (`Ctrl-A [`): snapshot
    /// its rendered scrollback and freeze it for scrolling. A no-op when no
    /// panel is focused (or the focused id no longer resolves). Opening the
    /// pager supersedes the transcript overlay.
    pub(crate) fn enter_scroll(&mut self) {
        let Some(focused) = self.focus.focused() else {
            return;
        };
        let Some(panel) = self.panels.iter().find(|p| p.id == focused) else {
            return;
        };
        let role = panel.role.clone();
        let lines: Vec<String> = Self::scrollback_text(panel)
            .lines()
            .map(str::to_string)
            .collect();
        let page = pager_page_height(self.area);
        // Open at the bottom (newest), like tmux copy-mode entry.
        let offset = lines.len().saturating_sub(page);
        self.show_transcript = false;
        self.focus.set_transcript_open(false);
        self.scroll = Some(ScrollState::new(role, lines, offset, page));
        self.focus.set_scroll_open(true);
    }

    /// Close the scrollback pager, returning to the live tiled view.
    pub(crate) fn exit_scroll(&mut self) {
        self.scroll = None;
        self.focus.set_scroll_open(false);
        // Neither the `/` input line nor copy mode can outlive the pager that
        // hosts them.
        self.focus.set_scroll_searching(false);
        self.focus.set_scroll_copying(false);
    }

    /// Re-sync an open pager's page height to the current [`Self::area`].
    ///
    /// `page` is the scroll clamp window and the page-step. The renderer windows
    /// the snapshot to the *live* area height (`draw_scroll_pager`), so a page
    /// frozen at [`Self::enter_scroll`] desyncs scrolling from what is drawn the
    /// moment the terminal resizes — a shrunk terminal makes the newest lines
    /// unreachable, a grown one scrolls past the end into blank space. Called by
    /// [`Multiplexer::resize`], the single owner of `area`, so the invariant
    /// `page == pager_page_height(area)` holds for the whole open lifetime.
    ///
    /// The offset is re-clamped to the new last page: a grown terminal shrinks
    /// the max offset, so a previously-valid offset could otherwise sit past it.
    pub(crate) fn resync_pager_page(&mut self) {
        let page = pager_page_height(self.area);
        if let Some(state) = self.scroll.as_mut() {
            state.page = page;
            let max = state.lines.len().saturating_sub(page);
            state.offset = state.offset.min(max);
        }
    }

    /// Scroll the pager by `delta` lines (negative = toward older output),
    /// clamped to `[0, lines.len() - page]`. No-op when the pager is closed.
    pub(crate) fn scroll_by(&mut self, delta: isize) {
        if let Some(state) = self.scroll.as_mut() {
            let max = state.lines.len().saturating_sub(state.page) as isize;
            state.offset = (state.offset as isize + delta).clamp(0, max) as usize;
        }
    }

    /// Scroll the pager by `pages` pages (negative = toward older output).
    pub(crate) fn scroll_page(&mut self, pages: isize) {
        let step = self.scroll.as_ref().map_or(0, |s| s.page as isize);
        self.scroll_by(pages * step);
    }

    /// Jump the pager to the oldest line (`top`) or the newest (`!top`).
    pub(crate) fn scroll_to_edge(&mut self, top: bool) {
        if let Some(state) = self.scroll.as_mut() {
            state.offset = if top {
                0
            } else {
                state.lines.len().saturating_sub(state.page)
            };
        }
    }

    /// Open the `/` search-input line in the pager (`docs/design.md` §1).
    /// Subsequent keystrokes build the query until `Enter` commits or `Esc`
    /// cancels. The router is told so it routes those keys to the input line
    /// rather than the pager's navigation bindings.
    pub(crate) fn search_start(&mut self) {
        if let Some(state) = self.scroll.as_mut() {
            state.search.editing = true;
            state.search.input.clear();
            self.focus.set_scroll_searching(true);
        }
    }

    /// Append a char to the open `/` input line.
    pub(crate) fn search_input(&mut self, c: char) {
        if let Some(state) = self.scroll.as_mut()
            && state.search.editing
        {
            state.search.input.push(c);
        }
    }

    /// Delete the last char of the `/` input line.
    pub(crate) fn search_backspace(&mut self) {
        if let Some(state) = self.scroll.as_mut()
            && state.search.editing
        {
            state.search.input.pop();
        }
    }

    /// Commit the `/` input line: it becomes the active query, matches are
    /// recomputed, and the pager jumps to the first match at or after the
    /// current top (wrapping to the first overall). An empty input clears any
    /// active search and leaves the offset put.
    pub(crate) fn search_commit(&mut self) {
        self.focus.set_scroll_searching(false);
        let Some(state) = self.scroll.as_mut() else {
            return;
        };
        state.search.editing = false;
        state.search.query = std::mem::take(&mut state.search.input);
        state.recompute_matches();
        if !state.search.matches.is_empty() {
            let start = state.offset;
            state.search.current = state
                .search
                .matches
                .iter()
                .position(|&m| m >= start)
                .unwrap_or(0);
            state.jump_to_current_match();
        }
    }

    /// Cancel the `/` input line, keeping any previously committed search.
    pub(crate) fn search_cancel(&mut self) {
        self.focus.set_scroll_searching(false);
        if let Some(state) = self.scroll.as_mut() {
            state.search.editing = false;
            state.search.input.clear();
        }
    }

    /// Step to the next match (`n`), wrapping. A no-op without an active search.
    pub(crate) fn search_next(&mut self) {
        if let Some(state) = self.scroll.as_mut() {
            state.step_match(1);
        }
    }

    /// Step to the previous match (`N`), wrapping.
    pub(crate) fn search_prev(&mut self) {
        if let Some(state) = self.scroll.as_mut() {
            state.step_match(-1);
        }
    }

    /// Enter the pager's copy mode (`v`): drop the selection anchor and cursor
    /// on the top visible line. A no-op when the pager is closed or its
    /// snapshot is empty (nothing to select). The router is told so navigation
    /// keys steer the selection instead of scrolling the window.
    pub(crate) fn copy_start(&mut self) {
        let Some(state) = self.scroll.as_mut() else {
            return;
        };
        if state.lines.is_empty() {
            return;
        }
        let cursor = state.offset.min(state.lines.len() - 1);
        state.copy.active = true;
        state.copy.anchor = cursor;
        state.copy.cursor = cursor;
        self.focus.set_scroll_copying(true);
    }

    /// Move the copy-mode cursor per `motion`, extending the selection.
    pub(crate) fn copy_move(&mut self, motion: CopyMotion) {
        if let Some(state) = self.scroll.as_mut() {
            state.copy_move(motion);
        }
    }

    /// Cancel copy mode (`Esc`), keeping the pager open. Nothing is copied.
    pub(crate) fn copy_cancel(&mut self) {
        self.focus.set_scroll_copying(false);
        if let Some(state) = self.scroll.as_mut() {
            state.copy.active = false;
        }
    }

    /// Copy the selected lines to the host clipboard (`y`/`Enter`): join the
    /// inclusive selection with newlines and queue an OSC 52 set-clipboard
    /// sequence for the event loop to write, then leave copy mode. A no-op when
    /// copy mode is not active.
    ///
    /// OSC 52 is dependency-free and travels over SSH (the terminal, not
    /// caucus, owns the clipboard), at the cost of needing terminal support —
    /// terminals without it silently drop the sequence.
    pub(crate) fn copy_yank(&mut self) {
        self.focus.set_scroll_copying(false);
        let Some(state) = self.scroll.as_mut() else {
            return;
        };
        if !state.copy.active {
            return;
        }
        let (lo, hi) = state.copy.selection();
        let text = state.lines[lo..=hi].join("\n");
        state.copy.active = false;
        self.pending_clipboard = Some(osc52_set_clipboard(&text));
    }

    /// Take the OSC 52 set-clipboard sequence a copy-mode yank queued, if any —
    /// drained and written to the host terminal by the event loop. Leaves
    /// `None` behind so each yank is emitted exactly once.
    pub fn take_pending_clipboard(&mut self) -> Option<String> {
        self.pending_clipboard.take()
    }
}

/// Maximum decoded bytes carried in one OSC 52 set-clipboard sequence.
///
/// Terminals cap the length of an OSC string and *silently drop the whole
/// sequence* once it is exceeded (xterm's limit is configurable; many fixed
/// implementations sit near 100 KB for the entire escape), so an unbounded yank
/// of a huge selection would copy nothing at all. Bounding the payload keeps the
/// emitted `ESC ] 52 ; c ;` + base64 + BEL — base64 inflates the text by 4/3 —
/// under that budget: 72 KiB of text encodes to 96 KiB of base64, comfortably
/// under 100 KB. A larger selection lands a truncated prefix on the clipboard
/// rather than vanishing.
const OSC52_MAX_TEXT_BYTES: usize = 72 * 1024;

/// Wrap `text` in an OSC 52 set-clipboard escape sequence targeting the system
/// clipboard (`c`): `ESC ] 52 ; c ; <base64> BEL`. The host terminal — not
/// caucus — applies it, so it works over SSH; terminals without OSC 52 support
/// silently ignore it.
///
/// The single owner of OSC 52 emission, so the payload bound
/// ([`OSC52_MAX_TEXT_BYTES`]) is enforced here by construction: no caller can
/// emit a sequence the terminal would reject wholesale.
fn osc52_set_clipboard(text: &str) -> String {
    format!(
        "\x1b]52;c;{}\x07",
        base64_encode(bound_clipboard_text(text).as_bytes())
    )
}

/// Bound `text` to [`OSC52_MAX_TEXT_BYTES`], truncating on a UTF-8 char boundary
/// so the base64 payload never encodes a split character. Returns the input
/// unchanged when it already fits.
fn bound_clipboard_text(text: &str) -> &str {
    if text.len() <= OSC52_MAX_TEXT_BYTES {
        return text;
    }
    let mut end = OSC52_MAX_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Standard base64 (RFC 4648, `+`/`/` alphabet, `=` padding). Hand-rolled to
/// keep the clipboard path dependency-free (no `base64` crate pulled in for one
/// call site), mirroring the no-new-dep stance elsewhere.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Visible body rows in the scrollback pager for a multiplexer body area.
///
/// [`Multiplexer::area`] excludes the one-line status bar, while
/// `draw_scroll_pager` receives the full terminal frame, insets the popup by
/// two rows at the top/bottom, then subtracts the border. Therefore:
/// `(area.height + status) - popup_inset(4) - border(2) = area.height - 5`.
fn pager_page_height(area: Rect) -> usize {
    (area.height as usize).saturating_sub(5).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{CaucusCommand, CopyMotion};
    use crate::session::runtime::test_support::*;
    use tempfile::TempDir;

    /// `EnterScroll` with no focused panel is a no-op (no pager, no panic).
    #[tokio::test]
    async fn enter_scroll_with_no_focus_is_a_noop() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.apply_command(CaucusCommand::EnterScroll);
        assert!(mux.scroll_state().is_none());
    }

    /// Inject a known pager state directly (no PTY needed) and prove the offset
    /// clamps at both ends — per-boundary, not per-scenario.
    #[tokio::test]
    async fn scroll_offset_clamps_at_both_ends() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        // 10 lines, page of 4 → max offset = 6. Start mid-buffer.
        let lines: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
        mux.scroll = Some(ScrollState::new("worker".to_string(), lines, 3, 4));

        mux.apply_command(CaucusCommand::ScrollUp);
        assert_eq!(mux.scroll_state().unwrap().offset, 2);
        mux.apply_command(CaucusCommand::ScrollDown);
        assert_eq!(mux.scroll_state().unwrap().offset, 3);

        // Top edge: never below 0.
        mux.apply_command(CaucusCommand::ScrollTop);
        assert_eq!(mux.scroll_state().unwrap().offset, 0);
        mux.apply_command(CaucusCommand::ScrollUp);
        assert_eq!(mux.scroll_state().unwrap().offset, 0);
        mux.apply_command(CaucusCommand::ScrollPageUp);
        assert_eq!(mux.scroll_state().unwrap().offset, 0);

        // Bottom edge: never past lines.len() - page (= 6).
        mux.apply_command(CaucusCommand::ScrollBottom);
        assert_eq!(mux.scroll_state().unwrap().offset, 6);
        mux.apply_command(CaucusCommand::ScrollDown);
        assert_eq!(mux.scroll_state().unwrap().offset, 6);
        mux.apply_command(CaucusCommand::ScrollPageDown);
        assert_eq!(mux.scroll_state().unwrap().offset, 6);

        // One page up from the bottom lands a page (4) earlier.
        mux.apply_command(CaucusCommand::ScrollPageUp);
        assert_eq!(mux.scroll_state().unwrap().offset, 2);
    }

    /// A buffer shorter than a page pins the offset at 0 (max = 0).
    #[tokio::test]
    async fn scroll_offset_pins_at_zero_when_buffer_shorter_than_page() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.scroll = Some(ScrollState::new(
            "worker".to_string(),
            vec!["only line".to_string()],
            0,
            4,
        ));
        mux.apply_command(CaucusCommand::ScrollBottom);
        assert_eq!(mux.scroll_state().unwrap().offset, 0);
        mux.apply_command(CaucusCommand::ScrollDown);
        assert_eq!(mux.scroll_state().unwrap().offset, 0);
    }

    /// The pager opens at the bottom by computing `offset = lines.len() -
    /// page`; `page` must match the renderer's actual inner popup height. The
    /// multiplexer area excludes the status row, while rendering insets the
    /// full frame by two rows and subtracts a border: body height 39 (40-row
    /// terminal minus status) leaves 34 visible pager rows.
    #[test]
    fn pager_page_height_matches_the_rendered_popup_body() {
        assert_eq!(
            pager_page_height(Rect {
                x: 0,
                y: 0,
                width: 120,
                height: 39,
            }),
            34
        );
    }

    /// On a terminal resize the open pager's `page` (the scroll clamp + step)
    /// must track the new area: the renderer windows to the live height, so a
    /// page frozen at entry makes the newest lines unreachable when the terminal
    /// shrinks and scrolls past the end when it grows. The offset is re-clamped
    /// to the new last page.
    #[tokio::test]
    async fn resize_resyncs_the_open_pager_page_and_clamps_offset() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        // 60 lines; open the pager scrolled to the bottom for the initial area.
        let lines: Vec<String> = (0..60).map(|i| format!("line {i}")).collect();
        let start = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 25,
        }; // page = 20
        mux.area = start;
        let start_page = pager_page_height(start);
        assert_eq!(start_page, 20);
        mux.scroll = Some(ScrollState::new(
            "w".to_string(),
            lines,
            60 - start_page, // offset 40 — the bottom
            start_page,
        ));

        // Shrink: page must shrink with the area so the clamp and the live
        // render agree (max offset grows, newest lines stay reachable).
        let small = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 12,
        }; // page = 7
        mux.resize(small).unwrap();
        assert_eq!(
            mux.scroll_state().unwrap().page,
            7,
            "page tracks the shrunk area"
        );

        // Grow: page grows, so the max offset shrinks (60 - 35 = 25); the
        // bottom-parked offset 40 is clamped back to 25, not left in blank space.
        let big = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 40,
        }; // page = 35
        mux.resize(big).unwrap();
        let st = mux.scroll_state().unwrap();
        assert_eq!(st.page, 35, "page tracks the grown area");
        assert_eq!(
            st.offset, 25,
            "offset re-clamped to the new last page (60 - 35)"
        );
    }

    /// `ExitScroll` clears the pager state.
    #[tokio::test]
    async fn exit_scroll_clears_the_pager() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.scroll = Some(ScrollState::new(
            "worker".to_string(),
            vec!["a".to_string(), "b".to_string()],
            0,
            1,
        ));
        mux.apply_command(CaucusCommand::ExitScroll);
        assert!(mux.scroll_state().is_none());
    }

    /// `EnterScroll` snapshots the focused panel and opens at the bottom
    /// (newest). CLI-gated: spawning a panel needs a real agent CLI.
    #[tokio::test]
    async fn enter_scroll_snapshots_the_focused_panel() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let Ok(_id) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        // Spawning the first panel auto-focuses it.
        mux.apply_command(CaucusCommand::EnterScroll);
        let state = mux.scroll_state().expect("pager open after EnterScroll");
        assert_eq!(state.role, "reviewer");
        // Opened at the bottom: offset is the clamped maximum.
        assert_eq!(state.offset, state.lines.len().saturating_sub(state.page));

        mux.shutdown();
    }

    /// Typing `/find` then Enter through the full key path: the router diverts
    /// the keystrokes into the query while the `/` line is open (even `f`/`i`,
    /// which would otherwise be plain chars swallowed by the pager), commits,
    /// and jumps to the first match.
    #[tokio::test]
    async fn pager_search_via_handle_key_routes_input_to_the_query() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.scroll = Some(ScrollState::new(
            "w".to_string(),
            vec![
                "alpha".to_string(),
                "find me".to_string(),
                "beta find".to_string(),
                "gamma".to_string(),
            ],
            0,
            2,
        ));
        mux.focus.set_scroll_open(true);

        let ch = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        mux.handle_key(ch('/'));
        for c in ['f', 'i', 'n', 'd'] {
            mux.handle_key(ch(c));
        }
        mux.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let s = mux.scroll_state().unwrap();
        assert_eq!(s.search.query, "find");
        assert_eq!(s.search.matches, vec![1, 2], "lines containing 'find'");
        assert_eq!(s.offset, 1, "jumped to the first match");
        assert!(
            !s.search.editing,
            "Enter committed and closed the input line"
        );
    }

    /// Per-boundary search navigation: a committed query finds case-insensitive
    /// matches, `n`/`N` step and wrap, cancel keeps the prior query, and an
    /// empty commit clears the search.
    #[tokio::test]
    async fn pager_search_steps_wraps_and_clears() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        // "needle" sits at lines 1, 4, 8 of 12; page of 3 → max offset 9.
        let lines: Vec<String> = (0..12)
            .map(|i| {
                if [1usize, 4, 8].contains(&i) {
                    format!("row {i} needle")
                } else {
                    format!("row {i}")
                }
            })
            .collect();
        mux.scroll = Some(ScrollState::new("w".to_string(), lines, 0, 3));

        // Search "NEEDLE" (upper) against lowercase lines → case-insensitive.
        mux.apply_command(CaucusCommand::SearchStart);
        for c in "NEEDLE".chars() {
            mux.apply_command(CaucusCommand::SearchInput(c));
        }
        mux.apply_command(CaucusCommand::SearchCommit);
        let s = mux.scroll_state().unwrap();
        assert_eq!(s.search.query, "NEEDLE");
        assert_eq!(s.search.matches, vec![1, 4, 8]);
        assert_eq!(s.offset, 1, "jumped to the first match");

        mux.apply_command(CaucusCommand::SearchNext);
        assert_eq!(mux.scroll_state().unwrap().offset, 4);
        mux.apply_command(CaucusCommand::SearchNext);
        assert_eq!(mux.scroll_state().unwrap().offset, 8);
        // `n` past the last wraps to the first.
        mux.apply_command(CaucusCommand::SearchNext);
        assert_eq!(mux.scroll_state().unwrap().search.current, 0);
        assert_eq!(mux.scroll_state().unwrap().offset, 1);
        // `N` from the first wraps to the last.
        mux.apply_command(CaucusCommand::SearchPrev);
        assert_eq!(mux.scroll_state().unwrap().search.current, 2);
        assert_eq!(mux.scroll_state().unwrap().offset, 8);

        // Editing then cancelling keeps the committed query and clears the line.
        mux.apply_command(CaucusCommand::SearchStart);
        mux.apply_command(CaucusCommand::SearchInput('z'));
        mux.apply_command(CaucusCommand::SearchCancel);
        let s = mux.scroll_state().unwrap();
        assert_eq!(
            s.search.query, "NEEDLE",
            "cancel preserves the committed query"
        );
        assert!(s.search.input.is_empty());
        assert!(!s.search.editing);

        // An empty commit clears the active search.
        mux.apply_command(CaucusCommand::SearchStart);
        mux.apply_command(CaucusCommand::SearchCommit);
        let s = mux.scroll_state().unwrap();
        assert!(s.search.query.is_empty());
        assert!(s.search.matches.is_empty());
    }

    /// Copy mode selects an inclusive line range and yanks it to the clipboard
    /// as an OSC 52 sequence, then exits — leaving no further pending payload.
    #[tokio::test]
    async fn copy_mode_selects_a_range_and_yanks_to_the_clipboard() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let lines = vec![
            "alpha".to_string(),
            "beta".to_string(),
            "gamma".to_string(),
            "delta".to_string(),
        ];
        mux.scroll = Some(ScrollState::new("w".to_string(), lines, 0, 2));

        // `v` drops the anchor + cursor on the top line; one `j` extends down.
        mux.apply_command(CaucusCommand::CopyStart);
        mux.apply_command(CaucusCommand::CopyMove(CopyMotion::Down));
        let s = mux.scroll_state().unwrap();
        assert!(s.copy.active);
        assert_eq!(s.copy.selection(), (0, 1), "anchor 0, cursor 1");

        // `y` copies the inclusive range joined by newlines and leaves copy mode.
        mux.apply_command(CaucusCommand::CopyYank);
        assert!(
            !mux.scroll_state().unwrap().copy.active,
            "yank exits copy mode"
        );
        assert_eq!(
            mux.take_pending_clipboard(),
            Some(osc52_set_clipboard("alpha\nbeta")),
            "the two selected lines are queued as one OSC 52 payload"
        );
        // Drained exactly once.
        assert_eq!(mux.take_pending_clipboard(), None);
    }

    /// Per-boundary cursor navigation: the cursor clamps to the buffer ends and
    /// the window scrolls to keep it visible.
    #[tokio::test]
    async fn copy_cursor_follows_the_window_and_clamps() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        // 12 lines, page 3 → max offset 9.
        let lines: Vec<String> = (0..12).map(|i| format!("l{i}")).collect();
        mux.scroll = Some(ScrollState::new("w".to_string(), lines, 0, 3));

        mux.apply_command(CaucusCommand::CopyStart);
        assert_eq!(mux.scroll_state().unwrap().copy.cursor, 0);

        // Stepping the cursor below the page pulls the window down to follow.
        for _ in 0..3 {
            mux.apply_command(CaucusCommand::CopyMove(CopyMotion::Down));
        }
        let s = mux.scroll_state().unwrap();
        assert_eq!(s.copy.cursor, 3);
        assert_eq!(s.offset, 1, "window scrolled so the cursor stays visible");

        // Jump to the bottom: cursor clamps at the last line, window at max.
        mux.apply_command(CaucusCommand::CopyMove(CopyMotion::Bottom));
        let s = mux.scroll_state().unwrap();
        assert_eq!(s.copy.cursor, 11);
        assert_eq!(s.offset, 9);
        // Down past the last line is a no-op (clamped).
        mux.apply_command(CaucusCommand::CopyMove(CopyMotion::Down));
        assert_eq!(mux.scroll_state().unwrap().copy.cursor, 11);

        // Jump to the top: cursor 0, window back to 0.
        mux.apply_command(CaucusCommand::CopyMove(CopyMotion::Top));
        let s = mux.scroll_state().unwrap();
        assert_eq!(s.copy.cursor, 0);
        assert_eq!(s.offset, 0);
    }

    /// Cancelling copy mode copies nothing and leaves no pending payload.
    #[tokio::test]
    async fn copy_cancel_yanks_nothing() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let lines: Vec<String> = (0..4).map(|i| format!("l{i}")).collect();
        mux.scroll = Some(ScrollState::new("w".to_string(), lines, 0, 2));

        mux.apply_command(CaucusCommand::CopyStart);
        mux.apply_command(CaucusCommand::CopyMove(CopyMotion::Down));
        mux.apply_command(CaucusCommand::CopyCancel);

        assert!(!mux.scroll_state().unwrap().copy.active);
        assert_eq!(mux.take_pending_clipboard(), None, "cancel copies nothing");
    }

    /// `v` on an empty snapshot is a no-op — there is nothing to select.
    #[tokio::test]
    async fn copy_start_on_empty_buffer_is_a_noop() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.scroll = Some(ScrollState::new("w".to_string(), vec![], 0, 4));
        mux.apply_command(CaucusCommand::CopyStart);
        assert!(!mux.scroll_state().unwrap().copy.active);
    }

    /// Exiting the pager tears down an in-progress copy mode.
    #[tokio::test]
    async fn exit_scroll_clears_copy_mode() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let lines: Vec<String> = (0..4).map(|i| format!("l{i}")).collect();
        mux.scroll = Some(ScrollState::new("w".to_string(), lines, 0, 2));
        mux.apply_command(CaucusCommand::CopyStart);
        assert!(mux.scroll_state().unwrap().copy.active);

        mux.apply_command(CaucusCommand::ExitScroll);
        assert!(mux.scroll_state().is_none());
    }

    #[test]
    fn base64_encode_matches_known_vectors() {
        // RFC 4648 §10 vectors plus the padding boundaries.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"M"), "TQ==");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn osc52_wraps_the_base64_payload() {
        // ESC ] 52 ; c ; <base64> BEL targeting the system clipboard.
        assert_eq!(osc52_set_clipboard("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn osc52_caps_an_oversized_payload() {
        // A selection past the terminal's OSC-string cap is bounded so the
        // sequence is still emitted, not silently dropped whole by the terminal.
        let big = "x".repeat(OSC52_MAX_TEXT_BYTES + 5000);
        let seq = osc52_set_clipboard(&big);
        let b64 = seq
            .strip_prefix("\x1b]52;c;")
            .and_then(|s| s.strip_suffix('\x07'))
            .unwrap();
        // base64 length is ceil(capped/3)*4; the cap is a multiple of 3.
        assert_eq!(b64.len(), OSC52_MAX_TEXT_BYTES.div_ceil(3) * 4);
    }

    #[test]
    fn osc52_truncates_on_a_char_boundary() {
        // The cap landing inside a multi-byte char must back up to a boundary so
        // the base64 never encodes a split character.
        let mut s = "a".repeat(OSC52_MAX_TEXT_BYTES - 1);
        s.push('é'); // 2 bytes → the byte at the cap is a continuation byte
        let bounded = bound_clipboard_text(&s);
        assert!(std::str::from_utf8(bounded.as_bytes()).is_ok());
        assert_eq!(
            bounded.len(),
            OSC52_MAX_TEXT_BYTES - 1,
            "backed up off the split char rather than cutting through it"
        );
    }
}
