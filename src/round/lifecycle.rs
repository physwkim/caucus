//! Round lifecycle: lay out a round's directory, distribute the agenda, and
//! report response-collection status. Built on top of `agent::spawn` and
//! `tmux::TmuxService` — the orchestration logic lives here; the IO
//! primitives live in their respective modules.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::agent::derive_state::{DerivedState, PaneScreenHint};
use crate::agent::manifest::{AgentManifest, ManifestError, write_json};
use crate::sentinel::{Sentinel, read_sentinel};
use crate::session::id::AgentId;
use crate::tmux::{TmuxError, TmuxService};

/// Per-role layout under a session root.
#[derive(Debug, Clone)]
pub struct RoundLayout {
    pub session_root: PathBuf,
    pub round_number: u32,
}

impl RoundLayout {
    pub fn new(session_root: PathBuf, round_number: u32) -> Self {
        Self {
            session_root,
            round_number,
        }
    }

    pub fn round_dir(&self) -> PathBuf {
        self.session_root
            .join(format!("round-{:02}", self.round_number))
    }

    pub fn agenda_path(&self) -> PathBuf {
        self.round_dir().join("agenda.md")
    }

    pub fn response_path(&self, role: &str) -> PathBuf {
        self.round_dir().join(format!("response-{role}.md"))
    }

    pub fn system_prompt_path(&self, role: &str) -> PathBuf {
        self.round_dir().join(format!("system-{role}.md"))
    }
}

#[derive(Debug, Error)]
pub enum RoundError {
    #[error("round io ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("agenda not provided and no existing agenda at {0}")]
    NoAgenda(PathBuf),
    #[error(transparent)]
    Tmux(#[from] TmuxError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

/// Lay out the round directory: create `round-NN/`, copy the agenda in,
/// render per-role system prompts (concatenated from the role template).
pub fn prepare_round(
    layout: &RoundLayout,
    roles: &[(String, PathBuf /* template path */)],
    agenda_source: &Path,
) -> Result<(), RoundError> {
    let dir = layout.round_dir();
    std::fs::create_dir_all(&dir).map_err(|source| RoundError::Io {
        path: dir.clone(),
        source,
    })?;
    let agenda_dest = layout.agenda_path();
    std::fs::copy(agenda_source, &agenda_dest).map_err(|source| RoundError::Io {
        path: agenda_dest.clone(),
        source,
    })?;

    for (role, template) in roles {
        let body = std::fs::read_to_string(template).map_err(|source| RoundError::Io {
            path: template.clone(),
            source,
        })?;
        let sys_path = layout.system_prompt_path(role);
        std::fs::write(&sys_path, body).map_err(|source| RoundError::Io {
            path: sys_path.clone(),
            source,
        })?;
    }

    Ok(())
}

/// Status of one role's response in the current round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleStatus {
    pub role: String,
    pub agent_id: AgentId,
    pub sentinel_present: bool,
    pub response_bytes: Option<u64>,
    pub derived_state: DerivedState,
    #[serde(default)]
    pub last_event_ts: Option<DateTime<Utc>>,
    #[serde(default)]
    pub current_pane_hint: Option<PaneScreenHint>,
}

/// Aggregate over a single round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundStatus {
    pub round_number: u32,
    pub roles: Vec<RoleStatus>,
    /// True if every role's response file is non-empty.
    pub all_responses_complete: bool,
}

/// Collect the current status of every role's response in this round.
pub fn round_status(
    layout: &RoundLayout,
    role_to_agent: &[(String, AgentId)],
) -> Result<RoundStatus, RoundError> {
    let mut roles = Vec::with_capacity(role_to_agent.len());
    for (role, agent_id) in role_to_agent {
        let sentinel = read_sentinel(&layout.session_root, *agent_id).ok();
        let response_path = layout.response_path(role);
        let response_bytes = std::fs::metadata(&response_path).map(|m| m.len()).ok();
        let response_non_empty = response_bytes.unwrap_or(0) > 0;
        let manifest_path = AgentManifest::json_path(&layout.session_root, *agent_id);
        let manifest = read_manifest(&manifest_path).ok();
        let derived_state = manifest
            .as_ref()
            .map(|m| m.derived_state)
            .unwrap_or(DerivedState::Working);
        let last_event_ts = manifest
            .as_ref()
            .and_then(|m| m.lane_events.last().map(|e| e.ts()));
        let current_pane_hint = manifest.as_ref().and_then(|m| m.current_pane_hint);
        roles.push(RoleStatus {
            role: role.clone(),
            agent_id: *agent_id,
            sentinel_present: sentinel.is_some(),
            response_bytes,
            derived_state: if response_non_empty && matches!(derived_state, DerivedState::Working) {
                DerivedState::FinishedCleanable
            } else {
                derived_state
            },
            last_event_ts,
            current_pane_hint,
        });
    }
    let all_responses_complete = roles.iter().all(|r| r.response_bytes.unwrap_or(0) > 0);
    Ok(RoundStatus {
        round_number: layout.round_number,
        roles,
        all_responses_complete,
    })
}

fn read_manifest(path: &Path) -> std::io::Result<AgentManifest> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(std::io::Error::other)
}

/// Append a sentinel-derived event to the manifest, then re-write atomically.
/// This is the bridge between `sentinel::watcher` and `agent::manifest`.
pub fn record_sentinel(
    session_root: &Path,
    sentinel: &Sentinel,
) -> Result<AgentManifest, RoundError> {
    use crate::agent::derive_state::{DerivedState, RawStatus, derive};
    use crate::agent::lane_event::LaneEvent;
    use crate::agent::manifest::read_json;
    use crate::sentinel::SentinelKind;

    let mut manifest = read_json(session_root, sentinel.agent_id)?;
    manifest.lane_events.push(LaneEvent::SentinelReceived {
        ts: sentinel.ts,
        sentinel_kind: format!("{:?}", sentinel.kind).to_lowercase(),
    });
    // Capture the Claude session id from the first hook payload that carries
    // one. It's stable across rounds for the same `claude` process, so we
    // only set it once and never overwrite. `caucus execute start
    // --continue-meeting` reads this to invoke `claude --resume <id>`.
    if manifest.claude_session_id.is_none() {
        if let Some(payload) = sentinel.raw_hook_payload.as_ref() {
            if let Some(sid) = payload.get("session_id").and_then(|v| v.as_str()) {
                manifest.claude_session_id = Some(sid.to_string());
            }
        }
    }
    let (raw, error) = match sentinel.kind {
        SentinelKind::Stop => (RawStatus::Completed, None),
        SentinelKind::ToolBlocked => (
            RawStatus::Failed,
            Some(format!(
                "tool blocked: {}",
                sentinel.last_message.clone().unwrap_or_default()
            )),
        ),
        SentinelKind::Error => (
            RawStatus::Failed,
            Some(
                sentinel
                    .last_message
                    .clone()
                    .unwrap_or_else(|| "error".into()),
            ),
        ),
        SentinelKind::Killed => (RawStatus::Failed, Some("killed by orchestrator".into())),
    };
    manifest.status = raw;
    manifest.error = error.clone();
    manifest.completed_at = Some(sentinel.ts);

    let response_non_empty = manifest
        .lane_events
        .iter()
        .any(|ev| matches!(ev, LaneEvent::ResponseFileWritten { bytes, .. } if *bytes > 0));
    manifest.derived_state = derive(raw, response_non_empty, error.as_deref(), None);

    if matches!(manifest.derived_state, DerivedState::FinishedCleanable)
        || matches!(manifest.derived_state, DerivedState::FinishedPendingReport)
    {
        manifest.lane_events.push(LaneEvent::Finished {
            ts: sentinel.ts,
            detail: sentinel
                .last_message
                .clone()
                .unwrap_or_else(|| "stopped".into()),
        });
    }

    write_json(&manifest, session_root)?;
    Ok(manifest)
}

/// Record a pane-screen-hint transition on one agent's manifest.
///
/// The single owner for the `current_pane_hint` field — both this helper
/// and [`record_pane_gone`] are the only sites that mutate it (audit:
/// `rg -n "manifest\.current_pane_hint\s*=" src/`). Routes through
/// [`write_json`], honouring invariant I-2.
///
/// Returns `Ok(true)` if the hint changed (write occurred), `Ok(false)`
/// on a dedup hit (no write).
pub fn record_pane_hint(
    session_root: &Path,
    agent_id: AgentId,
    current: Option<PaneScreenHint>,
) -> Result<bool, RoundError> {
    use crate::agent::derive_state::derive;
    use crate::agent::lane_event::LaneEvent;
    use crate::agent::manifest::read_json;

    let mut manifest = read_json(session_root, agent_id)?;
    let previous = manifest.current_pane_hint;
    if previous == current {
        return Ok(false);
    }
    let now = chrono::Utc::now();
    manifest.lane_events.push(LaneEvent::PaneHintChanged {
        ts: now,
        previous,
        current,
    });
    manifest.current_pane_hint = current;
    let response_non_empty = manifest
        .lane_events
        .iter()
        .any(|ev| matches!(ev, LaneEvent::ResponseFileWritten { bytes, .. } if *bytes > 0));
    manifest.derived_state = derive(
        manifest.status,
        response_non_empty,
        manifest.error.as_deref(),
        current,
    );
    write_json(&manifest, session_root)?;
    Ok(true)
}

/// Record that the pane backing one agent has gone (poller's
/// `capture_pane` failed). Sets `derived_state` to
/// [`DerivedState::InterruptedTransport`] (architect D4); the regular
/// `derive()` path is not used because the pane-loss signal is not
/// reflected in `RawStatus`.
pub fn record_pane_gone(
    session_root: &Path,
    agent_id: AgentId,
    pane: String,
) -> Result<AgentManifest, RoundError> {
    use crate::agent::lane_event::LaneEvent;
    use crate::agent::manifest::read_json;

    let mut manifest = read_json(session_root, agent_id)?;
    let now = chrono::Utc::now();
    manifest
        .lane_events
        .push(LaneEvent::PaneGone { ts: now, pane });
    manifest.current_pane_hint = None;
    manifest.derived_state = DerivedState::InterruptedTransport;
    write_json(&manifest, session_root)?;
    Ok(manifest)
}

/// Send the round's bootstrap message to one role's pane. Used when the
/// pane is already alive and we want to start the next round without
/// re-spawning the `claude` process.
pub async fn nudge_role(
    tmux: &TmuxService,
    pane_id: &str,
    layout: &RoundLayout,
    role: &str,
) -> Result<(), RoundError> {
    nudge_pane_with_brief(
        tmux,
        pane_id,
        &layout.agenda_path(),
        &layout.response_path(role),
    )
    .await
}

/// Lower-level form of [`nudge_role`]: deliver an explicit brief path +
/// response path to a pane. Used by the architect-led round flow where
/// followers receive a brief that quotes the lead's response rather than
/// the round's shared agenda.
pub async fn nudge_pane_with_brief(
    tmux: &TmuxService,
    pane_id: &str,
    brief_path: &Path,
    response_path: &Path,
) -> Result<(), RoundError> {
    let message = format!(
        "Read {brief} and write your reply to {response}. \
         Finish with a one-line summary.",
        brief = brief_path.display(),
        response = response_path.display(),
    );
    tmux.send_text(pane_id, &message, true).await?;
    Ok(())
}

/// Write the follower brief for one role in an architect-led round. The
/// brief quotes the lead's response and frames the follower's job as
/// "react from your role's angle" rather than starting from scratch.
/// Returns the path the brief was saved to (under
/// `round-NN/follower-brief-<role>.md`) so the CEO can audit it later.
pub fn write_follower_brief(
    layout: &RoundLayout,
    follower_role: &str,
    lead_role: &str,
) -> Result<PathBuf, RoundError> {
    let dir = layout.round_dir();
    std::fs::create_dir_all(&dir).map_err(|source| RoundError::Io {
        path: dir.clone(),
        source,
    })?;
    let path = dir.join(format!("follower-brief-{follower_role}.md"));
    let body = compose_follower_brief(layout, follower_role, lead_role);
    std::fs::write(&path, body).map_err(|source| RoundError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// Pure formatter — easier to unit-test than the file writer.
pub fn compose_follower_brief(
    layout: &RoundLayout,
    follower_role: &str,
    lead_role: &str,
) -> String {
    let lead_response = layout.response_path(lead_role);
    let agenda = layout.agenda_path();
    format!(
        "# Round {n} follower brief — {follower}\n\n\
         The lead role (**{lead}**) just submitted their take on this round's \
         agenda. Read their response **first** before anything else:\n\n\
         - {lead_response}\n\n\
         The original agenda is at:\n\n\
         - {agenda}\n\n\
         Your job, as the **{follower}** role: react to the lead's proposal \
         from your role's angle. Specifically:\n\n\
         - Confirm, sharpen, or push back on the lead's recommendation. Cite \
           specific bullets / paragraphs from the lead response — don't just \
           rewrite it in your own words.\n\
         - Surface concrete failure modes / risks / costs the lead missed \
           from your perspective.\n\
         - If you disagree with the lead's recommendation, say so explicitly \
           and name your alternative.\n\
         - Keep your response tight (≤ 1 screen). The CEO synthesises across \
           all follower responses; brevity helps.\n",
        n = layout.round_number,
        follower = follower_role,
        lead = lead_role,
        lead_response = lead_response.display(),
        agenda = agenda.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::derive_state::{DerivedState, RawStatus};
    use crate::agent::lane_event::LaneEvent;
    use crate::agent::manifest::{AgentKind, AgentManifest};
    use crate::sentinel::writer::{Sentinel, SentinelKind, write_sentinel};
    use crate::session::id::SessionId;
    use tempfile::TempDir;

    #[test]
    fn follower_brief_quotes_lead_response_path() {
        let tmp = TempDir::new().unwrap();
        let layout = RoundLayout::new(tmp.path().to_path_buf(), 2);
        let brief = compose_follower_brief(&layout, "backend", "architect");
        // Must reference both files by absolute path.
        assert!(brief.contains(layout.response_path("architect").to_string_lossy().as_ref()));
        assert!(brief.contains(layout.agenda_path().to_string_lossy().as_ref()));
        // Must name both roles by ID so a reader knows who they are.
        assert!(brief.contains("**architect**"));
        assert!(brief.contains("**backend**"));
        // Must instruct the follower NOT to rewrite the lead's response.
        assert!(brief.contains("don't just rewrite"));
    }

    #[test]
    fn write_follower_brief_persists_to_disk() {
        let tmp = TempDir::new().unwrap();
        let layout = RoundLayout::new(tmp.path().to_path_buf(), 1);
        let path = write_follower_brief(&layout, "reviewer", "architect").unwrap();
        assert!(path.exists());
        assert!(path.ends_with("follower-brief-reviewer.md"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("Round 1 follower brief — reviewer"));
    }

    #[test]
    fn layout_paths_match_design() {
        let tmp = TempDir::new().unwrap();
        let layout = RoundLayout::new(tmp.path().to_path_buf(), 3);
        assert_eq!(layout.round_dir(), tmp.path().join("round-03"));
        assert_eq!(
            layout.agenda_path(),
            tmp.path().join("round-03").join("agenda.md")
        );
        assert_eq!(
            layout.response_path("reviewer"),
            tmp.path().join("round-03").join("response-reviewer.md")
        );
    }

    #[test]
    fn prepare_round_writes_agenda_and_system_prompts() {
        let tmp = TempDir::new().unwrap();
        let layout = RoundLayout::new(tmp.path().to_path_buf(), 1);
        let agenda = tmp.path().join("agenda-src.md");
        std::fs::write(&agenda, "# topic\n").unwrap();
        let tmpl_arch = tmp.path().join("templates").join("architect.md");
        std::fs::create_dir_all(tmpl_arch.parent().unwrap()).unwrap();
        std::fs::write(&tmpl_arch, "you are architect").unwrap();

        prepare_round(&layout, &[("architect".into(), tmpl_arch.clone())], &agenda).unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.agenda_path()).unwrap(),
            "# topic\n"
        );
        assert_eq!(
            std::fs::read_to_string(layout.system_prompt_path("architect")).unwrap(),
            "you are architect"
        );
    }

    #[test]
    fn round_status_reports_per_role_completion() {
        let tmp = TempDir::new().unwrap();
        let session_root = tmp.path().to_path_buf();
        std::fs::create_dir_all(session_root.join("round-01")).unwrap();
        std::fs::create_dir_all(session_root.join("agents")).unwrap();

        let manifest = AgentManifest::new(
            SessionId::new(),
            "reviewer".into(),
            "reviewer".into(),
            AgentKind::Meeting,
            None,
        );
        let agent_id = manifest.agent_id;
        write_json(&manifest, &session_root).unwrap();

        let layout = RoundLayout::new(session_root.clone(), 1);
        let status_before = round_status(&layout, &[("reviewer".into(), agent_id)]).unwrap();
        assert!(!status_before.all_responses_complete);

        // Write the response file → status should now report complete.
        std::fs::write(layout.response_path("reviewer"), "# review\n- ok").unwrap();
        let status_after = round_status(&layout, &[("reviewer".into(), agent_id)]).unwrap();
        assert!(status_after.all_responses_complete);
        assert!(status_after.roles[0].response_bytes.unwrap() > 0);
    }

    #[test]
    fn record_sentinel_finishes_cleanly_for_stop() {
        let tmp = TempDir::new().unwrap();
        let session_root = tmp.path().to_path_buf();
        std::fs::create_dir_all(session_root.join("agents")).unwrap();

        let mut manifest = AgentManifest::new(
            SessionId::new(),
            "qa".into(),
            "qa".into(),
            AgentKind::Meeting,
            None,
        );
        // Pretend the response was written first.
        manifest.lane_events.push(LaneEvent::ResponseFileWritten {
            ts: chrono::Utc::now(),
            path: PathBuf::from("/tmp/r.md"),
            bytes: 42,
        });
        write_json(&manifest, &session_root).unwrap();

        let sentinel = Sentinel::new(
            manifest.session_id,
            manifest.agent_id,
            SentinelKind::Stop,
            Some("done".into()),
            None,
        );
        write_sentinel(&session_root, &sentinel).unwrap();
        let updated = record_sentinel(&session_root, &sentinel).unwrap();
        assert!(matches!(updated.status, RawStatus::Completed));
        assert!(matches!(
            updated.derived_state,
            DerivedState::FinishedCleanable
        ));
        assert!(
            updated
                .lane_events
                .iter()
                .any(|e| matches!(e, LaneEvent::Finished { .. }))
        );
    }

    #[test]
    fn record_pane_hint_updates_derived_state() {
        let tmp = TempDir::new().unwrap();
        let session_root = tmp.path().to_path_buf();
        std::fs::create_dir_all(session_root.join("agents")).unwrap();

        let manifest = AgentManifest::new(
            SessionId::new(),
            "backend".into(),
            "backend".into(),
            AgentKind::Meeting,
            None,
        );
        let agent_id = manifest.agent_id;
        write_json(&manifest, &session_root).unwrap();

        let changed = record_pane_hint(
            &session_root,
            agent_id,
            Some(PaneScreenHint::PermissionPromptVisible),
        )
        .unwrap();
        assert!(changed);
        let updated = crate::agent::manifest::read_json(&session_root, agent_id).unwrap();
        assert_eq!(
            updated.current_pane_hint,
            Some(PaneScreenHint::PermissionPromptVisible)
        );
        assert!(matches!(
            updated.derived_state,
            DerivedState::BlockedPermissionPrompt
        ));
        assert_eq!(
            updated
                .lane_events
                .iter()
                .filter(|e| matches!(e, LaneEvent::PaneHintChanged { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn record_pane_hint_dedups_when_unchanged() {
        let tmp = TempDir::new().unwrap();
        let session_root = tmp.path().to_path_buf();
        std::fs::create_dir_all(session_root.join("agents")).unwrap();

        let manifest = AgentManifest::new(
            SessionId::new(),
            "qa".into(),
            "qa".into(),
            AgentKind::Meeting,
            None,
        );
        let agent_id = manifest.agent_id;
        write_json(&manifest, &session_root).unwrap();

        let first = record_pane_hint(
            &session_root,
            agent_id,
            Some(PaneScreenHint::EscToInterruptVisible),
        )
        .unwrap();
        assert!(first, "first call must write");
        let second = record_pane_hint(
            &session_root,
            agent_id,
            Some(PaneScreenHint::EscToInterruptVisible),
        )
        .unwrap();
        assert!(!second, "same hint must dedup");

        let updated = crate::agent::manifest::read_json(&session_root, agent_id).unwrap();
        // Only one PaneHintChanged event should be present.
        let hint_events = updated
            .lane_events
            .iter()
            .filter(|e| matches!(e, LaneEvent::PaneHintChanged { .. }))
            .count();
        assert_eq!(hint_events, 1);
    }

    #[test]
    fn record_pane_gone_sets_interrupted_transport() {
        let tmp = TempDir::new().unwrap();
        let session_root = tmp.path().to_path_buf();
        std::fs::create_dir_all(session_root.join("agents")).unwrap();

        let manifest = AgentManifest::new(
            SessionId::new(),
            "reviewer".into(),
            "reviewer".into(),
            AgentKind::Meeting,
            None,
        );
        let agent_id = manifest.agent_id;
        write_json(&manifest, &session_root).unwrap();

        let updated = record_pane_gone(&session_root, agent_id, "%42".into()).unwrap();
        // status must not be flipped — pane-loss is not a status transition.
        assert!(matches!(updated.status, RawStatus::Running));
        assert!(matches!(
            updated.derived_state,
            DerivedState::InterruptedTransport
        ));
        assert_eq!(updated.current_pane_hint, None);
        assert!(
            updated
                .lane_events
                .iter()
                .any(|e| matches!(e, LaneEvent::PaneGone { .. }))
        );
    }

    #[test]
    fn round_status_includes_last_event_ts() {
        let tmp = TempDir::new().unwrap();
        let session_root = tmp.path().to_path_buf();
        std::fs::create_dir_all(session_root.join("round-01")).unwrap();
        std::fs::create_dir_all(session_root.join("agents")).unwrap();

        let mut manifest = AgentManifest::new(
            SessionId::new(),
            "scribe".into(),
            "scribe".into(),
            AgentKind::Meeting,
            None,
        );
        let known_ts = chrono::DateTime::parse_from_rfc3339("2026-05-12T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        manifest.lane_events.push(LaneEvent::ResponseFileWritten {
            ts: known_ts,
            path: PathBuf::from("/tmp/r.md"),
            bytes: 7,
        });
        let agent_id = manifest.agent_id;
        write_json(&manifest, &session_root).unwrap();

        let layout = RoundLayout::new(session_root, 1);
        let status = round_status(&layout, &[("scribe".into(), agent_id)]).unwrap();
        assert_eq!(status.roles[0].last_event_ts, Some(known_ts));
    }

    #[test]
    fn record_sentinel_classifies_tool_blocked_as_failure() {
        let tmp = TempDir::new().unwrap();
        let session_root = tmp.path().to_path_buf();
        std::fs::create_dir_all(session_root.join("agents")).unwrap();

        let manifest = AgentManifest::new(
            SessionId::new(),
            "backend".into(),
            "backend".into(),
            AgentKind::Meeting,
            None,
        );
        write_json(&manifest, &session_root).unwrap();

        let sentinel = Sentinel::new(
            manifest.session_id,
            manifest.agent_id,
            SentinelKind::ToolBlocked,
            Some("Bash blocked".into()),
            None,
        );
        let updated = record_sentinel(&session_root, &sentinel).unwrap();
        assert!(matches!(updated.status, RawStatus::Failed));
        assert!(
            updated
                .error
                .as_deref()
                .unwrap_or("")
                .contains("tool blocked")
        );
    }
}
