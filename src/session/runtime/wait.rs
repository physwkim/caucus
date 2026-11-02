//! The `wait` tool: a control reply held until one panel stops working.
//!
//! The main worker's turn stays open for the whole wait, so the reply cannot
//! be answered on the tick it is drained: [`Multiplexer::drain_control`] hands
//! it to [`Multiplexer::begin_wait`], and [`Multiplexer::poll_waits`] answers
//! it on the first tick the panel is no longer busy or the deadline passes.

use super::mcp::ReportedState;
use super::*;
use crate::mcp::McpError;
use crate::mcp::protocol::{ControlResponse, DEFAULT_WAIT_SECS, MAX_WAIT_SECS};
use std::time::Duration;
use tokio::sync::oneshot;

/// A `wait` whose panel was still busy when it was drained.
pub(crate) struct PendingWait {
    panel: PanelId,
    deadline: Instant,
    /// The control-socket oneshot the MCP call is blocked on.
    reply: oneshot::Sender<ControlResponse>,
}

impl Multiplexer {
    /// Answer a `wait` now if its panel is not busy, or park it for
    /// [`Multiplexer::poll_waits`].
    pub(crate) fn begin_wait(
        &mut self,
        panel: PanelId,
        timeout_secs: Option<u64>,
        reply: oneshot::Sender<ControlResponse>,
    ) {
        let deadline = match wait_deadline(timeout_secs, Instant::now()) {
            Ok(deadline) => deadline,
            Err(response) => {
                let _ = reply.send(response);
                return;
            }
        };
        match self.wait_check(panel) {
            WaitCheck::Done(response) => {
                let _ = reply.send(response);
            }
            WaitCheck::Busy(_) => self.pending_waits.push(PendingWait {
                panel,
                deadline,
                reply,
            }),
        }
    }

    /// A `wait` answered without parking, for callers that cannot hold a
    /// reply ([`Multiplexer::execute_control`]): a busy panel times out at
    /// once.
    pub(crate) fn wait_now(
        &mut self,
        panel: PanelId,
        timeout_secs: Option<u64>,
    ) -> ControlResponse {
        if let Err(response) = wait_deadline(timeout_secs, Instant::now()) {
            return response;
        }
        match self.wait_check(panel) {
            WaitCheck::Done(response) => response,
            WaitCheck::Busy(state) => wait_timed_out(panel, state),
        }
    }

    /// Answer every parked `wait` that is decided at this tick. Runs after
    /// [`Multiplexer::poll_worker_watches`], so a watched panel fed its next
    /// backlog task this tick is already `working` again and is not reported
    /// as finished between tasks.
    pub(crate) fn poll_waits(&mut self) {
        let now = Instant::now();
        let mut i = 0;
        while i < self.pending_waits.len() {
            let (panel, deadline) = (self.pending_waits[i].panel, self.pending_waits[i].deadline);
            let response = match self.wait_check(panel) {
                WaitCheck::Done(response) => response,
                WaitCheck::Busy(state) if now >= deadline => wait_timed_out(panel, state),
                WaitCheck::Busy(_) => {
                    i += 1;
                    continue;
                }
            };
            let _ = self.pending_waits.remove(i).reply.send(response);
        }
    }

    /// Tell every parked `wait` that caucus is shutting down.
    pub(crate) fn abort_pending_waits(&mut self) {
        for pending in self.pending_waits.drain(..) {
            let _ = pending.reply.send(ControlResponse::error(
                "caucus multiplexer is shutting down",
            ));
        }
    }

    /// The one rule for every `wait` path. The main panel is refused because
    /// the caller is the main worker, which stays `working` for as long as it
    /// waits. A panel that vanished (killed, or restarted under a new id) is
    /// an error.
    fn wait_check(&mut self, panel: PanelId) -> WaitCheck {
        if Some(panel) == self.main_panel_id {
            return WaitCheck::Done(ControlResponse::error(
                "wait cannot target the main panel: it is the caller",
            ));
        }
        match self.reported_state(panel) {
            None => WaitCheck::Done(ControlResponse::error(McpError::NoSuchPanel(panel))),
            Some(state) if state.is_busy() => WaitCheck::Busy(state),
            Some(_) => WaitCheck::Done(ControlResponse::Panels {
                panels: self.panel_summaries(&[panel]),
            }),
        }
    }
}

/// Whether a `wait` is answered now, or its panel is still busy in a state.
enum WaitCheck {
    Done(ControlResponse),
    Busy(ReportedState),
}

fn wait_timed_out(panel: PanelId, state: ReportedState) -> ControlResponse {
    ControlResponse::error(format!(
        "wait timed out: panel {panel} is still {}",
        state.as_str()
    ))
}

/// The deadline for a `wait` started at `now`, or the error for a timeout
/// over [`MAX_WAIT_SECS`].
fn wait_deadline(timeout_secs: Option<u64>, now: Instant) -> Result<Instant, ControlResponse> {
    let secs = timeout_secs.unwrap_or(DEFAULT_WAIT_SECS);
    if secs > MAX_WAIT_SECS {
        return Err(ControlResponse::error(format!(
            "timeout_secs {secs} exceeds the {MAX_WAIT_SECS} s maximum"
        )));
    }
    Ok(now + Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench_support::cat_panel;
    use crate::panel::lifecycle::PanelState;
    use crate::session::runtime::test_support::*;
    use tempfile::TempDir;
    use tokio::sync::oneshot::error::TryRecvError;

    fn push_panel(mux: &mut Multiplexer, state: PanelState) -> PanelId {
        let mut panel = cat_panel("worker");
        panel.state = state;
        let id = panel.id;
        mux.panels.push(panel);
        id
    }

    fn set_state(mux: &mut Multiplexer, id: PanelId, state: PanelState) {
        mux.panels.iter_mut().find(|p| p.id == id).unwrap().state = state;
    }

    fn summary_state(response: ControlResponse) -> String {
        match response {
            ControlResponse::Panels { panels } => {
                assert_eq!(panels.len(), 1, "{panels:?}");
                panels[0].state.clone()
            }
            other => panic!("expected Panels, got {other:?}"),
        }
    }

    fn error_message(response: ControlResponse) -> String {
        match response {
            ControlResponse::Error { message } => message,
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_idle_panel_is_answered_without_parking() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = push_panel(&mut mux, PanelState::Idle);
        let (tx, mut rx) = oneshot::channel();
        mux.begin_wait(id, None, tx);
        assert!(mux.pending_waits.is_empty());
        assert_eq!(summary_state(rx.try_recv().unwrap()), "idle");
        mux.shutdown();
    }

    #[tokio::test]
    async fn a_working_panel_is_answered_on_the_tick_it_goes_idle() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = push_panel(&mut mux, PanelState::Working);
        let (tx, mut rx) = oneshot::channel();
        mux.begin_wait(id, None, tx);
        mux.poll_waits();
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(mux.pending_waits.len(), 1);

        set_state(&mut mux, id, PanelState::Idle);
        mux.poll_waits();
        assert_eq!(summary_state(rx.try_recv().unwrap()), "idle");
        assert!(mux.pending_waits.is_empty());
        mux.shutdown();
    }

    /// `wait` and `list_panels` read one state: a working panel stopped on a
    /// selection menu reports `awaiting_selection`, which is not busy.
    #[tokio::test]
    async fn a_panel_stopped_on_a_menu_is_not_busy() {
        use crate::agent::manifest::AgentManifest;
        use crate::role::spec::AgentCli;
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = push_panel(&mut mux, PanelState::Working);
        let mf = AgentManifest::new(
            mux.session.id,
            id,
            "worker",
            "worker-1",
            AgentCli::Claude,
            None,
        );
        mux.manifests.insert(id, mf);
        let generation = mux.panels[0].grid().generation();
        mux.screen_scan_cache.insert(
            id,
            (
                generation,
                rounds::ScreenScan {
                    blocked: Some(rounds::BlockedPrompt::Selection(menu_of(
                        "Proceed?",
                        ["Yes", "No"],
                        0,
                    ))),
                    activity: crate::term::ScreenActivity::Unknown,
                },
            ),
        );
        assert_eq!(summary_state(mux.wait_now(id, None)), "awaiting_selection");
        mux.shutdown();
    }

    /// The tick order is the guarantee: a watched panel fed its next backlog
    /// task on the tick it went idle is `working` again when `poll_waits`
    /// runs, so the wait is answered only once the queue is drained.
    #[tokio::test]
    async fn a_backlog_feed_keeps_the_wait_parked() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_panel(&mut mux, PanelState::Working);
        mux.main_panel_id = Some(main);
        let worker = push_panel(&mut mux, PanelState::Working);
        mux.queue_tasks(worker, vec!["next".to_string()], None, Some(3600), None);
        let (tx, mut rx) = oneshot::channel();
        mux.begin_wait(worker, None, tx);

        set_state(&mut mux, worker, PanelState::Idle);
        mux.poll_worker_watches();
        mux.poll_waits();
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        set_state(&mut mux, worker, PanelState::Idle);
        mux.poll_worker_watches();
        mux.poll_waits();
        assert_eq!(summary_state(rx.try_recv().unwrap()), "idle");
        mux.shutdown();
    }

    #[tokio::test]
    async fn a_wait_past_its_deadline_is_an_error_naming_the_state() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = push_panel(&mut mux, PanelState::Working);
        let (tx, mut rx) = oneshot::channel();
        mux.begin_wait(id, Some(0), tx);
        assert_eq!(
            mux.pending_waits.len(),
            1,
            "a zero timeout still parks once"
        );
        mux.poll_waits();
        let message = error_message(rx.try_recv().unwrap());
        assert!(
            message.contains("timed out") && message.contains("working"),
            "{message}"
        );
        assert!(mux.pending_waits.is_empty());
        mux.shutdown();
    }

    #[tokio::test]
    async fn a_panel_killed_mid_wait_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = push_panel(&mut mux, PanelState::Working);
        let (tx, mut rx) = oneshot::channel();
        mux.begin_wait(id, None, tx);
        mux.kill_panel(id).unwrap();
        mux.poll_waits();
        let message = error_message(rx.try_recv().unwrap());
        assert!(message.contains(&id.to_string()), "{message}");
        mux.shutdown();
    }

    #[tokio::test]
    async fn the_main_panel_and_an_oversized_timeout_are_refused() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let main = push_panel(&mut mux, PanelState::Working);
        mux.main_panel_id = Some(main);
        let worker = push_panel(&mut mux, PanelState::Working);

        let message = error_message(mux.wait_now(main, None));
        assert!(message.contains("main panel"), "{message}");
        let message = error_message(mux.wait_now(worker, Some(MAX_WAIT_SECS + 1)));
        assert!(message.contains("maximum"), "{message}");

        let (tx, mut rx) = oneshot::channel();
        mux.begin_wait(worker, Some(MAX_WAIT_SECS + 1), tx);
        assert!(mux.pending_waits.is_empty());
        assert!(matches!(rx.try_recv(), Ok(ControlResponse::Error { .. })));
        mux.shutdown();
    }

    /// Over the control socket: `drain_control` parks a `wait` on a busy
    /// panel instead of answering it, and `poll_waits` answers the held
    /// connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_socket_wait_is_held_until_the_panel_settles() {
        use crate::mcp::control_client::roundtrip;
        use crate::mcp::protocol::ControlRequest;
        let tmp = TempDir::new().unwrap();
        let session = Session::new("test", tmp.path().to_path_buf());
        let config = Config::load(tmp.path()).unwrap();
        let (mut mux, _signal, mut control) =
            Multiplexer::new(session, config, area(), 'a', LaunchMode::Fresh).unwrap();
        let id = push_panel(&mut mux, PanelState::Working);
        let sock = control.sock_path().to_path_buf();
        let request = ControlRequest::Wait {
            panel: id,
            timeout_secs: None,
        };
        let client = tokio::spawn(async move { roundtrip(&sock, &request).await });
        let deadline = Instant::now() + Duration::from_secs(30);
        while control.jobs().is_empty() {
            assert!(Instant::now() < deadline, "wait never queued");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        mux.drain_control(&mut control);
        assert_eq!(mux.pending_waits.len(), 1);
        assert!(!client.is_finished());

        set_state(&mut mux, id, PanelState::Idle);
        mux.poll_waits();
        assert_eq!(summary_state(client.await.unwrap().unwrap()), "idle");
        mux.shutdown();
    }

    #[tokio::test]
    async fn shutdown_answers_parked_waits() {
        let tmp = TempDir::new().unwrap();
        let mut mux = mux(&tmp);
        let id = push_panel(&mut mux, PanelState::Working);
        let (tx, mut rx) = oneshot::channel();
        mux.begin_wait(id, None, tx);
        mux.shutdown();
        let message = error_message(rx.try_recv().unwrap());
        assert!(message.contains("shutting down"), "{message}");
    }
}
