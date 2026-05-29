//! Input routing: which panel has focus, and how keys (`Enter`, `Ctrl-C`,
//! arbitrary keys) reach a panel's PTY. See `docs/design.md` §0 #11, §9.
//!
//! caucus panels are fully bidirectional interactive terminals: the user can
//! type into a focused panel directly (logins, OAuth device codes, ...), and
//! the main worker can drive any panel via the MCP `send_keys` tool.
//!
//! # Keymap
//!
//! caucus reserves a single **prefix key**, `Ctrl-A` by default, for its own
//! commands. The prefix is configurable (`--prefix` / `CAUCUS_PREFIX`, see
//! [`FocusRouter::with_prefix`]) so it can dodge a collision with an outer
//! multiplexer — set `Ctrl-B` when tmux is remapped to `Ctrl-A`. The table
//! below shows the default; substitute your prefix for `Ctrl-A`. Every other
//! keystroke — including `Ctrl-C` — is encoded to terminal bytes and forwarded
//! verbatim to the focused panel's PTY, so an agent CLI sees a real terminal.
//!
//! | Key                       | Action                                  |
//! |---------------------------|-----------------------------------------|
//! | `Ctrl-A` then `n`         | focus the next panel (cycle order)      |
//! | `Ctrl-A` then `p`         | focus the previous panel (cycle order)  |
//! | `Ctrl-A` then `↑↓←→`      | focus the panel in that direction       |
//! | `Ctrl-A` then `Ctrl-↑↓←→` | resize the focused panel (tmux-style)   |
//! | `Ctrl-A` then `q`         | quit caucus                             |
//! | `Ctrl-A` then `z`         | toggle zoom on the focused panel        |
//! | `Ctrl-A` then `<`         | move the focused panel one step earlier |
//! | `Ctrl-A` then `>`         | move the focused panel one step later   |
//! | `Ctrl-A` then `x`         | close the focused panel (y/n confirm)   |
//! | `Ctrl-A` then `Space`     | cycle the layout arrangement mode       |
//! | `Ctrl-A` then `t`         | toggle the transcript overlay           |
//! | `Esc` (overlay open)      | hide the transcript overlay             |
//! | `Ctrl-A` then `[`         | open the scrollback pager (focused panel)|
//! | (pager open) `↑↓ k j`     | scroll a line; `PgUp/PgDn` a page       |
//! | (pager open) `g G Home End`| jump to oldest / newest line           |
//! | (pager open) `Esc` / `q`  | exit the scrollback pager               |
//! | `Ctrl-A` then `Ctrl-A`    | send a literal `Ctrl-A` to the panel    |
//! | any other key             | forwarded to the focused panel's PTY    |
//! | `Ctrl-C`                  | forwarded to the focused panel (§0 #11) |
//!
//! The prefix is consumed: after `Ctrl-A` the next key selects a command and
//! is *not* forwarded, except `Ctrl-A Ctrl-A` which forwards one literal
//! `Ctrl-A` (so the prefix byte itself can still reach a panel).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::render::Direction;
use crate::session::id::PanelId;

/// The default prefix letter — `Ctrl-A` — used when none is configured.
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
    /// Move focus to the next panel (linear cycle order).
    FocusNext,
    /// Move focus to the previous panel (linear cycle order).
    FocusPrev,
    /// Move focus to the panel geometrically in the given screen direction
    /// (`Ctrl-A` + arrow) — tmux-style directional navigation.
    FocusDir(Direction),
    /// Resize the focused panel one step in the given screen direction
    /// (`Ctrl-A` + `Ctrl`-arrow): grow it toward `Right`/`Down`, shrink it on
    /// `Left`/`Up` — tmux-style pane resize.
    ResizeDir(Direction),
    /// Quit caucus.
    Quit,
    /// Toggle full-screen zoom on the focused panel.
    ToggleZoom,
    /// Move the focused panel one step earlier in the panel order.
    MovePanelEarlier,
    /// Move the focused panel one step later in the panel order.
    MovePanelLater,
    /// Close the focused panel (`Ctrl-A x`) — arms a y/n confirm prompt. The
    /// main worker panel is protected and cannot be closed.
    CloseFocused,
    /// Confirm the pending panel close (`y` while the confirm prompt is open).
    ConfirmClose,
    /// Cancel the pending panel close (`n`/`Esc`/`Ctrl-C` while it is open).
    CancelClose,
    /// Cycle the arrangement mode (`Tiled` → `EvenHorizontal` → ...).
    CycleLayout,
    /// Toggle the read-only transcript overlay.
    ToggleTranscript,
    /// Hide the transcript overlay (the `Esc` path while it is open).
    HideTranscript,
    /// Open the scrollback pager on the focused panel (`Ctrl-A [`).
    EnterScroll,
    /// Close the scrollback pager, returning to the live tiled view.
    ExitScroll,
    /// Scroll the pager one line toward older output.
    ScrollUp,
    /// Scroll the pager one line toward newer output.
    ScrollDown,
    /// Scroll the pager one page toward older output.
    ScrollPageUp,
    /// Scroll the pager one page toward newer output.
    ScrollPageDown,
    /// Jump the pager to the oldest line.
    ScrollTop,
    /// Jump the pager to the newest line.
    ScrollBottom,
}

/// Tracks which panel currently receives the user's keystrokes, plus whether
/// the reserved prefix key is pending.
#[derive(Debug, Clone)]
pub struct FocusRouter {
    /// The reserved prefix letter — caucus commands are `Ctrl-<prefix>`
    /// (default `'a'` → `Ctrl-A`). Configurable so it can dodge a collision
    /// with an outer multiplexer (e.g. a tmux remapped to `Ctrl-A`).
    prefix: char,
    /// The focused panel, if any panel exists.
    focused: Option<PanelId>,
    /// `true` after the prefix key was pressed and before the next key.
    prefix_armed: bool,
    /// `true` while the transcript overlay is open. When set, a bare `Esc`
    /// hides the overlay instead of being forwarded to the focused panel.
    transcript_open: bool,
    /// `true` while the scrollback pager is open. When set, navigation keys
    /// drive scrolling and are *captured* — every key is consumed by the
    /// pager and none reach the focused panel's PTY.
    scroll_open: bool,
    /// `true` while the close-panel confirm prompt is open. Modal like the
    /// pager: `y` confirms, `n`/`Esc`/`Ctrl-C` cancels, every other key is
    /// swallowed and never reaches the focused panel's PTY.
    confirm_open: bool,
}

impl Default for FocusRouter {
    fn default() -> Self {
        Self::with_prefix(PREFIX_CHAR)
    }
}

impl FocusRouter {
    /// A router with no panels yet, using the default `Ctrl-A` prefix.
    pub fn new() -> Self {
        Self::default()
    }

    /// A router reserving `Ctrl-<prefix>` (case-insensitive) for caucus
    /// commands. `prefix` is the bare letter — `'b'` means `Ctrl-B`.
    pub fn with_prefix(prefix: char) -> Self {
        Self {
            prefix,
            focused: None,
            prefix_armed: false,
            transcript_open: false,
            scroll_open: false,
            confirm_open: false,
        }
    }

    /// The reserved prefix letter — caucus commands are `Ctrl-<this>`.
    pub fn prefix(&self) -> char {
        self.prefix
    }

    /// Whether `key` is the reserved caucus prefix (`Ctrl-<prefix>`,
    /// case-insensitive).
    fn is_prefix(&self, key: &KeyEvent) -> bool {
        key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&self.prefix))
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

    /// Tell the router whether the scrollback pager is open — when open, the
    /// pager captures every key in [`FocusRouter::route`] (navigation keys
    /// scroll, all others are swallowed).
    pub fn set_scroll_open(&mut self, open: bool) {
        self.scroll_open = open;
    }

    /// Tell the router whether the close-panel confirm prompt is open — when
    /// open, [`FocusRouter::route`] captures every key (`y` confirms,
    /// `n`/`Esc`/`Ctrl-C` cancels, all others are swallowed).
    pub fn set_confirm_open(&mut self, open: bool) {
        self.confirm_open = open;
    }

    /// Route a key event to an [`InputAction`].
    ///
    /// When the prefix is armed the key selects a [`CaucusCommand`]; an
    /// unrecognised key after the prefix is dropped (the prefix is consumed
    /// either way). Otherwise the key is encoded to terminal bytes and
    /// forwarded to the focused panel.
    pub fn route(&mut self, key: KeyEvent) -> InputAction {
        // The close-panel confirm prompt is modal and takes precedence over
        // everything (including the prefix): `y` confirms, `n`/`Esc`/`Ctrl-C`
        // cancels, every other key is swallowed so a stray keystroke can
        // neither confirm a destructive close nor leak to the panel.
        if self.confirm_open {
            return confirm_command(&key)
                .map(InputAction::Caucus)
                .unwrap_or(InputAction::Ignore);
        }
        // While the scrollback pager is open it is fully modal: it captures
        // every key. Navigation keys scroll it; `Esc`/`q` close it; all other
        // keys are swallowed and never reach the focused panel's PTY (unlike
        // the read-only transcript overlay, which passes input through).
        if self.scroll_open {
            return scroll_command(&key)
                .map(InputAction::Caucus)
                .unwrap_or(InputAction::Ignore);
        }
        if self.prefix_armed {
            self.prefix_armed = false;
            return self.route_prefixed(key);
        }
        if self.is_prefix(&key) {
            self.prefix_armed = true;
            return InputAction::Ignore;
        }
        // While the transcript overlay is open, a bare `Esc` hides it rather
        // than reaching the focused panel. Every other key still passes
        // through — the overlay is read-only and does not capture input.
        if self.transcript_open && key.code == KeyCode::Esc && key.modifiers.is_empty() {
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
        // `prefix prefix` forwards one literal prefix byte to the panel. The
        // control code for `Ctrl-<letter>` is the letter with its top three
        // bits cleared (`Ctrl-A` → 0x01, `Ctrl-B` → 0x02, ...).
        if self.is_prefix(&key) {
            return match self.focused {
                Some(panel) => InputAction::ToPanel {
                    panel,
                    bytes: vec![(self.prefix as u8) & 0x1f],
                },
                None => InputAction::Ignore,
            };
        }
        // Arrows are directional (tmux-style). A bare arrow moves focus to the
        // panel in that screen direction; holding `Ctrl` resizes the focused
        // panel toward it instead. n/p remain the linear focus cycle.
        if let Some(dir) = arrow_direction(key.code) {
            let cmd = if key.modifiers.contains(KeyModifiers::CONTROL) {
                CaucusCommand::ResizeDir(dir)
            } else {
                CaucusCommand::FocusDir(dir)
            };
            return InputAction::Caucus(cmd);
        }
        match key.code {
            KeyCode::Char('n') => InputAction::Caucus(CaucusCommand::FocusNext),
            KeyCode::Char('p') => InputAction::Caucus(CaucusCommand::FocusPrev),
            KeyCode::Char('q') => InputAction::Caucus(CaucusCommand::Quit),
            KeyCode::Char('z') => InputAction::Caucus(CaucusCommand::ToggleZoom),
            KeyCode::Char('<') => InputAction::Caucus(CaucusCommand::MovePanelEarlier),
            KeyCode::Char('>') => InputAction::Caucus(CaucusCommand::MovePanelLater),
            KeyCode::Char('x') => InputAction::Caucus(CaucusCommand::CloseFocused),
            KeyCode::Char(' ') => InputAction::Caucus(CaucusCommand::CycleLayout),
            KeyCode::Char('t') => InputAction::Caucus(CaucusCommand::ToggleTranscript),
            KeyCode::Char('[') => InputAction::Caucus(CaucusCommand::EnterScroll),
            // Any other key after the prefix: prefix consumed, nothing done.
            _ => InputAction::Ignore,
        }
    }
}

/// Map a key to a scrollback-pager command while the pager is open. Returns
/// `None` for keys the pager swallows (they reach neither caucus nor the PTY).
///
/// Bindings mirror tmux copy-mode / `less`: `↑↓ k j` line, `PgUp/PgDn b Space`
/// page, `g/Home` oldest, `G/End` newest, `Esc/q` exit.
fn scroll_command(key: &KeyEvent) -> Option<CaucusCommand> {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => Some(CaucusCommand::ScrollUp),
        KeyCode::Down | KeyCode::Char('j') => Some(CaucusCommand::ScrollDown),
        KeyCode::PageUp | KeyCode::Char('b') => Some(CaucusCommand::ScrollPageUp),
        KeyCode::PageDown | KeyCode::Char(' ') => Some(CaucusCommand::ScrollPageDown),
        KeyCode::Home | KeyCode::Char('g') => Some(CaucusCommand::ScrollTop),
        KeyCode::End | KeyCode::Char('G') => Some(CaucusCommand::ScrollBottom),
        KeyCode::Esc | KeyCode::Char('q') => Some(CaucusCommand::ExitScroll),
        _ => None,
    }
}

/// Map a key to a close-confirm command while the confirm prompt is open.
/// `y` confirms; `n`/`Esc`/`Ctrl-C` cancels; every other key returns `None`
/// and is swallowed (so a stray keystroke cannot confirm a destructive close).
fn confirm_command(key: &KeyEvent) -> Option<CaucusCommand> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        return Some(CaucusCommand::CancelClose);
    }
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(CaucusCommand::ConfirmClose),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(CaucusCommand::CancelClose),
        _ => None,
    }
}

/// Map an arrow key code to its [`Direction`], or `None` for any other key.
/// Shared by the prefixed focus-move (bare arrow) and resize (`Ctrl`-arrow)
/// bindings so both stay in lock-step.
fn arrow_direction(code: KeyCode) -> Option<Direction> {
    match code {
        KeyCode::Up => Some(Direction::Up),
        KeyCode::Down => Some(Direction::Down),
        KeyCode::Left => Some(Direction::Left),
        KeyCode::Right => Some(Direction::Right),
        _ => None,
    }
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
    fn prefix_then_arrows_are_directional_focus() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        let cases = [
            (KeyCode::Up, Direction::Up),
            (KeyCode::Down, Direction::Down),
            (KeyCode::Left, Direction::Left),
            (KeyCode::Right, Direction::Right),
        ];
        for (code, dir) in cases {
            router.route(ctrl('a'));
            match router.route(key(code)) {
                InputAction::Caucus(CaucusCommand::FocusDir(d)) => {
                    assert_eq!(d, dir, "arrow {code:?} should map to {dir:?}")
                }
                other => panic!("expected FocusDir({dir:?}) for {code:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn prefix_then_ctrl_arrows_are_directional_resize() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        let cases = [
            (KeyCode::Up, Direction::Up),
            (KeyCode::Down, Direction::Down),
            (KeyCode::Left, Direction::Left),
            (KeyCode::Right, Direction::Right),
        ];
        for (code, dir) in cases {
            router.route(ctrl('a'));
            let arrow = KeyEvent::new(code, KeyModifiers::CONTROL);
            match router.route(arrow) {
                InputAction::Caucus(CaucusCommand::ResizeDir(d)) => {
                    assert_eq!(d, dir, "Ctrl-{code:?} should resize {dir:?}")
                }
                other => panic!("expected ResizeDir({dir:?}) for Ctrl-{code:?}, got {other:?}"),
            }
        }
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
    fn a_configured_prefix_recognises_its_own_chord_only() {
        // With the prefix set to Ctrl-B, Ctrl-B arms caucus and Ctrl-A is just
        // a normal control byte forwarded to the panel (the tmux-collision fix).
        let mut router = FocusRouter::with_prefix('b');
        router.set_focus(Some(PanelId::new()));

        // Ctrl-A is no longer the prefix: it forwards verbatim.
        match router.route(ctrl('a')) {
            InputAction::ToPanel { bytes, .. } => assert_eq!(bytes, vec![0x01]),
            other => panic!("expected Ctrl-A forwarded, got {other:?}"),
        }
        assert!(!router.prefix_armed());

        // Ctrl-B now arms the prefix and selects a caucus command.
        assert!(matches!(router.route(ctrl('b')), InputAction::Ignore));
        assert!(router.prefix_armed());
        assert!(matches!(
            router.route(key(KeyCode::Char('n'))),
            InputAction::Caucus(CaucusCommand::FocusNext)
        ));
    }

    #[test]
    fn configured_prefix_doubled_forwards_its_own_literal_byte() {
        // Ctrl-B Ctrl-B sends a literal Ctrl-B (0x02), not Ctrl-A (0x01).
        let mut router = FocusRouter::with_prefix('b');
        router.set_focus(Some(PanelId::new()));
        router.route(ctrl('b'));
        match router.route(ctrl('b')) {
            InputAction::ToPanel { bytes, .. } => assert_eq!(bytes, vec![0x02]),
            other => panic!("expected literal Ctrl-B (0x02), got {other:?}"),
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
    fn prefix_then_bracket_enters_scroll() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.route(ctrl('a'));
        assert!(matches!(
            router.route(key(KeyCode::Char('['))),
            InputAction::Caucus(CaucusCommand::EnterScroll)
        ));
    }

    #[test]
    fn prefix_then_x_is_close_focused() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.route(ctrl('a'));
        assert!(matches!(
            router.route(key(KeyCode::Char('x'))),
            InputAction::Caucus(CaucusCommand::CloseFocused)
        ));
    }

    #[test]
    fn confirm_prompt_captures_y_n_esc_and_swallows_other_keys() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.set_confirm_open(true);

        // `y`/`Y` confirm.
        assert!(matches!(
            router.route(key(KeyCode::Char('y'))),
            InputAction::Caucus(CaucusCommand::ConfirmClose)
        ));
        assert!(matches!(
            router.route(key(KeyCode::Char('Y'))),
            InputAction::Caucus(CaucusCommand::ConfirmClose)
        ));
        // `n`/`Esc`/`Ctrl-C` cancel.
        assert!(matches!(
            router.route(key(KeyCode::Char('n'))),
            InputAction::Caucus(CaucusCommand::CancelClose)
        ));
        assert!(matches!(
            router.route(key(KeyCode::Esc)),
            InputAction::Caucus(CaucusCommand::CancelClose)
        ));
        assert!(matches!(
            router.route(ctrl('c')),
            InputAction::Caucus(CaucusCommand::CancelClose)
        ));
        // Every other key is swallowed — it must not reach the panel, and a
        // stray keystroke must not confirm the destructive close.
        assert!(matches!(
            router.route(key(KeyCode::Char('a'))),
            InputAction::Ignore
        ));
        assert!(matches!(
            router.route(key(KeyCode::Enter)),
            InputAction::Ignore
        ));
    }

    #[test]
    fn confirm_prompt_takes_precedence_over_the_prefix() {
        // While the confirm is open, even `Ctrl-A` is swallowed — the prompt
        // is modal, so the prefix cannot arm underneath it.
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.set_confirm_open(true);
        assert!(matches!(router.route(ctrl('a')), InputAction::Ignore));
        assert!(!router.prefix_armed());
    }

    #[test]
    fn open_pager_captures_navigation_keys() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.set_scroll_open(true);

        let cases = [
            (KeyCode::Up, CaucusCommand::ScrollUp),
            (KeyCode::Char('k'), CaucusCommand::ScrollUp),
            (KeyCode::Down, CaucusCommand::ScrollDown),
            (KeyCode::Char('j'), CaucusCommand::ScrollDown),
            (KeyCode::PageUp, CaucusCommand::ScrollPageUp),
            (KeyCode::PageDown, CaucusCommand::ScrollPageDown),
            (KeyCode::Home, CaucusCommand::ScrollTop),
            (KeyCode::Char('g'), CaucusCommand::ScrollTop),
            (KeyCode::End, CaucusCommand::ScrollBottom),
            (KeyCode::Char('G'), CaucusCommand::ScrollBottom),
            (KeyCode::Esc, CaucusCommand::ExitScroll),
            (KeyCode::Char('q'), CaucusCommand::ExitScroll),
        ];
        for (code, want) in cases {
            match router.route(key(code)) {
                InputAction::Caucus(got) => assert_eq!(got, want, "key {code:?}"),
                other => panic!("expected Caucus({want:?}) for {code:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn open_pager_swallows_other_keys_instead_of_forwarding() {
        // Capture (not pass-through): a plain char must NOT reach the PTY while
        // the pager is open — this is what distinguishes it from the
        // read-only transcript overlay.
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.set_scroll_open(true);
        assert!(matches!(
            router.route(key(KeyCode::Char('a'))),
            InputAction::Ignore
        ));
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
