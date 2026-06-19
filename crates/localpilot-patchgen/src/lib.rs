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
#![forbid(unsafe_code)]

mod error;
mod gate;
mod git;
mod proposal;
mod provenance;

pub use error::PatchError;
pub use gate::ApprovalToken;
pub use proposal::{PatchProposal, ProposedEdit};
pub use provenance::{ChangeProvenance, EvalResult, PROVENANCE_SCHEMA};

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
/// the worktree; dropping or [`Self::discard`]ing it is the rollback.
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
    provenance: ChangeProvenance,
) -> Result<ProposedPatch, PatchError> {
    if !provenance.is_complete() {
        return Err(PatchError::IncompleteProvenance);
    }
    proposal.validate_scope()?;

    let base_commit = git::head_commit(repo_root)?;
    let worktree = git::Worktree::create(repo_root, branch)?;

    // Write each edit strictly inside the worktree.
    for edit in &proposal.edits {
        let target = git::safe_join(worktree.path(), &edit.path)?;
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
    git::git(worktree.path(), &["add", "-A"])?;
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

    /// Discard the proposal — remove the worktree and delete its branch. The
    /// rollback path; nothing on the main branch was touched.
    ///
    /// # Errors
    /// [`PatchError::Git`] if the worktree cannot be removed.
    pub fn discard(mut self) -> Result<(), PatchError> {
        self.worktree.remove()
    }
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
    let numstat = git::git(worktree, &["diff", "--numstat", base, "HEAD"])?;
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
    let full = git::git(worktree, &["diff", "--no-color", base, "HEAD"])?;
    let (patch, truncated) = if full.len() > MAX_DIFF_BYTES {
        let mut cut = full;
        cut.truncate(MAX_DIFF_BYTES);
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
}
