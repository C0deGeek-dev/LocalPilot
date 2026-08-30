//! The real endpoint adapter: the convergence driver run over an adopted pair's
//! exact hosts and bound messaging.
//!
//! [`AdoptedPair`] implements [`PairEndpoints`] directly, so
//! [`PairDriver::run`](super::converge::PairDriver::run) drives two real
//! sessions with no protocol change from the deterministic fakes it runs over in
//! tests. There is deliberately no wrapper type and no reconstructed transport:
//! `drive` reaches a peer only through the [`SessionHost`](crate::host::SessionHost)
//! bound to it at adoption, and `notify` sends only through the
//! [`SessionPeers`](super::messaging::SessionPeers) view bound to the *sender*,
//! which cannot address the swarm as anyone else.
//!
//! Two correctness points the fakes cannot exercise but the real substrate must:
//!
//! - **The envelope is the turn's captured assistant message, not the event
//!   stream.** A turn emits text across several provider iterations around tool
//!   calls; concatenating the stream would fabricate an envelope. `drive_captured`
//!   reads the runtime's turn-scoped capture, so the envelope is exact and is
//!   absent — not stale — when a turn produces no assistant text.
//! - **Cancellation is propagated, not merely observed.** The driver's per-slot
//!   token is not the session's own; a cancel is carried to
//!   [`SessionHost::cancel`](crate::host::SessionHost::cancel), and a cancel that
//!   fires before the turn publishes its token drops the drive rather than letting
//!   an uncancelled turn run.

use std::future::Future;

use localpilot_core::SessionId;
use localpilot_harness::StopReason;
use localpilot_tools::{Audience, Delivery, PeerMessage, SwarmPeers};
use tokio_util::sync::CancellationToken;

use super::converge::{EndpointError, NotifyReply, PairEndpoints, TurnReply};
use super::spawn::AdoptedPair;
use crate::host::DriveCapture;

impl PairEndpoints for AdoptedPair {
    fn notify(
        &mut self,
        from: SessionId,
        to: SessionId,
        content: &str,
        cancel: &CancellationToken,
    ) -> impl Future<Output = Result<NotifyReply, EndpointError>> + Send {
        // Resolve the sender-bound messaging view up front and own only what the
        // delivery needs, so the future borrows nothing and stays `Send`.
        let peers = self.messaging(from);
        let message = PeerMessage {
            audience: Audience::One(to.to_string()),
            tldr: None,
            body: content.to_string(),
            delivery: Delivery::Notify,
        };
        let cancel = cancel.clone();
        async move {
            let peers = peers.ok_or_else(|| {
                EndpointError::PeerFailed(format!("no bound messaging for sender {from}"))
            })?;
            // Delivery does async registry work before its single injection. A
            // cancel that fires during it must stop it, not merely be checked
            // before it; the injection is the last step, so a dropped send leaves
            // no half-delivery.
            tokio::select! {
                biased;
                () = cancel.cancelled() => Ok(NotifyReply::Cancelled),
                sent = peers.send(&message) => match sent {
                    Ok(delivered) if delivered.reached == 0 => Err(EndpointError::PeerFailed(
                        format!("message to {to} reached nobody"),
                    )),
                    Ok(_) => Ok(NotifyReply::Delivered),
                    Err(reason) => Err(EndpointError::PeerFailed(reason)),
                },
            }
        }
    }

    fn drive(
        &mut self,
        peer: SessionId,
        prompt: &str,
        cancel: &CancellationToken,
    ) -> impl Future<Output = Result<TurnReply, EndpointError>> + Send {
        let host = self.host(peer);
        let prompt = prompt.to_string();
        let cancel = cancel.clone();
        async move {
            let host = host.ok_or_else(|| {
                EndpointError::PeerFailed(format!("no bound host for peer {peer}"))
            })?;
            // The driver is the sole scheduler: a peer already running a turn is
            // an invariant break, and driving it would let this endpoint cancel an
            // unrelated turn.
            if host.is_busy() {
                return Err(EndpointError::PeerFailed(format!(
                    "peer {peer} is already running a turn"
                )));
            }
            // A cancel that already fired stops the slot before a turn starts.
            if cancel.is_cancelled() {
                return Ok(TurnReply::Cancelled);
            }
            let drive = host.drive_captured(&prompt);
            tokio::pin!(drive);
            let mut signalled = false;
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled(), if !signalled => {
                        signalled = true;
                        // `cancel` returns false when no turn token is published:
                        // the turn has not started (or already finished) at this
                        // boundary. Rather than let an uncancelled turn run, drop
                        // the drive future and report the clean stop; otherwise the
                        // signal reached the running turn and we await its stop.
                        if !host.cancel() {
                            return Ok(TurnReply::Cancelled);
                        }
                    }
                    capture = &mut drive => return map_capture(capture, signalled, peer),
                }
            }
        }
    }
}

/// Map a captured turn to the driver's typed reply.
///
/// A cancel we signalled wins over the raw stop, so the driver's own post-op
/// precedence classifies it as an abort or a slot timeout. A `Done` with no
/// assistant text is an empty `Produced` — the driver's bounded repair answers a
/// malformed envelope, so the adapter never fabricates a transport failure for
/// one. A provider failure and a graceful quiesce are the only stops that are not
/// a reply: they are transport errors.
fn map_capture(
    capture: DriveCapture,
    signalled: bool,
    peer: SessionId,
) -> Result<TurnReply, EndpointError> {
    if signalled {
        return Ok(TurnReply::Cancelled);
    }
    Ok(match capture.reason {
        StopReason::Done => TurnReply::Produced {
            // The captured assistant text becomes the protocol envelope only
            // here, at the convergence boundary.
            envelope: capture.assistant_text.unwrap_or_default(),
            cost: capture.usage.total(),
        },
        StopReason::Cancelled => TurnReply::Cancelled,
        StopReason::TimedOut => TurnReply::TimedOut,
        StopReason::BudgetExceeded => TurnReply::BudgetExceeded,
        StopReason::NoProgress => TurnReply::NoProgress,
        StopReason::Degraded | StopReason::ProviderError => {
            return Err(EndpointError::ProviderError(format!(
                "peer {peer} provider stopped: {:?}",
                capture.reason
            )))
        }
        StopReason::Quiesced => {
            return Err(EndpointError::PeerFailed(format!(
                "peer {peer} quiesced before producing an envelope"
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_core::TokenUsage;

    /// A `Done` stop that carried no assistant text maps to an empty `Produced`,
    /// not a fabricated failure — the driver's bounded repair answers an empty or
    /// malformed envelope. A live turn cannot reach `Done` without text (an empty
    /// turn stops degraded), so this pins the defensive mapping directly.
    #[test]
    fn a_done_capture_without_text_maps_to_an_empty_envelope() {
        let usage = TokenUsage {
            input_tokens: 3,
            output_tokens: 4,
            ..TokenUsage::default()
        };
        let reply = map_capture(
            DriveCapture {
                reason: StopReason::Done,
                assistant_text: None,
                usage,
            },
            false,
            SessionId::new(),
        )
        .unwrap();
        assert_eq!(
            reply,
            TurnReply::Produced {
                envelope: String::new(),
                cost: usage.total(),
            }
        );
    }

    /// A cancel we signalled wins over whatever the turn's raw stop was, so the
    /// driver's own precedence — not the adapter — classifies it.
    #[test]
    fn a_signalled_capture_maps_to_cancelled_over_the_raw_stop() {
        let reply = map_capture(
            DriveCapture {
                reason: StopReason::Done,
                assistant_text: Some("a full answer".to_string()),
                usage: TokenUsage::default(),
            },
            true,
            SessionId::new(),
        )
        .unwrap();
        assert_eq!(reply, TurnReply::Cancelled);
    }
}
