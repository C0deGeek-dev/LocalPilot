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
        self.reserve_with_environment(reservation, Some(token.environment_identity()))
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
        let evaluation = self
            .journal
            .events
            .get(&receipt.run_id)
            .ok_or(ValidationError::RecoveryRequired)?;
        let expected_evaluation_binding = ContentDigest::hash(
            "localpilot-evaluation-consent-v1",
            &[
                proposal.digest().as_str().as_bytes(),
                plan.digest()?.as_str().as_bytes(),
            ],
        );
        if !evaluation.completed
            || evaluation.reservation.purpose != AuthorityPurpose::Evaluation
            || evaluation.reservation.binding != expected_evaluation_binding
            || self.journal.environments.get(&receipt.run_id)
                != Some(&super::authority::environment_digest(plan))
        {
            return Err(ValidationError::Stale);
        }
        self.reserve(reservation)
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
        self.reserve_with_environment(reservation, None)
    }

    fn reserve_with_environment(
        &mut self,
        reservation: Reservation,
        environment: Option<ContentDigest>,
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
        if let Some(environment) = environment {
            self.journal.environments.insert(nonce, environment);
        }
        self.journal.events.insert(
            nonce,
            AuthorityEvent {
                reservation,
                completed: false,
                outcome: None,
            },
        );
        self.flush()?;
        Ok(nonce)
    }

    /// Record the observed terminal outcome. An interrupted/unknown effect must
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
        let evaluation =
            EvaluationToken::approve(&proposal, &plan, "human", "execute fixed checks").unwrap();
        let run = tx.reserve_evaluation(evaluation, &proposal, &plan).unwrap();
        receipt.run_id = run;
        let token =
            PatchApprovalToken::approve(&proposal, &plan, &receipt, "human", "promote").unwrap();
        assert!(matches!(
            tx.reserve_promotion(token, &proposal, &plan, &receipt),
            Err(ValidationError::Stale)
        ));
        tx.finish(run, "controlled passing fixture with observed cleanup")
            .unwrap();
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
        tx.finish(
            promoted,
            "fixture reservation only; no actual Git promotion",
        )
        .unwrap();
        std::env::remove_var(key);
        assert_eq!(tx.events().count(), 2);
        let audit = serde_json::to_string(&tx.events().collect::<Vec<_>>()).unwrap();
        assert!(!audit.contains("private-original"));
        assert!(!audit.contains("environments"));
        assert!(!audit.contains(super::super::authority::environment_digest(&plan).as_str()));
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
