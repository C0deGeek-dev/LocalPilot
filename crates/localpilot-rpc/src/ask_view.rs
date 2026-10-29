//! One connection's view of a session's permission asks.
//!
//! A served session's asks live in its [`AskRegistry`]; every attached
//! connection keeps an [`AskView`] over it. The view turns the registry's
//! raised/resolved broadcast into the records that connection should receive,
//! and remembers which asks it has shown, so an ask is never shown twice and a
//! resolved ask never stays shown.
//!
//! A connection that falls behind the broadcast has missed events it cannot get
//! back. The view then drops the stale backlog, rereads the registry, and tells
//! the connection the authoritative state: clients that declared
//! [`CAPABILITY_ACKS`](crate::CAPABILITY_ACKS) get the outcome of every shown
//! ask that ended (when the registry still remembers it) followed by
//! `asks_resync` listing exactly the asks still pending; other clients are shown
//! any pending ask they have not seen. No outcome is ever invented for an ask
//! the registry no longer remembers.

use std::collections::HashSet;

use tokio::sync::broadcast;

use crate::approver::{AskEvent, AskRegistry, PendingAsk};
use crate::protocol::ServerEvent;

/// One connection's view of a session's permission asks.
pub struct AskView {
    registry: AskRegistry,
    events: broadcast::Receiver<AskEvent>,
    shown: HashSet<String>,
    acks: bool,
}

impl AskView {
    /// Start viewing `registry`. Returns the view and the records that show the
    /// asks already outstanding. The view subscribes before reading them, so an
    /// ask raised or resolved in between is not lost.
    #[must_use]
    pub fn attach(registry: AskRegistry, acks: bool) -> (Self, Vec<ServerEvent>) {
        let events = registry.subscribe();
        let mut view = Self {
            registry,
            events,
            shown: HashSet::new(),
            acks,
        };
        let initial = view.show_outstanding();
        (view, initial)
    }

    /// The records produced by the session's next ask event, or `None` once the
    /// registry is gone (the session closed).
    pub async fn next(&mut self) -> Option<Vec<ServerEvent>> {
        match self.events.recv().await {
            Ok(AskEvent::Raised(ask)) => Some(if self.shown.insert(ask.ask_id.clone()) {
                vec![ask.wire_event()]
            } else {
                Vec::new()
            }),
            Ok(AskEvent::Resolved {
                ask_id,
                outcome,
                by,
            }) => {
                self.shown.remove(&ask_id);
                Some(if self.acks {
                    vec![ServerEvent::PermissionResolved {
                        ask_id,
                        outcome,
                        by,
                    }]
                } else {
                    Vec::new()
                })
            }
            Err(broadcast::error::RecvError::Lagged(_)) => Some(self.resync()),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }

    /// The ids this connection currently shows as pending.
    #[must_use]
    pub fn shown(&self) -> &HashSet<String> {
        &self.shown
    }

    /// Recover from a missed stretch of the broadcast.
    fn resync(&mut self) -> Vec<ServerEvent> {
        // Drop the backlog first: an old `Raised` must not resurrect an ask
        // whose `Resolved` was the event that was lost. Anything published
        // from here on arrives on the fresh receiver; the shown-set dedupes an
        // ask that is both in it and in the snapshot below.
        self.events = self.events.resubscribe();
        let outstanding = self.registry.outstanding_detail();
        let pending: HashSet<&str> = outstanding.iter().map(|ask| ask.ask_id.as_str()).collect();
        let gone: Vec<String> = self
            .shown
            .iter()
            .filter(|id| !pending.contains(id.as_str()))
            .cloned()
            .collect();
        let mut records = Vec::new();
        for ask_id in gone {
            self.shown.remove(&ask_id);
            if self.acks {
                if let Some((outcome, by)) = self.registry.outcome_of(&ask_id) {
                    records.push(ServerEvent::PermissionResolved {
                        ask_id,
                        outcome,
                        by,
                    });
                }
            }
        }
        if self.acks {
            self.shown = outstanding.iter().map(|ask| ask.ask_id.clone()).collect();
            records.push(ServerEvent::AsksResync {
                asks: outstanding.iter().map(PendingAsk::detail).collect(),
            });
        } else {
            for ask in &outstanding {
                if self.shown.insert(ask.ask_id.clone()) {
                    records.push(ask.wire_event());
                }
            }
        }
        records
    }

    fn show_outstanding(&mut self) -> Vec<ServerEvent> {
        self.registry
            .outstanding_detail()
            .into_iter()
            .filter(|ask| self.shown.insert(ask.ask_id.clone()))
            .map(|ask| ask.wire_event())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use tokio::sync::oneshot;

    use super::*;
    use crate::approver::{AskOutcome, Principal};

    fn raise(registry: &AskRegistry, id: &str) -> oneshot::Receiver<AskOutcome> {
        let (sender, receiver) = oneshot::channel();
        registry.register(
            PendingAsk {
                ask_id: id.to_string(),
                tool: "run_shell".to_string(),
                detail: "cargo test".to_string(),
                risk: "run a command".to_string(),
                expires_at_ms: 1,
            },
            Instant::now() + Duration::from_secs(300),
            sender,
        );
        receiver
    }

    async fn drain(view: &mut AskView) -> Vec<ServerEvent> {
        let mut out = Vec::new();
        while let Ok(Some(records)) =
            tokio::time::timeout(Duration::from_millis(20), view.next()).await
        {
            out.extend(records);
        }
        out
    }

    #[tokio::test]
    async fn a_lagged_acks_view_resyncs_to_exactly_the_pending_set() {
        let registry = AskRegistry::with_event_capacity(2);
        let _a = raise(&registry, "a");
        let _b = raise(&registry, "b");
        let (mut view, initial) = AskView::attach(registry.clone(), true);
        assert_eq!(
            initial.len(),
            2,
            "both outstanding asks are shown on attach"
        );

        // Fall far behind: "a" is answered, "c" and "d" come and go, "e" stays.
        assert!(registry.reply("a", true, Principal::Local).is_ok());
        let _c = raise(&registry, "c");
        registry.end("c", AskOutcome::Cancelled);
        let _d = raise(&registry, "d");
        assert!(registry.reply("d", false, Principal::Local).is_ok());
        let _e = raise(&registry, "e");

        let records = drain(&mut view).await;
        // The answered ask the client was showing gets its real outcome.
        assert!(records.iter().any(|record| matches!(
            record,
            ServerEvent::PermissionResolved {
                ask_id,
                outcome: AskOutcome::Allowed,
                by: Some(Principal::Local),
            } if ask_id == "a"
        )));
        // The resync lists exactly the registry's pending asks.
        let resync = records
            .iter()
            .rev()
            .find_map(|record| match record {
                ServerEvent::AsksResync { asks } => Some(asks.clone()),
                _ => None,
            })
            .expect("an asks_resync record");
        let mut listed: Vec<String> = resync.into_iter().map(|ask| ask.ask_id).collect();
        listed.sort();
        assert_eq!(listed, ["b", "e"]);
        let mut shown: Vec<&String> = view.shown().iter().collect();
        shown.sort();
        assert_eq!(shown, ["b", "e"]);
        // Nothing stale from the dropped backlog resurfaces afterwards.
        assert!(drain(&mut view).await.is_empty());
    }

    #[tokio::test]
    async fn a_lagged_legacy_view_shows_only_new_pending_asks() {
        let registry = AskRegistry::with_event_capacity(1);
        let _a = raise(&registry, "a");
        let (mut view, initial) = AskView::attach(registry.clone(), false);
        assert_eq!(initial.len(), 1);

        assert!(registry.reply("a", true, Principal::Local).is_ok());
        let _b = raise(&registry, "b");
        let _c = raise(&registry, "c");

        let records = drain(&mut view).await;
        let shown_ids: Vec<&str> = records
            .iter()
            .filter_map(|record| match record {
                ServerEvent::PermissionAsk { ask_id, .. } => Some(ask_id.as_str()),
                _ => None,
            })
            .collect();
        assert!(shown_ids.contains(&"b") && shown_ids.contains(&"c"));
        assert!(
            !shown_ids.contains(&"a"),
            "a resolved ask is not shown again"
        );
        assert!(
            !records.iter().any(|record| matches!(
                record,
                ServerEvent::AsksResync { .. } | ServerEvent::PermissionResolved { .. }
            )),
            "a legacy client gets no new record types"
        );
        let mut shown: Vec<&String> = view.shown().iter().collect();
        shown.sort();
        assert_eq!(shown, ["b", "c"]);
    }

    #[tokio::test]
    async fn an_ask_raised_and_resolved_in_order_is_shown_then_cleared() {
        let registry = AskRegistry::default();
        let (mut view, initial) = AskView::attach(registry.clone(), true);
        assert!(initial.is_empty());
        let _a = raise(&registry, "a");
        assert!(registry.reply("a", false, Principal::Local).is_ok());
        let records = drain(&mut view).await;
        assert!(matches!(records[0], ServerEvent::PermissionAsk { .. }));
        assert!(matches!(
            records[1],
            ServerEvent::PermissionResolved {
                outcome: AskOutcome::Denied,
                ..
            }
        ));
        assert!(view.shown().is_empty());
    }
}
