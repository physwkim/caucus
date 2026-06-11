use super::*;
use crate::agent::derive_state::DerivedState;
use crate::mcp::protocol::{ControlResponse, SelectionPolicy};
use crate::mcp::{McpToolSurface, ReadPanelMode};
use crate::panel::lifecycle::{Panel, PanelState};
use crate::session::id::{PanelId, RoundId};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tracing::{info, warn};

// The round fallback default + hard cap live in `config::settings` (the default
// is the `round_fallback_secs` tunable; the cap bounds it and per-call
// overrides). Imported here so register_round reads the single source of truth.
use crate::config::settings::ROUND_FALLBACK_MAX_SECS;

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

/// Minimum gap between stranded-main nudges ([`Multiplexer::poll_stranded_main`]).
/// A main worker that keeps going idle without registering a round is prodded
/// at most this often, so the safety net never floods its context every tick.
const STRANDED_NUDGE_COOLDOWN: Duration = Duration::from_secs(60);

/// Per-captured-turn byte budget in the **on-disk** round report
/// ([`Multiplexer::assemble_round_report`]). A turn body over this is head/tail
/// truncated in the report and its full text spilled to
/// `<session_root>/round-spills/`. Without it a `scrollback` read (up to 10k
/// rows × the panel width per panel) concatenated across a multi-panel round
/// would inflate even the spilled report file without bound. (The report is no
/// longer injected into the main PTY — finding 24: the main worker is handed
/// the teaser-bounded [`Multiplexer::render_round_summary`] that points at the
/// report file. This bound keeps the file itself readable.) Split evenly
/// between the head and tail kept around the elision.
const MAX_ROUND_BODY_BYTES: usize = 16 * 1024;

/// Per-panel teaser budget in the *injected* round-delivery summary
/// ([`Multiplexer::render_round_summary`]). The full assembled report is
/// spilled to `<session_root>/rounds/<round_id>.md` and the main worker is
/// handed only a compact summary that points at it — so the bracketed paste
/// caucus injects into the main PTY stays small (per-panel teaser, not the
/// whole report). Each panel contributes at most this many bytes of its latest
/// output as a teaser; the rest is in the report file.
const ROUND_SUMMARY_TEASER_BYTES: usize = 1024;

/// Largest char-boundary byte index `<= idx` in `s` (stable stand-in for the
/// unstable `str::floor_char_boundary`). Truncating a `&str` at this index
/// never splits a UTF-8 scalar.
fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Smallest char-boundary byte index `>= idx` in `s` (stable stand-in for the
/// unstable `str::ceil_char_boundary`). Slicing `&s[idx..]` from this index
/// never splits a UTF-8 scalar.
fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx.min(s.len())
}

/// Trim `body` to at most `budget` bytes for the injected round summary,
/// landing on a UTF-8 char boundary and marking how much was elided. The full
/// text lives in the spilled report file, so the teaser only needs to be
/// enough for the main worker to judge whether to open it.
fn summary_teaser(body: &str, budget: usize) -> String {
    if body.len() <= budget {
        return body.to_string();
    }
    let end = floor_char_boundary(body, budget);
    format!(
        "{}\n…[{} more byte(s) in the report file]…",
        &body[..end],
        body.len() - end
    )
}

/// The shared opening line of both the full round report and the injected
/// summary — panel count plus a fallback note when the deadline forced
/// delivery before every panel settled.
fn round_header(panel_count: usize, all_settled: bool) -> String {
    format!(
        "[caucus] Round complete — {panel_count} panel(s){}.\n",
        if all_settled {
            ""
        } else {
            " (fallback deadline reached; some panels did not finish)"
        }
    )
}

/// Append the "caucus auto-answered" audit block shared by the full report and
/// the injected summary: one line per selection menu caucus resolved for this
/// round under the main worker's pre-authorized [`SelectionPolicy`], so the
/// delivered message shows which direction forks were taken on its behalf —
/// without the main worker ever having been interrupted. Nothing is emitted
/// when the round auto-answered no menu.
fn push_auto_answers(out: &mut String, round: &PendingRound) {
    if round.auto_answers.is_empty() {
        return;
    }
    out.push_str(&format!(
        "caucus auto-answered {} selection menu(s) per your hints (you were not interrupted):\n",
        round.auto_answers.len()
    ));
    for a in &round.auto_answers {
        out.push_str(&format!(
            "  panel {} (role: {}) → option {} \"{}\"\n",
            a.panel, a.role, a.number, a.label
        ));
    }
}

/// Append the per-panel trailing markers shared by the full report and the
/// summary: a "still working" note for a panel the fallback deadline caught
/// mid-turn, and a count of backlog tasks that never ran.
fn push_round_panel_footer(out: &mut String, c: &RoundPanelContribution) {
    if matches!(c.state, PanelState::Working | PanelState::Spawning) {
        out.push_str("⏳ still working — did not finish within the fallback window.\n");
    }
    if c.pending_backlog > 0 {
        out.push_str(&format!(
            "⏳ {} queued backlog task(s) were not run before the fallback window closed.\n",
            c.pending_backlog
        ));
    }
}

/// What a round panel is visibly waiting on, detected from its rendered grid
/// ([`Multiplexer::panel_blocked_prompt`]) — a mid-turn attention state the
/// coarse panel state cannot express, because none of these fire a `Stop` hook.
/// The panel therefore stays coarse `Working` and its round never settles; the
/// per-tick round watch ([`Multiplexer::poll_round_blocked_panels`]) pushes a
/// deduped interim notice for each so the main worker can act before the
/// fallback deadline rather than the round silently stalling.
#[derive(Debug, Clone)]
pub(crate) enum BlockedPrompt {
    /// A numbered selection menu — answered with `select_option`.
    Selection(crate::term::Menu),
    /// A raw `[y/n]`-style yes/no prompt from a tool/shell the agent ran —
    /// answered with `send_keys`. Carries the prompt line for the notice.
    Permission(String),
}

impl BlockedPrompt {
    /// The [`DerivedState`] this visible prompt corresponds to.
    pub(crate) fn derived_state(&self) -> DerivedState {
        match self {
            BlockedPrompt::Selection(_) => DerivedState::AwaitingSelection,
            BlockedPrompt::Permission(_) => DerivedState::BlockedPermissionPrompt,
        }
    }

    /// Content signature for dedup ([`Multiplexer::notified_blockers`]): a
    /// menu's question + numbered option labels (cursor row **excluded**, so a
    /// moving highlight never re-announces), or a yes/no prompt's text. A
    /// variant tag is folded in so the two kinds can never collide on a hash.
    pub(crate) fn signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        match self {
            BlockedPrompt::Selection(menu) => {
                0u8.hash(&mut h);
                menu.question.hash(&mut h);
                for opt in &menu.options {
                    opt.number.hash(&mut h);
                    opt.label.hash(&mut h);
                }
            }
            BlockedPrompt::Permission(line) => {
                1u8.hash(&mut h);
                line.hash(&mut h);
            }
        }
        h.finish()
    }
}

/// One queued auto-answer for a round panel's selection menu: the panel, the
/// menu signature that authorized it (the dedup key in
/// [`Multiplexer::auto_answered`]), and the chosen option's displayed number +
/// label for the audit log.
struct AutoAnswer {
    pid: PanelId,
    sig: u64,
    number: usize,
    label: String,
}

/// One selection menu caucus auto-answered for a round, recorded on its
/// [`PendingRound`] so the delivered report/summary names the direction forks
/// caucus resolved under the main worker's pre-authorized hints — the
/// transparency complement to never interrupting the main worker.
struct AutoAnswerRecord {
    panel: PanelId,
    role: String,
    number: usize,
    label: String,
}

/// Resolve a selection `menu` against a round's [`SelectionPolicy`], returning
/// the **displayed number** of the single option to auto-select, or `None` to
/// escalate the choice to the main worker.
///
/// An option *qualifies* when its label contains at least one `prefer` keyword
/// (an empty `prefer` list passes every option) and none of the `avoid`
/// keywords. caucus acts only on a **unique** qualifier: zero or several
/// qualifying options return `None`, so the existing notice path hands the
/// choice to the main worker — the narrowed "never auto-answer what main did
/// not pre-authorize, and only when the hints single out one option".
fn resolve_selection(menu: &crate::term::Menu, policy: &SelectionPolicy) -> Option<usize> {
    let mut qualifying = menu.options.iter().filter(|opt| {
        let prefer_ok = policy.prefer.is_empty()
            || policy
                .prefer
                .iter()
                .any(|kw| label_contains_kw(&opt.label, kw));
        let vetoed = policy
            .avoid
            .iter()
            .any(|kw| label_contains_kw(&opt.label, kw));
        prefer_ok && !vetoed
    });
    let first = qualifying.next()?;
    // A second qualifier means the hints did not single out one option.
    if qualifying.next().is_some() {
        return None;
    }
    Some(first.number)
}

/// ASCII-case-insensitive substring test of `keyword` within `label`. Non-ASCII
/// bytes (e.g. a Korean label) compare exactly, since `to_ascii_lowercase`
/// touches only ASCII — keyword matching is case-insensitive for English labels
/// and exact for the rest. An empty `keyword` never matches. Allocating here is
/// fine: it runs only when a menu is actually detected on a round panel that
/// carries hints.
fn label_contains_kw(label: &str, keyword: &str) -> bool {
    if keyword.is_empty() {
        return false;
    }
    label
        .to_ascii_lowercase()
        .contains(&keyword.to_ascii_lowercase())
}

/// A round caucus is watching on the main worker's behalf
/// ([`Multiplexer::poll_pending_rounds`]).
///
/// Unlike a control request (each answered immediately), a round carries no
/// reply channel — `register_round` already acked at registration. Instead
/// the event loop watches it each tick and, once every panel has settled (or
/// `fallback_deadline` passes), assembles the panels' results, spills the full
/// report to `<session_root>/rounds/<id>.md`, and *injects* a compact summary
/// pointing at that file into the main worker's panel as a fresh turn. This is
/// the caucus→main push that the pull-only MCP transport cannot do.
pub(super) struct PendingRound {
    /// Stable identity handed back at registration ([`ControlResponse::RoundRegistered`]),
    /// the handle the main worker uses to poll ([`Multiplexer::round_status`]) or
    /// cancel ([`Multiplexer::cancel_round`]) this round. Live-only: not persisted,
    /// since a round never survives a restart (the sub-agent processes start
    /// fresh — see [`Multiplexer::ingest_resumed_rounds`]).
    id: RoundId,
    /// Panel ids in the round. Ids that no longer exist count as settled
    /// (see [`Multiplexer::round_settled`]).
    panels: Vec<PanelId>,
    /// Per-panel follow-up task queue. A round panel that goes idle with a
    /// non-empty queue is fed its next task (popped front) by
    /// [`Multiplexer::feed_round_backlog`], flipping it back to `Working` — so
    /// an early finisher keeps working its backlog instead of idling until the
    /// barrier. A panel settles for the round only once it is idle *and* its
    /// queue is empty; a panel with no entry here settles on its first idle.
    backlog: HashMap<PanelId, VecDeque<String>>,
    /// Per-panel outputs of each *finished* turn, captured in `read_mode` by
    /// [`Multiplexer::feed_round_backlog`] the moment the panel goes idle and is
    /// about to be fed its next backlog task — i.e. the output of the turn that
    /// just ended (the pre-backlog work, then each prior backlog task). Captured
    /// while the panel is still idle so a `since_last_turn` read covers the
    /// finished turn before the next task reopens it. Without this the delivered
    /// report would show only the panel's *final* turn, hiding the earlier tasks
    /// of a multi-task backlog; [`Multiplexer::assemble_round_report`] emits
    /// these ahead of the final live read. Empty for a panel that ran a single
    /// task (no backlog feed ever happened).
    captured: HashMap<PanelId, Vec<String>>,
    /// How each panel's result is read for the delivered report.
    pub(super) read_mode: ReadPanelMode,
    /// Wall-clock instant past which the round is delivered regardless of
    /// state — the safety net, marking still-`working` panels unfinished.
    fallback_deadline: Instant,
    /// Optional main-supplied keyword hints for auto-answering this round's
    /// selection menus ([`SelectionPolicy`]). When a round panel stops on a
    /// chooser, [`Multiplexer::poll_round_blocked_panels`] resolves it against
    /// these; a unique keyword match is auto-selected (and noted in the
    /// delivered report), anything else is escalated to the main worker as
    /// before. `None` keeps every menu an escalation.
    selection_hints: Option<SelectionPolicy>,
    /// Selection menus caucus auto-answered for this round under
    /// `selection_hints`, in answer order. Rendered into the delivered
    /// report/summary ([`push_auto_answers`]) so the main worker sees which
    /// forks were resolved on its behalf, without ever being interrupted.
    auto_answers: Vec<AutoAnswerRecord>,
}

/// One round panel's contribution, collected once and rendered into both the
/// full report ([`Multiplexer::assemble_round_report`]) and the injected
/// summary ([`Multiplexer::render_round_summary`]) — so the two always show the
/// *same* captured text (the per-panel `read_panel` happens exactly once, in
/// [`Multiplexer::round_panel_contribution`]).
struct RoundPanelContribution {
    /// The panel's role label.
    role: String,
    /// The panel's coarse state at delivery time.
    state: PanelState,
    /// Every finished-turn body in order: the captured backlog turns, then the
    /// final live read for a settled panel (a still-working panel contributes
    /// only its captured turns — never a live read of a mid-turn panel).
    bodies: Vec<String>,
    /// Queued backlog tasks not yet run (non-zero only on a fallback delivery).
    pending_backlog: usize,
}

impl Multiplexer {
    /// Register a round on `panels`: stash a [`PendingRound`] for the event
    /// loop to deliver and ack immediately with the panels' current snapshot.
    /// `fallback_secs` is clamped to `[1, ROUND_FALLBACK_MAX_SECS]`, defaulting
    /// to the `[settings]` `round_fallback_secs` tunable
    /// (`self.config.settings`); `read_mode` defaults to `LastMessage`.
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
        selection_hints: Option<SelectionPolicy>,
    ) -> ControlResponse {
        let budget = fallback_secs
            .unwrap_or(self.config.settings.round_fallback_secs)
            .clamp(1, ROUND_FALLBACK_MAX_SECS);
        let backlog = backlog
            .unwrap_or_default()
            .into_iter()
            // Only queue work for panels actually in the round, and drop empty
            // queues so the feed/settle check never sees a vacuous entry.
            .filter(|(id, tasks)| panels.contains(id) && !tasks.is_empty())
            .map(|(id, tasks)| (id, VecDeque::from(tasks)))
            .collect();
        let round_id = RoundId::new();
        // Snapshot the panels for the ack before moving `panels` into the round.
        let panels_snapshot = self.panel_summaries(&panels);
        self.pending_rounds.push(PendingRound {
            id: round_id,
            panels,
            backlog,
            captured: HashMap::new(),
            read_mode: read_mode.unwrap_or(ReadPanelMode::LastMessage),
            fallback_deadline: Instant::now() + Duration::from_secs(budget),
            selection_hints,
            auto_answers: Vec::new(),
        });
        // Durably shadow the new round so a quit/crash before delivery surfaces
        // it on resume instead of losing it silently.
        self.persist_pending_rounds();
        ControlResponse::RoundRegistered {
            round_id,
            panels: panels_snapshot,
        }
    }

    /// Report the live status of a registered round by id: per-panel state
    /// (working / draining backlog / settled / gone), remaining backlog count,
    /// and seconds left on the fallback deadline. An unknown id is an error —
    /// the round never existed, already delivered, or was cancelled/dropped.
    ///
    /// Round ids are live-only (not persisted across a restart), so a status
    /// poll only ever resolves rounds the current caucus instance is watching.
    pub(crate) fn round_status(&self, round_id: RoundId) -> ControlResponse {
        let Some(round) = self.pending_rounds.iter().find(|r| r.id == round_id) else {
            return ControlResponse::error(format!(
                "no live round {round_id}: it never existed, already delivered, or was cancelled"
            ));
        };
        let remaining = round
            .fallback_deadline
            .saturating_duration_since(Instant::now())
            .as_secs();
        let mut out = format!(
            "Round {round_id}: {} panel(s), {remaining}s until the fallback deadline\n",
            round.panels.len()
        );
        for &id in &round.panels {
            let backlog = round.backlog.get(&id).map(VecDeque::len).unwrap_or(0);
            let (role, status) = match self.panels.iter().find(|p| p.id == id) {
                None => ("(gone)", "gone".to_string()),
                Some(p) => {
                    let idle = !matches!(p.state(), PanelState::Working | PanelState::Spawning);
                    let status = if idle && backlog == 0 {
                        "settled".to_string()
                    } else if idle {
                        "draining backlog".to_string()
                    } else {
                        "working".to_string()
                    };
                    (p.role.as_str(), status)
                }
            };
            out.push_str(&format!(
                "  - {role} ({id}): {status}, {backlog} backlog task(s) remaining\n"
            ));
        }
        ControlResponse::Panel { text: out }
    }

    /// Cancel a live registered round by id: stop watching it and drop its
    /// pending caucus→main delivery. The panels are left exactly where they
    /// are — work already in flight keeps running and any backlog stops being
    /// fed; only the barrier that would inject the assembled report into the
    /// main worker is removed. An unknown id is an error. The durable snapshot
    /// is re-written so the cancellation survives a crash.
    pub(crate) fn cancel_round(&mut self, round_id: RoundId) -> ControlResponse {
        let before = self.pending_rounds.len();
        self.pending_rounds.retain(|r| r.id != round_id);
        if self.pending_rounds.len() == before {
            return ControlResponse::error(format!(
                "no live round {round_id}: it never existed, already delivered, or was already cancelled"
            ));
        }
        self.persist_pending_rounds();
        ControlResponse::Ok
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
    /// naturally holds until the main worker is idle again.
    ///
    /// A due round whose main worker is *gone* — no main panel id, the panel
    /// no longer exists, or it has `Exited` (the agent process died on its
    /// own; only `kill_panel` clears the id, so a self-exit leaves
    /// `main_panel_id` pointing at an `Exited` panel) — is dropped after its
    /// assembled report is spilled to `dropped-rounds.log`: no caucus→main
    /// push can ever land, so re-queuing it would spin forever, and discarding
    /// it silently would lose every sub-agent's work. A delivery that *fails*
    /// (e.g. the main PTY writer went away mid-tick) keeps the round for a
    /// later tick rather than dropping it.
    ///
    /// Before the due-check, each non-expired round's backlog is fed
    /// (`Multiplexer::feed_round_backlog`): a panel that finished early with
    /// queued tasks is handed its next task and flips back to `Working`, so it
    /// is not yet settled and the round is not yet due — the early finisher
    /// keeps working its backlog instead of idling at the barrier. Once the
    /// fallback deadline has passed, no new backlog work starts; the round
    /// delivers the partial report.
    pub fn poll_pending_rounds(&mut self) {
        if self.pending_rounds.is_empty() {
            return;
        }
        let now = Instant::now();
        // Take the queue so the settle-checks below can borrow `self`.
        let rounds = std::mem::take(&mut self.pending_rounds);

        // Resolve main's liveness once. Main is *gone* — no caucus→main push
        // can ever land — when there is no main panel id, the panel no longer
        // exists, or it has `Exited`. A due round against a gone main is
        // spilled and dropped, never re-queued forever.
        let main = self.main_panel_id.and_then(|id| {
            self.panels
                .iter()
                .find(|p| p.id == id)
                .map(|p| (id, p.state()))
        });
        let main_gone = !matches!(main, Some((_, s)) if s != PanelState::Exited);
        let main_id = main.map(|(id, _)| id);
        let deliverable = self.main_deliverable();

        let mut delivered = false;
        // Whether the pending-round set changed this tick (a round dropped,
        // delivered, or had backlog fed) — only then is the durable snapshot
        // re-written, so the common idle tick does no I/O.
        let mut changed = false;
        for mut round in rounds {
            let fallback_due = now >= round.fallback_deadline;
            if !fallback_due {
                // Keep early finishers busy: hand any idle round-panel its
                // next queued task before judging the round done. Once the
                // fallback deadline has fired, do not start more queued work:
                // the deadline means deliver the partial report now.
                changed |= self.feed_round_backlog(&mut round);
            }
            let all_settled = self.round_settled(&round);
            let due = fallback_due || all_settled;

            if !due {
                // Sub-agents still working: keep watching, whatever main's state.
                self.pending_rounds.push(round);
                continue;
            }
            if main_gone {
                // No wake path will ever exist. Spill the assembled report so
                // the sub-agents' work is not silently lost, then drop.
                let report = self.assemble_round_report(&round, all_settled);
                self.record_dropped_round(&report);
                changed = true;
                continue;
            }
            if deliverable && !delivered {
                // Spill the full per-panel report to disk and inject only a
                // compact summary that points at it — never the whole
                // unstructured report into the main PTY (a multi-panel round's
                // results can run to hundreds of KB; one giant bracketed paste
                // risks the backend's paste-handling pathologies).
                let report = self.assemble_round_report(&round, all_settled);
                let report_path = self.spill_round_report(round.id, &report);
                let summary =
                    self.render_round_summary(&round, all_settled, report_path.as_deref());
                // `deliverable` implies a live, idle main panel exists.
                let mid = main_id.expect("deliverable implies a main panel");
                match McpToolSurface::send_keys(self, mid, &summary, true) {
                    Ok(()) => {
                        delivered = true;
                        changed = true;
                    }
                    Err(err) => {
                        // Delivery failed (e.g. the main PTY writer went away
                        // mid-tick): keep the round so a later tick retries,
                        // rather than discarding every sub-agent's result.
                        warn!(error = %err, "round delivery to main panel failed; will retry");
                        self.pending_rounds.push(round);
                    }
                }
            } else {
                // Gate closed (main busy / mid-compose) or one already
                // delivered this tick: keep it for a later tick.
                self.pending_rounds.push(round);
            }
        }
        if changed {
            self.persist_pending_rounds();
        }
    }

    /// Append a dropped round's assembled report to the session's
    /// `dropped-rounds.log`, so a round whose main worker is gone (exited or
    /// never existed) is recorded rather than silently lost. Best-effort: a
    /// write failure is logged, not propagated — the round is being dropped
    /// either way.
    fn record_dropped_round(&self, report: &str) {
        self.append_dropped_round(
            "----- dropped round (no main worker to deliver to) -----",
            report,
        );
    }

    /// Append a `header` line then a `body` block to the session's
    /// `dropped-rounds.log` — the single sink for every round caucus could not
    /// deliver (main gone, or lost to a restart). Best-effort: a write failure
    /// is logged, not propagated.
    fn append_dropped_round(&self, header: &str, body: &str) {
        use std::io::Write;
        let path = self.session.root_dir.join("dropped-rounds.log");
        let spill = || -> std::io::Result<()> {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            writeln!(f, "{header}")?;
            writeln!(f, "{body}")?;
            Ok(())
        };
        if let Err(err) = spill() {
            warn!(path = %path.display(), error = %err, "failed to spill dropped round report");
        }
    }

    /// Snapshot the live pending rounds into their persistable form. Panel ids
    /// (regenerated on resume) are resolved to role labels; per-panel captured
    /// turns and the remaining backlog count are carried so resume can both
    /// preserve the work and summarise it.
    fn pending_round_records(&self) -> Vec<crate::session::round_record::PendingRoundRecord> {
        use crate::session::round_record::{PendingRoundRecord, RoundPanelRecord};
        self.pending_rounds
            .iter()
            .map(|r| {
                let panels = r
                    .panels
                    .iter()
                    .map(|&id| RoundPanelRecord {
                        role: self
                            .panels
                            .iter()
                            .find(|p| p.id == id)
                            .map(|p| p.role.clone())
                            .unwrap_or_else(|| "(gone)".to_string()),
                        captured: r.captured.get(&id).cloned().unwrap_or_default(),
                        pending_backlog: r.backlog.get(&id).map(VecDeque::len).unwrap_or(0),
                    })
                    .collect();
                PendingRoundRecord {
                    panels,
                    read_mode: r.read_mode,
                }
            })
            .collect()
    }

    /// Persist the live pending rounds to `<session_root>/pending-rounds.json`
    /// (or remove it when there are none). Single owner of pending-round
    /// persistence: called whenever the set changes (registration, delivery,
    /// drop, backlog feed). Best-effort — a write failure is logged, not fatal.
    fn persist_pending_rounds(&self) {
        let records = self.pending_round_records();
        if let Err(err) = crate::session::round_record::write(&self.session.root_dir, &records) {
            warn!(error = %err, "pending-rounds persistence failed");
        }
    }

    /// Surface the rounds a prior caucus instance left in flight when it quit
    /// or crashed — read from the persisted `pending-rounds.json`. Called once
    /// after the roster is restored, before the event loop starts; a no-op on a
    /// fresh launch (no file) or after a clean delivery (file removed).
    ///
    /// The sub-agent processes restart fresh, so a round is never silently
    /// continued. Each dropped round's captured work is appended to
    /// `dropped-rounds.log` (preserved, not lost), and a single notice is
    /// queued for the resumed main worker — whose claude conversation reloaded
    /// still believing its `register_round` was live — telling it the round was
    /// dropped so it stops waiting and can re-issue the work.
    ///
    /// `pending-rounds.json` is cleared immediately (it is reused for *this*
    /// session's live rounds), but the generated notice is in-memory only and
    /// not delivered until the main worker next goes idle. So the notice is also
    /// persisted to `resume-notice.txt` and only removed once delivered
    /// ([`Self::poll_resume_notice`]) — and any such notice a *prior* run left
    /// undelivered is loaded here first, so a second crash before delivery does
    /// not lose it. Delivery is at-least-once, never silently dropped.
    pub fn ingest_resumed_rounds(&mut self) {
        let root = self.session.root_dir.clone();

        // A prior run may have generated a drop notice it crashed before
        // delivering; recover it so it is still surfaced this run.
        let carried = crate::session::round_record::read_notice(&root);

        let records = crate::session::round_record::read(&root);
        // Free `pending-rounds.json` for the resumed session's live rounds. The
        // dropped work is preserved in `dropped-rounds.log` and the notice in
        // `resume-notice.txt` below, so the source snapshot is no longer needed.
        crate::session::round_record::clear(&root);
        if records.is_empty() {
            // No newly-dropped rounds, but a carried-over notice still must be
            // delivered (and stays persisted until it is).
            self.resume_round_notice = carried;
            return;
        }

        let mut notice = format!(
            "[caucus] {} round(s) you registered before the last restart were \
             dropped — caucus cannot deliver a round across a restart (the \
             sub-agent processes started fresh). Re-issue the work if you still \
             need it. Their captured output is preserved in \
             dropped-rounds.log. Summary:\n",
            records.len()
        );
        for (i, round) in records.iter().enumerate() {
            notice.push_str(&format!(
                "\n## dropped round {} ({} panel(s))\n",
                i + 1,
                round.panels.len()
            ));
            let mut spill = String::new();
            for panel in &round.panels {
                notice.push_str(&format!(
                    "  - role {}: {} captured turn(s), {} backlog task(s) un-run\n",
                    panel.role,
                    panel.captured.len(),
                    panel.pending_backlog
                ));
                spill.push_str(&format!(
                    "\n## role {} ({} captured turn(s), {} backlog un-run)\n",
                    panel.role,
                    panel.captured.len(),
                    panel.pending_backlog
                ));
                for (t, body) in panel.captured.iter().enumerate() {
                    spill.push_str(&format!("\n### task {}\n{}\n", t + 1, body.trim()));
                }
            }
            self.append_dropped_round(
                &format!("----- dropped round {} (lost to a restart) -----", i + 1),
                &spill,
            );
        }

        // Prepend any carried-over notice a prior run never delivered, so both
        // reach the main worker in one push.
        let notice = match carried {
            Some(prev) => format!("{prev}\n\n{notice}"),
            None => notice,
        };
        // Persist the notice durably *before* queuing it: it is delivered only
        // when the main worker next goes idle, and a crash in that window would
        // otherwise lose it (`pending-rounds.json` was already cleared above).
        // `poll_resume_notice` removes this file once the push lands.
        if let Err(err) = crate::session::round_record::write_notice(&root, &notice) {
            warn!(error = %err, "resume-notice persistence failed");
        }
        self.resume_round_notice = Some(notice);
    }

    /// Deliver the resume notice (in-flight rounds dropped by the last restart)
    /// to the main worker once it is idle, then clear it. Mirrors
    /// [`Multiplexer::poll_stranded_main`]: one-shot, gated by
    /// `Multiplexer::main_deliverable` so it never lands mid-compose, and the
    /// push flips the main panel to `Working`, closing the gate for the tick.
    /// The notice is cleared only on a confirmed send, so a closed gate this
    /// tick simply retries next tick.
    pub fn poll_resume_notice(&mut self) {
        if self.resume_round_notice.is_none() {
            return;
        }
        let Some(main_id) = self.main_panel_id else {
            return;
        };
        if !self.main_deliverable() {
            return;
        }
        let notice = self
            .resume_round_notice
            .clone()
            .expect("checked Some above");
        match McpToolSurface::send_keys(self, main_id, &notice, true) {
            // Delivered: drop the in-memory copy and the durable backup together.
            Ok(()) => {
                self.resume_round_notice = None;
                crate::session::round_record::clear_notice(&self.session.root_dir);
            }
            Err(err) => warn!(error = %err, "resume-notice delivery to main panel failed"),
        }
    }

    /// Whether every panel in `round` has settled *and* any per-panel backlog
    /// has drained. A missing panel counts as settled: there is no live worker
    /// left to feed, even if a stale backlog entry exists for that id. An
    /// `Exited` panel counts the same way: it is terminal, and
    /// [`Self::feed_round_backlog`] only ever feeds `Idle` panels, so its
    /// backlog can never drain — gating on it would wedge the round forever.
    fn round_settled(&self, round: &PendingRound) -> bool {
        if !self.wait_panels_settled(&round.panels) {
            return false;
        }
        round
            .panels
            .iter()
            .all(|id| match self.panels.iter().find(|p| p.id == *id) {
                None => true,
                Some(p) if p.state() == PanelState::Exited => true,
                Some(_) => round.backlog.get(id).is_none_or(VecDeque::is_empty),
            })
    }

    /// Re-key a panel's live round membership after [`Multiplexer::restart_panel`]
    /// swaps its `PanelId` (a fresh PTY is a fresh id). The restarted panel is
    /// the *same* logical round member — same role, same resumed conversation,
    /// same worktree — so every pending round that referenced `old` must now
    /// reference `new` across all three id-keyed fields (`panels`, `backlog`,
    /// `captured`). Without this the round resolves `old` to a missing panel
    /// (see [`Self::round_settled`]) and silently drops the member, settling
    /// without its contribution. `restart_panel` is the only id-changing path:
    /// every other spawn allocates a genuinely new panel, and `kill_panel`
    /// detaches permanently with no replacement.
    pub(super) fn remap_round_membership(&mut self, old: PanelId, new: PanelId) {
        for round in &mut self.pending_rounds {
            for id in &mut round.panels {
                if *id == old {
                    *id = new;
                }
            }
            if let Some(backlog) = round.backlog.remove(&old) {
                round.backlog.insert(new, backlog);
            }
            if let Some(captured) = round.captured.remove(&old) {
                round.captured.insert(new, captured);
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
    /// `Working` (and [`Multiplexer::poll_round_blocked_panels`] routes it to
    /// the main worker instead). The next task is delivered with `enter`, which
    /// flips the panel back to `Working` (so it is no longer settled); an empty
    /// queue is left in place and the panel settles. The queue is popped only after
    /// the send actually succeeds, so a failed delivery leaves the task at the
    /// front to be retried next tick rather than silently dropped. Feeding is
    /// not gated by `Multiplexer::main_deliverable`: keeping a worker busy is
    /// independent of the main panel's state.
    ///
    /// Just before each next task is sent — while the panel is still idle — the
    /// finished turn's output is read in `read_mode` and, on a confirmed send,
    /// pushed to `round.captured`. This is what lets the delivered report carry
    /// every backlog task's result rather than only the panel's final turn (see
    /// [`Multiplexer::assemble_round_report`]); the capture and the queue pop are
    /// committed together so a failed send re-reads and retries both next tick.
    ///
    /// Returns `true` if at least one task was actually fed (queue popped +
    /// turn captured) — i.e. the round's persistable state changed, so the
    /// caller re-writes the durable snapshot.
    fn feed_round_backlog(&mut self, round: &mut PendingRound) -> bool {
        // Decide every feed first (borrows only `round` + reads `self.panels`),
        // then deliver (mut-borrows `self`), so the two borrows never overlap.
        // The front is cloned, not popped, here — it is consumed only on a
        // confirmed send below.
        let mut feeds: Vec<(PanelId, String, String)> = Vec::new();
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
                let task = task.clone();
                // Capture the just-finished turn *now*, while the panel is
                // still idle — the send below opens a new turn and (for
                // `SinceLastTurn`) would otherwise overwrite it. Committed to
                // `round.captured` only on a confirmed send.
                let done = self
                    .read_panel(id, round.read_mode)
                    .unwrap_or_else(|e| format!("(could not read panel: {e})"));
                feeds.push((id, task, done));
            }
        }
        let mut fed = false;
        for (id, task, done) in feeds {
            match McpToolSurface::send_keys(self, id, &task, true) {
                // Delivered: consume the task (still the queue's front, single
                // tick, nothing else mutates the queue between collect + here)
                // and record the finished turn's output so the delivered report
                // carries every task's result, not only the last.
                Ok(()) => {
                    round.backlog.get_mut(&id).and_then(VecDeque::pop_front);
                    round.captured.entry(id).or_default().push(done);
                    fed = true;
                }
                // Delivery failed: leave the task at the front and capture
                // nothing; the panel is still idle, so the next tick re-reads
                // and retries.
                Err(err) => warn!(error = %err, panel = %id, "round backlog feed failed"),
            }
        }
        fed
    }

    /// Whether a caucus→main push may land *this tick*: the main panel exists,
    /// is coarse `Idle`, and has no un-submitted human keystroke within
    /// `COMPOSE_GRACE` (so the user is not mid-compose). The single gate shared
    /// by both push paths — round completion
    /// ([`Multiplexer::poll_pending_rounds`]) and selection prompts
    /// ([`Multiplexer::poll_round_blocked_panels`]). Each push flips the main
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

    /// Nudge the main worker when it has gone idle while sub-panels still run
    /// but **no** round is registered to ever wake it — the stranded-main
    /// safety net. Called once per event-loop tick, after round delivery.
    ///
    /// caucus's only caucus→main pushes ([`Multiplexer::poll_pending_rounds`],
    /// [`Multiplexer::poll_round_blocked_panels`]) both require a registered
    /// round. If the main worker broadcasts/spawns work and ends its turn
    /// *without* calling `register_round`, no round exists, so nothing ever
    /// re-prompts it: it sits idle forever while the sub-panels work. This is
    /// the uniform guard for that gap — independent of how the work was
    /// dispatched (broadcast, per-panel `send_keys`, or `spawn_role`) — telling
    /// main its panels are still working and it must register a round (or act),
    /// because caucus cannot push their results back without one.
    ///
    /// Stranded ⟺ main exists and is `Idle`, `pending_rounds` is empty (a
    /// pending round *is* a wake path), and ≥1 non-main panel is
    /// `Working`/`Spawning`. Fires only while stranded, the shared
    /// deliverability gate is open (`Multiplexer::main_deliverable`), and at
    /// least `STRANDED_NUDGE_COOLDOWN` has passed since the last nudge. The
    /// `main_stranded_last_nudge` latch is cleared the instant main is no longer
    /// stranded, so a fresh stranding re-arms without waiting out the cooldown.
    pub fn poll_stranded_main(&mut self) {
        let Some(main_id) = self.main_panel_id else {
            return;
        };
        // A pending round is itself a wake path; with one queued, main is not
        // stranded — round delivery (or its fallback) will re-prompt it.
        let working: Vec<PanelId> = if self.pending_rounds.is_empty() {
            self.panels
                .iter()
                .filter(|p| {
                    p.id != main_id
                        && matches!(p.state(), PanelState::Working | PanelState::Spawning)
                })
                .map(|p| p.id)
                .collect()
        } else {
            Vec::new()
        };
        if working.is_empty() {
            // Not stranded (round queued, or nothing else running): re-arm.
            self.main_stranded_last_nudge = None;
            return;
        }
        // Gate closed (main not idle, or user mid-compose): hold the latch so
        // the cooldown keeps counting; do not nudge into a busy/typed-on main.
        if !self.main_deliverable() {
            return;
        }
        if let Some(t) = self.main_stranded_last_nudge
            && Instant::now().duration_since(t) < STRANDED_NUDGE_COOLDOWN
        {
            return;
        }

        let mut notice = String::from(
            "[caucus] You are idle with no round registered, but these panels \
             are still working:\n",
        );
        for &id in &working {
            let role = self
                .panels
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.role.clone())
                .unwrap_or_default();
            notice.push_str(&format!("  - {id} (role: {role})\n"));
        }
        notice.push_str(
            "caucus can only push their results back to you through a round. \
             Call register_round on these panel ids and end your turn; caucus \
             re-prompts you when they settle. (Without a round there is no \
             wake-up path — you stay idle indefinitely.)",
        );
        match McpToolSurface::send_keys(self, main_id, &notice, true) {
            // Arm the cooldown only on a real push. A failed delivery must not
            // start the cooldown, or the next tick would wait it out on a nudge
            // that never landed and main stays stranded longer.
            Ok(()) => self.main_stranded_last_nudge = Some(Instant::now()),
            Err(err) => warn!(error = %err, "stranded-main nudge to main panel failed"),
        }
    }

    /// Announce to the main worker when a panel in a pending round has stopped
    /// on a prompt it cannot self-resolve — a selection menu or a raw `[y/n]`
    /// yes/no prompt (`BlockedPrompt`). This is the caucus→main *blocked-panel*
    /// push.
    ///
    /// Neither kind fires a `Stop` hook, so the panel stays coarse `Working` and
    /// its round never settles; without this the main worker would only learn at
    /// the fallback deadline (default 600s). caucus pushes an interim notice so
    /// the main worker can answer it (`select_option` / `send_keys`) and let the
    /// round finish. Gated by `Multiplexer::main_deliverable` and deduped by
    /// prompt content signature (`Multiplexer::notified_blockers`): a panel
    /// sitting on one prompt is announced once; a prompt whose content changes
    /// re-announces; a panel that leaves its prompt is forgotten so a future
    /// prompt re-announces. At most one notice per tick (shares the
    /// deliverability gate with round completion, which a push closes by
    /// flipping the main panel to `Working`).
    ///
    /// A selection menu the round *pre-authorized* via its
    /// [`SelectionPolicy`](crate::mcp::protocol::SelectionPolicy) is handled
    /// first: when the hints' keywords single out exactly one option
    /// ([`resolve_selection`]) caucus answers it itself with `select_option` and
    /// pushes **no** notice — the main worker is never interrupted. This drives
    /// only the sub-agent's panel, so it is *not* gated by `main_deliverable`
    /// and is deduped separately (`Multiplexer::auto_answered`) so a menu still
    /// on screen the tick after it was answered is not re-driven. Anything the
    /// hints do not resolve (no hints, zero or several matches, or a `[y/n]`
    /// prompt) falls through to the notice path above — the narrowed invariant
    /// "caucus never auto-answers a prompt the main worker did not pre-authorize,
    /// and only when the hints single out one option".
    pub fn poll_round_blocked_panels(&mut self) {
        let Some(main_id) = self.main_panel_id else {
            return;
        };
        if self.pending_rounds.is_empty() {
            return;
        }

        // Round panels currently stuck on a prompt, with a content signature
        // (cursor-independent for a menu) so a moving highlight never
        // re-announces.
        let round_panels: std::collections::HashSet<PanelId> = self
            .pending_rounds
            .iter()
            .flat_map(|r| r.panels.iter().copied())
            .collect();
        // Snapshot each non-main round panel's grid generation (immutable
        // borrow), then scan through the generation-keyed cache (mutable
        // borrow) — so an idle panel whose grid did not change is never
        // re-materialised + re-scanned this tick.
        let scan_targets: Vec<(PanelId, u64)> = round_panels
            .iter()
            .copied()
            .filter(|&pid| pid != main_id)
            .filter_map(|pid| {
                self.panels
                    .iter()
                    .find(|p| p.id == pid)
                    .map(|p| (pid, p.grid().generation()))
            })
            .collect();
        // First pass (classify, no side effects): a selection menu this round
        // pre-authorized to a *unique* keyword match is queued for auto-answer;
        // everything else (no hints, no/several matches, or a yes/no prompt) is
        // queued for the main-worker notice. `blocked_now` is every panel stuck
        // on *any* prompt — the prune set for both dedup maps, a superset of the
        // notice candidates so an auto-answered-but-not-yet-redrawn panel keeps
        // its dedup entry instead of being re-answered every tick.
        let mut open: Vec<(PanelId, u64)> = Vec::new();
        let mut prompts: HashMap<PanelId, BlockedPrompt> = HashMap::new();
        let mut to_auto: Vec<AutoAnswer> = Vec::new();
        let mut blocked_now: std::collections::HashSet<PanelId> = std::collections::HashSet::new();
        for (pid, generation) in scan_targets {
            let Some(prompt) = self.panel_blocked_cached(pid, generation) else {
                continue;
            };
            let sig = prompt.signature();
            blocked_now.insert(pid);
            if let BlockedPrompt::Selection(menu) = &prompt
                && let Some(policy) = self.round_policy_for(pid)
                && let Some(number) = resolve_selection(menu, &policy)
            {
                // Pre-authorized and uniquely resolved → auto-answer, never
                // notify. Skip if we already answered *this exact menu* (same
                // signature) and are waiting for the panel to redraw.
                if self.auto_answered.get(&pid) != Some(&sig) {
                    let label = menu
                        .options
                        .iter()
                        .find(|o| o.number == number)
                        .map(|o| o.label.clone())
                        .unwrap_or_default();
                    to_auto.push(AutoAnswer {
                        pid,
                        sig,
                        number,
                        label,
                    });
                }
                continue;
            }
            open.push((pid, sig));
            prompts.insert(pid, prompt);
        }

        // Forget dedup entries for panels no longer stuck on any prompt, so a
        // future prompt re-announces (notice) or re-resolves (auto-answer).
        self.notified_blockers
            .retain(|pid, _| blocked_now.contains(pid));
        self.auto_answered
            .retain(|pid, _| blocked_now.contains(pid));

        // Apply the auto-answers. Each drives a *sub-agent's* panel only and
        // never touches the main worker, so they are not gated by
        // `main_deliverable` and several independent panels may resolve in one
        // tick. A failed select leaves the panel un-recorded so the next tick
        // retries, mirroring the notice path's failed-push handling.
        for ans in to_auto {
            match McpToolSurface::select_option(self, ans.pid, ans.number) {
                Ok(()) => {
                    let role = self
                        .panels
                        .iter()
                        .find(|p| p.id == ans.pid)
                        .map(|p| p.role.clone())
                        .unwrap_or_default();
                    info!(
                        panel = %ans.pid,
                        role,
                        option = ans.number,
                        label = %ans.label,
                        "auto-answered a round panel's selection menu per the main worker's hints"
                    );
                    self.auto_answered.insert(ans.pid, ans.sig);
                    // Record it on the round (the same one whose hints resolved
                    // it) so the delivered report names the fork caucus took.
                    if let Some(round) = self
                        .pending_rounds
                        .iter_mut()
                        .find(|r| r.panels.contains(&ans.pid))
                    {
                        round.auto_answers.push(AutoAnswerRecord {
                            panel: ans.pid,
                            role,
                            number: ans.number,
                            label: ans.label,
                        });
                    }
                }
                Err(err) => {
                    warn!(error = %err, panel = %ans.pid, "auto-answer select_option failed")
                }
            }
        }

        let pick = Self::pick_blocker_to_notify(&open, &self.notified_blockers);

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
        let prompt = prompts.remove(&pid).unwrap();
        let role = self
            .panels
            .iter()
            .find(|p| p.id == pid)
            .map(|p| p.role.clone())
            .unwrap_or_default();
        let notice = Self::blocked_prompt_notice(pid, &role, &prompt);
        match McpToolSurface::send_keys(self, main_id, &notice, true) {
            // Mark the panel notified only on a real push (the documented
            // invariant above). A failed delivery leaves it un-notified so the
            // next tick re-announces it instead of swallowing the only notice.
            Ok(()) => {
                self.notified_blockers.insert(pid, sig);
            }
            Err(err) => warn!(error = %err, "blocked-panel notice to main panel failed"),
        }
    }

    /// The interim notice text for a round panel stuck on `prompt`, tailored to
    /// how the main worker answers it: `select_option` for a menu, `send_keys`
    /// for a yes/no prompt.
    fn blocked_prompt_notice(pid: PanelId, role: &str, prompt: &BlockedPrompt) -> String {
        match prompt {
            BlockedPrompt::Selection(menu) => format!(
                "[caucus] panel {pid} (role: {role}) is waiting on a selection — \
                 answer it so the round can finish.\n{}\n(answer with \
                 select_option({pid}, <number>); for a free-text reply pick the \
                 'type something' option, then send_keys your text.)",
                Self::render_menu(menu)
            ),
            BlockedPrompt::Permission(line) => format!(
                "[caucus] panel {pid} (role: {role}) is waiting on a yes/no prompt — \
                 answer it so the round can finish.\n{line}\n(answer with \
                 send_keys({pid}, \"y\") or send_keys({pid}, \"n\") as appropriate; \
                 caucus never auto-answers prompts.)"
            ),
        }
    }

    /// Pick which round panel to announce a blocking prompt for this tick.
    ///
    /// Pure decision core of [`Multiplexer::poll_round_blocked_panels`]: given
    /// the panels whose prompt needs the main worker as `(panel, signature)` and
    /// the already-notified set, return the first panel whose signature is new
    /// or changed (the one to push). The caller prunes stale dedup entries with
    /// its own `blocked_now` set (every panel on *any* prompt, a superset of
    /// these notice candidates), so no panel set is returned here.
    fn pick_blocker_to_notify(
        open: &[(PanelId, u64)],
        notified: &HashMap<PanelId, u64>,
    ) -> Option<PanelId> {
        open.iter()
            .find(|(p, sig)| notified.get(p) != Some(sig))
            .map(|(p, _)| *p)
    }

    /// The [`SelectionPolicy`] of the (first) pending round containing `pid`, if
    /// that round carries selection hints. Cloned so the caller can drop the
    /// `&self` borrow before answering the panel. A panel in no round, or in a
    /// round with no hints, yields `None` (every menu escalates).
    fn round_policy_for(&self, pid: PanelId) -> Option<SelectionPolicy> {
        self.pending_rounds
            .iter()
            .find(|r| r.panels.contains(&pid))
            .and_then(|r| r.selection_hints.clone())
    }

    /// Collect one round panel's contribution — its role, state, finished-turn
    /// bodies (captured backlog turns, then the final live read for a settled
    /// panel), and un-run backlog count — for the report and summary to render.
    /// Returns `None` when the panel id is gone (killed). The single site of
    /// the per-panel `read_panel`, so the full report and the injected summary
    /// always render the same captured text.
    fn round_panel_contribution(
        &self,
        round: &PendingRound,
        id: PanelId,
    ) -> Option<RoundPanelContribution> {
        let panel = self.panels.iter().find(|p| p.id == id)?;
        let state = panel.state();
        // A still-working panel (fallback delivery) contributes only what it
        // already finished — never a live read of a mid-turn panel.
        let still_working = matches!(state, PanelState::Working | PanelState::Spawning);
        // Every finished backlog turn, in feed order, captured the moment
        // before its successor was fed.
        let mut bodies = round.captured.get(&id).cloned().unwrap_or_default();
        if !still_working {
            bodies.push(
                self.read_panel(id, round.read_mode)
                    .unwrap_or_else(|e| format!("(could not read panel: {e})")),
            );
        }
        Some(RoundPanelContribution {
            role: panel.role.clone(),
            state,
            bodies,
            pending_backlog: round.backlog.get(&id).map(VecDeque::len).unwrap_or(0),
        })
    }

    /// Assemble a round's **full** report: a self-describing block per panel —
    /// role + current state, plus its result(s). A panel that ran a single task
    /// contributes that one output read via `read_mode`; a panel that ran a
    /// multi-task `backlog` contributes every finished turn — the outputs
    /// captured in `captured` (each prior turn, in feed order) followed by its
    /// final turn read live — under `### task N` headers so the main worker can
    /// tell them apart. A panel still `working` when the fallback deadline
    /// forced delivery contributes whatever it already finished plus an
    /// "unfinished" marker. A panel id that no longer exists is reported as
    /// gone. This is the report spilled to disk
    /// ([`Multiplexer::spill_round_report`]) and the one appended to
    /// `dropped-rounds.log` when there is no main worker to deliver to; the
    /// main worker is handed the compact [`Multiplexer::render_round_summary`]
    /// that points at it, never this whole block.
    fn assemble_round_report(&self, round: &PendingRound, all_settled: bool) -> String {
        let mut out = round_header(round.panels.len(), all_settled);
        push_auto_answers(&mut out, round);
        for &id in &round.panels {
            let Some(c) = self.round_panel_contribution(round, id) else {
                out.push_str(&format!("\n## panel {id} — gone (killed)\n"));
                continue;
            };
            out.push_str(&format!(
                "\n## panel {id} (role: {}) — {}\n",
                c.role,
                c.state.label()
            ));
            // One output stays header-less, identical to the pre-backlog
            // report; two or more get `### task N` headers.
            let total = c.bodies.len();
            for (i, body) in c.bodies.iter().enumerate() {
                if total > 1 {
                    out.push_str(&format!("\n### task {}\n", i + 1));
                }
                let body = body.trim();
                if body.is_empty() {
                    out.push_str("(no output captured)\n");
                } else {
                    // Bound each turn's body so a `scrollback` read cannot
                    // inflate even the on-disk report without limit; the full
                    // text is spilled to `round-spills/` and pointed at.
                    out.push_str(&self.bound_round_body(id, body));
                    out.push('\n');
                }
            }
            push_round_panel_footer(&mut out, &c);
        }
        out
    }

    /// Assemble the **compact** round-delivery message injected into the main
    /// worker's panel: a one-line-per-panel summary (role, state, output count)
    /// with a short teaser of each panel's latest output, plus a pointer to the
    /// full report spilled to disk. This is the structural fix for "never inject
    /// the whole unstructured report into the main PTY": a multi-panel round's
    /// full results can run to hundreds of KB, and pasting that in one bracketed
    /// paste risks the backend's paste-handling pathologies. The main worker
    /// reads `report_path` when a teaser is not enough; `report_path` is `None`
    /// only when the spill itself failed.
    fn render_round_summary(
        &self,
        round: &PendingRound,
        all_settled: bool,
        report_path: Option<&std::path::Path>,
    ) -> String {
        let mut out = round_header(round.panels.len(), all_settled);
        push_auto_answers(&mut out, round);
        match report_path {
            Some(path) => out.push_str(&format!(
                "Full per-panel report: {} — read that file for each panel's complete output.\n",
                path.display()
            )),
            None => out.push_str(
                "(the full report could not be written to disk; the teasers below are all that is available)\n",
            ),
        }
        for &id in &round.panels {
            let Some(c) = self.round_panel_contribution(round, id) else {
                out.push_str(&format!("\n## panel {id} — gone (killed)\n"));
                continue;
            };
            out.push_str(&format!(
                "\n## panel {id} (role: {}) — {} · {} output(s)\n",
                c.role,
                c.state.label(),
                c.bodies.len()
            ));
            // Teaser: the panel's latest output (final live read, else last
            // captured), trimmed to the teaser budget — the full text is in the
            // report file.
            let latest = c.bodies.last().map(|b| b.trim()).unwrap_or("");
            if latest.is_empty() {
                out.push_str("(no output captured)\n");
            } else {
                out.push_str(&summary_teaser(latest, ROUND_SUMMARY_TEASER_BYTES));
                out.push('\n');
            }
            push_round_panel_footer(&mut out, &c);
        }
        out
    }

    /// Write a round's full assembled report to
    /// `<session_root>/rounds/<round_id>.md` and return its path, so the
    /// caucus→main delivery can inject a compact summary that points at it
    /// rather than the whole report. The file name is the round's id (a ULID),
    /// unique per round. Best-effort: a failure is logged and yields `None`, and
    /// the summary then says the spill failed.
    fn spill_round_report(&self, round_id: RoundId, report: &str) -> Option<std::path::PathBuf> {
        let dir = self.session.root_dir.join("rounds");
        if let Err(err) = std::fs::create_dir_all(&dir) {
            warn!(error = %err, "round report dir create failed");
            return None;
        }
        let path = dir.join(format!("{round_id}.md"));
        match std::fs::write(&path, report) {
            Ok(()) => Some(path),
            Err(err) => {
                warn!(path = %path.display(), error = %err, "round report write failed");
                None
            }
        }
    }

    /// Bound one captured turn's body for the **on-disk** round report. A body
    /// within [`MAX_ROUND_BODY_BYTES`] is returned verbatim; a larger one is
    /// head/tail truncated around an elision marker and its **full** text
    /// spilled to `<session_root>/round-spills/`, so nothing is lost and the
    /// report points the main worker at the complete output. Truncation lands
    /// on UTF-8 char boundaries so the returned string is always valid.
    fn bound_round_body(&self, panel: PanelId, body: &str) -> String {
        if body.len() <= MAX_ROUND_BODY_BYTES {
            return body.to_string();
        }
        let half = MAX_ROUND_BODY_BYTES / 2;
        let head_end = floor_char_boundary(body, half);
        let tail_start = ceil_char_boundary(body, body.len() - half);
        let elided = tail_start.saturating_sub(head_end);
        let pointer = match self.spill_round_body(panel, body) {
            Some(path) => format!("full output spilled to {}", path.display()),
            None => "full output spill failed".to_string(),
        };
        format!(
            "{}\n…[{elided} bytes elided — {pointer}]…\n{}",
            &body[..head_end],
            &body[tail_start..]
        )
    }

    /// Write a too-large round-report body verbatim to
    /// `<session_root>/round-spills/<panel>-<hash>.txt` and return its path.
    /// The file name carries a content hash so identical bodies map to one file
    /// (idempotent) and distinct bodies never clobber each other — a report's
    /// pointer always resolves to exactly the text it elided. Best-effort: a
    /// failure is logged and yields `None` (the report then says so).
    fn spill_round_body(&self, panel: PanelId, body: &str) -> Option<std::path::PathBuf> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        body.hash(&mut h);
        let dir = self.session.root_dir.join("round-spills");
        if let Err(err) = std::fs::create_dir_all(&dir) {
            warn!(error = %err, "round-spill dir create failed");
            return None;
        }
        let path = dir.join(format!("{panel}-{:016x}.txt", h.finish()));
        match std::fs::write(&path, body) {
            Ok(()) => Some(path),
            Err(err) => {
                warn!(path = %path.display(), error = %err, "round-spill write failed");
                None
            }
        }
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

    /// Scan a panel's visible grid for whatever it is stuck waiting on
    /// ([`BlockedPrompt`]): a numbered selection menu first (the richer,
    /// higher-confidence match), then a raw `[y/n]` yes/no prompt. `None` when
    /// neither is confidently detected.
    pub(crate) fn panel_blocked_prompt(panel: &Panel) -> Option<BlockedPrompt> {
        let (_, rows) = panel.grid().size();
        let lines: Vec<String> = (0..rows)
            .map(|r| panel.grid().row_text(r).trim_end().to_string())
            .collect();
        if let Some(menu) = crate::term::scan_menu(&lines) {
            return Some(BlockedPrompt::Selection(menu));
        }
        crate::term::scan_yes_no_prompt(&lines).map(BlockedPrompt::Permission)
    }

    /// [`Multiplexer::panel_blocked_prompt`] gated by the panel's grid
    /// `generation`. If the panel's grid has not changed since the last scan
    /// (cached generation matches), the cached result is returned without
    /// re-materialising the viewport or re-running the scanners; otherwise it
    /// re-scans and refreshes the cache. This is what keeps the per-tick
    /// blocked-panel poll from re-scanning every idle round panel. Returns
    /// `None` for an unknown id.
    fn panel_blocked_cached(&mut self, id: PanelId, generation: u64) -> Option<BlockedPrompt> {
        if let Some((cached_gen, cached)) = self.blocked_scan_cache.get(&id)
            && *cached_gen == generation
        {
            return cached.clone();
        }
        let prompt = self
            .panels
            .iter()
            .find(|p| p.id == id)
            .and_then(Self::panel_blocked_prompt);
        self.blocked_scan_cache
            .insert(id, (generation, prompt.clone()));
        prompt
    }

    /// Overlay a live grid-detected blocking prompt ([`BlockedPrompt`]) onto the
    /// turn-signal-derived state, so `list_panels` reports the same blocked
    /// state the round-watch push announces. A visible menu or `[y/n]` prompt
    /// means the agent stopped mid-turn needing the main worker — which the
    /// `Stop`-hook state cannot see — so it outranks the signal-derived
    /// `Working`/`Idle` (mirroring `derive_agent_state`, where a grid hint is
    /// weighed before the turn signal). It never masks a stronger state
    /// (`Exited`/`Blocked*`/`Interrupted`/`Degraded`).
    pub(crate) fn overlay_blocked_state(
        base: DerivedState,
        blocked: Option<&BlockedPrompt>,
    ) -> DerivedState {
        match blocked {
            Some(prompt) if matches!(base, DerivedState::Working | DerivedState::Idle) => {
                prompt.derived_state()
            }
            _ => base,
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
    use crate::panel::lifecycle::Panel;
    use crate::pty::{Pty, PtyCommand};
    use crate::session::id::AgentId;
    use crate::session::runtime::test_support::*;
    use crate::signal::TurnSignal;
    use crate::term::{Grid, OutputCapture};
    use tempfile::TempDir;

    fn pending_round(
        panels: Vec<PanelId>,
        read_mode: ReadPanelMode,
        captured: HashMap<PanelId, Vec<String>>,
        backlog: HashMap<PanelId, VecDeque<String>>,
    ) -> PendingRound {
        PendingRound {
            id: RoundId::new(),
            panels,
            backlog,
            captured,
            read_mode,
            fallback_deadline: Instant::now() + Duration::from_secs(600),
            selection_hints: None,
            auto_answers: Vec::new(),
        }
    }

    /// Insert a hermetic `/bin/cat` panel directly into the mux so round
    /// scheduler tests do not depend on a real agent CLI being installed.
    fn push_cat_panel(mux: &mut Multiplexer, role: &str, state: PanelState) -> PanelId {
        let id = PanelId::new();
        let inner = area().inner();
        let pty = Pty::spawn(&PtyCommand::new("/bin/cat"), inner.width, inner.height).unwrap();
        mux.panels.push(Panel {
            id,
            role: role.to_string(),
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

    /// `register_round` acks immediately with the round id and a panel snapshot
    /// and stashes a `PendingRound` — it never blocks. An unknown id is omitted
    /// from the ack (it would not appear in `list_panels` either). The acked
    /// `round_id` matches the stashed round. `read_mode` defaults to
    /// `last_message`.
    #[tokio::test]
    async fn register_round_acks_and_stashes_a_pending_round() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let ghost = PanelId::new();

        let ack = mux.register_round(vec![ghost], None, Some(60), None, None);
        let acked_id = match ack {
            ControlResponse::RoundRegistered { round_id, panels } => {
                assert!(panels.is_empty());
                round_id
            }
            other => panic!("expected an immediate RoundRegistered ack, got {other:?}"),
        };
        assert_eq!(mux.pending_rounds.len(), 1, "round must be stashed");
        assert_eq!(mux.pending_rounds[0].id, acked_id, "acked id must match");
        assert_eq!(mux.pending_rounds[0].read_mode, ReadPanelMode::LastMessage);
    }

    /// `round_status` reports a live round by its registered id: each panel's
    /// state and remaining backlog. An unknown id is an error.
    #[tokio::test]
    async fn round_status_reports_a_live_round_and_errors_on_unknown() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let working = push_cat_panel(&mut mux, "reviewer", PanelState::Working);
        let idle = push_cat_panel(&mut mux, "writer", PanelState::Idle);

        let round_id = match mux.register_round(vec![working, idle], None, Some(600), None, None) {
            ControlResponse::RoundRegistered { round_id, .. } => round_id,
            other => panic!("expected RoundRegistered, got {other:?}"),
        };

        match mux.round_status(round_id) {
            ControlResponse::Panel { text } => {
                assert!(text.contains("2 panel(s)"), "status: {text}");
                assert!(text.contains("reviewer"), "status: {text}");
                assert!(text.contains("working"), "status: {text}");
                assert!(text.contains("writer"), "status: {text}");
                assert!(text.contains("settled"), "status: {text}");
            }
            other => panic!("expected Panel status, got {other:?}"),
        }

        let ghost_round = RoundId::new();
        assert!(
            matches!(mux.round_status(ghost_round), ControlResponse::Error { .. }),
            "an unknown round id must be an error"
        );

        mux.shutdown();
    }

    /// `cancel_round` drops a live round by id (leaving the panels alone) and
    /// errors on an unknown id. After cancelling, the round no longer polls.
    #[tokio::test]
    async fn cancel_round_drops_a_live_round_and_errors_on_unknown() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let panel = push_cat_panel(&mut mux, "reviewer", PanelState::Working);

        let round_id = match mux.register_round(vec![panel], None, Some(600), None, None) {
            ControlResponse::RoundRegistered { round_id, .. } => round_id,
            other => panic!("expected RoundRegistered, got {other:?}"),
        };
        assert_eq!(mux.pending_rounds.len(), 1);

        // An unknown id is rejected and leaves the round in place.
        assert!(
            matches!(
                mux.cancel_round(RoundId::new()),
                ControlResponse::Error { .. }
            ),
            "an unknown round id must be an error"
        );
        assert_eq!(mux.pending_rounds.len(), 1, "unknown cancel must not drop");

        // The real id drops the round; the panel itself is untouched.
        assert!(matches!(mux.cancel_round(round_id), ControlResponse::Ok));
        assert!(mux.pending_rounds.is_empty(), "round must be dropped");
        assert!(
            mux.panels().iter().any(|p| p.id == panel),
            "cancel must not kill the panel"
        );

        mux.shutdown();
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
            None,
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

        let round = pending_round(
            vec![ghost],
            ReadPanelMode::LastMessage,
            HashMap::new(),
            HashMap::new(),
        );
        let report = mux.assemble_round_report(&round, true);
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

        let round = pending_round(
            vec![sub],
            ReadPanelMode::LastMessage,
            HashMap::new(),
            HashMap::new(),
        );
        let report = mux.assemble_round_report(&round, false);
        assert!(
            report.contains("still working"),
            "a Working panel must be marked unfinished: {report}"
        );

        mux.shutdown();
    }

    /// `assemble_round_report` emits a `### task N` section for every captured
    /// turn followed by the panel's final live read — so a multi-task backlog
    /// delivers all of its intermediate outputs, not only the last. Boundary:
    /// two captured turns + the live final read → three numbered sections, each
    /// captured body present in the report.
    ///
    /// Spawning a panel needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn assemble_round_report_emits_a_section_per_captured_task() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Some(sub) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };

        // Two finished turns already captured; the settled panel adds a third
        // (its final turn, read live) when the report is assembled.
        let captured = HashMap::from([(
            sub,
            vec![
                "output of task one".to_string(),
                "output of task two".to_string(),
            ],
        )]);
        let round = pending_round(
            vec![sub],
            ReadPanelMode::LastMessage,
            captured,
            HashMap::new(),
        );
        let report = mux.assemble_round_report(&round, true);

        assert!(
            report.contains("### task 1"),
            "first captured turn header missing: {report}"
        );
        assert!(
            report.contains("### task 2"),
            "second captured turn header missing: {report}"
        );
        assert!(
            report.contains("### task 3"),
            "final live-read turn header missing: {report}"
        );
        assert!(
            report.contains("output of task one"),
            "first captured body missing: {report}"
        );
        assert!(
            report.contains("output of task two"),
            "second captured body missing: {report}"
        );

        mux.shutdown();
    }

    /// A single-output panel (no backlog → empty `captured`) keeps the original
    /// header-less report shape: `### task N` sections appear only when there is
    /// more than one output to disambiguate. Backward-compat boundary against
    /// the multi-task case above.
    ///
    /// Spawning a panel needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn assemble_round_report_omits_task_headers_for_a_single_output() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Some(sub) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };

        let round = pending_round(
            vec![sub],
            ReadPanelMode::LastMessage,
            HashMap::new(),
            HashMap::new(),
        );
        let report = mux.assemble_round_report(&round, true);
        assert!(
            !report.contains("### task"),
            "a single-output panel must not get task headers: {report}"
        );

        mux.shutdown();
    }

    /// Finding 24: a round's full results are no longer injected wholesale into
    /// the main PTY. `spill_round_report` writes the complete report to
    /// `rounds/<round_id>.md`, and `render_round_summary` injects only a compact
    /// summary that points at that file. A small body is teased inline so a
    /// trivial round needs no file read. (A `Working` panel + a captured turn
    /// stands in for a finished sub-agent turn without a live read of the
    /// hermetic cat panel, which has no real output.)
    #[tokio::test]
    async fn round_summary_points_at_a_spilled_full_report() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let panel = push_cat_panel(&mut mux, "reviewer", PanelState::Working);
        let captured = HashMap::from([(panel, vec!["the full finding body".to_string()])]);
        let round = pending_round(
            vec![panel],
            ReadPanelMode::LastMessage,
            captured,
            HashMap::new(),
        );

        let report = mux.assemble_round_report(&round, false);
        let path = mux
            .spill_round_report(round.id, &report)
            .expect("report spilled");
        assert!(
            path.starts_with(mux.session.root_dir.join("rounds")),
            "report must land in rounds/: {}",
            path.display()
        );
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains("the full finding body"),
            "the disk report must hold the full body: {on_disk}"
        );

        let summary = mux.render_round_summary(&round, false, Some(path.as_path()));
        assert!(
            summary.contains("Round complete"),
            "summary header missing: {summary}"
        );
        assert!(
            summary.contains(&path.display().to_string()),
            "summary must point at the report file: {summary}"
        );
        assert!(
            summary.contains("the full finding body"),
            "a small body must be teased inline: {summary}"
        );

        mux.shutdown();
    }

    /// A body far over the teaser budget is bounded in the *injected* summary
    /// (with an elision marker) while the spilled report file keeps it in full
    /// — the whole point of finding 24: the main PTY gets a teaser, the file
    /// gets the body.
    #[tokio::test]
    async fn round_summary_teases_a_large_body_kept_full_in_the_report() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let panel = push_cat_panel(&mut mux, "reviewer", PanelState::Working);
        let big = "x".repeat(ROUND_SUMMARY_TEASER_BYTES * 4);
        let captured = HashMap::from([(panel, vec![big.clone()])]);
        let round = pending_round(
            vec![panel],
            ReadPanelMode::LastMessage,
            captured,
            HashMap::new(),
        );

        let report = mux.assemble_round_report(&round, false);
        let path = mux.spill_round_report(round.id, &report).unwrap();
        let summary = mux.render_round_summary(&round, false, Some(path.as_path()));

        // The injected summary is bounded — it must not inline the whole body.
        assert!(
            summary.len() < big.len(),
            "summary ({} bytes) must be far smaller than the body ({} bytes)",
            summary.len(),
            big.len()
        );
        assert!(
            summary.contains("more byte(s) in the report file"),
            "the teaser must mark the elision: {summary}"
        );
        // The spilled report keeps the full body (under F6's 16K per-body bound).
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains(&big),
            "the report file must hold the full body"
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

        mux.register_round(vec![PanelId::new()], None, Some(600), None, None);
        assert_eq!(mux.pending_rounds.len(), 1);

        mux.poll_pending_rounds();
        assert!(
            mux.pending_rounds.is_empty(),
            "a due round with no main panel must be dropped"
        );
    }

    /// A due round whose main worker has *exited on its own* (process crash/
    /// OOM) must drop, not re-queue forever. `pump_all` flips the panel to
    /// `Exited` but does not clear `main_panel_id` (only `kill_panel` does), so
    /// before this fix the round saw `main_panel_id == Some(..)` yet
    /// `deliverable == false`, falling into the re-queue arm every tick. The
    /// dropped report is spilled rather than silently lost.
    #[tokio::test]
    async fn poll_pending_rounds_drops_when_main_has_exited() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_cat_panel(&mut mux, "main", PanelState::Exited);
        mux.main_panel_id = Some(main);

        // A round on a non-existent id is due immediately (a missing id counts
        // as settled).
        mux.register_round(vec![PanelId::new()], None, Some(600), None, None);
        assert_eq!(mux.pending_rounds.len(), 1);

        mux.poll_pending_rounds();
        assert!(
            mux.pending_rounds.is_empty(),
            "a due round against an exited main must drop, not re-queue forever"
        );
        assert!(
            mux.session.root_dir.join("dropped-rounds.log").exists(),
            "the dropped round's report must be spilled to the session dir"
        );

        mux.shutdown();
    }

    /// The main worker panel cannot be killed through the destruction owner,
    /// mirroring `restart_panel`'s guard: it owns the MCP control channel and is
    /// the round-delivery target, so `kill_panel` over MCP / the control socket
    /// must not tear it down. A non-main panel is killed normally and leaves
    /// `main_panel_id` intact; killing main is refused and main stays live.
    #[tokio::test]
    async fn kill_panel_refuses_the_main_worker_but_kills_others() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_cat_panel(&mut mux, "main", PanelState::Idle);
        let other = push_cat_panel(&mut mux, "other", PanelState::Idle);
        mux.main_panel_id = Some(main);

        // Killing a non-main panel succeeds and does not disturb main_panel_id.
        mux.kill_panel(other).unwrap();
        assert!(!mux.panels().iter().any(|p| p.id == other));
        assert_eq!(mux.main_panel_id, Some(main));

        // Killing main is refused; main_panel_id and the live panel are intact.
        let err = mux.kill_panel(main).unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot kill the main worker panel"),
            "got: {err}"
        );
        assert_eq!(mux.main_panel_id, Some(main));
        assert!(
            mux.panels().iter().any(|p| p.id == main),
            "the refused kill leaves the main panel running"
        );

        mux.shutdown();
    }

    /// A due round is *held*, not delivered, while the main panel is alive but
    /// busy (not `Idle`). The round stays pending for a later tick rather than
    /// landing mid-turn. (A main that is *gone* — missing/exited — is dropped,
    /// not held: that is the wedge guarded by
    /// `poll_pending_rounds_drops_when_main_has_exited`.)
    #[tokio::test]
    async fn poll_pending_rounds_holds_when_main_busy() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_cat_panel(&mut mux, "main", PanelState::Working);
        mux.main_panel_id = Some(main);

        mux.register_round(vec![PanelId::new()], None, Some(600), None, None);
        mux.poll_pending_rounds();
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "round must be held while the live main panel is busy"
        );

        mux.shutdown();
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
        mux.register_round(vec![sub], None, Some(600), None, None);

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
        assert!(
            mux.session
                .root_dir
                .join("rounds")
                .read_dir()
                .is_ok_and(|mut d| d.next().is_some()),
            "delivering a round spills its full report to rounds/",
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
            None,
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

    /// Each backlog feed captures the just-finished turn into
    /// `PendingRound::captured`, growing it by exactly one per fed task, while
    /// the final (queue-empty) settle captures nothing — that last turn is read
    /// live by `assemble_round_report`. The accumulation that lets the delivered
    /// report carry every task's result, tested at its boundaries.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn feed_round_backlog_captures_each_finished_turn() {
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

        // Two queued tasks; generous fallback so only the backlog drives it.
        mux.register_round(
            vec![sub],
            None,
            Some(3600),
            Some(HashMap::from([(
                sub,
                vec!["task-1".to_string(), "task-2".to_string()],
            )])),
            None,
        );

        // First feed: idle sub with task-1 queued → captures the pre-backlog turn.
        mux.poll_pending_rounds();
        assert_eq!(
            mux.pending_rounds[0].captured.get(&sub).map(Vec::len),
            Some(1),
            "the first feed must capture exactly one finished turn",
        );

        // Settle, then second feed: captures task-1's output.
        mux.handle_signal(TurnSignal::now(
            session_id,
            sub,
            crate::signal::TurnKind::Stop,
            None,
            serde_json::Value::Null,
        ));
        mux.poll_pending_rounds();
        assert_eq!(
            mux.pending_rounds[0].captured.get(&sub).map(Vec::len),
            Some(2),
            "the second feed must capture a second finished turn",
        );

        // Settle with the queue now drained. Hold delivery (compose grace) so
        // the round stays pending and we can prove the final settle captures
        // nothing — the last turn is left for the live read.
        mux.handle_signal(TurnSignal::now(
            session_id,
            sub,
            crate::signal::TurnKind::Stop,
            None,
            serde_json::Value::Null,
        ));
        mux.main_compose_since = Some(Instant::now());
        mux.poll_pending_rounds();
        assert_eq!(
            mux.pending_rounds[0].captured.get(&sub).map(Vec::len),
            Some(2),
            "the queue-empty settle must not capture — the last turn is read live",
        );

        // Release the hold: the drained round now delivers to the idle main.
        mux.main_compose_since = None;
        mux.poll_pending_rounds();
        assert!(
            mux.pending_rounds.is_empty(),
            "the drained round must deliver once the compose hold clears",
        );

        mux.shutdown();
    }

    /// An idle panel with queued backlog is not settled yet. The queue must
    /// drain before round delivery; otherwise a failed or skipped feed could
    /// report a round complete while work remains queued.
    #[tokio::test]
    async fn round_settled_requires_live_panel_backlog_to_drain() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let sub = push_cat_panel(&mut mux, "reviewer", PanelState::Idle);
        let mut round = pending_round(
            vec![sub],
            ReadPanelMode::LastMessage,
            HashMap::new(),
            HashMap::from([(sub, VecDeque::from(["queued".to_string()]))]),
        );

        assert!(
            !mux.round_settled(&round),
            "an idle panel with queued backlog is not settled"
        );

        round.backlog.get_mut(&sub).unwrap().clear();
        assert!(mux.round_settled(&round), "draining the queue settles it");

        mux.shutdown();
    }

    /// An `Exited` panel with queued backlog settles immediately. The panel is
    /// terminal and `feed_round_backlog` only feeds `Idle` panels, so its
    /// backlog can never drain — gating on it (like a live `Idle` panel) would
    /// wedge the round forever. It is treated like a missing panel instead.
    #[tokio::test]
    async fn round_settles_when_a_panel_exits_with_undrained_backlog() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let sub = push_cat_panel(&mut mux, "reviewer", PanelState::Idle);
        let round = pending_round(
            vec![sub],
            ReadPanelMode::LastMessage,
            HashMap::new(),
            HashMap::from([(sub, VecDeque::from(["queued".to_string()]))]),
        );

        assert!(
            !mux.round_settled(&round),
            "an idle panel with queued backlog is not settled"
        );

        mux.panels.iter_mut().find(|p| p.id == sub).unwrap().state = PanelState::Exited;

        assert!(
            mux.round_settled(&round),
            "an exited panel with queued backlog settles — it can never drain"
        );

        mux.shutdown();
    }

    /// Restarting a round panel swaps its `PanelId` (fresh PTY = fresh id) but
    /// keeps it the same logical member. `remap_round_membership` must re-key
    /// the round's `panels`, `backlog`, and `captured` from the old id to the
    /// new one; otherwise the round resolves the old id to a missing panel and
    /// silently drops the member without its contribution.
    #[tokio::test]
    async fn restart_remaps_round_membership_to_the_new_panel_id() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let old = push_cat_panel(&mut mux, "reviewer", PanelState::Idle);
        let round = pending_round(
            vec![old],
            ReadPanelMode::LastMessage,
            HashMap::from([(old, vec!["earlier turn".to_string()])]),
            HashMap::from([(old, VecDeque::from(["queued".to_string()]))]),
        );
        mux.pending_rounds.push(round);

        let new = PanelId::new();
        mux.remap_round_membership(old, new);

        let r = &mux.pending_rounds[0];
        assert_eq!(r.panels, vec![new], "round member re-keyed to the new id");
        assert!(
            !r.backlog.contains_key(&old) && r.backlog.contains_key(&new),
            "backlog re-keyed to the new id"
        );
        assert!(
            !r.captured.contains_key(&old) && r.captured.contains_key(&new),
            "captured turns re-keyed to the new id"
        );

        mux.shutdown();
    }

    /// Once the fallback deadline has fired, polling must deliver the partial
    /// report instead of starting another queued backlog task. Before this
    /// guard, an idle panel at the deadline was fed first, flipping it back to
    /// `Working` even though the round was already due by timeout.
    #[tokio::test]
    async fn poll_pending_rounds_does_not_feed_backlog_after_fallback() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_cat_panel(&mut mux, "main", PanelState::Idle);
        mux.main_panel_id = Some(main);
        let sub = push_cat_panel(&mut mux, "reviewer", PanelState::Idle);

        mux.register_round(
            vec![sub],
            None,
            Some(3600),
            Some(HashMap::from([(sub, vec!["late task".to_string()])])),
            None,
        );
        mux.pending_rounds[0].fallback_deadline = Instant::now() - Duration::from_secs(1);

        mux.poll_pending_rounds();

        assert!(
            mux.pending_rounds.is_empty(),
            "fallback-due round should be delivered and dropped"
        );
        assert_eq!(
            mux.panels().iter().find(|p| p.id == sub).unwrap().state(),
            PanelState::Idle,
            "fallback delivery must not start another queued backlog task"
        );
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "delivery injects the partial report into the main panel"
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

    /// The stranded-main nudge: main idle, a sub-panel still `Working`, and no
    /// round registered → caucus prods main (flipping it to `Working`, the
    /// injected turn) and arms the cooldown latch. This is the only wake path
    /// when the main worker forgot to call `register_round`.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn poll_stranded_main_nudges_idle_main_with_working_sub_and_no_round() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Some(main) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.main_panel_id = Some(main);

        // A sub-panel left `Working`, and deliberately no round registered.
        let Ok(sub) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.note_prompt_delivered(sub);
        assert_eq!(
            mux.panels().iter().find(|p| p.id == sub).unwrap().state(),
            PanelState::Working,
        );
        assert!(mux.pending_rounds.is_empty());

        mux.poll_stranded_main();
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "stranded main must be nudged (its panel flips to Working)",
        );
        assert!(
            mux.main_stranded_last_nudge.is_some(),
            "nudging must arm the cooldown latch",
        );

        mux.shutdown();
    }

    /// A pending round is itself a wake path, so a main idle with a `Working`
    /// sub is *not* stranded while a round is queued — no nudge fires and the
    /// round stays queued for [`Multiplexer::poll_pending_rounds`] to deliver.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn poll_stranded_main_silent_when_a_round_is_pending() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Some(main) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.main_panel_id = Some(main);

        let Ok(sub) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.note_prompt_delivered(sub);
        mux.register_round(vec![sub], None, Some(600), None, None);

        mux.poll_stranded_main();
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Idle,
            "a queued round is a wake path: main must not be nudged",
        );
        assert_eq!(mux.pending_rounds.len(), 1, "the round stays queued");
        assert!(
            mux.main_stranded_last_nudge.is_none(),
            "latch stays disarmed"
        );

        mux.shutdown();
    }

    /// With every non-main panel settled, main is idle by choice, not stranded:
    /// no nudge fires and the latch is (re-)disarmed for a future stranding.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn poll_stranded_main_silent_when_nothing_else_working() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Some(main) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.main_panel_id = Some(main);
        let Some(_sub) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        // Pre-arm the latch to prove a non-stranded poll clears it.
        mux.main_stranded_last_nudge = Some(Instant::now());

        mux.poll_stranded_main();
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Idle,
            "no working sub: main must not be nudged",
        );
        assert!(
            mux.main_stranded_last_nudge.is_none(),
            "a non-stranded poll must re-arm (clear) the latch",
        );

        mux.shutdown();
    }

    /// The cooldown: once nudged, a main that goes idle again while still
    /// stranded is not re-nudged until `STRANDED_NUDGE_COOLDOWN` elapses;
    /// after it does, the nudge fires again.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn poll_stranded_main_respects_cooldown_then_renudges() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let session_id = mux.session.id;

        let Some(main) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.main_panel_id = Some(main);
        let Ok(sub) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.note_prompt_delivered(sub);

        // First nudge fires and flips main to Working.
        mux.poll_stranded_main();
        let armed = mux.main_stranded_last_nudge;
        assert!(armed.is_some());
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
        );

        // Main returns to Idle (still stranded), but within the cooldown: no
        // second nudge — main stays Idle and the latch is unchanged.
        mux.handle_signal(TurnSignal::now(
            session_id,
            main,
            crate::signal::TurnKind::Stop,
            None,
            serde_json::Value::Null,
        ));
        mux.poll_stranded_main();
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Idle,
            "within cooldown the stranded main must not be re-nudged",
        );
        assert_eq!(
            mux.main_stranded_last_nudge, armed,
            "a suppressed nudge must not bump the latch",
        );

        // Backdate the latch past the cooldown: the nudge fires again.
        mux.main_stranded_last_nudge =
            Some(Instant::now() - STRANDED_NUDGE_COOLDOWN - Duration::from_secs(1));
        mux.poll_stranded_main();
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "past the cooldown a still-stranded main is re-nudged",
        );

        mux.shutdown();
    }

    /// A live grid-detected prompt overlays its blocked state onto an otherwise
    /// signal-derived `Working`/`Idle`, but never masks a stronger state. A menu
    /// maps to `AwaitingSelection`, a `[y/n]` prompt to `BlockedPermissionPrompt`.
    #[test]
    fn overlay_blocked_state_only_overrides_working_and_idle() {
        use DerivedState::*;
        let menu = BlockedPrompt::Selection(menu_of("Pick one", ["alpha", "beta"], 0));
        let perm = BlockedPrompt::Permission("Continue? [y/N]".into());
        // Mid-turn (Working) or back-at-prompt (Idle) + a prompt → its state.
        assert_eq!(
            Multiplexer::overlay_blocked_state(Working, Some(&menu)),
            AwaitingSelection
        );
        assert_eq!(
            Multiplexer::overlay_blocked_state(Idle, Some(&perm)),
            BlockedPermissionPrompt
        );
        // No prompt detected → unchanged.
        assert_eq!(Multiplexer::overlay_blocked_state(Working, None), Working);
        // Stronger states are never masked by a stray on-screen prompt.
        assert_eq!(
            Multiplexer::overlay_blocked_state(Exited, Some(&menu)),
            Exited
        );
        assert_eq!(
            Multiplexer::overlay_blocked_state(BlockedMergeConflict, Some(&menu)),
            BlockedMergeConflict
        );
        assert_eq!(
            Multiplexer::overlay_blocked_state(InterruptedTransport, Some(&perm)),
            InterruptedTransport
        );
    }

    /// A `Selection` blocker's signature tracks menu *content* — question +
    /// option labels — and ignores the cursor row, so navigation alone never
    /// re-announces.
    #[test]
    fn selection_signature_ignores_cursor_tracks_content() {
        let sig = |q, labels: [&str; 2], cur| {
            BlockedPrompt::Selection(menu_of(q, labels, cur)).signature()
        };
        // Same content, cursor moved → same signature.
        assert_eq!(
            sig("Pick one", ["alpha", "beta"], 0),
            sig("Pick one", ["alpha", "beta"], 1),
            "cursor movement must not change the signature"
        );
        // Changed option label → different signature.
        assert_ne!(
            sig("Pick one", ["alpha", "beta"], 0),
            sig("Pick one", ["alpha", "gamma"], 0),
            "a changed option must change the signature"
        );
        // Changed question → different signature.
        assert_ne!(
            sig("Pick one", ["alpha", "beta"], 0),
            sig("Pick another", ["alpha", "beta"], 0),
            "a changed question must change the signature"
        );
    }

    /// A `Permission` blocker's signature tracks the prompt line, and a menu and
    /// a yes/no prompt never collide — the variant tag is folded into the hash.
    #[test]
    fn permission_signature_tracks_line_and_never_collides_with_a_menu() {
        let a = BlockedPrompt::Permission("Continue? [y/N]".into()).signature();
        let b = BlockedPrompt::Permission("Overwrite? [y/N]".into()).signature();
        assert_ne!(a, b, "a changed prompt line must change the signature");

        let menu =
            BlockedPrompt::Selection(menu_of("Continue? [y/N]", ["yes", "no"], 0)).signature();
        let perm = BlockedPrompt::Permission("Continue? [y/N]".into()).signature();
        assert_ne!(menu, perm, "a menu and a yes/no prompt must not collide");
    }

    /// `panel_blocked_prompt` glues the grid rows to the scanners: a `[y/n]`
    /// prompt rendered into a live panel's grid is detected as a `Permission`
    /// blocker (the grid → `row_text` → `scan_yes_no_prompt` path end to end).
    #[tokio::test]
    async fn panel_blocked_prompt_detects_a_live_yes_no_prompt() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let pid = push_cat_panel(&mut mux, "reviewer", PanelState::Working);
        // Render a yes/no prompt into the panel's grid directly (bypassing the
        // PTY for determinism — the scanner reads whatever the grid holds).
        let panel = mux.panels.iter_mut().find(|p| p.id == pid).unwrap();
        panel
            .grid
            .advance(b"Building project...\r\nContinue? [y/N]");

        match Multiplexer::panel_blocked_prompt(panel) {
            Some(BlockedPrompt::Permission(line)) => assert!(
                line.contains("[y/N]"),
                "the detected prompt line must be the y/n line: {line:?}"
            ),
            other => panic!("expected a Permission blocker, got {other:?}"),
        }

        mux.shutdown();
    }

    /// A blocked-panel notice that fails to reach main (dead PTY) must leave the
    /// panel un-notified, so the next tick re-announces it — a failed push is
    /// not a real push. Before the fix the panel was marked notified regardless,
    /// permanently swallowing the only notice until its prompt changed.
    #[tokio::test]
    async fn blocked_notice_send_failure_leaves_panel_unnotified() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        // Main is a live Idle panel (the deliverability gate is open) whose PTY
        // is then killed so the notice send fails.
        let main = push_cat_panel(&mut mux, "main", PanelState::Idle);
        mux.main_panel_id = Some(main);

        // A round sub-panel stuck on a yes/no prompt, rendered straight into its
        // grid for determinism.
        let sub = push_cat_panel(&mut mux, "reviewer", PanelState::Working);
        mux.panels
            .iter_mut()
            .find(|p| p.id == sub)
            .unwrap()
            .grid
            .advance(b"Building...\r\nContinue? [y/N]");
        mux.register_round(vec![sub], None, Some(600), None, None);

        // Kill main's PTY: a send_keys to it now errors.
        mux.panels
            .iter_mut()
            .find(|p| p.id == main)
            .unwrap()
            .pty
            .kill()
            .unwrap();

        mux.poll_round_blocked_panels();
        assert!(
            !mux.notified_blockers.contains_key(&sub),
            "a failed notice must leave the panel un-notified so it re-announces",
        );

        mux.shutdown();
    }

    /// A round panel stuck on a selection menu the round pre-authorized to a
    /// *unique* keyword match is auto-answered by caucus — never escalated to
    /// main — and the auto-answer is deduped so a second tick on the same menu
    /// does not re-drive it.
    #[tokio::test]
    async fn poll_auto_answers_a_pre_authorized_selection_without_notifying_main() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        // Main is a live Idle panel so the notice gate is open — proving the
        // auto-answer path does not depend on it (it drives the sub-panel only).
        let main = push_cat_panel(&mut mux, "main", PanelState::Idle);
        mux.main_panel_id = Some(main);

        // A round sub-panel showing a 3-option direction menu; only option 2's
        // label matches the `structural` hint.
        let sub = push_cat_panel(&mut mux, "reviewer", PanelState::Working);
        let screen = "Which fix approach?\r\n\
❯ 1. Patch the call site\r\n\
  2. Structural fix at source\r\n\
  3. Defer for now\r\n\
Enter to select - up/down to navigate - Esc to cancel";
        mux.panels
            .iter_mut()
            .find(|p| p.id == sub)
            .unwrap()
            .grid
            .advance(screen.as_bytes());

        mux.register_round(
            vec![sub],
            None,
            Some(600),
            None,
            Some(SelectionPolicy {
                prefer: vec!["structural".to_string()],
                avoid: vec![],
            }),
        );

        mux.poll_round_blocked_panels();
        assert!(
            mux.auto_answered.contains_key(&sub),
            "the uniquely-matched menu must be auto-answered",
        );
        assert!(
            !mux.notified_blockers.contains_key(&sub),
            "an auto-answered menu must not also escalate a notice to main",
        );
        // The round records the answered fork for the delivered report.
        assert_eq!(
            mux.pending_rounds[0].auto_answers.len(),
            1,
            "the auto-answer must be recorded on the round",
        );
        assert_eq!(mux.pending_rounds[0].auto_answers[0].number, 2);
        assert!(
            mux.pending_rounds[0].auto_answers[0]
                .label
                .contains("Structural")
        );

        // Second tick on the same (unchanged) menu must not re-drive it.
        let sig = mux.auto_answered[&sub];
        mux.poll_round_blocked_panels();
        assert_eq!(
            mux.auto_answered.get(&sub),
            Some(&sig),
            "the same menu must stay deduped, not be answered again",
        );

        mux.shutdown();
    }

    /// With hints that do *not* single out one option (no match), a menu falls
    /// through to the normal notice path: main is told, nothing is auto-answered.
    #[tokio::test]
    async fn poll_escalates_a_selection_the_hints_do_not_resolve() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_cat_panel(&mut mux, "main", PanelState::Idle);
        mux.main_panel_id = Some(main);

        let sub = push_cat_panel(&mut mux, "reviewer", PanelState::Working);
        let screen = "Which fix approach?\r\n\
❯ 1. Patch the call site\r\n\
  2. Rewrite the module\r\n\
Enter to select - up/down to navigate - Esc to cancel";
        mux.panels
            .iter_mut()
            .find(|p| p.id == sub)
            .unwrap()
            .grid
            .advance(screen.as_bytes());

        // No option label contains `structural` → no unique match → escalate.
        mux.register_round(
            vec![sub],
            None,
            Some(600),
            None,
            Some(SelectionPolicy {
                prefer: vec!["structural".to_string()],
                avoid: vec![],
            }),
        );

        mux.poll_round_blocked_panels();
        assert!(
            !mux.auto_answered.contains_key(&sub),
            "a menu the hints do not resolve must not be auto-answered",
        );
        assert!(
            mux.notified_blockers.contains_key(&sub),
            "an unresolved menu must escalate to main as a notice",
        );

        mux.shutdown();
    }

    /// `blocked_prompt_notice` tailors the answer instructions to the prompt
    /// kind: `select_option` for a menu, `send_keys` + the prompt line for a
    /// yes/no prompt.
    #[test]
    fn blocked_prompt_notice_tailors_the_answer_path() {
        let pid = PanelId::new();
        let menu = BlockedPrompt::Selection(menu_of("Pick one", ["alpha", "beta"], 0));
        let menu_notice = Multiplexer::blocked_prompt_notice(pid, "reviewer", &menu);
        assert!(
            menu_notice.contains("select_option"),
            "a menu notice must point at select_option: {menu_notice}"
        );

        let perm = BlockedPrompt::Permission("Continue? [y/N]".into());
        let perm_notice = Multiplexer::blocked_prompt_notice(pid, "reviewer", &perm);
        assert!(
            perm_notice.contains("send_keys") && perm_notice.contains("Continue? [y/N]"),
            "a yes/no notice must point at send_keys and echo the prompt: {perm_notice}"
        );
    }

    /// `pick_blocker_to_notify` announces a panel's prompt once, re-announces on
    /// a content change, and stays silent while unchanged.
    #[test]
    fn pick_blocker_to_notify_announces_new_and_dedups() {
        let pid = PanelId::new();
        let sig_a = 11u64;
        let sig_b = 22u64;

        // Nothing needing the main worker → nothing to announce.
        assert_eq!(
            Multiplexer::pick_blocker_to_notify(&[], &HashMap::new()),
            None
        );

        // A prompt not yet notified → announce it.
        assert_eq!(
            Multiplexer::pick_blocker_to_notify(&[(pid, sig_a)], &HashMap::new()),
            Some(pid)
        );

        // Same prompt already notified → silent.
        let notified = HashMap::from([(pid, sig_a)]);
        assert_eq!(
            Multiplexer::pick_blocker_to_notify(&[(pid, sig_a)], &notified),
            None
        );

        // Prompt content changed under the same panel → re-announce.
        assert_eq!(
            Multiplexer::pick_blocker_to_notify(&[(pid, sig_b)], &notified),
            Some(pid)
        );
    }

    /// Build a menu from `(number, label)` pairs (cursor on the first row).
    fn test_menu(options: &[(usize, &str)]) -> crate::term::Menu {
        crate::term::Menu {
            question: "pick an approach".to_string(),
            options: options
                .iter()
                .map(|(number, label)| crate::term::MenuOption {
                    number: *number,
                    label: label.to_string(),
                })
                .collect(),
            cursor: 0,
        }
    }

    fn policy(prefer: &[&str], avoid: &[&str]) -> SelectionPolicy {
        SelectionPolicy {
            prefer: prefer.iter().map(|s| s.to_string()).collect(),
            avoid: avoid.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// A `prefer` keyword that matches exactly one option (case-insensitively)
    /// resolves to that option's displayed number.
    #[test]
    fn resolve_selection_unique_prefer_match_is_selected() {
        let menu = test_menu(&[
            (1, "Patch the call site"),
            (2, "STRUCTURAL fix at source"),
            (3, "Defer for now"),
        ]);
        assert_eq!(
            resolve_selection(&menu, &policy(&["structural"], &[])),
            Some(2)
        );
    }

    /// Two options match `prefer` → the hints did not single one out → escalate.
    #[test]
    fn resolve_selection_ambiguous_prefer_escalates() {
        let menu = test_menu(&[(1, "structural fix A"), (2, "structural fix B")]);
        assert_eq!(
            resolve_selection(&menu, &policy(&["structural"], &[])),
            None
        );
    }

    /// No option matches `prefer` → escalate.
    #[test]
    fn resolve_selection_no_prefer_match_escalates() {
        let menu = test_menu(&[(1, "option a"), (2, "option b")]);
        assert_eq!(
            resolve_selection(&menu, &policy(&["structural"], &[])),
            None
        );
    }

    /// `avoid` vetoes the only `prefer` match → escalate (caucus stays out).
    #[test]
    fn resolve_selection_avoid_vetoes_the_only_match() {
        let menu = test_menu(&[(1, "structural rewrite"), (2, "tiny patch")]);
        assert_eq!(
            resolve_selection(&menu, &policy(&["structural"], &["rewrite"])),
            None
        );
    }

    /// `push_auto_answers` emits nothing for a round that auto-answered no menu,
    /// and one line per recorded auto-answer otherwise.
    #[test]
    fn auto_answers_block_lists_each_resolved_menu() {
        let mut round = pending_round(
            vec![],
            ReadPanelMode::LastMessage,
            HashMap::new(),
            HashMap::new(),
        );

        let mut out = String::new();
        push_auto_answers(&mut out, &round);
        assert!(out.is_empty(), "no auto-answers → no block");

        let p = PanelId::new();
        round.auto_answers.push(AutoAnswerRecord {
            panel: p,
            role: "reviewer".to_string(),
            number: 2,
            label: "Structural fix at source".to_string(),
        });
        push_auto_answers(&mut out, &round);
        assert!(
            out.contains("caucus auto-answered 1 selection menu(s)"),
            "got: {out}"
        );
        assert!(
            out.contains(&format!(
                "panel {p} (role: reviewer) → option 2 \"Structural fix at source\""
            )),
            "got: {out}"
        );
    }

    /// Empty `prefer` passes every option; `avoid` narrows. Exactly one survivor
    /// is selected, two or more survivors escalate.
    #[test]
    fn resolve_selection_empty_prefer_narrows_by_avoid() {
        let menu = test_menu(&[(1, "broad refactor"), (2, "rewrite"), (3, "targeted fix")]);
        // Two vetoes leave exactly one survivor → select it.
        assert_eq!(
            resolve_selection(&menu, &policy(&[], &["broad refactor", "rewrite"])),
            Some(3)
        );
        // One veto leaves two survivors → ambiguous → escalate.
        assert_eq!(
            resolve_selection(&menu, &policy(&[], &["broad refactor"])),
            None
        );
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

    /// Registering a round writes a durable snapshot to `pending-rounds.json`
    /// (resolving panel ids to role labels) so a quit/crash before delivery can
    /// surface it on resume instead of losing it silently.
    #[tokio::test]
    async fn register_round_persists_a_durable_snapshot() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let sub = push_cat_panel(&mut mux, "reviewer", PanelState::Working);

        mux.register_round(vec![sub], None, Some(600), None, None);

        let recs = crate::session::round_record::read(&mux.session.root_dir);
        assert_eq!(recs.len(), 1, "the registered round must be persisted");
        assert_eq!(recs[0].panels.len(), 1);
        assert_eq!(
            recs[0].panels[0].role, "reviewer",
            "the panel id must be resolved to its role label"
        );

        mux.shutdown();
    }

    /// Delivering the last round removes `pending-rounds.json` — resume must see
    /// a clean state once nothing is in flight, not a stale snapshot.
    #[tokio::test]
    async fn delivering_a_round_clears_the_durable_snapshot() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_cat_panel(&mut mux, "main", PanelState::Idle);
        mux.main_panel_id = Some(main);

        // A round on a non-existent id is due immediately and delivers to the
        // idle main panel.
        mux.register_round(vec![PanelId::new()], None, Some(600), None, None);
        assert!(
            crate::session::round_record::path(&mux.session.root_dir).exists(),
            "registration writes the snapshot"
        );

        mux.poll_pending_rounds();
        assert!(
            mux.pending_rounds.is_empty(),
            "the round delivered to the idle main panel"
        );
        assert!(
            !crate::session::round_record::path(&mux.session.root_dir).exists(),
            "a delivered round must clear the durable snapshot"
        );

        mux.shutdown();
    }

    /// `ingest_resumed_rounds` reads a prior instance's persisted round, spills
    /// its captured work to `dropped-rounds.log` (preserved, not lost), clears
    /// the persisted file, and queues a notice — which `poll_resume_notice` then
    /// delivers to the idle main worker (flipping it to `Working`), one-shot.
    #[tokio::test]
    async fn resume_ingests_dropped_rounds_and_notifies_main() {
        use crate::session::round_record::{PendingRoundRecord, RoundPanelRecord};
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        // A round the prior instance left in flight when it quit/crashed.
        let recs = vec![PendingRoundRecord {
            panels: vec![RoundPanelRecord {
                role: "backend".into(),
                captured: vec!["finished analysis".into()],
                pending_backlog: 1,
            }],
            read_mode: ReadPanelMode::LastMessage,
        }];
        crate::session::round_record::write(&mux.session.root_dir, &recs).unwrap();

        mux.ingest_resumed_rounds();

        let log = std::fs::read_to_string(mux.session.root_dir.join("dropped-rounds.log")).unwrap();
        assert!(log.contains("lost to a restart"), "spill header: {log}");
        assert!(
            log.contains("finished analysis"),
            "captured work must be preserved in the log: {log}"
        );
        assert!(
            !crate::session::round_record::path(&mux.session.root_dir).exists(),
            "the persisted file must be cleared after ingest"
        );

        // The notice is held until the main worker exists and is idle, then
        // delivered exactly once.
        let main = push_cat_panel(&mut mux, "main", PanelState::Idle);
        mux.main_panel_id = Some(main);
        mux.poll_resume_notice();
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "delivering the resume notice injects a turn into the main panel",
        );

        mux.shutdown();
    }

    /// `ingest_resumed_rounds` is a no-op on a fresh launch (no persisted file):
    /// no notice is queued, so `poll_resume_notice` never injects anything.
    #[tokio::test]
    async fn resume_ingest_is_a_noop_without_a_persisted_file() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_cat_panel(&mut mux, "main", PanelState::Idle);
        mux.main_panel_id = Some(main);

        mux.ingest_resumed_rounds();
        mux.poll_resume_notice();
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Idle,
            "no persisted rounds means no notice and no injected turn",
        );

        mux.shutdown();
    }

    /// Ingest clears `pending-rounds.json` (freeing it for live rounds) but must
    /// persist the generated drop notice to `resume-notice.txt`, so a crash
    /// before the main worker is idle does not lose it. Delivery then removes the
    /// durable backup.
    #[tokio::test]
    async fn ingest_persists_the_notice_until_delivered() {
        use crate::session::round_record::{self, PendingRoundRecord, RoundPanelRecord};
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let recs = vec![PendingRoundRecord {
            panels: vec![RoundPanelRecord {
                role: "backend".into(),
                captured: vec![],
                pending_backlog: 0,
            }],
            read_mode: ReadPanelMode::LastMessage,
        }];
        round_record::write(&mux.session.root_dir, &recs).unwrap();

        mux.ingest_resumed_rounds();
        assert!(
            !round_record::path(&mux.session.root_dir).exists(),
            "pending-rounds.json is freed for the resumed session's live rounds"
        );
        assert!(
            round_record::notice_path(&mux.session.root_dir).exists(),
            "the undelivered notice must be persisted to survive a crash"
        );
        assert!(mux.resume_round_notice.is_some());

        // Deliver it: the durable backup is removed only once it lands.
        let main = push_cat_panel(&mut mux, "main", PanelState::Idle);
        mux.main_panel_id = Some(main);
        mux.poll_resume_notice();
        assert!(
            !round_record::notice_path(&mux.session.root_dir).exists(),
            "a delivered notice clears the durable backup"
        );

        mux.shutdown();
    }

    /// A notice a prior run generated but crashed before delivering survives in
    /// `resume-notice.txt`; the next resume must re-surface it (even with no new
    /// dropped rounds) so the drop is delivered at-least-once, never lost.
    #[tokio::test]
    async fn ingest_recovers_a_prior_undelivered_notice() {
        use crate::session::round_record;
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        round_record::write_notice(&mux.session.root_dir, "[caucus] dropped round X").unwrap();
        // No pending-rounds.json this run (the prior run already cleared it).

        mux.ingest_resumed_rounds();
        assert_eq!(
            mux.resume_round_notice.as_deref(),
            Some("[caucus] dropped round X"),
            "a carried-over notice must be re-surfaced even with no new dropped rounds",
        );

        let main = push_cat_panel(&mut mux, "main", PanelState::Idle);
        mux.main_panel_id = Some(main);
        mux.poll_resume_notice();
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "the recovered notice is delivered to the idle main worker",
        );
        assert!(
            !round_record::notice_path(&mux.session.root_dir).exists(),
            "delivery clears the durable notice backup",
        );

        mux.shutdown();
    }

    /// An oversized captured-turn body is head/tail truncated in the report and
    /// its full text spilled to `round-spills/`, so a `scrollback` read cannot
    /// inflate the injected paste without bound. Boundary: a body twice the cap.
    #[tokio::test]
    async fn bound_round_body_truncates_and_spills_oversized_output() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let pid = PanelId::new();
        let big = "x".repeat(MAX_ROUND_BODY_BYTES * 2);

        let bounded = mux.bound_round_body(pid, &big);
        assert!(
            bounded.len() < big.len(),
            "an oversized body must shrink: {} vs {}",
            bounded.len(),
            big.len()
        );
        assert!(
            bounded.len() <= MAX_ROUND_BODY_BYTES + 256,
            "the kept head+tail+marker must stay near the cap: {}",
            bounded.len()
        );
        assert!(bounded.contains("elided"), "must mark the elision");
        assert!(
            bounded.contains("round-spills"),
            "must point at the spill file"
        );

        // The spill file holds the *full* body — nothing is lost.
        let dir = mux.session.root_dir.join("round-spills");
        let spilled: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(spilled.len(), 1, "one spill file written");
        assert_eq!(
            std::fs::read_to_string(spilled[0].path()).unwrap().len(),
            big.len(),
            "the spill preserves the entire body"
        );

        mux.shutdown();
    }

    /// `panel_blocked_cached` reuses its result while the panel's grid
    /// generation is unchanged and recomputes once it advances. A sentinel
    /// prompt seeded into the cache (the live cat-panel grid has no prompt) lets
    /// the test tell a cache hit from a recompute: a hit returns the sentinel, a
    /// recompute on the empty grid returns `None`.
    #[tokio::test]
    async fn panel_blocked_cached_reuses_until_generation_advances() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let pid = push_cat_panel(&mut mux, "reviewer", PanelState::Working);
        let generation = mux
            .panels()
            .iter()
            .find(|p| p.id == pid)
            .unwrap()
            .grid()
            .generation();

        let sentinel = BlockedPrompt::Permission("Continue? [y/N]".into());
        mux.blocked_scan_cache
            .insert(pid, (generation, Some(sentinel.clone())));

        // Same generation → cache hit → the sentinel, no re-scan.
        assert_eq!(
            mux.panel_blocked_cached(pid, generation)
                .map(|p| p.signature()),
            Some(sentinel.signature()),
        );

        // Advanced generation → recompute → the empty grid yields None, and the
        // cache is refreshed to the new generation.
        assert!(mux.panel_blocked_cached(pid, generation + 1).is_none());
        assert_eq!(
            mux.blocked_scan_cache.get(&pid).map(|(g, _)| *g),
            Some(generation + 1),
            "the cache must track the latest generation it scanned"
        );

        mux.shutdown();
    }

    /// A killed panel's blocked-scan cache entry is pruned, so the cache cannot
    /// grow with dead-panel ids over a long session.
    #[tokio::test]
    async fn kill_panel_prunes_the_blocked_scan_cache() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let pid = push_cat_panel(&mut mux, "reviewer", PanelState::Working);
        mux.blocked_scan_cache.insert(pid, (0, None));

        mux.kill_panel(pid).unwrap();
        assert!(
            !mux.blocked_scan_cache.contains_key(&pid),
            "killing a panel must drop its blocked-scan cache entry"
        );

        mux.shutdown();
    }

    /// A body within the cap is returned verbatim and writes no spill file —
    /// the common case pays nothing.
    #[tokio::test]
    async fn bound_round_body_leaves_small_output_untouched() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let small = "hello caucus";

        assert_eq!(mux.bound_round_body(PanelId::new(), small), small);
        assert!(
            !mux.session.root_dir.join("round-spills").exists(),
            "a within-cap body must not spill"
        );

        mux.shutdown();
    }

    /// Truncation lands on UTF-8 char boundaries: a body of multi-byte scalars
    /// over the cap still yields a valid `String` (the test would panic on an
    /// invalid slice). Guards the head/tail boundary math.
    #[tokio::test]
    async fn bound_round_body_truncates_on_char_boundaries() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        // '한' is 3 bytes in UTF-8; repeat past the cap so both cut points land
        // inside multi-byte runs.
        let big = "한".repeat(MAX_ROUND_BODY_BYTES);
        let bounded = mux.bound_round_body(PanelId::new(), &big);
        assert!(bounded.contains("elided"), "oversized body must be elided");
        // Reaching here without a slice panic proves both cuts were on
        // boundaries; assert the kept text is intact Hangul, never a mojibake.
        assert!(bounded.starts_with('한'));
        mux.shutdown();
    }
}
