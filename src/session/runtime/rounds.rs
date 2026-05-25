use super::*;
use crate::agent::derive_state::DerivedState;
use crate::mcp::protocol::ControlResponse;
use crate::mcp::{McpToolSurface, ReadPanelMode};
use crate::panel::lifecycle::{Panel, PanelState};
use crate::session::id::PanelId;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tracing::warn;

/// Default fallback budget for a registered round when the caller omits
/// `fallback_secs` — the safety-net deadline after which caucus delivers a
/// partial report even if some panels never settled.
const ROUND_FALLBACK_DEFAULT_SECS: u64 = 600;
/// Hard cap on a round's fallback budget.
const ROUND_FALLBACK_MAX_SECS: u64 = 3600;
/// How long caucus holds an injected round after the user's last un-submitted
/// keystroke to the main panel — the grace across a mid-compose pause, so a
/// delivery never lands in the middle of a line the user is still typing.
///
/// Longer than a reflexive "quiet" debounce on purpose: a human pausing
/// mid-sentence to think routinely exceeds a second, and the hold is only ever
/// armed by an *un-submitted* line (`main_compose_since` is cleared the instant
/// the line is sent — see [`Multiplexer::note_prompt_delivered`]). The sole
/// cost of the longer grace is delivery latency in the rare case the user types
/// a line and walks away without submitting it; correctness (never splicing a
/// round into a half-typed line) is the priority.
const COMPOSE_GRACE: Duration = Duration::from_secs(5);

/// A round caucus is watching on the main worker's behalf
/// ([`Multiplexer::poll_pending_rounds`]).
///
/// Unlike a control request (each answered immediately), a round carries no
/// reply channel — `register_round` already acked at registration. Instead
/// the event loop watches it each tick and, once every panel has settled (or
/// `fallback_deadline` passes), assembles the panels' results and *injects*
/// them into the main worker's panel as a fresh turn. This is the caucus→main
/// push that the pull-only MCP transport cannot do.
pub(super) struct PendingRound {
    /// Panel ids in the round. Ids that no longer exist count as settled
    /// (see [`Multiplexer::wait_panels_settled`]).
    panels: Vec<PanelId>,
    /// Per-panel follow-up task queue. A round panel that goes idle with a
    /// non-empty queue is fed its next task (popped front) by
    /// [`Multiplexer::feed_round_backlog`], flipping it back to `Working` — so
    /// an early finisher keeps working its backlog instead of idling until the
    /// barrier. A panel settles for the round only once it is idle *and* its
    /// queue is empty; a panel with no entry here settles on its first idle.
    backlog: HashMap<PanelId, VecDeque<String>>,
    /// How each panel's result is read for the delivered report.
    pub(super) read_mode: ReadPanelMode,
    /// Wall-clock instant past which the round is delivered regardless of
    /// state — the safety net, marking still-`working` panels unfinished.
    fallback_deadline: Instant,
}

impl Multiplexer {
    /// Register a round on `panels`: stash a [`PendingRound`] for the event
    /// loop to deliver and ack immediately with the panels' current snapshot.
    /// `fallback_secs` is clamped to `[1, ROUND_FALLBACK_MAX_SECS]`, defaulting
    /// to `ROUND_FALLBACK_DEFAULT_SECS`; `read_mode` defaults to `LastMessage`.
    ///
    /// Unlike the removed blocking wait, this never special-cases an
    /// already-settled round — delivery is decided uniformly by
    /// [`Multiplexer::poll_pending_rounds`] (which also gates on the main panel
    /// being idle), so the registration path has exactly one shape.
    ///
    /// `backlog` is an optional per-panel task queue: a panel listed here is
    /// fed its next task on each idle (keeping an early finisher busy) and
    /// settles only once its queue drains; a panel absent from `backlog`
    /// settles on its first idle, the original one-task behaviour. Entries for
    /// panels not in `panels` and empty queues are dropped at registration, so
    /// `backlog` only ever holds work that can actually be fed.
    pub(crate) fn register_round(
        &mut self,
        panels: Vec<PanelId>,
        read_mode: Option<ReadPanelMode>,
        fallback_secs: Option<u64>,
        backlog: Option<HashMap<PanelId, Vec<String>>>,
    ) -> ControlResponse {
        let budget = fallback_secs
            .unwrap_or(ROUND_FALLBACK_DEFAULT_SECS)
            .clamp(1, ROUND_FALLBACK_MAX_SECS);
        let backlog = backlog
            .unwrap_or_default()
            .into_iter()
            // Only queue work for panels actually in the round, and drop empty
            // queues so the feed/settle check never sees a vacuous entry.
            .filter(|(id, tasks)| panels.contains(id) && !tasks.is_empty())
            .map(|(id, tasks)| (id, VecDeque::from(tasks)))
            .collect();
        let ack = self.panel_snapshot(&panels);
        self.pending_rounds.push(PendingRound {
            panels,
            backlog,
            read_mode: read_mode.unwrap_or(ReadPanelMode::LastMessage),
            fallback_deadline: Instant::now() + Duration::from_secs(budget),
        });
        ack
    }

    /// Deliver one due, deliverable round to the main worker — the caucus→main
    /// push. Called once per event-loop tick, after signals + pump have
    /// updated panel state.
    ///
    /// A round is *due* when all its panels have settled, or its
    /// `fallback_deadline` has passed. It is *delivered* only while the main
    /// panel exists, is `Idle`, and has no un-submitted human keystroke within
    /// `COMPOSE_GRACE` — so an injected turn never collides with a line the
    /// user is composing. At most one round is delivered per tick: the
    /// injection flips the main panel to `Working`, so any other due round
    /// naturally holds until the main worker is idle again. A due round with
    /// no main panel to deliver to is dropped (it would otherwise be stranded).
    ///
    /// Before the due-check, each round's backlog is fed
    /// ([`Multiplexer::feed_round_backlog`]): a panel that finished early with
    /// queued tasks is handed its next task and flips back to `Working`, so it
    /// is not yet settled and the round is not yet due — the early finisher
    /// keeps working its backlog instead of idling at the barrier.
    pub fn poll_pending_rounds(&mut self) {
        if self.pending_rounds.is_empty() {
            return;
        }
        let now = Instant::now();
        // Take the queue so the settle-checks below can borrow `self`.
        let rounds = std::mem::take(&mut self.pending_rounds);

        let main_id = self.main_panel_id;
        let deliverable = self.main_deliverable();

        let mut delivered = false;
        for mut round in rounds {
            // Keep early finishers busy: hand any idle round-panel its next
            // queued task before judging the round done. A fed panel flips to
            // `Working`, so `wait_panels_settled` sees it as unfinished.
            self.feed_round_backlog(&mut round);
            let due = now >= round.fallback_deadline || self.wait_panels_settled(&round.panels);
            match main_id {
                // Due, gate open, nothing delivered yet this tick: assemble +
                // inject into the main panel, then drop the round.
                Some(main_id) if due && deliverable && !delivered => {
                    let report = self.assemble_round_report(&round.panels, round.read_mode);
                    if let Err(err) = McpToolSurface::send_keys(self, main_id, &report, true) {
                        warn!(error = %err, "round delivery to main panel failed");
                    }
                    delivered = true;
                }
                // Due but there is no main panel to deliver to: drop it.
                None if due => {}
                // Not due, gate closed, or already delivered one this tick:
                // keep it for a later tick.
                _ => self.pending_rounds.push(round),
            }
        }
    }

    /// Hand each cleanly-idle round panel its next backlog task, keeping an
    /// early finisher busy instead of idling at the barrier. Called once per
    /// round per tick from [`Multiplexer::poll_pending_rounds`], before the
    /// due-check.
    ///
    /// Only a panel in coarse `Idle` is fed — never one that is `Working` or
    /// still `Spawning`. A panel stopped on a selection menu is excluded for
    /// free: a chooser fires no `Stop` hook, so such a panel stays coarse
    /// `Working` (and [`Multiplexer::poll_round_selection_prompts`] routes it to
    /// the main worker instead). The next task is delivered with `enter`, which
    /// flips the panel back to `Working` (so it is no longer settled); an empty
    /// queue is left in place and the panel settles. The queue is popped only after
    /// the send actually succeeds, so a failed delivery leaves the task at the
    /// front to be retried next tick rather than silently dropped. Feeding is
    /// not gated by [`Multiplexer::main_deliverable`]: keeping a worker busy is
    /// independent of the main panel's state.
    fn feed_round_backlog(&mut self, round: &mut PendingRound) {
        // Decide every feed first (borrows only `round` + reads `self.panels`),
        // then deliver (mut-borrows `self`), so the two borrows never overlap.
        // The front is cloned, not popped, here — it is consumed only on a
        // confirmed send below.
        let mut feeds: Vec<(PanelId, String)> = Vec::new();
        for &id in &round.panels {
            let idle = self
                .panels
                .iter()
                .find(|p| p.id == id)
                .is_some_and(|p| p.state() == PanelState::Idle);
            if !idle {
                continue;
            }
            if let Some(task) = round.backlog.get(&id).and_then(VecDeque::front) {
                feeds.push((id, task.clone()));
            }
        }
        for (id, task) in feeds {
            match McpToolSurface::send_keys(self, id, &task, true) {
                // Delivered: consume the task (still the queue's front, single
                // tick, nothing else mutates the queue between collect + here).
                Ok(()) => {
                    round.backlog.get_mut(&id).and_then(VecDeque::pop_front);
                }
                // Delivery failed: leave the task at the front; the panel is
                // still idle, so the next tick retries it.
                Err(err) => warn!(error = %err, panel = %id, "round backlog feed failed"),
            }
        }
    }

    /// Whether a caucus→main push may land *this tick*: the main panel exists,
    /// is coarse `Idle`, and has no un-submitted human keystroke within
    /// `COMPOSE_GRACE` (so the user is not mid-compose). The single gate shared
    /// by both push paths — round completion
    /// ([`Multiplexer::poll_pending_rounds`]) and selection prompts
    /// ([`Multiplexer::poll_round_selection_prompts`]). Each push flips the main
    /// panel to `Working`, closing the gate for the rest of the tick, so at most
    /// one push of either kind lands per tick.
    fn main_deliverable(&self) -> bool {
        let main_idle = self.main_panel_id.is_some_and(|id| {
            self.panels
                .iter()
                .find(|p| p.id == id)
                .is_some_and(|p| p.state() == PanelState::Idle)
        });
        let quiet = self
            .main_compose_since
            .is_none_or(|t| Instant::now().duration_since(t) >= COMPOSE_GRACE);
        main_idle && quiet
    }

    /// Announce to the main worker when a panel in a pending round has stopped
    /// on an interactive selection menu — the caucus→main *selection* push.
    ///
    /// A chooser fires no `Stop` hook, so the panel stays coarse `Working` and
    /// its round never settles; without this the main worker would only learn
    /// at the fallback deadline. caucus pushes an interim notice so the main
    /// worker can answer it (`read_menu` / `select_option`) and let the round
    /// finish. Gated by `Multiplexer::main_deliverable` and deduped by menu
    /// content signature (`Multiplexer::notified_menus`): a panel sitting on
    /// one menu is announced once; a menu whose content changes re-announces;
    /// a panel that leaves its menu is forgotten so a future menu re-announces.
    /// At most one notice per tick (shares the deliverability gate with round
    /// completion, which a push closes by flipping the main panel to `Working`).
    pub fn poll_round_selection_prompts(&mut self) {
        let Some(main_id) = self.main_panel_id else {
            return;
        };
        if self.pending_rounds.is_empty() {
            return;
        }

        // Round panels currently showing a menu, with a content signature
        // (question + options, not the cursor row) so cursor movement alone
        // never re-announces.
        let round_panels: std::collections::HashSet<PanelId> = self
            .pending_rounds
            .iter()
            .flat_map(|r| r.panels.iter().copied())
            .collect();
        let mut open: Vec<(PanelId, u64)> = Vec::new();
        let mut menus: HashMap<PanelId, crate::term::Menu> = HashMap::new();
        for pid in round_panels {
            if pid == main_id {
                continue;
            }
            if let Some(p) = self.panels.iter().find(|p| p.id == pid)
                && let Some(menu) = Self::panel_menu(p)
            {
                open.push((pid, Self::menu_signature(&menu)));
                menus.insert(pid, menu);
            }
        }

        let (pick, open_set) = Self::pick_menu_to_notify(&open, &self.notified_menus);
        // Forget panels that have left their menu, so a future menu re-announces.
        self.notified_menus.retain(|pid, _| open_set.contains(pid));

        // One notice per tick, only while the gate is open. Dedup state above
        // is updated regardless; the panel is marked notified only on a real
        // push, so a closed gate this tick still announces next tick.
        if !self.main_deliverable() {
            return;
        }
        let Some(pid) = pick else {
            return;
        };
        // `pick` came from `open`, so both lookups are present.
        let sig = open
            .iter()
            .find(|(p, _)| *p == pid)
            .map(|(_, s)| *s)
            .unwrap();
        let menu = menus.remove(&pid).unwrap();
        let role = self
            .panels
            .iter()
            .find(|p| p.id == pid)
            .map(|p| p.role.clone())
            .unwrap_or_default();
        let notice = format!(
            "[caucus] panel {pid} (role: {role}) is waiting on a selection — \
             answer it so the round can finish.\n{}\n(answer with \
             select_option({pid}, <number>); for a free-text reply pick the \
             'type something' option, then send_keys your text.)",
            Self::render_menu(&menu)
        );
        if let Err(err) = McpToolSurface::send_keys(self, main_id, &notice, true) {
            warn!(error = %err, "selection-prompt notice to main panel failed");
        }
        self.notified_menus.insert(pid, sig);
    }

    /// Pick which round panel to announce a selection menu for this tick.
    ///
    /// Pure decision core of [`Multiplexer::poll_round_selection_prompts`]:
    /// given the panels currently showing a menu as `(panel, signature)` and
    /// the already-notified set, return the first panel whose signature is new
    /// or changed (the one to push), plus the set of panels showing a menu now
    /// (so the caller can prune stale dedup entries).
    fn pick_menu_to_notify(
        open: &[(PanelId, u64)],
        notified: &HashMap<PanelId, u64>,
    ) -> (Option<PanelId>, std::collections::HashSet<PanelId>) {
        let open_set = open.iter().map(|(p, _)| *p).collect();
        let pick = open
            .iter()
            .find(|(p, sig)| notified.get(p) != Some(sig))
            .map(|(p, _)| *p);
        (pick, open_set)
    }

    /// Content signature of a selection menu — a hash of the question and the
    /// numbered option labels, **excluding** the cursor row. Two reads of the
    /// same chooser hash equal even as the highlighted row moves; a changed
    /// question or option set hashes differently. Used to dedup the
    /// selection-prompt push ([`Multiplexer::notified_menus`]).
    fn menu_signature(menu: &crate::term::Menu) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        menu.question.hash(&mut h);
        for opt in &menu.options {
            opt.number.hash(&mut h);
            opt.label.hash(&mut h);
        }
        h.finish()
    }

    /// Assemble a round's delivery message: a self-describing block per panel
    /// — role + current state, plus its result read via `read_mode` (settled
    /// panels) or an "unfinished" marker (a panel still `working` when the
    /// fallback deadline forced delivery). A panel id that no longer exists is
    /// reported as gone. This is the text injected into the main worker's
    /// panel as a fresh turn.
    fn assemble_round_report(&self, panels: &[PanelId], read_mode: ReadPanelMode) -> String {
        let all_settled = self.wait_panels_settled(panels);
        let mut out = format!(
            "[caucus] Round complete — {} panel(s){}.\n",
            panels.len(),
            if all_settled {
                ""
            } else {
                " (fallback deadline reached; some panels did not finish)"
            }
        );
        for &id in panels {
            let Some(panel) = self.panels.iter().find(|p| p.id == id) else {
                out.push_str(&format!("\n## panel {id} — gone (killed)\n"));
                continue;
            };
            let state = panel.state();
            out.push_str(&format!(
                "\n## panel {id} (role: {}) — {}\n",
                panel.role,
                state.label()
            ));
            if matches!(state, PanelState::Working | PanelState::Spawning) {
                out.push_str("⏳ still working — did not finish within the fallback window.\n");
                continue;
            }
            let body = self
                .read_panel(id, read_mode)
                .unwrap_or_else(|e| format!("(could not read panel: {e})"));
            let body = body.trim();
            out.push_str(if body.is_empty() {
                "(no output captured)\n"
            } else {
                body
            });
            if !body.is_empty() {
                out.push('\n');
            }
        }
        out
    }

    /// Render a panel's visible grid viewport as text, one row per line.
    pub(crate) fn screen_text(panel: &Panel) -> String {
        let (_, rows) = panel.grid().size();
        let mut out = String::new();
        for row in 0..rows {
            out.push_str(panel.grid().row_text(row).trim_end());
            out.push('\n');
        }
        // Drop trailing blank lines so the main worker is not handed a wall of spaces.
        while out.ends_with("\n\n") {
            out.pop();
        }
        out
    }

    /// Scan a panel's visible grid for an interactive selection menu
    /// ([`crate::term::scan_menu`]). `None` unless one is confidently detected.
    pub(crate) fn panel_menu(panel: &Panel) -> Option<crate::term::Menu> {
        let (_, rows) = panel.grid().size();
        let lines: Vec<String> = (0..rows)
            .map(|r| panel.grid().row_text(r).trim_end().to_string())
            .collect();
        crate::term::scan_menu(&lines)
    }

    /// Overlay a live selection-menu detection onto the turn-signal-derived
    /// state. A visible menu means the agent stopped mid-turn for a choice —
    /// which the `Stop`-hook state cannot see — so it outranks the
    /// signal-derived `Working`/`Idle` (mirroring `derive_agent_state`, where
    /// a grid hint is weighed before the turn signal). It never masks a
    /// stronger state (`Exited`/`Blocked*`/`Interrupted`/`Degraded`).
    pub(crate) fn overlay_menu_state(base: DerivedState, has_menu: bool) -> DerivedState {
        if has_menu && matches!(base, DerivedState::Working | DerivedState::Idle) {
            DerivedState::AwaitingSelection
        } else {
            base
        }
    }

    /// Render a panel's scrollback ring as text, oldest row first.
    pub(crate) fn scrollback_text(panel: &Panel) -> String {
        let mut out = String::new();
        for row in panel.grid().scrollback() {
            let line: String = row.iter().filter(|c| c.ch != '\0').map(|c| c.ch).collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        // Include the live viewport so `scrollback` is the complete retained
        // buffer (history + current screen), not just the off-screen rows.
        out.push_str(&Self::screen_text(panel));
        out
    }

    /// Render a raw PTY byte capture — a whole turn, escape sequences and all —
    /// into readable text by replaying it through a throwaway grid. Without
    /// this, `read_panel(since_last_turn)` would hand the main worker an
    /// escape-sequence soup instead of the turn's rendered output.
    pub(crate) fn rendered_capture_text(bytes: &[u8], cols: usize) -> String {
        let mut grid = crate::term::Grid::new(cols.max(20), 50);
        grid.advance(bytes);
        let mut out = String::new();
        for row in grid.scrollback() {
            let line: String = row.iter().filter(|c| c.ch != '\0').map(|c| c.ch).collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        let (_, rows) = grid.size();
        for r in 0..rows {
            out.push_str(grid.row_text(r).trim_end());
            out.push('\n');
        }
        while out.ends_with("\n\n") {
            out.pop();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::runtime::test_support::*;
    use crate::signal::TurnSignal;
    use tempfile::TempDir;

    /// `register_round` acks immediately with a panel snapshot and stashes a
    /// `PendingRound` — it never blocks. An unknown id is omitted from the ack
    /// (it would not appear in `list_panels` either). `read_mode` defaults to
    /// `last_message`.
    #[tokio::test]
    async fn register_round_acks_and_stashes_a_pending_round() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let ghost = PanelId::new();

        let ack = mux.register_round(vec![ghost], None, Some(60), None);
        match ack {
            ControlResponse::Panels { panels } => assert!(panels.is_empty()),
            other => panic!("expected an immediate Panels ack, got {other:?}"),
        }
        assert_eq!(mux.pending_rounds.len(), 1, "round must be stashed");
        assert_eq!(mux.pending_rounds[0].read_mode, ReadPanelMode::LastMessage);
    }

    /// `register_round` stashes a backlog only for panels actually in the round
    /// and drops empty queues, so the feed/settle check never sees a vacuous
    /// entry: a queue for a stray (non-round) panel and an empty queue are both
    /// discarded at registration.
    #[tokio::test]
    async fn register_round_keeps_backlog_only_for_round_panels_and_drops_empty() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let keep = PanelId::new();
        let empty = PanelId::new();
        let stray = PanelId::new();

        mux.register_round(
            vec![keep, empty],
            None,
            Some(600),
            Some(HashMap::from([
                (keep, vec!["t1".to_string(), "t2".to_string()]),
                (empty, vec![]),                // in round but no work → dropped
                (stray, vec!["x".to_string()]), // has work but not in round → dropped
            ])),
        );

        let backlog = &mux.pending_rounds[0].backlog;
        assert_eq!(
            backlog.len(),
            1,
            "only the in-round non-empty queue survives"
        );
        assert_eq!(
            backlog
                .get(&keep)
                .map(|q| q.iter().cloned().collect::<Vec<_>>()),
            Some(vec!["t1".to_string(), "t2".to_string()]),
        );
        assert!(!backlog.contains_key(&empty), "empty queue must be dropped");
        assert!(!backlog.contains_key(&stray), "stray panel must be dropped");
    }

    /// `assemble_round_report` reports an id that no longer exists as gone
    /// rather than panicking or omitting it silently.
    #[tokio::test]
    async fn assemble_round_report_marks_a_missing_panel_gone() {
        let tmp = TempDir::new().unwrap();
        let mux = mux(&tmp);
        let ghost = PanelId::new();

        let report = mux.assemble_round_report(&[ghost], ReadPanelMode::LastMessage);
        assert!(report.contains("Round complete"), "report: {report}");
        assert!(
            report.contains("gone"),
            "a missing panel must be marked gone: {report}"
        );
    }

    /// `assemble_round_report` marks a panel still `Working` (the fallback
    /// case) as unfinished rather than reading a half-done turn.
    ///
    /// Spawning a panel needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn assemble_round_report_marks_a_working_panel_unfinished() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let Ok(sub) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.note_prompt_delivered(sub);
        assert_eq!(
            mux.panels().iter().find(|p| p.id == sub).unwrap().state(),
            PanelState::Working,
        );

        let report = mux.assemble_round_report(&[sub], ReadPanelMode::LastMessage);
        assert!(
            report.contains("still working"),
            "a Working panel must be marked unfinished: {report}"
        );

        mux.shutdown();
    }

    /// A due round with no main panel to deliver to is dropped — it would
    /// otherwise be stranded forever. (A non-existent id counts as settled, so
    /// the round is due immediately.)
    #[tokio::test]
    async fn poll_pending_rounds_drops_a_due_round_with_no_main() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        assert!(mux.main_panel_id.is_none());

        mux.register_round(vec![PanelId::new()], None, Some(600), None);
        assert_eq!(mux.pending_rounds.len(), 1);

        mux.poll_pending_rounds();
        assert!(
            mux.pending_rounds.is_empty(),
            "a due round with no main panel must be dropped"
        );
    }

    /// Killing a panel keeps `main_panel_id` an accurate invariant — it points
    /// to a live panel or is None. Boundary: killing a non-main panel leaves it
    /// intact; killing main clears it, so a due round then *drops* (reaching the
    /// no-main drop arm) instead of re-queuing forever — the leak this guards.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn kill_panel_clears_main_panel_id_only_for_main() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let Ok(main) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        let other = mux.spawn_panel("reviewer", None, None, None).unwrap();
        mux.main_panel_id = Some(main);

        // Killing a non-main panel must not disturb main_panel_id.
        mux.kill_panel(other).unwrap();
        assert_eq!(mux.main_panel_id, Some(main));

        // A round on a non-existent id is due immediately (a missing id counts
        // as settled). Killing main clears the id, so the next poll drops it.
        mux.register_round(vec![PanelId::new()], None, Some(600), None);
        mux.kill_panel(main).unwrap();
        assert!(
            mux.main_panel_id.is_none(),
            "killing main must clear main_panel_id"
        );

        mux.poll_pending_rounds();
        assert!(
            mux.pending_rounds.is_empty(),
            "a due round must drop once main is gone, not re-queue forever"
        );

        mux.shutdown();
    }

    /// A due round is *held*, not delivered, while the main panel is not idle.
    /// Here `main_panel_id` points at an id with no live panel, so the idle
    /// gate is closed and the round stays pending for a later tick.
    #[tokio::test]
    async fn poll_pending_rounds_holds_when_main_not_idle() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        mux.main_panel_id = Some(PanelId::new());

        mux.register_round(vec![PanelId::new()], None, Some(600), None);
        mux.poll_pending_rounds();
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "round must be held while the main panel is not idle"
        );
    }

    /// The caucus→main push end to end: a round on a `Working` sub-panel is
    /// held until the panel settles, then delivered to the idle main panel —
    /// proven by the main panel flipping to `Working` (the injection opens a
    /// turn) and the round being dropped. A fresh human keystroke also holds
    /// delivery (the quiet window).
    ///
    /// Spawning panels needs a real agent CLI; the test is skipped (not
    /// failed) when none is on PATH, matching `tests/mcp_integration.rs`.
    #[tokio::test]
    async fn poll_pending_rounds_delivers_to_idle_main_on_settle() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let session_id = mux.session.id;

        // Spawn a main panel and drive it to Idle (Spawning -> Working -> Idle).
        let Ok(main) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.main_panel_id = Some(main);
        mux.note_prompt_delivered(main);
        mux.handle_signal(TurnSignal::now(
            session_id,
            main,
            crate::signal::TurnKind::Stop,
            None,
            serde_json::Value::Null,
        ));
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Idle,
        );

        // A sub-panel in `Working`, with a round registered on it.
        let Ok(sub) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.note_prompt_delivered(sub);
        mux.register_round(vec![sub], None, Some(600), None);

        // Sub still working -> round held.
        mux.poll_pending_rounds();
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "round held while the sub-panel is working"
        );

        // Settle the sub-panel (Working -> Idle).
        mux.handle_signal(TurnSignal::now(
            session_id,
            sub,
            crate::signal::TurnKind::Stop,
            None,
            serde_json::Value::Null,
        ));

        // A fresh un-submitted keystroke to main arms the compose hold: still held.
        mux.main_compose_since = Some(Instant::now());
        mux.poll_pending_rounds();
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "round held while the user is mid-compose (compose grace)"
        );

        // Clear the compose hold: now the round delivers.
        mux.main_compose_since = None;
        mux.poll_pending_rounds();
        assert!(
            mux.pending_rounds.is_empty(),
            "round delivered once due + main idle + quiet"
        );
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "delivery injects a turn into the main panel",
        );

        mux.shutdown();
    }

    /// A round panel with a backlog is fed its next task on idle (flipping it
    /// back to `Working`, so the round is *not* due), and settles — letting the
    /// round deliver to main — only once its queue drains. The end-to-end
    /// early-finisher loop: keep working the backlog instead of idling at the
    /// barrier.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn poll_pending_rounds_feeds_backlog_then_settles_when_drained() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let session_id = mux.session.id;

        let Some(main) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.main_panel_id = Some(main);
        let Some(sub) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };

        // One queued task for the sub; generous fallback so only the backlog,
        // not the deadline, drives the round.
        mux.register_round(
            vec![sub],
            None,
            Some(3600),
            Some(HashMap::from([(sub, vec!["only-task".to_string()])])),
        );

        // Sub is idle with a task queued: the poll feeds it (→ Working), so the
        // round is not yet due and stays pending; the queue is now empty.
        mux.poll_pending_rounds();
        assert_eq!(
            mux.panels().iter().find(|p| p.id == sub).unwrap().state(),
            PanelState::Working,
            "an idle sub with a queued task must be fed (flips to Working)",
        );
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "round held while the sub works"
        );
        assert!(
            mux.pending_rounds[0]
                .backlog
                .get(&sub)
                .is_none_or(VecDeque::is_empty),
            "the fed task must be consumed from the queue",
        );

        // Sub finishes its backlog task → idle with an empty queue → settles,
        // so the round becomes due and delivers to the idle main panel.
        mux.handle_signal(TurnSignal::now(
            session_id,
            sub,
            crate::signal::TurnKind::Stop,
            None,
            serde_json::Value::Null,
        ));
        mux.poll_pending_rounds();
        assert!(
            mux.pending_rounds.is_empty(),
            "with the queue drained the round must deliver, not re-feed",
        );
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "delivery injects the round report into the main panel",
        );

        mux.shutdown();
    }

    /// Drive a freshly spawned panel to `Idle` (Spawning → Working → Idle) by
    /// marking its prompt delivered then handing it a `Stop` turn signal — the
    /// settle path the stranded-main tests share. Returns the panel id.
    fn spawn_idle(mux: &mut Multiplexer, role: &str) -> Option<PanelId> {
        let id = mux.spawn_panel(role, None, None, None).ok()?;
        let session_id = mux.session.id;
        mux.note_prompt_delivered(id);
        mux.handle_signal(TurnSignal::now(
            session_id,
            id,
            crate::signal::TurnKind::Stop,
            None,
            serde_json::Value::Null,
        ));
        assert_eq!(
            mux.panels().iter().find(|p| p.id == id).unwrap().state(),
            PanelState::Idle,
        );
        Some(id)
    }

    /// A live selection menu overlays `AwaitingSelection` onto an otherwise
    /// signal-derived state, but never masks a stronger state.
    #[test]
    fn overlay_menu_state_only_overrides_working_and_idle() {
        use DerivedState::*;
        // Mid-turn (Working) or back-at-prompt (Idle) + menu → AwaitingSelection.
        assert_eq!(
            Multiplexer::overlay_menu_state(Working, true),
            AwaitingSelection
        );
        assert_eq!(
            Multiplexer::overlay_menu_state(Idle, true),
            AwaitingSelection
        );
        // No menu detected → unchanged.
        assert_eq!(Multiplexer::overlay_menu_state(Working, false), Working);
        // Stronger states are never masked by a stray on-screen menu.
        assert_eq!(Multiplexer::overlay_menu_state(Exited, true), Exited);
        assert_eq!(
            Multiplexer::overlay_menu_state(BlockedMergeConflict, true),
            BlockedMergeConflict
        );
        assert_eq!(
            Multiplexer::overlay_menu_state(InterruptedTransport, true),
            InterruptedTransport
        );
    }

    /// `menu_signature` tracks menu *content* — question + option labels — and
    /// ignores the cursor row, so navigation alone never re-announces.
    #[test]
    fn menu_signature_ignores_cursor_tracks_content() {
        let base = menu_of("Pick one", ["alpha", "beta"], 0);
        // Same content, cursor moved → same signature.
        let moved = menu_of("Pick one", ["alpha", "beta"], 1);
        assert_eq!(
            Multiplexer::menu_signature(&base),
            Multiplexer::menu_signature(&moved),
            "cursor movement must not change the signature"
        );
        // Changed option label → different signature.
        let relabelled = menu_of("Pick one", ["alpha", "gamma"], 0);
        assert_ne!(
            Multiplexer::menu_signature(&base),
            Multiplexer::menu_signature(&relabelled),
            "a changed option must change the signature"
        );
        // Changed question → different signature.
        let requestioned = menu_of("Pick another", ["alpha", "beta"], 0);
        assert_ne!(
            Multiplexer::menu_signature(&base),
            Multiplexer::menu_signature(&requestioned),
            "a changed question must change the signature"
        );
    }

    /// `pick_menu_to_notify` announces a panel's menu once, re-announces on a
    /// content change, stays silent while unchanged, and reports the open set
    /// so the caller can prune panels that have left their menus.
    #[test]
    fn pick_menu_to_notify_announces_new_and_dedups() {
        let pid = PanelId::new();
        let sig_a = 11u64;
        let sig_b = 22u64;

        // Nothing on screen → nothing to announce, empty open set.
        let (pick, open) = Multiplexer::pick_menu_to_notify(&[], &HashMap::new());
        assert_eq!(pick, None);
        assert!(open.is_empty());

        // A menu not yet notified → announce it; open set carries the panel.
        let (pick, open) = Multiplexer::pick_menu_to_notify(&[(pid, sig_a)], &HashMap::new());
        assert_eq!(pick, Some(pid));
        assert!(open.contains(&pid));

        // Same menu already notified → silent.
        let notified = HashMap::from([(pid, sig_a)]);
        let (pick, open) = Multiplexer::pick_menu_to_notify(&[(pid, sig_a)], &notified);
        assert_eq!(pick, None);
        assert!(open.contains(&pid));

        // Menu content changed under the same panel → re-announce.
        let (pick, _) = Multiplexer::pick_menu_to_notify(&[(pid, sig_b)], &notified);
        assert_eq!(pick, Some(pid));

        // Panel left its menu (not in `open`) → not in the open set, so the
        // caller's retain drops its dedup entry.
        let (pick, open) = Multiplexer::pick_menu_to_notify(&[], &notified);
        assert_eq!(pick, None);
        assert!(!open.contains(&pid));
    }

    #[test]
    fn rendered_capture_strips_escape_sequences() {
        // A raw turn capture: SGR colour, CR/LF, cursor moves — what a real
        // agent emits. `read_panel(since_last_turn)` must hand the main
        // worker readable text, never this escape soup.
        let raw = b"\x1b[1;32mhello\x1b[0m\r\nfrom \x1b[31mcaucus\x1b[0m\x1b[K\r\n";
        let text = Multiplexer::rendered_capture_text(raw, 80);
        assert!(
            !text.contains('\x1b'),
            "escape sequences must be rendered away: {text:?}"
        );
        assert!(text.contains("hello"), "got: {text:?}");
        assert!(text.contains("from caucus"), "got: {text:?}");
    }
}
