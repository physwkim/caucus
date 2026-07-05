use super::*;
use crate::render::{Direction, Layout, LayoutMode, LayoutTree, Rect};
use crate::session::id::PanelId;
use tracing::warn;

impl Multiplexer {
    /// Resize the whole-screen area and reflow every panel's PTY + grid.
    pub fn resize(&mut self, area: Rect) -> Result<()> {
        self.area = area;
        // A resize is a view change with no other render-signature input (a
        // panel's grid only bumps once its PTY child reacts to the SIGWINCH),
        // so bump the epoch — the next tick repaints the reflowed layout
        // instead of waiting on child output or the forced-redraw safety net.
        self.view_epoch += 1;
        self.reflow();
        // An open scrollback pager draws full-screen over the tiled view; keep
        // its page height (scroll clamp + step) in sync with the new area, or
        // scrolling desyncs from the live-windowed render.
        self.resync_pager_page();
        Ok(())
    }

    /// Project the current layout tree onto [`Multiplexer::area`] and resize
    /// each panel's PTY/grid to its new slot (`docs/design.md` §0 #10).
    ///
    /// When [`Multiplexer::zoom`] names a still-live panel the layout is a
    /// single full-area slot for that panel; otherwise the slots come from
    /// [`Multiplexer::layout_tree`] (empty before the first spawn). This keeps
    /// any manual `Ctrl-A Ctrl-arrow` split ratios across a terminal resize —
    /// only [`Multiplexer::rebuild_layout_tree`] resets them to the preset.
    /// Hidden (un-tiled) panels keep their last PTY size — they are resized
    /// again the moment they reappear in a slot.
    pub(crate) fn reflow(&mut self) {
        let zoomed = self
            .zoom
            .filter(|id| self.panels.iter().any(|p| p.id == *id));
        self.layout = match zoomed {
            Some(id) => Layout {
                slots: vec![(id, self.area)],
            },
            None => Layout {
                slots: self
                    .layout_tree
                    .as_ref()
                    .map(|t| t.rects(self.area))
                    .unwrap_or_default(),
            },
        };
        for panel in &mut self.panels {
            if let Some(rect) = self.layout.rect_of(panel.id)
                && let Err(err) = panel.resize(rect)
            {
                warn!(panel = %panel.id, error = %err, "panel resize failed");
            }
        }
    }

    /// Rebuild the layout tree from the current panels and [`LayoutMode`]
    /// preset, then reflow. Called on every *structural* change — spawn, kill,
    /// move, mode switch — so the tree always matches the live panel set. This
    /// resets any manual split ratios to the preset's even split, matching
    /// tmux `select-layout`: a custom resize lasts only until the arrangement
    /// itself changes. A plain terminal resize keeps the ratios (it calls
    /// [`Multiplexer::reflow`], not this).
    pub(crate) fn rebuild_layout_tree(&mut self) {
        let ids: Vec<PanelId> = self.panels.iter().map(|p| p.id).collect();
        self.layout_tree = LayoutTree::from_preset(&ids, self.layout_mode);
        self.reflow();
    }

    /// Resize the focused panel one step in screen direction `dir` (`Ctrl-A`
    /// then `Ctrl-arrow`): grow it toward `dir` (Right/Down) or shrink it
    /// (Left/Up) by nudging the nearest matching-axis split in the layout tree.
    /// A no-op with no focused panel, no live tree, or while zoomed (a single
    /// full-screen pane has no divider to move).
    pub(crate) fn resize_focused(&mut self, dir: Direction) {
        if self.zoom.is_some() {
            return;
        }
        let Some(focused) = self.focus.focused() else {
            return;
        };
        let Some(tree) = self.layout_tree.as_mut() else {
            return;
        };
        tree.resize(focused, dir);
        self.reflow();
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
        self.rebuild_layout_tree();
        // `order_index` in the record changed.
        self.persist_record();
    }

    /// Move focus to the panel geometrically nearest the focused one in screen
    /// direction `dir` (`Ctrl-A` + arrow). A no-op when there is no focused
    /// panel, no computed slot for it (e.g. it is hidden behind a zoom), or
    /// nothing lies in that direction.
    pub(crate) fn focus_dir(&mut self, dir: Direction) {
        let Some(focused) = self.focus.focused() else {
            return;
        };
        let Some(from) = self.layout.rect_of(focused) else {
            return;
        };
        let candidates = self
            .layout
            .slots
            .iter()
            .filter(|(id, _)| *id != focused)
            .map(|(id, r)| (*id, *r));
        if let Some(target) = crate::render::nearest_in_direction(from, candidates, dir) {
            self.focus.set_focus(Some(target));
        }
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
        self.rebuild_layout_tree();
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

    /// `Ctrl-A Ctrl-arrow` grows the focused pane by perturbing the layout
    /// tree, and the perturbed ratio survives a plain reflow (terminal
    /// SIGWINCH) — only a structural rebuild resets it.
    #[tokio::test]
    async fn resize_focused_grows_the_focused_pane_and_survives_reflow() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let a = PanelId::new();
        let b = PanelId::new();
        mux.layout_tree = LayoutTree::from_preset(&[a, b], LayoutMode::EvenHorizontal);
        mux.focus.set_focus(Some(a));
        mux.reflow();
        let before = mux.layout().rect_of(a).unwrap().width;

        mux.resize_focused(Direction::Right);
        let grown = mux.layout().rect_of(a).unwrap().width;
        assert!(
            grown > before,
            "Ctrl-arrow resize must grow the focused pane: {before} -> {grown}"
        );

        // A plain reflow re-projects the same tree, so the manual ratio holds.
        mux.reflow();
        assert_eq!(
            mux.layout().rect_of(a).unwrap().width,
            grown,
            "the manual split ratio must survive a terminal resize"
        );
    }

    /// `resize_focused` is a no-op without a focused panel and while zoomed (a
    /// full-screen pane has no divider to move).
    #[tokio::test]
    async fn resize_focused_is_a_noop_while_unfocused_or_zoomed() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let a = PanelId::new();
        let b = PanelId::new();
        mux.layout_tree = LayoutTree::from_preset(&[a, b], LayoutMode::EvenHorizontal);

        // No focus: the resize does nothing.
        mux.focus.set_focus(None);
        mux.reflow();
        let even = mux.layout().rect_of(a).unwrap().width;
        mux.resize_focused(Direction::Right);
        assert_eq!(
            mux.layout().rect_of(a).unwrap().width,
            even,
            "no focused panel => no resize"
        );

        // Zoomed: the guard returns before touching the tree, so unzooming
        // still shows the untouched even split.
        mux.focus.set_focus(Some(a));
        mux.zoom = Some(a);
        mux.resize_focused(Direction::Right);
        mux.zoom = None;
        mux.reflow();
        assert_eq!(
            mux.layout().rect_of(a).unwrap().width,
            even,
            "zoom guards the resize — the tree ratio is untouched"
        );
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
