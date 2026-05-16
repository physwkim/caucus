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

use ratatui::Frame;
use ratatui::layout::Rect as TuiRect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::panel::Panel;
use crate::session::id::PanelId;
use crate::term::Grid;
use crate::term::grid::attr;

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
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
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
    /// rounding slack distributed cell-by-cell via [`split`].
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
            let cells_in_row = if in_last_row {
                n - row * cols
            } else {
                cols
            };
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
            Style::default().fg(if focused {
                Color::Cyan
            } else {
                Color::Gray
            }),
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
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(take_cols);
        let mut c = 0;
        while c < take_cols {
            let Some(cell) = grid.cell(r, c) else { break };
            if cell.ch == '\0' {
                // Trailing half of a wide glyph — already emitted by the lead.
                c += 1;
                continue;
            }
            spans.push(Span::styled(cell.ch.to_string(), cell_style(cell)));
            c += 1;
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
        1 => Some(Color::Red),
        2 => Some(Color::Red),
        3 => Some(Color::Green),
        4 => Some(Color::Yellow),
        5 => Some(Color::Blue),
        6 => Some(Color::Magenta),
        7 => Some(Color::Cyan),
        8 => Some(Color::Gray),
        9 => Some(Color::Black),
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
    fn reflow_empty_is_empty() {
        assert!(
            Layout::reflow(&[], area(), LayoutMode::Tiled)
                .slots
                .is_empty()
        );
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
}
