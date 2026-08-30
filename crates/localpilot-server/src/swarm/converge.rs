//! The peer-pair convergence protocol: two symmetric peers exchange proposals
//! and revisions of one shared artifact until both agree on the same revision, or
//! a bound stops them.
//!
//! This module holds the pure state core — the versioned-envelope parser and the
//! candidate/agreement state machine — plus a transport-agnostic driver that
//! schedules the two peers over an abstract endpoint trait. It owns no real
//! transport and no real session: a production integration supplies concrete
//! session-host and peer-messaging endpoints behind that trait. Keeping the two
//! apart lets the whole protocol be exercised with no provider, no network, and no
//! clock.
//!
//! Two rules shape the wire format:
//!
//! - **A peer speaks through a versioned envelope, parsed once here.** Everything
//!   downstream sees a typed [`PairAction`], never raw text — so no part of the
//!   driver scans prose to decide what happened.
//! - **A peer never names itself.** Which peer is acting is whichever one the
//!   driver scheduled, not anything the envelope says. An envelope that tries to
//!   claim an identity is not trusted: the field is simply never read.

use std::collections::HashSet;
use std::time::Duration;

use localpilot_core::SessionId;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// The only envelope version this build understands. A peer that speaks a later
/// version is refused loudly rather than misread as this one.
pub(crate) const PROTOCOL_VERSION: u32 = 1;

/// Render the system-prompt contract for one member of a symmetric pair.
///
/// The convergence parser owns this wording because it owns the accepted JSON
/// envelope. Rendering the version from [`PROTOCOL_VERSION`] keeps the model's
/// advertised contract and the parser's contract from drifting apart.
#[must_use]
pub fn pair_session_directive(self_label: &str, other_label: &str, task: &str) -> String {
    format!(
        "You are peer {self_label} in a symmetric two-peer collaboration. The host assigned this \
identity; never claim or infer a different peer identity. Peer {other_label} is working on the \
same original task.\n\n\
Original task:\n\
{task}\n\n\
On every scheduled pair turn, reply with exactly one JSON object and no surrounding prose. Use \
one of:\n\
{{\"v\":{PROTOCOL_VERSION},\"action\":\"propose\",\"artifact\":\"...\"}}\n\
{{\"v\":{PROTOCOL_VERSION},\"action\":\"revise\",\"artifact\":\"...\"}}\n\
{{\"v\":{PROTOCOL_VERSION},\"action\":\"agree\",\"revision\":1,\"digest\":\"...\"}}\n\
Use propose or revise for the complete candidate artifact. Agree only to the current revision and \
digest supplied by the host. You remain an ordinary interactive Agent-mode session: tools, \
approvals, questions, and direct peer messages stay available."
    )
}

/// What a peer did on its turn, as the protocol core sees it — the typed result
/// of parsing one envelope.
///
/// Identity is deliberately absent: the driver knows who acted from the slot it
/// scheduled, so there is nowhere here for a model-supplied "who I am" to land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PairAction {
    /// Put forward a fresh candidate artifact.
    Propose {
        /// The candidate's full text.
        artifact: String,
    },
    /// Replace the current candidate with a changed one.
    Revise {
        /// The revised candidate's full text.
        artifact: String,
    },
    /// Accept the candidate at this exact revision and digest. The driver checks
    /// both against the candidate it currently holds, so an agreement to a
    /// superseded revision cannot pass for agreement to the live one.
    Agree {
        /// The revision the peer means to accept.
        revision: u64,
        /// The digest the peer saw for that revision, echoed back for the driver
        /// to cross-check against its own.
        digest: String,
    },
}

/// Why an envelope could not become a [`PairAction`].
///
/// The driver treats one of these as a single repairable slip — it asks the same
/// peer once more within the same slot; a second failure ends the pair with a
/// protocol outcome rather than looping.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub(crate) enum EnvelopeError {
    /// The envelope names a version this build does not speak.
    #[error("unsupported convergence protocol version {found}; this build speaks {expected}")]
    UnknownVersion {
        /// The version the envelope declared.
        found: u32,
        /// The version this build understands ([`PROTOCOL_VERSION`]).
        expected: u32,
    },
    /// Not valid JSON, or a required field is missing, mistyped, or names an
    /// action that does not exist.
    #[error("malformed convergence envelope: {0}")]
    Malformed(String),
}

/// The wire shape, before it is narrowed to a [`PairAction`]. The version rides
/// *outside* the action arm so it can be checked before the action is trusted.
#[derive(Deserialize)]
struct Envelope {
    /// Protocol version; validated against [`PROTOCOL_VERSION`] before use.
    v: u32,
    /// The action itself, tagged by its `action` field.
    #[serde(flatten)]
    action: WireAction,
}

/// The action arm of the envelope, tagged by `action`.
///
/// Unknown extra fields — including any attempt to name a peer — are ignored
/// rather than rejected, so a spoofed identity is inert instead of a parse error
/// that would cost the peer its one repair.
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum WireAction {
    Propose { artifact: String },
    Revise { artifact: String },
    Agree { revision: u64, digest: String },
}

/// Parse one peer envelope into a typed [`PairAction`] — the single boundary
/// where text becomes protocol.
///
/// # Errors
/// [`EnvelopeError::UnknownVersion`] if the envelope names a version this build
/// does not speak, or [`EnvelopeError::Malformed`] if it is not valid JSON or is
/// missing a field its action needs.
pub(crate) fn parse_action(input: &str) -> Result<PairAction, EnvelopeError> {
    let envelope: Envelope =
        serde_json::from_str(input).map_err(|error| EnvelopeError::Malformed(error.to_string()))?;
    if envelope.v != PROTOCOL_VERSION {
        return Err(EnvelopeError::UnknownVersion {
            found: envelope.v,
            expected: PROTOCOL_VERSION,
        });
    }
    Ok(match envelope.action {
        WireAction::Propose { artifact } => PairAction::Propose { artifact },
        WireAction::Revise { artifact } => PairAction::Revise { artifact },
        WireAction::Agree { revision, digest } => PairAction::Agree { revision, digest },
    })
}

/// The pair's shared candidate: the artifact both peers are converging on, in its
/// canonical form, tagged with the revision it was installed at and its digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    revision: u64,
    digest: String,
    artifact: String,
}

impl Candidate {
    /// The monotonic revision this candidate was installed at.
    #[must_use]
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// The SHA-256 digest of the canonical artifact bytes, lower-case hex.
    #[must_use]
    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }

    /// The canonical artifact text (line endings normalised to LF).
    #[must_use]
    pub(crate) fn artifact(&self) -> &str {
        &self.artifact
    }
}

/// Normalise an artifact to its canonical form before it is digested, so two peers
/// on different platforms agree on the same bytes for the same content.
///
/// The one normalisation applied is line endings: a Windows `\r\n` and a lone `\r`
/// both become `\n`. Nothing else is touched — trailing whitespace and every other
/// byte are preserved, because the line ending is the only difference the platform
/// itself introduces between two identical artifacts, and discarding more would
/// change an artifact the user will see without a correctness reason to.
#[must_use]
pub(crate) fn canonicalize(artifact: &str) -> String {
    artifact.replace("\r\n", "\n").replace('\r', "\n")
}

/// The lower-case hex SHA-256 of some canonical artifact bytes.
fn digest_of(canonical: &str) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(canonical.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Writing a byte to a `String` cannot fail.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// What applying one peer action did to the pair's shared state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Transition {
    /// A fresh candidate replaced whatever was there; every prior agreement is
    /// cleared, so both peers must agree again.
    CandidateInstalled {
        /// The revision the new candidate was installed at.
        revision: u64,
    },
    /// The acting peer agreed to the current candidate; the pair is not yet
    /// unanimous.
    AgreementRecorded {
        /// The revision that was agreed to.
        revision: u64,
    },
    /// Both peers have now agreed to the current candidate — the pair has
    /// converged on this revision.
    Converged {
        /// The revision the pair converged on.
        revision: u64,
    },
    /// A well-formed `Agree` that did not name the current revision *and* digest:
    /// the peer is behind, or there is no candidate yet. The state is not touched.
    /// The driver treats this as a repairable slip and spends the slot's single
    /// repair on it — the same budget a malformed envelope draws from — rather than
    /// a silent skip; a second stale-or-invalid result ends the slot with a
    /// protocol outcome.
    StaleAgreement,
}

/// A failure the state itself refuses, distinct from an ordinary [`Transition`].
/// These are not model slips to be repaired: a foreign actor or an exhausted
/// revision space is a driver-invariant break the driver maps straight to a
/// terminal protocol outcome.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub(crate) enum PairStateError {
    /// An action arrived from a session that is not one of the pair's two peers.
    /// The driver only ever schedules the two peers, so this is an invariant break.
    #[error("action from a session that is not one of the pair's two peers")]
    ForeignActor,
    /// The monotonic revision counter has no next value. Unreachable in any real
    /// run (it would take 2^64 revisions); modelled so monotonicity never silently
    /// wraps into reuse.
    #[error("the pair's revision space is exhausted")]
    RevisionExhausted,
}

/// The pair's shared convergence state: which two peers are converging, the
/// current candidate, and who has agreed to it.
///
/// The driver owns this. Revisions are monotonic and never reused, so an agreement
/// to a superseded candidate can never be mistaken for agreement to the current
/// one even when the peer names an old revision number.
#[derive(Debug, Clone)]
pub(crate) struct PairState {
    peers: [SessionId; 2],
    candidate: Option<Candidate>,
    next_revision: u64,
    agreed: HashSet<SessionId>,
}

impl PairState {
    /// A fresh pair over two peers the caller has *already* checked are distinct.
    /// The public [`PairDriver::new`] is the one place that check happens.
    fn distinct(first: SessionId, second: SessionId) -> Self {
        Self {
            peers: [first, second],
            candidate: None,
            next_revision: 1,
            agreed: HashSet::new(),
        }
    }

    /// The current candidate, if one has been proposed.
    #[must_use]
    pub(crate) fn candidate(&self) -> Option<&Candidate> {
        self.candidate.as_ref()
    }

    /// Whether `peer` has agreed to the current candidate.
    #[must_use]
    pub(crate) fn agreed(&self, peer: SessionId) -> bool {
        self.agreed.contains(&peer)
    }

    /// Apply one peer's action, attributed to the peer the driver scheduled — not
    /// to anything the envelope claimed.
    ///
    /// # Errors
    /// [`PairStateError::ForeignActor`] if `actor` is not one of the pair's peers,
    /// or [`PairStateError::RevisionExhausted`] if a propose/revise has no next
    /// revision. Neither mutates the shared state.
    pub(crate) fn apply(
        &mut self,
        actor: SessionId,
        action: PairAction,
    ) -> Result<Transition, PairStateError> {
        if !self.peers.contains(&actor) {
            return Err(PairStateError::ForeignActor);
        }
        match action {
            PairAction::Propose { artifact } | PairAction::Revise { artifact } => {
                self.install(&artifact)
            }
            PairAction::Agree { revision, digest } => {
                Ok(self.record_agreement(actor, revision, &digest))
            }
        }
    }

    /// Install a fresh candidate at the next monotonic revision, clearing every
    /// prior agreement. A propose and a revise differ only in intent to the peers;
    /// to the shared state both replace the candidate and reset agreement.
    ///
    /// The next revision is reserved *before* the candidate is touched, so an
    /// exhausted counter leaves the current candidate and agreements untouched.
    fn install(&mut self, artifact: &str) -> Result<Transition, PairStateError> {
        let revision = self.next_revision;
        let advanced = revision
            .checked_add(1)
            .ok_or(PairStateError::RevisionExhausted)?;
        let canonical = canonicalize(artifact);
        let digest = digest_of(&canonical);
        self.candidate = Some(Candidate {
            revision,
            digest,
            artifact: canonical,
        });
        self.agreed.clear();
        self.next_revision = advanced;
        Ok(Transition::CandidateInstalled { revision })
    }

    /// Record an agreement, but only to the current candidate. An agreement that
    /// names any other revision, or a mismatched digest, is stale and does not
    /// touch the state.
    fn record_agreement(&mut self, actor: SessionId, revision: u64, digest: &str) -> Transition {
        let Some(candidate) = &self.candidate else {
            return Transition::StaleAgreement;
        };
        if revision != candidate.revision || digest != candidate.digest {
            return Transition::StaleAgreement;
        }
        self.agreed.insert(actor);
        if self.peers.iter().all(|peer| self.agreed.contains(peer)) {
            Transition::Converged { revision }
        } else {
            Transition::AgreementRecorded { revision }
        }
    }
}

/// The instruction handed to a peer on every turn after the first. The other peer
/// has just shared its current proposal (delivered as a peer message immediately
/// before this drive); the peer is asked to respond in kind.
const PAIR_TURN_PROMPT: &str = "The other peer has shared its current proposal. \
Review it and reply with your own proposal, a revision of it, or your agreement.";

/// How the pair stopped. Every terminal reason is its own variant so a surface can
/// tell them apart — a timeout, a failed peer, or a spent budget never collapses
/// into protocol text.
///
/// Non-exhaustive so later terminal reasons stay a forward-compatible addition.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PairOutcome {
    /// Both peers agreed the same candidate at this revision.
    Converged {
        /// The revision the pair converged on.
        revision: u64,
    },
    /// The round cap was reached without convergence.
    CapReached {
        /// How many full rounds ran.
        rounds: u32,
    },
    /// A peer produced a second invalid-or-stale result within one slot, or broke a
    /// driver invariant. The pair stops rather than loop.
    ProtocolError {
        /// What went wrong, for the surface to show.
        detail: String,
    },
    /// A caller aborted the pair through its [`PairAbort`] handle.
    Aborted,
    /// A slot's wall-clock allowance elapsed before its peer replied.
    TimedOut,
    /// A peer's session failed to run its turn (crash, transport loss, delivery
    /// reaching nobody).
    PeerFailed {
        /// What the endpoint reported.
        detail: String,
    },
    /// A peer's model provider errored.
    ProviderError {
        /// What the endpoint reported.
        detail: String,
    },
    /// A slot's token budget was spent before its peer produced a usable reply.
    BudgetExceeded,
    /// A peer's session reported it was making no progress.
    NoProgress,
}

/// One atomic candidate snapshot: the revision, digest, and artifact of a
/// successfully-applied proposal, always carried together — a caller never sees a
/// revision without its bytes, and the driver never invents one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CandidateSnapshot {
    revision: u64,
    digest: String,
    artifact: String,
}

impl CandidateSnapshot {
    /// Snapshot the current candidate.
    fn of(candidate: &Candidate) -> Self {
        Self {
            revision: candidate.revision(),
            digest: candidate.digest().to_string(),
            artifact: candidate.artifact().to_string(),
        }
    }

    /// The revision this candidate was installed at.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The SHA-256 digest of the canonical artifact bytes, lower-case hex.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The canonical artifact text.
    #[must_use]
    pub fn artifact(&self) -> &str {
        &self.artifact
    }
}

/// The full result of a pair run: the terminal reason plus everything retained for
/// a caller to present or diagnose — never fabricated, honest about what is missing.
#[non_exhaustive]
pub struct PairReport {
    outcome: PairOutcome,
    completed_rounds: u32,
    peers: [SessionId; 2],
    raw: [Option<String>; 2],
    candidate: Option<CandidateSnapshot>,
}

impl PairReport {
    /// Why the pair stopped.
    #[must_use]
    pub fn reason(&self) -> &PairOutcome {
        &self.outcome
    }

    /// How many full rounds completed before the pair stopped — the same count the
    /// live progress reported, read back from the retained snapshot.
    #[must_use]
    pub fn completed_rounds(&self) -> u32 {
        self.completed_rounds
    }

    /// The two scheduled peers, in stable order (index-aligned with [`raw`](Self::raw)).
    #[must_use]
    pub fn peers(&self) -> [SessionId; 2] {
        self.peers
    }

    /// Each peer's latest raw produced envelope, index-aligned with
    /// [`peers`](Self::peers). `None` for a peer that never produced one.
    #[must_use]
    pub fn raw(&self) -> &[Option<String>; 2] {
        &self.raw
    }

    /// The latest raw produced envelope for `peer`, if it produced one.
    #[must_use]
    pub fn raw_for(&self, peer: SessionId) -> Option<&str> {
        self.peers
            .iter()
            .position(|candidate| *candidate == peer)
            .and_then(|index| self.raw[index].as_deref())
    }

    /// The last successfully-applied candidate, preserved across a later stop; `None`
    /// if none was ever installed.
    #[must_use]
    pub fn candidate(&self) -> Option<&CandidateSnapshot> {
        self.candidate.as_ref()
    }
}

/// A read-only, deterministic snapshot of a pair's progress.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PairProgress {
    completed_rounds: u32,
    max_rounds: u32,
    next_peer: Option<SessionId>,
    candidate: Option<CandidateSnapshot>,
    agreements: [(SessionId, bool); 2],
    repairing_peer: Option<SessionId>,
}

impl PairProgress {
    /// How many full rounds have completed.
    #[must_use]
    pub fn completed_rounds(&self) -> u32 {
        self.completed_rounds
    }

    /// The round cap.
    #[must_use]
    pub fn max_rounds(&self) -> u32 {
        self.max_rounds
    }

    /// The peer whose turn is scheduled next, or `None` once the pair has stopped —
    /// a terminal snapshot never claims a next peer.
    #[must_use]
    pub fn next_peer(&self) -> Option<SessionId> {
        self.next_peer
    }

    /// The current candidate, if one is installed.
    #[must_use]
    pub fn candidate(&self) -> Option<&CandidateSnapshot> {
        self.candidate.as_ref()
    }

    /// Each peer and whether it has agreed to the current candidate, in stable order.
    #[must_use]
    pub fn agreements(&self) -> [(SessionId, bool); 2] {
        self.agreements
    }

    /// The peer currently spending its slot's single repair, if any. Set the moment
    /// the driver decides to repair a malformed or stale result, and cleared by the
    /// next valid-slot publish or the terminal snapshot — never inferred from prose.
    #[must_use]
    pub fn repairing_peer(&self) -> Option<SessionId> {
        self.repairing_peer
    }
}

/// A read-only receiver for a pair's [`PairProgress`]. Obtain it from
/// [`PairDriver::progress`] before the driver is run; it can never reach the
/// driver's mutable state. [`latest`](Self::latest) reads the current value without
/// blocking; [`changed`](Self::changed) asynchronously awaits the next update
/// (suspending the task, never blocking an executor thread).
#[derive(Clone)]
pub struct PairProgressRx(watch::Receiver<PairProgress>);

impl PairProgressRx {
    /// The latest progress snapshot. Never blocks.
    #[must_use]
    pub fn latest(&self) -> PairProgress {
        self.0.borrow().clone()
    }

    /// Await the next progress update, returning it — or `None` once the pair has
    /// finished and its sole sender has dropped (the run consuming the driver).
    pub async fn changed(&mut self) -> Option<PairProgress> {
        match self.0.changed().await {
            Ok(()) => Some(self.0.borrow_and_update().clone()),
            Err(_) => None,
        }
    }
}

/// Why an endpoint operation failed — distinct from anything the *protocol* can go
/// wrong about, so a failed session or provider never reads as a malformed message.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum EndpointError {
    /// The peer's session could not run its turn, or a delivery reached nobody.
    #[error("peer session failed: {0}")]
    PeerFailed(String),
    /// The peer's model provider errored.
    #[error("provider error: {0}")]
    ProviderError(String),
}

/// What a driven turn produced. The session's own terminal stops are carried
/// through typed, so the driver never has to rediscover them from prose.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnReply {
    /// The peer produced an envelope, at some token cost.
    Produced {
        /// The raw envelope text the peer emitted.
        envelope: String,
        /// The tokens the turn cost, counted against the slot's budget.
        cost: u64,
    },
    /// The turn observed the driver's cancellation signal and stopped cleanly,
    /// releasing its own resources. Whether that was an abort or a slot timeout is
    /// the driver's to decide.
    Cancelled,
    /// The session itself stopped on its own timeout.
    TimedOut,
    /// The session itself stopped on its own budget.
    BudgetExceeded,
    /// The session itself stopped reporting no progress.
    NoProgress,
}

/// What a delivery produced.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NotifyReply {
    /// The message was delivered to the recipient.
    Delivered,
    /// The delivery observed the driver's cancellation signal and stopped cleanly.
    Cancelled,
}

/// The two things the driver needs from the outside world, kept behind a trait so
/// the protocol runs over deterministic fakes in tests and over real session hosts
/// in production, with no protocol change between them.
///
/// There is deliberately no "wake" here: the only thing that starts a turn is
/// [`drive`](PairEndpoints::drive), which the driver alone calls, one at a time.
///
/// The futures are `Send` so the driver can be spawned or multiplexed alongside
/// other work; the real session-peer and host operations are all `Send`-capable,
/// so requiring it costs nothing and avoids a public API that could never be
/// driven from a spawned task.
pub trait PairEndpoints {
    /// Deliver `content` from `from` to `to` as an Audience::One / Notify peer
    /// message: labelled as coming from the peer that produced it, enqueued for the
    /// recipient's next turn, and never starting one. Completes before the driver
    /// drives the recipient.
    ///
    /// `cancel` is the same per-slot signal the drive observes: a delivery that
    /// cannot complete promptly must stop on it and return [`NotifyReply::Cancelled`]
    /// rather than block — the driver never drops this future either.
    ///
    /// # Errors
    /// [`EndpointError`] if the message reached nobody or the transport failed — a
    /// delivery that lands nowhere must not read as success.
    fn notify(
        &mut self,
        from: SessionId,
        to: SessionId,
        content: &str,
        cancel: &CancellationToken,
    ) -> impl std::future::Future<Output = Result<NotifyReply, EndpointError>> + Send;

    /// Drive `peer`'s scheduled turn with `prompt` and return what it produced. The
    /// sole turn-starting call.
    ///
    /// `cancel` is the driver's per-slot signal: when it fires (an abort or the slot
    /// deadline), the implementation must stop the turn, release its resources, and
    /// return [`TurnReply::Cancelled`] — the driver never simply drops this future,
    /// so a real session's in-flight turn is torn down cleanly rather than leaked.
    ///
    /// # Errors
    /// [`EndpointError`] if the session or its provider failed.
    fn drive(
        &mut self,
        peer: SessionId,
        prompt: &str,
        cancel: &CancellationToken,
    ) -> impl std::future::Future<Output = Result<TurnReply, EndpointError>> + Send;
}

/// One driven envelope's effect, once parsed and applied, classified for the slot
/// loop.
enum StepResult {
    /// The pair converged on this revision.
    Converged(u64),
    /// The candidate advanced or an agreement was recorded; move to the next slot.
    Progressed,
    /// A malformed envelope or a stale agreement: the slot's one repair answers it.
    Repairable(String),
    /// A driver-invariant break (foreign actor, revision exhaustion): stop now.
    Fatal(String),
}

/// Why a [`PairDriver`] could not be constructed — the setup mistakes a caller can
/// make, and only those. Internal invariant breaks are never surfaced here; they
/// end a run as a [`PairOutcome::ProtocolError`] instead.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PairSetupError {
    /// The two peers are the same session; a pair is exactly two distinct peers.
    #[error("a pair needs two distinct peers")]
    DuplicatePeers,
    /// The round cap was zero; a pair must be allowed at least one round.
    #[error("a pair needs a round cap of at least one")]
    ZeroRoundCap,
    /// The slot timeout was zero; every slot would time out before it ran.
    #[error("a pair needs a slot timeout greater than zero")]
    ZeroSlotTimeout,
}

/// The bounds that keep a pair finite: how many rounds, how long a single slot may
/// take, and how many tokens one slot may spend.
#[derive(Debug, Clone)]
pub struct PairBounds {
    /// The most rounds (one scheduled turn per peer) before a non-converged stop.
    pub max_rounds: u32,
    /// The wall-clock a single slot may take, covering its delivery, its drive, and
    /// its one optional repair drive as a whole. A repair does not reset it.
    pub slot_timeout: Duration,
    /// The token budget for one slot, spanning its primary and repair drives
    /// together; a repair does not reset it. `0` disables the budget.
    pub slot_token_budget: u64,
}

/// A handle that aborts a running pair from elsewhere — a UI, a supervisor — either
/// before [`PairDriver::run`] starts or while it is in flight. Cheap to clone.
#[derive(Debug, Clone)]
pub struct PairAbort(CancellationToken);

impl PairAbort {
    /// Ask the pair to stop. The in-flight slot is cancelled and the run ends in
    /// [`PairOutcome::Aborted`]. Idempotent.
    pub fn abort(&self) {
        self.0.cancel();
    }
}

/// Map an endpoint failure to the outcome it ends the pair with.
fn endpoint_outcome(error: EndpointError) -> PairOutcome {
    match error {
        EndpointError::PeerFailed(detail) => PairOutcome::PeerFailed { detail },
        EndpointError::ProviderError(detail) => PairOutcome::ProviderError { detail },
    }
}

/// An endpoint that reports a clean cancellation when the driver set no signal is
/// misbehaving; treat it as a failed peer rather than invent a timeout.
fn cancellation_anomaly() -> PairOutcome {
    PairOutcome::PeerFailed {
        detail: "endpoint reported cancellation without a driver signal".to_string(),
    }
}

/// Drives two symmetric peers through the convergence protocol, one scheduled turn
/// at a time, until they agree or a bound stops them.
///
/// The driver owns turn order and the shared [`PairState`]; the transport and the
/// sessions live behind [`PairEndpoints`]. Exactly one peer turn is ever in flight.
///
/// It is a one-shot: [`run`](PairDriver::run) consumes it, and it is not `Clone`, so
/// a single pair cannot be resumed after it ends or split into two schedulers over
/// the same sessions.
#[derive(Debug)]
pub struct PairDriver {
    state: PairState,
    peers: [SessionId; 2],
    original_task: String,
    bounds: PairBounds,
    cancel: CancellationToken,
    progress: watch::Sender<PairProgress>,
}

impl PairDriver {
    /// A driver for two distinct peers on one task, held to `bounds`.
    ///
    /// # Errors
    /// [`PairSetupError::DuplicatePeers`] if the two peers are the same session,
    /// [`PairSetupError::ZeroRoundCap`] if `bounds.max_rounds` is zero, or
    /// [`PairSetupError::ZeroSlotTimeout`] if `bounds.slot_timeout` is zero.
    pub fn new(
        first: SessionId,
        second: SessionId,
        original_task: impl Into<String>,
        bounds: PairBounds,
    ) -> Result<Self, PairSetupError> {
        if bounds.max_rounds == 0 {
            return Err(PairSetupError::ZeroRoundCap);
        }
        if bounds.slot_timeout.is_zero() {
            return Err(PairSetupError::ZeroSlotTimeout);
        }
        if first == second {
            return Err(PairSetupError::DuplicatePeers);
        }
        let peers = [first, second];
        let (progress, _) = watch::channel(PairProgress {
            completed_rounds: 0,
            max_rounds: bounds.max_rounds,
            next_peer: Some(first),
            candidate: None,
            agreements: [(first, false), (second, false)],
            repairing_peer: None,
        });
        Ok(Self {
            state: PairState::distinct(first, second),
            peers,
            original_task: original_task.into(),
            bounds,
            cancel: CancellationToken::new(),
            progress,
        })
    }

    /// A handle that aborts this pair — before or during [`run`](Self::run).
    #[must_use]
    pub fn abort_handle(&self) -> PairAbort {
        PairAbort(self.cancel.clone())
    }

    /// A read-only progress receiver. Obtain it before [`run`](Self::run) consumes
    /// the driver; it never reaches the driver's state. `latest()` reads without
    /// blocking; `changed()` asynchronously awaits the next update.
    #[must_use]
    pub fn progress(&self) -> PairProgressRx {
        PairProgressRx(self.progress.subscribe())
    }

    /// The outcome to return if the pair has been aborted or the slot deadline has
    /// fired, checked in that precedence: an abort wins over a coincident deadline.
    fn stop_precedence(&self, deadline_fired: bool) -> Option<PairOutcome> {
        if self.cancel.is_cancelled() {
            Some(PairOutcome::Aborted)
        } else if deadline_fired {
            Some(PairOutcome::TimedOut)
        } else {
            None
        }
    }

    /// Publish the current state with no repair in flight. `next_peer` is the peer
    /// scheduled for the upcoming slot, or `None` for the final terminal snapshot.
    /// Every scheduled-slot and terminal publish clears a prior repair signal.
    /// Nonblocking; a run with no observers simply updates the retained value.
    fn publish(&self, next_peer: Option<SessionId>, completed_rounds: u32) {
        self.publish_state(next_peer, completed_rounds, None);
    }

    /// Publish the moment the driver spends a slot's single repair on `peer`: that
    /// same peer stays scheduled for its repair drive, and the repair signal is set.
    fn publish_repairing(&self, peer: SessionId, completed_rounds: u32) {
        self.publish_state(Some(peer), completed_rounds, Some(peer));
    }

    /// The one place a progress snapshot is written, so the repair set/clear is
    /// auditable at every call site. Candidate and agreements are always the current
    /// authoritative state; only `next_peer` and `repairing_peer` vary per call.
    fn publish_state(
        &self,
        next_peer: Option<SessionId>,
        completed_rounds: u32,
        repairing_peer: Option<SessionId>,
    ) {
        let _previous = self.progress.send_replace(PairProgress {
            completed_rounds,
            max_rounds: self.bounds.max_rounds,
            next_peer,
            candidate: self.state.candidate().map(CandidateSnapshot::of),
            agreements: [
                (self.peers[0], self.state.agreed(self.peers[0])),
                (self.peers[1], self.state.agreed(self.peers[1])),
            ],
            repairing_peer,
        });
    }

    /// Run the pair to a terminal [`PairReport`], driving each scheduled peer
    /// through `endpoints`.
    ///
    /// The first proposer is driven on the original task with nothing delivered;
    /// every later turn first delivers the current candidate to the scheduled peer,
    /// then drives it on [`PAIR_TURN_PROMPT`]. A malformed or stale result spends
    /// the slot's single repair on the same peer — no delivery to the other peer,
    /// no slot advance; a second bad result ends the pair with `ProtocolError`.
    ///
    /// One wall-clock allowance and one token budget cover each slot's delivery,
    /// drive, and optional repair together; a repair resets neither. When the
    /// allowance elapses, or a caller aborts, the in-flight delivery or drive is
    /// *cancelled through a token it observes* — never dropped — so a real session's
    /// turn is torn down cleanly. Before and after every endpoint call the driver
    /// checks, in order, abort then deadline; a pair aborted before its turn makes
    /// no endpoint calls at all.
    ///
    /// The returned [`PairReport`] carries the terminal reason, each peer's latest
    /// raw response, and the last applied candidate — all preserved even when a
    /// later stop ends the run.
    pub async fn run(mut self, endpoints: &mut impl PairEndpoints) -> PairReport {
        let mut raw: [Option<String>; 2] = [None, None];
        let outcome = self.run_loop(endpoints, &mut raw).await;
        let candidate = self.state.candidate().map(CandidateSnapshot::of);
        // Read the round count back from the authoritative progress the loop already
        // published, so the retained report agrees with the last live snapshot.
        let completed_rounds = self.progress.borrow().completed_rounds();
        PairReport {
            outcome,
            completed_rounds,
            peers: self.peers,
            raw,
            candidate,
        }
    }

    /// The scheduling loop: drives the pair to a terminal [`PairOutcome`], recording
    /// each peer's latest raw response into `raw` and publishing progress on every
    /// state change.
    async fn run_loop(
        &mut self,
        endpoints: &mut impl PairEndpoints,
        raw: &mut [Option<String>; 2],
    ) -> PairOutcome {
        let mut slot: usize = 0;
        let mut rounds: u32 = 0;
        let outcome = loop {
            // A pair aborted before its turn makes zero endpoint calls.
            if self.cancel.is_cancelled() {
                break PairOutcome::Aborted;
            }

            let peer = self.peers[slot % 2];
            let other = self.peers[(slot + 1) % 2];
            let delivery = self.state.candidate().map(render_delivery);
            let base_prompt = if delivery.is_none() {
                self.original_task.clone()
            } else {
                PAIR_TURN_PROMPT.to_string()
            };

            // One allowance and one budget for the whole slot. The child fires on the
            // pair abort (its parent) or when this deadline elapses.
            let child = self.cancel.child_token();
            let deadline = tokio::time::sleep(self.bounds.slot_timeout);
            tokio::pin!(deadline);
            let mut deadline_fired = false;
            let mut slot_spent: u64 = 0;

            let mut repaired = false;
            let mut repair_prompt = String::new();

            let terminal: Option<PairOutcome> = 'slot: loop {
                // ── delivery (first drive of the slot only) ──
                if !repaired {
                    if let Some(content) = &delivery {
                        if let Some(outcome) = self.stop_precedence(deadline_fired) {
                            break 'slot Some(outcome);
                        }
                        let reply = {
                            let notify = endpoints.notify(other, peer, content, &child);
                            tokio::pin!(notify);
                            loop {
                                // Biased, deadline first: if the delivery and the
                                // deadline are ready together, the deadline wins —
                                // it sets the flag and cancels the child without
                                // breaking, then the re-poll takes the (still-ready)
                                // reply and post-op precedence returns `TimedOut`.
                                tokio::select! {
                                    biased;
                                    () = &mut deadline, if !deadline_fired => {
                                        deadline_fired = true;
                                        child.cancel();
                                    }
                                    result = &mut notify => break result,
                                }
                            }
                        };
                        if let Some(outcome) = self.stop_precedence(deadline_fired) {
                            break 'slot Some(outcome);
                        }
                        match reply {
                            Err(error) => break 'slot Some(endpoint_outcome(error)),
                            Ok(NotifyReply::Delivered) => {}
                            Ok(NotifyReply::Cancelled) => break 'slot Some(cancellation_anomaly()),
                        }
                    }
                }

                // ── drive (primary, then optional repair — same peer, same slot) ──
                if let Some(outcome) = self.stop_precedence(deadline_fired) {
                    break 'slot Some(outcome);
                }
                let prompt = if repaired {
                    repair_prompt.as_str()
                } else {
                    base_prompt.as_str()
                };
                let reply = {
                    let drive = endpoints.drive(peer, prompt, &child);
                    tokio::pin!(drive);
                    loop {
                        // Biased, deadline first (see the notify loop): the hard
                        // deadline wins a tie with a ready drive result, so a reply
                        // that lands exactly at the boundary still times out.
                        tokio::select! {
                            biased;
                            () = &mut deadline, if !deadline_fired => {
                                deadline_fired = true;
                                child.cancel();
                            }
                            result = &mut drive => break result,
                        }
                    }
                };
                // Retain a produced envelope *before* terminal precedence, the budget
                // check, and parsing — so a malformed, stale, over-budget, or even a
                // deadline-crossing reply stays diagnosable in `raw`. A repair
                // replaces the same peer's raw. A `Cancelled`/session-stop/`Err` reply
                // produced no envelope, so none is recorded.
                if let Ok(TurnReply::Produced { envelope, .. }) = &reply {
                    raw[slot % 2] = Some(envelope.clone());
                }
                if let Some(outcome) = self.stop_precedence(deadline_fired) {
                    break 'slot Some(outcome);
                }
                let (envelope, cost) = match reply {
                    Err(error) => break 'slot Some(endpoint_outcome(error)),
                    Ok(TurnReply::Produced { envelope, cost }) => (envelope, cost),
                    Ok(TurnReply::Cancelled) => break 'slot Some(cancellation_anomaly()),
                    Ok(TurnReply::TimedOut) => break 'slot Some(PairOutcome::TimedOut),
                    Ok(TurnReply::BudgetExceeded) => break 'slot Some(PairOutcome::BudgetExceeded),
                    Ok(TurnReply::NoProgress) => break 'slot Some(PairOutcome::NoProgress),
                };

                // Per-slot budget: the primary and the repair spend against one
                // counter, and an overflow is a spent budget rather than a wrap.
                slot_spent = match slot_spent.checked_add(cost) {
                    Some(total) => total,
                    None => break 'slot Some(PairOutcome::BudgetExceeded),
                };
                if self.bounds.slot_token_budget > 0 && slot_spent > self.bounds.slot_token_budget {
                    break 'slot Some(PairOutcome::BudgetExceeded);
                }

                match self.step(peer, &envelope) {
                    StepResult::Converged(revision) => {
                        // Converging on the second peer's slot completes that round;
                        // converging on the first peer's leaves the round half done.
                        // A failed/timed-out second-peer slot never reaches here, so
                        // it is not miscounted.
                        if slot % 2 == 1 {
                            rounds += 1;
                        }
                        break 'slot Some(PairOutcome::Converged { revision });
                    }
                    StepResult::Progressed => break 'slot None,
                    StepResult::Fatal(detail) => {
                        break 'slot Some(PairOutcome::ProtocolError { detail })
                    }
                    StepResult::Repairable(detail) => {
                        if repaired {
                            break 'slot Some(PairOutcome::ProtocolError { detail });
                        }
                        repaired = true;
                        repair_prompt = render_repair(&detail);
                        // The same peer is about to spend its one repair drive; surface
                        // it now so observers see the repair, not a silent stall. The
                        // next slot or terminal publish clears it.
                        self.publish_repairing(peer, rounds);
                    }
                }
            };

            if let Some(outcome) = terminal {
                break outcome;
            }

            slot += 1;
            if slot % 2 == 0 {
                rounds += 1;
                if rounds >= self.bounds.max_rounds {
                    break PairOutcome::CapReached { rounds };
                }
            }
            // Nonterminal slot done: publish the peer scheduled next and the round
            // count so far.
            self.publish(Some(self.peers[slot % 2]), rounds);
        };

        // One final terminal snapshot: never a next peer, and the correct round
        // count (a CapReached break already incremented `rounds`).
        self.publish(None, rounds);
        outcome
    }

    /// Parse one envelope and apply it, classifying the result for the slot loop.
    fn step(&mut self, peer: SessionId, envelope: &str) -> StepResult {
        let action = match parse_action(envelope) {
            Ok(action) => action,
            Err(error) => return StepResult::Repairable(error.to_string()),
        };
        match self.state.apply(peer, action) {
            Ok(Transition::Converged { revision }) => StepResult::Converged(revision),
            Ok(Transition::CandidateInstalled { .. } | Transition::AgreementRecorded { .. }) => {
                StepResult::Progressed
            }
            Ok(Transition::StaleAgreement) => StepResult::Repairable("stale agreement".to_string()),
            Err(error) => StepResult::Fatal(error.to_string()),
        }
    }
}

/// Render the current candidate as the message a peer is handed before its turn:
/// its revision and digest — so the peer can accept it by naming them — and the
/// artifact itself. Original wording; the peer reads it as ordinary turn context.
fn render_delivery(candidate: &Candidate) -> String {
    format!(
        "The current proposal is revision {} (digest {}). To accept it exactly, \
agree to that revision and digest; otherwise revise it.\n\n{}",
        candidate.revision(),
        candidate.digest(),
        candidate.artifact(),
    )
}

/// The instruction handed to a peer whose previous reply could not be used: the
/// bounded reason, plus a reminder of the exact shape a reply must take. No new
/// proposal is delivered — the peer is answering the same delivery again. Original
/// wording.
fn render_repair(reason: &str) -> String {
    format!(
        "Your previous reply could not be used: {reason}. Reply with exactly one \
protocol-version-1 JSON envelope — a proposal, a revision, or an agreement. To \
agree, name the current proposal's revision and digest exactly as delivered."
    )
}

#[cfg(test)]
mod tests;
