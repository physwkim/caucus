use super::*;
use crate::mcp::control_server::{ControlJob, ControlServer};
use crate::mcp::protocol::{ControlRequest, ControlResponse};
use crate::mcp::{McpToolSurface, PanelSummary};
use crate::session::id::PanelId;

/// Most control jobs one [`Multiplexer::drain_control`] call executes. A
/// burst larger than this is answered over several loop turns, so input,
/// the PTY pump and the frame keep running while it drains.
pub(super) const CONTROL_DRAIN_LIMIT: usize = 64;

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
            ControlRequest::QueueTasks {
                panel,
                tasks,
                read_mode,
                fallback_secs,
                selection_hints,
            } => self.queue_tasks(panel, tasks, read_mode, fallback_secs, selection_hints),
            ControlRequest::CollectWorkers => self.collect_workers(),
            ControlRequest::StopWatching { panel } => self.stop_watching(panel),
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
            // This path cannot hold a reply; `drain_control` parks a wait.
            ControlRequest::Wait {
                panel,
                timeout_secs,
            } => self.wait_now(panel, timeout_secs),
            ControlRequest::SubscribeEvents => {
                // Publish first: the snapshot must be the rows as of its
                // seq, and they go stale while no one is subscribed.
                self.refresh_event_feed();
                let (seq, panels) = self.events.snapshot();
                ControlResponse::Subscribed { seq, panels }
            }
        }
    }

    /// Publish what changed in the `list_panels` rows since the last call to
    /// `subscribe_events` clients. Called once per event-loop tick, after
    /// every step that can change a row. With no subscriber connected it
    /// returns at once: the rows are not even read.
    pub fn publish_panel_events(&mut self) {
        if self.events.has_subscribers() {
            self.refresh_event_feed();
        }
    }

    fn refresh_event_feed(&mut self) {
        let rows = self.list_panels();
        self.events.publish(rows);
    }

    /// Execute up to [`CONTROL_DRAIN_LIMIT`] queued control jobs from
    /// `server` in arrival order and answer each through its oneshot reply;
    /// the rest wait for the next loop turn. Called once per event-loop tick
    /// — the single point at which main worker MCP tool calls touch live
    /// panels, on the same thread that pumps PTYs (Invariant I-5).
    ///
    /// Most requests are answered immediately via
    /// [`Multiplexer::execute_control`]. Two are deferred.
    /// `spawn_role(worktree=true)`: its `git worktree add` is slow enough to
    /// freeze the single-threaded loop, so it is deferred off-thread
    /// (`Multiplexer::begin_spawn_role_worktree`) and its reply is sent later
    /// from `Multiplexer::poll_pending_spawns`. `wait` on a busy panel is
    /// parked (`Multiplexer::begin_wait`) and answered from
    /// `Multiplexer::poll_waits`. `register_round` is
    /// non-blocking in a different way: it acks now and the round is delivered
    /// later by the caucus→main push in [`Multiplexer::poll_worker_watches`].
    pub fn drain_control(&mut self, server: &mut ControlServer) {
        for _ in 0..CONTROL_DRAIN_LIMIT {
            let Ok(ControlJob { request, reply }) = server.jobs().try_recv() else {
                break;
            };
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
                ControlRequest::Wait {
                    panel,
                    timeout_secs,
                } => self.begin_wait(panel, timeout_secs, reply),
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
    pub(crate) fn panel_summaries(&mut self, panels: &[PanelId]) -> Vec<PanelSummary> {
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

    /// A burst over [`CONTROL_DRAIN_LIMIT`] is answered in loop-turn-sized
    /// batches: each drain answers at most the limit and leaves the rest
    /// queued for the next turn.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_control_answers_at_most_the_limit_per_turn() {
        use crate::mcp::control_client::call;
        let tmp = TempDir::new().unwrap();
        let session = Session::new("test", tmp.path().to_path_buf());
        let config = Config::load(tmp.path()).unwrap();
        let (mut mux, _signal, mut control) =
            Multiplexer::new(session, config, area(), 'a', LaunchMode::Fresh).unwrap();
        let sock = control.sock_path().to_path_buf();
        let count = CONTROL_DRAIN_LIMIT * 2 + 1;
        let clients: Vec<_> = (0..count)
            .map(|_| {
                let sock = sock.clone();
                tokio::spawn(async move {
                    call(&sock, "t", "list_panels", &serde_json::Value::Null).await
                })
            })
            .collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while control.jobs().len() < count {
            assert!(std::time::Instant::now() < deadline, "burst never queued");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }

        let mut left = count;
        while left > 0 {
            mux.drain_control(&mut control);
            left -= CONTROL_DRAIN_LIMIT.min(left);
            assert_eq!(control.jobs().len(), left);
        }
        for client in clients {
            assert!(matches!(
                client.await.unwrap().unwrap(),
                Ok(ControlResponse::Panels { .. })
            ));
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

    /// `subscribe_events` answers with the current rows at the feed's seq;
    /// each tick then publishes one event per changed row and nothing for an
    /// unchanged one. With no subscriber the tick publishes nothing.
    #[tokio::test]
    async fn subscribe_then_ticks_publish_row_changes() {
        use crate::mcp::events::PanelEvent;
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let panel = crate::bench_support::cat_panel("worker");
        let id = panel.id;
        mux.panels.push(panel);
        mux.rebuild_layout_tree();

        mux.publish_panel_events();
        assert_eq!(mux.events.snapshot().0, 0, "no subscriber, no publish");

        let mut rx = mux.events.subscriber().subscribe();
        let ControlResponse::Subscribed { seq, panels } =
            mux.execute_control(ControlRequest::SubscribeEvents)
        else {
            panic!("expected Subscribed");
        };
        assert_eq!(seq, 1);
        assert_eq!(panels, mux.list_panels());
        assert_eq!(rx.try_recv().unwrap().seq(), 1);

        mux.publish_panel_events();
        assert!(rx.try_recv().is_err(), "an unchanged row publishes nothing");

        mux.panels[0].state = PanelState::Working;
        mux.publish_panel_events();
        match rx.try_recv().unwrap() {
            PanelEvent::PanelUpdated { seq: 2, panel } => assert_eq!(panel.state, "working"),
            other => panic!("expected the working row at seq 2, got {other:?}"),
        }

        mux.kill_panel(id).unwrap();
        mux.publish_panel_events();
        assert_eq!(
            rx.try_recv().unwrap(),
            PanelEvent::PanelRemoved {
                seq: 3,
                panel_id: id
            }
        );
        mux.shutdown();
    }
}
