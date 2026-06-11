use super::*;
use crate::agent::manifest;
use crate::input::CaucusCommand;
use crate::panel::lifecycle::{self, PanelState};
use crate::signal::TurnSignal;
use std::time::{Duration, Instant};
use tracing::warn;

/// How often [`Multiplexer::pump_all`] probes child-process liveness. PTYs are
/// drained every tick, but `try_wait` (a `waitpid` syscall) per panel on the
/// ~250 Hz idle loop is pure overhead; a child exit surfaces within one
/// interval, imperceptible for a UI reap.
const LIVENESS_PROBE_INTERVAL: Duration = Duration::from_millis(250);

impl Multiplexer {
    /// Apply a key event via the focus router. Returns `true` while caucus
    /// should keep running, `false` once quit was requested.
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        use crate::input::InputAction;
        // Any keystroke may change the view: it arms/disarms the prefix hint,
        // toggles layout/zoom/scroll/transcript, or is forwarded to a panel
        // (whose echo the grid-generation signature will catch a tick later,
        // but the local cursor feedback should not wait). Bump the view epoch
        // so the dirty-gated draw renders exactly one frame for this key.
        self.view_epoch = self.view_epoch.wrapping_add(1);
        match self.focus.route(key) {
            InputAction::ToPanel { panel, bytes } => {
                let submit = bytes.contains(&b'\r') || bytes.contains(&b'\n');
                let is_main = Some(panel) == self.main_panel_id;
                // A non-submit keystroke to the main panel means the user may
                // be composing an un-submitted line: open the compose hold so a
                // round delivery never lands mid-line (see `poll_pending_rounds`
                // / `COMPOSE_GRACE`). A submit is not a compose — it is handled
                // by `note_prompt_delivered` below, which clears the hold.
                if is_main && !submit {
                    self.main_compose_since = Some(Instant::now());
                }
                // A submit on the main panel delivers a prompt only if the user
                // actually composed a line. A bare Enter on an empty main input
                // (no composition since the last submit) sends the agent nothing
                // to act on, so it never opens a turn that could end — flipping
                // it to `Working` would wedge it there forever. `main_compose_since`
                // is caucus's model of "the main line holds un-submitted text".
                // Sub-panels carry no compose model, so a submit there always
                // delivers (unchanged behaviour).
                let delivers_prompt = submit && (!is_main || self.main_compose_since.is_some());
                if let Some(p) = self.panels.iter_mut().find(|p| p.id == panel)
                    && let Err(err) = p.write_input(&bytes)
                {
                    warn!(panel = %panel, error = %err, "panel write failed");
                }
                // A submitted line (Enter) typed directly into a panel is a
                // prompt delivered by the user — flip it to `Working`, the
                // same as the MCP `send_keys` path. An empty main submit is not
                // a prompt: the keystroke still reaches the agent above, but no
                // turn is opened and the panel stays put.
                if delivers_prompt {
                    self.note_prompt_delivered(panel);
                }
            }
            InputAction::Caucus(cmd) => self.apply_command(cmd),
            InputAction::Ignore => {}
        }
    }

    /// Deliver a host-side bracketed paste to the focused panel's PTY as one
    /// paste burst. Without this the host terminal streams a paste key-by-key
    /// and every embedded newline (`\r`) is taken as a submit, so a multi-line
    /// paste fires the panel's prompt at its first line. Routed through the
    /// same `Multiplexer::deliver_text` / `plan_delivery` framing the MCP
    /// `send_keys` tool uses, with `enter = false`: a paste only *inserts* the
    /// text — the user presses Enter themselves when ready — so no submitting
    /// `\r` is appended or deferred and no turn is opened.
    ///
    /// Ignored when a modal (scrollback pager / close-confirm) owns input or no
    /// panel is focused ([`crate::input::FocusRouter::paste_target`]).
    pub fn handle_paste(&mut self, text: &str) {
        let Some(panel) = self.focus.paste_target() else {
            return;
        };
        // A paste changes the view and, on the main panel, is un-submitted
        // composition — bump the view epoch for one frame of cursor feedback
        // and arm the compose hold so a round delivery never lands mid-paste
        // (mirrors the non-submit keystroke path in `handle_key`).
        self.view_epoch = self.view_epoch.wrapping_add(1);
        if Some(panel) == self.main_panel_id {
            self.main_compose_since = Some(Instant::now());
        }
        if let Err(err) = self.deliver_text(panel, text, false) {
            warn!(panel = %panel, error = %err, "panel paste write failed");
        }
    }

    /// Apply a mouse event (`docs/design.md` §1). Only the scroll wheel is
    /// acted on; it drives the scrollback pager. Delivered to the event loop
    /// only when mouse capture is on (`[settings] mouse`).
    ///
    /// Scrolling up from the live view opens the pager on the focused panel —
    /// tmux copy-mode-on-scroll entry — and once it is open the wheel pages the
    /// frozen snapshot. Scrolling down at the live bottom is a no-op: nothing is
    /// newer than the live view. Clicks, drags, and moves are ignored. While the
    /// close-confirm prompt owns the screen the wheel is swallowed, matching the
    /// keyboard router's modal capture.
    pub fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;
        /// Rows the wheel moves per notch — a few lines, like a terminal.
        const WHEEL_STEP: isize = 3;

        // The close-confirm prompt is modal: do not scroll a pager underneath it.
        if self.pending_close().is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                // Entering scrollback from the live view; a no-op when no panel
                // is focused (the pager stays closed and the next branch skips).
                if self.scroll_state().is_none() {
                    self.enter_scroll();
                }
                if self.scroll_state().is_some() {
                    self.view_epoch = self.view_epoch.wrapping_add(1);
                    self.scroll_by(-WHEEL_STEP);
                }
            }
            // Only meaningful inside the pager — at the live bottom there is
            // nothing newer to reveal, so an unguarded ScrollDown falls through.
            MouseEventKind::ScrollDown if self.scroll_state().is_some() => {
                self.view_epoch = self.view_epoch.wrapping_add(1);
                self.scroll_by(WHEEL_STEP);
            }
            _ => {}
        }
    }

    /// Whether the reserved prefix key is armed (for a status hint).
    pub fn prefix_armed(&self) -> bool {
        self.focus.prefix_armed()
    }

    /// Apply a caucus-level command (focus switch / quit / layout control).
    pub(crate) fn apply_command(&mut self, cmd: CaucusCommand) {
        match cmd {
            CaucusCommand::Quit => self.quit = true,
            CaucusCommand::FocusNext => self.cycle_focus(1),
            CaucusCommand::FocusPrev => self.cycle_focus(-1),
            CaucusCommand::FocusDir(dir) => self.focus_dir(dir),
            CaucusCommand::ResizeDir(dir) => self.resize_focused(dir),
            CaucusCommand::ToggleZoom => self.toggle_zoom(),
            CaucusCommand::MovePanelEarlier => self.move_panel(-1),
            CaucusCommand::MovePanelLater => self.move_panel(1),
            CaucusCommand::CloseFocused => self.arm_close_confirm(),
            CaucusCommand::ConfirmClose => self.confirm_close(),
            CaucusCommand::CancelClose => self.cancel_close(),
            CaucusCommand::CycleLayout => {
                self.layout_mode = self.layout_mode.next();
                self.rebuild_layout_tree();
                // The record carries `layout_mode` and the panel order.
                self.persist_record();
            }
            CaucusCommand::ToggleTranscript => {
                self.show_transcript = !self.show_transcript;
                self.focus.set_transcript_open(self.show_transcript);
            }
            CaucusCommand::HideTranscript => {
                self.show_transcript = false;
                self.focus.set_transcript_open(false);
            }
            CaucusCommand::EnterScroll => self.enter_scroll(),
            CaucusCommand::ExitScroll => self.exit_scroll(),
            CaucusCommand::ScrollUp => self.scroll_by(-1),
            CaucusCommand::ScrollDown => self.scroll_by(1),
            CaucusCommand::ScrollPageUp => self.scroll_page(-1),
            CaucusCommand::ScrollPageDown => self.scroll_page(1),
            CaucusCommand::ScrollTop => self.scroll_to_edge(true),
            CaucusCommand::ScrollBottom => self.scroll_to_edge(false),
            CaucusCommand::SearchStart => self.search_start(),
            CaucusCommand::SearchInput(c) => self.search_input(c),
            CaucusCommand::SearchBackspace => self.search_backspace(),
            CaucusCommand::SearchCommit => self.search_commit(),
            CaucusCommand::SearchCancel => self.search_cancel(),
            CaucusCommand::SearchNext => self.search_next(),
            CaucusCommand::SearchPrev => self.search_prev(),
            CaucusCommand::CopyStart => self.copy_start(),
            CaucusCommand::CopyMove(motion) => self.copy_move(motion),
            CaucusCommand::CopyYank => self.copy_yank(),
            CaucusCommand::CopyCancel => self.copy_cancel(),
        }
    }

    /// Drain every panel's PTY into its grid + capture, and reap panels whose
    /// agent process has exited. Called once per event-loop tick.
    pub fn pump_all(&mut self) {
        // Drain every PTY into its grid + capture on every tick — this is the
        // responsiveness path and must not be throttled.
        for panel in &mut self.panels {
            match panel.pump() {
                Ok(n) => {
                    // First output from a freshly-spawned agent: its CLI
                    // process is alive and drawing its UI — it has left
                    // `Spawning` and is now an idle agent awaiting its first
                    // instruction.
                    if n > 0 && panel.state() == PanelState::Spawning {
                        let _ = lifecycle::transition(panel, PanelState::Idle);
                    }
                }
                Err(err) => {
                    warn!(panel = %panel.id, error = %err, "panel pump failed");
                }
            }
        }
        // Liveness probing is throttled (`LIVENESS_PROBE_INTERVAL`): `try_wait`
        // is a `waitpid` syscall per panel, and on the idle loop that runs
        // ~250×/s for no benefit. A child exit surfaces within one interval.
        let now = Instant::now();
        let due = self
            .last_liveness_probe
            .is_none_or(|t| now.duration_since(t) >= LIVENESS_PROBE_INTERVAL);
        if !due {
            return;
        }
        self.last_liveness_probe = Some(now);
        let mut exited = Vec::new();
        for panel in &mut self.panels {
            if panel.state() != PanelState::Exited && !panel.is_child_alive() {
                exited.push(panel.id);
            }
        }
        for id in exited {
            if let Some(panel) = self.panels.iter_mut().find(|p| p.id == id) {
                // Drain any final output, then mark exited.
                let _ = panel.pump();
                let _ = lifecycle::transition(panel, PanelState::Exited);
            }
            // Reflect the exit on the manifest so `list_panels` shows `exited`.
            if let Some(manifest) = self.manifests.get_mut(&id)
                && manifest.status() != crate::agent::AgentStatus::Exited
                && let Err(err) = manifest::record_exited(manifest, &self.session.root_dir)
            {
                warn!(panel = %id, error = %err, "manifest exit write failed");
            }
        }
    }

    /// Ingest a turn-completion signal: close the panel's capture turn, flip
    /// it to `Idle`, and record a `TurnCompleted` lane event on the panel's
    /// manifest so `list_panels` shows `idle` (`docs/design.md` §4, §8.3).
    ///
    /// The manifest mutation routes through `agent::manifest::record_turn_completed`
    /// — the single owner of that transition (Invariant I-2) — which also
    /// recomputes `derived_state` and stores the signal's `last_message`.
    pub fn handle_signal(&mut self, signal: TurnSignal) {
        let Some(panel) = self.panels.iter_mut().find(|p| p.id == signal.panel_id) else {
            return;
        };
        panel.end_turn();
        // A turn signal means the agent is idle, waiting for the next prompt.
        if panel.state() == PanelState::Working {
            let _ = lifecycle::transition(panel, PanelState::Idle);
        }

        // Append the TurnCompleted lane event + recompute derived_state.
        // A turn signal can carry Claude's conversation id for the first time;
        // re-persist the session record so a relaunch can `--resume` it.
        let mut session_id_changed = false;
        if let Some(manifest) = self.manifests.get_mut(&signal.panel_id) {
            let before = manifest.claude_session_id().map(str::to_string);
            if let Err(err) =
                manifest::record_turn_completed(manifest, &self.session.root_dir, &signal)
            {
                warn!(panel = %signal.panel_id, error = %err, "manifest turn-signal write failed");
            }
            session_id_changed = manifest.claude_session_id().map(str::to_string) != before;
        }
        if session_id_changed {
            self.persist_record();
        }
    }

    /// Mark a panel as having received a prompt: open a capture turn and flip
    /// it to `Working` (`docs/design.md` §4). Used by the MCP `send_keys` path.
    ///
    /// A submitted prompt to the *main* panel consumes the input line, so any
    /// compose hold is over — clear `main_compose_since` so the next round is
    /// not held by a stale timestamp left from the line that was just sent.
    pub fn note_prompt_delivered(&mut self, panel_id: PanelId) {
        if Some(panel_id) == self.main_panel_id {
            self.main_compose_since = None;
        }
        if let Some(panel) = self.panels.iter_mut().find(|p| p.id == panel_id) {
            panel.begin_turn();
            match panel.state() {
                PanelState::Spawning | PanelState::Idle => {
                    let _ = lifecycle::transition(panel, PanelState::Working);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::id::PanelId;
    use crate::session::runtime::test_support::*;
    use tempfile::TempDir;

    /// A submitted prompt to the main panel clears the compose hold so a stale
    /// timestamp from the just-sent line cannot hold the next round — even
    /// before the panel itself flips state (the clear precedes the lookup).
    #[tokio::test]
    async fn note_prompt_delivered_clears_main_compose_hold() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = PanelId::new();
        mux.main_panel_id = Some(main);
        mux.main_compose_since = Some(Instant::now());

        mux.note_prompt_delivered(main);
        assert!(
            mux.main_compose_since.is_none(),
            "submitting the main line must clear the compose hold"
        );
    }

    /// Insert a hermetic `/bin/cat` panel so paste tests do not depend on a
    /// real agent CLI being installed.
    fn push_cat_panel(mux: &mut Multiplexer, state: PanelState) -> PanelId {
        use crate::pty::{Pty, PtyCommand};
        use crate::session::id::AgentId;
        use crate::term::{Grid, OutputCapture};
        let id = PanelId::new();
        let inner = area().inner();
        let pty = Pty::spawn(&PtyCommand::new("/bin/cat"), inner.width, inner.height).unwrap();
        mux.panels.push(lifecycle::Panel {
            id,
            role: "reviewer".to_string(),
            agent_id: AgentId::new(),
            state,
            worktree_path: None,
            pty,
            grid: Grid::new(inner.width as usize, inner.height as usize),
            capture: OutputCapture::new(),
        });
        mux.rebuild_layout_tree();
        id
    }

    /// A host-side paste of a multi-line block is delivered to the focused
    /// panel as composition — it arms the main compose hold but must NOT submit
    /// (no `note_prompt_delivered`, so the panel stays `Idle`, not `Working`).
    /// Before this fix the paste streamed key-by-key and each `\n` flipped the
    /// panel to `Working`, firing the prompt at the first line.
    #[tokio::test]
    async fn handle_paste_delivers_to_focused_panel_without_submitting() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let panel = push_cat_panel(&mut mux, PanelState::Idle);
        mux.main_panel_id = Some(panel);
        mux.focus.set_focus(Some(panel));
        assert!(mux.main_compose_since.is_none());

        mux.handle_paste("line one\nline two\nline three");

        assert_eq!(
            mux.panels.iter().find(|p| p.id == panel).unwrap().state(),
            PanelState::Idle,
            "a paste must not submit — the panel stays Idle, never flipped to Working",
        );
        assert!(
            mux.main_compose_since.is_some(),
            "a paste into the main panel arms the compose hold (un-submitted composition)",
        );

        mux.shutdown();
    }

    /// A bare Enter on an empty main input must not flip main to `Working`.
    /// Nothing was composed, so the agent receives no prompt it can finish —
    /// flipping it would wedge it in `Working` with no turn ever to end. A
    /// submit *after* real composition is a genuine prompt and does flip it.
    #[tokio::test]
    async fn bare_enter_on_empty_main_does_not_flip_to_working() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_cat_panel(&mut mux, PanelState::Idle);
        mux.main_panel_id = Some(main);
        mux.focus.set_focus(Some(main));
        assert!(mux.main_compose_since.is_none());

        // Bare Enter, no prior composition: the keystroke reaches the agent but
        // opens no turn — the panel stays Idle.
        mux.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            mux.panels.iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Idle,
            "a bare Enter on an empty main input must not open a turn",
        );

        // Compose a character, then submit: a genuine prompt flips to Working
        // and clears the compose hold.
        mux.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert!(
            mux.main_compose_since.is_some(),
            "composing a character arms the compose hold",
        );
        mux.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            mux.panels.iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "submitting a composed line delivers a prompt and flips to Working",
        );
        assert!(
            mux.main_compose_since.is_none(),
            "delivering the prompt clears the compose hold",
        );

        mux.shutdown();
    }

    /// A paste while a modal owns input (scrollback pager) is swallowed — it
    /// must not reach the focused panel or arm the compose hold.
    #[tokio::test]
    async fn handle_paste_is_ignored_while_a_modal_captures_input() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let panel = push_cat_panel(&mut mux, PanelState::Idle);
        mux.main_panel_id = Some(panel);
        mux.focus.set_focus(Some(panel));
        mux.focus.set_scroll_open(true);

        mux.handle_paste("pasted while scrolling");

        assert!(
            mux.main_compose_since.is_none(),
            "the scrollback pager must swallow a paste — no compose hold armed",
        );
        assert_eq!(
            mux.panels.iter().find(|p| p.id == panel).unwrap().state(),
            PanelState::Idle,
        );

        mux.shutdown();
    }

    /// The scroll wheel pages an open scrollback by `WHEEL_STEP` lines and
    /// clamps at the bottom; a non-scroll mouse event and the close-confirm
    /// modal both leave the pager untouched.
    #[tokio::test]
    async fn mouse_wheel_pages_the_scrollback_and_respects_the_confirm_modal() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let at = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };

        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        // 20 lines, page 4 → max offset 16. Start at the bottom (newest).
        mux.scroll = Some(ScrollState::new(
            "worker".to_string(),
            (0..20).map(|i| format!("l{i}")).collect(),
            16,
            4,
        ));

        // Wheel up moves toward older output by WHEEL_STEP (3).
        mux.handle_mouse(at(MouseEventKind::ScrollUp));
        assert_eq!(mux.scroll_state().unwrap().offset, 13);
        // Wheel down moves back toward the newest.
        mux.handle_mouse(at(MouseEventKind::ScrollDown));
        assert_eq!(mux.scroll_state().unwrap().offset, 16);
        // Down at the bottom clamps — never past the max.
        mux.handle_mouse(at(MouseEventKind::ScrollDown));
        assert_eq!(mux.scroll_state().unwrap().offset, 16);
        // A non-scroll event (a click) is ignored.
        mux.handle_mouse(at(MouseEventKind::Down(MouseButton::Left)));
        assert_eq!(mux.scroll_state().unwrap().offset, 16);

        // While the close-confirm prompt is up the wheel is swallowed.
        mux.pending_close = Some(PanelId::new());
        mux.handle_mouse(at(MouseEventKind::ScrollUp));
        assert_eq!(
            mux.scroll_state().unwrap().offset,
            16,
            "the confirm modal swallows the wheel"
        );
    }

    /// Scrolling up from the live view opens the pager on the focused panel
    /// (tmux copy-mode-on-scroll entry). With nothing focused it stays closed.
    #[tokio::test]
    async fn mouse_wheel_up_enters_scrollback_on_the_focused_panel() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let wheel_up = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };

        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        // Nothing focused → wheel up cannot open a pager (no panic).
        mux.handle_mouse(wheel_up);
        assert!(mux.scroll_state().is_none());

        // Focus a panel → wheel up enters scrollback on it.
        let panel = push_cat_panel(&mut mux, PanelState::Idle);
        mux.focus.set_focus(Some(panel));
        mux.handle_mouse(wheel_up);
        assert!(
            mux.scroll_state().is_some(),
            "wheel up opens the pager from the live view"
        );

        mux.shutdown();
    }

    /// A prompt delivered to a *non-main* panel (the usual MCP `send_keys`
    /// fan-out) must not touch the main panel's compose hold.
    #[tokio::test]
    async fn note_prompt_delivered_for_non_main_keeps_compose_hold() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.main_panel_id = Some(PanelId::new());
        let held = Instant::now();
        mux.main_compose_since = Some(held);

        mux.note_prompt_delivered(PanelId::new());
        assert_eq!(
            mux.main_compose_since,
            Some(held),
            "a prompt to another panel must not clear the main compose hold"
        );
    }

    /// `ToggleTranscript` flips `show_transcript`; `HideTranscript` always
    /// clears it.
    #[tokio::test]
    async fn toggle_transcript_flips_show_transcript() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        assert!(!mux.show_transcript());

        mux.apply_command(CaucusCommand::ToggleTranscript);
        assert!(mux.show_transcript());

        mux.apply_command(CaucusCommand::ToggleTranscript);
        assert!(!mux.show_transcript());

        // Open it, then hide it explicitly.
        mux.apply_command(CaucusCommand::ToggleTranscript);
        assert!(mux.show_transcript());
        mux.apply_command(CaucusCommand::HideTranscript);
        assert!(!mux.show_transcript());
    }

    #[tokio::test]
    async fn quit_command_sets_should_quit() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        assert!(!mux.should_quit());
        mux.apply_command(CaucusCommand::Quit);
        assert!(mux.should_quit());
    }

    /// `CloseFocused` on the main worker panel is refused — main is protected,
    /// so no confirm is armed.
    #[tokio::test]
    async fn close_focused_protects_the_main_panel() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = PanelId::new();
        mux.main_panel_id = Some(main);
        mux.focus.set_focus(Some(main));

        mux.apply_command(CaucusCommand::CloseFocused);
        assert!(
            mux.pending_close().is_none(),
            "closing the main panel must be refused"
        );
    }

    /// `CloseFocused` on a non-main panel arms the confirm for that panel.
    #[tokio::test]
    async fn close_focused_arms_confirm_for_a_non_main_panel() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.main_panel_id = Some(PanelId::new());
        let target = PanelId::new();
        mux.focus.set_focus(Some(target));

        mux.apply_command(CaucusCommand::CloseFocused);
        assert_eq!(mux.pending_close(), Some(target));

        // Cancelling clears the pending close, leaving the panel alone.
        mux.apply_command(CaucusCommand::CancelClose);
        assert!(mux.pending_close().is_none());
    }

    /// `CloseFocused` with no focused panel is a no-op (no panic, no arm).
    #[tokio::test]
    async fn close_focused_with_no_focus_is_a_noop() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.apply_command(CaucusCommand::CloseFocused);
        assert!(mux.pending_close().is_none());
    }

    /// The render signature changes on any view-affecting change the
    /// dirty-gated draw must repaint for: a handled keystroke (via the view
    /// epoch — the catch-all for prefix-hint / layout / scroll toggles) and a
    /// focus change (which can be non-key-driven, e.g. a kill moving focus off
    /// a dead panel). An unchanged view yields a stable signature so an idle
    /// session does not repaint.
    #[tokio::test]
    async fn render_signature_tracks_view_changes() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let s0 = mux.render_signature();
        assert_eq!(
            s0,
            mux.render_signature(),
            "an unchanged view must yield a stable signature"
        );

        // A handled key bumps the view epoch → the signature must change.
        mux.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        let s1 = mux.render_signature();
        assert_ne!(s0, s1, "a handled key must change the render signature");

        // A focus change (not key-driven here) is reflected too.
        mux.focus.set_focus(Some(PanelId::new()));
        let s2 = mux.render_signature();
        assert_ne!(s1, s2, "a focus change must change the render signature");
    }

    /// `pump_all` probes child liveness on its first call, then throttles:
    /// a second pump within `LIVENESS_PROBE_INTERVAL` must not re-probe
    /// (the timestamp is unchanged), so per-panel `waitpid` does not run
    /// every ~4 ms tick on the idle loop.
    #[tokio::test]
    async fn pump_all_throttles_the_liveness_probe() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        assert!(
            mux.last_liveness_probe.is_none(),
            "no probe has run before the first pump"
        );

        mux.pump_all();
        let first = mux.last_liveness_probe;
        assert!(first.is_some(), "the first pump_all probes liveness");

        // A second pump immediately after is inside the interval — the probe
        // timestamp must be untouched (no second waitpid sweep).
        mux.pump_all();
        assert_eq!(
            mux.last_liveness_probe, first,
            "a pump within LIVENESS_PROBE_INTERVAL must not re-probe"
        );

        // Backdate the latch past the interval; the next pump re-probes.
        mux.last_liveness_probe = Some(Instant::now() - LIVENESS_PROBE_INTERVAL);
        mux.pump_all();
        assert!(
            mux.last_liveness_probe.unwrap() > first.unwrap(),
            "a pump after the interval re-probes and advances the latch"
        );
    }
}
