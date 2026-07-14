use super::*;
use crate::mcp::control_server::{ControlJob, ControlServer};
use crate::mcp::protocol::{ControlRequest, ControlResponse};
use crate::mcp::{McpToolSurface, PanelSummary};
use crate::session::id::PanelId;

impl Multiplexer {
    /// Execute one queued [`ControlRequest`] against the live panels and
    /// produce its [`ControlResponse`] (`docs/design.md` §0 #4).
    ///
    /// Called by the event loop for every [`ControlJob`] drained from the
    /// control socket — see [`Multiplexer::drain_control`]. Each variant maps
    /// onto one [`McpToolSurface`] method; failures become
    /// [`ControlResponse::Error`] so the main worker sees the message in-band.
    pub fn execute_control(&mut self, request: ControlRequest) -> ControlResponse {
        match request {
            ControlRequest::SendKeys { panel, text, enter } => {
                match self.send_keys(panel, &text, enter) {
                    Ok(()) => ControlResponse::Ok,
                    Err(err) => ControlResponse::error(err),
                }
            }
            ControlRequest::SendKey { panel, key } => match self.send_key(panel, &key) {
                Ok(()) => ControlResponse::Ok,
                Err(err) => ControlResponse::error(err),
            },
            ControlRequest::Broadcast {
                panels,
                text,
                enter,
            } => self.broadcast(&panels, &text, enter),
            ControlRequest::CtrlC { panel } => match self.ctrl_c(panel) {
                Ok(()) => ControlResponse::Ok,
                Err(err) => ControlResponse::error(err),
            },
            ControlRequest::ReadPanel { panel, mode } => match self.read_panel(panel, mode) {
                Ok(text) => ControlResponse::Panel { text },
                Err(err) => ControlResponse::error(err),
            },
            ControlRequest::SpawnRole {
                role,
                worktree,
                model,
                agent_cli,
                prompt,
            } => match self.spawn_role(
                &role,
                worktree,
                model.as_deref(),
                agent_cli,
                prompt.as_deref(),
            ) {
                Ok(panel) => ControlResponse::Spawned { panel },
                Err(err) => ControlResponse::error(err),
            },
            ControlRequest::KillPanel { panel } => {
                // The trait method (McpError) — the inherent `kill_panel`
                // (anyhow) is shadowed, so call it through the trait.
                match McpToolSurface::kill_panel(self, panel) {
                    Ok(()) => ControlResponse::Ok,
                    Err(err) => ControlResponse::error(err),
                }
            }
            ControlRequest::RestartPanel { panel } => {
                // Trait method (McpError) — the inherent `restart_panel`
                // (anyhow) is shadowed, so call it through the trait.
                match McpToolSurface::restart_panel(self, panel) {
                    Ok(new_id) => ControlResponse::Spawned { panel: new_id },
                    Err(err) => ControlResponse::error(err),
                }
            }
            ControlRequest::ListPanels => ControlResponse::Panels {
                panels: self.list_panels(),
            },
            ControlRequest::RegisterRound {
                panels,
                read_mode,
                fallback_secs,
                backlog,
                selection_hints,
            } => self.register_round(panels, read_mode, fallback_secs, backlog, selection_hints),
            ControlRequest::RoundStatus { round } => self.round_status(round),
            ControlRequest::CancelRound { round } => self.cancel_round(round),
            ControlRequest::ReadMenu { panel } => match self.read_menu(panel) {
                Ok(text) => ControlResponse::Panel { text },
                Err(err) => ControlResponse::error(err),
            },
            ControlRequest::SelectOption { panel, index } => {
                match self.select_option(panel, index) {
                    Ok(()) => ControlResponse::Ok,
                    Err(err) => ControlResponse::error(err),
                }
            }
        }
    }

    /// Drain every queued control job from `server`, execute it, and answer
    /// each through its oneshot reply. Called once per event-loop tick — the
    /// single point at which main worker MCP tool calls touch live panels, on
    /// the same thread that pumps PTYs (Invariant I-5).
    ///
    /// Most requests are answered immediately via
    /// [`Multiplexer::execute_control`]. The one exception is
    /// `spawn_role(worktree=true)`: its `git worktree add` is slow enough to
    /// freeze the single-threaded loop, so it is deferred off-thread
    /// (`Multiplexer::begin_spawn_role_worktree`) and its reply is sent later
    /// from `Multiplexer::poll_pending_spawns`. `register_round` is
    /// non-blocking in a different way: it acks now and the round is delivered
    /// later by the caucus→main push in [`Multiplexer::poll_pending_rounds`].
    pub fn drain_control(&mut self, server: &mut ControlServer) {
        while let Ok(job) = server.jobs().try_recv() {
            let ControlJob { request, reply } = job;
            match request {
                // Defer the worktree create off the event loop; the reply is
                // moved into the pending entry and answered on completion.
                ControlRequest::SpawnRole {
                    role,
                    worktree: true,
                    model,
                    agent_cli,
                    prompt,
                } => {
                    self.begin_spawn_role_worktree(role, model, agent_cli, prompt, reply);
                }
                other => {
                    let response = self.execute_control(other);
                    // A dropped reply channel means the control-socket
                    // connection closed before we answered — nothing to do.
                    let _ = reply.send(response);
                }
            }
        }
    }

    /// Type the same `text` into every panel in `panels` — a round's fan-out
    /// (`docs/design.md` §4). Each panel is driven exactly as the MCP
    /// `send_keys` tool would drive it: the text is written, a `\r` appended
    /// when `enter`, and on `enter` [`Multiplexer::note_prompt_delivered`]
    /// opens a capture turn and flips the panel to `Working`.
    ///
    /// A panel id that does not exist (or whose write fails) is reported, not
    /// fatal — the remaining panels still receive the text. The reply is
    /// always [`ControlResponse::Panels`]: the post-broadcast [`PanelSummary`]
    /// of each targeted id that exists, the same shape `list_panels` /
    /// `register_round` return. A bad id is visible by its absence from that
    /// list, so the main worker can tell which panels a typo missed while the
    /// good ones still ran.
    fn broadcast(&mut self, panels: &[PanelId], text: &str, enter: bool) -> ControlResponse {
        for &panel in panels {
            // Per-panel failures (no such panel, write error) are non-fatal:
            // the other panels in the round still get the text.
            let _ = self.send_keys(panel, text, enter);
        }
        ControlResponse::Panels {
            panels: self.panel_summaries(panels),
        }
    }

    /// The [`PanelSummary`] for each id in `panels` that still exists, in the
    /// caller's order — missing ids are omitted (they were killed or the id
    /// was bad).
    pub(crate) fn panel_summaries(&self, panels: &[PanelId]) -> Vec<PanelSummary> {
        let all = self.list_panels();
        panels
            .iter()
            .filter_map(|id| all.iter().find(|s| s.panel_id == *id).cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::lifecycle::PanelState;
    use crate::session::runtime::test_support::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn control_request_for_unknown_panel_is_an_error() {
        use crate::mcp::protocol::{ControlRequest, ControlResponse};
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let ghost = PanelId::new();

        for req in [
            ControlRequest::CtrlC { panel: ghost },
            ControlRequest::SendKeys {
                panel: ghost,
                text: "hi".into(),
                enter: true,
            },
            ControlRequest::SendKey {
                panel: ghost,
                key: "esc".into(),
            },
            ControlRequest::ReadPanel {
                panel: ghost,
                mode: crate::mcp::ReadPanelMode::Screen,
            },
            ControlRequest::KillPanel { panel: ghost },
            ControlRequest::RestartPanel { panel: ghost },
        ] {
            let resp = mux.execute_control(req);
            assert!(
                matches!(resp, ControlResponse::Error { .. }),
                "expected an error response for an unknown panel"
            );
        }
    }

    #[tokio::test]
    async fn list_panels_control_request_is_empty_for_a_fresh_mux() {
        use crate::mcp::protocol::{ControlRequest, ControlResponse};
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        match mux.execute_control(ControlRequest::ListPanels) {
            ControlResponse::Panels { panels } => assert!(panels.is_empty()),
            other => panic!("expected Panels, got {other:?}"),
        }
    }

    /// `execute_control(Broadcast{..})` fans the same text into every panel:
    /// each real panel that exists is flipped to `Working` (with `enter`) and
    /// appears in the `Panels` reply; a non-existent id is non-fatal and is
    /// simply absent from the reply.
    ///
    /// Spawning a panel needs a real agent CLI; the test is skipped (not
    /// failed) when none is on PATH, matching `tests/mcp_integration.rs`.
    #[tokio::test]
    async fn broadcast_control_request_fans_text_into_every_panel() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);

        let Ok(a) = mux.spawn_panel("reviewer", None, None, None) else {
            eprintln!("skipping: no agent CLI on PATH");
            return;
        };
        let b = mux.spawn_panel("reviewer", None, None, None).unwrap();
        let ghost = PanelId::new();

        let resp = mux.execute_control(ControlRequest::Broadcast {
            // The ghost is interleaved between the two real ids — it must not
            // stop `b` from receiving the text.
            panels: vec![a, ghost, b],
            text: "the agenda".into(),
            enter: true,
        });

        match resp {
            ControlResponse::Panels { panels } => {
                // Only the two real panels come back; the ghost is omitted.
                assert_eq!(panels.len(), 2, "ghost id must be reported by absence");
                let ids: Vec<PanelId> = panels.iter().map(|s| s.panel_id).collect();
                assert!(ids.contains(&a) && ids.contains(&b));
                assert!(!ids.contains(&ghost));
            }
            other => panic!("expected Panels, got {other:?}"),
        }

        // `enter=true` opened a capture turn and flipped each real panel to
        // `Working`; the ghost did nothing.
        for id in [a, b] {
            assert_eq!(
                mux.panels().iter().find(|p| p.id == id).unwrap().state(),
                PanelState::Working,
            );
        }

        mux.shutdown();
    }
}
