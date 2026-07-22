//! Lane events: append-only timeline per agent (`docs/design.md` §8.2).
//! "Lane" is claw-code's term for one agent's work flow.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::provenance::{LaneCommitProvenance, SupersededBy};
use crate::signal::NoteKind;

/// Failure-class taxonomy for blockers (`docs/design.md` §8.3).
///
/// One variant per way a turn can end badly, and caucus learns that only from
/// the turn signal's `kind` (`crate::signal::TurnKind`) — so this enum is the
/// image of the non-`Stop` kinds, nothing more. `tool_blocked` is a
/// [`Self::PermissionPrompt`]; `error` is a [`Self::Transport`]. Do not add a
/// class ahead of a signal that produces it: an unproducible class is a dead
/// branch in `derive_state::blocker_state` that reads like a live one.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneFailureClass {
    /// The agent's turn stopped on a tool-permission prompt (`tool_blocked`).
    PermissionPrompt,
    /// The turn ended on a transport-level error (`error`).
    Transport,
}

/// A blocker attached to a `Blocked` or `Failed` lane event.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaneEventBlocker {
    pub failure_class: LaneFailureClass,
    pub detail: String,
}

impl LaneEventBlocker {
    pub fn new(failure_class: LaneFailureClass, detail: impl Into<String>) -> Self {
        Self {
            failure_class,
            detail: detail.into(),
        }
    }
}

/// Discriminant + payload for a lane event (`docs/design.md` §8.2).
///
/// Every variant is produced by exactly one place in the running session, named
/// in its doc below. A variant with no producer does not belong here: a timeline
/// that *could* say something it never says is worse than one that cannot, since
/// a reader waits for a record that will never appear. `LANE_EVENT_PRODUCERS` in
/// the tests pins that, one line per variant.
///
/// There is deliberately no `Finished`. caucus's only completion signal is the
/// backend's turn-completion hook, so "the agent finished its task" and "the
/// agent's turn ended" are the same event arriving once — which is why a
/// sub-agent's system prompt states the contract outright (`SUBAGENT_TURN_CONTRACT`:
/// ending your turn claims the work is done) instead of caucus pretending it can
/// tell the two apart.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaneEventKind {
    /// Agent process started. Produced by `AgentManifest::new`.
    Started,
    /// A prompt was delivered to the panel — by the main worker's `send_keys`,
    /// or by the user typing into it. Produced by
    /// `manifest::record_prompt_delivered`, the sole writer of this event and
    /// the turn-phase transition back into `Working` (the mirror of
    /// `record_turn_completed`); the runtime's `Idle -> Working` paths route
    /// through it.
    PromptDelivered,
    /// A turn signal was received for this panel, whatever kind it carried: the
    /// turn ended. Produced by `manifest::record_turn_completed`.
    TurnCompleted,
    /// A local slash command (`/compact`, `/clear`) finished in the panel.
    /// Such a command runs no agent turn, so no Stop hook ever fires for it —
    /// its completion arrives as a lifecycle signal (`PostCompact` /
    /// `SessionStart` hooks, `docs/design.md` §7) and settles the panel the
    /// way a turn signal would. Produced by
    /// `manifest::record_local_command_completed`.
    LocalCommandCompleted { command: String },
    /// The turn ended blocked (recoverable) — see `blocker`. Produced by
    /// `manifest::record_turn_completed` from a `tool_blocked` signal.
    Blocked { blocker: LaneEventBlocker },
    /// The turn ended in failure — see `blocker`. Produced by
    /// `manifest::record_turn_completed` from an `error` signal.
    Failed { blocker: LaneEventBlocker },
    /// The agent named a commit that exists in its own worktree. Produced by
    /// `Multiplexer::record_commit_provenance` from the turn signal's final
    /// message, verified with `git rev-parse`.
    CommitCreated { provenance: LaneCommitProvenance },
    /// A commit this agent created is no longer on its branch: the agent
    /// amended or rebased it away. Produced by
    /// `Multiplexer::record_commit_supersessions`, from the branch itself.
    ///
    /// This is why `LaneCommitProvenance` has no `superseded_by` field. The
    /// timeline is append-only, so a commit's later fate is a later event, not
    /// a back-write into the `CommitCreated` that announced it. A commit's
    /// standing is *read* from the pair: created and never named here → live;
    /// named here → superseded, by the commit given or by something unnameable.
    /// `AgentManifest::live_commits` is that derivation, and the chain of these
    /// events is the lineage — neither is stored twice.
    CommitSuperseded { commit: String, by: SupersededBy },
    /// A mid-turn note the agent posted (`caucus signal note`) — progress, an
    /// artifact reference, or a question. Produced by `manifest::record_note`.
    /// Deliberately transition-free: the panel stays `Working`.
    NoteRecorded { note_kind: NoteKind, body: String },
    /// A desktop-notification escape (OSC 9 / 99 / 777) the panel's process
    /// emitted — an in-band attention signal from tools with no hook channel
    /// (`docs/design.md` §7.7). Produced by `manifest::record_notification`.
    /// Capture only: no state transition and no settle semantics — if it ever
    /// hints settle (D-2), that must route through the turn-completion owner.
    NotificationSeen { body: String },
    /// A worktree was created for this agent. Produced at the manifest's first
    /// write in `Multiplexer::spawn_panel_inner`.
    WorktreeCreated { path: PathBuf },
    /// The agent's worktree was removed. Produced by the cleanup worker
    /// (`worktree::cleanup::record_removals`) from what it actually removed —
    /// the multiplexer cannot write this one; see `WorktreeOwner`.
    WorktreeRemoved { path: PathBuf },
}

/// One timestamped lane event.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaneEvent {
    pub kind: LaneEventKind,
    pub ts: DateTime<Utc>,
}

impl LaneEvent {
    /// Build an event stamped with `Utc::now()`.
    pub fn now(kind: LaneEventKind) -> Self {
        Self {
            kind,
            ts: Utc::now(),
        }
    }

    /// Build a `Started` event at `ts`.
    pub fn started(ts: DateTime<Utc>) -> Self {
        Self {
            kind: LaneEventKind::Started,
            ts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_completed_serde_roundtrip() {
        let ev = LaneEvent::now(LaneEventKind::TurnCompleted);
        let s = serde_json::to_string(&ev).unwrap();
        let back: LaneEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
        assert!(s.contains("\"kind\":\"turn_completed\""));
    }

    #[test]
    fn blocked_carries_blocker() {
        let ev = LaneEvent::now(LaneEventKind::Blocked {
            blocker: LaneEventBlocker::new(LaneFailureClass::PermissionPrompt, "y/n prompt"),
        });
        let s = serde_json::to_string(&ev).unwrap();
        let back: LaneEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    /// Every variant is written by a real production site — the invariant this
    /// enum exists under (see its doc). Two ways to break it, both caught here:
    ///
    /// - **Add a variant with no producer.** The `match` is exhaustive, so it
    ///   fails to compile until someone names the site that writes it.
    /// - **Delete the producer, keep the variant.** Each arm names the source it
    ///   is written from, and this searches that file's *production* half — the
    ///   text before `#[cfg(test)]` — so a producer that survives only in a test
    ///   does not count.
    ///
    /// The alternative is what this enum used to be: seven variants no code
    /// constructed, a timeline that advertised events it never emitted, and a
    /// `render_md` that formatted labels no reader would ever see.
    #[test]
    fn every_lane_event_kind_is_written_by_a_production_site() {
        /// The half of a source file before its `#[cfg(test)]` module.
        fn production(src: &str) -> &str {
            src.split("#[cfg(test)]").next().unwrap()
        }
        let manifest = production(include_str!("manifest.rs"));
        let input = production(include_str!("../session/runtime/input.rs"));
        let spawn = production(include_str!("../session/runtime/spawn.rs"));
        let cleanup = production(include_str!("../worktree/cleanup.rs"));

        // (variant, the file that writes it, the text that writes it)
        let producer = |kind: &LaneEventKind| -> (&str, &str) {
            match kind {
                LaneEventKind::Started => (manifest, "LaneEvent::started(now)"),
                LaneEventKind::PromptDelivered => {
                    (manifest, "LaneEvent::now(LaneEventKind::PromptDelivered)")
                }
                LaneEventKind::TurnCompleted => {
                    (manifest, "LaneEvent::now(LaneEventKind::TurnCompleted)")
                }
                LaneEventKind::LocalCommandCompleted { .. } => {
                    (manifest, "LaneEventKind::LocalCommandCompleted {")
                }
                LaneEventKind::Blocked { .. } => (manifest, "LaneEventKind::Blocked { blocker"),
                LaneEventKind::Failed { .. } => (manifest, "LaneEventKind::Failed { blocker"),
                LaneEventKind::CommitCreated { .. } => (input, "LaneEventKind::CommitCreated {"),
                LaneEventKind::CommitSuperseded { .. } => {
                    (input, "LaneEventKind::CommitSuperseded {")
                }
                LaneEventKind::NoteRecorded { .. } => (manifest, "LaneEventKind::NoteRecorded {"),
                LaneEventKind::NotificationSeen { .. } => {
                    (manifest, "LaneEventKind::NotificationSeen {")
                }
                LaneEventKind::WorktreeCreated { .. } => {
                    (spawn, "LaneEventKind::WorktreeCreated {")
                }
                LaneEventKind::WorktreeRemoved { .. } => {
                    (cleanup, "LaneEventKind::WorktreeRemoved {")
                }
            }
        };

        let blocker = LaneEventBlocker::new(LaneFailureClass::Transport, "x");
        let path = PathBuf::from("/tmp/wt");
        for kind in [
            LaneEventKind::Started,
            LaneEventKind::PromptDelivered,
            LaneEventKind::TurnCompleted,
            LaneEventKind::LocalCommandCompleted {
                command: "/compact".into(),
            },
            LaneEventKind::Blocked {
                blocker: blocker.clone(),
            },
            LaneEventKind::Failed { blocker },
            LaneEventKind::CommitCreated {
                provenance: LaneCommitProvenance {
                    commit: "abc1234".into(),
                    branch: "b".into(),
                    worktree: None,
                },
            },
            LaneEventKind::CommitSuperseded {
                commit: "abc1234".into(),
                by: SupersededBy::Unknown,
            },
            LaneEventKind::NoteRecorded {
                note_kind: NoteKind::Progress,
                body: "half done".into(),
            },
            LaneEventKind::NotificationSeen {
                body: "build finished".into(),
            },
            LaneEventKind::WorktreeCreated { path: path.clone() },
            LaneEventKind::WorktreeRemoved { path },
        ] {
            let (src, needle) = producer(&kind);
            assert!(
                src.contains(needle),
                "{kind:?} has no production site writing {needle:?} — a lane event \
                 nothing emits must not exist"
            );
        }
    }
}
