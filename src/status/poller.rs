//! Low-rate poller that captures a pane and feeds its tail through
//! [`super::pane_hint::classify`]. Used by `caucus watch` when the
//! sentinel hook is not installed; emits a [`PaneScreenHint`] update via
//! a tokio channel whenever the classification *changes*.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::interval;
use tracing::warn;

use crate::agent::derive_state::PaneScreenHint;
use crate::tmux::TmuxService;

/// One classification update from the poller.
#[derive(Debug, Clone)]
pub struct HintUpdate {
    pub pane: String,
    pub previous: Option<PaneScreenHint>,
    pub current: Option<PaneScreenHint>,
}

/// Spawn a tokio task that polls `pane_id` every `tick`, classifies the
/// captured text, and pushes a [`HintUpdate`] when the answer changes.
/// Cancel by dropping the receiver.
pub fn spawn_poller(
    tmux: TmuxService,
    pane_id: String,
    tick: Duration,
    look_back_lines: usize,
) -> mpsc::UnboundedReceiver<HintUpdate> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut interval = interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last: Option<PaneScreenHint> = None;
        loop {
            interval.tick().await;
            let text = match tmux.capture_pane(&pane_id, None, None).await {
                Ok(t) => t,
                Err(err) => {
                    warn!(pane = %pane_id, %err, "capture_pane failed in poller");
                    // Pane likely gone — exit cleanly.
                    let _ = tx.send(HintUpdate {
                        pane: pane_id.clone(),
                        previous: last,
                        current: None,
                    });
                    break;
                }
            };
            let now = super::pane_hint::classify(&text, look_back_lines);
            if now != last {
                if tx
                    .send(HintUpdate {
                        pane: pane_id.clone(),
                        previous: last,
                        current: now,
                    })
                    .is_err()
                {
                    break;
                }
                last = now;
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::{TmuxConfig, TmuxService};

    /// End-to-end against a real detached tmux session. Verifies the poller
    /// fires `EscToInterruptVisible` after we paint a busy banner via
    /// send_shell, and then no further updates until the banner clears.
    #[tokio::test]
    #[ignore = "requires tmux on PATH"]
    async fn poller_fires_on_busy_banner() -> Result<(), Box<dyn std::error::Error>> {
        let svc = TmuxService::with_config(TmuxConfig::default());
        let session = format!("caucus-poller-{}", std::process::id());
        svc.new_session(&session).await?;
        // Wait for the shell to print its initial prompt; without this the
        // first `send_shell` may be queued before the shell is reading stdin.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let panes = svc.list_pane_ids_in_session(&session).await?;
        let pane_id = panes[0].clone();

        let mut rx = spawn_poller(svc.clone(), pane_id.clone(), Duration::from_millis(120), 30);

        // Paint a busy banner. Use a literal echo so we don't depend on
        // the shell's printf path; redirect to /dev/null on stderr keeps
        // the output clean.
        svc.send_shell(&pane_id, "echo '(esc to interrupt)'", true)
            .await?;
        let update = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await?
            .expect("poller should deliver");
        assert_eq!(update.current, Some(PaneScreenHint::EscToInterruptVisible));

        svc.kill_session(&session).await?;
        Ok(())
    }
}
