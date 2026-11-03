//! Pairing a device from the full-screen terminal: one owned pairing attempt
//! at a time (see ADR-0255 for the server side).
//!
//! An attempt's identity is fixed before anything is sent: this terminal's
//! issuer stream, the next request number, a fresh receipt and attempt id. It
//! then takes two connections: a no-code `pair_hello` (the server instance),
//! and a fresh one whose first record is `pair_open`. Before any `pair_open`
//! byte could have been sent, cancelling simply ends the attempt with no code;
//! from then on every cancel, timeout or error is an uncertain creation,
//! settled with `pair_cancel_receipt` on the same identity, never a new one.
//!
//! The attempt's state is one coalesced value whose outcome, once known, is
//! never overwritten; the code (as a link and QR rows) waits in one slot until
//! the terminal takes it, and is wiped when the attempt's presentation goes.
//! The attempt's tasks end with it.

#![cfg_attr(not(feature = "tui"), allow(dead_code))]

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use localpilot_rpc::{
    ClientCommand, ClientRecord, JsonRecordReader, PairingOutcome, PairingState, Scope,
    ServerEvent, ServerRecord, WireSecret, RPC_PROTOCOL_VERSION,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::remote_share::{Dial, Link};

/// How long each step of a pairing may take: dialing, a reply, a write.
const STEP: Duration = Duration::from_secs(3);
/// How long the prerequisites may take: the gateway check's own bounds
/// (connect, then answer) plus a margin, so its own typed result arrives.
const PREREQUISITES: Duration = Duration::from_secs(6);
/// How long a cancel on the issuer connection may take to be answered.
const CANCEL_ANSWER: Duration = Duration::from_secs(2);

/// Text that must not be shown, logged or kept: wiped when dropped.
pub(crate) struct SecretText(String);

impl SecretText {
    pub(crate) fn new(text: String) -> Self {
        Self(text)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for SecretText {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.fill(0);
    }
}

impl Clone for SecretText {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl std::fmt::Debug for SecretText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretText(<redacted>)")
    }
}

/// What a device is granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Grant {
    pub(crate) name: String,
    pub(crate) scope: Scope,
    pub(crate) this_workspace_only: bool,
}

/// A live code, ready to show: the exact link and its QR rows (when one
/// could be drawn), and when it stops working.
pub(crate) struct Code {
    pub(crate) link: SecretText,
    pub(crate) qr: Result<Vec<SecretText>, String>,
    pub(crate) deadline: Instant,
}

impl std::fmt::Debug for Code {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Code(<redacted>)")
    }
}

/// Where an attempt stands. Once `Finished`, it never changes again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Phase {
    Checking,
    Opening,
    /// The code is live until `deadline`.
    Active {
        deadline: Instant,
    },
    /// Closing: cancelling or settling with the server.
    Closing,
    /// How it ended, as the server says (or, before any code could exist,
    /// `NoCode`).
    Finished(Ended),
    /// It could not be confirmed how it ended; the code may still work.
    Unconfirmed {
        restarted: bool,
    },
}

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ended {
    /// No code was ever created (stopped before one could be).
    NoCode(String),
    /// The server says so.
    Outcome(PairingOutcome),
    /// The server refused to open it.
    Refused(String),
}

/// The issuer stream of this terminal process and its request numbers.
pub(crate) struct Issuer {
    stream: String,
    next: AtomicU64,
}

impl Issuer {
    pub(crate) fn new() -> Self {
        Self {
            stream: crate::server_devices::random_hex(16)
                .unwrap_or_else(|_| localpilot_core::EventId::new().to_string()),
            next: AtomicU64::new(1),
        }
    }

    /// The next request number; `None` once exhausted (never wrapped).
    fn take(&self) -> Option<u64> {
        self.next
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_add(1))
            .ok()
    }
}

/// Checks that pairing can be useful before any code is made: remote access
/// on, a phone address, the gateway running. `Ok` is the address's origin and
/// the workspace id the link names.
pub(crate) type Prerequisites = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<(String, String), String>> + Send>>
        + Send
        + Sync,
>;

/// The real prerequisites for the workspace at `cwd`.
pub(crate) fn prerequisites_for(cwd: PathBuf) -> Prerequisites {
    Arc::new(move || {
        let cwd = cwd.clone();
        Box::pin(async move {
            let status = crate::remote_gateway::status().await;
            let settings = status
                .settings
                .map_err(|reason| format!("Remote access settings cannot be read: {reason}"))?;
            if !settings.enabled {
                return Err(
                    "Remote access is disabled. Set [remote] enabled = true in your user config."
                        .to_string(),
                );
            }
            let origin = match settings.advertised {
                Ok(Some(origin)) => origin,
                Ok(None) => {
                    return Err("No phone address is configured. Set [remote] advertised_url to your trusted HTTPS address.".to_string())
                }
                Err(reason) => return Err(format!("The phone address must be a valid HTTPS origin. {reason}")),
            };
            use crate::remote_status::GatewayHealth;
            match status.gateway {
                Some(GatewayHealth::Running) => {}
                Some(GatewayHealth::NotRunning) => {
                    return Err(
                        "The gateway is not running. Start it with localpilot remote gateway."
                            .to_string(),
                    )
                }
                Some(GatewayHealth::Unexpected) => {
                    return Err(format!(
                        "Another service is using the gateway port {}. Check [remote] port.",
                        settings.port
                    ))
                }
                Some(GatewayHealth::TimedOut) => {
                    return Err(
                        "The gateway did not answer in time; check it and try again.".to_string(),
                    )
                }
                Some(GatewayHealth::CheckFailed(reason)) => {
                    return Err(format!("The gateway could not be checked: {reason}"))
                }
                None => return Err("The gateway could not be checked.".to_string()),
            }
            let workspace = crate::server_index::discovery_id(&cwd).ok_or_else(|| {
                "This workspace's folder cannot be resolved, so it cannot be paired from."
                    .to_string()
            })?;
            Ok((origin, workspace))
        })
    })
}

/// One attempt's identity, fixed before anything is sent.
#[derive(Clone)]
struct Identity {
    stream: String,
    n: u64,
    attempt: String,
    receipt: SecretText,
}

/// What an attempt shares with its tasks. Locks are taken code slot first,
/// then phase, never the other way.
struct Shared {
    phase: Mutex<Phase>,
    instance: Mutex<Option<String>>,
    /// The code, until the terminal takes it, the presentation goes or the
    /// attempt ends.
    code: Mutex<Option<Code>>,
    /// Whether the code may still be handed to the terminal (changed only
    /// under the slot's lock).
    presented: std::sync::atomic::AtomicBool,
}

impl Shared {
    fn new() -> Self {
        Self {
            phase: Mutex::new(Phase::Checking),
            instance: Mutex::new(None),
            code: Mutex::new(None),
            presented: std::sync::atomic::AtomicBool::new(true),
        }
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Code>> {
        self.code.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn phase(&self) -> std::sync::MutexGuard<'_, Phase> {
        self.phase.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Move to `next`, unless the attempt already ended. Ending empties the
    /// slot in the same step, so an ended attempt never hands over a code.
    fn set(&self, next: Phase) {
        let ending = matches!(next, Phase::Finished(_));
        let mut slot = self.slot();
        let mut phase = self.phase();
        if matches!(*phase, Phase::Finished(_)) {
            return;
        }
        *phase = next;
        drop(phase);
        if ending {
            let code = slot.take();
            drop(slot);
            drop(code);
        }
    }

    /// Move from `from` to `next` only: progress never undoes a close.
    fn advance(&self, from: Phase, next: Phase) {
        let mut phase = self.phase();
        if *phase == from {
            *phase = next;
        }
    }

    /// Keep `code` for the terminal, unless the presentation is gone, the
    /// attempt ended or the code is already past its deadline. Returns
    /// whether it was kept (a refused code is wiped).
    fn offer(&self, code: Code) -> bool {
        let mut slot = self.slot();
        let phase = self.phase();
        let open = !matches!(*phase, Phase::Finished(_))
            && self.presented.load(Ordering::SeqCst)
            && code.deadline > Instant::now();
        drop(phase);
        if open {
            *slot = Some(code);
        }
        open
    }

    /// The code, once, if it may still be shown.
    fn take(&self) -> Option<Code> {
        let mut slot = self.slot();
        let code = slot.take()?;
        let phase = self.phase();
        let open = !matches!(*phase, Phase::Finished(_))
            && self.presented.load(Ordering::SeqCst)
            && code.deadline > Instant::now();
        drop(phase);
        drop(slot);
        open.then_some(code)
    }

    /// No presentation from now on: wipe what waits.
    fn retire(&self) {
        let mut slot = self.slot();
        self.presented.store(false, Ordering::SeqCst);
        let code = slot.take();
        drop(slot);
        drop(code);
    }

    fn instance(&self) -> Option<String> {
        self.instance
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// One pairing attempt. Dropping it stops its tasks.
pub(crate) struct Attempt {
    identity: Identity,
    grant: Grant,
    shared: Arc<Shared>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    settle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    dial: Dial,
}

impl Drop for Attempt {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(settle) = self
            .settle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            settle.abort();
        }
        self.retire();
    }
}

impl std::fmt::Debug for Attempt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Attempt")
            .field("attempt", &self.identity.attempt)
            .field("n", &self.identity.n)
            .finish_non_exhaustive()
    }
}

impl Attempt {
    /// Start an attempt: prerequisites, then the two legs. `None` when this
    /// terminal has no request number left.
    pub(crate) fn start(
        issuer: &Issuer,
        dial: Dial,
        prerequisites: Prerequisites,
        grant: Grant,
    ) -> Option<Self> {
        let n = issuer.take()?;
        let identity = Identity {
            stream: issuer.stream.clone(),
            n,
            attempt: crate::server_devices::random_hex(8).ok()?,
            receipt: SecretText::new(crate::server_devices::random_hex(32).ok()?),
        };
        let shared = Arc::new(Shared::new());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(
            identity.clone(),
            grant.clone(),
            dial.clone(),
            prerequisites,
            shared.clone(),
            cancel.clone(),
        ));
        Some(Self {
            identity,
            grant,
            shared,
            cancel,
            task,
            settle: Mutex::new(None),
            dial,
        })
    }

    pub(crate) fn phase(&self) -> Phase {
        self.shared.phase().clone()
    }

    /// The code, once, if it is ready and may still be shown (never after
    /// the attempt ended, the presentation went or the deadline passed).
    pub(crate) fn take_code(&self) -> Option<Code> {
        self.shared.take()
    }

    /// The presentation is gone: nothing may be shown from now on, and a code
    /// waiting (or arriving later) is wiped.
    pub(crate) fn retire(&self) {
        self.shared.retire();
    }

    /// Close: hide at once, then cancel (or settle) with the server. Returns
    /// at once; the outcome arrives as the phase.
    pub(crate) fn close(&self) {
        self.retire();
        if !matches!(self.phase(), Phase::Finished(_)) {
            self.shared.set(Phase::Closing);
        }
        self.cancel.cancel();
    }

    /// Settle with the server by receipt (Retry invalidation). Only while
    /// unconfirmed and no settle runs: at most one at a time, never beside
    /// the attempt's own. Returns whether it began.
    pub(crate) fn retry(&self) -> bool {
        self.settle_with(true)
    }

    /// Ask the server how the attempt ended (Check status), the same way.
    pub(crate) fn check(&self) -> bool {
        self.settle_with(false)
    }

    fn settle_with(&self, cancel: bool) -> bool {
        let mut settle = self.settle.lock().unwrap_or_else(PoisonError::into_inner);
        if settle.as_ref().is_some_and(|task| !task.is_finished()) {
            return false;
        }
        if !matches!(self.phase(), Phase::Unconfirmed { .. }) {
            return false;
        }
        let identity = self.identity.clone();
        let shared = self.shared.clone();
        let dial = self.dial.clone();
        *settle = Some(tokio::spawn(async move {
            settle_by_receipt(&identity, &dial, &shared, cancel).await;
        }));
        true
    }

    /// Whether a settle is running (Retry/Check disabled meanwhile).
    pub(crate) fn settling(&self) -> bool {
        self.settle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|task| !task.is_finished())
    }

    /// Whether a code could still be live (shown, or uncertain). A known
    /// server restart ended every code the earlier instance held.
    pub(crate) fn unresolved(&self) -> bool {
        !matches!(
            self.phase(),
            Phase::Finished(_) | Phase::Unconfirmed { restarted: true }
        )
    }

    pub(crate) fn grant(&self) -> &Grant {
        &self.grant
    }
}

/// Bytes wiped when dropped (a serialized record may carry a secret).
struct Wiped(Vec<u8>);

impl Drop for Wiped {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

pub(crate) async fn send_record<W: AsyncWrite + Unpin>(
    write: &mut W,
    command: ClientCommand,
) -> std::io::Result<()> {
    let record = ClientRecord {
        v: RPC_PROTOCOL_VERSION,
        id: None,
        request: None,
        command,
    };
    // Wiped however this ends, including a write cut off part way.
    let mut line = Wiped(serde_json::to_vec(&record).map_err(std::io::Error::other)?);
    drop(record);
    line.0.push(b'\n');
    write.write_all(&line.0).await?;
    write.flush().await
}

pub(crate) async fn next_event<R: AsyncRead + Unpin>(
    reader: &mut JsonRecordReader<R>,
) -> Option<ServerEvent> {
    let value = reader.next().await.ok()??;
    serde_json::from_value::<ServerRecord>(value)
        .ok()
        .map(|record| record.event)
}

/// One step, bounded and cancellable: `None` when cancelled or timed out. A
/// cancel already made wins over work that is ready too.
async fn step<T>(cancel: &CancellationToken, work: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        done = tokio::time::timeout(STEP, work) => done.ok(),
    }
}

async fn run(
    identity: Identity,
    grant: Grant,
    dial: Dial,
    prerequisites: Prerequisites,
    shared: Arc<Shared>,
    cancel: CancellationToken,
) {
    let no_code = |why: &str| Phase::Finished(Ended::NoCode(why.to_string()));
    // Before any `pair_open` byte could leave, stopping makes no code.
    let ready = tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        done = tokio::time::timeout(PREREQUISITES, prerequisites()) => done.ok(),
    };
    let ready = match ready {
        None if cancel.is_cancelled() => return shared.set(no_code("cancelled")),
        None => return shared.set(no_code("Remote access could not be checked in time.")),
        Some(Err(why)) => return shared.set(no_code(&why)),
        Some(Ok(ready)) => ready,
    };
    let (origin, workspace) = ready;
    shared.advance(Phase::Checking, Phase::Opening);
    let hello = step(&cancel, hello(&dial)).await;
    let instance = match hello {
        Some(Ok(instance)) => instance,
        Some(Err(why)) => return shared.set(no_code(&why)),
        None if cancel.is_cancelled() => return shared.set(no_code("cancelled")),
        None => {
            return shared.set(no_code(
                "The background server did not answer in time; try again.",
            ))
        }
    };
    *shared
        .instance
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(instance.clone());
    let link = step(&cancel, dial()).await;
    // The last point at which no open byte can have left.
    let Some(Ok(link)) = link.filter(|_| !cancel.is_cancelled()) else {
        let why = if cancel.is_cancelled() {
            "cancelled"
        } else {
            "The background server could not be reached."
        };
        return shared.set(no_code(why));
    };
    // From here on, a code may exist.
    let request_start = Instant::now();
    let ended = issue(
        &identity,
        &grant,
        &instance,
        link,
        &shared,
        &cancel,
        request_start,
        &origin,
        &workspace,
    )
    .await;
    match ended {
        Issued::Ended(outcome) => shared.set(Phase::Finished(Ended::Outcome(outcome))),
        Issued::Refused(message) => shared.set(Phase::Finished(Ended::Refused(message))),
        Issued::Uncertain => {
            shared.set(Phase::Closing);
            settle_by_receipt(&identity, &dial, &shared, true).await;
        }
    }
}

/// Learn the server instance on a connection of its own; `Err` says why it
/// could not be learned, as precisely as is known.
async fn hello(dial: &Dial) -> Result<String, String> {
    use localpilot_server::TransportError;
    let link = match dial().await {
        Ok(link) => link,
        Err(TransportError::NotRunning) => return Err(
            "No background server is running for this workspace. Start one with localpilot serve."
                .to_string(),
        ),
        Err(TransportError::Busy) => {
            return Err("The background server is busy; try again.".to_string())
        }
        Err(error) => {
            return Err(format!(
                "The background server could not be reached: {error}"
            ))
        }
    };
    let (read, mut write) = tokio::io::split(link);
    let mut reader = JsonRecordReader::new(read);
    send_record(&mut write, ClientCommand::PairHello)
        .await
        .map_err(|error| format!("The background server could not be reached: {error}"))?;
    match next_event(&mut reader).await {
        Some(ServerEvent::PairingReady { instance }) => Ok(instance),
        Some(ServerEvent::Error { message }) => {
            Err(format!("The background server refused pairing: {message}"))
        }
        Some(_) => Err(
            "The background server does not support pairing from this terminal; update LocalPilot."
                .to_string(),
        ),
        None => Err("The background server closed the connection.".to_string()),
    }
}

/// How the issuer connection ended.
enum Issued {
    /// The server stated the outcome.
    Ended(PairingOutcome),
    /// The server refused to open it: nothing was made.
    Refused(String),
    /// A code may exist; settle by receipt.
    Uncertain,
}

/// The issuer connection (dropped, and so closed, on every return): open,
/// hand over the code, then wait for its end or a cancel.
#[allow(clippy::too_many_arguments)] // one attempt's whole issuer context
async fn issue(
    identity: &Identity,
    grant: &Grant,
    instance: &str,
    link: Box<dyn Link>,
    shared: &Shared,
    cancel: &CancellationToken,
    request_start: Instant,
    origin: &str,
    workspace: &str,
) -> Issued {
    let (read, mut write) = tokio::io::split(link);
    let mut reader = JsonRecordReader::new(read);
    let open = ClientCommand::PairOpen {
        instance: instance.to_string(),
        stream: identity.stream.clone(),
        n: identity.n,
        attempt: identity.attempt.clone(),
        receipt: WireSecret::new(identity.receipt.expose()),
        name: grant.name.clone(),
        scope: grant.scope,
        this_workspace_only: grant.this_workspace_only,
    };
    if !matches!(
        step(cancel, send_record(&mut write, open)).await,
        Some(Ok(()))
    ) {
        return Issued::Uncertain;
    }
    let Some(Some(opened)) = step(cancel, next_event(&mut reader)).await else {
        return Issued::Uncertain;
    };
    match opened {
        ServerEvent::PairingOpened {
            code,
            expires_in_secs,
            attempt,
            instance: answered,
        } if attempt == identity.attempt && answered == instance => {
            // The display ends early, never late.
            let deadline = request_start + Duration::from_secs(expires_in_secs);
            let link = SecretText::new(crate::server_cmd::pairing_link(
                origin,
                code.expose(),
                workspace,
            ));
            drop(code);
            let qr = crate::server_cmd::pairing_qr_rows(link.expose())
                .map(|rows| rows.into_iter().map(SecretText::new).collect());
            shared.offer(Code { link, qr, deadline });
            shared.advance(Phase::Opening, Phase::Active { deadline });
        }
        // The key was already used: nothing new was minted.
        ServerEvent::PairingStatus {
            state,
            instance: answered,
        } if answered == instance => {
            return match state {
                PairingState::Finished { outcome } => Issued::Ended(outcome),
                PairingState::CancelledBeforeOpen => Issued::Ended(PairingOutcome::Cancelled),
                PairingState::Pending | PairingState::Unknown => Issued::Uncertain,
            };
        }
        ServerEvent::Error { message } => return Issued::Refused(message),
        _ => return Issued::Uncertain,
    }
    let ours = |event: &ServerEvent| match event {
        ServerEvent::PairingFinished {
            outcome,
            attempt,
            instance: answered,
        } if *attempt == identity.attempt && answered == instance => Some(outcome.clone()),
        _ => None,
    };
    // Wait for the end, or a cancel (then ask the server to cancel, the
    // whole exchange within its budget).
    let ended = tokio::select! {
        ended = next_event(&mut reader) => ended.as_ref().and_then(ours),
        () = cancel.cancelled() => {
            tokio::time::timeout(CANCEL_ANSWER, async {
                let cancel = ClientCommand::PairCancel { attempt: identity.attempt.clone() };
                send_record(&mut write, cancel).await.ok()?;
                next_event(&mut reader).await.as_ref().and_then(ours)
            })
            .await
            .ok()
            .flatten()
        }
    };
    ended.map_or(Issued::Uncertain, Issued::Ended)
}

/// Settle the attempt with the server by its receipt: cancel it (or, with
/// `cancel` false, only ask). A known outcome ends the attempt; anything else
/// leaves it unconfirmed (a known restart says so).
async fn settle_by_receipt(identity: &Identity, dial: &Dial, shared: &Shared, cancel: bool) {
    let instance = shared.instance();
    let answer = tokio::time::timeout(STEP, async {
        let link = dial().await.ok()?;
        let (read, mut write) = tokio::io::split(link);
        let mut reader = JsonRecordReader::new(read);
        let command = match (&instance, cancel) {
            (Some(instance), true) => ClientCommand::PairCancelReceipt {
                instance: instance.clone(),
                stream: identity.stream.clone(),
                n: identity.n,
                receipt: WireSecret::new(identity.receipt.expose()),
            },
            _ => ClientCommand::PairStatus {
                receipt: WireSecret::new(identity.receipt.expose()),
            },
        };
        send_record(&mut write, command).await.ok()?;
        next_event(&mut reader).await
    })
    .await
    .ok()
    .flatten();
    let restarted = |answered: &str| instance.as_deref().is_some_and(|known| known != answered);
    match answer {
        Some(ServerEvent::PairingStatus {
            instance: answered,
            state,
        }) if restarted(&answered) => {
            // Another server instance cannot speak for this attempt.
            let _ = state;
            shared.set(Phase::Unconfirmed { restarted: true });
        }
        Some(ServerEvent::PairingStatus { state, .. }) => match state {
            PairingState::Finished { outcome } => {
                shared.set(Phase::Finished(Ended::Outcome(outcome)));
            }
            PairingState::CancelledBeforeOpen => {
                shared.set(Phase::Finished(Ended::Outcome(PairingOutcome::Cancelled)));
            }
            PairingState::Pending | PairingState::Unknown => {
                shared.set(Phase::Unconfirmed { restarted: false });
            }
        },
        Some(ServerEvent::Error { message }) if message.contains("another server instance") => {
            shared.set(Phase::Unconfirmed { restarted: true });
        }
        _ => shared.set(Phase::Unconfirmed { restarted: false }),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    use localpilot_llm::FakeProvider;
    use localpilot_server::Endpoint;
    use tokio::io::DuplexStream;

    use super::*;
    use crate::server_cmd::tests::{
        dial as dial_server, exchange_with, pairing_state, register_gateway, spawn_server_on,
        FakeFactory, DEADLINE,
    };
    use crate::server_cmd::HostMap;

    /// What one dial does.
    pub(crate) enum Leg {
        /// Reach this server.
        Real(Endpoint),
        /// Connect to something that never answers.
        Silent,
        /// Connect to something that never reads (a write stalls).
        Stuck,
        /// Never finish dialing.
        Hang,
        /// Reach this server once `gate` is notified (a delayed answer).
        Later(Arc<tokio::sync::Notify>, Endpoint),
        /// An issuer that opens a code, then never reads again (a cancel
        /// write stalls).
        OpensThenStalls,
        /// Cancel the attempt while dialing, then connect to something that
        /// would accept any write.
        CancelWhileDialing(Arc<std::sync::OnceLock<CancellationToken>>),
    }

    /// Dials in order; once the script runs out, every dial reaches `fallback`.
    pub(crate) struct Script {
        legs: Mutex<VecDeque<Leg>>,
        fallback: Endpoint,
        kept: Mutex<Vec<DuplexStream>>,
        dials: AtomicUsize,
    }

    impl Script {
        pub(crate) fn new(fallback: &Endpoint, legs: Vec<Leg>) -> Arc<Self> {
            Arc::new(Self {
                legs: Mutex::new(legs.into()),
                fallback: fallback.clone(),
                kept: Mutex::new(Vec::new()),
                dials: AtomicUsize::new(0),
            })
        }

        pub(crate) fn dials(&self) -> usize {
            self.dials.load(Ordering::SeqCst)
        }

        async fn dialed(&self, at_least: usize) {
            tokio::time::timeout(DEADLINE, async {
                while self.dials() < at_least {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
        }

        pub(crate) fn dial(self: &Arc<Self>) -> Dial {
            let script = self.clone();
            Arc::new(move || {
                let script = script.clone();
                Box::pin(async move {
                    script.dials.fetch_add(1, Ordering::SeqCst);
                    let leg = script.legs.lock().unwrap().pop_front();
                    let link: Box<dyn Link> = match leg {
                        None => Box::new(dial_server(&script.fallback).await),
                        Some(Leg::Real(endpoint)) => Box::new(dial_server(&endpoint).await),
                        Some(Leg::Silent) => {
                            let (near, far) = tokio::io::duplex(64 * 1024);
                            script.kept.lock().unwrap().push(far);
                            Box::new(near)
                        }
                        Some(Leg::Stuck) => {
                            let (near, far) = tokio::io::duplex(1);
                            script.kept.lock().unwrap().push(far);
                            Box::new(near)
                        }
                        Some(Leg::Hang) => std::future::pending().await,
                        Some(Leg::Later(gate, endpoint)) => {
                            gate.notified().await;
                            Box::new(dial_server(&endpoint).await)
                        }
                        Some(Leg::OpensThenStalls) => {
                            let (near, far) = tokio::io::duplex(16);
                            tokio::spawn(opens_then_stalls(far));
                            Box::new(near)
                        }
                        Some(Leg::CancelWhileDialing(token)) => {
                            token.get().unwrap().cancel();
                            let (near, far) = tokio::io::duplex(64 * 1024);
                            script.kept.lock().unwrap().push(far);
                            Box::new(near)
                        }
                    };
                    Ok(link)
                })
            })
        }
    }

    /// Read the open, answer it with a code, then hold the connection open
    /// without reading again.
    async fn opens_then_stalls(far: DuplexStream) {
        use tokio::io::AsyncBufReadExt;
        let (read, mut write) = tokio::io::split(far);
        let mut lines = tokio::io::BufReader::new(read);
        let mut line = String::new();
        lines.read_line(&mut line).await.unwrap();
        let record: ClientRecord = serde_json::from_str(&line).unwrap();
        let ClientCommand::PairOpen {
            instance, attempt, ..
        } = record.command
        else {
            panic!("expected the open first")
        };
        let opened = ServerRecord {
            v: RPC_PROTOCOL_VERSION,
            id: None,
            seq: None,
            event: ServerEvent::PairingOpened {
                code: WireSecret::new("fake-code"),
                expires_in_secs: 600,
                attempt,
                instance,
            },
        };
        let mut bytes = serde_json::to_vec(&opened).unwrap();
        bytes.push(b'\n');
        write.write_all(&bytes).await.unwrap();
        // Never read again; keep both halves alive.
        std::future::pending::<()>().await;
        drop((lines, write));
    }

    pub(crate) fn ready() -> Prerequisites {
        Arc::new(|| {
            Box::pin(async { Ok(("https://pc.example.test".to_string(), "w1".to_string())) })
        })
    }

    fn grant() -> Grant {
        Grant {
            name: "phone".to_string(),
            scope: Scope::Control,
            this_workspace_only: false,
        }
    }

    pub(crate) fn daemon(connections: usize) -> (Endpoint, HostMap) {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let hosts = HostMap::default();
        let (endpoint, _server) = spawn_server_on(factory, connections, hosts.clone());
        (endpoint, hosts)
    }

    fn start(script: &Arc<Script>, prerequisites: Prerequisites) -> Attempt {
        Attempt::start(&Issuer::new(), script.dial(), prerequisites, grant()).unwrap()
    }

    async fn reach(attempt: &Attempt, what: &str, done: impl Fn(&Phase) -> bool) -> Phase {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let phase = attempt.phase();
                if done(&phase) {
                    return phase;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("never reached {what}: {:?}", attempt.phase()))
    }

    fn finished(phase: &Phase) -> bool {
        matches!(phase, Phase::Finished(_))
    }

    fn active(phase: &Phase) -> bool {
        matches!(phase, Phase::Active { .. })
    }

    fn unconfirmed(phase: &Phase) -> bool {
        matches!(phase, Phase::Unconfirmed { .. })
    }

    fn cancelled() -> Phase {
        Phase::Finished(Ended::Outcome(PairingOutcome::Cancelled))
    }

    pub(crate) fn code_of(link: &str) -> String {
        let after = link.split("#pair=").nth(1).unwrap();
        after.split('&').next().unwrap().to_string()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_code_shown_and_used_ends_used_and_is_handed_over_once() {
        let (endpoint, hosts) = daemon(16);
        let (_gateway, capability) = register_gateway(&endpoint).await;
        let script = Script::new(&endpoint, vec![]);
        let attempt = start(&script, ready());
        reach(&attempt, "active", active).await;
        let code = attempt.take_code().unwrap();
        assert!(attempt.take_code().is_none(), "handed over once");
        let link = code.link.expose().to_string();
        assert!(link.starts_with("https://pc.example.test/#pair="), "{link}");
        assert!(link.ends_with("&w=w1"), "{link}");
        assert!(code.qr.as_ref().is_ok_and(|rows| !rows.is_empty()));
        assert!(!format!("{code:?}").contains(&code_of(&link)));
        assert!(!format!("{attempt:?}").contains(attempt.identity.receipt.expose()));
        let paired = exchange_with(&endpoint, &capability, &code_of(&link)).await;
        assert!(
            matches!(paired, ServerEvent::DevicePaired { .. }),
            "{paired:?}"
        );
        let ended = reach(&attempt, "finished", finished).await;
        assert!(
            matches!(
                &ended,
                Phase::Finished(Ended::Outcome(PairingOutcome::Used { name, .. })) if name == "phone"
            ),
            "{ended:?}"
        );
        assert_eq!(hosts.devices.list().unwrap().len(), 1);
        assert_eq!(script.dials(), 2, "hello and open only");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn closing_a_shown_code_hides_it_at_once_and_cancels_it() {
        let (endpoint, _hosts) = daemon(16);
        let (_gateway, capability) = register_gateway(&endpoint).await;
        let script = Script::new(&endpoint, vec![]);
        let attempt = start(&script, ready());
        reach(&attempt, "active", active).await;
        let code = code_of(attempt.take_code().unwrap().link.expose());
        attempt.close();
        assert!(attempt.take_code().is_none());
        assert!(!active(&attempt.phase()), "closing shows at once");
        assert_eq!(reach(&attempt, "finished", finished).await, cancelled());
        let refused = exchange_with(&endpoint, &capability, &code).await;
        assert!(matches!(refused, ServerEvent::Error { .. }), "{refused:?}");
        assert_eq!(script.dials(), 2, "the issuer's own cancel was answered");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_code_waiting_when_the_view_closes_is_wiped_unshown() {
        let (endpoint, _hosts) = daemon(16);
        let script = Script::new(&endpoint, vec![]);
        let attempt = start(&script, ready());
        reach(&attempt, "active", active).await;
        // The code waits in its slot; the view goes before taking it.
        attempt.close();
        assert!(attempt.take_code().is_none(), "never handed over");
        assert_eq!(reach(&attempt, "finished", finished).await, cancelled());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_code_arriving_after_the_view_retired_is_never_kept() {
        let (endpoint, _hosts) = daemon(16);
        let script = Script::new(&endpoint, vec![]);
        let gate = Arc::new(tokio::sync::Notify::new());
        let prerequisites: Prerequisites = {
            let gate = gate.clone();
            Arc::new(move || {
                let gate = gate.clone();
                Box::pin(async move {
                    gate.notified().await;
                    Ok(("https://pc.example.test".to_string(), "w1".to_string()))
                })
            })
        };
        let attempt = start(&script, prerequisites);
        attempt.retire();
        gate.notify_one();
        reach(&attempt, "active", active).await;
        assert!(attempt.take_code().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn closing_while_checking_sends_nothing() {
        let (endpoint, _hosts) = daemon(4);
        let script = Script::new(&endpoint, vec![]);
        let never: Prerequisites = Arc::new(|| Box::pin(std::future::pending()));
        let attempt = start(&script, never);
        tokio::time::sleep(Duration::from_millis(50)).await;
        attempt.close();
        let ended = reach(&attempt, "finished", finished).await;
        assert!(
            matches!(ended, Phase::Finished(Ended::NoCode(_))),
            "{ended:?}"
        );
        assert_eq!(script.dials(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unmet_prerequisites_end_with_their_reason_and_no_code() {
        let (endpoint, _hosts) = daemon(4);
        let script = Script::new(&endpoint, vec![]);
        let off: Prerequisites =
            Arc::new(|| Box::pin(async { Err("Remote access is disabled.".to_string()) }));
        let attempt = start(&script, off);
        assert_eq!(
            reach(&attempt, "finished", finished).await,
            Phase::Finished(Ended::NoCode("Remote access is disabled.".to_string()))
        );
        assert_eq!(script.dials(), 0);
    }

    /// Close during the first leg or the second dial: no open byte can have
    /// left, so the attempt ends with no code and nothing is settled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn closing_before_the_open_is_written_makes_no_code() {
        let (endpoint, _hosts) = daemon(8);
        for (legs, dialed) in [
            (vec![Leg::Silent], 1),
            (vec![Leg::Hang], 1),
            (vec![Leg::Real(endpoint.clone()), Leg::Hang], 2),
        ] {
            let script = Script::new(&endpoint, legs);
            let attempt = start(&script, ready());
            script.dialed(dialed).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            attempt.close();
            let ended = reach(&attempt, "finished", finished).await;
            assert!(
                matches!(ended, Phase::Finished(Ended::NoCode(_))),
                "{ended:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(script.dials(), dialed, "nothing settled");
        }
    }

    /// Close while the open is being written, or while its answer is awaited:
    /// a code may exist, so it is settled by receipt on the same identity.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn closing_once_the_open_may_have_left_settles_by_receipt() {
        let (endpoint, _hosts) = daemon(16);
        for leg in [Leg::Stuck, Leg::Silent] {
            let script = Script::new(&endpoint, vec![Leg::Real(endpoint.clone()), leg]);
            let attempt = start(&script, ready());
            script.dialed(2).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            attempt.close();
            assert_eq!(reach(&attempt, "finished", finished).await, cancelled());
            assert_eq!(script.dials(), 3, "one settle by receipt");
            // The receipt is spent on the server: it can never open a code.
            assert_eq!(
                pairing_state(&endpoint, attempt.identity.receipt.expose()).await,
                PairingState::CancelledBeforeOpen
            );
        }
    }

    /// A step that times out after the open may have left is settled the
    /// same way as a close.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_open_left_unanswered_is_settled_by_receipt() {
        let (endpoint, _hosts) = daemon(8);
        let script = Script::new(&endpoint, vec![Leg::Real(endpoint.clone()), Leg::Silent]);
        let attempt = start(&script, ready());
        assert_eq!(reach(&attempt, "finished", finished).await, cancelled());
        assert_eq!(script.dials(), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_unanswered_settle_is_unconfirmed_and_retry_and_check_run_one_at_a_time() {
        let (endpoint, _hosts) = daemon(8);
        let script = Script::new(
            &endpoint,
            vec![
                Leg::Real(endpoint.clone()),
                Leg::Silent,
                Leg::Silent,
                Leg::Silent,
            ],
        );
        let attempt = start(&script, ready());
        script.dialed(2).await;
        attempt.close();
        assert_eq!(
            reach(&attempt, "unconfirmed", unconfirmed).await,
            Phase::Unconfirmed { restarted: false }
        );
        assert!(attempt.retry());
        assert!(attempt.settling());
        assert!(!attempt.retry(), "one at a time");
        assert!(!attempt.check(), "one at a time");
        tokio::time::timeout(DEADLINE, async {
            while attempt.settling() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(attempt.phase(), Phase::Unconfirmed { restarted: false });
        // The server answers this time.
        assert!(attempt.check());
        reach(&attempt, "checked", |_| !attempt.settling()).await;
        assert_eq!(
            attempt.phase(),
            Phase::Unconfirmed { restarted: false },
            "never opened: a check alone settles nothing"
        );
        assert!(attempt.retry());
        assert_eq!(reach(&attempt, "finished", finished).await, cancelled());
        assert!(!attempt.retry(), "nothing left to settle");
        assert_eq!(script.dials(), 6);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_settle_answered_by_a_restarted_server_says_so() {
        let (first, _a) = daemon(4);
        let (second, _b) = daemon(4);
        let script = Script::new(&second, vec![Leg::Real(first.clone()), Leg::Silent]);
        let attempt = start(&script, ready());
        script.dialed(2).await;
        attempt.close();
        assert_eq!(
            reach(&attempt, "unconfirmed", unconfirmed).await,
            Phase::Unconfirmed { restarted: true }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dropping_an_attempt_ends_its_work() {
        let (endpoint, _hosts) = daemon(8);
        let script = Script::new(&endpoint, vec![Leg::Real(endpoint.clone()), Leg::Silent]);
        let attempt = start(&script, ready());
        script.dialed(2).await;
        drop(attempt);
        tokio::time::sleep(STEP + Duration::from_millis(500)).await;
        assert_eq!(script.dials(), 2, "no settle outlives its attempt");
    }

    fn shared_at(phase: Phase) -> Shared {
        let shared = Shared::new();
        *shared.phase() = phase;
        shared
    }

    #[test]
    fn an_outcome_is_never_overwritten_and_progress_never_undoes_a_close() {
        let shared = shared_at(Phase::Checking);
        shared.set(cancelled());
        shared.set(Phase::Unconfirmed { restarted: false });
        shared.set(Phase::Closing);
        shared.advance(Phase::Checking, Phase::Opening);
        assert_eq!(*shared.phase(), cancelled());
        let shared = shared_at(Phase::Closing);
        shared.advance(Phase::Checking, Phase::Opening);
        shared.advance(
            Phase::Opening,
            Phase::Active {
                deadline: Instant::now(),
            },
        );
        assert_eq!(*shared.phase(), Phase::Closing);
    }

    #[test]
    fn request_numbers_are_never_wrapped() {
        let issuer = Issuer::new();
        issuer.next.store(u64::MAX - 1, Ordering::SeqCst);
        assert_eq!(issuer.take(), Some(u64::MAX - 1));
        assert_eq!(issuer.take(), None);
        assert_eq!(issuer.take(), None);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancel_whose_write_stalls_is_bounded_and_settled_by_receipt() {
        let (endpoint, _hosts) = daemon(8);
        let script = Script::new(
            &endpoint,
            vec![Leg::Real(endpoint.clone()), Leg::OpensThenStalls],
        );
        let attempt = start(&script, ready());
        reach(&attempt, "active", active).await;
        let closed = Instant::now();
        attempt.close();
        assert_eq!(reach(&attempt, "finished", finished).await, cancelled());
        assert!(
            closed.elapsed() < CANCEL_ANSWER + STEP,
            "bounded: {:?}",
            closed.elapsed()
        );
        assert_eq!(script.dials(), 3, "the issuer leg, then one settle");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancel_made_while_dialing_sends_no_open() {
        let (endpoint, _hosts) = daemon(4);
        let token = Arc::new(std::sync::OnceLock::new());
        let script = Script::new(
            &endpoint,
            vec![
                Leg::Real(endpoint.clone()),
                Leg::CancelWhileDialing(token.clone()),
            ],
        );
        let gate = Arc::new(tokio::sync::Notify::new());
        let prerequisites: Prerequisites = {
            let gate = gate.clone();
            Arc::new(move || {
                let gate = gate.clone();
                Box::pin(async move {
                    gate.notified().await;
                    Ok(("https://pc.example.test".to_string(), "w1".to_string()))
                })
            })
        };
        let attempt = start(&script, prerequisites);
        token.set(attempt.cancel.clone()).unwrap();
        gate.notify_one();
        let ended = reach(&attempt, "finished", finished).await;
        assert!(
            matches!(ended, Phase::Finished(Ended::NoCode(_))),
            "{ended:?}"
        );
        let mut far = script.kept.lock().unwrap().pop().unwrap();
        let mut seen = Vec::new();
        let _ = tokio::time::timeout(
            Duration::from_millis(100),
            tokio::io::AsyncReadExt::read_to_end(&mut far, &mut seen),
        )
        .await;
        assert!(seen.is_empty(), "no open byte left: {} bytes", seen.len());
        assert_eq!(script.dials(), 2, "nothing settled");
    }

    /// The code is waiting, unshown, when the phone uses it: the slot is
    /// emptied with the outcome and never refilled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_ended_attempt_never_hands_over_its_code() {
        let (endpoint, _hosts) = daemon(16);
        let (_gateway, capability) = register_gateway(&endpoint).await;
        let script = Script::new(&endpoint, vec![]);
        let attempt = start(&script, ready());
        reach(&attempt, "active", active).await;
        let code = attempt
            .shared
            .slot()
            .as_ref()
            .map(|code| code_of(code.link.expose()))
            .unwrap();
        let paired = exchange_with(&endpoint, &capability, &code).await;
        assert!(
            matches!(paired, ServerEvent::DevicePaired { .. }),
            "{paired:?}"
        );
        let ended = reach(&attempt, "finished", finished).await;
        assert!(
            matches!(
                ended,
                Phase::Finished(Ended::Outcome(PairingOutcome::Used { .. }))
            ),
            "{ended:?}"
        );
        assert!(attempt.shared.slot().is_none(), "emptied with the outcome");
        assert!(attempt.take_code().is_none());
        assert!(!attempt.shared.offer(live_code()), "never refilled");
        assert!(attempt.shared.slot().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_expired_code_is_never_handed_over() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let hosts = crate::server_cmd::tests::hosts_with_pairing_ttl(Duration::from_millis(300));
        let (endpoint, _server) = spawn_server_on(factory, 8, hosts);
        let script = Script::new(&endpoint, vec![]);
        let attempt = start(&script, ready());
        assert_eq!(
            reach(&attempt, "finished", finished).await,
            Phase::Finished(Ended::Outcome(PairingOutcome::Expired))
        );
        assert!(attempt.shared.slot().is_none());
        assert!(attempt.take_code().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn retry_and_check_wait_for_an_unconfirmed_attempt() {
        let (endpoint, _hosts) = daemon(8);
        let script = Script::new(&endpoint, vec![]);
        let attempt = start(&script, ready());
        reach(&attempt, "active", active).await;
        assert!(!attempt.retry(), "not while live");
        assert!(!attempt.check(), "not while live");
        attempt.close();
        assert!(!attempt.retry(), "not beside the attempt's own settle");
        assert_eq!(reach(&attempt, "finished", finished).await, cancelled());
        assert_eq!(script.dials(), 2);
    }

    fn live_code() -> Code {
        Code {
            link: SecretText::new("https://pc.example.test/#pair=x&w=w1".to_string()),
            qr: Ok(Vec::new()),
            deadline: Instant::now() + Duration::from_secs(60),
        }
    }

    #[test]
    fn a_code_is_kept_only_while_it_may_be_shown() {
        let shared = shared_at(Phase::Opening);
        assert!(shared.offer(live_code()));
        assert!(shared.take().is_some());
        let mut late = live_code();
        late.deadline = Instant::now();
        assert!(!shared.offer(late), "already past its deadline");
        assert!(shared.offer(live_code()));
        shared.set(cancelled());
        assert!(shared.slot().is_none(), "ending empties the slot");
        let shared = shared_at(Phase::Opening);
        shared.retire();
        assert!(!shared.offer(live_code()), "retired");
        assert!(shared.take().is_none());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failed_hello_says_what_failed() {
        let (endpoint, _hosts) = daemon(4);
        let absent: Dial =
            Arc::new(|| Box::pin(async { Err(localpilot_server::TransportError::NotRunning) }));
        let attempt = Attempt::start(&Issuer::new(), absent, ready(), grant()).unwrap();
        let ended = reach(&attempt, "finished", finished).await;
        assert!(
            matches!(&ended, Phase::Finished(Ended::NoCode(why)) if why.contains("No background server")),
            "{ended:?}"
        );
        let script = Script::new(&endpoint, vec![Leg::Silent]);
        let attempt = start(&script, ready());
        let ended = reach(&attempt, "finished", finished).await;
        assert!(
            matches!(&ended, Phase::Finished(Ended::NoCode(why)) if why.contains("did not answer in time")),
            "{ended:?}"
        );
    }
}
