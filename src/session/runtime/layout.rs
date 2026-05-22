use super::*;
use crate::render::{Layout, LayoutMode, Rect};
use crate::session::id::PanelId;
use tracing::warn;

impl Multiplexer {
    /// Resize the whole-screen area and reflow every panel's PTY + grid.
    pub fn resize(&mut self, area: Rect) -> Result<()> {
        self.area = area;
        self.reflow();
        Ok(())
    }

    /// Recompute the layout for the current panels and resize each panel's
    /// PTY/grid to its new slot (`docs/design.md` §0 #10).
    ///
    /// When [`Multiplexer::zoom`] names a still-live panel the layout is a
    /// single full-area slot for that panel; otherwise the panels tile per
    /// the current [`LayoutMode`]. Hidden (un-tiled) panels keep their last
    /// PTY size — they are resized again the moment they reappear in a slot.
    pub(crate) fn reflow(&mut self) {
        let ids: Vec<PanelId> = self.panels.iter().map(|p| p.id).collect();
        let zoomed = self
            .zoom
            .filter(|id| self.panels.iter().any(|p| p.id == *id));
        self.layout = match zoomed {
            Some(id) => Layout {
                slots: vec![(id, self.area)],
            },
            None => Layout::reflow(&ids, self.area, self.layout_mode),
        };
        for panel in &mut self.panels {
            if let Some(rect) = self.layout.rect_of(panel.id) {
                if let Err(err) = panel.resize(rect) {
                    warn!(panel = %panel.id, error = %err, "panel resize failed");
                }
            }
        }
    }

    /// Toggle full-screen zoom on the focused panel. A second toggle (or a
    /// toggle while a different panel is zoomed) restores the tiled layout
    /// or moves the zoom; with no focused panel it is a no-op.
    pub(crate) fn toggle_zoom(&mut self) {
        let Some(focused) = self.focus.focused() else {
            return;
        };
        self.zoom = if self.zoom == Some(focused) {
            None
        } else {
            Some(focused)
        };
        self.reflow();
    }

    /// Move the focused panel one step (`delta` = -1 earlier, +1 later) in the
    /// panel order — which is also the tile order and the focus-cycle order.
    /// A no-op when there is no focused panel or it is already at the end.
    pub(crate) fn move_panel(&mut self, delta: isize) {
        let Some(focused) = self.focus.focused() else {
            return;
        };
        let Some(idx) = self.panels.iter().position(|p| p.id == focused) else {
            return;
        };
        let target = idx as isize + delta;
        if target < 0 || target as usize >= self.panels.len() {
            return;
        }
        self.panels.swap(idx, target as usize);
        self.reflow();
        // `order_index` in the record changed.
        self.persist_record();
    }

    /// Move focus by `delta` panels, wrapping around.
    pub(crate) fn cycle_focus(&mut self, delta: isize) {
        if self.panels.is_empty() {
            return;
        }
        let cur = self
            .focus
            .focused()
            .and_then(|id| self.panels.iter().position(|p| p.id == id))
            .unwrap_or(0);
        let n = self.panels.len() as isize;
        let next = ((cur as isize + delta) % n + n) % n;
        self.focus.set_focus(Some(self.panels[next as usize].id));
    }

    /// The current panel arrangement mode (for the status bar).
    pub fn layout_mode(&self) -> LayoutMode {
        self.layout_mode
    }

    /// Set the panel arrangement mode and reflow — used by `caucus resume` to
    /// restore the persisted layout. Does not persist the record itself; the
    /// resume path persists once after the whole roster is rebuilt.
    pub fn set_layout_mode(&mut self, mode: LayoutMode) {
        self.layout_mode = mode;
        self.reflow();
    }

    /// The zoomed panel id, if a panel is currently zoomed.
    pub fn zoomed(&self) -> Option<PanelId> {
        self.zoom
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::CaucusCommand;
    use crate::session::runtime::test_support::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn cycle_focus_wraps_with_no_panels() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        // No panels: cycling is a no-op, not a panic.
        mux.cycle_focus(1);
        assert!(mux.focused().is_none());
    }

    /// `CycleLayout` advances the arrangement mode through the full cycle and
    /// wraps back to `Tiled`.
    #[tokio::test]
    async fn cycle_layout_advances_the_mode() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        assert_eq!(mux.layout_mode(), LayoutMode::Tiled);
        mux.apply_command(CaucusCommand::CycleLayout);
        assert_eq!(mux.layout_mode(), LayoutMode::EvenHorizontal);
        mux.apply_command(CaucusCommand::CycleLayout);
        assert_eq!(mux.layout_mode(), LayoutMode::EvenVertical);
        mux.apply_command(CaucusCommand::CycleLayout);
        assert_eq!(mux.layout_mode(), LayoutMode::MainVertical);
        mux.apply_command(CaucusCommand::CycleLayout);
        assert_eq!(mux.layout_mode(), LayoutMode::Tiled);
    }

    /// `ToggleZoom` with no focused panel is a no-op (no panic).
    #[tokio::test]
    async fn toggle_zoom_with_no_panels_is_a_noop() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.apply_command(CaucusCommand::ToggleZoom);
        assert!(mux.zoomed().is_none());
    }

    /// The zoom layout is a single full-area slot for the zoomed panel; a
    /// second toggle restores the tiled layout.
    #[tokio::test]
    async fn zoom_yields_one_full_area_slot() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        // Three synthetic panel ids in `panels` would need real panels; the
        // zoom layout is computed in `reflow` from `self.zoom` + `self.area`,
        // so drive it directly with a known id.
        let id = PanelId::new();
        mux.zoom = Some(id);
        // `id` is not a live panel — zoom is filtered to live ids only, so
        // the layout falls back to the (empty) tiled reflow.
        mux.reflow();
        assert!(mux.layout().slots.is_empty());

        // With a live zoomed panel the layout is exactly one full-area slot.
        let Ok(panel) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.focus.set_focus(Some(panel));
        mux.apply_command(CaucusCommand::ToggleZoom);
        assert_eq!(mux.zoomed(), Some(panel));
        assert_eq!(mux.layout().slots.len(), 1);
        assert_eq!(mux.layout().slots[0], (panel, area()));

        // Toggling again restores the tiled layout.
        mux.apply_command(CaucusCommand::ToggleZoom);
        assert!(mux.zoomed().is_none());

        mux.shutdown();
    }

    /// `MovePanelEarlier`/`MovePanelLater` swap adjacent entries in the panel
    /// order; moving past either end is a no-op.
    #[tokio::test]
    async fn move_panel_swaps_order() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Ok(a) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        let Ok(b) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        let order = |m: &Multiplexer| m.panels().iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(order(&mux), vec![a, b]);

        // Focus `a` (index 0) and move it later — order becomes [b, a].
        mux.focus.set_focus(Some(a));
        mux.apply_command(CaucusCommand::MovePanelLater);
        assert_eq!(order(&mux), vec![b, a]);

        // `a` is now last — moving later again is a no-op.
        mux.apply_command(CaucusCommand::MovePanelLater);
        assert_eq!(order(&mux), vec![b, a]);

        // Move `a` back earlier — order returns to [a, b].
        mux.apply_command(CaucusCommand::MovePanelEarlier);
        assert_eq!(order(&mux), vec![a, b]);

        // `a` is first — moving earlier again is a no-op.
        mux.apply_command(CaucusCommand::MovePanelEarlier);
        assert_eq!(order(&mux), vec![a, b]);

        mux.shutdown();
    }

    /// Killing the zoomed panel clears the zoom so the layout never points at
    /// a dead id.
    #[tokio::test]
    async fn killing_the_zoomed_panel_clears_zoom() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Ok(panel) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.focus.set_focus(Some(panel));
        mux.apply_command(CaucusCommand::ToggleZoom);
        assert_eq!(mux.zoomed(), Some(panel));

        Multiplexer::kill_panel(&mut mux, panel).unwrap();
        assert!(
            mux.zoomed().is_none(),
            "zoom must clear when its panel dies"
        );

        mux.shutdown();
    }
}
