//! Input routing: which panel has focus, and how keys (`Enter`, `Ctrl-C`,
//! arbitrary keys) reach a panel's PTY. See `docs/design.md` §0 #11, §9.
//!
//! caucus panels are fully bidirectional interactive terminals: the user can
//! type into a focused panel directly (logins, OAuth device codes, ...), and
//! the main worker can drive any panel via the MCP `send_keys` tool.
//!
//! A host-side **paste** is delivered to the focused panel as one bracketed
//! paste burst ([`FocusRouter::paste_target`] /
//! [`crate::session::Multiplexer::handle_paste`]), not streamed key-by-key —
//! so a multi-line paste inserts as one block instead of submitting at every
//! embedded newline. It only inserts; the user presses `Enter` to submit.
//!
//! # Keymap
//!
//! caucus reserves a single **prefix key**, `Ctrl-A` by default, for its own
//! commands. The prefix is configurable — `--prefix` / `CAUCUS_PREFIX`, then
//! the `[settings] prefix` key, resolved by [`effective_prefix`] — so it can
//! dodge a collision with an outer multiplexer; with no configuration at all,
//! launching inside a tmux whose own prefix is `Ctrl-A` auto-dodges the
//! default to `Ctrl-B` (tmux would otherwise swallow every command). The table
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
//! | (pager open) `/`          | search the scrollback (Enter run, Esc cancel)|
//! | (pager open) `n` / `N`    | next / previous search match            |
//! | (pager open) `v`          | copy mode — select lines to yank        |
//! | (copy mode) `↑↓ k j`      | move the cursor / extend the selection  |
//! | (copy mode) `g G Home End`| extend to the oldest / newest line      |
//! | (copy mode) `y` / `Enter` | copy the selection (OSC 52) and exit    |
//! | (copy mode) `Esc`         | cancel copy mode                        |
//! | (pager open) `Esc` / `q`  | exit the scrollback pager               |
//! | scroll wheel up           | enter / page back the scrollback pager  |
//! | scroll wheel down         | page forward in the pager (off at live) |
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

/// The letter the default dodges to when an outer tmux already owns
/// `Ctrl-<PREFIX_CHAR>` as its prefix. Safe by construction: the dodge fires
/// only when tmux's prefix *is* `PREFIX_CHAR`, so the fallback can never be
/// the colliding chord itself.
const FALLBACK_PREFIX_CHAR: char = 'b';

/// Resolve the effective prefix letter for this launch: an explicit
/// `--prefix` / `CAUCUS_PREFIX` wins, then the `[settings] prefix` key, then
/// the default `Ctrl-A` — auto-dodged to `Ctrl-B` when running inside a tmux
/// whose own prefix is that same chord. Without the dodge, tmux swallows
/// every caucus command: each one needs a `C-a C-a <key>` triple chord and a
/// plain `C-a n`/`C-a p` switches tmux windows instead of caucus panels.
///
/// The dodge applies only to the *default*: a prefix the user chose anywhere
/// explicitly is honoured even when it collides. The dodge is logged (the
/// status bar always shows the live prefix either way).
pub fn effective_prefix(explicit: Option<char>, configured: Option<char>) -> char {
    let tmux = tmux_prefix_letter();
    let resolved = resolve_prefix(explicit, configured, tmux);
    if resolved != PREFIX_CHAR && explicit.or(configured).is_none() {
        tracing::warn!(
            "the outer tmux uses Ctrl-{} as its own prefix; caucus commands moved to Ctrl-{} \
             (pin one with --prefix / CAUCUS_PREFIX / `[settings] prefix`)",
            PREFIX_CHAR.to_ascii_uppercase(),
            resolved.to_ascii_uppercase(),
        );
    }
    resolved
}

/// Pure resolution core of [`effective_prefix`]: explicit > configured >
/// default, with the default dodging a colliding outer-tmux prefix.
fn resolve_prefix(
    explicit: Option<char>,
    configured: Option<char>,
    tmux_prefix: Option<char>,
) -> char {
    if let Some(chosen) = explicit.or(configured) {
        return chosen;
    }
    if tmux_prefix == Some(PREFIX_CHAR) {
        FALLBACK_PREFIX_CHAR
    } else {
        PREFIX_CHAR
    }
}

/// The outer tmux's prefix letter (`C-a` → `'a'`), or `None` when not inside
/// tmux ($TMUX unset), the probe fails, or the tmux prefix is not a
/// `Ctrl-<letter>` chord — only those can collide with a caucus prefix.
fn tmux_prefix_letter() -> Option<char> {
    std::env::var_os("TMUX")?;
    let out = std::process::Command::new("tmux")
        .args(["show-options", "-gqv", "prefix"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_tmux_prefix(std::str::from_utf8(&out.stdout).ok()?)
}

/// Parse tmux's `show-options -gqv prefix` output (`"C-a"`, `"C-b"`, ...) to
/// the prefix letter. Non-`C-<letter>` prefixes (`` ` ``, `C-Space`, an empty
/// probe) yield `None` — they cannot collide with a `Ctrl-<letter>` chord.
fn parse_tmux_prefix(raw: &str) -> Option<char> {
    let rest = raw.trim().strip_prefix("C-")?;
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => Some(c.to_ascii_lowercase()),
        _ => None,
    }
}

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

/// A cursor motion inside the pager's copy mode (`docs/design.md` §1). Carried
/// by [`CaucusCommand::CopyMove`] so a single command covers every
/// selection-extend key instead of one variant each (like [`Direction`] on
/// [`CaucusCommand::FocusDir`]).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CopyMotion {
    /// Move the cursor up one line.
    Up,
    /// Move the cursor down one line.
    Down,
    /// Move the cursor up one page.
    PageUp,
    /// Move the cursor down one page.
    PageDown,
    /// Move the cursor to the oldest line.
    Top,
    /// Move the cursor to the newest line.
    Bottom,
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
    /// Open the pager's `/` search-input line.
    SearchStart,
    /// Append a character to the open `/` search line.
    SearchInput(char),
    /// Delete the last character of the `/` search line.
    SearchBackspace,
    /// Commit the `/` search line: run the query and jump to the first match.
    SearchCommit,
    /// Cancel the `/` search line, keeping any prior committed search.
    SearchCancel,
    /// Step to the next search match (`n`).
    SearchNext,
    /// Step to the previous search match (`N`).
    SearchPrev,
    /// Enter the pager's copy mode (`v`): start a line selection at the top
    /// visible line. Subsequent [`CaucusCommand::CopyMove`] keys extend it.
    CopyStart,
    /// Move the copy-mode cursor, extending the line selection.
    CopyMove(CopyMotion),
    /// Copy the selected lines to the host clipboard (OSC 52) and leave copy
    /// mode (`y`/`Enter`).
    CopyYank,
    /// Leave copy mode without copying (`Esc`).
    CopyCancel,
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
    /// `true` while the pager's `/` search-input line is open. A sub-mode of
    /// `scroll_open`: keystrokes edit the query instead of driving navigation.
    scroll_searching: bool,
    /// `true` while the pager's copy mode is active (`v`). A sub-mode of
    /// `scroll_open`: navigation keys move the selection cursor, `y`/`Enter`
    /// copy, `Esc` cancels — instead of driving the window scroll.
    scroll_copying: bool,
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
            scroll_searching: false,
            scroll_copying: false,
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

    /// The panel a host-side paste should be delivered to, if any. A paste is
    /// inserted into the focused panel exactly like typed input — but only when
    /// no modal capture owns input: while the scrollback pager or the
    /// close-panel confirm prompt is open, a paste is swallowed (returns `None`)
    /// rather than injected behind the modal's back, matching how those modals
    /// capture every key in [`FocusRouter::route`]. The read-only transcript
    /// overlay passes input through, so it does not block a paste.
    pub fn paste_target(&self) -> Option<PanelId> {
        if self.scroll_open || self.confirm_open {
            return None;
        }
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

    /// Tell the router whether the pager's `/` search-input line is open — when
    /// open, [`FocusRouter::route`] sends keystrokes to the query line (printable
    /// chars, `Backspace`, `Enter` to commit, `Esc` to cancel) instead of the
    /// pager's navigation bindings.
    pub fn set_scroll_searching(&mut self, searching: bool) {
        self.scroll_searching = searching;
    }

    /// Tell the router whether the pager's copy mode is active — when active,
    /// [`FocusRouter::route`] sends keystrokes to the selection (navigation
    /// moves the cursor, `y`/`Enter` copies, `Esc` cancels) instead of the
    /// pager's window-scroll bindings.
    pub fn set_scroll_copying(&mut self, copying: bool) {
        self.scroll_copying = copying;
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
        // the read-only transcript overlay, which passes input through). Its `/`
        // search sub-mode reroutes keystrokes to the query input line.
        if self.scroll_open {
            let cmd = if self.scroll_searching {
                search_input_command(&key)
            } else if self.scroll_copying {
                copy_input_command(&key)
            } else {
                scroll_command(&key)
            };
            return cmd.map(InputAction::Caucus).unwrap_or(InputAction::Ignore);
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
        // `/` opens the search line; `n`/`N` step the committed matches (`less`).
        KeyCode::Char('/') => Some(CaucusCommand::SearchStart),
        KeyCode::Char('n') => Some(CaucusCommand::SearchNext),
        KeyCode::Char('N') => Some(CaucusCommand::SearchPrev),
        // `v` enters copy mode (vim visual-line) to select lines for the
        // clipboard — the in-app answer to the native selection that mouse
        // capture suppresses (`[settings] mouse`).
        KeyCode::Char('v') => Some(CaucusCommand::CopyStart),
        KeyCode::Esc | KeyCode::Char('q') => Some(CaucusCommand::ExitScroll),
        _ => None,
    }
}

/// Map a key to a copy-mode command while a line selection is active. The
/// navigation keys mirror [`scroll_command`] but move the *selection cursor*
/// (extending the selection) rather than the window; `y`/`Enter` copy and
/// `Esc` cancels. Every other key is swallowed so it cannot leak past the
/// selection (matching the pager's modal capture).
fn copy_input_command(key: &KeyEvent) -> Option<CaucusCommand> {
    use CopyMotion::*;
    let motion = match key.code {
        KeyCode::Up | KeyCode::Char('k') => Some(Up),
        KeyCode::Down | KeyCode::Char('j') => Some(Down),
        KeyCode::PageUp | KeyCode::Char('b') => Some(PageUp),
        KeyCode::PageDown | KeyCode::Char(' ') => Some(PageDown),
        KeyCode::Home | KeyCode::Char('g') => Some(Top),
        KeyCode::End | KeyCode::Char('G') => Some(Bottom),
        _ => None,
    };
    if let Some(motion) = motion {
        return Some(CaucusCommand::CopyMove(motion));
    }
    match key.code {
        KeyCode::Enter | KeyCode::Char('y') => Some(CaucusCommand::CopyYank),
        KeyCode::Esc => Some(CaucusCommand::CopyCancel),
        _ => None,
    }
}

/// Map a key to a search-input command while the pager's `/` line is open.
/// Printable chars extend the query, `Backspace` trims it, `Enter` commits,
/// `Esc` cancels; every other key (including the pager's navigation bindings)
/// is swallowed so it cannot leak past the input line.
fn search_input_command(key: &KeyEvent) -> Option<CaucusCommand> {
    match key.code {
        KeyCode::Enter => Some(CaucusCommand::SearchCommit),
        KeyCode::Esc => Some(CaucusCommand::SearchCancel),
        KeyCode::Backspace => Some(CaucusCommand::SearchBackspace),
        // A bare printable char (no Ctrl/Alt) extends the query.
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(CaucusCommand::SearchInput(c))
        }
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

/// Parse a human-readable key name (as the MCP `send_key` tool receives it)
/// into a [`KeyEvent`] — the inverse direction of [`encode_key`], whose output
/// is fed straight back through `encode_key` to produce the PTY bytes.
///
/// Grammar: zero or more case-insensitive modifier prefixes — `ctrl`
/// (`control`), `alt` (`meta` / `option`), `shift` — joined to the base by `-`
/// or `+`, then one base key:
/// * a named key: `esc`/`escape`, `enter`/`return`, `tab`, `backtab`,
///   `backspace`/`bs`, `space`, `up`, `down`, `left`, `right`, `home`, `end`,
///   `pageup`/`pgup`, `pagedown`/`pgdn`, `insert`/`ins`, `delete`/`del`;
/// * a function key `f1`..`f12`;
/// * or a single character (`a`, `/`, `?`, and the separators `-`/`+`
///   themselves when given alone), taken with its literal case.
///
/// Examples: `esc`, `up`, `ctrl-c`, `alt-enter`, `ctrl-shift-left`, `f5`.
///
/// Returns a human-readable error for an empty name, an unknown modifier, or
/// an unrecognised base key. A parsed key that has no terminal encoding (e.g.
/// `ctrl-5`) parses here but [`encode_key`] returns `None` for it.
pub fn parse_key_name(name: &str) -> Result<KeyEvent, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("empty key name".to_string());
    }

    // A lone character is taken literally — this is also how the separator
    // characters `-` / `+` reach a panel as bare keys, since the split below
    // would otherwise consume them.
    let mut solo = trimmed.chars();
    if let (Some(c), None) = (solo.next(), solo.next()) {
        return Ok(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }

    // Otherwise: separator-joined tokens — every token but the last is a
    // modifier, the last is the base key. Empty tokens (a doubled or trailing
    // separator) are dropped.
    let parts: Vec<&str> = trimmed
        .split(['-', '+'])
        .filter(|s| !s.is_empty())
        .collect();
    let (base, mod_toks) = parts
        .split_last()
        .ok_or_else(|| format!("unrecognised key `{trimmed}`"))?;

    let mut modifiers = KeyModifiers::NONE;
    for m in mod_toks {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
            "alt" | "meta" | "option" => modifiers |= KeyModifiers::ALT,
            "shift" => modifiers |= KeyModifiers::SHIFT,
            other => return Err(format!("unknown key modifier `{other}`")),
        }
    }

    let code = match base.to_ascii_lowercase().as_str() {
        "esc" | "escape" => KeyCode::Esc,
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" | "bs" => KeyCode::Backspace,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "insert" | "ins" => KeyCode::Insert,
        "delete" | "del" => KeyCode::Delete,
        lower => {
            // `fN` function key, else a single literal character (original
            // case, e.g. `ctrl-A` vs `ctrl-a`).
            if let Some(n) = lower
                .strip_prefix('f')
                .and_then(|d| d.parse::<u8>().ok())
                .filter(|n| (1..=12).contains(n))
            {
                KeyCode::F(n)
            } else {
                let mut bc = base.chars();
                match (bc.next(), bc.next()) {
                    (Some(c), None) => KeyCode::Char(c),
                    _ => return Err(format!("unrecognised key `{base}`")),
                }
            }
        }
    };
    Ok(KeyEvent::new(code, modifiers))
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

    // Prefix resolution boundaries: one case per rung of the chain (explicit >
    // configured > default) plus the two sides of the tmux-collision dodge.
    #[test]
    fn resolve_prefix_explicit_wins_even_when_it_collides() {
        assert_eq!(resolve_prefix(Some('c'), Some('d'), Some('c')), 'c');
        assert_eq!(resolve_prefix(Some('a'), None, Some('a')), 'a');
    }

    #[test]
    fn resolve_prefix_configured_wins_over_the_dodge() {
        // A settings-chosen prefix is the user's choice — honoured verbatim,
        // even when it is exactly the colliding tmux chord.
        assert_eq!(resolve_prefix(None, Some('a'), Some('a')), 'a');
        assert_eq!(resolve_prefix(None, Some('g'), Some('a')), 'g');
    }

    #[test]
    fn resolve_prefix_default_dodges_a_colliding_tmux_prefix_only() {
        // Colliding tmux (C-a) → the default moves to the fallback.
        assert_eq!(resolve_prefix(None, None, Some('a')), FALLBACK_PREFIX_CHAR);
        // Non-colliding tmux (C-b) or no tmux at all → the plain default.
        assert_eq!(resolve_prefix(None, None, Some('b')), PREFIX_CHAR);
        assert_eq!(resolve_prefix(None, None, None), PREFIX_CHAR);
    }

    #[test]
    fn parse_tmux_prefix_accepts_only_ctrl_letter_chords() {
        assert_eq!(parse_tmux_prefix("C-a\n"), Some('a'));
        assert_eq!(parse_tmux_prefix("C-B"), Some('b'));
        // Chords that cannot collide with a Ctrl-<letter> prefix parse to None.
        assert_eq!(parse_tmux_prefix("C-Space"), None);
        assert_eq!(parse_tmux_prefix("`"), None);
        assert_eq!(parse_tmux_prefix(""), None);
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
    fn paste_target_is_focused_panel_unless_a_modal_captures() {
        let mut router = FocusRouter::new();
        // No focus → nowhere to paste.
        assert_eq!(router.paste_target(), None);

        let panel = PanelId::new();
        router.set_focus(Some(panel));
        assert_eq!(router.paste_target(), Some(panel));

        // The scrollback pager and the close-confirm prompt are modal: each
        // swallows a paste rather than injecting it behind the modal's back.
        router.set_scroll_open(true);
        assert_eq!(router.paste_target(), None, "scroll pager captures paste");
        router.set_scroll_open(false);
        assert_eq!(router.paste_target(), Some(panel));

        router.set_confirm_open(true);
        assert_eq!(router.paste_target(), None, "confirm prompt captures paste");
        router.set_confirm_open(false);
        assert_eq!(router.paste_target(), Some(panel));
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
    fn pager_search_keys_route_to_search_commands() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.set_scroll_open(true);

        // In the pager (not yet searching) `/` opens the search line and `n`/`N`
        // step committed matches.
        assert!(matches!(
            router.route(key(KeyCode::Char('/'))),
            InputAction::Caucus(CaucusCommand::SearchStart)
        ));
        assert!(matches!(
            router.route(key(KeyCode::Char('n'))),
            InputAction::Caucus(CaucusCommand::SearchNext)
        ));
        assert!(matches!(
            router.route(key(KeyCode::Char('N'))),
            InputAction::Caucus(CaucusCommand::SearchPrev)
        ));

        // While the `/` line is open, keystrokes edit the query — even `j`,
        // which would otherwise scroll — and Backspace/Enter/Esc edit it.
        router.set_scroll_searching(true);
        assert!(matches!(
            router.route(key(KeyCode::Char('j'))),
            InputAction::Caucus(CaucusCommand::SearchInput('j'))
        ));
        assert!(matches!(
            router.route(key(KeyCode::Backspace)),
            InputAction::Caucus(CaucusCommand::SearchBackspace)
        ));
        assert!(matches!(
            router.route(key(KeyCode::Enter)),
            InputAction::Caucus(CaucusCommand::SearchCommit)
        ));
        assert!(matches!(
            router.route(key(KeyCode::Esc)),
            InputAction::Caucus(CaucusCommand::SearchCancel)
        ));
        // A Ctrl-modified char is not query text — it is swallowed, not typed.
        assert!(matches!(router.route(ctrl('c')), InputAction::Ignore));
    }

    #[test]
    fn pager_copy_keys_route_to_copy_commands() {
        let mut router = FocusRouter::new();
        router.set_focus(Some(PanelId::new()));
        router.set_scroll_open(true);

        // In the pager (not yet copying) `v` enters copy mode.
        assert!(matches!(
            router.route(key(KeyCode::Char('v'))),
            InputAction::Caucus(CaucusCommand::CopyStart)
        ));

        // While copy mode is active the navigation keys move the selection
        // cursor (extending the selection) instead of scrolling the window.
        router.set_scroll_copying(true);
        let moves = [
            (KeyCode::Char('j'), CopyMotion::Down),
            (KeyCode::Down, CopyMotion::Down),
            (KeyCode::Char('k'), CopyMotion::Up),
            (KeyCode::Up, CopyMotion::Up),
            (KeyCode::PageUp, CopyMotion::PageUp),
            (KeyCode::PageDown, CopyMotion::PageDown),
            (KeyCode::Char('g'), CopyMotion::Top),
            (KeyCode::Char('G'), CopyMotion::Bottom),
        ];
        for (code, want) in moves {
            match router.route(key(code)) {
                InputAction::Caucus(CaucusCommand::CopyMove(got)) => {
                    assert_eq!(got, want, "key {code:?}")
                }
                other => panic!("expected CopyMove({want:?}) for {code:?}, got {other:?}"),
            }
        }
        // `y`/`Enter` copy; `Esc` cancels.
        assert!(matches!(
            router.route(key(KeyCode::Char('y'))),
            InputAction::Caucus(CaucusCommand::CopyYank)
        ));
        assert!(matches!(
            router.route(key(KeyCode::Enter)),
            InputAction::Caucus(CaucusCommand::CopyYank)
        ));
        assert!(matches!(
            router.route(key(KeyCode::Esc)),
            InputAction::Caucus(CaucusCommand::CopyCancel)
        ));
        // An unmapped key is swallowed, not forwarded to the panel.
        assert!(matches!(
            router.route(key(KeyCode::Char('z'))),
            InputAction::Ignore
        ));
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

    #[test]
    fn parse_key_name_named_keys() {
        assert_eq!(parse_key_name("esc").unwrap(), key(KeyCode::Esc));
        assert_eq!(parse_key_name("Escape").unwrap(), key(KeyCode::Esc));
        assert_eq!(parse_key_name("enter").unwrap(), key(KeyCode::Enter));
        assert_eq!(parse_key_name("up").unwrap(), key(KeyCode::Up));
        assert_eq!(parse_key_name("pgdn").unwrap(), key(KeyCode::PageDown));
        assert_eq!(parse_key_name("del").unwrap(), key(KeyCode::Delete));
        assert_eq!(parse_key_name("space").unwrap(), key(KeyCode::Char(' ')));
        assert_eq!(parse_key_name(" tab ").unwrap(), key(KeyCode::Tab));
    }

    #[test]
    fn parse_key_name_function_keys() {
        assert_eq!(parse_key_name("f1").unwrap(), key(KeyCode::F(1)));
        assert_eq!(parse_key_name("F12").unwrap(), key(KeyCode::F(12)));
        // f0 / f13 are out of range — fall through to a literal char base,
        // which is multi-char ("f0") and therefore unrecognised.
        assert!(parse_key_name("f13").is_err());
    }

    #[test]
    fn parse_key_name_modifiers() {
        assert_eq!(parse_key_name("ctrl-c").unwrap(), ctrl('c'));
        assert_eq!(
            parse_key_name("alt-enter").unwrap(),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
        );
        assert_eq!(
            parse_key_name("ctrl-shift-left").unwrap(),
            KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        );
        // `+` is an accepted separator and `meta`/`option` alias `alt`.
        assert_eq!(
            parse_key_name("meta+up").unwrap(),
            KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)
        );
    }

    /// A lone character — including the separator characters themselves — is a
    /// literal key with its original case, no modifiers.
    #[test]
    fn parse_key_name_single_chars() {
        assert_eq!(parse_key_name("a").unwrap(), key(KeyCode::Char('a')));
        assert_eq!(parse_key_name("A").unwrap(), key(KeyCode::Char('A')));
        assert_eq!(parse_key_name("/").unwrap(), key(KeyCode::Char('/')));
        assert_eq!(parse_key_name("-").unwrap(), key(KeyCode::Char('-')));
        assert_eq!(parse_key_name("+").unwrap(), key(KeyCode::Char('+')));
    }

    /// The parse → encode round-trip: a name the tool receives produces the
    /// same bytes a real keypress would.
    #[test]
    fn parse_key_name_round_trips_through_encode() {
        let bytes = |n: &str| encode_key(&parse_key_name(n).unwrap()).unwrap();
        assert_eq!(bytes("ctrl-c"), vec![0x03]);
        assert_eq!(bytes("esc"), vec![0x1b]);
        assert_eq!(bytes("up"), b"\x1b[A");
        assert_eq!(bytes("alt-b"), vec![0x1b, b'b']);
    }

    #[test]
    fn parse_key_name_rejects_bad_input() {
        assert!(parse_key_name("").is_err());
        assert!(parse_key_name("   ").is_err());
        assert!(parse_key_name("hyper-x").is_err());
        assert!(parse_key_name("ctrl-notakey").is_err());
    }
}
