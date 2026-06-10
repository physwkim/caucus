use super::*;

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
        // The `/` input line cannot outlive the pager that hosts it.
        self.focus.set_scroll_searching(false);
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
    use crate::input::CaucusCommand;
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
}
