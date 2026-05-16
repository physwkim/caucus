//! Render layer: ratatui panel layout, reflow, drawing, focus indication.
//! See `docs/design.md` §0 #3, §9.
//!
//! Panels are dynamic (`docs/design.md` §0 #10): the layout reflows whenever
//! the CEO spawns or kills a panel. The real ratatui drawing is Phase 2.

use crate::session::id::PanelId;

/// A rectangle of the terminal, in cells.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// A computed layout: the screen rectangle assigned to each panel.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// One `(panel, rect)` per visible panel.
    pub slots: Vec<(PanelId, Rect)>,
}

impl Layout {
    /// Reflow `panels` into `area` — a grid-ish split across the live panels.
    ///
    /// Phase 2 implements a real tiling algorithm with a focus-aware split.
    pub fn reflow(panels: &[PanelId], area: Rect) -> Self {
        // TODO(phase 2): real tiling/reflow. Skeleton stacks panels vertically.
        let _ = area;
        Self {
            slots: panels.iter().map(|&id| (id, area)).collect(),
        }
    }
}

/// Draw the full caucus screen for `layout`, marking `focused`.
///
/// Phase 2 wires a ratatui `Frame` and renders each panel's grid.
pub fn draw(layout: &Layout, focused: Option<PanelId>) {
    // TODO(phase 2): ratatui frame draw — per-panel grid blit, focus border.
    let _ = (layout, focused);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflow_assigns_a_slot_per_panel() {
        let panels = vec![PanelId::new(), PanelId::new()];
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let layout = Layout::reflow(&panels, area);
        assert_eq!(layout.slots.len(), 2);
    }
}
