//! Unbound turn-signal resolution (`docs/design.md` §7.8).
//!
//! The exact signal path names its panel via the `CAUCUS_*` env caucus injects
//! at spawn — inherited process state. Claude Code can move a live
//! conversation into a process that inherited nothing from the panel's PTY:
//! the `claude daemon` re-hosts sessions across auto-update restarts and crash
//! recovery (`--fork-session`, with a *fresh* conversation id). The hook still
//! fires there, posts an [`UnboundSignal`] to every live caucus socket
//! (`caucus signal post --discover`), and this module is the server half: the
//! single owner of the unbound→bound conversion. It decides — from the payload
//! alone — whether the conversation is one of this session's panels, then
//! routes the bound signal through the ordinary turn-completion owner
//! ([`Multiplexer::handle_signal_with_reply`]), which also heals the
//! manifest's stored conversation id (`record_turn_completed` lifts the
//! payload's `session_id`), so a fork's very next signal matches by exact id.
//!
//! Resolution rules, in order:
//! 1. **Exact**: the payload's conversation id is one this session's manifests
//!    already hold — the same process lost only its env, not its identity.
//! 2. **Lineage**: the id is unknown (a fork minted it), but the conversation
//!    transcript's head carries the records copied from its parent, and those
//!    records name the parent's conversation id — if exactly one panel's known
//!    id appears there, the fork is that panel's conversation continued.
//! 3. Otherwise the signal is not ours: the reply sender is dropped (the
//!    server answers allow) and nothing changes.
//!
//! A conversation judged not-ours after a complete lineage read is cached
//! (`Multiplexer::unbound_unclaimed`) so ordinary env-less Claude sessions on
//! the machine do not cost a transcript read per turn. A panel spawned later
//! is matched by rule 1 before the cache is consulted, so the cache can never
//! shadow a real panel.

use std::collections::HashSet;
use std::path::Path;

use tracing::warn;

use super::*;
use crate::signal::{StopDirective, TurnSignal, UnboundSignal};

/// How much of a transcript's head a lineage scan reads. A fork's transcript
/// begins with bookkeeping records (file-history snapshots can run to hundreds
/// of KiB per line) before the copied parent records that carry the parent's
/// conversation id; the observed distance is well under 1 MiB, and 4 MiB of
/// headroom keeps one bounded read cheap while making a false "no lineage"
/// unlikely. A capped read that found nothing is *not* cached as not-ours —
/// the next signal simply re-reads — so the cap bounds cost, never
/// correctness.
const TRANSCRIPT_HEAD_CAP: usize = 4 * 1024 * 1024;

impl Multiplexer {
    /// Ingest an [`UnboundSignal`]: resolve which panel (if any) it belongs to
    /// and hand the bound signal to the ordinary turn-completion owner. A
    /// signal that resolves to no panel drops `reply`, which the server
    /// answers as allow — the posting hook proceeds untouched.
    ///
    /// The reply slot is passed through only for the **main** panel, mirroring
    /// the exact path where `CAUCUS_HOOK_REPLY=1` is injected into the main
    /// panel alone; every gate `handle_signal_with_reply` applies (compose
    /// hold, alternation) applies here unchanged.
    pub fn handle_unbound_signal(
        &mut self,
        sig: UnboundSignal,
        reply: Option<tokio::sync::oneshot::Sender<StopDirective>>,
    ) {
        let Some(panel_id) = self.resolve_unbound_panel(&sig) else {
            return;
        };
        let wants_reply = Some(panel_id) == self.main_panel_id;
        let bound = TurnSignal {
            session_id: self.session.id,
            panel_id,
            ts: sig.ts,
            kind: sig.kind,
            last_message: sig.last_message,
            transcript_path: sig.transcript_path,
            wants_reply,
            raw_hook_payload: sig.raw_hook_payload,
        };
        self.handle_signal_with_reply(bound, if wants_reply { reply } else { None });
    }

    /// Resolve an unbound signal to this session's panel, or `None` when the
    /// conversation is not (provably) ours. See the module docs for the rule
    /// order; this is the only place an unbound signal acquires a panel id.
    fn resolve_unbound_panel(&mut self, sig: &UnboundSignal) -> Option<PanelId> {
        // Without a conversation id there is nothing to match on — and
        // nothing to key a cache entry by.
        let sid = sig.claude_session_id.as_deref()?;

        // Rule 1 — exact. Checked before the not-ours cache so a panel that
        // *becomes* known (spawned or healed after a cache entry landed) can
        // never be shadowed by it.
        if let Some(id) = self.panel_by_claude_session_id(sid) {
            return Some(id);
        }

        if self.unbound_unclaimed.contains(sid) {
            return None;
        }

        // Cost bound, not a correctness gate: a conversation running outside
        // this session's repo (and outside every panel worktree) cannot be a
        // fork of one of its panels, so its transcript is never read. A
        // payload without `cwd` skips the gate — lineage still decides.
        if let Some(cwd) = sig.cwd.as_deref()
            && !self.unbound_cwd_plausible(cwd)
        {
            self.unbound_unclaimed.insert(sid.to_string());
            return None;
        }

        // Rule 2 — lineage. A missing transcript path or an unreadable file
        // is not cached: both can be transient, and the read costs nothing
        // when it fails.
        let transcript = sig.transcript_path.as_deref()?;
        let (head_ids, complete) = transcript_head_session_ids(transcript)?;
        let matches: Vec<PanelId> = self
            .manifests
            .iter()
            .filter(|(_, m)| m.claude_session_id().is_some_and(|k| head_ids.contains(k)))
            .map(|(&id, _)| id)
            .collect();
        match matches.as_slice() {
            [id] => Some(*id),
            [] => {
                // Only a complete read proves the head names no panel of
                // ours; a capped read may simply not have reached the copied
                // parent records yet.
                if complete {
                    self.unbound_unclaimed.insert(sid.to_string());
                }
                None
            }
            many => {
                // Two panels' conversations named by one head cannot be
                // disambiguated; claiming either could settle the wrong
                // panel. Left uncached so the anomaly stays observable.
                warn!(
                    claude_session_id = %sid,
                    candidates = many.len(),
                    "unbound signal lineage is ambiguous; leaving it unclaimed"
                );
                None
            }
        }
    }

    /// The panel whose manifest holds `sid` as its Claude conversation id.
    fn panel_by_claude_session_id(&self, sid: &str) -> Option<PanelId> {
        self.manifests
            .iter()
            .find(|(_, m)| m.claude_session_id() == Some(sid))
            .map(|(&id, _)| id)
    }

    /// Whether `cwd` could belong to one of this session's panels: inside the
    /// session repo, or inside any panel's worktree (worktrees live *outside*
    /// the repo checkout).
    fn unbound_cwd_plausible(&self, cwd: &Path) -> bool {
        cwd.starts_with(&self.session.repo_path)
            || self
                .manifests
                .values()
                .filter_map(|m| m.worktree_path.as_deref())
                .any(|wt| cwd.starts_with(wt))
    }
}

/// Collect every Claude conversation id named at the top level of the first
/// [`TRANSCRIPT_HEAD_CAP`] bytes of the transcript JSONL at `path`, plus
/// whether the whole file was read (`true`) or the read was capped.
///
/// A fork's transcript carries the records copied from its parent
/// conversation, and each record names its own conversation id as a top-level
/// field — `sessionId` on Claude Code's newer record shapes, `session_id` on
/// older ones, so both spellings are collected. Only top-level fields count:
/// a conversation id merely *mentioned* in message text sits nested under the
/// record's message content and must not create lineage (the main worker's
/// conversation routinely quotes its sub-agents' ids).
///
/// Returns `None` when the file cannot be read at all. A partial trailing
/// line (the cap landed mid-line, or a writer is mid-append) is skipped, as is
/// any line that is not a JSON object.
fn transcript_head_session_ids(path: &Path) -> Option<(HashSet<String>, bool)> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; TRANSCRIPT_HEAD_CAP];
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    buf.truncate(filled);
    let complete = filled < TRANSCRIPT_HEAD_CAP;

    let mut ids = HashSet::new();
    let mut lines = buf.split(|&b| b == b'\n').peekable();
    while let Some(line) = lines.next() {
        // The final fragment has no terminating newline: on a capped read it
        // is mid-line for certain; on a complete read it may be a writer's
        // half-appended record. Skip it either way — the ids it could carry
        // arrive with a later signal's read.
        if lines.peek().is_none() {
            break;
        }
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        for key in ["sessionId", "session_id"] {
            if let Some(sid) = record.get(key).and_then(serde_json::Value::as_str) {
                ids.insert(sid.to_string());
            }
        }
    }
    Some((ids, complete))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::derive_state::DerivedState;
    use crate::agent::manifest::AgentManifest;
    use crate::panel::lifecycle::{Panel, PanelState};
    use crate::role::spec::AgentCli;
    use crate::session::id::AgentId;
    use crate::session::runtime::test_support::*;
    use crate::signal::TurnKind;
    use tempfile::TempDir;

    /// Insert a hermetic `/bin/cat` panel (no real agent CLI needed).
    fn push_cat_panel(mux: &mut Multiplexer, role: &str, state: PanelState) -> PanelId {
        use crate::pty::{Pty, PtyCommand};
        use crate::term::{Grid, OutputCapture};
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

    /// A panel plus a manifest that already knows `claude_sid` — the state
    /// every env-path signal (or a resumed spawn) leaves behind.
    fn panel_with_known_id(mux: &mut Multiplexer, claude_sid: &str) -> PanelId {
        let id = push_cat_panel(mux, "reviewer", PanelState::Working);
        let mut mf = AgentManifest::new(
            mux.session.id,
            id,
            "reviewer",
            "reviewer-1",
            AgentCli::Claude,
            None,
        );
        mf.claude_session_id = Some(claude_sid.to_string());
        mux.manifests.insert(id, mf);
        id
    }

    /// An unbound Stop whose payload names `sid`, `transcript`, and `cwd` —
    /// what `caucus signal post --discover` builds from a real hook payload.
    fn unbound_stop(sid: &str, transcript: Option<&Path>, cwd: &Path) -> UnboundSignal {
        let mut payload = serde_json::json!({
            "session_id": sid,
            "cwd": cwd.display().to_string(),
        });
        if let Some(t) = transcript {
            payload["transcript_path"] = serde_json::json!(t.display().to_string());
        }
        UnboundSignal::now(TurnKind::Stop, Some("done".into()), payload)
    }

    /// Rule 1: the daemon re-hosted the same conversation (env lost, id kept).
    /// The unbound Stop settles the panel exactly as the env path would.
    #[tokio::test]
    async fn unbound_with_a_known_conversation_id_settles_the_panel() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = panel_with_known_id(&mut mux, "conv-known");
        let cwd = mux.session.repo_path.clone();

        mux.handle_unbound_signal(unbound_stop("conv-known", None, &cwd), None);

        assert_eq!(
            mux.panels.iter().find(|p| p.id == id).unwrap().state(),
            PanelState::Idle,
            "an exact-id unbound Stop must settle the panel"
        );
        assert_eq!(mux.manifests[&id].derived_state(), DerivedState::Idle);
        mux.shutdown();
    }

    /// Rule 2 plus the heal: a fork minted a fresh conversation id, but its
    /// transcript head carries records copied from the parent conversation.
    /// The panel settles AND the manifest learns the fork's id, so the next
    /// signal matches by exact id (and `caucus resume` resumes the fork, not
    /// the stale parent).
    #[tokio::test]
    async fn unbound_fork_resolves_by_lineage_and_heals_the_stored_id() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = panel_with_known_id(&mut mux, "conv-parent");
        let cwd = mux.session.repo_path.clone();

        let transcript = tmp.path().join("fork.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                "{\"type\":\"ai-title\",\"sessionId\":\"conv-fork\"}\n",
                "{\"type\":\"assistant\",\"session_id\":\"conv-parent\",\"message\":{}}\n",
            ),
        )
        .unwrap();

        mux.handle_unbound_signal(unbound_stop("conv-fork", Some(&transcript), &cwd), None);

        assert_eq!(
            mux.panels.iter().find(|p| p.id == id).unwrap().state(),
            PanelState::Idle,
            "a lineage-resolved unbound Stop must settle the panel"
        );
        assert_eq!(
            mux.manifests[&id].claude_session_id(),
            Some("conv-fork"),
            "the manifest heals to the fork's conversation id"
        );
        mux.shutdown();
    }

    /// A conversation running outside the repo (and every worktree) is not
    /// ours: nothing changes, and its transcript is never read — the cwd gate
    /// caches the verdict without IO.
    #[tokio::test]
    async fn unbound_from_a_foreign_cwd_is_unclaimed_without_a_transcript_read() {
        let tmp = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = panel_with_known_id(&mut mux, "conv-parent");

        // The transcript WOULD establish lineage — the cwd gate must reject
        // before it is consulted.
        let transcript = elsewhere.path().join("t.jsonl");
        std::fs::write(&transcript, "{\"session_id\":\"conv-parent\"}\n{}\n").unwrap();

        mux.handle_unbound_signal(
            unbound_stop("conv-foreign", Some(&transcript), elsewhere.path()),
            None,
        );

        assert_eq!(
            mux.panels.iter().find(|p| p.id == id).unwrap().state(),
            PanelState::Working,
            "a foreign-cwd signal must not touch any panel"
        );
        assert!(
            mux.unbound_unclaimed.contains("conv-foreign"),
            "the foreign conversation is cached as not-ours"
        );
        mux.shutdown();
    }

    /// The not-ours cache short-circuits: once a conversation is judged
    /// not-ours after a complete read, a later signal is not re-judged — even
    /// if its transcript would now match.
    #[tokio::test]
    async fn unbound_unclaimed_verdict_is_cached_after_a_complete_read() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = panel_with_known_id(&mut mux, "conv-parent");
        let cwd = mux.session.repo_path.clone();

        // First signal: transcript names no known conversation → not ours,
        // cached (the file was read to EOF).
        let transcript = tmp.path().join("t.jsonl");
        std::fs::write(&transcript, "{\"sessionId\":\"conv-stranger\"}\n{}\n").unwrap();
        mux.handle_unbound_signal(unbound_stop("conv-stranger", Some(&transcript), &cwd), None);
        assert!(mux.unbound_unclaimed.contains("conv-stranger"));

        // Rewrite the transcript so lineage WOULD match now: the cache must
        // still answer, proving no re-read happens.
        std::fs::write(&transcript, "{\"session_id\":\"conv-parent\"}\n{}\n").unwrap();
        mux.handle_unbound_signal(unbound_stop("conv-stranger", Some(&transcript), &cwd), None);
        assert_eq!(
            mux.panels.iter().find(|p| p.id == id).unwrap().state(),
            PanelState::Working,
            "a cached not-ours conversation stays unclaimed"
        );
        mux.shutdown();
    }

    /// A head naming two known conversations cannot be disambiguated: neither
    /// panel is touched, and the verdict is NOT cached (the anomaly stays
    /// observable on every signal).
    #[tokio::test]
    async fn unbound_ambiguous_lineage_claims_no_panel() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let a = panel_with_known_id(&mut mux, "conv-a");
        let b = panel_with_known_id(&mut mux, "conv-b");
        let cwd = mux.session.repo_path.clone();

        let transcript = tmp.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            "{\"session_id\":\"conv-a\"}\n{\"session_id\":\"conv-b\"}\n{}\n",
        )
        .unwrap();

        mux.handle_unbound_signal(unbound_stop("conv-fork", Some(&transcript), &cwd), None);

        for id in [a, b] {
            assert_eq!(
                mux.panels.iter().find(|p| p.id == id).unwrap().state(),
                PanelState::Working,
                "ambiguous lineage must claim neither panel"
            );
        }
        assert!(
            !mux.unbound_unclaimed.contains("conv-fork"),
            "ambiguity is not cached — it must stay observable"
        );
        mux.shutdown();
    }

    /// No conversation id in the payload → nothing to match or cache.
    #[tokio::test]
    async fn unbound_without_a_conversation_id_is_unclaimed() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = panel_with_known_id(&mut mux, "conv-known");

        let sig = UnboundSignal::now(TurnKind::Stop, None, serde_json::json!({}));
        mux.handle_unbound_signal(sig, None);

        assert_eq!(
            mux.panels.iter().find(|p| p.id == id).unwrap().state(),
            PanelState::Working
        );
        assert!(mux.unbound_unclaimed.is_empty(), "nothing to cache by");
        mux.shutdown();
    }

    /// A resolved **main** panel keeps the hook-reply channel: a due
    /// deliverable (here a queued question notice) rides back on the unbound
    /// signal's reply slot exactly as on the env path — a re-hosted main loses
    /// neither its signals nor its round/notice delivery.
    #[tokio::test]
    async fn unbound_main_stop_rides_the_reply_slot() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = panel_with_known_id(&mut mux, "conv-main");
        mux.main_panel_id = Some(main);
        let cwd = mux.session.repo_path.clone();
        mux.enqueue_question_notice(PanelId::new(), "reviewer".into(), "which API?".into());

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        mux.handle_unbound_signal(unbound_stop("conv-main", None, &cwd), Some(tx));

        let StopDirective::Deliver { reason } = rx.try_recv().expect("directive must be sent");
        assert!(
            reason.contains("which API?"),
            "the queued notice rides the unbound reply: {reason}"
        );
        mux.shutdown();
    }

    /// A resolved **sub** panel never consumes the reply slot: it is dropped
    /// (allow) even though the server attached one, mirroring the env path
    /// where only the main panel gets `CAUCUS_HOOK_REPLY=1`.
    #[tokio::test]
    async fn unbound_sub_stop_drops_the_reply_slot() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_cat_panel(&mut mux, "main", PanelState::Working);
        mux.main_panel_id = Some(main);
        let sub = panel_with_known_id(&mut mux, "conv-sub");
        let cwd = mux.session.repo_path.clone();
        mux.enqueue_question_notice(PanelId::new(), "reviewer".into(), "pending?".into());

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        mux.handle_unbound_signal(unbound_stop("conv-sub", None, &cwd), Some(tx));

        assert!(
            rx.try_recv().is_err(),
            "a sub panel's unbound Stop must drop the reply (allow)"
        );
        assert_eq!(
            mux.panels.iter().find(|p| p.id == sub).unwrap().state(),
            PanelState::Idle,
            "the sub panel still settles"
        );
        mux.shutdown();
    }

    /// The head scanner: both id spellings are collected, only top-level
    /// fields count, the unterminated tail is skipped, and a complete read
    /// reports `complete = true`.
    #[test]
    fn transcript_head_scan_collects_top_level_ids_only() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("t.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"sessionId\":\"camel\"}\n",
                "{\"session_id\":\"snake\"}\n",
                "{\"message\":{\"session_id\":\"nested\"}}\n",
                "not json\n",
                "{\"session_id\":\"tail-partial\"",
            ),
        )
        .unwrap();
        let (ids, complete) = transcript_head_session_ids(&path).expect("readable");
        assert!(complete, "the whole file fits under the cap");
        assert!(ids.contains("camel") && ids.contains("snake"));
        assert!(
            !ids.contains("nested"),
            "a nested mention must not create lineage"
        );
        assert!(
            !ids.contains("tail-partial"),
            "the unterminated tail line is skipped"
        );
    }

    /// A missing file is `None` (transient — never cached by the caller).
    #[test]
    fn transcript_head_scan_of_a_missing_file_is_none() {
        assert!(transcript_head_session_ids(Path::new("/nonexistent/t.jsonl")).is_none());
    }
}
