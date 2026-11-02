//! Exclusive ownership of a runtime's session, and handing the session over to
//! another process (the workspace's background server).
//!
//! A runtime claims the session it runs (see `localpilot_store::SessionClaim`)
//! and keeps one admission gate for its whole life, shared with every handle it
//! gives out (reasoning effort, permission engine, steering queue). Every
//! execution and mutation consults it:
//!
//! - `Owned`: the runtime may run turns, tools and setters.
//! - `Preparing`: a hand-over froze the session; everything that executes or
//!   mutates is refused until the hand-over is released or aborted.
//! - `Released`: the session was handed over.
//! - `Lost`: the runtime could not claim its session, or a store write found it
//!   owned elsewhere; a running turn is cancelled.
//!
//! Only a successful new claim (resuming a session) makes a released or lost
//! runtime `Owned` again.
//!
//! A hand-over is idle-only. `prepare_transfer` holds the gate while it checks
//! idleness and captures the log tail and a settings manifest, then sets
//! `Preparing`, so no handle can change anything between the check and the
//! freeze. `release_transfer` holds the gate while it revalidates both and lets
//! the claim go (the store kills the write lease and publishes the released
//! record before dropping the lock). Release never runs `close()`: nothing the
//! next owner needs (scratch, background state) is cleared, and no
//! `SessionClosed` is written.

use std::fmt::Write as _;

pub use localpilot_core::admission::Admission;
use localpilot_core::{EventId, SessionId};
use localpilot_sandbox::Profile;
use localpilot_store::{Holder, SessionClaim, StoreError, TransferRecord};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::SessionRuntime;

/// The transfer-manifest format version this build writes and reads.
pub const TRANSFER_MANIFEST_VERSION: u32 = 1;

/// The largest encoded manifest accepted.
pub const TRANSFER_MANIFEST_MAX_BYTES: usize = 16 * 1024;

/// The settings that move with a session in a live hand-over. Everything else
/// is recomputed by the destination (context usage, tool and MCP registries,
/// hooks, system prompt). It carries no secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferManifest {
    pub v: u32,
    pub session: SessionId,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<localpilot_llm::ReasoningEffort>,
    pub permission_profile: String,
    /// The user allowlist in force with the profile.
    pub permission_allowlist: Vec<String>,
    pub trusted: bool,
    pub envelope_enabled: bool,
    pub image_support_override: Option<bool>,
    pub name: Option<String>,
}

impl TransferManifest {
    /// The manifest's stable digest (hex SHA-256 of its encoding).
    #[must_use]
    pub fn digest(&self) -> String {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        let mut hex = String::with_capacity(64);
        for byte in Sha256::digest(&bytes) {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    /// Encode for the wire, refusing an oversized manifest.
    ///
    /// # Errors
    /// [`TransferRefusal::ManifestTooLarge`] or a serialization error.
    pub fn encode(&self) -> Result<Vec<u8>, TransferRefusal> {
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > TRANSFER_MANIFEST_MAX_BYTES {
            return Err(TransferRefusal::ManifestTooLarge);
        }
        Ok(bytes)
    }

    /// Decode a received manifest, refusing an oversized one or an unknown
    /// version before anything is staged.
    ///
    /// # Errors
    /// [`TransferRefusal::ManifestTooLarge`],
    /// [`TransferRefusal::UnsupportedManifest`] or a parse error.
    pub fn decode(bytes: &[u8]) -> Result<Self, TransferRefusal> {
        if bytes.len() > TRANSFER_MANIFEST_MAX_BYTES {
            return Err(TransferRefusal::ManifestTooLarge);
        }
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        let version = value
            .get("v")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if version != u64::from(TRANSFER_MANIFEST_VERSION) {
            return Err(TransferRefusal::UnsupportedManifest(version));
        }
        Ok(serde_json::from_value(value)?)
    }
}

/// The permission profile's stable manifest name.
#[must_use]
pub fn profile_name(profile: Profile) -> &'static str {
    match profile {
        Profile::Default => "default",
        Profile::ReadOnly => "read-only",
        Profile::Relaxed => "relaxed",
        Profile::Bypass => "bypass",
        Profile::Unrestricted => "unrestricted",
    }
}

/// A session frozen for a hand-over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenTransfer {
    pub transfer: String,
    /// The log's last event when the session was frozen.
    pub tail: Option<EventId>,
    pub manifest: TransferManifest,
    pub digest: String,
}

/// Why a hand-over was refused or failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransferRefusal {
    #[error("a turn is running")]
    TurnRunning,
    #[error("steering input is still queued")]
    SteerQueued,
    #[error("background processes are running")]
    BackgroundJobs,
    #[error("an incognito session cannot be shared")]
    Incognito,
    #[error("this runtime does not own the session")]
    NotOwned,
    #[error("the session holds no ownership lock")]
    Unclaimed,
    #[error("the session changed while it was being handed over")]
    Changed,
    #[error("transfer {0} is not the one being prepared")]
    WrongTransfer(String),
    #[error("unsupported transfer manifest version {0}")]
    UnsupportedManifest(u64),
    #[error("the transfer manifest is too large")]
    ManifestTooLarge,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("transfer manifest: {0}")]
    Serde(#[from] serde_json::Error),
}

impl SessionRuntime {
    /// Claim `session` for this runtime, or say why it cannot be.
    pub(super) fn claim_for(&self, session: SessionId) -> Result<Option<SessionClaim>, StoreError> {
        self.store
            .claim_session(session, Holder::this_process(self.owner_kind))
    }

    /// Claim the runtime's freshly created session. A claim that cannot be
    /// taken leaves the runtime `Lost`: it runs nothing and writes nothing,
    /// rather than running a persistent session without exclusive ownership.
    pub(super) fn claim_fresh(&mut self) {
        match self.claim_for(self.session_id) {
            Ok(claim) => {
                self.claim = claim;
                self.ownership_error = None;
                self.admission.set(Admission::Owned);
            }
            Err(error) => {
                tracing::warn!(%error, "could not take ownership of the new session");
                self.claim = None;
                self.ownership_error = Some(error.to_string());
                self.admission.set(Admission::Lost);
            }
        }
    }

    /// The runtime lost its session (a store write found it owned elsewhere):
    /// nothing more runs, and a running turn is cancelled. A released runtime
    /// stays released.
    pub(super) fn lose_ownership(&mut self) {
        {
            let mut state = self.admission.lock();
            if !matches!(*state, Admission::Released { .. }) {
                *state = Admission::Lost;
            }
        }
        if let Some(turn) = &self.turn_cancel {
            turn.cancel();
        }
    }

    /// Log and report a mutation refused because this runtime does not own its
    /// session. `true` means refused.
    pub(super) fn refuse_unless_owned(&self, what: &str) -> bool {
        if self.admission.is_owned() {
            return false;
        }
        tracing::warn!("{what} refused: this runtime does not own its session");
        true
    }

    /// Set what kind of host runs this runtime. Recorded with the next claim,
    /// and shown to other processes that find the session busy.
    pub fn set_owner_kind(&mut self, kind: localpilot_store::HolderKind) {
        self.owner_kind = kind;
    }

    /// Whether this runtime may execute and mutate its session.
    #[must_use]
    pub fn admission(&self) -> Admission {
        self.admission.state()
    }

    /// Why the runtime could not claim its session, when it is `Lost` for that
    /// reason.
    #[must_use]
    pub fn ownership_error(&self) -> Option<&str> {
        self.ownership_error.as_deref()
    }

    /// Why this runtime cannot be handed over right now, if anything stops it.
    /// Never touches the admission gate (callers may hold it).
    fn idle_refusal(&self) -> Option<TransferRefusal> {
        if self.turn_in_flight {
            return Some(TransferRefusal::TurnRunning);
        }
        if self.config.incognito || self.store.is_ephemeral() {
            return Some(TransferRefusal::Incognito);
        }
        if !self.steer.is_empty() {
            return Some(TransferRefusal::SteerQueued);
        }
        if self.background.list().iter().any(|proc| proc.alive) {
            return Some(TransferRefusal::BackgroundJobs);
        }
        None
    }

    /// The settings that move with this session. Never touches the admission
    /// gate.
    #[must_use]
    pub fn transfer_manifest(&self) -> TransferManifest {
        let name = self.store.list_sessions().ok().and_then(|entries| {
            entries
                .into_iter()
                .find(|entry| entry.id == self.session_id)
                .and_then(|entry| entry.name)
        });
        let engine = self.engine.snapshot();
        TransferManifest {
            v: TRANSFER_MANIFEST_VERSION,
            session: self.session_id,
            provider: self.active_provider_id().to_string(),
            model: self.active_model().to_string(),
            reasoning_effort: self.reasoning_effort(),
            permission_profile: profile_name(engine.profile()).to_string(),
            permission_allowlist: engine.allowlist().to_vec(),
            trusted: self.config.trusted,
            envelope_enabled: self.envelope_enabled(),
            image_support_override: self.image_support_override,
            name,
        }
    }

    /// Freeze the session for hand-over `transfer`: under the admission gate,
    /// verify it is idle, capture the log tail and the manifest, and set
    /// `Preparing`, which refuses every execution and mutation (handles
    /// included) until [`SessionRuntime::release_transfer`] or
    /// [`SessionRuntime::abort_transfer`].
    ///
    /// # Errors
    /// A [`TransferRefusal`] naming what stops the hand-over; the runtime is
    /// unchanged.
    pub fn prepare_transfer(&mut self, transfer: &str) -> Result<FrozenTransfer, TransferRefusal> {
        let gate = self.admission.clone();
        let mut state = gate.lock();
        if !matches!(*state, Admission::Owned) {
            return Err(TransferRefusal::NotOwned);
        }
        if self.claim.is_none() {
            return Err(TransferRefusal::Unclaimed);
        }
        if let Some(refusal) = self.idle_refusal() {
            return Err(refusal);
        }
        let manifest = self.transfer_manifest();
        manifest.encode()?;
        let frozen = FrozenTransfer {
            transfer: transfer.to_string(),
            tail: self.last_event,
            digest: manifest.digest(),
            manifest,
        };
        *state = Admission::Preparing {
            transfer: transfer.to_string(),
        };
        Ok(frozen)
    }

    /// Abort hand-over `transfer` before it was released: the runtime owns its
    /// session again. Returns `false` when that transfer is not being prepared.
    pub fn abort_transfer(&mut self, transfer: &str) -> bool {
        let mut state = self.admission.lock();
        if *state
            == (Admission::Preparing {
                transfer: transfer.to_string(),
            })
        {
            *state = Admission::Owned;
            true
        } else {
            false
        }
    }

    /// Release the frozen session to the destination. Holding the admission
    /// gate, revalidates that nothing changed since
    /// [`SessionRuntime::prepare_transfer`] (any difference aborts back to
    /// `Owned`) and lets the claim go. On success the runtime is `Released` and
    /// no longer holds the session; on a failed record publish it is `Owned`
    /// again and still holds it.
    ///
    /// # Errors
    /// [`TransferRefusal::WrongTransfer`], [`TransferRefusal::Changed`] (aborted
    /// to `Owned`) or the store error of a failed publish (back to `Owned`).
    pub fn release_transfer(&mut self, frozen: &FrozenTransfer) -> Result<(), TransferRefusal> {
        let gate = self.admission.clone();
        let mut state = gate.lock();
        let preparing = Admission::Preparing {
            transfer: frozen.transfer.clone(),
        };
        if *state != preparing {
            return Err(TransferRefusal::WrongTransfer(frozen.transfer.clone()));
        }
        let unchanged = self.idle_refusal().is_none()
            && self.last_event == frozen.tail
            && self.transfer_manifest().digest() == frozen.digest;
        if !unchanged {
            *state = Admission::Owned;
            return Err(TransferRefusal::Changed);
        }
        let Some(claim) = self.claim.take() else {
            *state = Admission::Owned;
            return Err(TransferRefusal::Unclaimed);
        };
        let record = TransferRecord {
            id: frozen.transfer.clone(),
            state: localpilot_store::TransferState::Released,
            tail: frozen.tail,
            manifest_digest: frozen.digest.clone(),
        };
        match claim.release(record) {
            Ok(()) => {
                *state = Admission::Released {
                    transfer: frozen.transfer.clone(),
                };
                Ok(())
            }
            Err((claim, error)) => {
                self.claim = Some(claim);
                *state = Admission::Owned;
                Err(TransferRefusal::Store(error))
            }
        }
    }
}
