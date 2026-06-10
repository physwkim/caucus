use super::*;
use crate::mcp::{McpError, McpToolSurface, PanelSummary, ReadPanelMode};
use crate::role::spec::AgentCli;
use crate::session::id::PanelId;
use std::time::{Duration, Instant};
use tracing::warn;

/// The main worker's MCP tool surface, backed by the live panel registry
/// (`docs/design.md` §0 #4). Every method runs on the multiplexer's own
/// thread — control jobs are executed by [`Multiplexer::drain_control`] inside
/// the event loop, never concurrently with `pump_all` (Invariant I-5).
impl McpToolSurface for Multiplexer {
    fn send_keys(&mut self, panel: PanelId, text: &str, enter: bool) -> Result<(), McpError> {
        let p = self
            .panels
            .iter_mut()
            .find(|p| p.id == panel)
            .ok_or(McpError::NoSuchPanel(panel))?;
        // Frame the prompt for the agent's input mode (see `plan_delivery`).
        let plan = plan_delivery(text.as_bytes(), enter, p.grid().bracketed_paste());
        p.write_input(&plan.bytes)
            .map_err(|e| McpError::Tool(format!("send_keys: {e}")))?;

        // Delivering a prompt opens a capture turn and flips the panel to
        // `Working` (`docs/design.md` §4) — only when the line is submitted,
        // since a partial line is not yet a turn. The commitment is made here
        // even when the submit is deferred: caucus has decided to submit, and
        // the deferral is only the mechanical separation the agent needs to
        // accept the Enter.
        if enter {
            self.note_prompt_delivered(panel);
        }
        // A bracketed paste's submitting `\r` was held out of the burst — it
        // must land as a discrete keypress once the agent has ingested the
        // paste, or it is swallowed during the `[Pasted text #N]` commit. The
        // hold scales with the paste size (the commit it races is larger for a
        // bigger paste), so the byte count is threaded through.
        if plan.defer_submit {
            self.enqueue_submit(panel, text.len());
        }
        Ok(())
    }

    fn ctrl_c(&mut self, panel: PanelId) -> Result<(), McpError> {
        let p = self
            .panels
            .iter_mut()
            .find(|p| p.id == panel)
            .ok_or(McpError::NoSuchPanel(panel))?;
        // 0x03 = ETX = Ctrl-C.
        p.write_input(&[0x03])
            .map_err(|e| McpError::Tool(format!("ctrl_c: {e}")))
    }

    fn read_panel(&self, panel: PanelId, mode: ReadPanelMode) -> Result<String, McpError> {
        let p = self
            .panels
            .iter()
            .find(|p| p.id == panel)
            .ok_or(McpError::NoSuchPanel(panel))?;
        Ok(match mode {
            ReadPanelMode::Screen => Self::screen_text(p),
            ReadPanelMode::Scrollback => Self::scrollback_text(p),
            ReadPanelMode::SinceLastTurn => {
                // Whole-turn capture (`docs/design.md` §8.5), rendered to
                // readable text — the main worker never races the screen and
                // is never handed raw escape sequences.
                let (cols, _) = p.grid().size();
                Self::rendered_capture_text(p.capture().since_last_turn(), cols)
            }
            ReadPanelMode::LastMessage => self
                .manifests
                .get(&panel)
                .and_then(|m| m.last_message().map(str::to_string))
                .unwrap_or_default(),
        })
    }

    fn spawn_role(
        &mut self,
        role: &str,
        worktree: bool,
        model: Option<&str>,
        agent_cli: Option<AgentCli>,
        prompt: Option<&str>,
    ) -> Result<PanelId, McpError> {
        // Worktree creation is the slow part (`git worktree add`). The real
        // socket path defers it off the event loop (see
        // [`Multiplexer::begin_spawn_role_worktree`] / `poll_pending_spawns`);
        // this synchronous trait method — kept for direct callers and tests —
        // creates it inline, then shares the same finish path.
        let wt_handle = if worktree {
            Some(self.create_role_worktree(role)?)
        } else {
            None
        };
        self.finish_spawn_role(role, model, agent_cli, prompt, wt_handle)
    }

    fn kill_panel(&mut self, panel: PanelId) -> Result<(), McpError> {
        if !self.panels.iter().any(|p| p.id == panel) {
            return Err(McpError::NoSuchPanel(panel));
        }
        // Delegate to the inherent single-owner destruction path.
        Multiplexer::kill_panel(self, panel)
            .map_err(|e| McpError::Tool(format!("kill_panel: {e:#}")))
    }

    fn list_panels(&self) -> Vec<PanelSummary> {
        self.panels
            .iter()
            .map(|p| {
                // Prefer the manifest's derived_state (turn-signal fed); fall
                // back to the coarse panel-state label before the first turn.
                // A live selection menu on the grid overlays `awaiting_selection`
                // (no Stop hook fires while a chooser is open).
                let (state, agent_cli) = match self.manifests.get(&p.id) {
                    Some(m) => {
                        let st = Self::overlay_menu_state(
                            m.derived_state(),
                            Self::panel_menu(p).is_some(),
                        );
                        (st.as_str().to_string(), m.agent_cli)
                    }
                    None => (p.state_label().to_string(), AgentCli::Claude),
                };
                PanelSummary {
                    panel_id: p.id,
                    role: p.role.clone(),
                    state,
                    agent_cli,
                }
            })
            .collect()
    }

    fn read_menu(&self, panel: PanelId) -> Result<String, McpError> {
        let p = self
            .panels
            .iter()
            .find(|p| p.id == panel)
            .ok_or(McpError::NoSuchPanel(panel))?;
        Ok(match Self::panel_menu(p) {
            Some(menu) => Self::render_menu(&menu),
            None => "(no selection menu visible on this panel)".to_string(),
        })
    }

    fn select_option(&mut self, panel: PanelId, index: usize) -> Result<(), McpError> {
        // Scan the menu (immutable) before writing (mutable) — no overlapping
        // borrows of `self.panels`.
        let menu = {
            let p = self
                .panels
                .iter()
                .find(|p| p.id == panel)
                .ok_or(McpError::NoSuchPanel(panel))?;
            Self::panel_menu(p)
                .ok_or_else(|| McpError::Tool("no selection menu visible on this panel".into()))?
        };
        let target = menu
            .options
            .iter()
            .position(|o| o.number == index)
            .ok_or_else(|| {
                McpError::Tool(format!(
                    "no option {index} in the menu (options 1..={})",
                    menu.options.len()
                ))
            })?;
        let bytes = Self::menu_nav_bytes(menu.cursor, target);

        let p = self
            .panels
            .iter_mut()
            .find(|p| p.id == panel)
            .ok_or(McpError::NoSuchPanel(panel))?;
        p.write_input(&bytes)
            .map_err(|e| McpError::Tool(format!("select_option: {e}")))?;
        // Submitting a selection resumes the agent's turn — open a capture turn
        // and flip the panel to `Working`, exactly like the `send_keys` path.
        self.note_prompt_delivered(panel);
        Ok(())
    }
}

/// Floor delay for holding a bracketed paste's submitting Enter before
/// delivering it as a discrete keypress ([`Multiplexer::poll_pending_submits`]).
///
/// The agent reads its PTY in its own loop; this gap guarantees the held-back
/// `\r` arrives in a *separate* read cycle, after the agent has processed the
/// paste and committed its `[Pasted text #N]` placeholder, so the Enter is seen
/// as a submit rather than swallowed during the commit. Comfortably above the
/// agent's paste-processing latency for a small paste yet imperceptible.
const SUBMIT_DELAY_BASE: Duration = Duration::from_millis(100);

/// Extra hold per KiB of pasted text. The placeholder-commit the held-back
/// `\r` races against takes longer for a bigger paste, so a *constant* delay
/// (the prior design) was too short for a large report and let the Enter be
/// swallowed mid-commit — the panel was already flipped `Working`, so the
/// prompt then sat unsent until the round fallback. Scaling the hold with the
/// paste size keeps the guard proportional to the race it guards.
const SUBMIT_DELAY_PER_KIB: Duration = Duration::from_millis(4);

/// Cap on the scaled hold, so a pathologically large paste cannot defer a
/// submit for an absurd interval (the user would perceive the stall).
const SUBMIT_DELAY_MAX: Duration = Duration::from_millis(2000);

/// Hold time for a deferred submit whose paste was `paste_len` bytes:
/// [`SUBMIT_DELAY_BASE`] plus [`SUBMIT_DELAY_PER_KIB`] per KiB, capped at
/// [`SUBMIT_DELAY_MAX`]. Proportional to the placeholder-commit it races.
fn submit_delay_for(paste_len: usize) -> Duration {
    let scaled =
        SUBMIT_DELAY_BASE + SUBMIT_DELAY_PER_KIB * (paste_len / 1024).min(u32::MAX as usize) as u32;
    scaled.min(SUBMIT_DELAY_MAX)
}

/// A submitting Enter held back from a bracketed paste, waiting to be delivered
/// as a discrete keypress (see [`Multiplexer::pending_submits`]).
pub(crate) struct PendingSubmit {
    /// Panel the held-back `\r` is destined for.
    panel: PanelId,
    /// Earliest instant the `\r` may be written — `now + submit_delay_for(len)`
    /// at enqueue time (the hold scales with the paste size).
    due: Instant,
}

/// How `send_keys` should deliver `text` to a panel's PTY, framed for the
/// agent's input mode.
struct Delivery {
    /// Bytes to write to the PTY now.
    bytes: Vec<u8>,
    /// When true the submitting `\r` was deliberately *omitted* from `bytes`
    /// and must be delivered separately on a later tick (see `plan_delivery`).
    defer_submit: bool,
}

/// Decide how to deliver `text` (and, when `enter`, submit it) for the agent's
/// input mode.
///
/// A TUI agent (e.g. Claude Code) that has enabled bracketed-paste mode
/// (`CSI ?2004h`) treats a multi-byte input burst as a *paste*. Two failures
/// follow if the submitting `\r` rides along in that burst:
///
/// * a `\r` *inside* the `ESC[200~` … `ESC[201~` markers is inserted as a
///   literal newline (a multi-line report would submit at its first line); and
/// * a `\r` placed *after* the paste-end marker in the *same* write is still
///   swallowed when the agent commits a large paste to a `[Pasted text #N]`
///   placeholder — the Enter is consumed during that transition instead of
///   submitting. This is a race (it fires only for pastes big enough to become
///   a placeholder, and only when the agent is mid-commit), which is exactly
///   the "Enter didn't go through, but caucus already flipped the panel to
///   Working" symptom: the prompt sits unsent, no Stop signal ever fires.
///
/// So when the agent has bracketed paste on and there is text to submit, this
/// returns only the paste (`ESC[200~` … `ESC[201~`, no `\r`) and sets
/// `defer_submit`: `send_keys` holds the `\r` and
/// [`Multiplexer::poll_pending_submits`] writes it as a discrete keypress a
/// tick later, after the agent has ingested the paste.
///
/// When `bracketed` is false the agent has not enabled the mode (the markers
/// would land as literal `[200~`/`[201~` garbage) and when `text` is empty
/// there is nothing to paste (a bare Enter), so in both cases the `\r` is
/// written inline — there is no paste mode to absorb it — and no submit is
/// deferred.
fn plan_delivery(text: &[u8], enter: bool, bracketed: bool) -> Delivery {
    if bracketed && !text.is_empty() {
        let mut bytes = Vec::with_capacity(text.len() + 12);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(text);
        bytes.extend_from_slice(b"\x1b[201~");
        // The submitting `\r` is held back and delivered as a discrete
        // keypress — never inline with the paste.
        Delivery {
            bytes,
            defer_submit: enter,
        }
    } else {
        let mut bytes = text.to_vec();
        if enter {
            bytes.push(b'\r');
        }
        Delivery {
            bytes,
            defer_submit: false,
        }
    }
}

impl Multiplexer {
    /// Record a deferred submit for `panel`: its held-back `\r` becomes
    /// writable after [`submit_delay_for`]`(paste_len)` — the hold scales with
    /// the paste size. Replaces any pending submit already queued for the same
    /// panel (the latest paste wins) so a panel never accrues stale Enters.
    fn enqueue_submit(&mut self, panel: PanelId, paste_len: usize) {
        let due = Instant::now() + submit_delay_for(paste_len);
        match self.pending_submits.iter_mut().find(|s| s.panel == panel) {
            Some(s) => s.due = due,
            None => self.pending_submits.push(PendingSubmit { panel, due }),
        }
    }

    /// Flush every deferred submit whose delay has elapsed: write the held-back
    /// `\r` as a discrete keypress to its panel. Called once per event-loop
    /// tick. A submit whose panel has been killed is dropped (the panel can no
    /// longer accept input).
    pub(crate) fn poll_pending_submits(&mut self) {
        let now = Instant::now();
        let mut still_pending = Vec::with_capacity(self.pending_submits.len());
        for s in std::mem::take(&mut self.pending_submits) {
            if s.due > now {
                still_pending.push(s);
                continue;
            }
            if let Some(p) = self.panels.iter_mut().find(|p| p.id == s.panel) {
                if let Err(e) = p.write_input(b"\r") {
                    warn!(panel = %s.panel, error = %e, "deferred submit write failed");
                }
            }
        }
        self.pending_submits = still_pending;
    }

    /// Render a detected [`crate::term::Menu`] as readable text for the main
    /// worker: the question, the numbered options (the highlighted one marked),
    /// and how to answer.
    pub(crate) fn render_menu(menu: &crate::term::Menu) -> String {
        let mut out = String::from("selection menu:\n");
        if !menu.question.is_empty() {
            out.push_str(&format!("question: {}\n", menu.question));
        }
        for (i, opt) in menu.options.iter().enumerate() {
            let marker = if i == menu.cursor { "❯ " } else { "  " };
            out.push_str(&format!("{marker}{}. {}\n", opt.number, opt.label));
        }
        out.push_str("(answer with select_option(panel, <number>))");
        out
    }

    /// Bytes that move a chooser's cursor from `cursor` to `target` and submit:
    /// `|target-cursor|` arrow keys (down when target is lower, up otherwise)
    /// then Enter. Reuses [`crate::input::encode_key`] so the xterm sequences
    /// match what a real keyboard would send.
    fn menu_nav_bytes(cursor: usize, target: usize) -> Vec<u8> {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (code, count) = if target >= cursor {
            (KeyCode::Down, target - cursor)
        } else {
            (KeyCode::Up, cursor - target)
        };
        let arrow =
            crate::input::encode_key(&KeyEvent::new(code, KeyModifiers::NONE)).unwrap_or_default();
        let mut bytes = Vec::new();
        for _ in 0..count {
            bytes.extend_from_slice(&arrow);
        }
        bytes.extend_from_slice(
            &crate::input::encode_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap_or_default(),
        );
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session::runtime::test_support::*;
    use tempfile::TempDir;

    /// Bracketed + submit: the paste carries the text with NO trailing `\r`
    /// (the submit is held back, never inline with the paste), and
    /// `defer_submit` is set so `send_keys` queues the discrete Enter.
    #[test]
    fn plan_delivery_pastes_without_an_inline_submit() {
        let plan = plan_delivery(b"hello", true, true);
        assert_eq!(plan.bytes, b"\x1b[200~hello\x1b[201~");
        assert!(
            plan.defer_submit,
            "bracketed + enter must defer the submitting \\r"
        );
    }

    #[test]
    fn plan_delivery_paste_keeps_internal_newlines_inside_the_markers() {
        // A multi-line report: the internal newline stays *inside* the paste
        // (does not submit early), and still no trailing `\r` — the submit is
        // deferred, not appended.
        let plan = plan_delivery(b"line1\nline2", true, true);
        assert_eq!(plan.bytes, b"\x1b[200~line1\nline2\x1b[201~");
        assert!(plan.defer_submit);
    }

    #[test]
    fn plan_delivery_bracketed_without_enter_pastes_and_defers_nothing() {
        let plan = plan_delivery(b"hi", false, true);
        assert_eq!(plan.bytes, b"\x1b[200~hi\x1b[201~");
        assert!(!plan.defer_submit, "no enter requested → nothing to defer");
    }

    #[test]
    fn plan_delivery_unbracketed_is_raw_text_plus_inline_cr() {
        // Agent without `?2004`: markers would be literal garbage and there is
        // no paste mode to absorb the `\r`, so it rides inline and is not
        // deferred.
        let submit = plan_delivery(b"hello", true, false);
        assert_eq!(submit.bytes, b"hello\r");
        assert!(!submit.defer_submit);
        let no_submit = plan_delivery(b"hello", false, false);
        assert_eq!(no_submit.bytes, b"hello");
        assert!(!no_submit.defer_submit);
    }

    #[test]
    fn plan_delivery_empty_text_is_a_bare_inline_enter_never_deferred() {
        // A bare Enter (e.g. confirming) is just `\r`, never empty paste markers
        // and never deferred — there is no paste for it to be swallowed by.
        for bracketed in [true, false] {
            let plan = plan_delivery(b"", true, bracketed);
            assert_eq!(plan.bytes, b"\r", "bracketed={bracketed}");
            assert!(!plan.defer_submit, "bracketed={bracketed}");
        }
        let none = plan_delivery(b"", false, true);
        assert_eq!(none.bytes, b"");
        assert!(!none.defer_submit);
    }

    /// `enqueue_submit` keeps a single pending Enter per panel: a re-paste
    /// before the first flush updates the existing entry's deadline rather than
    /// stacking a second `\r` that would double-submit.
    #[tokio::test]
    async fn enqueue_submit_dedups_per_panel() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let panel = PanelId::new();

        mux.enqueue_submit(panel, 0);
        let first_due = mux.pending_submits[0].due;
        assert_eq!(mux.pending_submits.len(), 1);

        mux.enqueue_submit(panel, 0);
        assert_eq!(mux.pending_submits.len(), 1, "same panel must not stack");
        assert!(
            mux.pending_submits[0].due >= first_due,
            "re-enqueue pushes the deadline out, it does not add a second submit"
        );
    }

    /// The deferred-submit hold scales with the paste size and is clamped: a
    /// tiny paste gets the base delay, a mid paste gets base + per-KiB, and a
    /// pathologically large paste is capped (never an absurd stall).
    #[test]
    fn submit_delay_scales_with_paste_size_and_clamps() {
        assert_eq!(
            submit_delay_for(0),
            SUBMIT_DELAY_BASE,
            "an empty/tiny paste holds for the base delay only"
        );
        assert_eq!(
            submit_delay_for(64 * 1024),
            SUBMIT_DELAY_BASE + SUBMIT_DELAY_PER_KIB * 64,
            "a 64 KiB paste adds 64 KiB of per-KiB hold"
        );
        assert_eq!(
            submit_delay_for(usize::MAX),
            SUBMIT_DELAY_MAX,
            "an enormous paste is clamped to the max hold, not overflowed"
        );
        assert!(
            submit_delay_for(16 * 1024) > submit_delay_for(1024),
            "a bigger paste holds strictly longer (until the clamp)"
        );
    }

    /// `poll_pending_submits` retains a submit whose delay has not yet elapsed,
    /// and removes one that is due — here the due one's panel does not exist
    /// (killed), so it is dropped without a write.
    #[tokio::test]
    async fn poll_pending_submits_retains_not_yet_due_and_drops_due() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let due_panel = PanelId::new();
        let pending_panel = PanelId::new();
        mux.pending_submits.push(PendingSubmit {
            panel: due_panel,
            due: Instant::now() - Duration::from_millis(1),
        });
        mux.pending_submits.push(PendingSubmit {
            panel: pending_panel,
            due: Instant::now() + Duration::from_secs(60),
        });

        mux.poll_pending_submits();

        assert_eq!(
            mux.pending_submits.len(),
            1,
            "the due submit is flushed/dropped, the not-yet-due one stays"
        );
        assert_eq!(mux.pending_submits[0].panel, pending_panel);
    }

    /// `menu_nav_bytes` emits the right count + direction of arrow keys, then
    /// Enter — per navigation boundary.
    #[test]
    fn menu_nav_bytes_moves_the_cursor_and_submits() {
        // Down two then Enter (cursor 0 → option index 2).
        assert_eq!(Multiplexer::menu_nav_bytes(0, 2), b"\x1b[B\x1b[B\r");
        // Up two then Enter (cursor 2 → index 0).
        assert_eq!(Multiplexer::menu_nav_bytes(2, 0), b"\x1b[A\x1b[A\r");
        // Already on target: just Enter, no arrows.
        assert_eq!(Multiplexer::menu_nav_bytes(1, 1), b"\r");
    }

    /// `render_menu` lists the options and marks the highlighted one.
    #[test]
    fn render_menu_marks_the_cursor_option() {
        let menu = crate::term::Menu {
            question: "Pick one".to_string(),
            options: vec![
                crate::term::MenuOption {
                    number: 1,
                    label: "alpha".to_string(),
                },
                crate::term::MenuOption {
                    number: 2,
                    label: "beta".to_string(),
                },
            ],
            cursor: 1,
        };
        let text = Multiplexer::render_menu(&menu);
        assert!(text.contains("question: Pick one"));
        assert!(text.contains("❯ 2. beta"), "cursor option marked: {text:?}");
        assert!(text.contains("  1. alpha"));
        assert!(text.contains("select_option"));
    }
}
