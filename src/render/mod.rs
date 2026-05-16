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

/// A computed layout: the screen rectangle assigned to each panel.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// One `(panel, rect)` per visible panel.
    pub slots: Vec<(PanelId, Rect)>,
}

impl Layout {
    /// Reflow `panels` into `area` — an even grid split across the live panels
    /// (`docs/design.md` §0 #10: caucus reflows on every spawn/kill).
    ///
    /// Algorithm: pick `cols = ceil(sqrt(n))` columns and
    /// `rows = ceil(n / cols)` rows, then hand each panel one cell. The last
    /// row's panels widen to absorb the remainder when `n` is not a perfect
    /// rectangle, and rounding slack is distributed cell-by-cell so the tiles
    /// exactly partition `area` with no gaps or overlap.
    pub fn reflow(panels: &[PanelId], area: Rect) -> Self {
        let n = panels.len();
        if n == 0 || area.width == 0 || area.height == 0 {
            return Self::default();
        }

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
pub fn draw(frame: &mut Frame, layout: &Layout, panels: &[Panel], focused: Option<PanelId>) {
    for (id, rect) in &layout.slots {
        let Some(panel) = panels.iter().find(|p| p.id == *id) else {
            continue;
        };
        draw_panel(frame, panel, *rect, focused == Some(*id));
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

    #[test]
    fn reflow_assigns_a_slot_per_panel() {
        let panels = vec![PanelId::new(), PanelId::new()];
        let layout = Layout::reflow(&panels, area());
        assert_eq!(layout.slots.len(), 2);
    }

    #[test]
    fn reflow_empty_is_empty() {
        assert!(Layout::reflow(&[], area()).slots.is_empty());
    }

    #[test]
    fn single_panel_fills_the_whole_area() {
        let id = PanelId::new();
        let layout = Layout::reflow(&[id], area());
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
        let layout = Layout::reflow(&panels, area());
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
        let layout = Layout::reflow(&panels, area());
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
        let layout = Layout::reflow(&panels, area());
        // Every cell of the 80x24 area must be covered exactly once.
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
