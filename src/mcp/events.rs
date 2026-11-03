//! Panel event feed — the `subscribe_events` stream on the control socket
//! (`docs/design.md` §10.3).
//!
//! The [`EventFeed`] lives on the multiplexer thread. Each loop turn with a
//! subscriber connected it is handed the current `list_panels` rows and
//! publishes the difference from the rows it saw last: a changed or new row
//! is a [`PanelEvent::PanelUpdated`] carrying the whole row, a vanished one a
//! [`PanelEvent::PanelRemoved`]. Events are latest-value, so a client that
//! applies them in `seq` order over its snapshot holds the current rows.
//!
//! Every event takes the next `seq`. The broadcast ring keeps the last
//! [`EVENT_RING`] events; a subscriber that falls further behind cannot be
//! caught up and is cut off with `events_lost`, after which it subscribes
//! again for a fresh snapshot.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use super::PanelSummary;
use crate::session::id::PanelId;

/// Events a subscriber may fall behind by before it is cut off.
pub const EVENT_RING: usize = 512;

/// One change to the `list_panels` rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PanelEvent {
    /// `panel`'s row is new or differs from its previous one.
    PanelUpdated { seq: u64, panel: Box<PanelSummary> },
    /// The panel is gone from the rows.
    PanelRemoved { seq: u64, panel_id: PanelId },
}

impl PanelEvent {
    /// This event's position in the feed.
    pub fn seq(&self) -> u64 {
        match self {
            Self::PanelUpdated { seq, .. } | Self::PanelRemoved { seq, .. } => *seq,
        }
    }
}

/// The multiplexer's side of the feed: the rows last published and the
/// `seq` of the last event.
pub struct EventFeed {
    tx: broadcast::Sender<PanelEvent>,
    seq: u64,
    rows: Vec<PanelSummary>,
}

/// The control server's side of the feed: it can only subscribe.
#[derive(Clone)]
pub struct EventSubscriber(broadcast::Sender<PanelEvent>);

impl EventSubscriber {
    /// A receiver of every event published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<PanelEvent> {
        self.0.subscribe()
    }
}

impl Default for EventFeed {
    fn default() -> Self {
        Self::new()
    }
}

impl EventFeed {
    /// An empty feed at `seq` 0.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(EVENT_RING);
        Self {
            tx,
            seq: 0,
            rows: Vec::new(),
        }
    }

    /// A handle the control server subscribes through.
    pub fn subscriber(&self) -> EventSubscriber {
        EventSubscriber(self.tx.clone())
    }

    /// Whether any subscription is open. With none, publishing can be
    /// skipped: the next subscription publishes before taking its snapshot.
    pub fn has_subscribers(&self) -> bool {
        self.tx.receiver_count() > 0
    }

    /// Publish the difference between `rows` and the rows last published,
    /// one event per changed, new or removed panel, and keep `rows`.
    pub fn publish(&mut self, rows: Vec<PanelSummary>) {
        let updated: Vec<PanelSummary> = rows
            .iter()
            .filter(|row| !self.rows.contains(row))
            .cloned()
            .collect();
        let removed: Vec<PanelId> = self
            .rows
            .iter()
            .filter(|old| !rows.iter().any(|row| row.panel_id == old.panel_id))
            .map(|old| old.panel_id)
            .collect();
        for panel in updated {
            self.seq += 1;
            self.send(PanelEvent::PanelUpdated {
                seq: self.seq,
                panel: Box::new(panel),
            });
        }
        for panel_id in removed {
            self.seq += 1;
            self.send(PanelEvent::PanelRemoved {
                seq: self.seq,
                panel_id,
            });
        }
        self.rows = rows;
    }

    /// The `seq` of the last event and the rows as of that event.
    pub fn snapshot(&self) -> (u64, Vec<PanelSummary>) {
        (self.seq, self.rows.clone())
    }

    fn send(&self, event: PanelEvent) {
        // No receiver is not an error: the event is still counted, so a
        // later subscriber's snapshot seq stays monotonic.
        let _ = self.tx.send(event);
    }
}

/// A minimal `worker` row for tests.
#[cfg(test)]
pub(crate) fn test_row(panel_id: PanelId, state: &str) -> PanelSummary {
    PanelSummary {
        panel_id,
        role: "worker".into(),
        state: state.into(),
        agent_cli: crate::role::spec::AgentCli::Claude,
        worktree_path: None,
        branch: None,
        model: None,
        transcript_path: None,
        last_note: None,
        last_notification: None,
    }
}

#[cfg(test)]
mod tests {
    use super::test_row as row;
    use super::*;
    use tokio::sync::broadcast::error::TryRecvError;

    /// A new row and a changed row are each one `panel_updated`, an
    /// unchanged row is nothing, a vanished row is one `panel_removed`; the
    /// seqs run on without a gap and the snapshot is the latest rows.
    #[test]
    fn publish_sends_one_event_per_changed_row() {
        let mut feed = EventFeed::new();
        let mut rx = feed.subscriber().subscribe();
        let (a, b) = (PanelId::new(), PanelId::new());

        feed.publish(vec![row(a, "idle"), row(b, "idle")]);
        feed.publish(vec![row(a, "idle"), row(b, "working")]);
        feed.publish(vec![row(b, "working")]);

        let events: Vec<PanelEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            events,
            vec![
                PanelEvent::PanelUpdated {
                    seq: 1,
                    panel: Box::new(row(a, "idle"))
                },
                PanelEvent::PanelUpdated {
                    seq: 2,
                    panel: Box::new(row(b, "idle"))
                },
                PanelEvent::PanelUpdated {
                    seq: 3,
                    panel: Box::new(row(b, "working"))
                },
                PanelEvent::PanelRemoved {
                    seq: 4,
                    panel_id: a
                },
            ]
        );
        assert_eq!(feed.snapshot(), (4, vec![row(b, "working")]));
    }

    /// A subscriber more than [`EVENT_RING`] events behind is told it lagged.
    #[test]
    fn a_subscriber_past_the_ring_lags() {
        let mut feed = EventFeed::new();
        let mut rx = feed.subscriber().subscribe();
        let a = PanelId::new();
        for i in 0..=EVENT_RING {
            feed.publish(vec![row(a, if i % 2 == 0 { "idle" } else { "working" })]);
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Lagged(1))));
    }

    #[test]
    fn has_subscribers_tracks_open_receivers() {
        let feed = EventFeed::new();
        assert!(!feed.has_subscribers());
        let rx = feed.subscriber().subscribe();
        assert!(feed.has_subscribers());
        drop(rx);
        assert!(!feed.has_subscribers());
    }

    #[test]
    fn event_wire_shape() {
        let a = PanelId::new();
        let value = serde_json::to_value(PanelEvent::PanelRemoved {
            seq: 7,
            panel_id: a,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"type": "panel_removed", "seq": 7, "panel_id": a.to_string()})
        );
        let updated = PanelEvent::PanelUpdated {
            seq: 8,
            panel: Box::new(row(a, "idle")),
        };
        let back: PanelEvent =
            serde_json::from_value(serde_json::to_value(&updated).unwrap()).unwrap();
        assert_eq!(back, updated);
    }
}
