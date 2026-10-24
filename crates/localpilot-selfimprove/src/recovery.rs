//! Explicit inspection and acknowledgement of unfinished authority. Never retry
//! unknown checks/publication or change source refs while recovering metadata.

use crate::orchestrator::{read_loop_state, write_loop_state};
use crate::promotion::loop_state_digest;
use crate::{LoopError, LoopState, Stage};
use localpilot_patchgen::validation::{
    AuthorityEvent, AuthorityPurpose, CallerCheckpoint, ContentDigest, PromotionObservation,
    PromotionPosition, RepositoryAuthority, RepositoryTransaction, ValidationError,
};
use serde::Serialize;
use std::path::Path;

/// Inspectable current data, never reconstructed execution or promotion consent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoveryReview {
    /// Common repository identity used for this inspection.
    pub repository: ContentDigest,
    /// Exact unfinished intent, with no private environment comparison state.
    pub event: AuthorityEvent,
    /// Current application state, if present.
    pub loop_state: Option<LoopState>,
    /// Source observation only for a promotion intent; publication needs no tree.
    pub promotion: Option<PromotionObservation>,
}

impl RecoveryReview {
    /// Canonical current review identity. Any event/source/state change requires
    /// a new explicit acknowledgement, even when its nonce is unchanged.
    ///
    /// # Errors
    /// Unsupported review data cannot be encoded.
    pub fn digest(&self) -> Result<ContentDigest, LoopError> {
        let bytes = serde_json::to_vec(self)
            .map_err(|_| LoopError::Serde("recovery review cannot be encoded".into()))?;
        Ok(ContentDigest::for_bytes(&bytes))
    }

    /// Render only public audit/context data, never private environment values.
    ///
    /// # Errors
    /// Unsupported review data cannot be encoded.
    pub fn render(&self) -> Result<String, LoopError> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|_| LoopError::Serde("recovery review cannot be encoded".into()))?;
        Ok(localpilot_config::redact::redact(&format!("Unfinished authority inspection:\n{json}\nRecovery review identity: {}\nInspect remaining processes, host effects and any remote publication before acknowledging. Recovery never runs checks, promotes source, publishes, or certifies validation. A known source promotion may reconcile only its exact journaled loop-state checkpoint.\n", self.digest()?.as_str())))
    }
}

/// List unfinished public audit events without executing or recovering them.
///
/// # Errors
/// Busy, missing/corrupt authority or invalid repository metadata.
pub fn pending_authority(root: &Path) -> Result<Vec<AuthorityEvent>, LoopError> {
    let mut authority = RepositoryAuthority::open(root)?;
    let transaction = authority.try_transaction()?;
    Ok(transaction
        .events()
        .filter(|event| !event.completed)
        .cloned()
        .collect())
}

fn inspect(
    root: &Path,
    nonce: uuid::Uuid,
    transaction: &RepositoryTransaction<'_>,
) -> Result<RecoveryReview, LoopError> {
    let event = transaction
        .events()
        .find(|event| event.reservation.nonce == nonce)
        .ok_or(ValidationError::RecoveryRequired)?
        .clone();
    if event.completed {
        return Err(ValidationError::Consumed.into());
    }
    let promotion = if event.reservation.purpose == AuthorityPurpose::PatchPromotion {
        let goal = event
            .promotion
            .as_ref()
            .ok_or(ValidationError::RecoveryRequired)?;
        Some(transaction.inspect_promotion_goal(root, goal)?)
    } else {
        None
    };
    Ok(RecoveryReview {
        repository: transaction.repository_identity(),
        event,
        loop_state: read_loop_state(root)?,
        promotion,
    })
}

/// Inspect an unfinished event and current context without consuming authority.
///
/// # Errors
/// Busy, unknown/completed nonce, missing goal or unavailable metadata.
pub fn recovery_review(root: &Path, nonce: uuid::Uuid) -> Result<RecoveryReview, LoopError> {
    let mut authority = RepositoryAuthority::open(root)?;
    let transaction = authority.try_transaction()?;
    inspect(root, nonce, &transaction)
}

/// Acknowledge only after a host explicitly confirms unknown-effects inspection
/// for this exact displayed review. A generic acknowledgement cannot certify a
/// validation receipt. Source refs never change and no effect is replayed.
///
/// # Errors
/// Stale review, missing acknowledgement identity/note, unknown source/state or
/// failed durable checkpoint. Any failure leaves the unfinished intent pending.
pub fn acknowledge_recovery(
    root: &Path,
    review: &RecoveryReview,
    reviewer: &str,
    note: &str,
) -> Result<(), LoopError> {
    if reviewer.trim().is_empty()
        || reviewer.len() > 256
        || note.trim().is_empty()
        || note.len() > 1024
        || reviewer.contains('\0')
        || note.contains('\0')
    {
        return Err(ValidationError::Invalid(
            "recovery requires a bounded reviewer and explicit inspection note".into(),
        )
        .into());
    }
    let note = localpilot_config::redact::redact(&format!("Explicit recovery inspection by {reviewer}: {note}; no effect replayed or validation certified"));
    if note.len() > 2048 {
        return Err(ValidationError::Invalid(
            "redacted recovery outcome exceeds the audit bound".into(),
        )
        .into());
    }
    let mut authority = RepositoryAuthority::open(root)?;
    let mut transaction = authority.try_transaction()?;
    let current = inspect(root, review.event.reservation.nonce, &transaction)?;
    if current != *review || current.digest()? != review.digest()? {
        return Err(ValidationError::Stale.into());
    }
    if current.event.reservation.purpose == AuthorityPurpose::PatchPromotion {
        reconcile_source_checkpoint(root, &current)?;
    }
    transaction.finish(current.event.reservation.nonce, &note)?;
    Ok(())
}

fn reconcile_source_checkpoint(root: &Path, review: &RecoveryReview) -> Result<(), LoopError> {
    let goal = review
        .event
        .promotion
        .as_ref()
        .ok_or(ValidationError::RecoveryRequired)?;
    let observation = review
        .promotion
        .as_ref()
        .ok_or(ValidationError::RecoveryRequired)?;
    let position = observation.position(goal);
    if position == PromotionPosition::Diverged {
        return Err(ValidationError::RecoveryRequired.into());
    }
    match &goal.caller_checkpoint {
        CallerCheckpoint::Unspecified => {
            return Err(ValidationError::Invalid(
                "older promotion has no recorded application-state proof; automatic reconciliation is unavailable and manual inspection is required".into(),
            ).into());
        }
        CallerCheckpoint::None => {
            if review
                .loop_state
                .as_ref()
                .is_some_and(|state| state.proposal_id.as_deref() == Some(&goal.proposal_id))
            {
                return Err(ValidationError::Stale.into());
            }
        }
        CallerCheckpoint::Required { before, after } => {
            let state = review
                .loop_state
                .as_ref()
                .ok_or(ValidationError::RecoveryRequired)?;
            let current = loop_state_digest(state)?;
            if position == PromotionPosition::NotApplied {
                if current != *before {
                    return Err(ValidationError::Stale.into());
                }
            } else if current == *after {
                // Caller state already persisted; only retire the pending intent.
            } else if current == *before
                && state.stage == Stage::Proposed
                && state.proposal_id.as_deref() == Some(&goal.proposal_id)
            {
                let mut approved = state.clone();
                approved.stage = Stage::Approved;
                approved.reviewer = Some(review.event.reservation.reviewer.clone());
                if loop_state_digest(&approved)? != *after {
                    return Err(ValidationError::Stale.into());
                }
                write_loop_state(root, &approved)?;
            } else {
                return Err(ValidationError::Stale.into());
            }
        }
    }
    Ok(())
}
