//! Input routing: which panel has focus, and how keys (`Enter`, `Ctrl-C`,
//! arbitrary keys) reach a panel's PTY. See `docs/design.md` §0 #11, §9.
//!
//! caucus panels are fully bidirectional interactive terminals: the user can
//! type into a focused panel directly (logins, OAuth device codes, ...), and
//! the CEO can drive any panel via the MCP `send_keys` tool.

use crossterm::event::KeyEvent;

use crate::session::id::PanelId;

/// Where a key event should go after focus routing.
#[derive(Debug, Clone)]
pub enum InputAction {
    /// Forward these bytes to the focused panel's PTY.
    ToPanel { panel: PanelId, bytes: Vec<u8> },
    /// A caucus-level shortcut (focus switch, quit, ...) — handled by caucus,
    /// not forwarded to any PTY.
    Caucus(CaucusCommand),
    /// Nothing to do.
    Ignore,
}

/// caucus-level commands triggered by reserved key chords.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CaucusCommand {
    /// Move focus to the next panel.
    FocusNext,
    /// Move focus to the previous panel.
    FocusPrev,
    /// Quit caucus.
    Quit,
}

/// Tracks which panel currently receives the user's keystrokes.
#[derive(Debug, Clone, Default)]
pub struct FocusRouter {
    /// The focused panel, if any panel exists.
    focused: Option<PanelId>,
}

impl FocusRouter {
    /// A router with no panels yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The currently focused panel.
    pub fn focused(&self) -> Option<PanelId> {
        self.focused
    }

    /// Set the focused panel.
    pub fn set_focus(&mut self, panel: Option<PanelId>) {
        self.focused = panel;
    }

    /// Route a key event to an [`InputAction`].
    ///
    /// Phase 2 implements the reserved-chord table and the key → PTY-byte
    /// encoding; the skeleton forwards to the focused panel.
    pub fn route(&self, key: KeyEvent) -> InputAction {
        // TODO(phase 2): detect caucus chords, encode keys to terminal bytes.
        let _ = key;
        match self.focused {
            Some(panel) => InputAction::ToPanel {
                panel,
                bytes: Vec::new(),
            },
            None => InputAction::Ignore,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_focus_routes_to_ignore() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let router = FocusRouter::new();
        let action = router.route(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(matches!(action, InputAction::Ignore));
    }

    #[test]
    fn focus_routes_to_panel() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut router = FocusRouter::new();
        let panel = PanelId::new();
        router.set_focus(Some(panel));
        let action = router.route(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, InputAction::ToPanel { panel: p, .. } if p == panel));
    }
}
