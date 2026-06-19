//! The hard human-approval gate.
//!
//! Promotion of a proposed patch to the main branch requires an [`ApprovalToken`],
//! and a token authorizes exactly one patch by id. The token's only constructor,
//! [`ApprovalToken::approve`], is called by a host **only after** it has obtained
//! explicit human confirmation; the autonomous self-review path never constructs
//! one. So there is no code path from "observe → propose" to a write on the main
//! branch — the gate is structural (D002 / ADR-0034), not a prompt convention.

/// An explicit human approval authorizing one specific proposed patch to be
/// promoted onto the main branch. Without it, [`crate::ProposedPatch::promote`]
/// cannot be called — the signature requires it.
#[derive(Debug, Clone)]
pub struct ApprovalToken {
    patch_id: String,
    reviewer: String,
}

impl ApprovalToken {
    /// Mint approval for the patch with id `patch_id`, recording the human
    /// `reviewer`.
    ///
    /// Callers MUST have explicit human confirmation before calling this. It is
    /// intentionally the *only* way to obtain a token, so a promotion is always
    /// traceable to a deliberate human act; the agent loop never calls it.
    #[must_use]
    pub fn approve(patch_id: impl Into<String>, reviewer: impl Into<String>) -> Self {
        Self {
            patch_id: patch_id.into(),
            reviewer: reviewer.into(),
        }
    }

    /// Whether this token authorizes the patch with id `patch_id`.
    #[must_use]
    pub fn authorizes(&self, patch_id: &str) -> bool {
        self.patch_id == patch_id
    }

    /// The human reviewer recorded on the token.
    #[must_use]
    pub fn reviewer(&self) -> &str {
        &self.reviewer
    }

    /// The patch id this token authorizes.
    #[must_use]
    pub fn patch_id(&self) -> &str {
        &self.patch_id
    }
}
