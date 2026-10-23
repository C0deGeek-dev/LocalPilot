//! Proposal execution contracts. These records describe evidence, not authority
//! against another process with the same user's access to the repository.
//!
//! The execution coordinator supplies observations; promotion must independently
//! revalidate the current identities under the repository transaction lock.

mod authority;
mod inspection;
#[cfg(test)]
mod tests;
mod transaction;

pub use authority::{EvaluationToken, OutwardApprovalToken, PatchApprovalToken};
pub use inspection::ProposalInspection;
pub use transaction::{
    AuthorityEvent, AuthorityPurpose, RepositoryAuthority, RepositoryTransaction, Reservation,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::path::PathBuf;

/// Current persisted validation format. Unknown formats never imply a pass.
pub const VALIDATION_SCHEMA: &str = "localpilot-proposal-validation-v1";
/// Maximum age of passing evidence at promotion.
pub const RECEIPT_LIFETIME_SECS: u64 = 24 * 60 * 60;

/// Errors at the proposal authorization/evidence boundary.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ValidationError {
    /// The owning proposal/Git primitive refused inspection.
    #[error(transparent)]
    Patch(#[from] crate::PatchError),
    /// Invalid canonical input or unsupported persisted schema.
    #[error("invalid validation record: {0}")]
    Invalid(String),
    /// The current proposal, plan or publication target differs from consent.
    #[error("authorization identity changed; preview and approve again")]
    Stale,
    /// A grant or evidence is outside its approved lifetime.
    #[error("authorization or evidence expired; preview and approve again")]
    Expired,
    /// Wall time moved backwards or cannot be obtained.
    #[error("authorization clock is not valid")]
    Clock,
    /// Another process owns the effectful repository transaction.
    #[error("another repository operation is active; try again after it completes")]
    Busy,
    /// A previously reserved nonce can never be used again.
    #[error("authorization has already been consumed")]
    Consumed,
    /// An interrupted or unfinished effect needs inspection before another one.
    #[error("repository has an unfinished authority transaction; inspect recovery state")]
    RecoveryRequired,
    /// A durable authority file could not be read or written.
    #[error("authority I/O at {}: {source}", path.display())]
    Io {
        /// File involved.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// Reused atomic-write primitive failed.
    #[error(transparent)]
    Store(#[from] localpilot_store::StoreError),
    /// Malformed authority persistence is not recoverable as unused consent.
    #[error("authority record cannot be decoded: {0}")]
    Json(#[from] serde_json::Error),
}

/// Validated lowercase SHA256, shared by records without exposing token internals.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContentDigest(String);

impl ContentDigest {
    /// Standard SHA256 of actual content bytes (for executables and artifacts).
    #[must_use]
    pub fn for_bytes(bytes: &[u8]) -> Self {
        Self(format!("{:x}", Sha256::digest(bytes)))
    }
    /// Read the canonical hex representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn hash(domain: &str, fields: &[&[u8]]) -> Self {
        let mut hash = CanonicalHash::new(domain);
        for value in fields {
            hash.field(value);
        }
        hash.finish()
    }
}

impl TryFrom<String> for ContentDigest {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(ValidationError::Invalid("expected lowercase SHA256".into()));
        }
        Ok(Self(value))
    }
}

impl From<ContentDigest> for String {
    fn from(value: ContentDigest) -> Self {
        value.0
    }
}

struct CanonicalHash(Sha256);

impl CanonicalHash {
    fn new(domain: &str) -> Self {
        let mut hash = Self(Sha256::new());
        hash.field(domain.as_bytes());
        hash
    }

    fn field(&mut self, value: &[u8]) {
        self.0.update((value.len() as u64).to_be_bytes());
        self.0.update(value);
    }

    fn number(&mut self, value: u64) {
        self.field(&value.to_be_bytes());
    }

    fn finish(self) -> ContentDigest {
        ContentDigest(format!("{:x}", self.0.finalize()))
    }
}

/// Snapshot obtained by inspecting a clean, supported proposal and its target.
/// Deserialization restores data only; the promotion gate must inspect again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalIdentity {
    /// Shared repository identity including canonical Git-common-dir location.
    pub repository: ContentDigest,
    /// Proposal branch identifier.
    pub proposal_id: String,
    /// Exact target reference, including its namespace.
    pub target_ref: String,
    /// Target commit consent was based on.
    pub base_commit: String,
    /// Exact proposed commit.
    pub proposed_commit: String,
    /// Git tree at that commit.
    pub proposed_tree: String,
    /// Actual source bytes, including supported unchanged submodules.
    pub source_bytes: ContentDigest,
    /// Complete diff digest, independent of bounded display text.
    pub diff: ContentDigest,
    /// Scoped provenance, excluding validation records to avoid recursive hashes.
    pub provenance: ContentDigest,
    /// Git attributes, line ending and checkout policy.
    pub text_policy: ContentDigest,
}

impl ProposalIdentity {
    /// Shared preview/consent binding for this proposal and exact ordered plan.
    /// The policy version in the plan owns the derived risk/permission wording.
    ///
    /// # Errors
    /// Unsupported or incomplete validation plans cannot be authorized.
    pub fn execution_identity(
        &self,
        plan: &ValidationPlanIdentity,
    ) -> Result<ContentDigest, ValidationError> {
        Ok(ContentDigest::hash(
            "localpilot-evaluation-consent-v1",
            &[
                self.digest().as_str().as_bytes(),
                plan.digest()?.as_str().as_bytes(),
            ],
        ))
    }

    /// Hash the explicit ordered fields, never display or pretty JSON.
    #[must_use]
    pub fn digest(&self) -> ContentDigest {
        ContentDigest::hash(
            "localpilot-proposal-identity-v1",
            &[
                self.repository.as_str().as_bytes(),
                self.proposal_id.as_bytes(),
                self.target_ref.as_bytes(),
                self.base_commit.as_bytes(),
                self.proposed_commit.as_bytes(),
                self.proposed_tree.as_bytes(),
                self.source_bytes.as_str().as_bytes(),
                self.diff.as_str().as_bytes(),
                self.provenance.as_str().as_bytes(),
                self.text_policy.as_str().as_bytes(),
            ],
        )
    }
}

/// Typed execution from a ratified profile. Resolution never executes this
/// executable merely to obtain an identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedCheck {
    /// Ratified check name.
    pub name: String,
    /// Resolved absolute program path, not a shell string.
    pub program: String,
    /// Resolved executable bytes.
    pub executable: ContentDigest,
    /// Exact arguments, preserving boundaries and order.
    pub args: Vec<String>,
}

/// Installed host toolchain components resolved without running proposed tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostToolchainIdentity {
    /// Explicit selector from the trusted proposal base.
    pub selector: String,
    /// Resolved installed toolchain root, outside both project worktrees.
    pub root: String,
    /// Sorted component names and actual content digests.
    pub components: std::collections::BTreeMap<String, ContentDigest>,
}

impl HostToolchainIdentity {
    /// Stable identity including tool paths, compiler and executable content.
    #[must_use]
    pub fn digest(&self) -> ContentDigest {
        let mut hash = CanonicalHash::new("localpilot-host-toolchain-v1");
        hash.field(self.selector.as_bytes());
        hash.field(self.root.as_bytes());
        hash.number(self.components.len() as u64);
        for (name, content) in &self.components {
            hash.field(name.as_bytes());
            hash.field(content.as_str().as_bytes());
        }
        hash.finish()
    }
}

/// Execution limits are part of consent, not ambient defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationLimits {
    /// Deadline per command.
    pub per_check_secs: u64,
    /// Total wall deadline including cleanup.
    pub total_secs: u64,
    /// Captured bytes per output stream.
    pub stream_bytes: u64,
    /// Maximum number of declared artifacts.
    pub artifact_count: u64,
    /// Maximum size per artifact.
    pub artifact_bytes: u64,
    /// Maximum aggregate artifact size.
    pub artifact_total_bytes: u64,
}

impl Default for ValidationLimits {
    fn default() -> Self {
        Self {
            per_check_secs: 300,
            total_secs: 900,
            stream_bytes: 16 * 1024,
            artifact_count: 16,
            artifact_bytes: 1024 * 1024,
            artifact_total_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Canonical reviewed command plan. Environment value digests are intentionally
/// not persisted in this inspectable record; the coordinator supplies a private
/// comparison digest when resolving a plan, never a secret or its public hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationPlanIdentity {
    /// Ratified profile and explicit version.
    pub profile: String,
    /// Policy controlling profile selection and permissions.
    pub policy_version: String,
    /// Actual platform (platform-specific commands have different identities).
    pub platform: String,
    /// Canonical proposed worktree root.
    pub worktree: String,
    /// Trusted toolchain identity.
    pub toolchain: ContentDigest,
    /// Ordered execute-only checks; no fixers or automatic retries.
    pub checks: Vec<PlannedCheck>,
    /// Names of allowed inherited environment variables, sorted uniquely.
    pub environment_names: Vec<String>,
    /// Relevant host configuration paths. Only names are public; private token/
    /// journal comparison binds their bounded content without exposing it.
    pub private_config_paths: Vec<String>,
    /// Only these ignored outputs may be created.
    pub artifact_paths: Vec<String>,
    /// Declared ignored build/cache directories; not source or receipt artifacts.
    pub cache_paths: Vec<String>,
    /// Explicit resource limits.
    pub limits: ValidationLimits,
}

impl ValidationPlanIdentity {
    /// Validate supported limits and deterministic representation, then hash.
    ///
    /// # Errors
    /// Rejects empty checks, unsupported limits, relative programs, shell
    /// commands, duplicate/unsorted environment names and invalid artifact paths.
    pub fn digest(&self) -> Result<ContentDigest, ValidationError> {
        if self.profile.trim().is_empty()
            || self.policy_version.trim().is_empty()
            || self.platform.trim().is_empty()
            || !std::path::Path::new(&self.worktree).is_absolute()
            || self.checks.is_empty()
            || self.checks.len() > 16
            || self.limits != ValidationLimits::default()
            || self.environment_names.windows(2).any(|p| p[0] >= p[1])
            || self
                .environment_names
                .iter()
                .any(|n| n.is_empty() || n.contains(['=', '\0']))
            || self.artifact_paths.len() > 16
            || self.cache_paths.len() > 16
            || self.private_config_paths.len() > 8
        {
            return Err(ValidationError::Invalid("unsupported command plan".into()));
        }
        let mut hash = CanonicalHash::new("localpilot-validation-plan-v1");
        for field in [
            &self.profile,
            &self.policy_version,
            &self.platform,
            &self.worktree,
        ] {
            hash.field(field.as_bytes());
        }
        hash.field(self.toolchain.as_str().as_bytes());
        hash.number(self.checks.len() as u64);
        for check in &self.checks {
            let file = std::path::Path::new(&check.program)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if check.name.trim().is_empty()
                || !std::path::Path::new(&check.program).is_absolute()
                || matches!(
                    file.as_str(),
                    "sh" | "bash" | "zsh" | "cmd" | "powershell" | "pwsh"
                )
                || check.program.contains('\0')
                || check.args.iter().any(|arg| arg.contains('\0'))
                || std::path::Path::new(&check.program)
                    .extension()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| {
                        matches!(
                            s.to_ascii_lowercase().as_str(),
                            "cmd" | "bat" | "ps1" | "sh"
                        )
                    })
            {
                return Err(ValidationError::Invalid(
                    "unsupported check executable".into(),
                ));
            }
            hash.field(check.name.as_bytes());
            hash.field(check.program.as_bytes());
            hash.field(check.executable.as_str().as_bytes());
            hash.number(check.args.len() as u64);
            for arg in &check.args {
                hash.field(arg.as_bytes());
            }
        }
        hash.number(self.environment_names.len() as u64);
        for name in &self.environment_names {
            hash.field(name.as_bytes());
        }
        hash.number(self.private_config_paths.len() as u64);
        for path in &self.private_config_paths {
            if !std::path::Path::new(path).is_absolute() {
                return Err(ValidationError::Invalid(
                    "private configuration path must be absolute".into(),
                ));
            }
            hash.field(path.as_bytes());
        }
        hash.number(self.artifact_paths.len() as u64);
        let mut seen_artifacts = std::collections::BTreeSet::new();
        for path in &self.artifact_paths {
            let identity = if cfg!(windows) {
                path.to_lowercase()
            } else {
                path.clone()
            };
            if !valid_relative_path(path)
                || matches!(path.split('/').next(), Some(".git" | ".localpilot"))
                || !seen_artifacts.insert(identity)
            {
                return Err(ValidationError::Invalid("unsupported artifact path".into()));
            }
            hash.field(path.as_bytes());
        }
        hash.number(self.cache_paths.len() as u64);
        for path in &self.cache_paths {
            if !valid_relative_path(path)
                || matches!(path.split('/').next(), Some(".git" | ".localpilot"))
            {
                return Err(ValidationError::Invalid(
                    "unsupported cache directory".into(),
                ));
            }
            hash.field(path.as_bytes());
        }
        for limit in [
            self.limits.per_check_secs,
            self.limits.total_secs,
            self.limits.stream_bytes,
            self.limits.artifact_count,
            self.limits.artifact_bytes,
            self.limits.artifact_total_bytes,
        ] {
            hash.number(limit);
        }
        Ok(hash.finish())
    }
}

fn valid_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', ':', '\0'])
        && !path.starts_with('/')
        && path
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}

/// Adjunct to the existing five self-improvement stages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ValidationState {
    /// Legacy records without evidence never qualify as passing.
    #[default]
    NotRun,
    /// Recorded human intent; not executable consent after restart.
    Authorized,
    /// Process-owned execution; interrupted execution is invalid.
    Running,
    /// All required commands and cleanup completed successfully.
    Passed,
    /// A required command failed.
    Failed,
    /// Explicitly cancelled.
    Cancelled,
    /// Malformed, interrupted, missing proof or uncertain cleanup.
    Invalid,
    /// The lifetime elapsed.
    Expired,
    /// Proposal or plan changed.
    Stale,
}

/// Exact command result, independent of display logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ValidationCommandEnd {
    /// An observed exit; missing code cannot be called successful.
    Exited { code: i32 },
    /// Permission denied before execution.
    Denied,
    /// Start failed.
    NotStarted,
    /// Approved deadline elapsed.
    TimedOut,
    /// Cancelled before or during execution.
    Cancelled,
    /// Process wait or capture failed.
    Failed,
}

/// Bounded and redacted observation of one actual execute-only check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationCheckEvidence {
    /// Exact resolved command approved in the plan.
    pub command: PlannedCheck,
    /// Process outcome.
    pub end: ValidationCommandEnd,
    /// Measured elapsed milliseconds.
    pub elapsed_ms: u64,
    /// Bounded redacted stdout.
    pub stdout: String,
    /// Bounded redacted stderr.
    pub stderr: String,
    /// Capture discarded excess bytes.
    pub truncated: bool,
    /// Both inherited output pipes closed.
    pub pipes_closed: bool,
    /// Cleanup acknowledged, not merely a best-effort kill request.
    pub cleanup_confirmed: bool,
}

/// Declared contained output and its observed bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationArtifact {
    /// Relative path from the proposal worktree.
    pub path: String,
    /// Actual size.
    pub bytes: u64,
    /// Full artifact content hash.
    pub digest: ContentDigest,
}

/// Inspectable evidence. Deserialization never produces a promotion capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationEvidence {
    /// Explicit version; unknown versions fail closed.
    pub schema: String,
    /// Unique run identity; no execution token is persisted.
    pub run_id: uuid::Uuid,
    /// Proposal at the start of approved execution.
    pub proposal: ContentDigest,
    /// Exact reviewed plan at execution.
    pub plan: ContentDigest,
    /// Aggregate outcome, never sufficient without the detailed observations.
    pub state: ValidationState,
    /// Unix seconds at start.
    pub started_at: u64,
    /// Unix seconds after checks and cleanup.
    pub finished_at: u64,
    /// Actual ordered command observations.
    pub checks: Vec<ValidationCheckEvidence>,
    /// Actual declared bounded outputs.
    pub artifacts: Vec<ValidationArtifact>,
}

impl ValidationEvidence {
    /// Recheck every pass condition against a freshly resolved current plan.
    ///
    /// # Errors
    /// Rejects stale, expired, partial, mutated, malformed or uncertain evidence.
    pub fn verify_current(
        &self,
        proposal: &ProposalIdentity,
        plan: &ValidationPlanIdentity,
        now: u64,
    ) -> Result<(), ValidationError> {
        if self.schema != VALIDATION_SCHEMA || self.run_id.is_nil() {
            return Err(ValidationError::Invalid(
                "unsupported evidence identity".into(),
            ));
        }
        if self.proposal != proposal.digest() || self.plan != plan.digest()? {
            return Err(ValidationError::Stale);
        }
        if self.started_at > self.finished_at || self.finished_at > now {
            return Err(ValidationError::Clock);
        }
        if now - self.finished_at >= RECEIPT_LIFETIME_SECS {
            return Err(ValidationError::Expired);
        }
        if self.state != ValidationState::Passed
            || self.finished_at - self.started_at > plan.limits.total_secs
            || self.checks.len() != plan.checks.len()
        {
            return Err(ValidationError::Invalid(
                "incomplete passing evidence".into(),
            ));
        }
        for (actual, expected) in self.checks.iter().zip(&plan.checks) {
            if actual.command != *expected
                || actual.end != (ValidationCommandEnd::Exited { code: 0 })
                || !actual.pipes_closed
                || !actual.cleanup_confirmed
                || actual.elapsed_ms > plan.limits.per_check_secs * 1000
                || actual.stdout.len() as u64 > plan.limits.stream_bytes
                || actual.stderr.len() as u64 > plan.limits.stream_bytes
            {
                return Err(ValidationError::Invalid(
                    "command or cleanup did not pass".into(),
                ));
            }
        }
        let observed_ms = self
            .checks
            .iter()
            .try_fold(0_u64, |total, check| total.checked_add(check.elapsed_ms));
        if observed_ms.is_none_or(|elapsed| elapsed > plan.limits.total_secs * 1000) {
            return Err(ValidationError::Invalid(
                "aggregate command deadline exceeded".into(),
            ));
        }
        if self.artifacts.len() != plan.artifact_paths.len() {
            return Err(ValidationError::Invalid("incomplete artifacts".into()));
        }
        let mut total = 0_u64;
        for (actual, expected) in self.artifacts.iter().zip(&plan.artifact_paths) {
            if actual.path != *expected || actual.bytes > plan.limits.artifact_bytes {
                return Err(ValidationError::Invalid(
                    "artifact exceeds approved bounds".into(),
                ));
            }
            total = total
                .checked_add(actual.bytes)
                .ok_or_else(|| ValidationError::Invalid("artifact size overflow".into()))?;
        }
        if total > plan.limits.artifact_total_bytes {
            return Err(ValidationError::Invalid(
                "artifact total exceeds approved bounds".into(),
            ));
        }
        Ok(())
    }

    /// Inspect resumed evidence without resuming execution or trusting old passes.
    #[must_use]
    pub fn resumed_state(&self) -> ValidationState {
        if self.schema != VALIDATION_SCHEMA || self.run_id.is_nil() {
            return ValidationState::Invalid;
        }
        match self.state {
            ValidationState::Authorized | ValidationState::Running => ValidationState::Invalid,
            other => other,
        }
    }
}
