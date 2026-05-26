//! Render layer: ratatui panel layout, reflow, drawing, focus indication.
//! See `docs/design.md` §0 #3, §9.
//!
//! Panels are dynamic (`docs/design.md` §0 #10): the layout reflows whenever
//! the main worker spawns or kills a panel. Two pieces live here:
//!
//! * [`Layout::reflow`] — a pure tiling computation: given a screen area and
//!   N panel ids, assign each a non-overlapping [`Rect`]. No ratatui types,
//!   so it is trivially unit-testable (see `tests`).
//! * [`draw`] — paints every panel's [`Grid`] viewport into its rect on a
//!   ratatui [`Frame`], with a titled border (role + derived state) and a
//!   focus highlight.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::Rect as TuiRect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::agent::derive_state::DerivedState;
use crate::agent::lane_event::LaneEventKind;
use crate::agent::manifest::AgentManifest;
use crate::panel::Panel;
use crate::session::id::PanelId;
use crate::session::runtime::ScrollState;
use crate::term::Grid;
use crate::term::grid::attr;

mod tree;
pub use tree::LayoutTree;

/// A rectangle of the terminal, in cells.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    /// The interior of this rect once a single-cell border is drawn — the
    /// area a panel's grid viewport actually occupies. Saturating, so a rect
    /// too small for a border yields a zero-sized interior rather than
    /// underflowing.
    pub fn inner(&self) -> Rect {
        Rect {
            x: self.x.saturating_add(1),
            y: self.y.saturating_add(1),
            width: self.width.saturating_sub(2),
            height: self.height.saturating_sub(2),
        }
    }
}

impl From<Rect> for TuiRect {
    fn from(r: Rect) -> Self {
        TuiRect {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        }
    }
}

/// How [`Layout::reflow`] arranges the panels into the screen area.
///
/// `Tiled` is the historical roughly-square auto-tile; the rest mirror the
/// tmux layout names. The arrangement is cycled at runtime via `Ctrl-A Space`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutMode {
    /// Roughly-square auto-tile: `cols = ceil(sqrt(n))` columns.
    #[default]
    Tiled,
    /// N side-by-side columns, each full height.
    EvenHorizontal,
    /// N stacked rows, each full width.
    EvenVertical,
    /// Panel 0 fills the left half; the rest stack in the right half.
    MainVertical,
}

impl LayoutMode {
    /// The next arrangement in the `Ctrl-A Space` cycle.
    pub fn next(self) -> Self {
        match self {
            LayoutMode::Tiled => LayoutMode::EvenHorizontal,
            LayoutMode::EvenHorizontal => LayoutMode::EvenVertical,
            LayoutMode::EvenVertical => LayoutMode::MainVertical,
            LayoutMode::MainVertical => LayoutMode::Tiled,
        }
    }

    /// A short human-readable label for the status bar.
    pub fn label(self) -> &'static str {
        match self {
            LayoutMode::Tiled => "tiled",
            LayoutMode::EvenHorizontal => "even-horizontal",
            LayoutMode::EvenVertical => "even-vertical",
            LayoutMode::MainVertical => "main-vertical",
        }
    }
}

/// A computed layout: the screen rectangle assigned to each panel.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// One `(panel, rect)` per visible panel.
    pub slots: Vec<(PanelId, Rect)>,
}

impl Layout {
    /// Reflow `panels` into `area` according to `mode`
    /// (`docs/design.md` §0 #10: caucus reflows on every spawn/kill).
    ///
    /// Every mode partitions `area` exactly — no gaps, no overlap — with
    /// rounding slack distributed cell-by-cell via `split`.
    pub fn reflow(panels: &[PanelId], area: Rect, mode: LayoutMode) -> Self {
        let n = panels.len();
        if n == 0 || area.width == 0 || area.height == 0 {
            return Self::default();
        }
        match mode {
            LayoutMode::Tiled => Self::reflow_tiled(panels, area),
            LayoutMode::EvenHorizontal => Self::reflow_even_horizontal(panels, area),
            LayoutMode::EvenVertical => Self::reflow_even_vertical(panels, area),
            LayoutMode::MainVertical => Self::reflow_main_vertical(panels, area),
        }
    }

    /// Roughly-square auto-tile: pick `cols = ceil(sqrt(n))` columns and
    /// `rows = ceil(n / cols)` rows, then hand each panel one cell. The last
    /// row's panels widen to absorb the remainder when `n` is not a perfect
    /// rectangle.
    fn reflow_tiled(panels: &[PanelId], area: Rect) -> Self {
        let n = panels.len();
        // Grid shape: roughly square, columns >= rows.
        let cols = (n as f64).sqrt().ceil() as usize;
        let rows = n.div_ceil(cols);

        let col_bounds = split(area.x, area.width, cols);
        let row_bounds = split(area.y, area.height, rows);

        let mut slots = Vec::with_capacity(n);
        for (i, &id) in panels.iter().enumerate() {
            let row = i / cols;
            let col = i % cols;
            // Panels in the final, possibly short, row span all the columns
            // not taken by an earlier panel — so the bottom row is never
            // ragged: it widens to fill `area`.
            let in_last_row = row == rows - 1;
            let cells_in_row = if in_last_row { n - row * cols } else { cols };
            let col_b = if in_last_row && cells_in_row != cols {
                split(area.x, area.width, cells_in_row)
            } else {
                col_bounds.clone()
            };
            let (cx, cw) = col_b[col.min(col_b.len() - 1)];
            let (ry, rh) = row_bounds[row];
            slots.push((
                id,
                Rect {
                    x: cx,
                    y: ry,
                    width: cw,
                    height: rh,
                },
            ));
        }
        Self { slots }
    }

    /// N side-by-side columns, each spanning the full height of `area`.
    fn reflow_even_horizontal(panels: &[PanelId], area: Rect) -> Self {
        let cols = split(area.x, area.width, panels.len());
        let slots = panels
            .iter()
            .zip(cols)
            .map(|(&id, (cx, cw))| {
                (
                    id,
                    Rect {
                        x: cx,
                        y: area.y,
                        width: cw,
                        height: area.height,
                    },
                )
            })
            .collect();
        Self { slots }
    }

    /// N stacked rows, each spanning the full width of `area`.
    fn reflow_even_vertical(panels: &[PanelId], area: Rect) -> Self {
        let rows = split(area.y, area.height, panels.len());
        let slots = panels
            .iter()
            .zip(rows)
            .map(|(&id, (ry, rh))| {
                (
                    id,
                    Rect {
                        x: area.x,
                        y: ry,
                        width: area.width,
                        height: rh,
                    },
                )
            })
            .collect();
        Self { slots }
    }

    /// Panel 0 fills the left half of `area`; the remaining panels stack in
    /// the right half. With a single panel it fills the whole area.
    fn reflow_main_vertical(panels: &[PanelId], area: Rect) -> Self {
        if panels.len() == 1 {
            return Self {
                slots: vec![(panels[0], area)],
            };
        }
        let cols = split(area.x, area.width, 2);
        let (lx, lw) = cols[0];
        let (rx, rw) = cols[1];
        let mut slots = Vec::with_capacity(panels.len());
        slots.push((
            panels[0],
            Rect {
                x: lx,
                y: area.y,
                width: lw,
                height: area.height,
            },
        ));
        let rest = &panels[1..];
        let rows = split(area.y, area.height, rest.len());
        for (&id, (ry, rh)) in rest.iter().zip(rows) {
            slots.push((
                id,
                Rect {
                    x: rx,
                    y: ry,
                    width: rw,
                    height: rh,
                },
            ));
        }
        Self { slots }
    }

    /// The rect assigned to `panel`, if any.
    pub fn rect_of(&self, panel: PanelId) -> Option<Rect> {
        self.slots
            .iter()
            .find(|(id, _)| *id == panel)
            .map(|(_, r)| *r)
    }
}

/// Partition the half-open interval `[start, start + total)` into `parts`
/// contiguous chunks, returning `(offset, length)` for each. The first
/// `total % parts` chunks are one cell larger so the chunks exactly tile the
/// interval with no gap.
fn split(start: u16, total: u16, parts: usize) -> Vec<(u16, u16)> {
    let parts = parts.max(1);
    let base = total as usize / parts;
    let extra = total as usize % parts;
    let mut out = Vec::with_capacity(parts);
    let mut offset = start as usize;
    for i in 0..parts {
        let len = base + usize::from(i < extra);
        out.push((offset as u16, len as u16));
        offset += len;
    }
    out
}

/// A screen direction for spatial focus navigation (`Ctrl-A` + arrow).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// The panel nearest to `from` in screen direction `dir`, among `candidates`
/// (each a panel id with its current slot rect) — the target for `Ctrl-A` +
/// arrow directional focus navigation.
///
/// A candidate qualifies when its center lies strictly in `dir` of `from`'s
/// center. Among qualifying candidates the sort key is, in order: whether its
/// *perpendicular* span overlaps `from` (a panel sharing rows when moving
/// left/right is preferred, so in a grid the move lands on the tile directly
/// beside rather than a diagonal one), then the smallest travel-direction
/// distance, then the smallest perpendicular offset. Returns `None` when
/// nothing lies in that direction.
pub fn nearest_in_direction(
    from: Rect,
    candidates: impl IntoIterator<Item = (PanelId, Rect)>,
    dir: Direction,
) -> Option<PanelId> {
    let fcx = from.x as i32 + from.width as i32 / 2;
    let fcy = from.y as i32 + from.height as i32 / 2;
    let mut best: Option<(PanelId, (u8, i64, i64))> = None;
    for (id, r) in candidates {
        let cx = r.x as i32 + r.width as i32 / 2;
        let cy = r.y as i32 + r.height as i32 / 2;
        let (primary, perp, overlaps) = match dir {
            Direction::Right => (
                cx - fcx,
                (cy - fcy).abs(),
                ranges_overlap(from.y, from.height, r.y, r.height),
            ),
            Direction::Left => (
                fcx - cx,
                (cy - fcy).abs(),
                ranges_overlap(from.y, from.height, r.y, r.height),
            ),
            Direction::Down => (
                cy - fcy,
                (cx - fcx).abs(),
                ranges_overlap(from.x, from.width, r.x, r.width),
            ),
            Direction::Up => (
                fcy - cy,
                (cx - fcx).abs(),
                ranges_overlap(from.x, from.width, r.x, r.width),
            ),
        };
        if primary <= 0 {
            continue; // not in this direction
        }
        let key = (u8::from(!overlaps), primary as i64, perp as i64);
        match &best {
            Some((_, bk)) if *bk <= key => {}
            _ => best = Some((id, key)),
        }
    }
    best.map(|(id, _)| id)
}

/// Whether the half-open intervals `[a, a+alen)` and `[b, b+blen)` overlap.
/// Computed in `u32` so a slot at the far edge of a large terminal cannot
/// overflow the `u16` sum.
fn ranges_overlap(a: u16, alen: u16, b: u16, blen: u16) -> bool {
    (a as u32) < (b as u32 + blen as u32) && (b as u32) < (a as u32 + alen as u32)
}

/// Draw the full caucus screen: every panel's grid into its [`Layout`] slot,
/// with a titled border and a focus highlight on `focused`.
///
/// After painting, the hardware cursor is parked at the focused panel's grid
/// cursor. This is what makes the real terminal's cursor — and therefore IME
/// pre-edit (composing CJK / Korean input) — track the focused agent's input
/// position; without it the terminal composes pre-edit text at a stale spot
/// and CJK input renders detached.
pub fn draw(frame: &mut Frame, layout: &Layout, panels: &[Panel], focused: Option<PanelId>) {
    for (id, rect) in &layout.slots {
        let Some(panel) = panels.iter().find(|p| p.id == *id) else {
            continue;
        };
        draw_panel(frame, panel, *rect, focused == Some(*id));
    }

    // Park the hardware cursor at the focused panel's grid cursor. Grid
    // columns map 1:1 to screen columns (a wide glyph occupies two grid
    // columns and `grid_lines` renders it two columns wide), so the grid
    // cursor column is also the screen column.
    if let Some(fid) = focused
        && let Some(rect) = layout.rect_of(fid)
        && let Some(panel) = panels.iter().find(|p| p.id == fid)
    {
        let interior = rect.inner();
        if interior.width > 0 && interior.height > 0 {
            let (crow, ccol) = panel.grid().cursor();
            let x = interior.x + (ccol as u16).min(interior.width - 1);
            let y = interior.y + (crow as u16).min(interior.height - 1);
            frame.set_cursor_position((x, y));
        }
    }
}

/// Draw one panel: a bordered block titled with the role and derived state,
/// the grid viewport painted into the interior.
fn draw_panel(frame: &mut Frame, panel: &Panel, rect: Rect, focused: bool) {
    let tui_rect: TuiRect = rect.into();
    if tui_rect.width < 2 || tui_rect.height < 2 {
        return;
    }

    let border_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let title = format!(" {} · {} ", panel.role, panel.state_label());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(
            title,
            Style::default().fg(if focused { Color::Cyan } else { Color::Gray }),
        ));

    let lines = grid_lines(panel.grid(), &tui_rect);
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, tui_rect);
}

/// Render a [`Grid`]'s viewport as ratatui [`Line`]s, clipped to the interior
/// `rect` (which still includes the border — interior is `height - 2` rows).
fn grid_lines(grid: &Grid, rect: &TuiRect) -> Vec<Line<'static>> {
    let (cols, rows) = grid.size();
    let view_rows = rect.height.saturating_sub(2) as usize;
    let view_cols = rect.width.saturating_sub(2) as usize;
    let take_rows = rows.min(view_rows);
    let take_cols = cols.min(view_cols);

    let mut lines = Vec::with_capacity(take_rows);
    for r in 0..take_rows {
        // Coalesce a run of consecutive cells that share a style into one span:
        // one `String` + one `Span` per style-run instead of per cell. A
        // terminal row is mostly long same-style runs, so this cuts the
        // per-frame allocation count sharply. The rendered cells are identical
        // — ratatui lays spans out left to right, so "ab" in one styled span
        // draws the same columns as "a","b" in two (pinned byte-for-byte by
        // `grid_lines_coalesces_runs_identically`).
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run = String::new();
        let mut run_style: Option<Style> = None;
        for c in 0..take_cols {
            let Some(cell) = grid.cell(r, c) else { break };
            if cell.ch == '\0' {
                // Trailing half of a wide glyph — covered by its lead cell's
                // char. Emit nothing; this does not break the run.
                continue;
            }
            let style = cell_style(cell);
            if run_style == Some(style) {
                run.push(cell.ch);
            } else {
                if let Some(prev) = run_style.take() {
                    spans.push(Span::styled(std::mem::take(&mut run), prev));
                }
                run.push(cell.ch);
                run_style = Some(style);
            }
        }
        if let Some(prev) = run_style {
            spans.push(Span::styled(run, prev));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Translate a [`crate::term::Cell`]'s packed colour + attribute bytes into a
/// ratatui [`Style`].
fn cell_style(cell: &crate::term::Cell) -> Style {
    let mut style = Style::default();
    if let Some(fg) = palette_color(cell.fg) {
        style = style.fg(fg);
    }
    if let Some(bg) = palette_color(cell.bg) {
        style = style.bg(bg);
    }
    let mut modifier = Modifier::empty();
    if cell.attrs & attr::BOLD != 0 {
        modifier |= Modifier::BOLD;
    }
    if cell.attrs & attr::DIM != 0 {
        modifier |= Modifier::DIM;
    }
    if cell.attrs & attr::ITALIC != 0 {
        modifier |= Modifier::ITALIC;
    }
    if cell.attrs & attr::UNDERLINE != 0 {
        modifier |= Modifier::UNDERLINED;
    }
    if cell.attrs & attr::REVERSE != 0 {
        modifier |= Modifier::REVERSED;
    }
    if cell.attrs & attr::HIDDEN != 0 {
        modifier |= Modifier::HIDDEN;
    }
    if cell.attrs & attr::STRIKE != 0 {
        modifier |= Modifier::CROSSED_OUT;
    }
    style.add_modifier(modifier)
}

/// Map a [`crate::term::Cell`] packed colour index to a ratatui [`Color`].
///
/// Index `0` is "default" (`None` — let the terminal decide); `1..=8` the
/// standard ANSI colours, `9..=16` the bright variants, `17..=255` a direct
/// 256-colour palette index (the grid stores true-colour pre-quantised).
fn palette_color(idx: u8) -> Option<Color> {
    match idx {
        0 => None,
        1 => Some(Color::Black),
        2 => Some(Color::Red),
        3 => Some(Color::Green),
        4 => Some(Color::Yellow),
        5 => Some(Color::Blue),
        6 => Some(Color::Magenta),
        7 => Some(Color::Cyan),
        8 => Some(Color::Gray),
        9 => Some(Color::DarkGray),
        10 => Some(Color::LightRed),
        11 => Some(Color::LightGreen),
        12 => Some(Color::LightYellow),
        13 => Some(Color::LightBlue),
        14 => Some(Color::LightMagenta),
        15 => Some(Color::LightCyan),
        16 => Some(Color::White),
        n => Some(Color::Indexed(n)),
    }
}

/// One row of the transcript overlay — a pure summary of a panel's activity,
/// derived from its [`Panel`] plus its [`AgentManifest`]. Built by
/// [`TranscriptRow::build`] so the formatting (turn count, branch, message
/// truncation) is unit-testable without a ratatui frame.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TranscriptRow {
    /// Role name driving the panel.
    pub role: String,
    /// Lower-case derived-state label (`working` / `idle` / `blocked_*` / ...).
    pub state: String,
    /// Whether this panel currently has focus.
    pub focused: bool,
    /// Number of `TurnCompleted` lane events on the manifest.
    pub turns: usize,
    /// Worktree branch name, if the panel runs in a worktree.
    pub branch: Option<String>,
    /// First line of the agent's last message, untruncated.
    pub last_message: String,
}

impl TranscriptRow {
    /// Build a row from a panel and its manifest. The manifest is optional —
    /// before the first manifest write a panel falls back to its coarse
    /// panel-state label and a zero turn count.
    pub fn build(panel: &Panel, manifest: Option<&AgentManifest>, focused: bool) -> Self {
        let state = match manifest {
            Some(m) => derived_state_label(m.derived_state()).to_string(),
            None => panel.state_label().to_string(),
        };
        let turns = manifest.map(turn_count).unwrap_or(0);
        // The worktree branch is the last path component of the worktree dir
        // (`worktree::manager::create` names the dir after the branch).
        let branch = panel
            .worktree_path
            .as_ref()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        let last_message = manifest
            .and_then(|m| m.last_message())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        Self {
            role: panel.role.clone(),
            state,
            focused,
            turns,
            branch,
            last_message,
        }
    }

    /// Render the row to a single display string clipped to `width` columns.
    /// The fixed-width prefix (marker, role, state, turns, branch) is laid
    /// out first; the last message takes whatever width is left, truncated
    /// with an ellipsis.
    pub fn render_line(&self, width: usize) -> String {
        let marker = if self.focused { "▸ " } else { "  " };
        let branch = match &self.branch {
            Some(b) => format!(" [{b}]"),
            None => String::new(),
        };
        let prefix = format!(
            "{marker}{role} · {state} · {turns} turn(s){branch}  ",
            role = self.role,
            state = self.state,
            turns = self.turns,
        );
        let prefix_w = prefix.chars().count();
        if prefix_w >= width {
            return truncate_ellipsis(&prefix, width);
        }
        let msg = truncate_ellipsis(&self.last_message, width - prefix_w);
        format!("{prefix}{msg}")
    }
}

/// Count the `TurnCompleted` lane events on a manifest — the panel's turn
/// count shown in the transcript overlay.
fn turn_count(manifest: &AgentManifest) -> usize {
    manifest
        .lane_events()
        .iter()
        .filter(|e| matches!(e.kind, LaneEventKind::TurnCompleted))
        .count()
}

/// Lower-case label for a [`DerivedState`], matching the overlay's vocabulary
/// (`working` / `idle` / `blocked_*` / `exited` / ...).
fn derived_state_label(state: DerivedState) -> &'static str {
    match state {
        DerivedState::Working => "working",
        DerivedState::Idle => "idle",
        DerivedState::BlockedPermissionPrompt => "blocked_permission",
        DerivedState::BlockedMergeConflict => "blocked_merge",
        DerivedState::BlockedBackgroundJob => "blocked_job",
        DerivedState::AwaitingSelection => "awaiting_selection",
        DerivedState::DegradedMcp => "degraded_mcp",
        DerivedState::InterruptedTransport => "interrupted",
        DerivedState::Exited => "exited",
    }
}

/// Colour-code a state label: green idle, yellow working, red blocked/exited/
/// interrupted, gray otherwise.
fn state_color(state: &str) -> Color {
    if state == "idle" {
        Color::Green
    } else if state == "working" {
        Color::Yellow
    } else if state.starts_with("blocked")
        || state == "exited"
        || state == "interrupted"
        || state == "degraded_mcp"
        || state == "awaiting_selection"
    {
        Color::Red
    } else {
        Color::Gray
    }
}

/// Truncate `s` to at most `width` display columns, appending `…` when it had
/// to be cut. A `width` of 0 yields an empty string.
fn truncate_ellipsis(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if s.chars().count() <= width {
        return s.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let kept: String = s.chars().take(width - 1).collect();
    format!("{kept}…")
}

/// Draw the read-only transcript overlay on top of the panels: a bordered,
/// near-full-screen popup with one summary row per panel.
///
/// Draw-time only — the panels keep rendering underneath; this just paints
/// over them. The popup area is blanked with [`Clear`] first so the panels
/// beneath do not bleed through.
pub fn draw_transcript(
    frame: &mut Frame,
    panels: &[Panel],
    manifests: &HashMap<PanelId, AgentManifest>,
    focused: Option<PanelId>,
) {
    let full = frame.area();
    if full.width < 8 || full.height < 6 {
        return;
    }
    // Inset the popup two cells on every side so the panels remain visible
    // as a frame around it.
    let popup = TuiRect {
        x: full.x + 2,
        y: full.y + 2,
        width: full.width.saturating_sub(4),
        height: full.height.saturating_sub(4),
    };

    let title = format!(" caucus · transcript — {} panel(s) ", panels.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));

    // Interior: the popup minus its single-cell border.
    let inner_w = popup.width.saturating_sub(2) as usize;
    let inner_h = popup.height.saturating_sub(2) as usize;

    let mut lines: Vec<Line<'static>> = Vec::new();
    if panels.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no panels)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        // Reserve one row for a "… +N more" line when there are too many
        // panels to fit; otherwise every panel gets a row.
        let fits = inner_h;
        let (visible, overflow) = if panels.len() > fits && fits > 0 {
            (
                fits.saturating_sub(1),
                panels.len() - fits.saturating_sub(1),
            )
        } else {
            (panels.len(), 0)
        };
        for panel in panels.iter().take(visible) {
            let row =
                TranscriptRow::build(panel, manifests.get(&panel.id), focused == Some(panel.id));
            lines.push(transcript_row_line(&row, inner_w));
        }
        if overflow > 0 {
            lines.push(Line::from(Span::styled(
                format!("  … +{overflow} more"),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Render one [`TranscriptRow`] as a styled ratatui [`Line`]: the state token
/// is colour-coded, the rest is plain.
fn transcript_row_line(row: &TranscriptRow, width: usize) -> Line<'static> {
    let text = row.render_line(width);
    // Colour the whole line by the panel's state — simple and legible; the
    // state word is the at-a-glance signal the overlay exists for.
    let mut style = Style::default().fg(state_color(&row.state));
    if row.focused {
        style = style.add_modifier(Modifier::BOLD);
    }
    Line::from(Span::styled(text, style))
}

/// Window the scrollback into the visible slice plus a title string — the
/// pure, frame-free core of [`draw_scroll_pager`], unit-testable without a
/// ratatui [`Frame`] (like [`TranscriptRow::render_line`]).
///
/// Returns `lines[offset .. offset+height]` (clamped to the buffer) and a
/// title reporting the panel role and the 1-based visible range
/// `start–end/total`.
fn scroll_window<'a>(
    role: &str,
    lines: &'a [String],
    offset: usize,
    height: usize,
) -> (&'a [String], String) {
    let total = lines.len();
    let start = offset.min(total);
    let end = (start + height).min(total);
    let visible = &lines[start..end];
    let range = if total == 0 {
        "empty".to_string()
    } else {
        format!("{}–{}/{}", start + 1, end, total)
    };
    let title = format!(" caucus · scrollback — {role} [{range}] · ↑↓ PgUp/PgDn g/G · Esc/q exit ");
    (visible, title)
}

/// Draw the scrollback pager (`Ctrl-A [`) full-screen over the panels: a
/// bordered box titled with the panel role + visible range + key hints, the
/// body being the windowed scrollback lines.
///
/// Like [`draw_transcript`], this is draw-time only — the panels keep running
/// underneath; the popup area is [`Clear`]ed first so they do not bleed
/// through. The pager is *modal* (it captures input via the router's
/// `scroll_open` gate), but rendering-wise it is just a top overlay.
///
/// `pub(crate)`: [`ScrollState`] is an internal type, drawn only by `tui::draw`.
pub(crate) fn draw_scroll_pager(frame: &mut Frame, state: &ScrollState) {
    let full = frame.area();
    if full.width < 8 || full.height < 6 {
        return;
    }
    let popup = TuiRect {
        x: full.x + 2,
        y: full.y + 2,
        width: full.width.saturating_sub(4),
        height: full.height.saturating_sub(4),
    };

    // Interior height (popup minus its single-cell border) is the real visible
    // body; window the snapshot to exactly that many rows.
    let inner_h = popup.height.saturating_sub(2) as usize;
    let (visible, title) = scroll_window(&state.role, &state.lines, state.offset, inner_h);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));

    let lines: Vec<Line<'static>> = if visible.is_empty() {
        vec![Line::from(Span::styled(
            "  (no scrollback)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        visible
            .iter()
            .map(|l| Line::from(Span::raw(l.clone())))
            .collect()
    };

    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        }
    }

    /// Assert the layout's slots cover every cell of the 80x24 `area()`
    /// exactly once — no gaps, no overlap.
    fn assert_partitions_area(layout: &Layout) {
        let mut covered = vec![0u8; 80 * 24];
        for (_, r) in &layout.slots {
            for y in r.y..r.y + r.height {
                for x in r.x..r.x + r.width {
                    covered[y as usize * 80 + x as usize] += 1;
                }
            }
        }
        assert!(
            covered.iter().all(|&c| c == 1),
            "every cell covered exactly once"
        );
    }

    #[test]
    fn reflow_assigns_a_slot_per_panel() {
        let panels = vec![PanelId::new(), PanelId::new()];
        let layout = Layout::reflow(&panels, area(), LayoutMode::Tiled);
        assert_eq!(layout.slots.len(), 2);
    }

    #[test]
    fn palette_color_maps_the_standard_ansi_block() {
        // The grid encodes SGR 30..=37 as index 1..=8 and SGR 90..=97 as
        // index 9..=16 (`term::grid` apply_sgr). So index 1 = SGR 30 = black
        // and index 9 = SGR 90 = bright-black; the mapping must follow that.
        assert_eq!(palette_color(0), None, "0 is default (terminal decides)");
        assert_eq!(palette_color(1), Some(Color::Black), "SGR 30 → black");
        assert_eq!(palette_color(2), Some(Color::Red), "SGR 31 → red");
        assert_eq!(palette_color(3), Some(Color::Green));
        assert_eq!(palette_color(4), Some(Color::Yellow));
        assert_eq!(palette_color(5), Some(Color::Blue));
        assert_eq!(palette_color(6), Some(Color::Magenta));
        assert_eq!(palette_color(7), Some(Color::Cyan));
        assert_eq!(palette_color(8), Some(Color::Gray), "SGR 37 → white/gray");
        assert_eq!(
            palette_color(9),
            Some(Color::DarkGray),
            "SGR 90 → bright-black (visible, not pure black)"
        );
        assert_eq!(palette_color(10), Some(Color::LightRed));
        assert_eq!(
            palette_color(16),
            Some(Color::White),
            "SGR 97 → bright white"
        );
        assert_eq!(palette_color(200), Some(Color::Indexed(200)));
    }

    /// The run-length coalesce in `grid_lines` must not change the rendered
    /// cells: the flattened `(char, style)` sequence per row must equal the
    /// per-cell reference (the pre-coalesce shape — one span per non-null
    /// cell). It must also actually merge runs (fewer spans than cells).
    #[test]
    fn grid_lines_coalesces_runs_identically() {
        // A row with two same-style runs ("RED" red, then default), a wide
        // CJK glyph, and the trailing blank cells the grid pads with.
        let mut grid = crate::term::Grid::new(16, 1);
        grid.advance("\x1b[31mRED\x1b[0mab中cd".as_bytes());
        let rect = TuiRect {
            x: 0,
            y: 0,
            width: 18, // interior width 16 = grid cols
            height: 3, // interior height 1 = grid rows
        };

        let lines = grid_lines(&grid, &rect);
        assert_eq!(lines.len(), 1, "one row in, one Line out");

        // Per-cell reference: exactly what the old loop emitted — one
        // (char, style) per non-null cell, in column order.
        let (cols, _) = grid.size();
        let expected: Vec<(char, Style)> = (0..cols.min(16))
            .filter_map(|c| grid.cell(0, c))
            .filter(|cell| cell.ch != '\0')
            .map(|cell| (cell.ch, cell_style(cell)))
            .collect();

        // Flatten the coalesced spans back to (char, style) per grapheme.
        let actual: Vec<(char, Style)> = lines[0]
            .spans
            .iter()
            .flat_map(|s| {
                let st = s.style;
                s.content.chars().map(move |c| (c, st))
            })
            .collect();
        assert_eq!(
            actual, expected,
            "coalesced output must render the same cells as the per-cell form"
        );

        // The optimization actually fired: fewer spans than non-null cells.
        assert!(
            lines[0].spans.len() < expected.len(),
            "runs must coalesce: {} spans for {} cells",
            lines[0].spans.len(),
            expected.len()
        );
    }

    #[test]
    fn scroll_window_slices_and_reports_the_range() {
        let lines: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();

        // Mid-buffer: offset 3, height 4 → lines[3..7], range "4–7/10".
        let (vis, title) = scroll_window("worker", &lines, 3, 4);
        assert_eq!(vis, &lines[3..7]);
        assert!(title.contains("worker"));
        assert!(title.contains("4–7/10"), "title was {title:?}");

        // Bottom clamp: a window past the end stops at total, not out of range.
        let (vis, title) = scroll_window("worker", &lines, 8, 4);
        assert_eq!(vis, &lines[8..10]);
        assert!(title.contains("9–10/10"), "title was {title:?}");
    }

    #[test]
    fn scroll_window_handles_an_empty_buffer() {
        let (vis, title) = scroll_window("worker", &[], 0, 5);
        assert!(vis.is_empty());
        assert!(title.contains("empty"), "title was {title:?}");
    }

    #[test]
    fn reflow_empty_is_empty() {
        assert!(
            Layout::reflow(&[], area(), LayoutMode::Tiled)
                .slots
                .is_empty()
        );
    }

    #[test]
    fn nearest_in_direction_walks_a_2x2_grid() {
        // A 2x2 grid over the 80x24 area.
        let tl = (
            PanelId::new(),
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 12,
            },
        );
        let tr = (
            PanelId::new(),
            Rect {
                x: 40,
                y: 0,
                width: 40,
                height: 12,
            },
        );
        let bl = (
            PanelId::new(),
            Rect {
                x: 0,
                y: 12,
                width: 40,
                height: 12,
            },
        );
        let br = (
            PanelId::new(),
            Rect {
                x: 40,
                y: 12,
                width: 40,
                height: 12,
            },
        );
        let all = [tl, tr, bl, br];
        let others = |exclude: PanelId| {
            all.iter()
                .filter(move |(id, _)| *id != exclude)
                .map(|&(id, r)| (id, r))
                .collect::<Vec<_>>()
        };

        // From top-left: right -> top-right, down -> bottom-left; up/left empty.
        assert_eq!(
            nearest_in_direction(tl.1, others(tl.0), Direction::Right),
            Some(tr.0)
        );
        assert_eq!(
            nearest_in_direction(tl.1, others(tl.0), Direction::Down),
            Some(bl.0)
        );
        assert_eq!(
            nearest_in_direction(tl.1, others(tl.0), Direction::Up),
            None
        );
        assert_eq!(
            nearest_in_direction(tl.1, others(tl.0), Direction::Left),
            None
        );

        // From bottom-right: up -> top-right, left -> bottom-left.
        assert_eq!(
            nearest_in_direction(br.1, others(br.0), Direction::Up),
            Some(tr.0)
        );
        assert_eq!(
            nearest_in_direction(br.1, others(br.0), Direction::Left),
            Some(bl.0)
        );
    }

    #[test]
    fn nearest_in_direction_prefers_a_perpendicularly_overlapping_panel() {
        // Moving right from `from`, two candidates lie to the right: `aligned`
        // shares rows (overlaps), `diagonal` is lower and slightly nearer in x.
        // The overlapping one wins so the move stays in the same band.
        let from = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        let aligned = (
            PanelId::new(),
            Rect {
                x: 30,
                y: 0,
                width: 20,
                height: 10,
            },
        );
        let diagonal = (
            PanelId::new(),
            Rect {
                x: 25,
                y: 30,
                width: 20,
                height: 10,
            },
        );
        assert_eq!(
            nearest_in_direction(from, [aligned, diagonal], Direction::Right),
            Some(aligned.0)
        );
    }

    #[test]
    fn nearest_in_direction_empty_candidates_is_none() {
        let from = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        assert_eq!(nearest_in_direction(from, [], Direction::Up), None);
    }

    #[test]
    fn single_panel_fills_the_whole_area() {
        let id = PanelId::new();
        let layout = Layout::reflow(&[id], area(), LayoutMode::Tiled);
        assert_eq!(
            layout.rect_of(id),
            Some(Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 24
            })
        );
    }

    #[test]
    fn two_panels_split_into_two_columns() {
        let panels = vec![PanelId::new(), PanelId::new()];
        let layout = Layout::reflow(&panels, area(), LayoutMode::Tiled);
        // ceil(sqrt(2)) = 2 columns, 1 row.
        let r0 = layout.slots[0].1;
        let r1 = layout.slots[1].1;
        assert_eq!(r0.height, 24);
        assert_eq!(r1.height, 24);
        assert_eq!(r0.width + r1.width, 80);
        assert_eq!(r1.x, r0.x + r0.width, "no gap, no overlap");
    }

    #[test]
    fn four_panels_form_a_two_by_two_grid() {
        let panels: Vec<_> = (0..4).map(|_| PanelId::new()).collect();
        let layout = Layout::reflow(&panels, area(), LayoutMode::Tiled);
        // 2x2: each tile 40x12.
        for (_, r) in &layout.slots {
            assert_eq!(r.width, 40);
            assert_eq!(r.height, 12);
        }
    }

    #[test]
    fn tiles_partition_the_area_without_gap_or_overlap() {
        // 5 panels: ceil(sqrt(5))=3 cols, ceil(5/3)=2 rows. Last row has 2.
        let panels: Vec<_> = (0..5).map(|_| PanelId::new()).collect();
        let layout = Layout::reflow(&panels, area(), LayoutMode::Tiled);
        assert_partitions_area(&layout);
    }

    #[test]
    fn even_horizontal_partitions_the_area() {
        for n in 1..=7 {
            let panels: Vec<_> = (0..n).map(|_| PanelId::new()).collect();
            let layout = Layout::reflow(&panels, area(), LayoutMode::EvenHorizontal);
            assert_eq!(layout.slots.len(), n);
            // Every slot spans the full height — N side-by-side columns.
            for (_, r) in &layout.slots {
                assert_eq!(r.height, 24);
                assert_eq!(r.y, 0);
            }
            assert_partitions_area(&layout);
        }
    }

    #[test]
    fn even_vertical_partitions_the_area() {
        for n in 1..=7 {
            let panels: Vec<_> = (0..n).map(|_| PanelId::new()).collect();
            let layout = Layout::reflow(&panels, area(), LayoutMode::EvenVertical);
            assert_eq!(layout.slots.len(), n);
            // Every slot spans the full width — N stacked rows.
            for (_, r) in &layout.slots {
                assert_eq!(r.width, 80);
                assert_eq!(r.x, 0);
            }
            assert_partitions_area(&layout);
        }
    }

    #[test]
    fn main_vertical_partitions_the_area() {
        for n in 2..=7 {
            let panels: Vec<_> = (0..n).map(|_| PanelId::new()).collect();
            let layout = Layout::reflow(&panels, area(), LayoutMode::MainVertical);
            assert_eq!(layout.slots.len(), n);
            // Panel 0 is the left main pane, full height.
            let main = layout.rect_of(panels[0]).unwrap();
            assert_eq!(main.x, 0);
            assert_eq!(main.y, 0);
            assert_eq!(main.height, 24);
            assert_eq!(main.width, 40);
            assert_partitions_area(&layout);
        }
    }

    #[test]
    fn main_vertical_single_panel_fills_the_area() {
        let id = PanelId::new();
        let layout = Layout::reflow(&[id], area(), LayoutMode::MainVertical);
        assert_eq!(
            layout.rect_of(id),
            Some(Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 24
            })
        );
    }

    #[test]
    fn rounding_slack_is_distributed() {
        // 3 columns over width 80: 27 + 27 + 26 = 80, no cell lost.
        let cols = split(0, 80, 3);
        assert_eq!(cols.iter().map(|(_, w)| *w as u32).sum::<u32>(), 80);
        assert_eq!(cols[0].0, 0);
        assert_eq!(cols[1].0, cols[0].0 + cols[0].1);
        assert_eq!(cols[2].0, cols[1].0 + cols[1].1);
    }

    #[test]
    fn inner_strips_the_border() {
        let r = Rect {
            x: 5,
            y: 7,
            width: 40,
            height: 12,
        };
        assert_eq!(
            r.inner(),
            Rect {
                x: 6,
                y: 8,
                width: 38,
                height: 10
            }
        );
    }

    #[test]
    fn inner_of_tiny_rect_does_not_underflow() {
        let r = Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        assert_eq!(r.inner().width, 0);
        assert_eq!(r.inner().height, 0);
    }

    fn row(role: &str, msg: &str) -> TranscriptRow {
        TranscriptRow {
            role: role.into(),
            state: "working".into(),
            focused: false,
            turns: 3,
            branch: None,
            last_message: msg.into(),
        }
    }

    #[test]
    fn truncate_ellipsis_cuts_and_marks() {
        assert_eq!(truncate_ellipsis("hello", 10), "hello");
        assert_eq!(truncate_ellipsis("hello", 5), "hello");
        assert_eq!(truncate_ellipsis("hello world", 5), "hell…");
        assert_eq!(truncate_ellipsis("hello", 1), "…");
        assert_eq!(truncate_ellipsis("hello", 0), "");
    }

    #[test]
    fn render_line_truncates_the_message_to_width() {
        let r = row(
            "backend",
            "this is a fairly long last message that will not fit",
        );
        let line = r.render_line(50);
        assert!(
            line.chars().count() <= 50,
            "got {} cols",
            line.chars().count()
        );
        assert!(
            line.ends_with('…'),
            "long message must be ellipsised: {line:?}"
        );
        assert!(line.contains("backend"));
        assert!(line.contains("3 turn(s)"));
    }

    #[test]
    fn render_line_keeps_a_short_message_intact() {
        let r = row("qa", "done");
        let line = r.render_line(80);
        assert!(line.contains("done"));
        assert!(!line.ends_with('…'));
    }

    #[test]
    fn render_line_clips_the_prefix_when_width_is_tiny() {
        let r = row("architect", "ignored");
        let line = r.render_line(6);
        assert_eq!(line.chars().count(), 6);
        assert!(line.ends_with('…'));
    }

    #[test]
    fn focused_row_gets_the_marker() {
        let mut r = row("backend", "x");
        r.focused = true;
        assert!(r.render_line(80).starts_with("▸ "));
        r.focused = false;
        assert!(r.render_line(80).starts_with("  "));
    }

    #[test]
    fn render_line_shows_the_branch() {
        let mut r = row("backend", "x");
        r.branch = Some("caucus-abc-backend-1".into());
        assert!(r.render_line(120).contains("[caucus-abc-backend-1]"));
    }

    #[test]
    fn turn_count_counts_turn_completed_events() {
        use crate::agent::lane_event::LaneEvent;
        use crate::agent::manifest::AgentManifest;
        use crate::role::spec::AgentCli;
        use crate::session::id::{PanelId, SessionId};

        let mut m = AgentManifest::new(
            SessionId::new(),
            PanelId::new(),
            "reviewer",
            "reviewer-1",
            AgentCli::Claude,
            None,
        );
        // A fresh manifest has only a `Started` event — zero turns.
        assert_eq!(turn_count(&m), 0);

        // Append two TurnCompleted events directly to the timeline.
        m.lane_events
            .push(LaneEvent::now(LaneEventKind::TurnCompleted));
        m.lane_events
            .push(LaneEvent::now(LaneEventKind::TurnCompleted));
        assert_eq!(turn_count(&m), 2);
    }

    #[test]
    fn state_color_codes_idle_working_and_blocked() {
        assert_eq!(state_color("idle"), Color::Green);
        assert_eq!(state_color("working"), Color::Yellow);
        assert_eq!(state_color("blocked_permission"), Color::Red);
        assert_eq!(state_color("exited"), Color::Red);
        assert_eq!(state_color("interrupted"), Color::Red);
        assert_eq!(state_color("spawning"), Color::Gray);
    }
}
