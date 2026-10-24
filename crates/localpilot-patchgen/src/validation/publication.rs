//! Purpose-specific publication consent with the common lock retained through
//! the host's observed external effect and durable local event checkpoint.

use super::{OutwardApprovalToken, RepositoryTransaction, ValidationError};
use crate::{OutwardDraft, PublishPlan};

/// A consumed publication grant, retaining its common-repository transaction.
/// No patch evidence or patch promotion capability can construct this owner.
///
/// ```compile_fail
/// use localpilot_patchgen::validation::PendingPublication;
/// fn restore(json: &str) -> PendingPublication<'_> { serde_json::from_str(json).unwrap() }
/// ```
pub struct PendingPublication<'repo> {
    transaction: RepositoryTransaction<'repo>,
    nonce: uuid::Uuid,
    plan: PublishPlan,
}

impl std::fmt::Debug for PendingPublication<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingPublication { authority: [redacted] }")
    }
}

impl PendingPublication<'_> {
    /// Exact command data approved for this pending operation.
    #[must_use]
    pub fn plan(&self) -> &PublishPlan {
        &self.plan
    }

    /// Complete only after the host observed publication and durably recorded
    /// its local outcome. An error or drop leaves explicit inspection pending.
    ///
    /// # Errors
    /// Invalid outcome or failed journal persistence.
    pub fn finish(mut self, outcome: &str) -> Result<(), ValidationError> {
        self.transaction.finish(self.nonce, outcome)
    }
}

impl<'repo> RepositoryTransaction<'repo> {
    /// Consume a fresh publication grant before an effect, retaining the guard.
    ///
    /// # Errors
    /// Wrong draft/account/repository, expiry, replay, recovery or storage failure.
    pub fn prepare_publication(
        mut self,
        draft: &OutwardDraft,
        token: OutwardApprovalToken,
        account: &str,
    ) -> Result<PendingPublication<'repo>, ValidationError> {
        self.require_idle()?;
        let reservation = token.reservation(&self.repository_identity(), draft, account)?;
        let plan = PublishPlan {
            program: "gh".into(),
            args: draft.publish_argv(),
            target_repo: draft.target_repo.clone(),
            reviewer: reservation.reviewer,
        };
        let nonce = self.reserve_outward(token, draft, account)?;
        Ok(PendingPublication {
            transaction: self,
            nonce,
            plan,
        })
    }
}
