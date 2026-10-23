//! Human-gated patch generation.
//!
//! The write half of the self-improvement loop (ADR-0034), built so the human
//! gate is structural. The agent may, autonomously, turn an approved finding into
//! a **minimal** change inside an **isolated git worktree** on its own branch:
//! [`propose`] validates scope, writes the edits inside the worktree (never the
//! main working tree), confirms the produced diff stays within the finding's
//! named files, commits on the branch, and packages a [`ChangeProvenance`] record
//! plus a human-reviewable [`DiffSummary`]. It then **stops**.
//!
//! Applying that change to the main branch — [`ProposedPatch::promote`] — requires
//! an [`ApprovalToken`], which only a human-confirmation path mints. There is no
//! code path from proposing to promoting without one, the agent never merges to
//! `main`, nothing here ever pushes, and rollback is to drop the worktree/branch
//! ([`ProposedPatch::discard`]).
//!
//! The same gate guards the *outward* half (ADR-0053): an [`OutwardDraft`] is a
//! draft issue/PR the agent may author and persist locally, but turning one into a
//! runnable [`PublishPlan`] requires the same [`ApprovalToken`]. See
//! [`mod@outward`].
#![forbid(unsafe_code)]

mod error;
mod gate;
mod git;
mod outward;
mod proposal;
mod provenance;
pub mod validation;

pub use error::PatchError;
pub use gate::ApprovalToken;
pub use git::{
    check_path_budget, sweep_worktrees, worktrees_root, Worktree, MAX_WORKTREE_NAME, WORKTREES_DIR,
};
pub use outward::{
    discard as discard_outward_draft, list as list_outward_drafts,
    record_event as record_outward_event, DraftRequest, EmitPhase, OutwardDraft, OutwardError,
    OutwardEvent, OutwardKind, OutwardPolicy, PublishPlan, OUTWARD_SCHEMA,
};
pub use proposal::{PatchProposal, ProposedEdit};
pub use provenance::{ChangeProvenance, EvalResult, ProposalBase, PROVENANCE_SCHEMA};

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Cap on the unified-diff text kept in the summary, so a huge proposal stays
/// reviewable rather than dumping unbounded content.
const MAX_DIFF_BYTES: usize = 64 * 1024;

/// Identity used for the isolated proposal commit, so committing does not depend
/// on the host's global git config.
const COMMIT_NAME: &str = "localpilot";
const COMMIT_EMAIL: &str = "localpilot@localhost";

/// A human-reviewable summary of what a proposal changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSummary {
    /// Project-relative paths the change touched.
    pub files: Vec<String>,
    /// Lines added across the change.
    pub insertions: u64,
    /// Lines removed across the change.
    pub deletions: u64,
    /// The unified diff (bounded by `MAX_DIFF_BYTES`; truncation is marked).
    pub patch: String,
    /// Whether `patch` was truncated.
    pub truncated: bool,
}

/// A proposed patch sitting in an isolated worktree, awaiting human review. Owns
/// the worktree; dropping a new unpersisted handle or [`Self::discard`] is rollback.
#[derive(Debug)]
pub struct ProposedPatch {
    id: String,
    repo_root: PathBuf,
    worktree: git::Worktree,
    base_commit: String,
    provenance: ChangeProvenance,
    diff_summary: DiffSummary,
}

/// The result of promoting a patch onto the main branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromoteOutcome {
    /// The branch that was merged.
    pub branch: String,
    /// The human reviewer recorded on the approval token.
    pub reviewer: String,
}

/// Turn an approved finding's proposal into a committed, isolated proposed patch,
/// behind no main-branch write. The agent stops here; a human reviews and (with a
/// token) promotes.
///
/// # Errors
/// - [`PatchError::IncompleteProvenance`] if the record lacks required fields;
/// - [`PatchError::EmptyProposal`] / [`PatchError::OutOfScope`] from scope checks;
/// - [`PatchError::OutsideWorktree`] if an edit path escapes the worktree;
/// - [`PatchError::OutOfScopeChange`] if the produced diff touches an unnamed file;
/// - [`PatchError::Git`] / [`PatchError::Io`] on git or filesystem failure.
pub fn propose(
    repo_root: &Path,
    branch: &str,
    proposal: &PatchProposal,
    mut provenance: ChangeProvenance,
) -> Result<ProposedPatch, PatchError> {
    if !provenance.is_complete() {
        return Err(PatchError::IncompleteProvenance);
    }
    proposal.validate_scope()?;

    let base_commit = git::head_commit(repo_root)?;
    provenance.proposal_base = Some(ProposalBase {
        commit: base_commit.clone(),
        target_ref: git::git(repo_root, &["symbolic-ref", "--quiet", "HEAD"])?
            .trim()
            .into(),
        allowed_paths: proposal
            .allowed_paths
            .iter()
            .map(|p| proposal::normalize(p))
            .collect(),
    });
    // Fresh source proposals never inherit a historical validation receipt.
    provenance.validation = None;
    let worktree = git::Worktree::create(repo_root, branch)?;

    // Write each edit strictly inside the worktree.
    for edit in &proposal.edits {
        let target = git::contained_edit_path(worktree.path(), &edit.path)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|source| PatchError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(&target, &edit.new_content).map_err(|source| PatchError::Io {
            path: target.clone(),
            source,
        })?;
    }

    // Minimality + scope, enforced on the *produced* diff (defense in depth over
    // the declared edits): the change must touch something, and only files the
    // finding named.
    // Stage inside the disposable worktree before comparing against its base:
    // an unstaged Git diff omits newly created, untracked proposal files.
    git::git(worktree.path(), &["add", "-A"])?;
    let changed = git::changed_paths(worktree.path(), &base_commit)?;
    if changed.is_empty() {
        return Err(PatchError::EmptyProposal);
    }
    let allowed: Vec<String> = proposal
        .allowed_paths
        .iter()
        .map(|p| proposal::normalize(p))
        .collect();
    for path in &changed {
        if !allowed.contains(&proposal::normalize(path)) {
            return Err(PatchError::OutOfScopeChange(path.clone()));
        }
    }

    // Commit on the isolated branch (never main); identity is inline so it does
    // not depend on global git config.
    let message = commit_message(&proposal.finding_evidence);
    git::git(
        worktree.path(),
        &[
            "-c",
            &format!("user.name={COMMIT_NAME}"),
            "-c",
            &format!("user.email={COMMIT_EMAIL}"),
            "commit",
            "-m",
            &message,
        ],
    )?;

    let diff_summary = compute_diff_summary(worktree.path(), &base_commit)?;
    let id = branch.to_string();
    Ok(ProposedPatch {
        id,
        repo_root: repo_root.to_path_buf(),
        worktree,
        base_commit,
        provenance,
        diff_summary,
    })
}

impl ProposedPatch {
    /// Persist detailed observations for a run consumed under this repository's
    /// current exclusive transaction. This writes data, never a passing capability
    /// or promotion approval; the final gate rechecks the complete receipt.
    ///
    /// # Errors
    /// Different repository/run/plan, unsupported schema or atomic-store failure.
    pub fn record_validation(
        &mut self,
        transaction: &validation::RepositoryTransaction<'_>,
        evidence: validation::ValidationEvidence,
    ) -> Result<(), validation::ValidationError> {
        transaction.ensure_repository(&self.repo_root)?;
        transaction.verify_evaluation_record(&evidence)?;
        if evidence.schema != validation::VALIDATION_SCHEMA {
            return Err(validation::ValidationError::Invalid(
                "unknown validation schema".into(),
            ));
        }
        self.provenance.validation = Some(evidence);
        write_proposal_meta(&self.repo_root, &self.id, &self.provenance)?;
        Ok(())
    }
    /// Reattach to a proposal created in an earlier process (e.g. a separate
    /// `promote`/`discard` CLI invocation) so it can be reviewed, promoted (with a
    /// token), or discarded. Reconstructs the handle from the on-disk worktree and
    /// the persisted provenance, and recomputes the diff summary. New records keep
    /// their original base explicitly; legacy review falls back to `<branch>^`.
    /// Dropping this inspection handle preserves the proposal for later review
    /// or explicit discard. This never mints a token and never writes the main
    /// branch — the [`ApprovalToken`] gate on [`Self::promote`] is unchanged.
    ///
    /// # Errors
    /// - [`PatchError::UnknownProposal`] if no worktree or record exists for `branch`;
    /// - [`PatchError::Serde`] if the persisted record cannot be parsed;
    /// - [`PatchError::Git`] / [`PatchError::Io`] on git or filesystem failure.
    pub fn reopen(repo_root: &Path, branch: &str) -> Result<Self, PatchError> {
        let worktree = git::Worktree::open(repo_root, branch)?;
        let meta_path = proposal_meta_path(repo_root, branch);
        let json = std::fs::read_to_string(&meta_path)
            .map_err(|_| PatchError::UnknownProposal(branch.to_string()))?;
        let provenance: ChangeProvenance =
            serde_json::from_str(&json).map_err(|e| PatchError::Serde(e.to_string()))?;
        let base_commit = match &provenance.proposal_base {
            Some(base)
                if matches!(base.commit.len(), 40 | 64)
                    && base.commit.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                base.commit.clone()
            }
            Some(_) => {
                return Err(PatchError::UnsupportedGitPolicy(
                    "invalid recorded proposal base".into(),
                ))
            }
            None => git::git(repo_root, &["rev-parse", &format!("{branch}^")])?
                .trim()
                .to_string(),
        };
        let diff_summary = compute_diff_summary(worktree.path(), &base_commit)?;
        Ok(Self {
            id: branch.to_string(),
            repo_root: repo_root.to_path_buf(),
            worktree,
            base_commit,
            provenance,
            diff_summary,
        })
    }

    /// The patch id (its branch name).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The worktree path holding the proposed change, for human inspection.
    #[must_use]
    pub fn worktree_path(&self) -> &Path {
        self.worktree.path()
    }

    /// The commit the proposal was based on.
    #[must_use]
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }

    /// The change-provenance record.
    #[must_use]
    pub fn provenance(&self) -> &ChangeProvenance {
        &self.provenance
    }

    /// The human-reviewable diff summary.
    #[must_use]
    pub fn diff_summary(&self) -> &DiffSummary {
        &self.diff_summary
    }

    /// Attach an eval-gate result to the provenance.
    pub fn attach_eval_result(&mut self, result: EvalResult) {
        self.provenance.eval_result = Some(result);
    }

    /// Promote the proposed patch onto the current main branch — the **only** path
    /// that writes outside the worktree, and it requires an [`ApprovalToken`] that
    /// authorizes exactly this patch. Conservative: refuses a dirty target working
    /// tree, fast-forwards only, and never pushes.
    ///
    /// # Errors
    /// - [`PatchError::TokenMismatch`] if the token does not authorize this patch;
    /// - [`PatchError::DirtyTarget`] if the main working tree has local changes;
    /// - [`PatchError::NotFastForward`] if the base moved and a rebase is needed;
    /// - [`PatchError::Git`] on git failure.
    pub fn promote(&self, token: &ApprovalToken) -> Result<PromoteOutcome, PatchError> {
        if !token.authorizes(&self.id) {
            return Err(PatchError::TokenMismatch);
        }
        if !git::is_clean(&self.repo_root)? {
            return Err(PatchError::DirtyTarget);
        }
        // Fast-forward only: never silently create a merge or resolve conflicts.
        match git::git(
            &self.repo_root,
            &["merge", "--ff-only", self.worktree.branch()],
        ) {
            Ok(_) => Ok(PromoteOutcome {
                branch: self.worktree.branch().to_string(),
                reviewer: token.reviewer().to_string(),
            }),
            Err(PatchError::Git { .. }) => Err(PatchError::NotFastForward),
            Err(other) => Err(other),
        }
    }

    /// Leave the proposal on disk after this handle is dropped, so a later process
    /// can [`Self::reopen`] it to review, promote, or discard. Writes the provenance
    /// record beside the worktrees and suppresses drop-removal. Without this, dropping
    /// a `ProposedPatch` removes its worktree/branch (the rollback default). Call it
    /// once the proposal has been recorded and the process is about to exit.
    ///
    /// # Errors
    /// [`PatchError::Io`] / [`PatchError::Serde`] if the record cannot be written.
    pub fn persist(mut self) -> Result<(), PatchError> {
        write_proposal_meta(&self.repo_root, &self.id, &self.provenance)?;
        self.worktree.detach();
        Ok(())
    }

    /// Discard the proposal — remove the worktree and delete its branch. The
    /// rollback path; nothing on the main branch was touched.
    ///
    /// # Errors
    /// [`PatchError::Git`] if the worktree cannot be removed.
    pub fn discard(mut self) -> Result<(), PatchError> {
        self.worktree.remove()?;
        // Best-effort: drop the persisted provenance record too.
        let _ = std::fs::remove_file(proposal_meta_path(&self.repo_root, &self.id));
        Ok(())
    }
}

/// The path of the persisted provenance record for a proposal branch. Lives beside
/// the worktrees, never inside one, so it never appears in the proposal's diff.
fn proposal_meta_path(repo_root: &Path, branch: &str) -> PathBuf {
    repo_root
        .join(".localpilot")
        .join("proposals")
        .join(format!("{branch}.json"))
}

/// Persist a proposal's provenance so [`ProposedPatch::reopen`] can restore it in a
/// later process.
fn write_proposal_meta(
    repo_root: &Path,
    branch: &str,
    provenance: &ChangeProvenance,
) -> Result<(), PatchError> {
    let path = proposal_meta_path(repo_root, branch);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| PatchError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let json = provenance
        .to_json()
        .map_err(|e| PatchError::Serde(e.to_string()))?;
    localpilot_store::atomic_write(&path, json.as_bytes())?;
    Ok(())
}

/// A plan-agnostic commit subject for the isolated proposal branch.
fn commit_message(finding_evidence: &str) -> String {
    let summary: String = finding_evidence
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(60)
        .collect();
    if summary.is_empty() {
        "proposed change".to_string()
    } else {
        format!("propose: {summary}")
    }
}

/// Build the diff summary from `base..HEAD` in the worktree.
fn compute_diff_summary(worktree: &Path, base: &str) -> Result<DiffSummary, PatchError> {
    let numstat = git::git(
        worktree,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--numstat",
            base,
            "HEAD",
        ],
    )?;
    let mut files = Vec::new();
    let mut insertions = 0_u64;
    let mut deletions = 0_u64;
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let added = parts.next().unwrap_or("0");
        let removed = parts.next().unwrap_or("0");
        let path = parts.next().unwrap_or("").trim();
        if path.is_empty() {
            continue;
        }
        insertions = insertions.saturating_add(added.parse().unwrap_or(0));
        deletions = deletions.saturating_add(removed.parse().unwrap_or(0));
        files.push(path.replace('\\', "/"));
    }
    let full = git::git(
        worktree,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            base,
            "HEAD",
        ],
    )?;
    let (patch, truncated) = if full.len() > MAX_DIFF_BYTES {
        let mut cut = full;
        let mut boundary = MAX_DIFF_BYTES;
        while !cut.is_char_boundary(boundary) {
            boundary -= 1;
        }
        cut.truncate(boundary);
        cut.push_str("\n... (diff truncated)\n");
        (cut, true)
    } else {
        (full, false)
    };
    Ok(DiffSummary {
        files,
        insertions,
        deletions,
        patch,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A fresh git repo with one committed file, for the worktree tests.
    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let run = |args: &[&str]| git::git(root, args).unwrap();
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        // `.localpilot/` is git-ignored in real projects (ADR-0012), so the
        // worktrees this crate creates under it never dirty `git status`.
        std::fs::write(root.join(".gitignore"), ".localpilot/\n").unwrap();
        std::fs::write(root.join("a.rs"), "pub fn f() {}\n").unwrap();
        std::fs::write(root.join("b.rs"), "pub fn g() {}\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);
        dir
    }

    fn provenance() -> ChangeProvenance {
        ChangeProvenance::new("fix the TODO in a.rs", "test-model", "the TODO is stale")
    }

    #[test]
    fn inspecting_a_persisted_proposal_does_not_discard_its_worktree() {
        let repo = init_repo();
        let request = proposal(&["a.rs"], &[("a.rs", "pub fn fixed() {}\n")]);
        let patch = propose(repo.path(), "persisted-review", &request, provenance()).unwrap();
        let path = patch.worktree_path().to_owned();
        patch.persist().unwrap();
        {
            let preview = ProposedPatch::reopen(repo.path(), "persisted-review").unwrap();
            assert!(preview.diff_summary().patch.contains("fixed"));
        }
        assert!(
            path.is_dir(),
            "merely viewing a persisted proposal must not delete it"
        );
        let again = ProposedPatch::reopen(repo.path(), "persisted-review").unwrap();
        again.discard().unwrap();
        assert!(
            !path.exists(),
            "explicit discard still removes a persisted proposal"
        );
    }

    fn marker_script(path: &Path, marker: &Path) {
        let quoted = marker
            .to_str()
            .unwrap()
            .replace('\\', "/")
            .replace('\'', "'\\''");
        std::fs::write(
            path,
            format!("#!/bin/sh\nprintf 'executed\\n' >> '{quoted}'\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn proposal_and_promotion_suppress_hooks_without_changing_user_configuration() {
        let repo = init_repo();
        let hooks = tempfile::tempdir().unwrap();
        let marker = hooks.path().join("marker");
        for name in [
            "post-checkout",
            "pre-commit",
            "commit-msg",
            "post-commit",
            "post-merge",
        ] {
            marker_script(&hooks.path().join(name), &marker);
        }
        git::git(
            repo.path(),
            &["config", "core.hooksPath", hooks.path().to_str().unwrap()],
        )
        .unwrap();
        // Positive sensitivity control: the ordinary Git operation executes the
        // harmless hook, proving this fixture could detect implicit execution.
        let control = repo.path().join(".localpilot/control");
        let output = std::process::Command::new("git")
            .current_dir(repo.path())
            .args([
                "worktree",
                "add",
                "--detach",
                control.to_str().unwrap(),
                "HEAD",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let before = std::fs::read(&marker).unwrap();
        assert!(!before.is_empty());
        let request = proposal(&["a.rs"], &[("a.rs", "pub fn fixed() {}\n")]);
        let patch = propose(repo.path(), "static-hooks", &request, provenance()).unwrap();
        let token = ApprovalToken::approve(patch.id(), "controlled-fixture-reviewer");
        patch.promote(&token).unwrap();
        assert_eq!(std::fs::read(&marker).unwrap(), before);
        let setting = std::process::Command::new("git")
            .current_dir(repo.path())
            .args(["config", "--local", "--get", "core.hooksPath"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(setting.stdout).unwrap().trim(),
            hooks.path().to_str().unwrap()
        );
        git::git(
            repo.path(),
            &["worktree", "remove", "--force", control.to_str().unwrap()],
        )
        .unwrap();
    }

    #[test]
    fn unused_filter_configuration_is_supported_but_active_filters_refuse_before_checkout() {
        let repo = init_repo();
        let helpers = tempfile::tempdir().unwrap();
        let marker = helpers.path().join("marker");
        let script = helpers.path().join("filter.sh");
        marker_script(&script, &marker);
        let command = format!(
            "sh '{}'",
            script
                .to_str()
                .unwrap()
                .replace('\\', "/")
                .replace('\'', "'\\''")
        );
        git::git(repo.path(), &["config", "filter.fixture.clean", &command]).unwrap();
        git::git(repo.path(), &["config", "filter.fixture.smudge", &command]).unwrap();
        let request = proposal(&["a.rs"], &[("a.rs", "pub fn fixed() {}\n")]);
        propose(repo.path(), "unused-filter", &request, provenance())
            .unwrap()
            .discard()
            .unwrap();
        assert!(!marker.exists(), "an unused configured filter never runs");

        // Materialize attributes without invoking conversion: no index/checkout
        // operation follows until the production guard has inspected them.
        std::fs::write(repo.path().join(".gitattributes"), "*.rs filter=fixture\n").unwrap();
        let before = git::head_commit(repo.path()).unwrap();
        let error = propose(repo.path(), "active-filter", &request, provenance()).unwrap_err();
        assert!(matches!(error, PatchError::UnsupportedGitPolicy(_)));
        assert!(!repo
            .path()
            .join(".localpilot/worktrees/active-filter")
            .exists());
        assert_eq!(git::head_commit(repo.path()).unwrap(), before);
        assert!(!marker.exists(), "refusal must precede filter execution");
    }

    #[test]
    fn diff_helpers_do_not_run_when_producing_the_reviewable_summary() {
        let repo = init_repo();
        let helpers = tempfile::tempdir().unwrap();
        let marker = helpers.path().join("marker");
        let script = helpers.path().join("diff-helper.sh");
        marker_script(&script, &marker);
        let command = format!(
            "sh '{}'",
            script
                .to_str()
                .unwrap()
                .replace('\\', "/")
                .replace('\'', "'\\''")
        );
        git::git(repo.path(), &["config", "diff.external", &command]).unwrap();
        git::git(repo.path(), &["config", "diff.fixture.textconv", &command]).unwrap();
        std::fs::write(repo.path().join(".gitattributes"), "*.rs diff=fixture\n").unwrap();
        git::git(repo.path(), &["add", ".gitattributes"]).unwrap();
        git::git(repo.path(), &["commit", "-m", "controlled diff attributes"]).unwrap();
        let request = proposal(&["a.rs"], &[("a.rs", "pub fn fixed() {}\n")]);
        let patch = propose(repo.path(), "static-diff", &request, provenance()).unwrap();
        assert!(patch.diff_summary().patch.contains("+pub fn fixed()"));
        assert!(!marker.exists());
        let output = std::process::Command::new("git")
            .current_dir(repo.path())
            .args(["diff", "--ext-diff", "HEAD", patch.id()])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(
            marker.exists(),
            "ordinary Git would execute the configured helper"
        );
    }

    #[test]
    fn large_unicode_diff_stays_bounded_and_reviewable_without_panicking() {
        let repo = init_repo();
        for padding in 0..4 {
            let text = format!("{}{}\n", "x".repeat(padding), "🦀".repeat(20000));
            let request = proposal(&["a.rs"], &[("a.rs", &text)]);
            let patch = propose(
                repo.path(),
                &format!("unicode-{padding}"),
                &request,
                provenance(),
            )
            .unwrap();
            assert!(patch.diff_summary().truncated);
            assert!(patch
                .diff_summary()
                .patch
                .ends_with("\n... (diff truncated)\n"));
            assert!(patch.diff_summary().patch.len() <= MAX_DIFF_BYTES + 30);
        }
    }

    #[test]
    fn filter_driver_names_cannot_masquerade_as_unspecified_attribute_sentinels() {
        let repo = init_repo();
        let request = proposal(&["a.rs"], &[("a.rs", "pub fn fixed() {}\n")]);
        for (index, attribute) in ["filter=unspecified", "filter=unset", "-filter"]
            .iter()
            .enumerate()
        {
            std::fs::write(
                repo.path().join(".gitattributes"),
                format!("*.rs {attribute}\n"),
            )
            .unwrap();
            assert!(matches!(
                propose(
                    repo.path(),
                    &format!("sentinel-{index}"),
                    &request,
                    provenance()
                ),
                Err(PatchError::UnsupportedGitPolicy(_))
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn proposal_refuses_a_committed_symlink_escape_before_writing() {
        let repo = init_repo();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("owned.txt"), "original").unwrap();
        std::os::unix::fs::symlink(outside.path(), repo.path().join("escape")).unwrap();
        git::git(repo.path(), &["add", "escape"]).unwrap();
        git::git(repo.path(), &["commit", "-m", "controlled symlink fixture"]).unwrap();
        let request = proposal(&["escape/owned.txt"], &[("escape/owned.txt", "changed")]);
        assert!(matches!(
            propose(repo.path(), "symlink-refused", &request, provenance()),
            Err(PatchError::OutsideWorktree(_))
        ));
        assert_eq!(
            std::fs::read_to_string(outside.path().join("owned.txt")).unwrap(),
            "original"
        );
    }

    fn proposal(allowed: &[&str], edits: &[(&str, &str)]) -> PatchProposal {
        PatchProposal::new(
            "stale TODO in a.rs",
            allowed.iter().map(|s| s.to_string()).collect(),
            edits
                .iter()
                .map(|(p, c)| ProposedEdit::new(*p, *c))
                .collect(),
        )
    }

    #[test]
    fn propose_writes_only_in_the_worktree_never_main() {
        let dir = init_repo();
        let root = dir.path();
        let before = std::fs::read_to_string(root.join("a.rs")).unwrap();

        let patch = propose(
            root,
            "sr-1",
            &proposal(&["a.rs"], &[("a.rs", "pub fn f() { /* fixed */ }\n")]),
            provenance(),
        )
        .unwrap();

        // The main working tree is untouched; the change lives in the worktree.
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), before);
        let in_wt = std::fs::read_to_string(patch.worktree_path().join("a.rs")).unwrap();
        assert!(in_wt.contains("fixed"));
        assert_eq!(patch.diff_summary().files, vec!["a.rs".to_string()]);
    }

    #[test]
    fn out_of_scope_edit_is_rejected_before_any_write() {
        let dir = init_repo();
        let err = propose(
            dir.path(),
            "sr-2",
            &proposal(&["a.rs"], &[("b.rs", "pub fn g() { /* sneaky */ }\n")]),
            provenance(),
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::OutOfScope(_)));
    }

    #[test]
    fn path_escape_is_rejected() {
        let dir = init_repo();
        let err = propose(
            dir.path(),
            "sr-esc",
            &proposal(&["../escape.rs"], &[("../escape.rs", "x")]),
            provenance(),
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::OutsideWorktree(_)));
    }

    #[test]
    fn no_op_proposal_is_rejected() {
        let dir = init_repo();
        // Re-write a.rs with its existing content: nothing changes.
        let err = propose(
            dir.path(),
            "sr-noop",
            &proposal(&["a.rs"], &[("a.rs", "pub fn f() {}\n")]),
            provenance(),
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::EmptyProposal));
    }

    #[test]
    fn incomplete_provenance_is_rejected() {
        let dir = init_repo();
        let bad = ChangeProvenance::new("", "", "");
        let err = propose(
            dir.path(),
            "sr-prov",
            &proposal(&["a.rs"], &[("a.rs", "changed\n")]),
            bad,
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::IncompleteProvenance));
    }

    #[test]
    fn promote_requires_a_matching_token_and_never_runs_without_one() {
        let dir = init_repo();
        let root = dir.path();
        let patch = propose(
            root,
            "sr-3",
            &proposal(&["a.rs"], &[("a.rs", "pub fn f() { /* fixed */ }\n")]),
            provenance(),
        )
        .unwrap();

        // A token for a different patch does not authorize this one.
        let wrong = ApprovalToken::approve("other", "david");
        assert!(matches!(
            patch.promote(&wrong).unwrap_err(),
            PatchError::TokenMismatch
        ));
        // main is still untouched.
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "pub fn f() {}\n"
        );

        // The matching token (a human act) promotes it: now main carries the fix.
        let token = ApprovalToken::approve(patch.id(), "david");
        let outcome = patch.promote(&token).unwrap();
        assert_eq!(outcome.reviewer, "david");
        assert!(std::fs::read_to_string(root.join("a.rs"))
            .unwrap()
            .contains("fixed"));
    }

    #[test]
    fn discard_rolls_back_without_touching_main() {
        let dir = init_repo();
        let root = dir.path();
        let patch = propose(
            root,
            "sr-4",
            &proposal(&["a.rs"], &[("a.rs", "pub fn f() { /* fixed */ }\n")]),
            provenance(),
        )
        .unwrap();
        let wt_path = patch.worktree_path().to_path_buf();
        patch.discard().unwrap();
        assert!(!wt_path.exists(), "worktree should be gone after discard");
        // main never changed.
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "pub fn f() {}\n"
        );
    }

    #[test]
    fn provenance_round_trips_and_eval_result_attaches() {
        let dir = init_repo();
        let mut patch = propose(
            dir.path(),
            "sr-5",
            &proposal(&["a.rs"], &[("a.rs", "changed\n")]),
            provenance(),
        )
        .unwrap();
        assert!(patch.provenance().is_complete());
        patch.attach_eval_result(EvalResult {
            passed: true,
            summary: "gate: pass".to_string(),
        });
        let json = patch.provenance().to_json().unwrap();
        assert!(json.contains("gate: pass"));
        assert!(json.contains(PROVENANCE_SCHEMA));
    }

    #[test]
    fn persisted_proposal_reopens_in_a_later_handle_then_promotes_with_a_token() {
        let dir = init_repo();
        let root = dir.path();
        // Propose, record it, then drop the live handle as a separate process would —
        // `persist` keeps the worktree/branch/record on disk.
        {
            let patch = propose(
                root,
                "sr-reopen",
                &proposal(&["a.rs"], &[("a.rs", "pub fn f() { /* fixed */ }\n")]),
                provenance(),
            )
            .unwrap();
            assert_eq!(patch.diff_summary().files, vec!["a.rs".to_string()]);
            patch.persist().unwrap();
        }
        // A later invocation reattaches from disk: same diff + provenance restored.
        let reopened = ProposedPatch::reopen(root, "sr-reopen").unwrap();
        assert_eq!(reopened.id(), "sr-reopen");
        assert_eq!(reopened.diff_summary().files, vec!["a.rs".to_string()]);
        assert!(reopened.provenance().is_complete());
        // main is untouched until a human token promotes.
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "pub fn f() {}\n"
        );
        let token = ApprovalToken::approve(reopened.id(), "david");
        reopened.promote(&token).unwrap();
        assert!(std::fs::read_to_string(root.join("a.rs"))
            .unwrap()
            .contains("fixed"));
    }

    #[test]
    fn reopen_of_an_unknown_id_errors() {
        let dir = init_repo();
        assert!(matches!(
            ProposedPatch::reopen(dir.path(), "nope").unwrap_err(),
            PatchError::UnknownProposal(_)
        ));
    }

    #[test]
    fn a_dropped_unpersisted_proposal_is_removed_and_cannot_reopen() {
        let dir = init_repo();
        let root = dir.path();
        {
            let _patch = propose(
                root,
                "sr-drop",
                &proposal(&["a.rs"], &[("a.rs", "pub fn f() { /* fixed */ }\n")]),
                provenance(),
            )
            .unwrap();
            // No persist(): dropping here removes the worktree/branch (the default).
        }
        assert!(matches!(
            ProposedPatch::reopen(root, "sr-drop").unwrap_err(),
            PatchError::UnknownProposal(_)
        ));
    }
}
