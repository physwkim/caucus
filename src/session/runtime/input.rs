use super::*;
use crate::agent::lane_event::{LaneEvent, LaneEventKind};
use crate::agent::manifest;
use crate::agent::provenance::{self, LaneCommitProvenance, SupersededBy};
use crate::input::CaucusCommand;
use crate::panel::lifecycle::{self, PanelState};
use crate::signal::{
    AgentNote, CompactTrigger, LifecycleKind, LifecycleSignal, NoteKind, TurnSignal,
};
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

    /// Apply a mouse event (`docs/design.md` §1). The wheel has no mapping of
    /// its own: a notch up/down *is* a `PageUp`/`PageDown` keypress, routed
    /// through the ordinary key path ([`Multiplexer::handle_key`]). Delivered to
    /// the event loop only when mouse capture is on (`[settings] mouse`).
    ///
    /// So in the live view the wheel pages the focused agent's own scrollback —
    /// the panel program handles `PageUp`/`PageDown` and redraws, so the content
    /// the user scrolled to is what they see. Caucus's frozen pager is opened
    /// deliberately (`Ctrl-A [`) and never by a wheel notch; while it *is* open
    /// the wheel pages it, because that is what the key does there. Every modal
    /// gate (close-confirm capture, pager capture) comes free from the key
    /// router. Clicks, drags, and moves have no key equivalent and are ignored.
    pub fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
        let code = match mouse.kind {
            MouseEventKind::ScrollUp => KeyCode::PageUp,
            MouseEventKind::ScrollDown => KeyCode::PageDown,
            _ => return,
        };
        self.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
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
            CaucusCommand::ToggleZoom => self.toggle_zoom(),
            CaucusCommand::CloseFocused => self.arm_close_confirm(),
            CaucusCommand::ConfirmClose => self.confirm_close(),
            CaucusCommand::CancelClose => self.cancel_close(),
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
        self.handle_signal_with_reply(signal, None);
    }

    /// [`Multiplexer::handle_signal`] with the signal's reply slot — the
    /// hook-reply round delivery (`docs/design.md` §7.6).
    ///
    /// When the **main** panel's own Stop fires while a round is due, the
    /// summary is sent back through `reply` instead of waiting for main to go
    /// idle and typing it in: the hook prints Claude's block JSON and the main
    /// worker receives the round *inside the same turn* — closing the window
    /// where a round that completed mid-turn sat until the next idle gap.
    ///
    /// The delivery preconditions, checked in order, are: the reply slot
    /// exists and its receiver is still waiting (a hook that already timed out
    /// must not consume a round — the round stays for the keystroke push); the
    /// panel is the main panel; no human keystroke is holding the compose
    /// gate (the "no automated delivery while the human is composing" rule is
    /// path-independent — held here too, and the sender drop answers allow);
    /// the *previous* boundary was not itself hook-continued
    /// (`main_last_boundary_hook_continued`, the alternation gate — caucus
    /// never continues the main on two consecutive boundaries, so it always
    /// reaches a real idle boundary where Claude Code can compact and the user
    /// can type); and a round is actually due
    /// ([`Multiplexer::take_due_round_summary`]). Every precondition that fails
    /// simply drops the sender — allow — and the signal is handled exactly as
    /// before; the still-due round then rides the keystroke push.
    ///
    /// A delivered reply means the turn *continues*: the panel skips the
    /// `Idle` transition and its capture turn is reopened
    /// ([`Multiplexer::reopen_turn_after_hook_delivery`]). If the receiver
    /// disappears between the check and the send (the client timed out at
    /// that instant), the round has already been completed and removed — the
    /// summary falls back to the keystroke path immediately, and failing
    /// that, to `dropped-rounds.log` (the full report is already on disk).
    pub fn handle_signal_with_reply(
        &mut self,
        signal: TurnSignal,
        reply: Option<tokio::sync::oneshot::Sender<crate::signal::StopDirective>>,
    ) {
        use crate::signal::StopDirective;

        let Some(panel) = self.panels.iter_mut().find(|p| p.id == signal.panel_id) else {
            // An unknown panel's reply sender (if any) drops here — allow.
            return;
        };
        panel.end_turn();

        // A real turn boundary supersedes any manual-compact latch: the Stop
        // proves the panel ran (and finished) an agent turn since the
        // `PreCompact`, so a later `SessionStart(compact)` from *auto*
        // compaction must not be mistaken for the manual close.
        self.manual_compact_inflight.remove(&signal.panel_id);

        // Hook-reply delivery, decided before the Idle transition. The reason
        // carries every deliverable waiting for main's attention: the first
        // due round (the primary deliverable, so it leads), then the whole
        // question-notice queue — the reply is one continuing turn, so a
        // notice held back would wait an entire further main turn for the
        // push path, which is the latency this delivery exists to remove.
        // Both takes are gated behind the checks, so nothing is consumed on a
        // path that cannot deliver; with nothing pending the sender drops —
        // allow.
        let mut delivered_by_hook = false;
        let mut keystroke_fallback: Option<String> = None;
        if let Some(sender) = reply
            && Some(signal.panel_id) == self.main_panel_id
            && !sender.is_closed()
            && self.main_compose_quiet()
            && !self.main_last_boundary_hook_continued
        {
            let mut parts: Vec<String> = Vec::new();
            parts.extend(self.take_due_round_summary());
            parts.extend(self.take_question_notice_texts());
            if !parts.is_empty() {
                let reason = parts.join("\n\n");
                match sender.send(StopDirective::Deliver { reason }) {
                    Ok(()) => delivered_by_hook = true,
                    Err(StopDirective::Deliver { reason }) => keystroke_fallback = Some(reason),
                }
            }
        }

        // Alternation gate: remember whether *this* main boundary was
        // hook-continued so the next one is not (a due round then rides the
        // keystroke push, which waits for the main to be `Idle`). Only main
        // boundaries move the flag — a sub panel's Stop leaves it untouched.
        // An allowed boundary (`delivered_by_hook == false`) clears it, so the
        // main is never continued twice in a row and always reaches a real
        // idle boundary within one turn.
        if Some(signal.panel_id) == self.main_panel_id {
            self.main_last_boundary_hook_continued = delivered_by_hook;
        }

        // A turn signal means the agent is idle, waiting for the next prompt —
        // unless the hook delivery just continued the turn.
        if !delivered_by_hook
            && let Some(panel) = self.panels.iter_mut().find(|p| p.id == signal.panel_id)
            && panel.state() == PanelState::Working
        {
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
        self.record_commit_provenance(signal.panel_id);
        self.record_commit_supersessions(signal.panel_id);

        if delivered_by_hook {
            self.reopen_turn_after_hook_delivery(signal.panel_id);
        }
        if let Some(summary) = keystroke_fallback {
            // The receiver vanished after the liveness check: the round (if
            // one was taken) is already completed and the notices are out of
            // their queue, so this text is their only copy in flight. The
            // panel just flipped Idle above, which is exactly the state the
            // keystroke push delivers into.
            warn!(
                panel = %signal.panel_id,
                "hook reply receiver gone after payload was taken; delivering by keystroke"
            );
            if let Err(err) =
                crate::mcp::McpToolSurface::send_keys(self, signal.panel_id, &summary, true)
            {
                warn!(error = %err, "keystroke fallback failed; spilling to dropped-rounds.log");
                self.append_dropped_round(
                    "----- dropped turn-boundary delivery (hook reply lost and keystroke \
                     fallback failed) -----",
                    &summary,
                );
            }
        }
    }

    /// Reopen the main panel's capture turn after a turn-boundary payload (a
    /// round summary and/or queued question notices) was delivered through
    /// its Stop hook reply: the blocked Stop continues the same turn, so the
    /// panel must read as mid-turn again — `begin_turn()` for the capture,
    /// `Working` kept (or restored), and the injected prompt recorded on the
    /// timeline.
    ///
    /// Deliberately a sibling of [`Multiplexer::note_prompt_delivered`], not a
    /// call to it: that path clears `main_compose_since`, because a *submitted
    /// line* consumed the human's composition. Nothing was submitted here — a
    /// hook reply travels the socket, not the input line — so a compose hold
    /// the human owns must survive the delivery.
    fn reopen_turn_after_hook_delivery(&mut self, panel_id: PanelId) {
        let Some(panel) = self.panels.iter_mut().find(|p| p.id == panel_id) else {
            return;
        };
        panel.begin_turn();
        if matches!(panel.state(), PanelState::Spawning | PanelState::Idle) {
            let _ = lifecycle::transition(panel, PanelState::Working);
        }
        self.record_prompt_delivered(panel_id);
    }

    /// Ingest a lifecycle hook signal (`PreCompact` / `SessionStart`) — the
    /// close for a *local* slash command (`/compact`, `/clear`), which runs no
    /// agent turn and therefore never fires the Stop hook that is otherwise
    /// the only `Working -> Idle` producer. Without this, a panel handed
    /// `/compact` wedges in `working` forever and its round never settles.
    ///
    /// The latch (`manual_compact_inflight`) distinguishes the two producers
    /// of `SessionStart(source=compact)`:
    /// * a **manual** `/compact` announces itself first via
    ///   `PreCompact(trigger=manual)` — latch, then close on the matching
    ///   `SessionStart(compact)`;
    /// * **auto** compaction fires `PreCompact(trigger=auto)` +
    ///   `SessionStart(compact)` *mid-turn* — no latch, so the `SessionStart`
    ///   is ignored and the genuinely-working panel keeps its `Working` (its
    ///   real Stop closes the turn later).
    ///
    /// `SessionStart(source=clear)` closes unlatched: `/clear` has no
    /// `PreCompact`, and its `SessionStart` has no mid-turn producer to
    /// confuse it with. Every other source (`startup`, `resume`) is an agent
    /// (re)launch, not a local-command completion — ignored.
    pub fn handle_lifecycle(&mut self, sig: LifecycleSignal) {
        match sig.kind {
            LifecycleKind::PreCompact {
                trigger: CompactTrigger::Manual,
            } => {
                // Latch only a live panel — a stray signal for a killed panel
                // must not leave a latch that outlives its owner.
                if self.panels.iter().any(|p| p.id == sig.panel_id) {
                    self.manual_compact_inflight.insert(sig.panel_id);
                }
            }
            LifecycleKind::PreCompact {
                trigger: CompactTrigger::Auto,
            } => {}
            LifecycleKind::SessionStart { ref source } => match source.as_str() {
                "compact" => {
                    if self.manual_compact_inflight.remove(&sig.panel_id) {
                        self.close_local_command(sig.panel_id, "/compact");
                    }
                }
                "clear" => {
                    self.manual_compact_inflight.remove(&sig.panel_id);
                    self.close_local_command(sig.panel_id, "/clear");
                }
                _ => {}
            },
        }
    }

    /// Close the turn phase a local slash command occupied: end the capture
    /// turn, flip a `Working` panel back to `Idle` (via `lifecycle::transition`,
    /// Invariant I-5), and record `LocalCommandCompleted` on the manifest so
    /// `derived_state` follows in lockstep (Invariant I-2) and a pending round
    /// on the panel can settle. A panel that was not `Working` (the submit
    /// path already classified the command and never opened a turn) keeps its
    /// state — the manifest still records the completion fact.
    fn close_local_command(&mut self, panel_id: PanelId, command: &str) {
        let Some(panel) = self.panels.iter_mut().find(|p| p.id == panel_id) else {
            return;
        };
        panel.end_turn();
        if panel.state() == PanelState::Working {
            let _ = lifecycle::transition(panel, PanelState::Idle);
        }
        if let Some(manifest) = self.manifests.get_mut(&panel_id)
            && let Err(err) =
                manifest::record_local_command_completed(manifest, &self.session.root_dir, command)
        {
            warn!(panel = %panel_id, error = %err, "local-command manifest write failed");
        }
    }

    /// Ingest a mid-turn note: cap its body once (`AgentNote::truncated`, so
    /// the manifest record and any notice see the same text) and record it on
    /// the panel's manifest via `agent::manifest::record_note` (Invariant
    /// I-2). Deliberately NO panel state transition — a note arrives mid-turn;
    /// the turn is talking, not ending, so `Working` survives it. A
    /// `question` note is additionally queued for the main worker
    /// ([`Multiplexer::poll_question_notices`]).
    pub fn handle_note(&mut self, note: AgentNote) {
        // A note for a panel caucus does not know (already killed, or a stray
        // client) has nowhere to be recorded and no one to be forwarded for.
        let Some(panel) = self.panels.iter().find(|p| p.id == note.panel_id) else {
            return;
        };
        let role = panel.role.clone();
        let note = note.truncated();
        if let Some(manifest) = self.manifests.get_mut(&note.panel_id)
            && let Err(err) = manifest::record_note(manifest, &self.session.root_dir, &note)
        {
            warn!(panel = %note.panel_id, error = %err, "manifest note write failed");
        }
        if note.kind == NoteKind::Question {
            self.enqueue_question_notice(note.panel_id, role, note.body);
        }
    }

    /// Drain every panel's queued in-band desktop notifications (OSC 9 / 99 /
    /// 777 — `term::Grid`) onto its manifest timeline as `NotificationSeen`
    /// events (`docs/design.md` §7.7). Runs once per event-loop tick, after
    /// the PTY pump that fills the queues.
    ///
    /// Capture only: no panel state transition and no settle semantics.
    /// Whether a notification may ever hint settlement (D-2) is decided
    /// elsewhere and would have to route through the turn-completion owner
    /// (`handle_signal` → `record_turn_completed`) — never through here.
    pub(crate) fn poll_notifications(&mut self) {
        let drained: Vec<(PanelId, Vec<String>)> = self
            .panels
            .iter_mut()
            .map(|p| (p.id, p.take_notifications()))
            .filter(|(_, texts)| !texts.is_empty())
            .collect();
        for (panel_id, texts) in drained {
            let Some(manifest) = self.manifests.get_mut(&panel_id) else {
                continue;
            };
            for body in texts {
                if let Err(err) =
                    manifest::record_notification(manifest, &self.session.root_dir, &body)
                {
                    warn!(panel = %panel_id, error = %err, "manifest notification write failed");
                }
            }
        }
    }

    /// Record a `CommitCreated` event when the turn that just ended named a
    /// commit this agent left on its own branch (`docs/design.md` §5).
    ///
    /// A sub-agent's commits live on its worktree's branch and outlive the panel,
    /// but nothing tied them back to the agent that made them: `git log` on the
    /// branch shows the commit, and the manifest showed the branch, and no record
    /// joined the two. The turn signal's `last_message` is where an agent says
    /// what it did, so a SHA it names there is the join — but only once git has
    /// confirmed both halves of it (`provenance::extract_branch_commit`): the
    /// commit exists, *and* it is on this panel's branch. A worktree shares its
    /// object database with every other worktree, so the first half alone would
    /// let an agent claim a sibling panel's commit merely by mentioning it.
    ///
    /// Panels with no worktree are skipped: they commit in the shared checkout if
    /// they commit at all, and a SHA there names no work this panel owns. So are
    /// panels whose branch caucus never learned — with no branch there is no join
    /// to verify, and an unverified join is what this exists to prevent.
    fn record_commit_provenance(&mut self, panel_id: PanelId) {
        let Some(manifest) = self.manifests.get(&panel_id) else {
            return;
        };
        let (Some(worktree), Some(last_message)) =
            (manifest.worktree_path.clone(), manifest.last_message())
        else {
            return;
        };
        let Some(branch) = self.worktree_branches.get(&panel_id).cloned() else {
            return;
        };
        let Some(commit) = provenance::extract_branch_commit(&worktree, &branch, last_message)
        else {
            return;
        };
        // An agent that keeps referring to the commit it made ("still working on
        // top of abc1234") names it again every turn. The commit was created
        // once, so it is recorded once — a timeline that repeats the same
        // creation reads like repeated work.
        if manifest.live_commits().iter().any(|p| p.commit == commit) {
            return;
        }
        self.record_lane_event(
            panel_id,
            LaneEventKind::CommitCreated {
                provenance: LaneCommitProvenance {
                    commit,
                    branch,
                    worktree: Some(worktree),
                },
            },
        );
    }

    /// Record a `CommitSuperseded` event for every commit the panel created that
    /// its branch no longer holds (`docs/design.md` §8.2).
    ///
    /// An agent that amends or rebases leaves the sha it announced last turn
    /// pointing at a commit no branch contains. Without this, the timeline keeps
    /// claiming a commit that `git log` cannot show, and every later reader of
    /// the lane — a review round, a human reading the manifest — chases a sha
    /// that resolves to nothing. `provenance::detect_supersession` asks the
    /// branch, so the timeline says what the repository says.
    ///
    /// Runs on the turn signal, after `record_commit_provenance`: a rewrite is
    /// something the agent did *during* the turn that just ended, and the turn
    /// signal is caucus's only notification that it did anything at all.
    ///
    /// Gated on the lane's branch tip. A commit's reachability from a branch can
    /// only change when the branch ref moves, so a tip unchanged since the last
    /// look is proof that nothing left the lane — one `rev-parse` (~2ms, measured)
    /// answers for every commit on it, instead of a `merge-base` process per
    /// recorded commit on every single turn signal. The turns that *did* rewrite
    /// history pay the full check; the ordinary ones pay one process.
    fn record_commit_supersessions(&mut self, panel_id: PanelId) {
        let Some(manifest) = self.manifests.get(&panel_id) else {
            return;
        };
        let Some(worktree) = manifest.worktree_path.clone() else {
            return;
        };
        let live = manifest.live_commits();
        if live.is_empty() {
            return; // No recorded commit on this lane — nothing can have left it.
        }
        let Some(branch) = self.worktree_branches.get(&panel_id).cloned() else {
            return;
        };
        let Some(tip) = provenance::branch_tip(&worktree, &branch) else {
            return; // git cannot say where the branch is; make no claim.
        };
        if self.checked_branch_tips.get(&panel_id) == Some(&tip) {
            return;
        }

        let retired: Vec<(String, SupersededBy)> = live
            .into_iter()
            .filter_map(|p| {
                provenance::detect_supersession(&worktree, &p.branch, &p.commit)
                    .map(|by| (p.commit, by))
            })
            .collect();
        for (commit, by) in retired {
            self.record_lane_event(panel_id, LaneEventKind::CommitSuperseded { commit, by });
        }
        // Only after the checks ran: a turn that returned early above must
        // re-check next time rather than trust a tip it never looked past.
        self.checked_branch_tips.insert(panel_id, tip);
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
        // A new prompt supersedes any manual-compact latch: the `Working` it
        // opens belongs to the new turn, and only that turn's own boundary may
        // close it — a stale latch must not let a later `SessionStart(compact)`
        // (from auto compaction during the new turn) flip it to `Idle` early.
        self.manual_compact_inflight.remove(&panel_id);
        let Some(panel) = self.panels.iter_mut().find(|p| p.id == panel_id) else {
            return;
        };
        panel.begin_turn();
        match panel.state() {
            PanelState::Spawning | PanelState::Idle => {
                let _ = lifecycle::transition(panel, PanelState::Working);
            }
            _ => {}
        }
        // This is the only `Idle -> Working` path, so it is where a delivered
        // prompt becomes a fact — the timeline's `PromptDelivered` is written
        // here or nowhere, and it also reopens the manifest's `derived_state`
        // to `Working` (what `list_panels` reports) in lockstep with the live
        // `PanelState` transition above.
        self.record_prompt_delivered(panel_id);
    }

    /// Single owner of lane-event appends for a live panel (Invariant I-2).
    ///
    /// Every timeline event a running session records goes through here: it
    /// resolves the panel's manifest and hands the event to
    /// `agent::manifest::write`, the single owner of manifest persistence. A
    /// caller that reached into `self.manifests` to push an event itself would
    /// leave the on-disk JSON stale until some later write happened to flush it.
    ///
    /// Best-effort: a failed manifest write is logged, not propagated. The
    /// timeline is a record of the session, not a gate on it — losing an event
    /// to a full disk must not fail the prompt that produced it.
    ///
    /// Not every event passes through here, and cannot: `WorktreeRemoved` is a
    /// fact known only after the panel is detached and its manifest dropped from
    /// this map, so the cleanup worker records it against the manifest on disk
    /// (`worktree::cleanup`). That is the one writer outside this owner, and it
    /// owns the manifest exclusively by then.
    pub(crate) fn record_lane_event(&mut self, panel_id: PanelId, kind: LaneEventKind) {
        let Some(manifest) = self.manifests.get_mut(&panel_id) else {
            return;
        };
        if let Err(err) =
            manifest::write(manifest, &self.session.root_dir, Some(LaneEvent::now(kind)))
        {
            warn!(panel = %panel_id, error = %err, "lane event write failed");
        }
    }

    /// Owner of the `PromptDelivered` turn-phase transition on the manifest —
    /// the sibling of [`Multiplexer::record_lane_event`] for the one lane event
    /// that also advances `derived_state`. Routes through
    /// [`manifest::record_prompt_delivered`] so the manifest state `list_panels`
    /// reports flips to `working` in lockstep with the live [`PanelState`],
    /// instead of appending through the generic `write` that never recomputes.
    /// Best-effort, like `record_lane_event`.
    fn record_prompt_delivered(&mut self, panel_id: PanelId) {
        let Some(manifest) = self.manifests.get_mut(&panel_id) else {
            return;
        };
        if let Err(err) = manifest::record_prompt_delivered(manifest, &self.session.root_dir) {
            warn!(panel = %panel_id, error = %err, "prompt-delivered manifest write failed");
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

    /// A delivered prompt lands `PromptDelivered` on the panel's timeline, and
    /// it is persisted — `record_lane_event` routes through
    /// `agent::manifest::write`, so a reader of the on-disk manifest sees it
    /// without waiting for some later write to flush it.
    #[tokio::test]
    async fn note_prompt_delivered_records_the_event_on_the_manifest() {
        use crate::agent::manifest::AgentManifest;
        use crate::role::spec::AgentCli;

        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = push_cat_panel(&mut mux, PanelState::Idle);
        let mf = AgentManifest::new(
            mux.session.id,
            id,
            "reviewer",
            "reviewer-1",
            AgentCli::Claude,
            None,
        );
        let agent_id = mf.agent_id;
        mux.manifests.insert(id, mf);

        mux.note_prompt_delivered(id);

        let on_disk = crate::agent::manifest::read(&mux.session.root_dir, agent_id).unwrap();
        assert!(
            on_disk
                .lane_events()
                .iter()
                .any(|e| matches!(e.kind, LaneEventKind::PromptDelivered)),
            "a delivered prompt is on the persisted timeline: {:?}",
            on_disk.lane_events()
        );
        mux.shutdown();
    }

    /// After a completed turn pins the manifest `Idle` (the resume Stop-hook
    /// scenario), delivering a new prompt reopens `derived_state` to `Working`,
    /// so `list_panels` reports the commanded panel as `working` rather than
    /// leaving it stuck at `idle`. Regression for the resumed-worker-idle bug.
    #[tokio::test]
    async fn note_prompt_delivered_reopens_derived_state_after_a_completed_turn() {
        use crate::agent::derive_state::DerivedState;
        use crate::agent::manifest::{self, AgentManifest};
        use crate::role::spec::AgentCli;
        use crate::signal::{TurnKind, TurnSignal};

        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = push_cat_panel(&mut mux, PanelState::Idle);
        let mf = AgentManifest::new(
            mux.session.id,
            id,
            "reviewer",
            "reviewer-1",
            AgentCli::Claude,
            None,
        );
        let agent_id = mf.agent_id;
        mux.manifests.insert(id, mf);

        // A prior turn completes → the manifest's derived_state is pinned Idle,
        // exactly as a resumed agent's reloaded Stop hook leaves it.
        let signal = TurnSignal::now(
            mux.session.id,
            id,
            TurnKind::Stop,
            Some("prior".into()),
            serde_json::Value::Null,
        );
        manifest::record_turn_completed(
            mux.manifests.get_mut(&id).unwrap(),
            &mux.session.root_dir,
            &signal,
        )
        .unwrap();
        assert_eq!(
            mux.manifests.get(&id).unwrap().derived_state(),
            DerivedState::Idle
        );

        // A new command reopens the turn — the value `list_panels` reports.
        mux.note_prompt_delivered(id);
        assert_eq!(
            mux.manifests.get(&id).unwrap().derived_state(),
            DerivedState::Working,
            "a commanded panel must report working, not idle, to list_panels"
        );
        let on_disk = manifest::read(&mux.session.root_dir, agent_id).unwrap();
        assert_eq!(on_disk.derived_state(), DerivedState::Working);
        mux.shutdown();
    }

    /// A mid-turn note records on the panel's manifest and does NOT end the
    /// turn: the panel stays `Working` (contrast `handle_signal`, which flips
    /// it `Idle`). A non-`question` note queues nothing for the main worker.
    #[tokio::test]
    async fn handle_note_records_without_ending_the_turn() {
        use crate::agent::manifest::AgentManifest;
        use crate::role::spec::AgentCli;

        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = push_cat_panel(&mut mux, PanelState::Working);
        let mf = AgentManifest::new(
            mux.session.id,
            id,
            "reviewer",
            "reviewer-1",
            AgentCli::Claude,
            None,
        );
        let agent_id = mf.agent_id;
        mux.manifests.insert(id, mf);

        mux.handle_note(AgentNote::now(
            mux.session.id,
            id,
            NoteKind::Progress,
            "halfway through the sweep".into(),
        ));

        assert_eq!(
            mux.panels.iter().find(|p| p.id == id).unwrap().state(),
            PanelState::Working,
            "a note must not end the turn"
        );
        assert!(
            mux.pending_question_notices.is_empty(),
            "a progress note queues nothing for main"
        );
        let on_disk = crate::agent::manifest::read(&mux.session.root_dir, agent_id).unwrap();
        assert_eq!(
            on_disk.last_note(),
            Some("[progress] halfway through the sweep")
        );
        mux.shutdown();
    }

    /// A cat panel with a manifest, flipped `Working` by a delivered prompt —
    /// exactly the state a panel handed `/compact` wedges in, since a local
    /// command runs no agent turn and no Stop hook ever closes it.
    fn working_panel_with_manifest(
        mux: &mut Multiplexer,
    ) -> (PanelId, crate::session::id::AgentId) {
        use crate::agent::manifest::AgentManifest;
        use crate::role::spec::AgentCli;
        let id = push_cat_panel(mux, PanelState::Idle);
        let mf = AgentManifest::new(
            mux.session.id,
            id,
            "reviewer",
            "reviewer-1",
            AgentCli::Claude,
            None,
        );
        let agent_id = mf.agent_id;
        mux.manifests.insert(id, mf);
        mux.note_prompt_delivered(id);
        (id, agent_id)
    }

    fn panel_state(mux: &Multiplexer, id: PanelId) -> PanelState {
        mux.panels.iter().find(|p| p.id == id).unwrap().state()
    }

    fn lifecycle(mux: &mut Multiplexer, id: PanelId, kind: LifecycleKind) {
        let session = mux.session.id;
        mux.handle_lifecycle(LifecycleSignal::now(
            session,
            id,
            kind,
            serde_json::Value::Null,
        ));
    }

    /// The wedge this whole change closes: a manual `/compact` — `PreCompact
    /// (trigger=manual)` then `SessionStart(source=compact)`, with NO Stop
    /// hook in between — flips the panel `Working -> Idle`, recomputes the
    /// manifest's `derived_state` in lockstep, and records the completion on
    /// the timeline. The PreCompact alone closes nothing (compaction is still
    /// running).
    #[tokio::test]
    async fn lifecycle_manual_compact_closes_the_working_wedge() {
        use crate::agent::derive_state::DerivedState;
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let (id, agent_id) = working_panel_with_manifest(&mut mux);
        assert_eq!(panel_state(&mux, id), PanelState::Working);

        lifecycle(
            &mut mux,
            id,
            LifecycleKind::PreCompact {
                trigger: CompactTrigger::Manual,
            },
        );
        assert_eq!(
            panel_state(&mux, id),
            PanelState::Working,
            "PreCompact only latches — compaction has not finished yet"
        );

        lifecycle(
            &mut mux,
            id,
            LifecycleKind::SessionStart {
                source: "compact".into(),
            },
        );
        assert_eq!(panel_state(&mux, id), PanelState::Idle);
        assert!(
            mux.manual_compact_inflight.is_empty(),
            "the close consumes the latch"
        );
        let on_disk = crate::agent::manifest::read(&mux.session.root_dir, agent_id).unwrap();
        assert_eq!(
            on_disk.derived_state(),
            DerivedState::Idle,
            "list_panels must report idle so a pending round can settle"
        );
        assert!(
            on_disk.lane_events().iter().any(|e| matches!(
                &e.kind,
                LaneEventKind::LocalCommandCompleted { command } if command == "/compact"
            )),
            "the completion is on the persisted timeline: {:?}",
            on_disk.lane_events()
        );
        mux.shutdown();
    }

    /// *Auto* compaction happens mid-turn: `PreCompact(trigger=auto)` +
    /// `SessionStart(source=compact)` with no manual latch. The panel is
    /// genuinely working — the SessionStart must NOT flip it `Idle` (its real
    /// Stop closes the turn later).
    #[tokio::test]
    async fn lifecycle_auto_compact_session_start_leaves_working() {
        use crate::agent::derive_state::DerivedState;
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let (id, _) = working_panel_with_manifest(&mut mux);

        lifecycle(
            &mut mux,
            id,
            LifecycleKind::PreCompact {
                trigger: CompactTrigger::Auto,
            },
        );
        lifecycle(
            &mut mux,
            id,
            LifecycleKind::SessionStart {
                source: "compact".into(),
            },
        );

        assert_eq!(
            panel_state(&mux, id),
            PanelState::Working,
            "an unlatched SessionStart(compact) is auto-compaction mid-turn"
        );
        assert_eq!(
            mux.manifests.get(&id).unwrap().derived_state(),
            DerivedState::Working
        );
        mux.shutdown();
    }

    /// `/clear` fires no `PreCompact`, and its `SessionStart(source=clear)`
    /// has no mid-turn producer to confuse it with — it closes unlatched.
    #[tokio::test]
    async fn lifecycle_session_start_clear_closes_unlatched() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let (id, agent_id) = working_panel_with_manifest(&mut mux);

        lifecycle(
            &mut mux,
            id,
            LifecycleKind::SessionStart {
                source: "clear".into(),
            },
        );

        assert_eq!(panel_state(&mux, id), PanelState::Idle);
        let on_disk = crate::agent::manifest::read(&mux.session.root_dir, agent_id).unwrap();
        assert!(
            on_disk.lane_events().iter().any(|e| matches!(
                &e.kind,
                LaneEventKind::LocalCommandCompleted { command } if command == "/clear"
            )),
            "{:?}",
            on_disk.lane_events()
        );
        mux.shutdown();
    }

    /// A `SessionStart` from an agent (re)launch — `startup`, `resume`, or a
    /// source this binary does not know — is not a local-command completion
    /// and must not touch a working panel.
    #[tokio::test]
    async fn lifecycle_session_start_startup_and_resume_are_ignored() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let (id, _) = working_panel_with_manifest(&mut mux);

        for source in ["startup", "resume", "some-future-source"] {
            lifecycle(
                &mut mux,
                id,
                LifecycleKind::SessionStart {
                    source: source.into(),
                },
            );
            assert_eq!(
                panel_state(&mux, id),
                PanelState::Working,
                "SessionStart({source}) must not close the turn"
            );
        }
        mux.shutdown();
    }

    /// A real turn boundary (Stop signal) supersedes a stale manual-compact
    /// latch: a later `SessionStart(compact)` — auto-compaction during the
    /// panel's *next* turn — must not close that turn early.
    #[tokio::test]
    async fn lifecycle_latch_is_cleared_by_a_real_turn_boundary() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let (id, _) = working_panel_with_manifest(&mut mux);

        lifecycle(
            &mut mux,
            id,
            LifecycleKind::PreCompact {
                trigger: CompactTrigger::Manual,
            },
        );
        assert!(mux.manual_compact_inflight.contains(&id));

        let session = mux.session.id;
        mux.handle_signal(TurnSignal::now(
            session,
            id,
            crate::signal::TurnKind::Stop,
            Some("done".into()),
            serde_json::Value::Null,
        ));
        assert!(
            mux.manual_compact_inflight.is_empty(),
            "a Stop signal is a real boundary — the latch is stale"
        );

        // The panel's next turn: an auto-compact SessionStart mid-turn must
        // leave it Working, which only holds because the latch is gone.
        mux.note_prompt_delivered(id);
        lifecycle(
            &mut mux,
            id,
            LifecycleKind::SessionStart {
                source: "compact".into(),
            },
        );
        assert_eq!(panel_state(&mux, id), PanelState::Working);
        mux.shutdown();
    }

    /// A new prompt delivery supersedes a stale manual-compact latch: the
    /// `Working` it opens belongs to the new turn, so a later
    /// `SessionStart(compact)` must not flip it early.
    #[tokio::test]
    async fn lifecycle_latch_is_cleared_by_a_new_prompt() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let (id, _) = working_panel_with_manifest(&mut mux);

        lifecycle(
            &mut mux,
            id,
            LifecycleKind::PreCompact {
                trigger: CompactTrigger::Manual,
            },
        );
        mux.note_prompt_delivered(id);
        assert!(
            mux.manual_compact_inflight.is_empty(),
            "a delivered prompt supersedes the latch"
        );

        lifecycle(
            &mut mux,
            id,
            LifecycleKind::SessionStart {
                source: "compact".into(),
            },
        );
        assert_eq!(panel_state(&mux, id), PanelState::Working);
        mux.shutdown();
    }

    /// A `PreCompact(manual)` for a panel caucus does not know (already
    /// killed, or a stray client) must not leave a latch behind.
    #[tokio::test]
    async fn lifecycle_unknown_panel_never_latches() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let stray = PanelId::new();
        lifecycle(
            &mut mux,
            stray,
            LifecycleKind::PreCompact {
                trigger: CompactTrigger::Manual,
            },
        );
        assert!(mux.manual_compact_inflight.is_empty());
        mux.shutdown();
    }

    /// An OSC 9 the panel emitted lands on its manifest timeline as a
    /// `NotificationSeen` event, with no state transition — and a second poll
    /// records nothing new, because the drain consumed the queue.
    #[tokio::test]
    async fn poll_notifications_records_a_notification_seen_event() {
        use crate::agent::lane_event::LaneEventKind;
        use crate::agent::manifest::AgentManifest;
        use crate::role::spec::AgentCli;

        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = push_cat_panel(&mut mux, PanelState::Working);
        let mf = AgentManifest::new(
            mux.session.id,
            id,
            "reviewer",
            "reviewer-1",
            AgentCli::Claude,
            None,
        );
        let agent_id = mf.agent_id;
        mux.manifests.insert(id, mf);

        // Feed the escape the way the pump would: bytes through the grid.
        mux.panels
            .iter_mut()
            .find(|p| p.id == id)
            .unwrap()
            .grid
            .advance(b"\x1b]9;deploy done\x07");
        mux.poll_notifications();

        assert_eq!(
            mux.panels.iter().find(|p| p.id == id).unwrap().state(),
            PanelState::Working,
            "a notification must not end the turn"
        );
        let seen = |m: &AgentManifest| {
            m.lane_events()
                .iter()
                .filter(|e| matches!(&e.kind, LaneEventKind::NotificationSeen { body } if body == "deploy done"))
                .count()
        };
        let on_disk = crate::agent::manifest::read(&mux.session.root_dir, agent_id).unwrap();
        assert_eq!(on_disk.last_notification(), Some("deploy done"));
        assert_eq!(seen(&on_disk), 1);

        mux.poll_notifications();
        let on_disk = crate::agent::manifest::read(&mux.session.root_dir, agent_id).unwrap();
        assert_eq!(
            seen(&on_disk),
            1,
            "a drained notification is not re-recorded"
        );
        mux.shutdown();
    }

    /// A notification from a panel with no manifest (spawn raced, or a bare
    /// shell panel) has nowhere to be recorded — the poll drains the queue
    /// and drops the texts instead of panicking or leaking them to a later
    /// manifest.
    #[tokio::test]
    async fn poll_notifications_without_a_manifest_drops_the_texts() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = push_cat_panel(&mut mux, PanelState::Working);

        let panel = mux.panels.iter_mut().find(|p| p.id == id).unwrap();
        panel.grid.advance(b"\x1b]9;ping\x07");
        mux.poll_notifications();

        let panel = mux.panels.iter_mut().find(|p| p.id == id).unwrap();
        assert!(
            panel.take_notifications().is_empty(),
            "the poll drains the queue even with no manifest to record on"
        );
        mux.shutdown();
    }

    /// A `question` note is additionally queued for the main worker; delivery
    /// itself is `poll_question_notices`' job (tested in `rounds`).
    #[tokio::test]
    async fn handle_note_queues_a_question_for_the_main_worker() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = push_cat_panel(&mut mux, PanelState::Working);

        mux.handle_note(AgentNote::now(
            mux.session.id,
            id,
            NoteKind::Question,
            "which API version should I target?".into(),
        ));

        assert_eq!(mux.pending_question_notices.len(), 1);
        mux.shutdown();
    }

    /// A note for a panel caucus does not know (already killed, or a stray
    /// client) is dropped whole: nothing to record on, no one to forward for.
    #[tokio::test]
    async fn handle_note_for_an_unknown_panel_is_dropped() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        mux.handle_note(AgentNote::now(
            mux.session.id,
            PanelId::new(),
            NoteKind::Question,
            "does anyone hear me?".into(),
        ));

        assert!(mux.pending_question_notices.is_empty());
    }

    /// A turn signal whose final message names a commit this agent left on its
    /// own branch records `CommitCreated` — the join between an agent and the
    /// commits on its lane. Everything that is not that join records nothing: a
    /// hex-shaped token resolving to no commit (`deadbeef` in prose), a real
    /// commit that belongs to a *sibling* lane and is merely mentioned (the
    /// worktrees share one object database, so it resolves here too), and a panel
    /// with no worktree at all.
    #[tokio::test]
    async fn a_turn_that_names_a_real_commit_records_its_provenance() {
        use crate::agent::manifest::AgentManifest;
        use crate::agent::provenance::tests::repo_with_commit;
        use crate::agent::provenance::tests::{branch_of, commit_on_a_sibling_branch};
        use crate::role::spec::AgentCli;
        use crate::signal::TurnKind;

        let (repo, sha) = repo_with_commit();
        let lane = branch_of(repo.path());
        let sibling = commit_on_a_sibling_branch(repo.path(), &lane);
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let cases = [
            (
                Some(repo.path().to_path_buf()),
                format!("Done. Committed {} on my branch.", &sha[..12]),
                Some(sha.clone()),
            ),
            (
                Some(repo.path().to_path_buf()),
                "Done. See deadbeef for details.".to_string(),
                None,
            ),
            (
                Some(repo.path().to_path_buf()),
                format!("Reviewed {}; it looks right to me.", &sibling[..12]),
                None,
            ),
            (None, format!("Done. Committed {}.", &sha[..12]), None),
        ];

        for (worktree, message, expected) in cases {
            let id = push_cat_panel(&mut mux, PanelState::Working);
            if let Some(wt) = worktree.clone() {
                mux.panels
                    .iter_mut()
                    .find(|p| p.id == id)
                    .unwrap()
                    .worktree_path = Some(wt.clone());
                mux.worktree_branches.insert(id, lane.clone());
            }
            let mut mf = AgentManifest::new(
                mux.session.id,
                id,
                "reviewer",
                "reviewer-1",
                AgentCli::Claude,
                None,
            );
            mf.worktree_path = worktree.clone();
            let agent_id = mf.agent_id;
            mux.manifests.insert(id, mf);

            mux.handle_signal(TurnSignal::now(
                mux.session.id,
                id,
                TurnKind::Stop,
                Some(message),
                serde_json::Value::Null,
            ));

            let on_disk = crate::agent::manifest::read(&mux.session.root_dir, agent_id).unwrap();
            let recorded = on_disk.lane_events().iter().find_map(|e| match &e.kind {
                LaneEventKind::CommitCreated { provenance } => Some(provenance.clone()),
                _ => None,
            });
            match expected {
                Some(want) => {
                    let got = recorded.expect("a verified commit is recorded");
                    assert_eq!(got.commit, want, "the full canonical SHA is recorded");
                    assert_eq!(got.branch, lane);
                    assert_eq!(got.worktree, worktree);
                }
                None => assert!(
                    recorded.is_none(),
                    "an unverifiable SHA (or a panel with no worktree) records nothing: {recorded:?}"
                ),
            }
        }
        mux.shutdown();
    }

    /// An agent that amends the commit it announced last turn leaves the
    /// recorded SHA pointing at a commit no branch holds. The next turn signal
    /// records `CommitSuperseded` naming the commit that carries the same patch,
    /// and `live_commits` drops the dead SHA — so no reader of the lane chases a
    /// commit `git log` cannot show.
    #[tokio::test]
    async fn an_amended_commit_is_recorded_as_superseded_on_the_next_turn() {
        use crate::agent::manifest::AgentManifest;
        use crate::agent::provenance::tests::{amend_reword, branch_of, repo_with_commit};
        use crate::agent::provenance::{SupersededBy, verify_commit};
        use crate::role::spec::AgentCli;
        use crate::signal::TurnKind;

        let (repo, first) = repo_with_commit();
        let branch = branch_of(repo.path());
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let id = push_cat_panel(&mut mux, PanelState::Working);
        mux.worktree_branches.insert(id, branch.clone());
        let mut mf = AgentManifest::new(
            mux.session.id,
            id,
            "reviewer",
            "reviewer-1",
            AgentCli::Claude,
            None,
        );
        mf.worktree_path = Some(repo.path().to_path_buf());
        let agent_id = mf.agent_id;
        mux.manifests.insert(id, mf);

        let turn = |mux: &mut Multiplexer, message: String| {
            mux.handle_signal(TurnSignal::now(
                mux.session.id,
                id,
                TurnKind::Stop,
                Some(message),
                serde_json::Value::Null,
            ));
        };

        // Turn one: the agent announces the commit it made.
        turn(&mut mux, format!("Committed {}.", &first[..12]));
        assert_eq!(
            mux.manifests[&id]
                .live_commits()
                .iter()
                .map(|p| p.commit.clone())
                .collect::<Vec<_>>(),
            vec![first.clone()],
            "the announced commit is live while the branch holds it"
        );

        // Naming it again next turn does not re-create it: the commit was made
        // once, and the timeline says so once.
        turn(&mut mux, format!("Still building on {}.", &first[..12]));
        assert_eq!(
            mux.manifests[&id]
                .lane_events()
                .iter()
                .filter(|e| matches!(e.kind, LaneEventKind::CommitCreated { .. }))
                .count(),
            1,
            "a commit named across several turns is recorded once"
        );

        // The agent rewords it — same patch, new sha — and ends another turn.
        amend_reword(repo.path(), "reworded");
        let second = verify_commit(repo.path(), "HEAD").unwrap();
        turn(&mut mux, format!("Reworded it: {}.", &second[..12]));

        let on_disk = crate::agent::manifest::read(&mux.session.root_dir, agent_id).unwrap();
        let superseded: Vec<_> = on_disk
            .lane_events()
            .iter()
            .filter_map(|e| match &e.kind {
                LaneEventKind::CommitSuperseded { commit, by } => {
                    Some((commit.clone(), by.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            superseded,
            vec![(
                first.clone(),
                SupersededBy::Commit {
                    commit: second.clone()
                }
            )],
            "the dead SHA is retired exactly once, naming what replaced it"
        );
        assert_eq!(
            on_disk
                .live_commits()
                .iter()
                .map(|p| p.commit.clone())
                .collect::<Vec<_>>(),
            vec![second],
            "only the commit the branch holds survives the derivation"
        );

        // A third turn adds nothing: the retired commit is no longer live, so
        // it is not re-tested and not re-recorded.
        turn(&mut mux, "Nothing new.".to_string());
        let again = crate::agent::manifest::read(&mux.session.root_dir, agent_id).unwrap();
        assert_eq!(
            again
                .lane_events()
                .iter()
                .filter(|e| matches!(e.kind, LaneEventKind::CommitSuperseded { .. }))
                .count(),
            1,
            "a supersession is recorded once, not on every later turn"
        );

        mux.shutdown();
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

    /// Inside an open pager the wheel *is* `PgUp`/`PgDn`: it moves a full page
    /// and clamps at the bottom. A non-scroll mouse event and the close-confirm
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
        mux.focus.set_scroll_open(true);

        // Wheel up moves toward older output by one page (4).
        mux.handle_mouse(at(MouseEventKind::ScrollUp));
        assert_eq!(mux.scroll_state().unwrap().offset, 12);
        // Wheel down moves back toward the newest.
        mux.handle_mouse(at(MouseEventKind::ScrollDown));
        assert_eq!(mux.scroll_state().unwrap().offset, 16);
        // Down at the bottom clamps — never past the max.
        mux.handle_mouse(at(MouseEventKind::ScrollDown));
        assert_eq!(mux.scroll_state().unwrap().offset, 16);
        // A non-scroll event (a click) is ignored.
        mux.handle_mouse(at(MouseEventKind::Down(MouseButton::Left)));
        assert_eq!(mux.scroll_state().unwrap().offset, 16);

        // While the close-confirm prompt is up the wheel is swallowed — the same
        // modal capture the `PgUp` key hits, inherited from the key router.
        mux.focus.set_confirm_open(true);
        mux.handle_mouse(at(MouseEventKind::ScrollUp));
        assert_eq!(
            mux.scroll_state().unwrap().offset,
            16,
            "the confirm modal swallows the wheel"
        );
    }

    /// From the live view the wheel does NOT open caucus's frozen pager: it
    /// forwards `PageUp`/`PageDown` to the focused panel so the agent's own
    /// scrollback moves and stays visible. With nothing focused it is a no-op.
    #[tokio::test]
    async fn mouse_wheel_forwards_page_keys_to_the_focused_panel() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let at = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };

        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        // Nothing focused → the wheel has nowhere to go (no panic, no pager).
        mux.handle_mouse(at(MouseEventKind::ScrollUp));
        assert!(mux.scroll_state().is_none());

        // Focus a `cat` panel: it echoes whatever is written to its PTY, so the
        // wheel's bytes come back on the panel's own output. An open capture
        // turn is what records them (`OutputCapture::push` only fills an open
        // turn), so start one before scrolling.
        let panel = push_cat_panel(&mut mux, PanelState::Idle);
        mux.focus.set_focus(Some(panel));
        mux.panels
            .iter_mut()
            .find(|p| p.id == panel)
            .unwrap()
            .begin_turn();
        mux.handle_mouse(at(MouseEventKind::ScrollUp));
        mux.handle_mouse(at(MouseEventKind::ScrollDown));
        assert!(
            mux.scroll_state().is_none(),
            "the wheel must not open the frozen pager from the live view"
        );

        // Drain the echo and look for the xterm PageUp / PageDown sequences, in
        // order. The `[5~` / `[6~` tails are matched rather than the full
        // `ESC [ 5 ~`, so the assertion holds whether the tty echoes the ESC
        // byte literally or as `^[` (ECHOCTL).
        let mut echo = Vec::new();
        for _ in 0..200 {
            let p = mux.panels.iter_mut().find(|p| p.id == panel).unwrap();
            p.pump().unwrap();
            echo = p.capture().since_last_turn().to_vec();
            if echo.windows(3).any(|w| w == b"[6~") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pos_up = echo
            .windows(3)
            .position(|w| w == b"[5~")
            .expect("wheel up must forward the PageUp sequence to the panel");
        let pos_down = echo
            .windows(3)
            .position(|w| w == b"[6~")
            .expect("wheel down must forward the PageDown sequence to the panel");
        assert!(pos_up < pos_down, "the wheel keys arrive in the order sent");

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

        // A resize reflows the layout with no key and possibly no PTY output
        // (the display-wake heal path calls this directly) — it must repaint
        // on the next tick, not wait for the forced-redraw safety net.
        mux.resize(crate::render::Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 30,
        })
        .unwrap();
        let s3 = mux.render_signature();
        assert_ne!(s2, s3, "a resize must change the render signature");
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
