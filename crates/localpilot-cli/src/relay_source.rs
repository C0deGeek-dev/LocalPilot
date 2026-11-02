//! The full-screen session's own feed, kept from the moment the session starts.
//!
//! A full-screen session can later be shared with paired devices through the
//! workspace server. For a device to see a turn that was already running, or to
//! answer an ask that was already open, the session's state has to be known
//! from its first event, not from the moment sharing begins. So every
//! full-screen session keeps, whether or not it is ever shared:
//!
//! - a runtime-event channel per turn, forwarded into the feed (nothing is
//!   kept once the turn's channel is gone),
//! - the session's one ask registry (the approver opens every ask there), and
//! - a local [`SessionFeed`] over both: the same numbering, projection and
//!   snapshots the server keeps for the sessions it hosts, bounded by record
//!   count and bytes.
//!
//! Nothing here is published anywhere until the session is shared.

// Used by the full-screen host (the `tui` feature) and the relay tests.
#![cfg_attr(not(any(test, feature = "tui")), allow(dead_code))]

use std::sync::{Arc, Mutex, PoisonError};

use localpilot_harness::RuntimeEvent;
use localpilot_rpc::AskRegistry;
use tokio::sync::broadcast;

use crate::server_feed::{bounded_records, Ingress, SessionFeed};

/// Runtime events buffered for a turn's own consumers.
const EVENT_CAPACITY: usize = 1024;

/// The most bytes the local feed keeps for replay.
pub(crate) const SOURCE_FEED_BYTES: usize = 8 * 1024 * 1024;

/// One full-screen session's event source, ask registry and local feed.
pub(crate) struct SessionSource {
    asks: AskRegistry,
    feed: Mutex<(Arc<SessionFeed>, Arc<Ingress>)>,
    /// The latest turn's forwarder: the next one waits for it, so a turn's
    /// records always reach the feed before the next turn's.
    forwarder: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The session the feed describes (`None` until first followed).
    session: Mutex<Option<localpilot_core::SessionId>>,
}

impl SessionSource {
    /// Start the source for a session whose approver opens its asks in `asks`.
    /// Must run inside a Tokio runtime (the feed's pump is a task).
    pub(crate) fn start(asks: AskRegistry) -> Self {
        let feed = Self::feed_over(&asks);
        Self {
            asks,
            feed: Mutex::new(feed),
            forwarder: Mutex::new(None),
            session: Mutex::new(None),
        }
    }

    /// Keep the feed describing `session`: when the runtime moved to another
    /// session (new, resumed, forked, incognito) since the last call, the old
    /// feed closes and a fresh one starts. Called before every turn and before
    /// sharing.
    pub(crate) fn follow(&self, session: localpilot_core::SessionId) {
        let mut current = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        match *current {
            Some(known) if known == session => {}
            Some(_) => {
                *current = Some(session);
                drop(current);
                self.restart("the session changed");
            }
            None => *current = Some(session),
        }
    }

    fn feed_over(asks: &AskRegistry) -> (Arc<SessionFeed>, Arc<Ingress>) {
        let ingress = Arc::new(Ingress::default());
        (
            SessionFeed::start_fed(ingress.clone(), Some(asks.clone()), SOURCE_FEED_BYTES),
            ingress,
        )
    }

    /// A fresh channel for one turn's runtime events: the turn sends on it and
    /// its own consumers subscribe to it; everything sent is also bounded and
    /// offered to the session's feed, after the previous turn's records. The
    /// channel (and what it buffered) goes away with the turn; the turn never
    /// waits for the feed.
    pub(crate) fn turn_events(&self) -> broadcast::Sender<RuntimeEvent> {
        let (events, mut rx) = broadcast::channel(EVENT_CAPACITY);
        let ingress = self
            .feed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .1
            .clone();
        let mut forwarder = self
            .forwarder
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let previous = forwarder.take();
        *forwarder = Some(tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            loop {
                match rx.recv().await {
                    Ok(event) => ingress.offer(bounded_records(event)),
                    Err(broadcast::error::RecvError::Lagged(_)) => ingress.lost(),
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }));
        events
    }

    /// The session's ask registry (read by the sharing commands; see
    /// `relay_client`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn asks(&self) -> &AskRegistry {
        &self.asks
    }

    /// The session's local feed.
    pub(crate) fn feed(&self) -> Arc<SessionFeed> {
        self.feed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0
            .clone()
    }

    /// Close the current feed and start a fresh one.
    fn restart(&self, reason: &str) {
        let next = Self::feed_over(&self.asks);
        let previous = std::mem::replace(
            &mut *self.feed.lock().unwrap_or_else(PoisonError::into_inner),
            next,
        );
        previous.0.close(reason);
    }
}

impl Drop for SessionSource {
    fn drop(&mut self) {
        self.feed().close("the session ended");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_harness::StopReason;
    use localpilot_rpc::{PendingAsk, ServerEvent, TurnState};

    fn history() -> (Vec<localpilot_rpc::HistoryMessage>, bool) {
        (Vec::new(), false)
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn a_turn_running_before_anyone_looks_is_in_the_snapshot_with_its_open_ask() {
        let asks = AskRegistry::default();
        let source = SessionSource::start(asks.clone());
        let events = source.turn_events();
        let _ = events.send(RuntimeEvent::TurnStarted);
        let _ = events.send(RuntimeEvent::Text("half an answer".to_string()));
        let open = asks.open_unbounded(PendingAsk {
            ask_id: "ask-1".to_string(),
            tool: "write_file".to_string(),
            detail: "a.txt".to_string(),
            risk: "write".to_string(),
            expires_at_ms: 0,
        });
        settle().await;

        let (snapshot, head) = source
            .feed()
            .snapshot(&history(), &|| crate::server_feed::Admission::default());
        assert!(head > 0);
        let ServerEvent::Snapshot {
            turn,
            turn_text,
            asks: pending,
            ..
        } = snapshot
        else {
            panic!("a snapshot");
        };
        assert_eq!(turn, TurnState::Running);
        assert_eq!(turn_text.as_deref(), Some("half an answer"));
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].ask_id, "ask-1");
        drop(open);
        let _ = events.send(RuntimeEvent::Stopped(StopReason::Done));
    }

    #[tokio::test]
    async fn a_session_change_closes_the_old_feed_and_starts_an_empty_one() {
        let source = SessionSource::start(AskRegistry::default());
        let _ = source.turn_events().send(RuntimeEvent::TurnStarted);
        settle().await;
        source.follow(localpilot_core::SessionId::new());
        let old = source.feed();
        source.follow(localpilot_core::SessionId::new());
        let (closed, _) = old.snapshot(&history(), &|| crate::server_feed::Admission::default());
        assert!(matches!(closed, ServerEvent::SessionClosed { .. }));
        let (fresh, head) = source
            .feed()
            .snapshot(&history(), &|| crate::server_feed::Admission::default());
        assert_eq!(head, 0);
        assert!(matches!(
            fresh,
            ServerEvent::Snapshot {
                turn: TurnState::Idle,
                ..
            }
        ));
    }

    fn ask(id: &str, detail: usize) -> PendingAsk {
        PendingAsk {
            ask_id: id.to_string(),
            tool: "run_shell".to_string(),
            detail: "d".repeat(detail),
            risk: "shell".to_string(),
            expires_at_ms: 0,
        }
    }

    #[tokio::test]
    async fn an_oversized_ask_is_not_kept_and_makes_the_session_unshareable_until_it_ends() {
        let asks = AskRegistry::default();
        let source = SessionSource::start(asks.clone());
        let big = crate::server_feed::BOUNDED_RECORD_MAX + 1;
        let open = asks.open_unbounded(ask("ask-big", big));
        settle().await;
        let feed = source.feed();
        assert!(!feed.shareable());
        let (snapshot, _) = feed.snapshot(&history(), &|| crate::server_feed::Admission::default());
        let ServerEvent::Snapshot { asks: kept, .. } = snapshot else {
            panic!("a snapshot")
        };
        assert!(kept.is_empty(), "the shared state does not hold it");
        // The local ask is untouched.
        assert_eq!(asks.outstanding_detail()[0].detail.len(), big);
        drop(open); // the ask ends (cancelled)
        settle().await;
        assert!(feed.shareable());
    }

    #[tokio::test]
    async fn an_oversized_plan_is_not_kept_and_unshareable_until_the_next_turn() {
        let source = SessionSource::start(AskRegistry::default());
        let events = source.turn_events();
        let _ = events.send(RuntimeEvent::TurnStarted);
        let step = localpilot_harness::PlanStep {
            title: "s".repeat(crate::server_feed::BOUNDED_RECORD_MAX + 1),
            status: "pending".to_string(),
        };
        let _ = events.send(RuntimeEvent::Plan(vec![step]));
        settle().await;
        let feed = source.feed();
        assert!(!feed.shareable());
        let (snapshot, _) = feed.snapshot(&history(), &|| crate::server_feed::Admission::default());
        assert!(matches!(snapshot, ServerEvent::Snapshot { plan: None, .. }));
        let _ = events.send(RuntimeEvent::TurnStarted);
        settle().await;
        assert!(feed.shareable());
    }

    #[tokio::test]
    async fn a_stalled_reader_holds_a_bounded_number_of_bounded_records() {
        let source = SessionSource::start(AskRegistry::default());
        let feed = source.feed();
        let mut live = feed.live();
        let events = source.turn_events();
        // Far more, and far larger, records than the reader may hold.
        for _ in 0..400 {
            let _ = events.send(RuntimeEvent::Text("z".repeat(64 * 1024)));
            tokio::task::yield_now().await;
        }
        settle().await;
        let mut lagged = false;
        loop {
            match live.try_recv() {
                Ok((_, event)) => {
                    let size = serde_json::to_vec(&event).unwrap().len();
                    assert!(size <= crate::server_feed::BOUNDED_RECORD_MAX, "{size}");
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => lagged = true,
                Err(_) => break,
            }
        }
        assert!(lagged, "the reader fell behind a bounded queue");
    }

    #[tokio::test]
    async fn an_oversized_ask_open_before_the_source_starts_is_not_kept() {
        let asks = AskRegistry::default();
        let _open =
            asks.open_unbounded(ask("ask-early", crate::server_feed::BOUNDED_RECORD_MAX + 1));
        let source = SessionSource::start(asks.clone());
        let feed = source.feed();
        assert!(!feed.shareable());
        let (pending, _) = feed.pending_asks();
        assert!(pending.is_empty());
        assert_eq!(asks.outstanding().len(), 1, "the local ask stands");
    }

    #[tokio::test]
    async fn a_turn_reaches_the_feed_wholly_before_the_next_one() {
        let source = SessionSource::start(AskRegistry::default());
        let first = source.turn_events();
        let _ = first.send(RuntimeEvent::TurnStarted);
        for _ in 0..100 {
            let _ = first.send(RuntimeEvent::Text("1".to_string()));
        }
        let _ = first.send(RuntimeEvent::Stopped(StopReason::Done));
        drop(first);
        let second = source.turn_events();
        let _ = second.send(RuntimeEvent::TurnStarted);
        let _ = second.send(RuntimeEvent::Text("second".to_string()));
        drop(second);
        settle().await;
        let (snapshot, _) = source
            .feed()
            .snapshot(&history(), &|| crate::server_feed::Admission::default());
        let ServerEvent::Snapshot {
            turn, turn_text, ..
        } = snapshot
        else {
            panic!("a snapshot")
        };
        // Had the first turn's end landed after the second turn's start, the
        // session would read idle with the first turn's text.
        assert_eq!(turn, TurnState::Running);
        assert_eq!(turn_text.as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn one_large_escape_heavy_text_keeps_the_whole_answer_for_a_mid_turn_share() {
        let source = SessionSource::start(AskRegistry::default());
        let events = source.turn_events();
        let big = "\u{1}\"\\é".repeat(8 * 1024); // escape-heavy, multi-byte
        let _ = events.send(RuntimeEvent::TurnStarted);
        let _ = events.send(RuntimeEvent::Text(big.clone()));
        let _ = events.send(RuntimeEvent::Text("end".to_string()));
        settle().await;
        let (snapshot, _) = source
            .feed()
            .snapshot(&history(), &|| crate::server_feed::Admission::default());
        let ServerEvent::Snapshot {
            turn_text,
            turn_text_truncated,
            ..
        } = snapshot
        else {
            panic!("a snapshot")
        };
        let expected = format!("{big}end");
        assert!(expected.len() <= 64 * 1024);
        assert_eq!(turn_text.as_deref(), Some(expected.as_str()));
        assert!(!turn_text_truncated);
    }
}
