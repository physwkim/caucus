//! Input routing: which panel has focus, and how keys (`Enter`, `Ctrl-C`,
//! arbitrary keys) reach a panel's PTY. See `docs/design.md` §0 #11, §9.
//!
//! caucus panels are fully bidirectional interactive terminals: the user can
//! type into a focused panel directly (logins, OAuth device codes, ...), and
//! the main worker can drive any panel via the MCP `send_keys` tool.
//!
//! # Keymap
//!
//! caucus reserves a single **prefix key**, `Ctrl-A`, for its own commands.
//! Every other keystroke — including `Ctrl-C` — is encoded to terminal bytes
//! and forwarded verbatim to the focused panel's PTY, so an agent CLI sees a
//! real terminal.
//!
//! | Key                       | Action                                  |
//! |---------------------------|-----------------------------------------|
//! | `Ctrl-A` then `n` / `→`   | focus the next panel                    |
//! | `Ctrl-A` then `p` / `←`   | focus the previous panel                |
//! | `Ctrl-A` then `q`         | quit caucus                             |
//! | `Ctrl-A` then `z`         | toggle zoom on the focused panel        |
//! | `Ctrl-A` then `<`         | move the focused panel one step earlier |
//! | `Ctrl-A` then `>`         | move the focused panel one step later   |
//! | `Ctrl-A` then `Space`     | cycle the layout arrangement mode       |
//! | `Ctrl-A` then `t`         | toggle the transcript overlay           |
//! | `Esc` (overlay open)      | hide the transcript overlay             |
//! | `Ctrl-A` then `Ctrl-A`    | send a literal `Ctrl-A` to the panel    |
//! | any other key             | forwarded to the focused panel's PTY    |
//! | `Ctrl-C`                  | forwarded to the focused panel (§0 #11) |
//!
//! The prefix is consumed: after `Ctrl-A` the next key selects a command and
//! is *not* forwarded, except `Ctrl-A Ctrl-A` which forwards one literal
//! `Ctrl-A` (so the prefix byte itself can still reach a panel).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::session::id::PanelId;

/// The reserved prefix key chord (`Ctrl-A`).
const PREFIX_CHAR: char = 'a';

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
    /// Toggle full-screen zoom on the focused panel.
    ToggleZoom,
    /// Move the focused panel one step earlier in the panel order.
    MovePanelEarlier,
    /// Move the focused panel one step later in the panel order.
    MovePanelLater,
    /// Cycle the arrangement mode (`Tiled` → `EvenHorizontal` → ...).
    CycleLayout,
    /// Toggle the read-only transcript overlay.
    ToggleTranscript,
    /// Hide the transcript overlay (the `Esc` path while it is open).
    HideTranscript,
}

/// Tracks which panel currently receives the user's keystrokes, plus whether
/// the reserved prefix key is pending.
#[derive(Debug, Clone, Default)]
pub struct FocusRouter {
    /// The focused panel, if any panel exists.
    focused: Option<PanelId>,
    /// `true` after the prefix key was pressed and before the next key.
    prefix_armed: bool,
    /// `true` while the transcript overlay is open. When set, a bare `Esc`
    /// hides the overlay instead of being forwarded to the focused panel.
    transcript_open: bool,
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

    /// Whether the prefix key is armed (next key is a caucus command).
    pub fn prefix_armed(&self) -> bool {
        self.prefix_armed
    }

    /// Set the focused panel.
    pub fn set_focus(&mut self, panel: Option<PanelId>) {
        self.focused = panel;
    }

    /// Tell the router whether the transcript overlay is open — gates the
    /// `Esc`-hides-overlay diversion in [`FocusRouter::route`].
    pub fn set_transcript_open(&mut self, open: bool) {
        self.transcript_open = open;
    }

    /// Route a key event to an [`InputAction`].
    ///
    /// When the prefix is armed the key selects a [`CaucusCommand`]; an
    /// unrecognised key after the prefix is dropped (the prefix is consumed
    /// either way). Otherwise the key is encoded to terminal bytes and
    /// forwarded to the focused panel.
    pub fn route(&mut self, key: KeyEvent) -> InputAction {
        if self.prefix_armed {
            self.prefix_armed = false;
            return self.route_prefixed(key);
        }
        if is_prefix(&key) {
            self.prefix_armed = true;
            return InputAction::Ignore;
        }
        // While the transcript overlay is open, a bare `Esc` hides it rather
        // than reaching the focused panel. Every other key still passes
        // through — the overlay is read-only and does not capture input.
        if self.transcript_open
            && key.code == KeyCode::Esc
            && key.modifiers.is_empty()
        {
            return InputAction::Caucus(CaucusCommand::HideTranscript);
        }
        match self.focused {
            Some(panel) => match encode_key(&key) {
                Some(bytes) => InputAction::ToPanel { panel, bytes },
                None => InputAction::Ignore,
            },
            None => InputAction::Ignore,
        }
    }

    /// Interpret a key pressed *after* the prefix.
    fn route_prefixed(&self, key: KeyEvent) -> InputAction {
        // `Ctrl-A Ctrl-A` forwards one literal prefix byte to the panel.
        if is_prefix(&key) {
            return match self.focused {
                Some(panel) => InputAction::ToPanel {
                    panel,
                    bytes: vec![0x01],
                },
                None => InputAction::Ignore,
            };
        }
        match key.code {
            KeyCode::Char('n') | KeyCode::Right => {
                InputAction::Caucus(CaucusCommand::FocusNext)
            }
            KeyCode::Char('p') | KeyCode::Left => {
                InputAction::Caucus(CaucusCommand::FocusPrev)
            }
            KeyCode::Char('q') => InputAction::Caucus(CaucusCommand::Quit),
            KeyCode::Char('z') => InputAction::Caucus(CaucusCommand::ToggleZoom),
            KeyCode::Char('<') => {
                InputAction::Caucus(CaucusCommand::MovePanelEarlier)
            }
            KeyCode::Char('>') => {
                InputAction::Caucus(CaucusCommand::MovePanelLater)
            }
            KeyCode::Char(' ') => InputAction::Caucus(CaucusCommand::CycleLayout),
            KeyCode::Char('t') => {
                InputAction::Caucus(CaucusCommand::ToggleTranscript)
            }
            // Any other key after the prefix: prefix consumed, nothing done.
            _ => InputAction::Ignore,
        }
    }
}

/// Whether `key` is the reserved caucus prefix (`Ctrl-A`).
fn is_prefix(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&PREFIX_CHAR))
}

/// Encode a crossterm [`KeyEvent`] into the byte sequence a terminal would
/// send for that key — the fully bidirectional input path (`docs/design.md`
/// §0 #11).
///
/// Covers printable characters, `Ctrl-<letter>` control codes (including
/// `Ctrl-C` → `0x03`, which by design reaches the panel), `Enter`, `Tab`,
/// `Backspace`, `Esc`, and the arrow / navigation keys as their xterm escape
/// sequences. Returns `None` for keys with no terminal encoding (e.g. a bare
/// modifier press).
pub fn encode_key(key: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let mut bytes: Vec<u8> = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Control codes: Ctrl-A..Ctrl-Z -> 0x01..0x1A, plus the
                // standard punctuation control mappings.
                encode_ctrl_char(c)?
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::F(n) => encode_function_key(n)?,
        // Bare modifier presses and other non-text keys have no encoding.
        _ => return None,
    };

    // Alt prefixes the sequence with ESC (xterm meta convention).
    if alt {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x1b);
        out.append(&mut bytes);
        return Some(out);
    }
    Some(bytes)
}

/// Map a `Ctrl-<char>` chord to its control byte. `Ctrl-A`..`Ctrl-Z` collapse
/// to `0x01`..`0x1A`; the punctuation chords match xterm.
fn encode_ctrl_char(c: char) -> Option<Vec<u8>> {
    let b = match c {
        'a'..='z' => (c as u8) - b'a' + 1,
        'A'..='Z' => (c as u8) - b'A' + 1,
        ' ' | '@' => 0x00,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' | '/' => 0x1f,
        '?' => 0x7f,
        _ => return None,
    };
    Some(vec![b])
}

/// Map `F1`..`F12` to their xterm escape sequences.
fn encode_function_key(n: u8) -> Option<Vec<u8>> {
    let seq: &[u8] = match n {
        1 => b"\x1bOP",
        2 => b"\x1bOQ",
        3 => b"\x1bOR",
        4 => b"\x1bOS",
        5 => b"\x1b[15~",
        6 => b"\x1b[17~",
        7 => b"\x1b[18~",
        8 => b"\x1b[19~",
        9 => b"\x1b[20~",
        10 => b"\x1b[21~",
        11 => b"\x1b[23~",
        12 => b"\x1b[24~",
        _ => return None,
    };
    Some(seq.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn no_focus_routes_to_ignore() {
        let mut router = FocusRouter::new();
        let action = router.route(key(KeyCode::Char('a')));
        assert!(matches!(action, InputAction::Ignore));
    }

    #[test]
    fn focus_routes_printable_to_panel() {
        let mut router = FocusRouter::new();
        let panel = PanelId::new();
        router.set_focus(Some(panel));
        let action = router.route(key(KeyCode::Char('x')));
        match action {
            InputAction::ToPanel { panel: p, bytes } => {
                assert_eq!(p, panel);
                assert_eq!(bytes, b"x");
            }
            other => panic!("expected ToPanel, got {other:?}"),
        }
    }

    #[test]
    fn enter_encodes_to_carriage_return() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        match router.route(key(KeyCode::Enter)) {
            InputAction::ToPanel { bytes, .. } => assert_eq!(bytes, b"\r"),
            other => panic!("expected ToPanel, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_c_is_forwarded_to_the_panel() {
        // design.md §0 #11: Ctrl-C goes to the focused panel, not caucus.
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        match router.route(ctrl('c')) {
            InputAction::ToPanel { bytes, .. } => assert_eq!(bytes, vec![0x03]),
            other => panic!("expected ToPanel with 0x03, got {other:?}"),
        }
    }

    #[test]
    fn prefix_then_n_is_focus_next() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        assert!(matches!(router.route(ctrl('a')), InputAction::Ignore));
        assert!(router.prefix_armed());
        let action = router.route(key(KeyCode::Char('n')));
        assert!(matches!(
            action,
            InputAction::Caucus(CaucusCommand::FocusNext)
        ));
        assert!(!router.prefix_armed());
    }

    #[test]
    fn prefix_then_p_is_focus_prev() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.route(ctrl('a'));
        assert!(matches!(
            router.route(key(KeyCode::Char('p'))),
            InputAction::Caucus(CaucusCommand::FocusPrev)
        ));
    }

    #[test]
    fn prefix_then_q_is_quit() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.route(ctrl('a'));
        assert!(matches!(
            router.route(key(KeyCode::Char('q'))),
            InputAction::Caucus(CaucusCommand::Quit)
        ));
    }

    #[test]
    fn prefix_then_z_is_toggle_zoom() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.route(ctrl('a'));
        assert!(matches!(
            router.route(key(KeyCode::Char('z'))),
            InputAction::Caucus(CaucusCommand::ToggleZoom)
        ));
    }

    #[test]
    fn prefix_then_lt_gt_move_the_panel() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.route(ctrl('a'));
        assert!(matches!(
            router.route(key(KeyCode::Char('<'))),
            InputAction::Caucus(CaucusCommand::MovePanelEarlier)
        ));
        router.route(ctrl('a'));
        assert!(matches!(
            router.route(key(KeyCode::Char('>'))),
            InputAction::Caucus(CaucusCommand::MovePanelLater)
        ));
    }

    #[test]
    fn prefix_then_space_is_cycle_layout() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.route(ctrl('a'));
        assert!(matches!(
            router.route(key(KeyCode::Char(' '))),
            InputAction::Caucus(CaucusCommand::CycleLayout)
        ));
    }

    #[test]
    fn prefix_then_t_is_toggle_transcript() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.route(ctrl('a'));
        assert!(matches!(
            router.route(key(KeyCode::Char('t'))),
            InputAction::Caucus(CaucusCommand::ToggleTranscript)
        ));
    }

    #[test]
    fn esc_hides_transcript_only_while_overlay_open() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));

        // Overlay closed: `Esc` still reaches the focused panel as today.
        match router.route(key(KeyCode::Esc)) {
            InputAction::ToPanel { bytes, .. } => assert_eq!(bytes, vec![0x1b]),
            other => panic!("expected ToPanel with 0x1b, got {other:?}"),
        }

        // Overlay open: `Esc` diverts to a hide command, not the panel.
        router.set_transcript_open(true);
        assert!(matches!(
            router.route(key(KeyCode::Esc)),
            InputAction::Caucus(CaucusCommand::HideTranscript)
        ));

        // Other keys still pass through to the panel while the overlay is open.
        match router.route(key(KeyCode::Char('x'))) {
            InputAction::ToPanel { bytes, .. } => assert_eq!(bytes, b"x"),
            other => panic!("expected ToPanel, got {other:?}"),
        }
    }

    #[test]
    fn prefix_then_prefix_forwards_a_literal_ctrl_a() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.route(ctrl('a'));
        match router.route(ctrl('a')) {
            InputAction::ToPanel { bytes, .. } => assert_eq!(bytes, vec![0x01]),
            other => panic!("expected literal Ctrl-A, got {other:?}"),
        }
    }

    #[test]
    fn prefix_then_unknown_key_is_consumed() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.route(ctrl('a'));
        // 'k' is not a caucus command — consumed, nothing forwarded.
        assert!(matches!(
            router.route(key(KeyCode::Char('k'))),
            InputAction::Ignore
        ));
        assert!(!router.prefix_armed());
    }

    #[test]
    fn bare_ctrl_a_arms_prefix_and_is_not_forwarded() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        assert!(matches!(router.route(ctrl('a')), InputAction::Ignore));
    }

    #[test]
    fn arrow_keys_encode_to_xterm_sequences() {
        assert_eq!(encode_key(&key(KeyCode::Up)).unwrap(), b"\x1b[A");
        assert_eq!(encode_key(&key(KeyCode::Down)).unwrap(), b"\x1b[B");
        assert_eq!(encode_key(&key(KeyCode::Right)).unwrap(), b"\x1b[C");
        assert_eq!(encode_key(&key(KeyCode::Left)).unwrap(), b"\x1b[D");
    }

    #[test]
    fn ctrl_letters_encode_to_control_codes() {
        assert_eq!(encode_key(&ctrl('a')).unwrap(), vec![0x01]);
        assert_eq!(encode_key(&ctrl('z')).unwrap(), vec![0x1a]);
        assert_eq!(encode_key(&ctrl('d')).unwrap(), vec![0x04]);
    }

    #[test]
    fn alt_prefixes_with_escape() {
        let k = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT);
        assert_eq!(encode_key(&k).unwrap(), vec![0x1b, b'b']);
    }

    #[test]
    fn backspace_and_tab_and_esc_encode() {
        assert_eq!(encode_key(&key(KeyCode::Backspace)).unwrap(), vec![0x7f]);
        assert_eq!(encode_key(&key(KeyCode::Tab)).unwrap(), vec![b'\t']);
        assert_eq!(encode_key(&key(KeyCode::Esc)).unwrap(), vec![0x1b]);
    }

    #[test]
    fn function_key_encodes() {
        assert_eq!(encode_key(&key(KeyCode::F(1))).unwrap(), b"\x1bOP");
        assert_eq!(encode_key(&key(KeyCode::F(5))).unwrap(), b"\x1b[15~");
    }
}
