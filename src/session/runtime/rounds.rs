use super::*;
use crate::agent::derive_state::DerivedState;
use crate::mcp::protocol::{ControlResponse, SelectionPolicy};
use crate::mcp::{McpToolSurface, ReadPanelMode};
use crate::panel::lifecycle::{Panel, PanelState};
use crate::session::id::{PanelId, RoundId};
use std::collections::{HashMap, HashSet, VecDeque};
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

/// Most `question` notices queued for the main worker at once
/// (`Multiplexer::pending_question_notices`). Delivery drains one per tick and
/// needs an idle main, so a runaway agent posting questions in a loop would
/// otherwise grow the queue without bound; past the cap the oldest is dropped
/// with a warning — the newest question is the one still worth answering.
pub(super) const QUESTION_NOTICE_CAP: usize = 32;

/// Per-captured-turn byte budget in the **on-disk** panel report
/// ([`Multiplexer::assemble_panel_report`]). A turn body over this is head/tail
/// truncated in the report and its full text spilled to
/// `<session_root>/round-spills/`. Without it a `scrollback` read (up to 10k
/// rows × the panel width) would inflate even the spilled report file without
/// bound. (The report is no longer injected into the main PTY — the main worker
/// is handed the teaser-bounded [`Multiplexer::render_panel_summary`] that
/// points at the report file. This bound keeps the file itself readable.) Split
/// evenly between the head and tail kept around the elision.
const MAX_ROUND_BODY_BYTES: usize = 16 * 1024;

/// Per-panel teaser budget in the *injected* panel-delivery summary
/// ([`Multiplexer::render_panel_summary`]). The full report is spilled to
/// `<session_root>/rounds/<round_id>-<panel>.md` and the main worker is handed
/// only a compact summary that points at it — so the bracketed paste caucus
/// injects into the main PTY stays small (a teaser, not the whole report). The
/// panel contributes at most this many bytes of its latest output as a teaser;
/// the rest is in the report file.
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

/// Append the "caucus auto-answered" audit lines for a **single** panel — the
/// forks caucus resolved for it under the round's pre-authorized
/// [`SelectionPolicy`], so the streamed delivery shows which direction caucus
/// took on the worker's behalf without ever interrupting the main worker.
/// Streamed delivery hands the main worker one panel at a time, so it shows only
/// *that* panel's auto-answers; nothing is emitted when caucus auto-answered
/// none of its menus.
fn push_panel_auto_answers(out: &mut String, round: &PendingRound, id: PanelId) {
    let mine: Vec<&AutoAnswerRecord> = round
        .auto_answers
        .iter()
        .filter(|a| a.panel == id)
        .collect();
    if mine.is_empty() {
        return;
    }
    out.push_str(&format!(
        "caucus auto-answered {} selection menu(s) on this panel per your hints (you were not interrupted):\n",
        mine.len()
    ));
    for a in mine {
        out.push_str(&format!(
            "  → option {} \"{}\" (role: {})\n",
            a.number, a.label, a.role
        ));
    }
}

/// Append the per-panel trailing markers shared by the full report and the
/// summary: a "still working" note for a panel the fallback deadline caught
/// mid-turn, and a count of backlog tasks that never ran.
fn push_round_panel_footer(out: &mut String, c: &RoundPanelContribution) {
    if !c.settled {
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
/// One queued `question` note awaiting announcement to the main worker
/// (`Multiplexer::pending_question_notices` →
/// [`Multiplexer::poll_question_notices`]). The role is captured at enqueue —
/// the panel may be gone by delivery time.
#[derive(Debug, Clone)]
pub(super) struct QuestionNotice {
    pub(super) panel: PanelId,
    pub(super) role: String,
    pub(super) body: String,
}

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
/// the event loop watches it each tick and, as **each** panel settles (or when
/// `fallback_deadline` passes), spills that one panel's result to
/// `<session_root>/rounds/<id>-<panel>.md` and *injects* a compact summary
/// pointing at it into the main worker's panel as a fresh turn. This is the
/// caucus→main push that the pull-only MCP transport cannot do.
///
/// Delivery is **streamed per panel**, not barriered: a round no longer waits
/// for its slowest worker before waking main. Each panel is handed to main the
/// tick it settles, and the round is retired once every (still-live) panel has
/// been delivered ([`Multiplexer::round_fully_delivered`]).
pub(super) struct PendingRound {
    /// Stable identity handed back at registration ([`ControlResponse::RoundRegistered`]),
    /// the handle the main worker uses to poll ([`Multiplexer::round_status`]) or
    /// cancel ([`Multiplexer::cancel_round`]) this round. Live-only: not persisted,
    /// since a round never survives a restart (the sub-agent processes start
    /// fresh — see [`Multiplexer::ingest_resumed_rounds`]).
    id: RoundId,
    /// `true` when caucus armed this watch itself, at the instant it delivered a
    /// prompt to a sub-panel ([`Multiplexer::arm_worker_watch`]), rather than the
    /// main worker calling `register_round`.
    ///
    /// Every prompted sub-panel is watched, so **a worker can never run without a
    /// path back to main**: main sends a task and is told when that task is done,
    /// with no registration step, and the watch re-arms on the next prompt so the
    /// instruct → finish → instruct loop continues indefinitely. Before this, a
    /// round was retired on delivery and the worker went dark until main
    /// registered another one — which is what made a round feel like a barrier
    /// even after per-panel streaming landed.
    ///
    /// The flag exists so `register_round` can recognise a watch it did not
    /// create and fold it into the explicit round (carrying any already-captured
    /// work over) instead of double-watching the panel.
    implicit: bool,
    /// Panel ids in the round. Ids that no longer exist count as settled
    /// (see [`Multiplexer::round_settled`]).
    panels: Vec<PanelId>,
    /// The round's **settle latch** (Invariant I-9): a panel is inserted here
    /// by [`Multiplexer::poll_round_panels`] — the round's single owner — at
    /// the first tick it settles for this round (it left `Working`/`Spawning`
    /// with an empty backlog, or is terminal), and its finished turn is
    /// captured into `captured` in the same step. The value is the
    /// [`PanelState`] it settled *in*, which is what the delivered report
    /// renders.
    ///
    /// Presence is the latch: once a panel is here, neither its later state nor
    /// its later output can change its contribution. A panel's delivery is gated
    /// on the *main* panel being idle ([`Multiplexer::main_deliverable`]), so an
    /// arbitrary amount of time can pass between a panel settling and its
    /// delivery landing — and any path that re-opens a settled panel's turn in
    /// that window (an MCP `send_keys`, an auto-answered menu, a human Enter
    /// typed into the panel) would otherwise make
    /// [`Multiplexer::round_panel_contribution`] skip the finished panel's read
    /// and deliver it as "still working, no output captured".
    settled: HashMap<PanelId, PanelState>,
    /// The round's **per-panel delivery ledger** (Invariant I-10): a panel is
    /// inserted here at the instant its frozen contribution is handed off to the
    /// main worker — typed into an idle main ([`Multiplexer::poll_pending_rounds`]),
    /// returned to a polling main ([`Multiplexer::round_status`]), or carried on
    /// the Stop-hook reply ([`Multiplexer::take_ready_round_deliveries`]). A gone
    /// (killed) panel is inserted with no push — it has no contribution.
    ///
    /// Presence is the per-panel exactly-once latch: a delivered panel is never
    /// delivered again by any path, and the round is retired once every panel is
    /// here ([`Multiplexer::round_fully_delivered`]). This replaces the old
    /// all-panels-settled *barrier* — delivery no longer waits for the slowest
    /// worker; each panel streams to main the tick it settles.
    delivered: HashSet<PanelId>,
    /// Per-panel follow-up task queue. A round panel that goes idle with a
    /// non-empty queue is fed its next task (popped front) by
    /// [`Multiplexer::feed_round_backlog`], flipping it back to `Working` — so
    /// an early finisher keeps working its backlog instead of idling until the
    /// barrier. A panel settles for the round only once it is idle *and* its
    /// queue is empty; a panel with no entry here settles on its first idle.
    backlog: HashMap<PanelId, VecDeque<String>>,
    /// Per-panel output of every *finished* turn, in order, each read in
    /// `read_mode` by [`Multiplexer::poll_round_panels`] at the instant the
    /// panel went idle on it — a backlog turn just before its successor is fed,
    /// and the last turn as the panel is latched into `settled`.
    ///
    /// This is the round's whole record of a panel: the delivered report renders
    /// these bodies and never re-reads the panel
    /// ([`Multiplexer::round_panel_contribution`]). Reading at settle-time
    /// rather than at delivery-time is load-bearing in both directions — a
    /// `since_last_turn` read must happen before the next task reopens the turn,
    /// and a `last_message` read must happen before a *later* turn the agent
    /// starts on its own (a leftover background shell finishing wakes the CLI
    /// into a fresh turn caucus never prompted) overwrites the manifest's
    /// `last_message` with a stray "nothing new came out of it" line.
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
    /// `selection_hints`, in answer order. Rendered per panel into the streamed
    /// delivery ([`push_panel_auto_answers`]) so the main worker sees which
    /// forks were resolved on its behalf, without ever being interrupted.
    auto_answers: Vec<AutoAnswerRecord>,
}

/// One round panel's contribution, collected once and rendered into both the
/// full per-panel report ([`Multiplexer::assemble_panel_report`]) and the
/// injected summary ([`Multiplexer::render_panel_summary`]) — so the two always
/// show the *same* captured text (the per-panel `read_panel` happens exactly
/// once, in [`Multiplexer::round_panel_contribution`]).
struct RoundPanelContribution {
    /// The panel's role label.
    role: String,
    /// The state the panel **settled in** for this round, or its live state if
    /// it never settled (only reachable on a fallback delivery, where it is
    /// `Working`/`Spawning`).
    state: PanelState,
    /// Whether the panel latched for this round (Invariant I-9). `false` only on
    /// a fallback delivery that caught the panel mid-turn.
    settled: bool,
    /// Every finished-turn body, in order, as captured by
    /// [`Multiplexer::poll_round_panels`] at the instant each turn ended. A
    /// panel caught mid-turn by the fallback contributes whatever it had already
    /// finished (empty if it never finished a turn).
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
        // Fold in any implicit watch caucus already armed on these panels: the
        // explicit round supersedes it, but its latch and captured turns must
        // come along or work finished between the prompt and this call is lost.
        let (settled, captured) = self.absorb_implicit_watches(&panels);
        self.pending_rounds.push(PendingRound {
            id: round_id,
            implicit: false,
            panels,
            settled,
            delivered: HashSet::new(),
            backlog,
            captured,
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

    /// Arm a standing watch on a sub-panel caucus has just handed a prompt to,
    /// so its result finds its way back to the main worker without the main
    /// worker having registered anything.
    ///
    /// Called from [`Multiplexer::note_prompt_delivered`] — the single
    /// `Idle -> Working` owner, so every path that gives a worker work (MCP
    /// `send_keys`, `broadcast`, a backlog feed, a human typing into the panel)
    /// arms exactly once, at the moment the work becomes a fact.
    ///
    /// Three cases are skipped:
    /// - **no main panel at all** (`main_panel_id` is `None`): a watch exists to
    ///   deliver to main, so with no delivery target there is nothing to arm.
    ///   This also keeps a watch off a panel that is *about to become* main —
    ///   `spawn_main_panel` claims `main_panel_id` at spawn, before any prompt,
    ///   so main is never mistaken for a worker.
    /// - the **main panel**: main is the delivery target, not a worker.
    /// - a panel **already watched** — some live round still owes main this
    ///   panel's result. That covers both an explicit `register_round` and a
    ///   backlog feed re-prompting a panel its own round is already watching, so
    ///   a panel is never watched twice and never delivered twice.
    ///
    /// A panel whose watch already *delivered* is not "already watched": the
    /// watch is spent, and this fresh prompt arms a new one. That re-arm is what
    /// makes the loop continuous — main hears about task 2 the same way it heard
    /// about task 1, without registering anything in between.
    pub(super) fn arm_worker_watch(&mut self, panel: PanelId) {
        let Some(main) = self.main_panel_id else {
            return;
        };
        if panel == main || self.worker_watched(panel) {
            return;
        }
        let budget = self
            .config
            .settings
            .round_fallback_secs
            .clamp(1, ROUND_FALLBACK_MAX_SECS);
        self.pending_rounds.push(PendingRound {
            id: RoundId::new(),
            implicit: true,
            panels: vec![panel],
            settled: HashMap::new(),
            delivered: HashSet::new(),
            backlog: HashMap::new(),
            captured: HashMap::new(),
            read_mode: ReadPanelMode::LastMessage,
            fallback_deadline: Instant::now() + Duration::from_secs(budget),
            selection_hints: None,
            auto_answers: Vec::new(),
        });
        self.persist_pending_rounds();
    }

    /// Whether some live round still owes the main worker this panel's result —
    /// the panel is in a round that has not delivered it yet. A panel already
    /// delivered by its round reads `false`, so the next prompt re-arms it
    /// ([`Multiplexer::arm_worker_watch`]).
    ///
    /// Reads `watched_in_flight` as well as `pending_rounds`, so the answer does
    /// not depend on whether a delivery path currently holds the round out of
    /// the queue — see that field for the double-watch this closes.
    fn worker_watched(&self, panel: PanelId) -> bool {
        self.watched_in_flight.contains(&panel)
            || self
                .pending_rounds
                .iter()
                .any(|r| r.panels.contains(&panel) && !r.delivered.contains(&panel))
    }

    /// Move `pending_rounds` out for a delivery path to consume, recording the
    /// panels those rounds still owe in `watched_in_flight` so
    /// [`Multiplexer::worker_watched`] stays true for them while they are out of
    /// the queue. Paired with [`Multiplexer::end_round_poll`].
    fn take_rounds_for_poll(&mut self) -> Vec<PendingRound> {
        let rounds = std::mem::take(&mut self.pending_rounds);
        self.watched_in_flight = rounds
            .iter()
            .flat_map(|r| {
                r.panels
                    .iter()
                    .copied()
                    .filter(|id| !r.delivered.contains(id))
            })
            .collect();
        rounds
    }

    /// Clear the in-flight watch set once a delivery path has put its rounds
    /// back. Paired with [`Multiplexer::take_rounds_for_poll`].
    fn end_round_poll(&mut self) {
        self.watched_in_flight.clear();
    }

    /// Drop the implicit watches covering `panels` and return the latch and
    /// captured turns they had accumulated, keyed by panel, for the explicit
    /// round that supersedes them.
    ///
    /// In the normal flow (`broadcast` then `register_round` in one main turn)
    /// the panels are still `Working` and there is nothing to carry. This exists
    /// for the flow where a worker finished *between* the two calls: its latch
    /// and output would otherwise be dropped on the floor with the implicit
    /// watch, and the explicit round would re-read the panel at delivery time —
    /// exactly the stale read Invariant I-9 exists to prevent.
    ///
    /// Only implicit watches are absorbed. An explicit round is left alone; a
    /// panel it still owes is `worker_watched`, so it was never double-armed.
    fn absorb_implicit_watches(
        &mut self,
        panels: &[PanelId],
    ) -> (HashMap<PanelId, PanelState>, HashMap<PanelId, Vec<String>>) {
        let mut settled = HashMap::new();
        let mut captured: HashMap<PanelId, Vec<String>> = HashMap::new();
        self.pending_rounds.retain_mut(|r| {
            if !r.implicit || !r.panels.iter().any(|id| panels.contains(id)) {
                return true;
            }
            for &id in &r.panels {
                if let Some(state) = r.settled.get(&id) {
                    settled.insert(id, *state);
                }
                if let Some(bodies) = r.captured.remove(&id) {
                    captured.entry(id).or_default().extend(bodies);
                }
            }
            false
        });
        (settled, captured)
    }

    /// Check on a registered round by id — and hand back every panel that has
    /// settled since the last poll, streamed. This is the *pull* half of round
    /// delivery (Invariant I-10, per panel).
    ///
    /// Every [`Multiplexer::panel_ready`] panel is delivered right here: rendered,
    /// marked `delivered`, and returned in the response, followed by the live
    /// status of the panels still working (state, remaining backlog, seconds
    /// left on the fallback deadline). The round is removed only once all its
    /// panels are delivered ([`Multiplexer::round_fully_delivered`]); otherwise
    /// it stays pending and a later poll (or the push) streams the rest — a main
    /// worker polling in a loop hears each worker the tick it finishes, never
    /// waiting for the slowest.
    ///
    /// The pull half exists because the *push* half
    /// ([`Multiplexer::poll_pending_rounds`]) types into the main panel, so it
    /// can only land while that panel is `Idle` — i.e. only after the main
    /// worker ends its turn. A main worker that instead polls this from inside
    /// its turn is `Working` for the whole poll, so the push can never fire:
    /// main waits for the round, and the round waits for main to stop waiting.
    /// Handing ready panels to the poller breaks that livelock by construction,
    /// and marking them delivered here (so no later push re-delivers them) is
    /// what keeps each panel exactly-once.
    ///
    /// The round's single owner ([`Multiplexer::poll_round_panels`]) is run
    /// first, so both the readiness and the status see the *current* settle
    /// latch. Without it they would lag the panels by a tick — the event loop
    /// drains control requests before it polls rounds.
    ///
    /// Round ids are live-only (not persisted across a restart), so a status
    /// poll only ever resolves rounds the current caucus instance is watching. A
    /// round already fully delivered — by any path — is an unknown id.
    pub(crate) fn round_status(&mut self, round_id: RoundId) -> ControlResponse {
        let Some(idx) = self.pending_rounds.iter().position(|r| r.id == round_id) else {
            return ControlResponse::error(format!(
                "no live round {round_id}: it never existed, already delivered, or was cancelled"
            ));
        };
        // Take the round out so the owner poll can borrow `self`; it goes back
        // at `idx` unless every panel is delivered here. Its undelivered panels
        // stay `worker_watched` while it is out (see `watched_in_flight`) — the
        // backlog feed below re-prompts them from inside the poll.
        let mut round = self.pending_rounds.remove(idx);
        self.watched_in_flight = round
            .panels
            .iter()
            .copied()
            .filter(|id| !round.delivered.contains(id))
            .collect();
        let fallback_due = Instant::now() >= round.fallback_deadline;
        self.poll_round_panels(&mut round, fallback_due);
        self.retire_gone_round_panels(&mut round);

        // Stream every panel ready now (one pull return is one continuing main
        // turn, so batch them like the hook reply). Each is marked delivered so
        // no later push re-delivers it.
        let mut parts: Vec<String> = Vec::new();
        for id in self.ready_round_panels(&round, fallback_due) {
            if let Some(summary) = self.render_panel_delivery(&round, id) {
                parts.push(summary);
            }
            round.delivered.insert(id);
        }

        if Self::round_fully_delivered(&round) {
            // Round retired: it is already out of `pending_rounds`; persist so
            // the durable snapshot records it gone and no push revisits it.
            self.end_round_poll();
            self.persist_pending_rounds();
            let text = if parts.is_empty() {
                format!("Round {round_id} complete — every panel already delivered.")
            } else {
                parts.join("\n\n")
            };
            return ControlResponse::Panel { text };
        }

        // Panels still outstanding: return whatever streamed now, then the live
        // status of the rest, and keep the round pending.
        let status = self.render_round_status(&round);
        self.pending_rounds.insert(idx, round);
        self.end_round_poll();
        self.persist_pending_rounds();
        let text = if parts.is_empty() {
            status
        } else {
            format!("{}\n\n{status}", parts.join("\n\n"))
        };
        ControlResponse::Panel { text }
    }

    /// Render a round's per-panel status line-by-line. Each panel's status comes
    /// from the round's ledgers, not its live state: a panel already streamed to
    /// main reads `delivered`; one that settled but is awaiting the
    /// deliverability gate reads `settled` (and was re-woken after — a leftover
    /// background shell, an answered menu — still reads `settled`, exactly as
    /// its frozen contribution will render).
    fn render_round_status(&self, round: &PendingRound) -> String {
        let remaining = round
            .fallback_deadline
            .saturating_duration_since(Instant::now())
            .as_secs();
        let mut out = format!(
            "Round {}: {} panel(s), {remaining}s until the fallback deadline\n",
            round.id,
            round.panels.len()
        );
        for &id in &round.panels {
            let backlog = round.backlog.get(&id).map(VecDeque::len).unwrap_or(0);
            let (role, status) = match self.panels.iter().find(|p| p.id == id) {
                _ if round.delivered.contains(&id) => (
                    self.panels
                        .iter()
                        .find(|p| p.id == id)
                        .map(|p| p.role.as_str())
                        .unwrap_or("(gone)"),
                    "delivered",
                ),
                None => ("(gone)", "gone"),
                Some(p) if round.settled.contains_key(&id) => (p.role.as_str(), "settled"),
                // Not latched: either mid-turn, or idle with queued work left
                // (the owner poll feeds it its next task).
                Some(p) if backlog > 0 && p.state() == PanelState::Idle => {
                    (p.role.as_str(), "draining backlog")
                }
                Some(p) => (p.role.as_str(), "working"),
            };
            out.push_str(&format!(
                "  - {role} ({id}): {status}, {backlog} backlog task(s) remaining\n"
            ));
        }
        out
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

    /// Stream one ready round panel to the main worker — the caucus→main push.
    /// Called once per event-loop tick, after signals + pump have updated panel
    /// state.
    ///
    /// Delivery is **per panel**, not barriered: a panel is *ready*
    /// ([`Multiplexer::panel_ready`]) the tick it settles (or when the
    /// `fallback_deadline` forces the round out while it is still working). Its
    /// frozen contribution is *delivered* only while the main panel exists, is
    /// `Idle`, and has no un-submitted human keystroke within `COMPOSE_GRACE` —
    /// so an injected turn never collides with a line the user is composing. At
    /// most one panel is delivered per tick across all rounds: the injection
    /// flips the main panel to `Working`, so every other ready panel naturally
    /// holds until the main worker is idle again. A round is retired once all
    /// its panels are delivered ([`Multiplexer::round_fully_delivered`]) — main
    /// never waits for the slowest worker before hearing from the fast ones.
    ///
    /// A ready panel whose main worker is *gone* — no main panel id, the panel
    /// no longer exists, or it has `Exited` (the agent process died on its own;
    /// only `kill_panel` clears the id, so a self-exit leaves `main_panel_id`
    /// pointing at an `Exited` panel) — has its contribution spilled to
    /// `dropped-rounds.log` and is marked delivered: no caucus→main push can
    /// ever land, so re-queuing would spin forever, and discarding silently
    /// would lose the sub-agent's work. A delivery that *fails* (e.g. the main
    /// PTY writer went away mid-tick) leaves the panel undelivered so a later
    /// tick retries rather than dropping it.
    ///
    /// [`Multiplexer::poll_round_panels`] runs first each round: it latches
    /// every panel that settled this tick (capturing its finished turn) and,
    /// while there is still time, hands an early finisher its next queued task —
    /// so a backlog panel keeps working its queue and only becomes ready once
    /// the queue drains. Once the fallback deadline has passed, no new backlog
    /// work starts.
    pub fn poll_pending_rounds(&mut self) {
        if self.pending_rounds.is_empty() {
            return;
        }
        let now = Instant::now();
        // Take the queue so the settle-checks below can borrow `self`.
        let rounds = self.take_rounds_for_poll();

        // Resolve main's liveness once. Main is *gone* — no caucus→main push
        // can ever land — when there is no main panel id, the panel no longer
        // exists, or it has `Exited`. A ready panel against a gone main is
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

        // At most one panel is pushed per tick (each push flips main to
        // `Working`). Once set, later rounds only latch/retire and re-queue.
        let mut pushed = false;
        // Whether the pending-round set changed this tick (a panel delivered,
        // retired, or had backlog fed) — only then is the durable snapshot
        // re-written, so the common idle tick does no I/O.
        let mut changed = false;
        for mut round in rounds {
            let fallback_due = now >= round.fallback_deadline;
            changed |= self.poll_round_panels(&mut round, fallback_due);
            // Gone panels carry no contribution: retire them so they never hold
            // the round open, before any main-state gate.
            changed |= self.retire_gone_round_panels(&mut round);

            if main_gone {
                // No wake path will ever exist. Spill each ready panel's frozen
                // contribution so the sub-agents' work is not silently lost,
                // then mark it delivered.
                for id in self.ready_round_panels(&round, fallback_due) {
                    if let Some(report) = self.render_panel_delivery(&round, id) {
                        self.record_dropped_round(&report);
                    }
                    round.delivered.insert(id);
                    changed = true;
                }
            } else if deliverable && !pushed {
                // Deliver the first ready panel; the rest hold for later ticks.
                if let Some(id) = self
                    .ready_round_panels(&round, fallback_due)
                    .first()
                    .copied()
                    && let Some(summary) = self.render_panel_delivery(&round, id)
                {
                    // `deliverable` implies a live, idle main panel exists.
                    let mid = main_id.expect("deliverable implies a main panel");
                    match McpToolSurface::send_keys(self, mid, &summary, true) {
                        // Mark delivered only on a confirmed send (mirrors the
                        // backlog feed): a failed push leaves the panel ready so
                        // the next tick retries, never dropping its result.
                        Ok(()) => {
                            round.delivered.insert(id);
                            pushed = true;
                            changed = true;
                        }
                        Err(err) => {
                            warn!(error = %err, panel = %id, "round panel delivery to main failed; will retry");
                        }
                    }
                }
            }

            // Retire the round once every panel has landed; otherwise keep
            // watching (more panels to settle, or the gate was closed this tick).
            if !Self::round_fully_delivered(&round) {
                self.pending_rounds.push(round);
            } else {
                changed = true;
            }
        }
        self.end_round_poll();
        if changed {
            self.persist_pending_rounds();
        }
    }

    /// Take every round panel ready to deliver right now, per panel, returning
    /// one compact summary each — the hook-reply half of round delivery
    /// (`docs/design.md` §7.6), called by
    /// [`Multiplexer::handle_signal_with_reply`] when the main worker's own Stop
    /// hook is waiting for a directive.
    ///
    /// The main worker's Stop hook is one *continuing* turn, so — like the
    /// question-notice queue that rides the same reply — every ready panel is
    /// carried at once rather than one round per turn: a panel held back would
    /// wait an entire further main turn for the push path, which is the latency
    /// this delivery exists to remove. Each round's latch is refreshed first
    /// ([`Multiplexer::poll_round_panels`], Invariant I-9 — the judgement must
    /// see panels that settled this very tick), gone panels are retired, then
    /// every [`Multiplexer::panel_ready`] panel is rendered and **marked
    /// delivered** (Invariant I-10, per panel). A round with all panels
    /// delivered is retired; the rest stay pending, exactly as the push leaves
    /// them.
    ///
    /// The caller lands the returned texts (hook reply → keystroke fallback →
    /// `dropped-rounds.log`), so marking delivered as they are taken keeps each
    /// panel exactly-once even if the reply channel then dies.
    pub(super) fn take_ready_round_deliveries(&mut self) -> Vec<String> {
        if self.pending_rounds.is_empty() {
            return Vec::new();
        }
        let now = Instant::now();
        let mut changed = false;
        let mut out = Vec::new();
        let rounds = self.take_rounds_for_poll();
        for mut round in rounds {
            let fallback_due = now >= round.fallback_deadline;
            changed |= self.poll_round_panels(&mut round, fallback_due);
            changed |= self.retire_gone_round_panels(&mut round);
            for id in self.ready_round_panels(&round, fallback_due) {
                if let Some(summary) = self.render_panel_delivery(&round, id) {
                    out.push(summary);
                }
                round.delivered.insert(id);
                changed = true;
            }
            if !Self::round_fully_delivered(&round) {
                self.pending_rounds.push(round);
            }
        }
        self.end_round_poll();
        if changed {
            self.persist_pending_rounds();
        }
        out
    }

    /// Whether round panel `id` is **ready to deliver** this tick: its
    /// contribution is frozen and it has not yet been delivered. A panel is
    /// ready once it settled (latched by [`Multiplexer::poll_round_panels`], the
    /// normal path), or the `fallback_deadline` forced the round out while it
    /// was still working (delivered with a "still working" marker), or it is
    /// gone (killed — ready, but with no contribution to render).
    ///
    /// This is the per-panel replacement for the old round-level barrier
    /// (`round_settled`): a round no longer waits for its slowest worker; each
    /// ready panel is streamed to main independently.
    fn panel_ready(&self, round: &PendingRound, id: PanelId, fallback_due: bool) -> bool {
        if round.delivered.contains(&id) {
            return false;
        }
        let gone = !self.panels.iter().any(|p| p.id == id);
        gone || fallback_due || round.settled.contains_key(&id)
    }

    /// Whether every panel in the round has been delivered — the retirement
    /// condition that replaces the all-settled barrier. A gone panel counts only
    /// once it has been marked `delivered` (its silent retirement); until then
    /// the round stays pending. The single reader of `round.delivered` used to
    /// decide a round is done.
    fn round_fully_delivered(round: &PendingRound) -> bool {
        round.panels.iter().all(|id| round.delivered.contains(id))
    }

    /// Mark every round panel that can never be delivered as delivered, so it
    /// does not hold the round open. Returns `true` if any panel was newly
    /// retired, so the caller re-persists. Called at the top of each delivery
    /// path after [`Multiplexer::poll_round_panels`] — the one place all three
    /// paths share, so a panel that cannot deliver is retired uniformly.
    ///
    /// Two kinds:
    /// - **gone** (killed): no contribution to render and no live worker left
    ///   (mirrors the old `round_settled` counting a missing panel as settled).
    /// - **the main panel itself**: delivery types *into* main, so watching main
    ///   would feed its own output back to it. [`Multiplexer::arm_worker_watch`]
    ///   already refuses to arm main, but that check reads `main_panel_id` at arm
    ///   time; retiring here closes the case where a panel *becomes* main after a
    ///   watch was armed on it, without depending on assignment order.
    fn retire_gone_round_panels(&self, round: &mut PendingRound) -> bool {
        let mut changed = false;
        for &id in &round.panels {
            if round.delivered.contains(&id) {
                continue;
            }
            let gone = !self.panels.iter().any(|p| p.id == id);
            if gone || Some(id) == self.main_panel_id {
                round.delivered.insert(id);
                changed = true;
            }
        }
        changed
    }

    /// The panels in `round` that are [`Multiplexer::panel_ready`] this tick, in
    /// round order — the ones a delivery path may hand to main now. Gone panels
    /// are already retired by [`Multiplexer::retire_gone_round_panels`], so this
    /// only ever returns panels with a real contribution to render.
    fn ready_round_panels(&self, round: &PendingRound, fallback_due: bool) -> Vec<PanelId> {
        round
            .panels
            .iter()
            .copied()
            .filter(|&id| self.panel_ready(round, id, fallback_due))
            .collect()
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
    /// deliver (main gone, lost to a restart, or a hook reply lost with its
    /// keystroke fallback failing too). Best-effort: a write failure is
    /// logged, not propagated.
    pub(super) fn append_dropped_round(&self, header: &str, body: &str) {
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
                    // A delivered panel's work already reached main — do not
                    // record it as dropped, or a restart would tell the resumed
                    // main worker to re-issue work it already has.
                    .filter(|id| !r.delivered.contains(id))
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

    /// Whether every panel in `round` has **latched** (Invariant I-9) — i.e.
    /// [`Self::poll_round_panels`] has seen it settle for this round and
    /// captured its finished turn. A missing panel counts as settled: there is
    /// no live worker left to feed, even if a stale backlog entry exists for
    /// that id.
    ///
    /// Test-only: streaming delivery no longer gates on the whole round settling
    /// ([`Self::panel_ready`] drives delivery per panel now), but the latch
    /// tests still assert the I-9 completeness this expresses.
    #[cfg(test)]
    fn round_settled(&self, round: &PendingRound) -> bool {
        round
            .panels
            .iter()
            .all(|id| round.settled.contains_key(id) || !self.panels.iter().any(|p| p.id == *id))
    }

    /// Re-key a panel's live round membership after [`Multiplexer::restart_panel`]
    /// swaps its `PanelId` (a fresh PTY is a fresh id). The restarted panel is
    /// the *same* logical round member — same role, same resumed conversation,
    /// same worktree — so every pending round that referenced `old` must now
    /// reference `new` across all five id-keyed fields (`panels`, `settled`,
    /// `delivered`, `backlog`, `captured`). Without this the round resolves
    /// `old` to a missing panel and silently drops the member — retiring it
    /// (gone) or losing its contribution. `restart_panel` is the only id-changing
    /// path: every other spawn allocates a genuinely new panel, and `kill_panel`
    /// detaches permanently with no replacement.
    pub(super) fn remap_round_membership(&mut self, old: PanelId, new: PanelId) {
        for round in &mut self.pending_rounds {
            for id in &mut round.panels {
                if *id == old {
                    *id = new;
                }
            }
            if let Some(state) = round.settled.remove(&old) {
                round.settled.insert(new, state);
            }
            if round.delivered.remove(&old) {
                round.delivered.insert(new);
            }
            if let Some(backlog) = round.backlog.remove(&old) {
                round.backlog.insert(new, backlog);
            }
            if let Some(captured) = round.captured.remove(&old) {
                round.captured.insert(new, captured);
            }
        }
    }

    /// The round's **single owner** (Invariants I-9, I-10): the only writer of
    /// `round.settled` and `round.captured`. Called once per round per tick from
    /// [`Multiplexer::poll_pending_rounds`] before the due-check, and again from
    /// [`Multiplexer::round_status`] so a poll never lags the panels by a tick.
    ///
    /// For each panel not yet latched, in one pass:
    ///
    /// - `Working`/`Spawning` — still mid-turn, nothing to do. A panel stopped on
    ///   a selection menu is here for free: a chooser fires no `Stop` hook, so it
    ///   stays coarse `Working` and [`Multiplexer::poll_round_blocked_panels`]
    ///   routes it to the main worker instead.
    /// - `Idle` with queued backlog, before the deadline — **feed**: read the
    ///   finished turn, then send the next task, which flips the panel back to
    ///   `Working`. Not latched: the panel has more of this round's work to do.
    /// - otherwise (an empty backlog, a terminal `Exited`, or the fallback
    ///   deadline having closed the feeding window) — **latch**: read the
    ///   finished turn and record the panel in `round.settled`. Its contribution
    ///   to this round is now frozen.
    ///
    /// Those two arms are exhaustive: a panel that is neither `Working` nor
    /// `Spawning` is `Idle` or `Exited`, and there is no third disposition. The
    /// coarse state machine has no `Blocked` — see [`PanelState`].
    ///
    /// Every read happens at the instant the panel is observed idle, never at
    /// delivery time — see `PendingRound::captured` for why that is load-bearing
    /// for both `since_last_turn` and `last_message`. A read and the state change
    /// it belongs to are committed together: a failed send leaves the task at the
    /// front of the queue and captures nothing, so the next tick re-reads and
    /// retries rather than silently dropping the task.
    ///
    /// Returns `true` if the round's persistable state changed (a panel latched
    /// or a task was fed), so the caller re-writes the durable snapshot.
    fn poll_round_panels(&mut self, round: &mut PendingRound, fallback_due: bool) -> bool {
        /// What the owner decided to do with one panel this tick.
        enum Act {
            /// Send this task, then record the finished turn.
            Feed(String),
            /// Record the finished turn and freeze the panel's contribution.
            Latch(PanelState),
        }
        // Decide everything first (borrows only `round` + reads `self.panels`),
        // then act (mut-borrows `self`), so the two borrows never overlap. A
        // backlog front is cloned, not popped, here — it is consumed only on a
        // confirmed send below.
        let mut acts: Vec<(PanelId, String, Act)> = Vec::new();
        for &id in &round.panels {
            // Latched: its contribution is frozen, whatever it is doing now.
            if round.settled.contains_key(&id) {
                continue;
            }
            // Gone (killed): nothing to latch or feed. `round_settled` counts a
            // missing panel as settled — there is no live worker left.
            let Some(state) = self.panels.iter().find(|p| p.id == id).map(Panel::state) else {
                continue;
            };
            if matches!(state, PanelState::Working | PanelState::Spawning) {
                continue;
            }
            let backlog_empty = round.backlog.get(&id).is_none_or(VecDeque::is_empty);
            // `state` is `Idle` or `Exited` here, so this is exhaustive: an idle
            // panel with work left and time on the clock gets fed; everything
            // else settles.
            let act = if state == PanelState::Idle && !backlog_empty && !fallback_due {
                // The queue's front is cloned; popped only on a confirmed send.
                let task = round.backlog[&id]
                    .front()
                    .expect("backlog is non-empty")
                    .clone();
                Act::Feed(task)
            } else {
                Act::Latch(state)
            };
            // Read the just-finished turn *now*, while the panel is still idle.
            let done = self
                .read_panel(id, round.read_mode)
                .unwrap_or_else(|e| format!("(could not read panel: {e})"));
            acts.push((id, done, act));
        }

        let mut changed = false;
        for (id, done, act) in acts {
            match act {
                Act::Latch(state) => {
                    round.settled.insert(id, state);
                    round.captured.entry(id).or_default().push(done);
                    changed = true;
                }
                Act::Feed(task) => match McpToolSurface::send_keys(self, id, &task, true) {
                    // Delivered: consume the task (still the queue's front —
                    // single tick, nothing else mutates the queue in between)
                    // and record the finished turn, so the report carries every
                    // task's result and not only the last.
                    Ok(()) => {
                        round.backlog.get_mut(&id).and_then(VecDeque::pop_front);
                        round.captured.entry(id).or_default().push(done);
                        changed = true;
                    }
                    // Delivery failed: leave the task at the front and capture
                    // nothing; the panel is still idle, so the next tick re-reads
                    // and retries.
                    Err(err) => warn!(error = %err, panel = %id, "round backlog feed failed"),
                },
            }
        }
        changed
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
        main_idle && self.main_compose_quiet()
    }

    /// Whether no un-submitted human keystroke is holding the main panel
    /// (`COMPOSE_GRACE` since the last one). The compose half of
    /// [`Multiplexer::main_deliverable`], split out because the hook-reply
    /// path ([`Multiplexer::handle_signal_with_reply`]) must apply the same
    /// "no automated delivery while the human is composing" rule at a moment
    /// when the main panel is still coarse `Working` — the idle half of the
    /// gate cannot hold there, but the human-first rule holds everywhere.
    pub(super) fn main_compose_quiet(&self) -> bool {
        self.main_compose_since
            .is_none_or(|t| Instant::now().duration_since(t) >= COMPOSE_GRACE)
    }

    /// Nudge the main worker when it has gone idle while sub-panels still run
    /// but **nothing caucus watches** will ever wake it — the stranded-main
    /// safety net. Called once per event-loop tick, after round delivery.
    ///
    /// Most of what this used to cover is now impossible by construction: every
    /// panel caucus hands a prompt is watched from that instant
    /// ([`Multiplexer::arm_worker_watch`]), so a worker main gave work to always
    /// has a path back. What remains reachable is a panel that reached
    /// `Working`/`Spawning` **without caucus delivering it a prompt** — spawned
    /// and left un-tasked while main ended its turn, or an agent CLI that opened
    /// a turn on its own (a leftover background shell finishing). Nothing owes
    /// main that panel's result, so without this main sits idle indefinitely.
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
            "[caucus] You are idle, and these panels are busy with work caucus \
             never handed them, so nothing will report back to you about them:\n",
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
            "caucus watches every panel it delivers a prompt to and re-prompts \
             you the moment that panel finishes — but these were never given a \
             task through caucus (spawned and left un-tasked, or the agent \
             started a turn on its own). Send them their task with send_keys or \
             broadcast and end your turn; you will be woken per panel as each \
             finishes. Use read_panel or kill_panel if they should not be \
             running at all.",
        );
        match McpToolSurface::send_keys(self, main_id, &notice, true) {
            // Arm the cooldown only on a real push. A failed delivery must not
            // start the cooldown, or the next tick would wait it out on a nudge
            // that never landed and main stays stranded longer.
            Ok(()) => self.main_stranded_last_nudge = Some(Instant::now()),
            Err(err) => warn!(error = %err, "stranded-main nudge to main panel failed"),
        }
    }

    /// Queue a `question` note for announcement to the main worker
    /// ([`Multiplexer::poll_question_notices`]). The role is captured now —
    /// the panel may be gone by delivery time, and the question can still be
    /// worth answering (it may explain *why* the panel died).
    pub(super) fn enqueue_question_notice(&mut self, panel: PanelId, role: String, body: String) {
        if self.pending_question_notices.len() >= QUESTION_NOTICE_CAP {
            let dropped = self.pending_question_notices.pop_front();
            warn!(
                panel = %panel,
                dropped = ?dropped.map(|n| n.panel),
                "question-notice queue full; dropped the oldest notice"
            );
        }
        self.pending_question_notices
            .push_back(QuestionNotice { panel, role, body });
    }

    /// Announce queued `question` notes to the main worker, one per tick,
    /// under the same deliverability gate as every other caucus→main push
    /// ([`Multiplexer::main_deliverable`]). Called once per event-loop tick,
    /// after round delivery — a finished round is the primary deliverable.
    ///
    /// A failed injection keeps the notice queued so a later tick retries
    /// (mirroring round delivery). If the main worker is gone — no main panel,
    /// or it `Exited` — the queue is dropped with a warning: no wake path will
    /// ever exist, exactly the round-delivery rule.
    pub fn poll_question_notices(&mut self) {
        if self.pending_question_notices.is_empty() {
            return;
        }
        let main_live = self.main_panel_id.is_some_and(|id| {
            self.panels
                .iter()
                .find(|p| p.id == id)
                .is_some_and(|p| p.state() != PanelState::Exited)
        });
        if !main_live {
            warn!(
                dropped = self.pending_question_notices.len(),
                "main worker gone; dropping queued question notices"
            );
            self.pending_question_notices.clear();
            return;
        }
        if !self.main_deliverable() {
            return;
        }
        let Some(notice) = self.pending_question_notices.front() else {
            return;
        };
        let text = self.question_notice(notice);
        // `main_live` + `main_deliverable` imply the main panel exists.
        let main_id = self
            .main_panel_id
            .expect("deliverable implies a main panel");
        match McpToolSurface::send_keys(self, main_id, &text, true) {
            Ok(()) => {
                self.pending_question_notices.pop_front();
            }
            Err(err) => {
                warn!(error = %err, "question notice delivery to main panel failed; will retry")
            }
        }
    }

    /// Drain every queued question notice, rendered for delivery — the
    /// hook-reply counterpart of [`Multiplexer::poll_question_notices`]
    /// (`docs/design.md` §7.6). The push path delivers one notice per tick
    /// because each push opens its own main turn; the hook reply is one
    /// continuing turn, so it batches the whole queue — a notice held back
    /// here would wait an entire further main turn, which is exactly the
    /// latency this path exists to remove. Caller (the hook-delivery block in
    /// `handle_signal_with_reply`) guarantees the texts are delivered or fall
    /// back to a keystroke, mirroring the round summary.
    pub(super) fn take_question_notice_texts(&mut self) -> Vec<String> {
        let notices = std::mem::take(&mut self.pending_question_notices);
        notices.iter().map(|n| self.question_notice(n)).collect()
    }

    /// Render one question notice for injection into the main panel.
    ///
    /// The guidance clause reports the questioning panel's state **as of
    /// delivery**, not as of the note's arrival. A notice can wait behind a
    /// round push for a whole main turn (both share the one-push-per-tick
    /// gate), and the panel may settle or exit in that window — an
    /// enqueue-time "still mid-turn, it queues input" claim would then send
    /// the main worker to open a spurious new turn on an idle panel, or to
    /// answer into a dead one. The notice itself is never suppressed: whether
    /// the question is already resolved is knowledge caucus does not have
    /// (the main worker may have pre-answered from `last_note`), and a
    /// question from a panel that died may explain why it died.
    fn question_notice(&self, notice: &QuestionNotice) -> String {
        let state = self
            .panels
            .iter()
            .find(|p| p.id == notice.panel)
            .map(|p| p.state());
        let guidance = match state {
            Some(PanelState::Working) | Some(PanelState::Spawning) => format!(
                "(answer with send_keys({}, \"<your answer>\"); the panel is \
                 still mid-turn — its agent queues input that arrives while it \
                 works.)",
                notice.panel
            ),
            Some(PanelState::Idle) => format!(
                "(the panel has since ended its turn — check its result first \
                 (read_panel / list_panels last_note); it may have moved past \
                 this question. Answering with send_keys({}, ...) starts a new \
                 turn.)",
                notice.panel
            ),
            Some(PanelState::Exited) | None => "(the panel has since exited — there is no \
                 turn to answer into; the question may explain why it stopped.)"
                .to_string(),
        };
        format!(
            "[caucus] panel {} (role: {}) asks:\n{}\n{}",
            notice.panel, notice.role, notice.body, guidance
        )
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

    /// Collect one round panel's contribution — its role, the state it settled
    /// in, its finished-turn bodies, and its un-run backlog count — for the
    /// report and summary to render. Returns `None` when the panel id is gone
    /// (killed).
    ///
    /// Reads nothing from the live panel but its role: every body was captured
    /// by [`Multiplexer::poll_round_panels`] at the instant the turn ended
    /// (Invariant I-9). Re-reading here would hand the round whatever the panel
    /// happens to hold at delivery time, which is a different thing — delivery
    /// waits for the *main* panel to go idle, and in that window a settled panel
    /// can be re-woken (a leftover background shell finishing wakes the CLI into
    /// a fresh turn), overwriting the `last_message` this round is reporting and
    /// flipping the panel back to `Working` so the old code skipped its read
    /// entirely and delivered it as "still working, no output captured".
    fn round_panel_contribution(
        &self,
        round: &PendingRound,
        id: PanelId,
    ) -> Option<RoundPanelContribution> {
        let panel = self.panels.iter().find(|p| p.id == id)?;
        let settled = round.settled.get(&id);
        Some(RoundPanelContribution {
            role: panel.role.clone(),
            // A panel with no latch was caught mid-turn by the fallback; its
            // live state (`Working`/`Spawning`) is what the report should say.
            state: settled.copied().unwrap_or_else(|| panel.state()),
            settled: settled.is_some(),
            bodies: round.captured.get(&id).cloned().unwrap_or_default(),
            pending_backlog: round.backlog.get(&id).map(VecDeque::len).unwrap_or(0),
        })
    }

    /// Render one panel's **full** block — role + settled state, every finished
    /// turn (under `### task N` headers when it ran more than one), and the
    /// trailing footer — into `out`. The shared body of the per-panel delivery
    /// report ([`Multiplexer::assemble_panel_report`]), used both for a live
    /// delivery and for the main-gone spill to `dropped-rounds.log`.
    fn push_panel_report_block(&self, out: &mut String, id: PanelId, c: &RoundPanelContribution) {
        out.push_str(&format!(
            "\n## panel {id} (role: {}) — {}\n",
            c.role,
            c.state.label()
        ));
        // One output stays header-less, identical to the pre-backlog report;
        // two or more get `### task N` headers.
        let total = c.bodies.len();
        for (i, body) in c.bodies.iter().enumerate() {
            if total > 1 {
                out.push_str(&format!("\n### task {}\n", i + 1));
            }
            let body = body.trim();
            if body.is_empty() {
                out.push_str("(no output captured)\n");
            } else {
                // Bound each turn's body so a `scrollback` read cannot inflate
                // even the on-disk report without limit; the full text is
                // spilled to `round-spills/` and pointed at.
                out.push_str(&self.bound_round_body(id, body));
                out.push('\n');
            }
        }
        push_round_panel_footer(out, c);
    }

    /// Render one panel's **compact** teaser block — role + state + output
    /// count, then its latest output trimmed to the teaser budget, then the
    /// footer — into `out`. The shared per-panel body of the per-panel delivery
    /// summary ([`Multiplexer::render_panel_summary`]).
    fn push_panel_summary_block(&self, out: &mut String, id: PanelId, c: &RoundPanelContribution) {
        out.push_str(&format!(
            "\n## panel {id} (role: {}) — {} · {} output(s)\n",
            c.role,
            c.state.label(),
            c.bodies.len()
        ));
        // Teaser: the panel's latest output, trimmed to the teaser budget — the
        // full text is in the report file.
        let latest = c.bodies.last().map(|b| b.trim()).unwrap_or("");
        if latest.is_empty() {
            out.push_str("(no output captured)\n");
        } else {
            out.push_str(&summary_teaser(latest, ROUND_SUMMARY_TEASER_BYTES));
            out.push('\n');
        }
        push_round_panel_footer(out, c);
    }

    /// The opening line of a single panel's streamed delivery: which round it
    /// belongs to, which panel + role, and whether it settled on its own or was
    /// forced out mid-turn by the fallback deadline. The per-panel analogue of
    /// [`round_header`].
    fn panel_delivery_header(round_id: RoundId, id: PanelId, role: &str, settled: bool) -> String {
        format!(
            "[caucus] Round {round_id} — panel {id} (role: {role}) {}.\n",
            if settled {
                "finished"
            } else {
                "forced out by the fallback deadline (still working)"
            }
        )
    }

    /// Assemble one settled panel's **full** delivery report. Spilled to
    /// `<session_root>/rounds/<round>-<panel>.md` and pointed at by the compact
    /// summary the main worker is handed. Any selection menus caucus
    /// auto-answered *on this panel* are noted, so the delivered result still
    /// shows the fork caucus took on the worker's behalf.
    fn assemble_panel_report(
        &self,
        round: &PendingRound,
        id: PanelId,
        c: &RoundPanelContribution,
    ) -> String {
        let mut out = Self::panel_delivery_header(round.id, id, &c.role, c.settled);
        push_panel_auto_answers(&mut out, round, id);
        self.push_panel_report_block(&mut out, id, c);
        out
    }

    /// Assemble the **compact** streamed delivery for one settled panel: the
    /// per-panel header, this panel's auto-answer audit, a pointer to the
    /// spilled full report, and a teaser of the panel's latest output. Never the
    /// whole body into the main PTY (a `scrollback` panel can run to hundreds of
    /// KB, and one giant bracketed paste risks the backend's paste
    /// pathologies). `report_path` is `None` only when the spill itself failed.
    fn render_panel_summary(
        &self,
        round: &PendingRound,
        id: PanelId,
        c: &RoundPanelContribution,
        report_path: Option<&std::path::Path>,
    ) -> String {
        let mut out = Self::panel_delivery_header(round.id, id, &c.role, c.settled);
        push_panel_auto_answers(&mut out, round, id);
        match report_path {
            Some(path) => out.push_str(&format!(
                "Full report: {} — read that file for this panel's complete output.\n",
                path.display()
            )),
            None => out.push_str(
                "(the full report could not be written to disk; the teaser below is all that is available)\n",
            ),
        }
        self.push_panel_summary_block(&mut out, id, c);
        out
    }

    /// Render one round panel's frozen contribution into the compact message
    /// streamed to the main worker, spilling its full output to
    /// `<session_root>/rounds/<round>-<panel>.md` and pointing at it. The
    /// per-panel replacement for the old whole-round `complete_round`: each
    /// panel is delivered on its own, the tick it settles, instead of waiting
    /// for the barrier.
    ///
    /// Pure render + spill — it does **not** mark the panel delivered. Marking
    /// `round.delivered` is the caller's job, done at the handoff point so
    /// per-panel exactly-once (Invariant I-10) holds: typed-and-confirmed for
    /// the push, returned for the pull/hook-reply. Returns `None` for a gone
    /// panel — it has no contribution to deliver.
    fn render_panel_delivery(&self, round: &PendingRound, id: PanelId) -> Option<String> {
        let c = self.round_panel_contribution(round, id)?;
        let report = self.assemble_panel_report(round, id, &c);
        let path = self.spill_panel_report(round.id, id, &report);
        Some(self.render_panel_summary(round, id, &c, path.as_deref()))
    }

    /// Write one panel's full delivery report to
    /// `<session_root>/rounds/<round_id>-<panel>.md` and return its path, so the
    /// streamed delivery can inject a compact summary that points at it rather
    /// than the whole body. The file name is the round id (a ULID) plus the
    /// panel id, unique per (round, panel). Best-effort: a failure is logged and
    /// yields `None`, and the summary then says the spill failed.
    fn spill_panel_report(
        &self,
        round_id: RoundId,
        panel: PanelId,
        report: &str,
    ) -> Option<std::path::PathBuf> {
        let dir = self.session.root_dir.join("rounds");
        if let Err(err) = std::fs::create_dir_all(&dir) {
            warn!(error = %err, "round report dir create failed");
            return None;
        }
        let path = dir.join(format!("{round_id}-{panel}.md"));
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
    /// `Working`/`Idle`. This is the *only* path from a grid observation into a
    /// `DerivedState`: `derive_agent_state` is manifest-pure. It never masks a
    /// stronger state (`Exited`/`Blocked*`/`Interrupted`/`Degraded`).
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
        // Every panel the caller handed a captured body is one the owner poll
        // would have latched; mirror that so report-rendering tests see the
        // same round shape the live path builds.
        let settled = captured
            .keys()
            .map(|&id| (id, PanelState::Idle))
            .collect::<HashMap<_, _>>();
        PendingRound {
            id: RoundId::new(),
            implicit: false,
            panels,
            settled,
            delivered: HashSet::new(),
            backlog,
            captured,
            read_mode,
            fallback_deadline: Instant::now() + Duration::from_secs(600),
            selection_hints: None,
            auto_answers: Vec::new(),
        }
    }

    /// Index of the pending round watching `panel`.
    ///
    /// Tests address a round by the panel it watches, not by position: caucus
    /// arms an implicit watch on every prompted sub-panel
    /// ([`Multiplexer::arm_worker_watch`]), so what sits at `pending_rounds[0]`
    /// is not part of any contract.
    fn round_of(mux: &Multiplexer, panel: PanelId) -> usize {
        mux.pending_rounds
            .iter()
            .position(|r| r.panels.contains(&panel))
            .expect("a pending round must be watching the panel")
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
                assert!(text.contains("delivered"), "status: {text}");
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

    /// A panel whose id no longer exists has no contribution to stream:
    /// `render_panel_delivery` yields `None` (the retire path marks it delivered
    /// with no push), rather than panicking or emitting an empty block.
    #[tokio::test]
    async fn render_panel_delivery_skips_a_gone_panel() {
        let tmp = TempDir::new().unwrap();
        let mux = mux(&tmp);
        let ghost = PanelId::new();

        let round = pending_round(
            vec![ghost],
            ReadPanelMode::LastMessage,
            HashMap::new(),
            HashMap::new(),
        );
        assert!(
            mux.render_panel_delivery(&round, ghost).is_none(),
            "a gone panel must render no delivery"
        );
    }

    /// `assemble_panel_report` marks a panel still `Working` (the fallback case,
    /// `settled == false`) as unfinished rather than reading a half-done turn.
    ///
    /// Spawning a panel needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn panel_report_marks_a_working_panel_unfinished() {
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

        // No latch → contribution.settled is false → "forced out" header +
        // "still working" footer.
        let round = pending_round(
            vec![sub],
            ReadPanelMode::LastMessage,
            HashMap::new(),
            HashMap::new(),
        );
        let c = mux.round_panel_contribution(&round, sub).unwrap();
        let report = mux.assemble_panel_report(&round, sub, &c);
        assert!(
            report.contains("still working"),
            "a Working panel must be marked unfinished: {report}"
        );
        assert!(
            report.contains("forced out by the fallback deadline"),
            "an unsettled panel's header must say it was forced out: {report}"
        );

        mux.shutdown();
    }

    /// `assemble_panel_report` emits a `### task N` section for every captured
    /// turn — so a multi-task backlog delivers all of its intermediate outputs,
    /// not only the last. Boundary: three captured turns (two backlog feeds plus
    /// the settle capture) → three numbered sections, each body present.
    ///
    /// Spawning a panel needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn panel_report_emits_a_section_per_captured_task() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Some(sub) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };

        // Every finished turn is in `captured` — the two backlog turns and the
        // final one, all read by the owner poll at the instant each ended. The
        // report renders these and never re-reads the panel.
        let captured = HashMap::from([(
            sub,
            vec![
                "output of task one".to_string(),
                "output of task two".to_string(),
                "output of task three".to_string(),
            ],
        )]);
        let round = pending_round(
            vec![sub],
            ReadPanelMode::LastMessage,
            captured,
            HashMap::new(),
        );
        let c = mux.round_panel_contribution(&round, sub).unwrap();
        let report = mux.assemble_panel_report(&round, sub, &c);

        for (n, body) in [
            (1, "output of task one"),
            (2, "output of task two"),
            (3, "output of task three"),
        ] {
            assert!(
                report.contains(&format!("### task {n}")),
                "captured turn {n} header missing: {report}"
            );
            assert!(report.contains(body), "captured body {n} missing: {report}");
        }

        mux.shutdown();
    }

    /// A single-output panel (no backlog → one captured turn) keeps the original
    /// header-less report shape: `### task N` sections appear only when there is
    /// more than one output to disambiguate. Backward-compat boundary against
    /// the multi-task case above.
    #[tokio::test]
    async fn panel_report_omits_task_headers_for_a_single_output() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let sub = push_cat_panel(&mut mux, "reviewer", PanelState::Idle);

        let captured = HashMap::from([(sub, vec!["the only output".to_string()])]);
        let round = pending_round(
            vec![sub],
            ReadPanelMode::LastMessage,
            captured,
            HashMap::new(),
        );
        let c = mux.round_panel_contribution(&round, sub).unwrap();
        let report = mux.assemble_panel_report(&round, sub, &c);
        assert!(
            !report.contains("### task"),
            "a single-output panel must not get task headers: {report}"
        );

        mux.shutdown();
    }

    /// A panel's full result is no longer injected wholesale into the main PTY.
    /// `spill_panel_report` writes the complete per-panel report to
    /// `rounds/<round_id>-<panel>.md`, and `render_panel_summary` injects only a
    /// compact summary that points at that file. A small body is teased inline
    /// so a trivial delivery needs no file read.
    #[tokio::test]
    async fn panel_summary_points_at_a_spilled_full_report() {
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

        let c = mux.round_panel_contribution(&round, panel).unwrap();
        let report = mux.assemble_panel_report(&round, panel, &c);
        let path = mux
            .spill_panel_report(round.id, panel, &report)
            .expect("report spilled");
        assert!(
            path.starts_with(mux.session.root_dir.join("rounds")),
            "report must land in rounds/: {}",
            path.display()
        );
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .contains(&panel.to_string()),
            "the per-panel report file name must carry the panel id: {}",
            path.display()
        );
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains("the full finding body"),
            "the disk report must hold the full body: {on_disk}"
        );

        let summary = mux.render_panel_summary(&round, panel, &c, Some(path.as_path()));
        assert!(
            summary.contains(&format!("panel {panel}")),
            "summary header must name the panel: {summary}"
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

    /// A body far over the teaser budget is bounded in the *injected* per-panel
    /// summary (with an elision marker) while the spilled report file keeps it
    /// in full: the main PTY gets a teaser, the file gets the body.
    #[tokio::test]
    async fn panel_summary_teases_a_large_body_kept_full_in_the_report() {
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

        let c = mux.round_panel_contribution(&round, panel).unwrap();
        let report = mux.assemble_panel_report(&round, panel, &c);
        let path = mux.spill_panel_report(round.id, panel, &report).unwrap();
        let summary = mux.render_panel_summary(&round, panel, &c, Some(path.as_path()));

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

    /// A ready panel whose main worker has *exited on its own* (process crash/
    /// OOM) must drop, not re-queue forever. `pump_all` flips the panel to
    /// `Exited` but does not clear `main_panel_id` (only `kill_panel` does), so
    /// before this fix the round saw `main_panel_id == Some(..)` yet
    /// `deliverable == false`, falling into the re-queue arm every tick. The
    /// settled panel's contribution is spilled rather than silently lost.
    #[tokio::test]
    async fn poll_pending_rounds_drops_when_main_has_exited() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_cat_panel(&mut mux, "main", PanelState::Exited);
        mux.main_panel_id = Some(main);
        // A live idle sub-panel that will latch (settle) on the first poll, so
        // it carries a real contribution the main-gone path must preserve.
        let sub = push_cat_panel(&mut mux, "reviewer", PanelState::Idle);

        mux.register_round(vec![sub], None, Some(600), None, None);
        assert_eq!(mux.pending_rounds.len(), 1);

        mux.poll_pending_rounds();
        assert!(
            mux.pending_rounds.is_empty(),
            "a ready panel against an exited main must drop, not re-queue forever"
        );
        let log = mux.session.root_dir.join("dropped-rounds.log");
        assert!(
            log.exists(),
            "the settled panel's report must be spilled to the session dir"
        );
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            body.contains(&format!("panel {sub}")),
            "the dropped report must name the settled panel: {body}"
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

    /// A ready panel is *held*, not delivered, while the main panel is alive but
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
        // A live idle sub that latches (settles) on the poll, so it is ready to
        // deliver — the round is held only because main is busy, not because the
        // panel is gone.
        let sub = push_cat_panel(&mut mux, "reviewer", PanelState::Idle);

        mux.register_round(vec![sub], None, Some(600), None, None);
        mux.poll_pending_rounds();
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "a ready panel must be held while the live main panel is busy"
        );
        assert!(
            !mux.pending_rounds[0].delivered.contains(&sub),
            "the held panel must not be marked delivered"
        );
        assert!(
            mux.pending_rounds[0].settled.contains_key(&sub),
            "the held panel is latched (settled), just not yet delivered"
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

    /// A worker main talks to is watched without any registration, and the
    /// watch **re-arms** after each delivery — so `instruct → finish → instruct`
    /// runs as a loop instead of dying after one round.
    ///
    /// This is what made a round still feel like a barrier even after per-panel
    /// streaming: `register_round` retired on delivery, so the worker went dark
    /// and main had to register again to hear about the *next* task. Here main
    /// never registers anything at all.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn a_prompted_worker_is_watched_without_registration_and_rearms() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Some(main) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.main_panel_id = Some(main);
        let Some(worker) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };

        // Main sends the worker a task. No register_round anywhere in this test.
        mux.note_prompt_delivered(worker);
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "handing a worker a prompt watches it, with no registration",
        );
        assert!(
            mux.pending_rounds[0].implicit,
            "the watch was armed by caucus, not by register_round",
        );

        // Worker finishes task 1 → main is woken with its result.
        finish_turn(&mut mux, worker, "task one done");
        mux.poll_pending_rounds();
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "the finished worker wakes main",
        );
        assert!(
            mux.pending_rounds.is_empty(),
            "the spent watch is retired once delivered",
        );

        // Main acts on it and sends task 2. The watch re-arms — this is the step
        // that used to require a fresh register_round.
        finish_turn(&mut mux, main, "ack");
        mux.note_prompt_delivered(worker);
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "a later prompt re-arms the watch, continuing the loop",
        );

        // Worker finishes task 2 → main is woken again.
        finish_turn(&mut mux, worker, "task two done");
        mux.poll_pending_rounds();
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "the second finish wakes main just like the first",
        );
        assert!(mux.pending_rounds.is_empty(), "second watch also retires");

        // One spilled report per delivery — two turns, two reports.
        let spills = mux
            .session
            .root_dir
            .join("rounds")
            .read_dir()
            .expect("rounds dir exists")
            .filter_map(|e| e.ok())
            .count();
        assert_eq!(spills, 2, "each delivery spills its own report");

        mux.shutdown();
    }

    /// A backlog feed re-prompts a panel from *inside* `poll_pending_rounds`,
    /// which holds the round out of `pending_rounds` while it runs. The panel
    /// must still read as watched there, or the feed's `note_prompt_delivered`
    /// arms a second watch on a panel its own round already owns — and main is
    /// told about that worker twice.
    ///
    /// Pins `watched_in_flight`: without it `worker_watched` sees an empty queue
    /// mid-poll and arms the duplicate.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn a_backlog_feed_inside_the_poll_does_not_double_watch_the_panel() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Some(main) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.main_panel_id = Some(main);
        let Some(sub) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };

        mux.register_round(
            vec![sub],
            None,
            Some(3600),
            Some(HashMap::from([(sub, vec!["queued".to_string()])])),
            None,
        );
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "register_round absorbs the implicit watch rather than adding to it",
        );

        // The poll feeds the queued task, re-prompting `sub` while the round is
        // held out of `pending_rounds`.
        mux.poll_pending_rounds();
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "the feed must not arm a second watch on an already-watched panel",
        );
        assert_eq!(
            mux.pending_rounds
                .iter()
                .filter(|r| r.panels.contains(&sub))
                .count(),
            1,
            "exactly one round may owe main this panel",
        );

        mux.shutdown();
    }

    /// Streaming delivery: a multi-panel round wakes main the instant *each*
    /// panel settles, without waiting for the slowest. The fast panel is
    /// delivered while the slow one is still `Working`; the round stays pending
    /// (the slow panel undelivered) and lands its own delivery only once it
    /// finishes. No barrier — main interacts once per panel, N times per round.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn poll_pending_rounds_streams_each_panel_as_it_settles() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Some(main) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.main_panel_id = Some(main);
        let Some(fast) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        let Some(slow) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };

        // Both sub-panels open a turn (→ Working); a generous fallback so only a
        // real settle, not the deadline, drives delivery.
        mux.note_prompt_delivered(fast);
        mux.note_prompt_delivered(slow);
        let round_id = match mux.register_round(vec![fast, slow], None, Some(3600), None, None) {
            ControlResponse::RoundRegistered { round_id, .. } => round_id,
            other => panic!("expected RoundRegistered, got {other:?}"),
        };

        // Both working: nothing delivered, round held.
        mux.poll_pending_rounds();
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "round held while both panels work"
        );
        assert!(
            mux.pending_rounds[0].delivered.is_empty(),
            "no panel delivered while both work",
        );

        // The fast panel settles; the slow one keeps working.
        finish_turn(&mut mux, fast, "fast finding");
        mux.poll_pending_rounds();

        // Main was woken by the fast panel alone — no wait for the slow one.
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "the fast panel's settle wakes main immediately",
        );
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "round still pending: the slow panel is undelivered",
        );
        let round = &mux.pending_rounds[0];
        assert!(round.delivered.contains(&fast), "fast panel delivered");
        assert!(
            !round.delivered.contains(&slow),
            "slow panel not yet delivered",
        );
        assert_eq!(
            mux.panels().iter().find(|p| p.id == slow).unwrap().state(),
            PanelState::Working,
            "the slow panel is still working when the fast one already delivered",
        );

        // Drain main back to Idle, then the slow panel finishes.
        finish_turn(&mut mux, main, "ack fast");
        finish_turn(&mut mux, slow, "slow finding");
        mux.poll_pending_rounds();

        // The slow panel's own delivery lands and retires the round.
        assert!(
            mux.pending_rounds.is_empty(),
            "the round retires once every panel is delivered",
        );
        assert_eq!(
            mux.panels().iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "the slow panel's delivery wakes main again",
        );

        // One spilled report per panel — two deliveries, not a single barrier dump.
        let spills = mux
            .session
            .root_dir
            .join("rounds")
            .read_dir()
            .expect("rounds dir exists")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&round_id.to_string())
            })
            .count();
        assert_eq!(spills, 2, "one spilled report per streamed panel");

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

    /// The round owner captures every finished turn into `PendingRound::captured`
    /// — one per fed backlog task, **and** one more when the drained panel latches
    /// (Invariant I-9). The accumulation that lets the delivered report carry
    /// every task's result, tested at its boundaries: after feed 1 → 1 body,
    /// after feed 2 → 2, after the queue-empty settle → 3.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn round_owner_captures_every_finished_turn() {
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
        // the round stays pending and we can inspect the latch: the queue-empty
        // settle captures the panel's last turn *now*, rather than leaving it to
        // a live read at delivery time that a later wake-up could corrupt.
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
            Some(3),
            "the queue-empty settle must capture the panel's final turn",
        );
        assert!(
            mux.pending_rounds[0].settled.contains_key(&sub),
            "the drained panel must latch as settled for the round",
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

    /// An idle panel with queued backlog is not settled yet: the owner feeds it
    /// its next task instead of latching it. The queue must drain before round
    /// delivery; otherwise a failed or skipped feed could report a round complete
    /// while work remains queued. Boundary: queued → fed, not latched; drained →
    /// latched.
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

        // Idle with work queued: the owner feeds the task (flipping the panel
        // back to `Working`) rather than latching it.
        assert!(mux.poll_round_panels(&mut round, false));
        assert!(
            round.backlog[&sub].is_empty(),
            "the queued task must have been fed"
        );
        assert!(
            !mux.round_settled(&round),
            "an idle panel with queued backlog is not settled"
        );
        assert_eq!(
            mux.panels.iter().find(|p| p.id == sub).unwrap().state(),
            PanelState::Working,
            "the fed task reopens the panel's turn",
        );

        // The fed task finishes with the queue now empty → the owner latches it.
        mux.panels.iter_mut().find(|p| p.id == sub).unwrap().state = PanelState::Idle;
        assert!(mux.poll_round_panels(&mut round, false));
        assert!(
            mux.round_settled(&round),
            "the drained queue latches the panel"
        );

        mux.shutdown();
    }

    /// An `Exited` panel with queued backlog latches immediately. The panel is
    /// terminal and `poll_round_panels` only feeds `Idle` panels, so its backlog
    /// can never drain — gating on it (like a live `Idle` panel) would wedge the
    /// round forever.
    #[tokio::test]
    async fn round_settles_when_a_panel_exits_with_undrained_backlog() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let sub = push_cat_panel(&mut mux, "reviewer", PanelState::Exited);
        let mut round = pending_round(
            vec![sub],
            ReadPanelMode::LastMessage,
            HashMap::new(),
            HashMap::from([(sub, VecDeque::from(["queued".to_string()]))]),
        );

        assert!(
            !mux.round_settled(&round),
            "no panel is settled until the owner poll latches it"
        );

        assert!(mux.poll_round_panels(&mut round, false));
        assert!(
            mux.round_settled(&round),
            "an exited panel with queued backlog latches — it can never drain"
        );
        assert_eq!(
            round.backlog[&sub].len(),
            1,
            "a terminal panel is never fed its queued work",
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
        assert!(
            !r.settled.contains_key(&old) && r.settled.contains_key(&new),
            "settle latch re-keyed to the new id"
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

    /// Run one turn on `panel` and end it with a `Stop` signal carrying
    /// `message` — the settle path a round panel takes, with the manifest's
    /// `last_message` (what `ReadPanelMode::LastMessage` reads) left holding
    /// `message`. Panel ends `Idle`.
    fn finish_turn(mux: &mut Multiplexer, panel: PanelId, message: &str) {
        let session_id = mux.session.id;
        mux.note_prompt_delivered(panel);
        mux.handle_signal(TurnSignal::now(
            session_id,
            panel,
            crate::signal::TurnKind::Stop,
            Some(message.to_string()),
            serde_json::Value::Null,
        ));
    }

    /// A settled round panel that is later re-woken must **not** un-settle the
    /// round, and must still deliver the work it finished (Invariant I-9).
    ///
    /// The regression this pins: a round is delivered only once the *main* panel
    /// is idle, so an unbounded stretch can pass between a sub-panel settling and
    /// the round landing. A sub-agent whose leftover background shell finishes in
    /// that window wakes its CLI into a fresh turn — and the old code, which
    /// re-judged the barrier from live panel state on every tick, saw the panel
    /// back in `Working`, un-dued the finished round, and (via
    /// `round_panel_contribution`) skipped the panel's read entirely, so even the
    /// eventual fallback delivery reported it as "still working, no output
    /// captured". The latch makes the settle stick and freezes the contribution.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn a_rewoken_panel_does_not_unsettle_a_latched_round() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Some(main) = spawn_idle(&mut mux, "main") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.main_panel_id = Some(main);
        let Some(sub) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };

        finish_turn(&mut mux, sub, "finished the audit: 3 findings");
        mux.register_round(vec![sub], None, Some(3600), None, None);

        // Main is mid-turn, so the push is gated: the round stays pending with
        // the sub latched — exactly the window the incident lived in.
        mux.note_prompt_delivered(main);
        mux.poll_pending_rounds();
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "the push is gated on idle main"
        );
        assert!(
            mux.pending_rounds[0].settled.contains_key(&sub),
            "the settled sub-panel must latch",
        );

        // The leftover background shell finishes and the CLI wakes itself into a
        // turn caucus never prompted.
        mux.note_prompt_delivered(sub);
        assert_eq!(
            mux.panels.iter().find(|p| p.id == sub).unwrap().state(),
            PanelState::Working,
            "the wake-up reopens the panel's turn",
        );

        mux.poll_pending_rounds();
        assert!(
            mux.round_settled(&mux.pending_rounds[0]),
            "a re-woken panel must not un-settle a round it already finished",
        );

        let c = mux
            .round_panel_contribution(&mux.pending_rounds[0], sub)
            .unwrap();
        let report = mux.assemble_panel_report(&mux.pending_rounds[0], sub, &c);
        assert!(
            report.contains("finished the audit: 3 findings"),
            "the latched contribution must survive the wake-up: {report}",
        );
        assert!(
            !report.contains("still working"),
            "a latched panel must never be reported unfinished: {report}",
        );

        mux.shutdown();
    }

    /// The latch captures a panel's result at the instant it settles, so a turn
    /// the agent starts *on its own* afterwards cannot overwrite what the round
    /// reports (Invariant I-9).
    ///
    /// `ReadPanelMode::LastMessage` — the default — reads the manifest's
    /// `last_message`, which every `Stop` hook overwrites. A sub-agent that ended
    /// its turn with background shells still running gets woken when one finishes
    /// and ends a *second* turn with something like "that was a leftover
    /// wait-loop; nothing new came out of it". The old code read the panel at
    /// delivery time and so put that stray line in the round report in place of
    /// the panel's actual result.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn the_latch_freezes_last_message_against_a_later_self_started_turn() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Some(main) = spawn_idle(&mut mux, "main") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.main_panel_id = Some(main);
        let Some(sub) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };

        finish_turn(&mut mux, sub, "PARITY REPORT: 3 divergences, all fixed");
        mux.register_round(vec![sub], None, Some(3600), None, None);

        // Latch the sub while main is mid-turn (the push is gated).
        mux.note_prompt_delivered(main);
        mux.poll_pending_rounds();
        assert!(mux.pending_rounds[0].settled.contains_key(&sub));

        // The leftover shell finishes; the CLI runs a turn caucus never prompted
        // and its `Stop` hook overwrites the manifest's `last_message`.
        mux.handle_signal(TurnSignal::now(
            mux.session.id,
            sub,
            crate::signal::TurnKind::Stop,
            Some("that was a leftover wait-loop; nothing new came out of it".to_string()),
            serde_json::Value::Null,
        ));

        let c = mux
            .round_panel_contribution(&mux.pending_rounds[0], sub)
            .unwrap();
        let report = mux.assemble_panel_report(&mux.pending_rounds[0], sub, &c);
        assert!(
            report.contains("PARITY REPORT: 3 divergences, all fixed"),
            "the round must report the result captured at settle-time: {report}",
        );
        assert!(
            !report.contains("leftover wait-loop"),
            "a turn started after the latch must not overwrite the round's result: {report}",
        );

        mux.shutdown();
    }

    /// A main worker polling from inside its own turn can **pull** a completed
    /// round (Invariant I-10) — the escape from the livelock the push-only path
    /// created.
    ///
    /// The push (`poll_pending_rounds`) can only type into an *idle* main panel.
    /// A main worker that stays in its turn polling `round_status` is `Working`
    /// for the whole poll, so the push never fires: main waits for the round and
    /// the round waits for main to stop waiting. The fallback deadline does not
    /// break it — that makes a round *due*, not *deliverable*. `round_status`
    /// therefore returns the assembled report itself and completes the round.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn round_status_pulls_a_completed_round_for_a_busy_main() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Some(main) = spawn_idle(&mut mux, "main") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.main_panel_id = Some(main);
        let Some(sub) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };

        finish_turn(&mut mux, sub, "the sub-agent's result");
        // Main never ends its turn — it polls, which is what an LLM main worker
        // actually does.
        mux.note_prompt_delivered(main);
        let ControlResponse::RoundRegistered { round_id, .. } =
            mux.register_round(vec![sub], None, Some(3600), None, None)
        else {
            panic!("register_round must ack with a round id");
        };

        // The push is dead while main is working: it will re-queue forever.
        mux.poll_pending_rounds();
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "the push cannot land in a working main panel",
        );

        // The pull streams the settled panel to the poller instead.
        let ControlResponse::Panel { text } = mux.round_status(round_id) else {
            panic!("a completed round must answer with its report");
        };
        assert!(
            text.contains(&format!("panel {sub}")),
            "the poller must get the panel's delivery, not a status line: {text}",
        );
        assert!(
            text.contains("the sub-agent's result"),
            "the report must carry the panel's captured result: {text}",
        );

        // Exactly-once: the panel is delivered and the round retired, not left
        // for a second delivery.
        assert!(
            mux.pending_rounds.is_empty(),
            "a fully-pulled round must be retired, not re-queued for the push",
        );
        mux.poll_pending_rounds();
        assert!(matches!(
            mux.round_status(round_id),
            ControlResponse::Error { .. }
        ));

        mux.shutdown();
    }

    /// A round whose panels are still working is **not** pulled: `round_status`
    /// answers with its live status and leaves it pending. The other side of the
    /// pull boundary from `round_status_pulls_a_completed_round_for_a_busy_main`.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn round_status_leaves_an_unfinished_round_pending() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Some(sub) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        // Mid-turn: nothing to latch.
        mux.note_prompt_delivered(sub);

        let ControlResponse::RoundRegistered { round_id, .. } =
            mux.register_round(vec![sub], None, Some(3600), None, None)
        else {
            panic!("register_round must ack with a round id");
        };

        let ControlResponse::Panel { text } = mux.round_status(round_id) else {
            panic!("a live round must answer with its status");
        };
        assert!(
            text.contains("working"),
            "an unfinished panel must be reported as working: {text}",
        );
        assert!(
            !text.contains("Full report:"),
            "an unfinished panel must not be delivered: {text}",
        );
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "an unfinished round stays pending",
        );

        mux.shutdown();
    }

    /// The fallback tick still **latches** (captures) a panel that settled with
    /// queued work left — it only stops *feeding* it more. Without this the
    /// partial report the deadline forces would carry nothing for a panel whose
    /// finished turns were never read.
    ///
    /// Spawning panels needs a real agent CLI; skipped when none is on PATH.
    #[tokio::test]
    async fn the_fallback_tick_latches_an_idle_panel_with_queued_backlog() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Some(main) = spawn_idle(&mut mux, "main") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        mux.main_panel_id = Some(main);
        let Some(sub) = spawn_idle(&mut mux, "reviewer") else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };

        finish_turn(&mut mux, sub, "what the panel got through");
        mux.register_round(
            vec![sub],
            None,
            Some(3600),
            Some(HashMap::from([(sub, vec!["never-run".to_string()])])),
            None,
        );
        // Address the round by the panel it watches: caucus also arms implicit
        // watches (`arm_worker_watch`), so position in `pending_rounds` is not
        // part of any contract.
        let idx = round_of(&mux, sub);
        // Force the deadline: the feeding window is closed.
        mux.pending_rounds[idx].fallback_deadline = Instant::now();

        // Hold the push so the pending round can be inspected post-latch.
        mux.main_compose_since = Some(Instant::now());
        mux.poll_pending_rounds();

        let idx = round_of(&mux, sub);
        assert!(
            mux.pending_rounds[idx].settled.contains_key(&sub),
            "the deadline latches an idle panel rather than leaving it unread",
        );
        assert_eq!(
            mux.pending_rounds[idx].backlog[&sub].len(),
            1,
            "no new backlog work starts after the deadline",
        );

        let c = mux
            .round_panel_contribution(&mux.pending_rounds[idx], sub)
            .unwrap();
        let report = mux.assemble_panel_report(&mux.pending_rounds[idx], sub, &c);
        assert!(
            report.contains("what the panel got through"),
            "the partial report must carry the work the panel finished: {report}",
        );
        assert!(
            report.contains("1 queued backlog task(s) were not run"),
            "the partial report must name the un-run work: {report}",
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

        // A sub-panel `Working` on something caucus never handed it — the only
        // way to be unwatched now, since every prompt caucus delivers arms a
        // watch (`arm_worker_watch`). Pushed straight in, bypassing the prompt.
        let sub = push_cat_panel(&mut mux, "reviewer", PanelState::Working);
        assert_eq!(
            mux.panels().iter().find(|p| p.id == sub).unwrap().state(),
            PanelState::Working,
        );
        assert!(
            mux.pending_rounds.is_empty(),
            "an un-prompted panel is not watched, so nothing will wake main",
        );

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

    /// A queued question notice is injected into an idle main — which flips
    /// it `Working` (the injected turn), closing the deliverability gate: at
    /// most one notice lands per tick, the rest stay queued.
    #[tokio::test]
    async fn poll_question_notices_delivers_one_notice_per_tick_to_idle_main() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_cat_panel(&mut mux, "main", PanelState::Idle);
        mux.main_panel_id = Some(main);
        let sub = PanelId::new();
        mux.enqueue_question_notice(sub, "reviewer".into(), "first question?".into());
        mux.enqueue_question_notice(sub, "reviewer".into(), "second question?".into());

        mux.poll_question_notices();
        assert_eq!(
            mux.pending_question_notices.len(),
            1,
            "one notice delivered, the other queued"
        );
        assert_eq!(
            mux.pending_question_notices.front().unwrap().body,
            "second question?",
            "delivery is FIFO"
        );
        assert_eq!(
            mux.panels.iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "the injected notice is a turn: main flips Working"
        );

        // Same tick again: the gate is now closed (main `Working`).
        mux.poll_question_notices();
        assert_eq!(mux.pending_question_notices.len(), 1);
        mux.shutdown();
    }

    /// The notice's guidance clause reports the questioning panel's state at
    /// delivery time, one boundary per state. The observed live-session edge:
    /// a notice that waited behind a round push landed after its panel had
    /// settled, still claiming the panel "queues input" — a main worker
    /// following that literally opens a spurious new turn on an idle panel.
    #[tokio::test]
    async fn question_notice_guidance_tracks_the_panel_state_at_delivery() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let sub = push_cat_panel(&mut mux, "reviewer", PanelState::Working);
        let notice = QuestionNotice {
            panel: sub,
            role: "reviewer".into(),
            body: "which schema?".into(),
        };

        // Still mid-turn: the queue-input claim holds.
        let text = mux.question_notice(&notice);
        assert!(text.contains("still mid-turn"), "working: {text}");

        // Settled before delivery: the mid-turn claim must not survive, and
        // the reader is told an answer opens a fresh turn.
        let panel = mux.panels.iter_mut().find(|p| p.id == sub).unwrap();
        crate::panel::lifecycle::transition(panel, PanelState::Idle).unwrap();
        let text = mux.question_notice(&notice);
        assert!(!text.contains("still mid-turn"), "idle: {text}");
        assert!(text.contains("starts a new turn"), "idle: {text}");

        // Gone before delivery: no answer path is advertised at all.
        let text = mux.question_notice(&QuestionNotice {
            panel: PanelId::new(),
            role: "reviewer".into(),
            body: "which schema?".into(),
        });
        assert!(!text.contains("send_keys"), "gone: {text}");
        assert!(text.contains("exited"), "gone: {text}");
        mux.shutdown();
    }

    /// An un-submitted human keystroke in the main panel holds notice
    /// delivery — the same `COMPOSE_GRACE` gate as round delivery. The notice
    /// stays queued for a later tick.
    #[tokio::test]
    async fn poll_question_notices_holds_while_the_user_composes() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_cat_panel(&mut mux, "main", PanelState::Idle);
        mux.main_panel_id = Some(main);
        mux.enqueue_question_notice(PanelId::new(), "reviewer".into(), "held?".into());
        mux.main_compose_since = Some(Instant::now());

        mux.poll_question_notices();
        assert_eq!(
            mux.pending_question_notices.len(),
            1,
            "the compose hold must keep the notice queued"
        );
        assert_eq!(
            mux.panels.iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Idle,
            "nothing was injected into the composed-on panel"
        );
        mux.shutdown();
    }

    /// With the main worker gone (its panel `Exited`) no wake path will ever
    /// exist: the queue is dropped rather than accreting forever — the same
    /// rule round delivery applies.
    #[tokio::test]
    async fn poll_question_notices_drops_the_queue_when_main_is_gone() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_cat_panel(&mut mux, "main", PanelState::Exited);
        mux.main_panel_id = Some(main);
        mux.enqueue_question_notice(PanelId::new(), "reviewer".into(), "anyone?".into());

        mux.poll_question_notices();
        assert!(
            mux.pending_question_notices.is_empty(),
            "no main worker will ever answer; the queue must not accrete"
        );
        mux.shutdown();
    }

    /// The notice queue is bounded: past `QUESTION_NOTICE_CAP` the oldest is
    /// dropped, so a chatty sub cannot grow caucus memory without bound while
    /// the main worker is busy.
    #[tokio::test]
    async fn enqueue_question_notice_caps_the_queue_dropping_the_oldest() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let sub = PanelId::new();
        for i in 0..=QUESTION_NOTICE_CAP {
            mux.enqueue_question_notice(sub, "reviewer".into(), format!("q{i}"));
        }
        assert_eq!(mux.pending_question_notices.len(), QUESTION_NOTICE_CAP);
        assert_eq!(
            mux.pending_question_notices.front().unwrap().body,
            "q1",
            "the oldest notice is the one dropped"
        );
    }

    /// A hermetic main + settled-sub round: main mid-turn (`Working`), the sub
    /// finished, the round registered and due. The window the hook-reply path
    /// exists for.
    fn due_round_with_working_main(mux: &mut Multiplexer) -> (PanelId, PanelId) {
        let main = push_cat_panel(mux, "main", PanelState::Working);
        mux.main_panel_id = Some(main);
        let sub = push_cat_panel(mux, "reviewer", PanelState::Working);
        finish_turn(mux, sub, "audit done: 3 findings");
        mux.register_round(vec![sub], None, Some(3600), None, None);
        assert_eq!(mux.pending_rounds.len(), 1);
        (main, sub)
    }

    /// A `Stop` signal for `panel`, as `handle_signal_with_reply` receives it.
    fn stop_signal(mux: &Multiplexer, panel: PanelId) -> TurnSignal {
        TurnSignal::now(
            mux.session.id,
            panel,
            crate::signal::TurnKind::Stop,
            None,
            serde_json::Value::Null,
        )
    }

    /// The hook-reply delivery: main's own Stop with a waiting reply slot and
    /// a due round sends the summary as a `Deliver` directive, consumes the
    /// round exactly once (the push finds nothing left), and continues the
    /// main turn — the panel never goes `Idle` and its capture turn reopens.
    /// The sub's latch is taken *inside* this call (the round was registered
    /// with nothing latched), so the judgement saw fresh state.
    #[tokio::test]
    async fn hook_reply_delivers_a_due_round_into_the_continuing_turn() {
        use crate::signal::StopDirective;

        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let (main, sub) = due_round_with_working_main(&mut mux);

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        mux.handle_signal_with_reply(stop_signal(&mux, main), Some(tx));

        let StopDirective::Deliver { reason } = rx.try_recv().expect("directive must be sent");
        assert!(
            reason.contains(&format!("panel {sub}")),
            "the directive carries the panel's streamed delivery: {reason}"
        );
        assert!(
            mux.pending_rounds.is_empty(),
            "the delivered round is consumed"
        );
        assert_eq!(
            mux.panels.iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "a blocked Stop continues the turn: main never goes Idle"
        );

        // Exactly-once: the keystroke push has nothing left to deliver.
        mux.poll_pending_rounds();
        assert!(mux.pending_rounds.is_empty());
        mux.shutdown();
    }

    /// The human-first rule is path-independent: with the user mid-compose in
    /// the main panel, the hook is answered allow (sender dropped), the round
    /// stays pending for the keystroke push, and the compose hold survives —
    /// `note_prompt_delivered` would have cleared it, which is why the hook
    /// path reopens the turn through its own sibling helper.
    #[tokio::test]
    async fn hook_reply_allows_while_the_user_composes_and_keeps_the_hold() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let (main, sub) = due_round_with_working_main(&mut mux);
        mux.enqueue_question_notice(sub, "reviewer".into(), "held?".into());
        mux.main_compose_since = Some(Instant::now());

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        mux.handle_signal_with_reply(stop_signal(&mux, main), Some(tx));

        assert!(
            rx.try_recv().is_err(),
            "the sender must be dropped (allow), not answered"
        );
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "the round stays for the keystroke push after the grace"
        );
        assert_eq!(
            mux.pending_question_notices.len(),
            1,
            "the notice queue also survives the compose hold"
        );
        assert!(
            mux.main_compose_since.is_some(),
            "the human's compose hold must survive the allow"
        );
        assert_eq!(
            mux.panels.iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Idle,
            "an allowed Stop settles main normally"
        );
        mux.shutdown();
    }

    /// A reply slot whose receiver is already gone (the hook client timed out
    /// before this tick) must not consume a round: the signal is handled as a
    /// plain stop and the round remains — the keystroke push then delivers it
    /// to the now-idle main.
    #[tokio::test]
    async fn hook_reply_with_a_dead_receiver_leaves_the_round_for_the_push() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let (main, sub) = due_round_with_working_main(&mut mux);
        mux.enqueue_question_notice(sub, "reviewer".into(), "kept?".into());

        let (tx, rx) = tokio::sync::oneshot::channel();
        drop(rx);
        mux.handle_signal_with_reply(stop_signal(&mux, main), Some(tx));

        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "a dead hook must not consume the round"
        );
        assert_eq!(
            mux.pending_question_notices.len(),
            1,
            "a dead hook must not consume the notice queue either"
        );
        assert_eq!(
            mux.panels.iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Idle,
        );

        // The ordinary push completes the delivery.
        mux.poll_pending_rounds();
        assert!(mux.pending_rounds.is_empty(), "the push delivered it");
        assert_eq!(
            mux.panels.iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "the injected summary opened a main turn"
        );
        mux.shutdown();
    }

    /// A reply slot on a **sub** panel's signal is answered allow and touches
    /// no round: only the main panel's own Stop may carry a round back.
    #[tokio::test]
    async fn hook_reply_on_a_sub_panel_signal_is_allowed_and_consumes_nothing() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let (_main, sub) = due_round_with_working_main(&mut mux);
        let other = push_cat_panel(&mut mux, "qa", PanelState::Working);
        mux.enqueue_question_notice(sub, "reviewer".into(), "kept?".into());

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        mux.handle_signal_with_reply(stop_signal(&mux, other), Some(tx));

        assert!(rx.try_recv().is_err(), "a sub panel's hook is always allow");
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "no round is consumed by a sub panel's signal"
        );
        assert_eq!(
            mux.pending_question_notices.len(),
            1,
            "no notice is consumed by a sub panel's signal"
        );
        mux.shutdown();
    }

    /// A hook-continued boundary arms the alternation gate: the first delivery
    /// works exactly as before and records that this boundary continued the
    /// turn, so the next one may not.
    #[tokio::test]
    async fn a_hook_continued_boundary_arms_the_alternation_gate() {
        use crate::signal::StopDirective;
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let (main, _sub) = due_round_with_working_main(&mut mux);
        assert!(
            !mux.main_last_boundary_hook_continued,
            "a fresh boundary is free to continue"
        );

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        mux.handle_signal_with_reply(stop_signal(&mux, main), Some(tx));

        let StopDirective::Deliver { .. } = rx.try_recv().expect("directive must be sent");
        assert!(
            mux.main_last_boundary_hook_continued,
            "a hook-continued boundary arms the gate against a second in a row"
        );
        mux.shutdown();
    }

    /// Alternation gate: caucus must not hook-continue the main on two
    /// consecutive boundaries. With the previous boundary already
    /// hook-continued, a due round is answered *allow* (sender dropped) — the
    /// main settles to a real `Idle` boundary where Claude Code can compact and
    /// the user can type — the round stays for the keystroke push, and the gate
    /// clears so the next boundary is free again. This is the bound that stops a
    /// stream of due rounds from wedging the main in a continue→compact→continue
    /// loop (the resumed/heavy-session symptom).
    #[tokio::test]
    async fn hook_reply_does_not_continue_the_main_twice_in_a_row() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let (main, _sub) = due_round_with_working_main(&mut mux);
        // The previous boundary was hook-continued.
        mux.main_last_boundary_hook_continued = true;

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        mux.handle_signal_with_reply(stop_signal(&mux, main), Some(tx));

        assert!(
            rx.try_recv().is_err(),
            "a second consecutive boundary must be allowed, not delivered"
        );
        assert_eq!(
            mux.pending_rounds.len(),
            1,
            "the round stays pending for the keystroke push"
        );
        assert_eq!(
            mux.panels.iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Idle,
            "an allowed Stop settles main to a real idle boundary"
        );
        assert!(
            !mux.main_last_boundary_hook_continued,
            "the allowed boundary clears the gate so the next may continue"
        );

        // The still-due round now rides the keystroke push into the idle main.
        mux.poll_pending_rounds();
        assert!(mux.pending_rounds.is_empty(), "the push delivered it");
        assert_eq!(
            mux.panels.iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "the injected summary opened a main turn once it was idle"
        );
        mux.shutdown();
    }

    /// Queued question notices ride the hook reply even with no due round:
    /// the whole queue is batched into the continuing turn — held back, a
    /// notice would wait an entire further main turn for the idle push, which
    /// is the latency the hook path exists to remove.
    #[tokio::test]
    async fn hook_reply_delivers_queued_notices_without_a_round() {
        use crate::signal::StopDirective;

        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_cat_panel(&mut mux, "main", PanelState::Working);
        mux.main_panel_id = Some(main);
        let sub = push_cat_panel(&mut mux, "reviewer", PanelState::Working);
        mux.enqueue_question_notice(sub, "reviewer".into(), "which schema?".into());
        mux.enqueue_question_notice(sub, "reviewer".into(), "and which port?".into());

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        mux.handle_signal_with_reply(stop_signal(&mux, main), Some(tx));

        let StopDirective::Deliver { reason } = rx.try_recv().expect("directive must be sent");
        assert!(reason.contains("which schema?"), "first notice: {reason}");
        assert!(
            reason.contains("and which port?"),
            "the queue is batched: {reason}"
        );
        assert!(
            mux.pending_question_notices.is_empty(),
            "delivered notices are consumed"
        );
        assert_eq!(
            mux.panels.iter().find(|p| p.id == main).unwrap().state(),
            PanelState::Working,
            "a blocked Stop continues the turn: main never goes Idle"
        );
        mux.shutdown();
    }

    /// With both pending, one reply carries the round first (the primary
    /// deliverable) and the notice queue after it — and both are consumed, so
    /// neither push path delivers a second copy.
    #[tokio::test]
    async fn hook_reply_combines_a_due_round_and_the_notice_queue() {
        use crate::signal::StopDirective;

        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let (main, sub) = due_round_with_working_main(&mut mux);
        mux.enqueue_question_notice(sub, "reviewer".into(), "which schema?".into());

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        mux.handle_signal_with_reply(stop_signal(&mux, main), Some(tx));

        let StopDirective::Deliver { reason } = rx.try_recv().expect("directive must be sent");
        let round_at = reason
            .find(&format!("panel {sub}"))
            .expect("round in reason");
        let notice_at = reason.find("which schema?").expect("notice in reason");
        assert!(
            round_at < notice_at,
            "the round leads, notices follow: {reason}"
        );
        assert!(mux.pending_rounds.is_empty(), "the round is consumed");
        assert!(
            mux.pending_question_notices.is_empty(),
            "the notices are consumed"
        );

        // Exactly-once on both: neither push has anything left.
        mux.poll_pending_rounds();
        mux.poll_question_notices();
        assert!(mux.pending_rounds.is_empty());
        assert!(mux.pending_question_notices.is_empty());
        mux.shutdown();
    }

    /// `take_ready_round_deliveries` judges readiness fresh and takes each ready
    /// panel exactly once: an unfinished round yields nothing and stays pending;
    /// once the panel settles it is streamed, marked delivered, the round
    /// retired, and a second call finds nothing.
    #[tokio::test]
    async fn take_ready_round_deliveries_takes_each_ready_panel_once() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let sub = push_cat_panel(&mut mux, "reviewer", PanelState::Working);
        mux.note_prompt_delivered(sub);
        mux.register_round(vec![sub], None, Some(3600), None, None);

        assert!(
            mux.take_ready_round_deliveries().is_empty(),
            "a working panel is not ready to deliver"
        );
        assert_eq!(mux.pending_rounds.len(), 1, "and the round stays pending");

        finish_turn(&mut mux, sub, "done");
        let deliveries = mux.take_ready_round_deliveries();
        assert_eq!(deliveries.len(), 1, "the settled panel is streamed once");
        assert!(
            deliveries[0].contains(&format!("panel {sub}")),
            "got: {}",
            deliveries[0]
        );
        assert!(
            mux.pending_rounds.is_empty(),
            "the round retires once drained"
        );
        assert!(
            mux.take_ready_round_deliveries().is_empty(),
            "a delivered panel cannot be taken twice"
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
        // Un-prompted `Working` panel: unwatched, so main really is stranded.
        let _sub = push_cat_panel(&mut mux, "reviewer", PanelState::Working);

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

    /// `push_panel_auto_answers` emits nothing for a panel caucus auto-answered
    /// no menu on, and one line per recorded auto-answer for that panel — never
    /// another panel's, since streamed delivery hands main one panel at a time.
    #[test]
    fn auto_answers_block_lists_each_resolved_menu_for_the_panel() {
        let p = PanelId::new();
        let other = PanelId::new();
        let mut round = pending_round(
            vec![p, other],
            ReadPanelMode::LastMessage,
            HashMap::new(),
            HashMap::new(),
        );

        let mut out = String::new();
        push_panel_auto_answers(&mut out, &round, p);
        assert!(out.is_empty(), "no auto-answers → no block");

        round.auto_answers.push(AutoAnswerRecord {
            panel: p,
            role: "reviewer".to_string(),
            number: 2,
            label: "Structural fix at source".to_string(),
        });
        // An auto-answer on a *different* panel must not leak into this one's.
        round.auto_answers.push(AutoAnswerRecord {
            panel: other,
            role: "qa".to_string(),
            number: 9,
            label: "Some other fork".to_string(),
        });
        push_panel_auto_answers(&mut out, &round, p);
        assert!(
            out.contains("caucus auto-answered 1 selection menu(s) on this panel"),
            "got: {out}"
        );
        assert!(
            out.contains("→ option 2 \"Structural fix at source\" (role: reviewer)"),
            "got: {out}"
        );
        assert!(
            !out.contains("Some other fork"),
            "another panel's auto-answer must not appear: {out}"
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
