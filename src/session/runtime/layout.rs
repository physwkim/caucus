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
    /// [`Multiplexer::layout_tree`] (empty before the first spawn) re-projected
    /// onto the current area, so a plain terminal resize repaints the same
    /// arrangement without a structural rebuild. Hidden (un-tiled) panels keep
    /// their last PTY size — they are resized again the moment they reappear in
    /// a slot.
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
    /// preset, then reflow. Called on every *structural* change — spawn, kill —
    /// so the tree always matches the live panel set. A plain terminal resize
    /// re-projects the existing tree instead (it calls [`Multiplexer::reflow`],
    /// not this).
    pub(crate) fn rebuild_layout_tree(&mut self) {
        let ids: Vec<PanelId> = self.panels.iter().map(|p| p.id).collect();
        self.layout_tree = LayoutTree::from_preset(&ids, self.layout_mode);
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

    /// A fresh session seeds its fixed arrangement from `[settings] layout`
    /// (the sole selector now that runtime cycling is gone).
    #[tokio::test]
    async fn layout_mode_is_seeded_from_config_settings() {
        let tmp = TempDir::new().unwrap();
        // Default config resolves `layout` to `Tiled`.
        let mut config = crate::config::Config::load(tmp.path()).unwrap();
        assert_eq!(mux(&tmp).layout_mode(), LayoutMode::Tiled);

        // A non-`Tiled` setting flows through `Multiplexer::new` into the live
        // arrangement mode.
        config.settings.layout = LayoutMode::MainVertical;
        let session = crate::session::state::Session::new("test", tmp.path().to_path_buf());
        let (mux, _signal, _control) = Multiplexer::new(
            session,
            config,
            area(),
            'a',
            crate::session::LaunchMode::Fresh,
        )
        .unwrap();
        assert_eq!(mux.layout_mode(), LayoutMode::MainVertical);
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
