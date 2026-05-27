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
        self.scroll = Some(ScrollState {
            role,
            lines,
            offset,
            page,
        });
        self.focus.set_scroll_open(true);
    }

    /// Close the scrollback pager, returning to the live tiled view.
    pub(crate) fn exit_scroll(&mut self) {
        self.scroll = None;
        self.focus.set_scroll_open(false);
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
        mux.scroll = Some(ScrollState {
            role: "worker".to_string(),
            lines,
            offset: 3,
            page: 4,
        });

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
        mux.scroll = Some(ScrollState {
            role: "worker".to_string(),
            lines: vec!["only line".to_string()],
            offset: 0,
            page: 4,
        });
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
        mux.scroll = Some(ScrollState {
            role: "worker".to_string(),
            lines: vec!["a".to_string(), "b".to_string()],
            offset: 0,
            page: 1,
        });
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
}
