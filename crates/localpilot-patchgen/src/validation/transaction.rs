//! Stable Git-common-dir advisory lock and durable nonce reservations. This is
//! a process-crash journal, not a multi-file/Git atomic transaction or sandbox.

use super::{
    ContentDigest, EvaluationToken, OutwardApprovalToken, PatchApprovalToken, ProposalIdentity,
    ValidationError, ValidationEvidence, ValidationPlanIdentity,
};
use fd_lock::{RwLock, RwLockWriteGuard};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

const SCHEMA: &str = "localpilot-authority-journal-v1";
const MAX_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;

/// Purpose recorded in the consumption journal, never inferred from an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityPurpose {
    /// Execute a fixed plan in a fixed proposal.
    Evaluation,
    /// Promote already-validated content.
    PatchPromotion,
    /// Publish an exact outward draft.
    OutwardPublication,
}

/// Audit data, not a serializable execution or promotion token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    /// Intent's sole authority purpose.
    pub purpose: AuthorityPurpose,
    /// Identifier permanently retired before effects begin.
    pub nonce: uuid::Uuid,
    /// Public content/plan identity; no private environment value digest.
    pub binding: ContentDigest,
    /// Human who confirmed the action.
    pub reviewer: String,
    /// Explicit human intent.
    pub intent: String,
    /// Unix seconds when consent was issued.
    pub issued_at: u64,
}

/// Exact ref transition used to inspect an interrupted promotion without replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionIntent {
    /// Exact proposal whose source was approved.
    pub proposal_id: String,
    /// Branch reference expected to advance.
    pub target_ref: String,
    /// Required value before the effect.
    pub base_commit: String,
    /// Only permitted value after the effect.
    pub proposed_commit: String,
    /// Locally completed validation that qualified the proposal.
    pub validation_run: uuid::Uuid,
}

/// Observations committed by the execution coordinator, not reconstructed from
/// a mutable proposal receipt or a generic recovery/outcome note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationCompletion {
    /// Exact complete redacted receipt observed by the coordinator.
    pub evidence: ContentDigest,
    /// Its terminal state; generic completion is not a passing result.
    pub state: super::ValidationState,
}

/// A durable intent, retained after completion or interruption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityEvent {
    /// The consumed intent (not bearer authority).
    pub reservation: Reservation,
    /// Whether the effect's actual terminal outcome was observed and recorded.
    pub completed: bool,
    /// Bounded human-readable recovery/outcome note.
    pub outcome: Option<String>,
    /// Exact old/new ref goal, durable before a promotion effect. Older records
    /// may lack this data; they require manual inspection, never guessed replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion: Option<PromotionIntent>,
    /// Missing in historical/generic recovery records; such records cannot
    /// qualify receipt data as passing validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<ValidationCompletion>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema: String,
    repository_id: uuid::Uuid,
    common_dir: String,
    events: BTreeMap<uuid::Uuid, AuthorityEvent>,
    // Private comparison state, excluded from public audit events and receipts.
    // No raw values are stored. Never export/debug this journal as an audit log.
    environments: BTreeMap<uuid::Uuid, ContentDigest>,
}

/// File-lock owner. Its guard can be retained across execution; the stable lock
/// file is never replaced or unlinked. All linked worktrees resolve one owner.
pub struct RepositoryAuthority {
    lock: RwLock<File>,
    path: PathBuf,
    common_dir: String,
    initialize: bool,
}

impl RepositoryAuthority {
    /// Resolve the repository's real Git common directory and open its authority
    /// lock. Never discovers identity from a remote URL or another user's store.
    ///
    /// # Errors
    /// Invalid Git roots, non-UTF8 locations, filesystem failure or symlinked
    /// authority files. Missing state in an existing store requires recovery.
    pub fn open(repo_root: &Path) -> Result<Self, ValidationError> {
        let common = crate::git::git(repo_root, &["rev-parse", "--git-common-dir"])
            .map_err(|e| ValidationError::Invalid(e.to_string()))?;
        let common = fs::canonicalize(repo_root.join(common.trim()))
            .map_err(|source| io(repo_root, source))?;
        let common_dir = common
            .to_str()
            .ok_or_else(|| ValidationError::Invalid("non-UTF8 Git common directory".into()))?
            .to_owned();
        let authority = common.join("localpilot-authority");
        let initialize = match fs::create_dir(&authority) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(io(&authority, error)),
        };
        regular_directory(&authority)?;
        let lock_path = authority.join("authority.lock");
        if lock_path.exists() {
            regular_file(&lock_path)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| io(&lock_path, source))?;
        regular_file(&lock_path)?;
        Ok(Self {
            lock: RwLock::new(file),
            path: authority.join("journal.json"),
            common_dir,
            initialize,
        })
    }

    /// Acquire without waiting. Callers may retry within their displayed total
    /// deadline, never block indefinitely while another run owns the repository.
    ///
    /// # Errors
    /// Busy, malformed/missing journal, moved repository, or I/O failure.
    pub fn try_transaction(&mut self) -> Result<RepositoryTransaction<'_>, ValidationError> {
        let guard = self.lock.try_write().map_err(|source| {
            if source.kind() == std::io::ErrorKind::WouldBlock {
                ValidationError::Busy
            } else {
                io(&self.path, source)
            }
        })?;
        let journal = if self.path.exists() {
            regular_file(&self.path)?;
            read_journal(&self.path)?
        } else if self.initialize {
            let journal = Journal {
                schema: SCHEMA.into(),
                repository_id: uuid::Uuid::new_v4(),
                common_dir: self.common_dir.clone(),
                events: BTreeMap::new(),
                environments: BTreeMap::new(),
            };
            persist(&self.path, &journal)?;
            journal
        } else {
            return Err(ValidationError::RecoveryRequired);
        };
        self.initialize = false;
        if journal.schema != SCHEMA
            || journal.repository_id.is_nil()
            || journal.common_dir != self.common_dir
            || journal.events.iter().any(|(nonce, event)| {
                nonce.is_nil()
                    || *nonce != event.reservation.nonce
                    || event.reservation.reviewer.trim().is_empty()
                    || event.reservation.intent.trim().is_empty()
                    || event.completed != event.outcome.is_some()
                    || (event.reservation.purpose == AuthorityPurpose::Evaluation)
                        != journal.environments.contains_key(nonce)
            })
            || journal
                .environments
                .keys()
                .any(|nonce| !journal.events.contains_key(nonce))
        {
            return Err(ValidationError::RecoveryRequired);
        }
        Ok(RepositoryTransaction {
            _guard: guard,
            path: self.path.clone(),
            journal,
            poisoned: false,
        })
    }
}

/// Exclusive repository transaction. It cannot be cloned, serialized or
/// constructed by callers; keeping it alive keeps the OS advisory lock alive.
pub struct RepositoryTransaction<'a> {
    _guard: RwLockWriteGuard<'a, File>,
    path: PathBuf,
    journal: Journal,
    // If durability failed, the in-memory state must never authorize another
    // effect. Reopen and inspect rather than guessing whether rename succeeded.
    poisoned: bool,
}

impl RepositoryTransaction<'_> {
    /// Recheck the consumed run's private host configuration/environment without
    /// exposing values or comparison digests in public evidence.
    ///
    /// # Errors
    /// Missing/nonexecution intent, changed context or invalid plan/configuration.
    pub fn verify_execution_context(
        &self,
        run: uuid::Uuid,
        plan: &ValidationPlanIdentity,
    ) -> Result<(), ValidationError> {
        let event = self
            .journal
            .events
            .get(&run)
            .ok_or(ValidationError::RecoveryRequired)?;
        if event.reservation.purpose != AuthorityPurpose::Evaluation
            || self.journal.environments.get(&run)
                != Some(&super::authority::execution_context(plan)?)
        {
            return Err(ValidationError::Stale);
        }
        Ok(())
    }

    pub(crate) fn verify_evaluation_record(
        &self,
        evidence: &ValidationEvidence,
    ) -> Result<(), ValidationError> {
        let event = self
            .journal
            .events
            .get(&evidence.run_id)
            .ok_or(ValidationError::RecoveryRequired)?;
        if event.completed
            || event.reservation.purpose != AuthorityPurpose::Evaluation
            || event.reservation.binding
                != ContentDigest::hash(
                    "localpilot-evaluation-consent-v1",
                    &[
                        evidence.proposal.as_str().as_bytes(),
                        evidence.plan.as_str().as_bytes(),
                    ],
                )
            || evidence.approved_plan.digest()? != evidence.plan
        {
            return Err(ValidationError::Stale);
        }
        Ok(())
    }
    pub(crate) fn ensure_repository(&self, root: &Path) -> Result<(), ValidationError> {
        let common = crate::git::git(root, &["rev-parse", "--git-common-dir"])?;
        let common =
            fs::canonicalize(root.join(common.trim())).map_err(|source| io(root, source))?;
        if common.to_str() != Some(self.journal.common_dir.as_str()) {
            return Err(ValidationError::Stale);
        }
        Ok(())
    }
    /// Repository identity includes UUID and canonical location. A moved or
    /// independently cloned repository does not inherit outstanding authority.
    #[must_use]
    pub fn repository_identity(&self) -> ContentDigest {
        ContentDigest::hash(
            "localpilot-repository-v1",
            &[
                self.journal.repository_id.as_bytes(),
                self.journal.common_dir.as_bytes(),
            ],
        )
    }

    /// Retained audit and recovery records. No method here turns them into tokens.
    pub fn events(&self) -> impl Iterator<Item = &AuthorityEvent> {
        self.journal.events.values()
    }

    /// Refuse a new effect or approval review while any earlier intent requires
    /// inspection. Read-only recovery inspection can still enumerate events.
    ///
    /// # Errors
    /// Interrupted intent or an uncertain write in this transaction.
    pub fn require_idle(&self) -> Result<(), ValidationError> {
        if self.poisoned || self.journal.events.values().any(|event| !event.completed) {
            return Err(ValidationError::RecoveryRequired);
        }
        Ok(())
    }

    /// Consume execution authority before spawning. Keep this transaction guard
    /// alive for the run; another process must not mutate authority concurrently.
    ///
    /// # Errors
    /// Expired/stale token, different repository, replay, pending recovery or I/O.
    pub fn reserve_evaluation(
        &mut self,
        token: EvaluationToken,
        proposal: &ProposalIdentity,
        plan: &ValidationPlanIdentity,
    ) -> Result<uuid::Uuid, ValidationError> {
        self.check_repository(proposal)?;
        let reservation = token.reservation(proposal, plan)?;
        self.reserve_with_environment(reservation, Some(token.environment_identity()), None)
    }

    /// Retire promotion authority before Git effects. This is not itself the
    /// validation capability or the operation that changes a Git ref.
    ///
    /// # Errors
    /// Rejects stale/expired/partial evidence, replay, recovery and store failures.
    pub fn reserve_promotion(
        &mut self,
        token: PatchApprovalToken,
        proposal: &ProposalIdentity,
        plan: &ValidationPlanIdentity,
        receipt: &ValidationEvidence,
    ) -> Result<uuid::Uuid, ValidationError> {
        self.check_repository(proposal)?;
        let reservation = token.reservation(proposal, plan, receipt)?;
        self.verify_completed_evaluation(proposal, plan, receipt)?;
        self.reserve_with_environment(
            reservation,
            None,
            Some(PromotionIntent {
                proposal_id: proposal.proposal_id.clone(),
                target_ref: proposal.target_ref.clone(),
                base_commit: proposal.base_commit.clone(),
                proposed_commit: proposal.proposed_commit.clone(),
                validation_run: receipt.run_id,
            }),
        )
    }

    pub(super) fn verify_completed_evaluation(
        &self,
        proposal: &ProposalIdentity,
        plan: &ValidationPlanIdentity,
        receipt: &ValidationEvidence,
    ) -> Result<(), ValidationError> {
        self.check_repository(proposal)?;
        if self.journal.events.values().any(|event| !event.completed) {
            return Err(ValidationError::RecoveryRequired);
        }
        let evaluation = self
            .journal
            .events
            .get(&receipt.run_id)
            .ok_or(ValidationError::RecoveryRequired)?;
        let expected_evaluation_binding = proposal.execution_identity(plan)?;
        if !evaluation.completed {
            return Err(ValidationError::RecoveryRequired);
        }
        let completion = evaluation
            .validation
            .as_ref()
            .ok_or(ValidationError::NotValidated(
                super::ValidationState::Invalid,
            ))?;
        if completion.state != super::ValidationState::Passed {
            return Err(ValidationError::NotValidated(completion.state));
        }
        if completion.evidence != receipt.digest()? {
            return Err(ValidationError::Stale);
        }
        if evaluation.reservation.purpose != AuthorityPurpose::Evaluation
            || evaluation.reservation.binding != expected_evaluation_binding
            || self.journal.environments.get(&receipt.run_id)
                != Some(&super::authority::execution_context(plan)?)
        {
            return Err(ValidationError::Stale);
        }
        Ok(())
    }

    /// Inspect saved passing evidence against the locally committed observation
    /// digest. The host supplies freshly resolved proposal/plan identities under
    /// this guard. No token is minted and no effect or state transition occurs.
    ///
    /// # Errors
    /// Partial, expired, changed, failed or recovery-pending observations.
    pub fn verify_validation(
        &self,
        proposal: &ProposalIdentity,
        plan: &ValidationPlanIdentity,
        receipt: &ValidationEvidence,
    ) -> Result<(), ValidationError> {
        receipt.verify_current(proposal, plan, super::authority::unix_now()?)?;
        self.verify_completed_evaluation(proposal, plan, receipt)
    }

    /// Retire publication authority independently of patch validation.
    ///
    /// # Errors
    /// Changed draft/account, expiry, replay, pending recovery or storage failure.
    pub fn reserve_outward(
        &mut self,
        token: OutwardApprovalToken,
        draft: &crate::OutwardDraft,
        account: &str,
    ) -> Result<uuid::Uuid, ValidationError> {
        self.reserve(token.reservation(&self.repository_identity(), draft, account)?)
    }

    fn check_repository(&self, proposal: &ProposalIdentity) -> Result<(), ValidationError> {
        if proposal.repository != self.repository_identity() {
            return Err(ValidationError::Stale);
        }
        Ok(())
    }

    fn reserve(&mut self, reservation: Reservation) -> Result<uuid::Uuid, ValidationError> {
        self.reserve_with_environment(reservation, None, None)
    }

    fn reserve_with_environment(
        &mut self,
        reservation: Reservation,
        environment: Option<ContentDigest>,
        promotion: Option<PromotionIntent>,
    ) -> Result<uuid::Uuid, ValidationError> {
        if self.poisoned {
            return Err(ValidationError::RecoveryRequired);
        }
        let nonce = reservation.nonce;
        if self.journal.events.contains_key(&nonce) {
            return Err(ValidationError::Consumed);
        }
        if self.journal.events.values().any(|event| !event.completed) {
            return Err(ValidationError::RecoveryRequired);
        }
        if (reservation.purpose == AuthorityPurpose::Evaluation) != environment.is_some() {
            return Err(ValidationError::Invalid(
                "execution environment comparison required".into(),
            ));
        }
        if (reservation.purpose == AuthorityPurpose::PatchPromotion) != promotion.is_some() {
            return Err(ValidationError::Invalid(
                "promotion requires an exact old/new ref goal".into(),
            ));
        }
        if let Some(environment) = environment {
            self.journal.environments.insert(nonce, environment);
        }
        self.journal.events.insert(
            nonce,
            AuthorityEvent {
                reservation,
                completed: false,
                outcome: None,
                promotion,
                validation: None,
            },
        );
        self.flush()?;
        Ok(nonce)
    }

    /// Commit exact terminal validation observations only after their receipt is
    /// durably stored. A generic `finish` note does not create this proof.
    ///
    /// # Errors
    /// Wrong/finished intent, partial or invalid evidence, clock or persistence.
    pub fn finish_evaluation(
        &mut self,
        evidence: &ValidationEvidence,
    ) -> Result<(), ValidationError> {
        self.verify_evaluation_record(evidence)?;
        if matches!(
            evidence.state,
            super::ValidationState::NotRun
                | super::ValidationState::Authorized
                | super::ValidationState::Running
        ) {
            return Err(ValidationError::NotValidated(evidence.state));
        }
        let now = super::authority::unix_now()?;
        if evidence.finished_at < evidence.started_at || evidence.finished_at > now {
            return Err(ValidationError::Clock);
        }
        if evidence.state == super::ValidationState::Passed {
            // This verifies structure/limits. The coordinator separately resolves
            // current source and private host context before storing the receipt.
            evidence.verify_passing_observations(&evidence.approved_plan, now)?;
            self.verify_execution_context(evidence.run_id, &evidence.approved_plan)?;
        }
        let completion = ValidationCompletion {
            evidence: evidence.digest()?,
            state: evidence.state,
        };
        let event = self
            .journal
            .events
            .get_mut(&evidence.run_id)
            .ok_or(ValidationError::RecoveryRequired)?;
        event.validation = Some(completion);
        event.completed = true;
        event.outcome = Some(format!(
            "validation {:?} observations persisted",
            evidence.state
        ));
        self.flush()
    }

    /// Record a generic observed/recovered terminal outcome. This does not turn
    /// a validation receipt into passing proof. An interrupted/unknown effect must
    /// remain unfinished until an explicit inspection/recovery operation resolves
    /// it; simply reopening never invokes this method or retries the effect.
    ///
    /// # Errors
    /// Unknown/already-completed intent, invalid note, uncertain store or I/O.
    pub fn finish(&mut self, nonce: uuid::Uuid, outcome: &str) -> Result<(), ValidationError> {
        if self.poisoned {
            return Err(ValidationError::RecoveryRequired);
        }
        if outcome.trim().is_empty() || outcome.len() > 2048 {
            return Err(ValidationError::Invalid(
                "bounded outcome note required".into(),
            ));
        }
        let event = self
            .journal
            .events
            .get_mut(&nonce)
            .ok_or_else(|| ValidationError::Invalid("unknown authority intent".into()))?;
        if event.completed {
            return Err(ValidationError::Consumed);
        }
        if event.reservation.purpose == AuthorityPurpose::Evaluation {
            // Recovery can retire an interrupted run, but never manufacture or
            // retain passing observations from a generic outcome note.
            event.validation = None;
        }
        event.completed = true;
        event.outcome = Some(localpilot_config::redact::redact(outcome));
        self.flush()
    }

    fn flush(&mut self) -> Result<(), ValidationError> {
        if let Err(error) = persist(&self.path, &self.journal) {
            self.poisoned = true;
            return Err(error);
        }
        Ok(())
    }
}

fn read_journal(path: &Path) -> Result<Journal, ValidationError> {
    let file = File::open(path).map_err(|source| io(path, source))?;
    let mut bytes = Vec::new();
    file.take(MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| io(path, source))?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(ValidationError::RecoveryRequired);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn persist(path: &Path, journal: &Journal) -> Result<(), ValidationError> {
    let bytes = serde_json::to_vec(journal)?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(ValidationError::RecoveryRequired);
    }
    localpilot_store::atomic_write(path, &bytes)?;
    Ok(())
}

fn io(path: &Path, source: std::io::Error) -> ValidationError {
    ValidationError::Io {
        path: path.to_owned(),
        source,
    }
}

fn regular_file(path: &Path) -> Result<(), ValidationError> {
    if !fs::symlink_metadata(path)
        .map_err(|source| io(path, source))?
        .file_type()
        .is_file()
    {
        return Err(ValidationError::Invalid(
            "authority path is not an ordinary file".into(),
        ));
    }
    Ok(())
}

fn regular_directory(path: &Path) -> Result<(), ValidationError> {
    if !fs::symlink_metadata(path)
        .map_err(|source| io(path, source))?
        .file_type()
        .is_dir()
    {
        return Err(ValidationError::Invalid(
            "authority path is not an ordinary directory".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::validation::tests::{fixture, git};

    fn repository() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-b", "main"]);
        git(
            repo.path(),
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@localhost",
                "commit",
                "--allow-empty",
                "-m",
                "fixture",
            ],
        );
        repo
    }

    #[test]
    fn durable_reservation_survives_restart_and_nonce_collision_is_refused() {
        let repo = repository();
        let mut store = RepositoryAuthority::open(repo.path()).unwrap();
        let mut tx = store.try_transaction().unwrap();
        let identity = tx.repository_identity();
        let (proposal, plan, _) = fixture(identity.clone());
        let token =
            EvaluationToken::approve(&proposal, &plan, "human", "execute fixed checks").unwrap();
        let reservation = token.reservation(&proposal, &plan).unwrap();
        let nonce = tx.reserve_evaluation(token, &proposal, &plan).unwrap();
        assert!(matches!(
            tx.reserve(reservation.clone()),
            Err(ValidationError::Consumed)
        ));
        drop(tx);
        drop(store);

        let mut reopened = RepositoryAuthority::open(repo.path()).unwrap();
        let mut tx = reopened.try_transaction().unwrap();
        assert_eq!(tx.repository_identity(), identity);
        assert!(matches!(
            tx.reserve(reservation.clone()),
            Err(ValidationError::Consumed)
        ));
        let fresh = EvaluationToken::approve(&proposal, &plan, "human", "explicit retry").unwrap();
        assert!(matches!(
            tx.reserve_evaluation(fresh, &proposal, &plan),
            Err(ValidationError::RecoveryRequired)
        ));
        assert!(!tx.events().next().unwrap().completed);
        tx.finish(
            nonce,
            "interrupted before spawn; inspected no process started",
        )
        .unwrap();
        assert!(matches!(
            tx.reserve(reservation),
            Err(ValidationError::Consumed)
        ));
        let fresh =
            EvaluationToken::approve(&proposal, &plan, "human", "new independently approved run")
                .unwrap();
        assert!(tx.reserve_evaluation(fresh, &proposal, &plan).is_ok());
    }

    #[test]
    fn changed_patch_receipt_and_outward_account_cannot_reuse_consent() {
        let repo = repository();
        let mut store = RepositoryAuthority::open(repo.path()).unwrap();
        let mut tx = store.try_transaction().unwrap();
        let (proposal, plan, mut receipt) = fixture(tx.repository_identity());
        let token = PatchApprovalToken::approve(
            &proposal,
            &plan,
            &receipt,
            "human",
            "promote reviewed tree",
        )
        .unwrap();
        receipt.run_id = uuid::Uuid::new_v4();
        assert!(matches!(
            tx.reserve_promotion(token, &proposal, &plan, &receipt),
            Err(ValidationError::Stale)
        ));
        let draft = crate::OutwardDraft {
            schema: crate::OUTWARD_SCHEMA.into(),
            id: proposal.proposal_id.clone(),
            kind: crate::OutwardKind::Issue,
            target_repo: "owner/repo".into(),
            title: "reviewed title".into(),
            body: "reviewed body".into(),
            head_branch: None,
        };
        let token = OutwardApprovalToken::approve(
            &tx.repository_identity(),
            &draft,
            "reviewed-account",
            "human",
            "publish this issue",
        )
        .unwrap();
        assert!(matches!(
            tx.reserve_outward(token, &draft, "different-account"),
            Err(ValidationError::Stale)
        ));
        let token = OutwardApprovalToken::approve(
            &tx.repository_identity(),
            &draft,
            "account",
            "human",
            "publish this issue",
        )
        .unwrap();
        let mut changed = draft.clone();
        changed.body.push_str("changed");
        assert!(matches!(
            tx.reserve_outward(token, &changed, "account"),
            Err(ValidationError::Stale)
        ));
        assert_eq!(tx.events().count(), 0);
        let token = OutwardApprovalToken::approve(
            &tx.repository_identity(),
            &draft,
            "account",
            "human",
            "publish this issue",
        )
        .unwrap();
        let nonce = tx.reserve_outward(token, &draft, "account").unwrap();
        assert_eq!(
            tx.events().next().unwrap().reservation.purpose,
            AuthorityPurpose::OutwardPublication
        );
        tx.finish(nonce, "fixture did not publish externally")
            .unwrap();
    }

    #[test]
    fn promotion_requires_a_completed_local_evaluation_and_current_environment() {
        let repo = repository();
        let mut store = RepositoryAuthority::open(repo.path()).unwrap();
        let mut tx = store.try_transaction().unwrap();
        let (proposal, mut plan, mut receipt) = fixture(tx.repository_identity());
        let fake =
            PatchApprovalToken::approve(&proposal, &plan, &receipt, "human", "promote").unwrap();
        assert!(matches!(
            tx.reserve_promotion(fake, &proposal, &plan, &receipt),
            Err(ValidationError::RecoveryRequired)
        ));
        let key = format!("LOCALPILOT_RECEIPT_{}", uuid::Uuid::new_v4().simple());
        std::env::set_var(&key, "private-original");
        plan.environment_names = vec![key.clone()];
        receipt.plan = plan.digest().unwrap();
        receipt.approved_plan = plan.clone();
        let evaluation =
            EvaluationToken::approve(&proposal, &plan, "human", "execute fixed checks").unwrap();
        let run = tx.reserve_evaluation(evaluation, &proposal, &plan).unwrap();
        receipt.run_id = run;
        let token =
            PatchApprovalToken::approve(&proposal, &plan, &receipt, "human", "promote").unwrap();
        assert!(matches!(
            tx.reserve_promotion(token, &proposal, &plan, &receipt),
            Err(ValidationError::RecoveryRequired)
        ));
        tx.finish_evaluation(&receipt).unwrap();
        let token =
            PatchApprovalToken::approve(&proposal, &plan, &receipt, "human", "promote").unwrap();
        std::env::set_var(&key, "private-changed");
        assert!(matches!(
            tx.reserve_promotion(token, &proposal, &plan, &receipt),
            Err(ValidationError::Stale)
        ));
        std::env::set_var(&key, "private-original");
        let token =
            PatchApprovalToken::approve(&proposal, &plan, &receipt, "human", "promote").unwrap();
        let promoted = tx
            .reserve_promotion(token, &proposal, &plan, &receipt)
            .unwrap();
        let goal = PromotionIntent {
            proposal_id: proposal.proposal_id.clone(),
            target_ref: proposal.target_ref.clone(),
            base_commit: proposal.base_commit.clone(),
            proposed_commit: proposal.proposed_commit.clone(),
            validation_run: run,
        };
        let persisted: Journal = read_journal(&tx.path).unwrap();
        assert_eq!(persisted.events[&promoted].promotion.as_ref(), Some(&goal));
        assert!(!persisted.events[&promoted].completed);
        // Old records remain inspectable. Missing a goal is missing recovery
        // proof, never permission to infer that an effect did not happen.
        let mut legacy = serde_json::to_value(&persisted.events[&promoted]).unwrap();
        legacy.as_object_mut().unwrap().remove("promotion");
        let legacy: AuthorityEvent = serde_json::from_value(legacy).unwrap();
        assert!(legacy.promotion.is_none());
        assert!(!legacy.completed);
        tx.finish(
            promoted,
            "fixture reservation only; no actual Git promotion",
        )
        .unwrap();
        let private_environment = super::super::authority::execution_context(&plan).unwrap();
        std::env::remove_var(key);
        assert_eq!(tx.events().count(), 2);
        let audit = serde_json::to_string(&tx.events().collect::<Vec<_>>()).unwrap();
        assert!(!audit.contains("private-original"));
        assert!(!audit.contains("environments"));
        assert!(!audit.contains(private_environment.as_str()));
    }

    #[test]
    fn missing_malformed_moved_and_unknown_journals_never_reset_authority() {
        for condition in ["missing", "malformed", "moved", "unknown"] {
            let repo = repository();
            let path = repo.path().join(".git/localpilot-authority/journal.json");
            let mut store = RepositoryAuthority::open(repo.path()).unwrap();
            drop(store.try_transaction().unwrap());
            drop(store);
            match condition {
                "missing" => fs::remove_file(&path).unwrap(),
                "malformed" => fs::write(&path, "{torn").unwrap(),
                _ => {
                    let mut journal: Journal = read_journal(&path).unwrap();
                    if condition == "moved" {
                        journal.common_dir.push_str("-different");
                    } else {
                        journal.schema.push('2');
                    }
                    persist(&path, &journal).unwrap();
                }
            }
            let before = fs::read(&path).ok();
            let mut reopened = RepositoryAuthority::open(repo.path()).unwrap();
            assert!(reopened.try_transaction().is_err(), "{condition}");
            assert_eq!(
                fs::read(&path).ok(),
                before,
                "never repair by discarding history"
            );
        }
    }

    #[test]
    fn linked_worktrees_share_repository_identity_and_exclusive_lock() {
        let repo = repository();
        let parent = tempfile::tempdir().unwrap();
        let linked = parent.path().join("linked");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        );
        let mut a = RepositoryAuthority::open(repo.path()).unwrap();
        let tx = a.try_transaction().unwrap();
        let identity = tx.repository_identity();
        let mut b = RepositoryAuthority::open(&linked).unwrap();
        assert!(matches!(b.try_transaction(), Err(ValidationError::Busy)));
        drop(tx);
        assert_eq!(b.try_transaction().unwrap().repository_identity(), identity);
    }

    // Invoked by an independent test process; never run as a normal suite case.
    #[test]
    #[ignore = "subprocess fixture invoked by process_crash_releases_lock_but_preserves_intent"]
    fn authority_child_process() {
        let root = PathBuf::from(std::env::var_os("LOCALPILOT_AUTHORITY_FIXTURE").unwrap());
        let mut store = RepositoryAuthority::open(&root).unwrap();
        let mut tx = store.try_transaction().unwrap();
        let (proposal, plan, _) = fixture(tx.repository_identity());
        let token = EvaluationToken::approve(
            &proposal,
            &plan,
            "fixture-human",
            "controlled process-crash fixture",
        )
        .unwrap();
        tx.reserve_evaluation(token, &proposal, &plan).unwrap();
        fs::write(root.join("ready"), "reserved").unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn process_crash_releases_lock_but_preserves_intent() {
        let repo = repository();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "validation::transaction::tests::authority_child_process",
                "--ignored",
            ])
            .env("LOCALPILOT_AUTHORITY_FIXTURE", repo.path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !repo.path().join("ready").exists() {
            if child.try_wait().unwrap().is_some() || std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("authority subprocess never reserved its intent");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let mut competing = RepositoryAuthority::open(repo.path()).unwrap();
        let locked = matches!(competing.try_transaction(), Err(ValidationError::Busy));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(locked, "the independent process must own the OS lock");
        let mut tx = competing.try_transaction().unwrap();
        assert_eq!(tx.events().count(), 1);
        assert!(!tx.events().next().unwrap().completed);
        let (proposal, plan, _) = fixture(tx.repository_identity());
        let token = EvaluationToken::approve(&proposal, &plan, "human", "new action").unwrap();
        assert!(matches!(
            tx.reserve_evaluation(token, &proposal, &plan),
            Err(ValidationError::RecoveryRequired)
        ));
    }
}
