//! Permission asks over the wire.
//!
//! The decision logic stays in the permission engine; this approver only
//! transports the *ask*. A pending ask is surfaced to the client as an event
//! and answered by a `permission_reply` command. A non-responding client —
//! disconnected, or silent past the timeout — degrades exactly like
//! non-interactive mode: the ask is denied.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use localpilot_core::EventId;
use localpilot_sandbox::{Approver, PermissionRequest};
use tokio::sync::{mpsc, oneshot};

/// How long an unanswered ask waits before it is denied.
const ASK_TIMEOUT: Duration = Duration::from_secs(300);

/// A permission ask in flight to the client.
#[derive(Debug, Clone)]
pub struct PendingAsk {
    pub ask_id: String,
    pub tool: String,
    pub detail: String,
    pub risk: String,
}

/// The serve loop's handle to resolve (or enumerate) outstanding asks.
#[derive(Debug, Clone, Default)]
pub struct AskRegistry {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
}

impl AskRegistry {
    /// Answer an outstanding ask. Returns false when the ask is unknown
    /// (already resolved, timed out, or never existed).
    pub fn resolve(&self, ask_id: &str, allow: bool) -> bool {
        let Some(sender) = self
            .pending
            .lock()
            .ok()
            .and_then(|mut map| map.remove(ask_id))
        else {
            return false;
        };
        sender.send(allow).is_ok()
    }

    /// The ids of asks still awaiting an answer.
    #[must_use]
    pub fn outstanding(&self) -> Vec<String> {
        self.pending
            .lock()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn register(&self, ask_id: String, sender: oneshot::Sender<bool>) {
        if let Ok(mut map) = self.pending.lock() {
            map.insert(ask_id, sender);
        }
    }

    fn forget(&self, ask_id: &str) {
        if let Ok(mut map) = self.pending.lock() {
            map.remove(ask_id);
        }
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

impl Approver for RpcApprover {
    fn approve<'a>(
        &'a self,
        request: &'a PermissionRequest,
    ) -> Pin<Box<dyn Future<Output = bool> + 'a>> {
        let ask_id = format!("ask-{}", EventId::new());
        let (sender, receiver) = oneshot::channel();
        self.registry.register(ask_id.clone(), sender);
        let sent = self.asks.send(PendingAsk {
            ask_id: ask_id.clone(),
            tool: request.tool.clone(),
            detail: request.detail.clone(),
            risk: request.effect.risk_label().to_string(),
        });
        Box::pin(async move {
            // A closed channel (serve loop gone) is a denial, never approval.
            if sent.is_err() {
                self.registry.forget(&ask_id);
                return false;
            }
            let decision = tokio::time::timeout(ASK_TIMEOUT, receiver)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(false);
            self.registry.forget(&ask_id);
            decision
        })
    }
}
