//! Permission asks over the wire.
//!
//! The decision logic stays in the permission engine; this approver only
//! transports the *ask*. A pending ask is surfaced to the client as an event
//! and answered by a `permission_reply` command. A non-responding client —
//! disconnected, or silent past the timeout — degrades exactly like
//! non-interactive mode: the ask is denied.
//!
//! Every way an ask can end — a reply, the timeout, the turn dropping the ask
//! — goes through one registry transition taken under the registry lock, so
//! exactly one of them wins. The approver's future only *reads* the winner; it
//! never decides the outcome itself. The registry also publishes each raised
//! and resolved ask on a broadcast, so a host serving several clients can show
//! every client the same asks and outcomes.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use localpilot_core::EventId;
use localpilot_sandbox::{Approver, PermissionRequest};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::protocol::{AskDetail, ServerEvent};

/// How long an unanswered ask waits before it is denied.
const ASK_TIMEOUT: Duration = Duration::from_secs(300);

/// How many finished asks the registry remembers, so a late reply can be told
/// "already resolved" rather than "unknown".
const RECENT_CAPACITY: usize = 256;

/// Capacity of the raised/resolved broadcast.
const ASK_EVENT_CAPACITY: usize = 256;

/// Who acted on a session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Principal {
    /// A client on this machine (the IPC transport, the terminal UI, MCP).
    Local,
    /// A paired device, by its registry id, reaching the session through the
    /// gateway.
    Device { id: String },
}

impl Principal {
    /// A short label for audit records and request streams: `local`, or
    /// `device:<id>`.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Local => "local".to_string(),
            Self::Device { id } => format!("device:{id}"),
        }
    }
}

/// How an ask ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskOutcome {
    Allowed,
    Denied,
    /// Nobody answered before the deadline.
    Expired,
    /// The turn that raised the ask ended (cancelled, or the session closed)
    /// before anyone answered.
    Cancelled,
}

impl AskOutcome {
    /// Whether the tool may run.
    #[must_use]
    pub fn allows(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// A permission ask in flight to the client.
#[derive(Debug, Clone)]
pub struct PendingAsk {
    pub ask_id: String,
    pub tool: String,
    pub detail: String,
    pub risk: String,
    /// When the ask expires, in milliseconds since the Unix epoch (for display).
    pub expires_at_ms: u64,
}

impl PendingAsk {
    /// The ask with everything a client needs to render it.
    #[must_use]
    pub fn detail(&self) -> AskDetail {
        AskDetail {
            ask_id: self.ask_id.clone(),
            tool: self.tool.clone(),
            detail: self.detail.clone(),
            risk: self.risk.clone(),
            expires_at_ms: self.expires_at_ms,
        }
    }

    /// The `permission_ask` event announcing this ask.
    #[must_use]
    pub fn wire_event(&self) -> ServerEvent {
        ServerEvent::PermissionAsk {
            ask_id: self.ask_id.clone(),
            tool: self.tool.clone(),
            detail: self.detail.clone(),
            risk: self.risk.clone(),
            expires_at_ms: self.expires_at_ms,
        }
    }
}

/// What happened to an ask, as published on [`AskRegistry::subscribe`].
#[derive(Debug, Clone)]
pub enum AskEvent {
    Raised(PendingAsk),
    Resolved {
        ask_id: String,
        outcome: AskOutcome,
        by: Option<Principal>,
    },
}

/// Why a reply did not decide an ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyRefusal {
    /// Another answer, the timeout, or a cancellation got there first.
    AlreadyResolved(AskOutcome),
    /// The reply arrived after the deadline; the ask is now expired.
    Expired,
    /// No such ask (never existed, or finished too long ago to remember).
    Unknown,
}

struct Pending {
    ask: PendingAsk,
    deadline: Instant,
    sender: oneshot::Sender<AskOutcome>,
}

#[derive(Default)]
struct RegistryState {
    pending: HashMap<String, Pending>,
    recent: VecDeque<(String, AskOutcome, Option<Principal>)>,
}

impl RegistryState {
    /// The one terminal transition. Removes the ask, remembers its outcome,
    /// hands the outcome to the waiting approver and returns it; `None` when
    /// the ask is not pending (already terminal or unknown).
    fn finish(
        &mut self,
        ask_id: &str,
        outcome: AskOutcome,
        by: Option<Principal>,
    ) -> Option<AskOutcome> {
        let pending = self.pending.remove(ask_id)?;
        if self.recent.len() == RECENT_CAPACITY {
            self.recent.pop_front();
        }
        self.recent.push_back((ask_id.to_string(), outcome, by));
        // A dropped receiver means the approver is gone; the outcome is still
        // the recorded one.
        let _ = pending.sender.send(outcome);
        Some(outcome)
    }

    fn recent_outcome(&self, ask_id: &str) -> Option<(AskOutcome, Option<Principal>)> {
        self.recent
            .iter()
            .rev()
            .find(|(id, _, _)| id == ask_id)
            .map(|(_, outcome, by)| (*outcome, by.clone()))
    }
}

/// The serve loop's handle to resolve (or enumerate) outstanding asks.
#[derive(Clone)]
pub struct AskRegistry {
    state: Arc<Mutex<RegistryState>>,
    events: broadcast::Sender<AskEvent>,
}

impl std::fmt::Debug for AskRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskRegistry").finish_non_exhaustive()
    }
}

impl Default for AskRegistry {
    fn default() -> Self {
        Self::with_event_capacity(ASK_EVENT_CAPACITY)
    }
}

impl AskRegistry {
    pub(crate) fn with_event_capacity(capacity: usize) -> Self {
        let (events, _) = broadcast::channel(capacity);
        Self {
            state: Arc::default(),
            events,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Answer an outstanding ask as the local machine. Returns false when the
    /// reply did not decide it (already resolved, expired, or unknown).
    pub fn resolve(&self, ask_id: &str, allow: bool) -> bool {
        self.reply(ask_id, allow, Principal::Local).is_ok()
    }

    /// Answer an outstanding ask on behalf of `by`. Only the transition that
    /// wins returns `Ok`; a reply at or after the deadline expires the ask.
    pub fn reply(
        &self,
        ask_id: &str,
        allow: bool,
        by: Principal,
    ) -> Result<AskOutcome, ReplyRefusal> {
        let mut state = self.lock();
        let Some(deadline) = state.pending.get(ask_id).map(|pending| pending.deadline) else {
            return Err(state
                .recent_outcome(ask_id)
                .map_or(ReplyRefusal::Unknown, |(outcome, _)| {
                    ReplyRefusal::AlreadyResolved(outcome)
                }));
        };
        if Instant::now() >= deadline {
            if state.finish(ask_id, AskOutcome::Expired, None).is_some() {
                self.publish_resolved(ask_id, AskOutcome::Expired, None);
            }
            return Err(ReplyRefusal::Expired);
        }
        let outcome = if allow {
            AskOutcome::Allowed
        } else {
            AskOutcome::Denied
        };
        match state.finish(ask_id, outcome, Some(by.clone())) {
            Some(outcome) => {
                self.publish_resolved(ask_id, outcome, Some(by));
                Ok(outcome)
            }
            None => Err(ReplyRefusal::Unknown),
        }
    }

    /// End an ask without an answer (timeout or cancellation). A no-op when the
    /// ask already finished.
    pub(crate) fn end(&self, ask_id: &str, outcome: AskOutcome) {
        let mut state = self.lock();
        if state.finish(ask_id, outcome, None).is_some() {
            self.publish_resolved(ask_id, outcome, None);
        }
    }

    /// How an ask that is no longer pending ended, and who answered it, while
    /// the registry still remembers it.
    #[must_use]
    pub fn outcome_of(&self, ask_id: &str) -> Option<(AskOutcome, Option<Principal>)> {
        self.lock().recent_outcome(ask_id)
    }

    /// The ids of asks still awaiting an answer.
    #[must_use]
    pub fn outstanding(&self) -> Vec<String> {
        self.lock().pending.keys().cloned().collect()
    }

    /// Every ask still awaiting an answer, with its detail.
    #[must_use]
    pub fn outstanding_detail(&self) -> Vec<PendingAsk> {
        let mut asks: Vec<PendingAsk> = self
            .lock()
            .pending
            .values()
            .map(|pending| pending.ask.clone())
            .collect();
        asks.sort_by(|a, b| a.ask_id.cmp(&b.ask_id));
        asks
    }

    /// Raised and resolved asks from now on. Subscribe *before* reading
    /// [`outstanding_detail`](Self::outstanding_detail) so nothing falls in
    /// between.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<AskEvent> {
        self.events.subscribe()
    }

    pub(crate) fn register(
        &self,
        ask: PendingAsk,
        deadline: Instant,
        sender: oneshot::Sender<AskOutcome>,
    ) {
        let mut state = self.lock();
        let _ = self.events.send(AskEvent::Raised(ask.clone()));
        state.pending.insert(
            ask.ask_id.clone(),
            Pending {
                ask,
                deadline,
                sender,
            },
        );
    }

    fn publish_resolved(&self, ask_id: &str, outcome: AskOutcome, by: Option<Principal>) {
        let _ = self.events.send(AskEvent::Resolved {
            ask_id: ask_id.to_string(),
            outcome,
            by,
        });
    }
}

/// Ends the ask as cancelled if the approver's future is dropped before the
/// ask finished (the turn was cancelled or the session closed).
struct CancelOnDrop<'a> {
    registry: &'a AskRegistry,
    ask_id: &'a str,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        self.registry.end(self.ask_id, AskOutcome::Cancelled);
    }
}

/// An [`Approver`] that routes asks to the connected client.
pub struct RpcApprover {
    registry: AskRegistry,
    asks: mpsc::UnboundedSender<PendingAsk>,
}

impl RpcApprover {
    /// Build the approver plus the serve loop's halves: the stream of asks to
    /// surface and the registry that answers them.
    #[must_use]
    pub fn new() -> (Self, mpsc::UnboundedReceiver<PendingAsk>, AskRegistry) {
        let (asks, ask_rx) = mpsc::unbounded_channel();
        let registry = AskRegistry::default();
        (
            Self {
                registry: registry.clone(),
                asks,
            },
            ask_rx,
            registry,
        )
    }
}

fn unix_ms_after(timeout: Duration) -> u64 {
    SystemTime::now()
        .checked_add(timeout)
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

impl Approver for RpcApprover {
    fn approve<'a>(
        &'a self,
        request: &'a PermissionRequest,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let ask_id = format!("ask-{}", EventId::new());
            let ask = PendingAsk {
                ask_id: ask_id.clone(),
                tool: request.tool.clone(),
                detail: request.detail.clone(),
                risk: request.effect.risk_label().to_string(),
                expires_at_ms: unix_ms_after(ASK_TIMEOUT),
            };
            let (sender, mut receiver) = oneshot::channel();
            self.registry
                .register(ask.clone(), Instant::now() + ASK_TIMEOUT, sender);
            let guard = CancelOnDrop {
                registry: &self.registry,
                ask_id: &ask_id,
            };
            // A closed channel (serve loop gone) is a denial, never approval.
            if self.asks.send(ask).is_err() {
                self.registry.end(&ask_id, AskOutcome::Cancelled);
            }
            let outcome = match tokio::time::timeout(ASK_TIMEOUT, &mut receiver).await {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(_)) => AskOutcome::Cancelled,
                Err(_) => {
                    // The deadline passed: expire the ask unless a reply won the
                    // race, then read whichever transition won.
                    self.registry.end(&ask_id, AskOutcome::Expired);
                    receiver.await.unwrap_or(AskOutcome::Cancelled)
                }
            };
            drop(guard);
            outcome.allows()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(id: &str) -> PendingAsk {
        PendingAsk {
            ask_id: id.to_string(),
            tool: "shell".to_string(),
            detail: "cargo test".to_string(),
            risk: "high".to_string(),
            expires_at_ms: 0,
        }
    }

    fn registered(
        registry: &AskRegistry,
        id: &str,
        deadline: Instant,
    ) -> oneshot::Receiver<AskOutcome> {
        let (sender, receiver) = oneshot::channel();
        registry.register(ask(id), deadline, sender);
        receiver
    }

    #[test]
    fn first_reply_wins_and_later_replies_see_already_resolved() {
        let registry = AskRegistry::default();
        let mut events = registry.subscribe();
        let mut receiver = registered(&registry, "a", Instant::now() + ASK_TIMEOUT);

        assert_eq!(
            registry.reply("a", true, Principal::Local),
            Ok(AskOutcome::Allowed)
        );
        assert_eq!(
            registry.reply("a", false, Principal::Local),
            Err(ReplyRefusal::AlreadyResolved(AskOutcome::Allowed))
        );
        assert_eq!(receiver.try_recv(), Ok(AskOutcome::Allowed));
        assert!(matches!(events.try_recv(), Ok(AskEvent::Raised(_))));
        assert!(matches!(
            events.try_recv(),
            Ok(AskEvent::Resolved {
                outcome: AskOutcome::Allowed,
                by: Some(Principal::Local),
                ..
            })
        ));
        assert!(events.try_recv().is_err(), "exactly one resolution");
    }

    #[test]
    fn a_reply_at_the_deadline_expires_the_ask() {
        let registry = AskRegistry::default();
        let mut receiver = registered(&registry, "a", Instant::now());

        assert_eq!(
            registry.reply("a", true, Principal::Local),
            Err(ReplyRefusal::Expired)
        );
        assert_eq!(receiver.try_recv(), Ok(AskOutcome::Expired));
        assert_eq!(
            registry.reply("a", true, Principal::Local),
            Err(ReplyRefusal::AlreadyResolved(AskOutcome::Expired))
        );
    }

    #[test]
    fn cancellation_after_a_reply_changes_nothing() {
        let registry = AskRegistry::default();
        let mut events = registry.subscribe();
        let mut receiver = registered(&registry, "a", Instant::now() + ASK_TIMEOUT);

        assert!(registry.reply("a", false, Principal::Local).is_ok());
        registry.end("a", AskOutcome::Cancelled);
        assert_eq!(receiver.try_recv(), Ok(AskOutcome::Denied));
        let _raised = events.try_recv();
        assert!(matches!(
            events.try_recv(),
            Ok(AskEvent::Resolved {
                outcome: AskOutcome::Denied,
                ..
            })
        ));
        assert!(
            events.try_recv().is_err(),
            "cancel lost and published nothing"
        );
    }

    #[test]
    fn a_reply_after_cancellation_is_refused() {
        let registry = AskRegistry::default();
        let _receiver = registered(&registry, "a", Instant::now() + ASK_TIMEOUT);

        registry.end("a", AskOutcome::Cancelled);
        assert_eq!(
            registry.reply("a", true, Principal::Local),
            Err(ReplyRefusal::AlreadyResolved(AskOutcome::Cancelled))
        );
        assert!(registry.outstanding().is_empty());
    }

    #[test]
    fn unknown_asks_are_refused_and_the_bool_adapter_stays_false() {
        let registry = AskRegistry::default();
        assert_eq!(
            registry.reply("missing", true, Principal::Local),
            Err(ReplyRefusal::Unknown)
        );
        assert!(!registry.resolve("missing", true));
    }

    fn request() -> PermissionRequest {
        PermissionRequest {
            tool: "write_file".to_string(),
            effect: localpilot_sandbox::Effect::WritePath {
                inside_workspace: true,
                secret_like: false,
                overwrite: false,
            },
            interactivity: localpilot_sandbox::Interactivity::Interactive,
            trusted: true,
            detail: "notes.md".to_string(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_ask_expires_with_no_client_attached() {
        let (approver, _ask_rx, registry) = RpcApprover::new();
        let mut events = registry.subscribe();
        let request = request();
        let decision = approver.approve(&request);
        // Paused time auto-advances once the future is the only thing waiting.
        assert!(!decision.await, "an expired ask is a denial");
        assert!(matches!(events.try_recv(), Ok(AskEvent::Raised(_))));
        assert!(matches!(
            events.try_recv(),
            Ok(AskEvent::Resolved {
                outcome: AskOutcome::Expired,
                by: None,
                ..
            })
        ));
        assert!(registry.outstanding().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_ask_resolves_as_cancelled() {
        let (approver, _ask_rx, registry) = RpcApprover::new();
        let mut events = registry.subscribe();
        let request = request();
        {
            let decision = approver.approve(&request);
            // Poll once so the ask is raised, then drop it as a cancelled turn
            // would.
            assert!(tokio::time::timeout(Duration::from_millis(1), decision)
                .await
                .is_err());
        }
        assert!(matches!(events.try_recv(), Ok(AskEvent::Raised(_))));
        assert!(matches!(
            events.try_recv(),
            Ok(AskEvent::Resolved {
                outcome: AskOutcome::Cancelled,
                ..
            })
        ));
    }

    #[test]
    fn outstanding_detail_lists_pending_asks() {
        let registry = AskRegistry::default();
        let _a = registered(&registry, "a", Instant::now() + ASK_TIMEOUT);
        let _b = registered(&registry, "b", Instant::now() + ASK_TIMEOUT);
        let ids: Vec<String> = registry
            .outstanding_detail()
            .into_iter()
            .map(|ask| ask.ask_id)
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }
}
