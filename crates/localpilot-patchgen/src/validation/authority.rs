//! Purpose-specific, memory-only grants. Durable consumption belongs to the
//! repository transaction; moving a Rust value alone is not replay protection.

use super::{
    ContentDigest, ProposalIdentity, ValidationError, ValidationEvidence, ValidationPlanIdentity,
};
use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TOKEN_LIFETIME: Duration = Duration::from_secs(600);

pub(super) fn unix_now() -> Result<u64, ValidationError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| ValidationError::Clock)
}

struct Grant {
    nonce: uuid::Uuid,
    binding: ContentDigest,
    reviewer: String,
    intent: String,
    issued_at: u64,
    issued_instant: Instant,
}

impl Grant {
    fn issue(
        binding: ContentDigest,
        reviewer: &str,
        intent: &str,
    ) -> Result<Self, ValidationError> {
        if reviewer.trim().is_empty() || intent.trim().is_empty() {
            return Err(ValidationError::Invalid(
                "explicit reviewer and intent required".into(),
            ));
        }
        Ok(Self {
            nonce: uuid::Uuid::new_v4(),
            binding,
            reviewer: reviewer.to_owned(),
            intent: intent.to_owned(),
            issued_at: unix_now()?,
            issued_instant: Instant::now(),
        })
    }

    fn verify(&self, binding: &ContentDigest, now: u64) -> Result<(), ValidationError> {
        if &self.binding != binding {
            return Err(ValidationError::Stale);
        }
        if now < self.issued_at {
            return Err(ValidationError::Clock);
        }
        if now - self.issued_at >= TOKEN_LIFETIME.as_secs()
            || self.issued_instant.elapsed() >= TOKEN_LIFETIME
        {
            return Err(ValidationError::Expired);
        }
        Ok(())
    }

    fn reservation(&self, purpose: super::AuthorityPurpose) -> super::Reservation {
        super::Reservation {
            purpose,
            nonce: self.nonce,
            binding: self.binding.clone(),
            reviewer: localpilot_config::redact::redact(&self.reviewer),
            intent: localpilot_config::redact::redact(&self.intent),
            issued_at: self.issued_at,
        }
    }
}

impl fmt::Debug for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Grant { authority: [redacted] }")
    }
}

pub(super) fn environment_digest(plan: &ValidationPlanIdentity) -> ContentDigest {
    let mut hash = super::CanonicalHash::new("localpilot-private-execution-environment-v1");
    for name in &plan.environment_names {
        hash.field(name.as_bytes());
        match std::env::var_os(name) {
            None => hash.field(b"absent"),
            Some(value) => {
                hash.field(b"present");
                // Encode raw platform bytes; never a lossy UTF8 conversion. The
                // digest remains private to a token and is not an audit field.
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStrExt;
                    hash.field(value.as_os_str().as_bytes());
                }
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStrExt;
                    let bytes: Vec<u8> = value.encode_wide().flat_map(u16::to_be_bytes).collect();
                    hash.field(&bytes);
                }
            }
        }
    }
    hash.finish()
}

pub(super) fn execution_context(
    plan: &ValidationPlanIdentity,
) -> Result<ContentDigest, ValidationError> {
    use std::io::Read;
    let mut hash = super::CanonicalHash::new("localpilot-private-execution-context-v1");
    hash.field(environment_digest(plan).as_str().as_bytes());
    for path in &plan.private_config_paths {
        hash.field(path.as_bytes());
        match std::fs::File::open(path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|source| ValidationError::Io {
                        path: path.into(),
                        source,
                    })?;
                if bytes.len() > 1024 * 1024 {
                    return Err(ValidationError::Invalid(
                        "private configuration exceeds1MiB".into(),
                    ));
                }
                hash.field(b"present");
                hash.field(&bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => hash.field(b"absent"),
            Err(source) => {
                return Err(ValidationError::Io {
                    path: path.into(),
                    source,
                })
            }
        }
    }
    Ok(hash.finish())
}

fn evaluation_binding(
    proposal: &ProposalIdentity,
    plan: &ValidationPlanIdentity,
) -> Result<(ContentDigest, ContentDigest), ValidationError> {
    let environment = execution_context(plan)?;
    let binding = proposal.execution_identity(plan)?;
    Ok((binding, environment))
}

/// Execution consent, never promotion or publication consent. No Clone or
/// Deserialize implementation exists; a persisted run cannot recreate it.
///
/// ```compile_fail
/// use localpilot_patchgen::validation::{EvaluationToken, PatchApprovalToken};
/// fn substitute(execution: EvaluationToken) -> PatchApprovalToken { execution }
/// ```
/// ```compile_fail
/// use localpilot_patchgen::validation::{EvaluationToken, OutwardApprovalToken};
/// fn substitute(execution: EvaluationToken) -> OutwardApprovalToken { execution }
/// ```
/// ```compile_fail
/// use localpilot_patchgen::validation::EvaluationToken;
/// fn restore(json: &str) -> EvaluationToken { serde_json::from_str(json).unwrap() }
/// ```
pub struct EvaluationToken {
    grant: Grant,
    environment: ContentDigest,
}

impl EvaluationToken {
    pub(super) fn environment_identity(&self) -> ContentDigest {
        self.environment.clone()
    }
    /// Issue only after the host has displayed this exact proposal and plan and
    /// obtained explicit execution consent. This API performs no execution.
    ///
    /// # Errors
    /// Invalid plan, missing reviewer/intent or invalid system time.
    pub fn approve(
        proposal: &ProposalIdentity,
        plan: &ValidationPlanIdentity,
        reviewer: &str,
        intent: &str,
    ) -> Result<Self, ValidationError> {
        let (binding, environment) = evaluation_binding(proposal, plan)?;
        Ok(Self {
            grant: Grant::issue(binding, reviewer, intent)?,
            environment,
        })
    }

    pub(super) fn reservation(
        &self,
        proposal: &ProposalIdentity,
        plan: &ValidationPlanIdentity,
    ) -> Result<super::Reservation, ValidationError> {
        let (binding, environment) = evaluation_binding(proposal, plan)?;
        self.grant.verify(&binding, unix_now()?)?;
        if self.environment != environment {
            return Err(ValidationError::Stale);
        }
        Ok(self.grant.reservation(super::AuthorityPurpose::Evaluation))
    }
}

// Do not expose even a low-entropy environment value digest through Debug.
impl fmt::Debug for EvaluationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("execution consent [redacted]")
    }
}

fn patch_binding(
    proposal: &ProposalIdentity,
    plan: &ValidationPlanIdentity,
    receipt: &ValidationEvidence,
    now: u64,
) -> Result<ContentDigest, ValidationError> {
    receipt.verify_current(proposal, plan, now)?;
    Ok(ContentDigest::hash(
        "localpilot-patch-promotion-consent-v1",
        &[
            proposal.digest().as_str().as_bytes(),
            plan.digest()?.as_str().as_bytes(),
            receipt.run_id.as_bytes(),
            &receipt.started_at.to_be_bytes(),
            &receipt.finished_at.to_be_bytes(),
            receipt.digest()?.as_str().as_bytes(),
        ],
    ))
}

/// Consent to promote an exact already-validated patch. It cannot authorize
/// execution, retries or outward publication.
///
/// ```compile_fail
/// use localpilot_patchgen::validation::{PatchApprovalToken, OutwardApprovalToken};
/// fn substitute(patch: PatchApprovalToken) -> OutwardApprovalToken { patch }
/// ```
/// ```compile_fail
/// use localpilot_patchgen::validation::{PatchApprovalToken, EvaluationToken};
/// fn substitute(patch: PatchApprovalToken) -> EvaluationToken { patch }
/// ```
#[derive(Debug)]
pub struct PatchApprovalToken(Grant);

impl PatchApprovalToken {
    /// Issue only after explicit human promotion consent for the current receipt.
    ///
    /// # Errors
    /// Rejects nonpassing, expired or stale evidence and missing human identity.
    pub fn approve(
        proposal: &ProposalIdentity,
        plan: &ValidationPlanIdentity,
        receipt: &ValidationEvidence,
        reviewer: &str,
        intent: &str,
    ) -> Result<Self, ValidationError> {
        let binding = patch_binding(proposal, plan, receipt, unix_now()?)?;
        Ok(Self(Grant::issue(binding, reviewer, intent)?))
    }

    pub(super) fn reservation(
        &self,
        proposal: &ProposalIdentity,
        plan: &ValidationPlanIdentity,
        receipt: &ValidationEvidence,
    ) -> Result<super::Reservation, ValidationError> {
        let now = unix_now()?;
        self.0
            .verify(&patch_binding(proposal, plan, receipt, now)?, now)?;
        Ok(self.0.reservation(super::AuthorityPurpose::PatchPromotion))
    }
}

fn outward_binding(
    repository: &ContentDigest,
    draft: &crate::OutwardDraft,
    account: &str,
) -> Result<ContentDigest, ValidationError> {
    if draft.schema != crate::OUTWARD_SCHEMA || account.trim().is_empty() {
        return Err(ValidationError::Invalid(
            "resolved publication identity required".into(),
        ));
    }
    let mut hash = super::CanonicalHash::new("localpilot-outward-publication-consent-v1");
    hash.field(repository.as_str().as_bytes());
    hash.field(draft.id.as_bytes());
    hash.field(draft.target_repo.as_bytes());
    hash.field(account.as_bytes());
    let args = draft.publish_argv();
    hash.number(args.len() as u64);
    for arg in args {
        hash.field(arg.as_bytes());
    }
    Ok(hash.finish())
}

/// Publication-only authority, bound to exact draft argv and resolved account.
/// A draft id equal to a patch id grants no patch authority.
///
/// ```compile_fail
/// use localpilot_patchgen::validation::{OutwardApprovalToken, EvaluationToken};
/// fn substitute(outward: OutwardApprovalToken) -> EvaluationToken { outward }
/// ```
/// ```compile_fail
/// use localpilot_patchgen::validation::{OutwardApprovalToken, PatchApprovalToken};
/// fn substitute(outward: OutwardApprovalToken) -> PatchApprovalToken { outward }
/// ```
#[derive(Debug)]
pub struct OutwardApprovalToken(Grant);

impl OutwardApprovalToken {
    /// Issue only after explicit confirmation of draft, target, argv and account.
    /// Does not perform a network operation or require patch-shaped evidence.
    ///
    /// # Errors
    /// Missing resolved identity, invalid schema, reviewer, intent or clock.
    pub fn approve(
        repository: &ContentDigest,
        draft: &crate::OutwardDraft,
        account: &str,
        reviewer: &str,
        intent: &str,
    ) -> Result<Self, ValidationError> {
        Ok(Self(Grant::issue(
            outward_binding(repository, draft, account)?,
            reviewer,
            intent,
        )?))
    }

    pub(super) fn reservation(
        &self,
        repository: &ContentDigest,
        draft: &crate::OutwardDraft,
        account: &str,
    ) -> Result<super::Reservation, ValidationError> {
        self.0
            .verify(&outward_binding(repository, draft, account)?, unix_now()?)?;
        Ok(self
            .0
            .reservation(super::AuthorityPurpose::OutwardPublication))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_rejects_expiry_and_backwards_clock() {
        let digest = ContentDigest::hash("fixture", &[b"binding"]);
        let grant = Grant::issue(digest.clone(), "reviewer", "execute fixed plan").unwrap();
        assert!(matches!(
            grant.verify(&digest, grant.issued_at - 1),
            Err(ValidationError::Clock)
        ));
        assert!(matches!(
            grant.verify(&digest, grant.issued_at + 600),
            Err(ValidationError::Expired)
        ));
        assert!(grant.verify(&digest, grant.issued_at + 599).is_ok());
        assert!(matches!(
            grant.verify(&ContentDigest::hash("other", &[]), grant.issued_at),
            Err(ValidationError::Stale)
        ));
    }
}
