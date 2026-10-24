//! The thin orchestrator: it sequences the four self-improvement stages by
//! calling their **existing** entrypoints in contract order, persists the loop
//! state so a step can be resumed in a later process, and **stops at the
//! validated capability gate — never minting consent.** After approval it builds the
//! *approved, merged* tree, never the proposal worktree.
//!
//! It holds only sequencing and state. The stages stay in their own crates: the
//! read-only find (`localpilot-selfreview`), the human-gated source mutation
//! (`localpilot-patchgen`), and the binary lifecycle (`localpilot-selfdev`) are
//! different concerns with different blast radii, so this layer composes them
//! rather than fusing them.

use std::io::Write;
use std::path::{Path, PathBuf};

use localpilot_patchgen::validation::RepositoryAuthority;
use localpilot_patchgen::{propose, ChangeProvenance, PatchProposal, PromoteOutcome};
use localpilot_selfdev::AutoReloadBreaker;
use localpilot_selfreview::{review, Report, ReviewOptions};
use serde::{Deserialize, Serialize};

use crate::contract::{LoopError, Stage};

/// How many failed self-dev advances the reused circuit breaker tolerates before
/// it trips. Mirrors the `selfdev status` breaker bound; the autonomous loop that
/// would otherwise consume it is deferred (ADR-0128).
const AUTO_RELOAD_LIMIT: u32 = 3;

/// A build the self-dev stage produced and promoted — the immutable label it
/// installed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRecord {
    /// The version label the build was installed as, and a channel promoted to.
    pub label: String,
}

/// An error from the self-dev build/reload stage, surfaced through the seam so the
/// orchestrator can stay generic over it.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct StageError(pub String);

/// The build → reload half of the loop, behind a seam so the orchestrator stays
/// thin and offline-testable (a unit test never pays for a real cargo build). The
/// real implementation, [`SelfDevRunner`], delegates to `localpilot-selfdev`; it
/// re-implements neither the gauntlet, the immutable store, nor the reload.
pub trait SelfDevStage {
    /// Build the **approved** tree at `approved_root`, run the publish gauntlet,
    /// install it immutably, and promote the `current` channel to it. Performs no
    /// process swap. `out` receives progress lines.
    ///
    /// # Errors
    /// [`StageError`] carrying the underlying self-dev failure.
    fn build_and_promote(
        &mut self,
        approved_root: &Path,
        out: &mut dyn Write,
    ) -> Result<BuildRecord, StageError>;

    /// Swap the running process onto the promoted build. On success the process is
    /// replaced, so this **may not return**; a failure to even start the successor
    /// returns [`StageError`].
    ///
    /// # Errors
    /// [`StageError`] if the swap could not be started.
    fn reload_onto(&mut self, built: &BuildRecord, out: &mut dyn Write) -> Result<(), StageError>;

    /// Release the host's repository guard only after a successor has started.
    /// Adapters that spawn and then wait must override this method and call
    /// `launched` before waiting. Successful Unix exec closes the repository's
    /// close-on-exec guard handle and does not return. The default supports returning adapters.
    ///
    /// # Errors
    /// [`StageError`] before launch retains the guard for state rollback; after
    /// launch an error must not imply that no successor exists.
    fn reload_with_handoff(
        &mut self,
        built: &BuildRecord,
        out: &mut dyn Write,
        launched: &mut dyn FnMut(),
    ) -> Result<(), StageError> {
        self.reload_onto(built, out)?;
        launched();
        Ok(())
    }
}

/// The real self-dev stage: a thin adapter over `localpilot-selfdev` primitives.
/// It never re-implements them — it calls `build_gauntlet_promote` for the build
/// and `relaunch` for the swap. The rollback circuit breaker is applied by the
/// [`Orchestrator`] around this seam, so this adapter carries no policy.
pub struct SelfDevRunner {
    selfdev_root: PathBuf,
    reload_args: Vec<String>,
}

impl SelfDevRunner {
    /// A runner rooted at the per-user self-dev data root. After a swap, the new
    /// binary comes up running `selfimprove status`, so the reload is visibly
    /// complete.
    #[must_use]
    pub fn new(selfdev_root: impl Into<PathBuf>) -> Self {
        Self {
            selfdev_root: selfdev_root.into(),
            reload_args: vec!["selfimprove".to_string(), "status".to_string()],
        }
    }
}

impl SelfDevStage for SelfDevRunner {
    fn build_and_promote(
        &mut self,
        approved_root: &Path,
        out: &mut dyn Write,
    ) -> Result<BuildRecord, StageError> {
        let installed = localpilot_selfdev::build_gauntlet_promote(
            approved_root,
            &self.selfdev_root,
            localpilot_selfdev::CURRENT.into(),
            None,
            out,
        )
        .map_err(|error| StageError(error.to_string()))?;
        Ok(BuildRecord {
            label: installed.label,
        })
    }

    fn reload_onto(&mut self, built: &BuildRecord, out: &mut dyn Write) -> Result<(), StageError> {
        self.reload_with_handoff(built, out, &mut || {})
    }

    fn reload_with_handoff(
        &mut self,
        built: &BuildRecord,
        out: &mut dyn Write,
        launched: &mut dyn FnMut(),
    ) -> Result<(), StageError> {
        let store = localpilot_selfdev::VersionStore::new(&self.selfdev_root);
        let stored = store
            .get(&built.label)
            .ok_or_else(|| StageError(format!("built version {} is not installed", built.label)))?;
        let program = stored.executable();
        writeln!(out, "reloading onto {}...", built.label)
            .map_err(|error| StageError(error.to_string()))?;
        out.flush().ok();
        let plan = localpilot_selfdev::relaunch_plan(&program, &self.reload_args);
        match localpilot_selfdev::relaunch(&plan) {
            // Unix `exec` never returns on success; this arm is Windows, where the
            // successor was spawned and the parent must now exit to complete the swap.
            Ok(child) => {
                let status = wait_after_handoff(child, launched)?;
                std::process::exit(status.code().unwrap_or(0));
            }
            Err(error) => Err(StageError(format!("relaunch failed: {error}"))),
        }
    }
}

fn wait_after_handoff(
    mut child: std::process::Child,
    launched: &mut dyn FnMut(),
) -> Result<std::process::ExitStatus, StageError> {
    launched();
    child.wait().map_err(|error| {
        StageError(format!(
            "successor launched; waiting for it failed: {error}"
        ))
    })
}

/// The persisted loop state — the resume record, written under
/// `<root>/.localpilot/selfimprove/state.json` (`.localpilot/` is git-ignored, so
/// it never dirties `git status`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopState {
    /// The stage the loop is parked at.
    pub stage: Stage,
    /// The proposal id (its branch), once one is proposed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
    /// The built version label, once the approved tree is built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_label: Option<String>,
    /// The human reviewer who crossed the gate, once approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
}

/// The result of a Found → Proposed step: everything a caller needs to render the
/// proposal for human review before the gate.
#[derive(Debug, Clone)]
pub struct Proposed {
    /// The proposal id (its branch name).
    pub id: String,
    /// The project-relative files the change touched.
    pub files: Vec<String>,
    /// Lines added.
    pub insertions: u64,
    /// Lines removed.
    pub deletions: u64,
    /// The worktree holding the change, for human inspection.
    pub worktree: PathBuf,
    /// The unified diff (already bounded by patchgen).
    pub patch: String,
}

/// The thin sequencing layer over the four stage crates.
pub struct Orchestrator<S: SelfDevStage> {
    root: PathBuf,
    selfdev_root: PathBuf,
    selfdev: S,
}

impl<S: SelfDevStage> Orchestrator<S> {
    /// Open the loop for the repository at `root`, with `selfdev_root` the per-user
    /// self-dev data root and `selfdev` the build/reload stage.
    #[must_use]
    pub fn open(root: impl Into<PathBuf>, selfdev_root: impl Into<PathBuf>, selfdev: S) -> Self {
        Self {
            root: root.into(),
            selfdev_root: selfdev_root.into(),
            selfdev,
        }
    }

    fn state_path(&self) -> PathBuf {
        loop_state_path(&self.root)
    }

    /// The persisted loop state, or `None` when no loop is active.
    ///
    /// # Errors
    /// [`LoopError::Io`] / [`LoopError::Serde`] if the record exists but cannot be
    /// read or parsed.
    pub fn state(&self) -> Result<Option<LoopState>, LoopError> {
        read_loop_state(&self.root)
    }

    fn write_state(&self, state: &LoopState) -> Result<(), LoopError> {
        write_loop_state(&self.root, state)
    }

    /// Clear the loop so a fresh one can start. Idempotent.
    ///
    /// # Errors
    /// [`LoopError::Io`] if a present record cannot be removed.
    pub fn reset(&self) -> Result<(), LoopError> {
        let path = self.state_path();
        if !path.try_exists().map_err(|source| LoopError::Io {
            path: path.clone(),
            source,
        })? {
            return Ok(());
        }
        let mut authority = RepositoryAuthority::open(&self.root)?;
        let transaction = authority.try_transaction()?;
        transaction.require_idle()?;
        clear_loop_state(&self.root)
    }

    /// The read-only self-review that surfaces the [`Stage::Found`] candidates.
    /// A pure pass-through to `localpilot_selfreview::review`; it writes nothing.
    #[must_use]
    pub fn review(&self, options: &ReviewOptions) -> Report {
        review(&self.root, options)
    }

    /// **Found → Proposed.** Package `proposal` in an isolated worktree on `branch`
    /// (via `localpilot_patchgen::propose`), leave it on disk for review, and park
    /// the loop at [`Stage::Proposed`]. A fresh loop is required — a completed
    /// (`Reloaded`) loop is replaced only after the new proposal is persisted;
    /// any other active loop is an error.
    ///
    /// # Errors
    /// [`LoopError::OutOfOrder`] if a loop is already mid-flight; [`LoopError::Patch`]
    /// if the proposal is rejected (scope, provenance, git); [`LoopError::Io`] /
    /// [`LoopError::Serde`] if the state cannot be persisted.
    pub fn propose(
        &self,
        branch: &str,
        proposal: &PatchProposal,
        provenance: ChangeProvenance,
    ) -> Result<Proposed, LoopError> {
        let mut authority = RepositoryAuthority::open(&self.root)?;
        let transaction = authority.try_transaction()?;
        transaction.require_idle()?;
        match self.state()? {
            None => {}
            Some(state) if state.stage == Stage::Reloaded => {}
            Some(state) => {
                return Err(LoopError::OutOfOrder {
                    actual: state.stage,
                    attempted: "propose",
                })
            }
        }

        let patch = propose(&self.root, branch, proposal, provenance)?;
        let id = patch.id().to_string();
        let summary = patch.diff_summary().clone();
        let worktree = patch.worktree_path().to_path_buf();
        // Leave the proposal on disk so a later process can reopen it to promote or
        // discard — the human gate happens between processes, not in one call.
        patch.persist()?;

        self.write_state(&LoopState {
            stage: Stage::Proposed,
            proposal_id: Some(id.clone()),
            built_label: None,
            reviewer: None,
        })?;

        Ok(Proposed {
            id,
            files: summary.files,
            insertions: summary.insertions,
            deletions: summary.deletions,
            worktree,
            patch: summary.patch,
        })
    }

    /// Approve an exact locally validated review through the common locked
    /// service. The host supplies separate explicit promotion consent. This
    /// API retains the common guard through source and loop-state persistence.
    ///
    /// # Errors
    /// Missing/changed active loop, stale evidence/consent, unfinished authority
    /// or a failed source/state/journal checkpoint.
    pub fn approve(
        &self,
        review: &crate::PromotionReview,
        token: localpilot_patchgen::validation::PatchApprovalToken,
    ) -> Result<PromoteOutcome, LoopError> {
        crate::promotion::promote_for_loop(
            &self.root,
            &review.preflight.proposal.identity.proposal_id,
            review,
            token,
            true,
        )
    }

    /// **Approved → Built.** Build the *approved, promoted* tree (the workspace
    /// root — never the proposal worktree) via the self-dev stage, guarded by the
    /// reused rollback circuit breaker.
    ///
    /// # Errors
    /// [`LoopError::OutOfOrder`] if not at `Approved`; [`LoopError::Build`] if the
    /// breaker is tripped or the build/gauntlet fails.
    pub fn build(&mut self, out: &mut dyn Write) -> Result<BuildRecord, LoopError> {
        self.require_stage(Stage::Approved, "build")?;
        let mut authority = RepositoryAuthority::open(&self.root)?;
        let transaction = authority.try_transaction()?;
        transaction.require_idle()?;
        let mut state = self.require_stage(Stage::Approved, "build")?;
        let breaker = AutoReloadBreaker::new(&self.selfdev_root, AUTO_RELOAD_LIMIT);
        if breaker.is_tripped() {
            return Err(LoopError::Build(format!(
                "self-dev circuit breaker tripped after {AUTO_RELOAD_LIMIT} failed attempts; \
                 clear it with a successful `selfdev reload`"
            )));
        }
        // The approved tree is the workspace root — the sequencing-correctness
        // guarantee: never the patchgen worktree/branch.
        match self.selfdev.build_and_promote(&self.root, out) {
            Ok(record) => {
                let _ = breaker.reset();
                state.stage = Stage::Built;
                state.built_label = Some(record.label.clone());
                self.write_state(&state)?;
                Ok(record)
            }
            Err(error) => {
                // Reuse the existing breaker: a failed build counts toward the bound.
                let _ = breaker.record_attempt();
                Err(LoopError::Build(error.0))
            }
        }
    }

    /// **Built → Reloaded.** Swap the running process onto the built binary. The
    /// state is marked `Reloaded` *before* the swap, because `relaunch` replaces
    /// the process and never returns on success — so the successor finds the loop
    /// already complete and never re-reloads. A swap that fails to start rolls the
    /// state back to `Built`.
    ///
    /// # Errors
    /// [`LoopError::OutOfOrder`] if not at `Built`; [`LoopError::Build`] if the
    /// breaker is tripped or the swap could not be started.
    pub fn reload(&mut self, out: &mut dyn Write) -> Result<(), LoopError> {
        self.require_stage(Stage::Built, "reload")?;
        let mut authority = RepositoryAuthority::open(&self.root)?;
        let mut transaction = Some(authority.try_transaction()?);
        if let Some(transaction) = &transaction {
            transaction.require_idle()?;
        }
        let mut state = self.require_stage(Stage::Built, "reload")?;
        let label = state.built_label.clone().ok_or(LoopError::NoActiveLoop {
            attempted: "reload",
        })?;
        let breaker = AutoReloadBreaker::new(&self.selfdev_root, AUTO_RELOAD_LIMIT);
        if breaker.is_tripped() {
            return Err(LoopError::Build(format!(
                "self-dev circuit breaker tripped after {AUTO_RELOAD_LIMIT} failed attempts; \
                 clear it with a successful `selfdev reload`"
            )));
        }
        // Mark complete before the (non-returning) swap; undo if it never starts.
        let built = BuildRecord { label };
        state.stage = Stage::Reloaded;
        self.write_state(&state)?;
        match self.selfdev.reload_with_handoff(&built, out, &mut || {
            drop(transaction.take());
        }) {
            Ok(()) => {
                let _ = breaker.reset();
                Ok(())
            }
            Err(error) => {
                let _ = breaker.record_attempt();
                if transaction.is_some() {
                    state.stage = Stage::Built;
                    self.write_state(&state)?;
                    Err(LoopError::Build(error.0))
                } else {
                    Err(LoopError::Build(format!(
                        "{}; successor was launched: inspect it before another reload",
                        error.0
                    )))
                }
            }
        }
    }

    fn require_stage(&self, want: Stage, attempted: &'static str) -> Result<LoopState, LoopError> {
        match self.state()? {
            Some(state) if state.stage == want => Ok(state),
            Some(state) => Err(LoopError::OutOfOrder {
                actual: state.stage,
                attempted,
            }),
            None => Err(LoopError::NoActiveLoop { attempted }),
        }
    }
}

pub(crate) fn loop_state_path(root: &Path) -> PathBuf {
    root.join(".localpilot")
        .join("selfimprove")
        .join("state.json")
}

pub(crate) fn clear_loop_state(root: &Path) -> Result<(), LoopError> {
    let path = loop_state_path(root);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(LoopError::Io { path, source }),
    }
}

pub(crate) fn read_loop_state(root: &Path) -> Result<Option<LoopState>, LoopError> {
    let path = loop_state_path(root);
    match std::fs::read_to_string(&path) {
        Ok(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|e| LoopError::Serde(e.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(LoopError::Io { path, source }),
    }
}

pub(crate) fn write_loop_state(root: &Path, state: &LoopState) -> Result<(), LoopError> {
    let path = loop_state_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| LoopError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let json = serde_json::to_vec_pretty(state).map_err(|e| LoopError::Serde(e.to_string()))?;
    localpilot_store::atomic_write(&path, &json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use localpilot_patchgen::ProposedEdit;
    use std::cell::RefCell;
    use std::sync::Mutex;

    /// A spy self-dev stage that records the path it was asked to build, and can be
    /// told to fail — so the orchestrator's sequencing and breaker reuse are tested
    /// without a real cargo build or a process swap.
    #[derive(Default)]
    struct SpySelfDev {
        built_paths: RefCell<Vec<PathBuf>>,
        fail: bool,
    }

    impl SelfDevStage for SpySelfDev {
        fn build_and_promote(
            &mut self,
            approved_root: &Path,
            _out: &mut dyn Write,
        ) -> Result<BuildRecord, StageError> {
            self.built_paths
                .borrow_mut()
                .push(approved_root.to_path_buf());
            if self.fail {
                return Err(StageError("build failed (exit 1): spy".to_string()));
            }
            Ok(BuildRecord {
                label: "spy-1".to_string(),
            })
        }

        fn reload_onto(
            &mut self,
            _built: &BuildRecord,
            _out: &mut dyn Write,
        ) -> Result<(), StageError> {
            Ok(())
        }
    }

    /// Serialize the git-repo tests: `propose` runs `git` in a temp tree, and
    /// worktree operations under one process must not interleave surprisingly.
    static GIT: Mutex<()> = Mutex::new(());

    fn git(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// A fresh git repo with a committed file carrying a stale TODO — a real
    /// self-review finding to drive the loop.
    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        std::fs::write(root.join(".gitignore"), ".localpilot/\n").unwrap();
        std::fs::write(
            root.join("worker.rs"),
            "pub fn run() {}\n// TODO: handle retries\n",
        )
        .unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "init"]);
        dir
    }

    fn provenance() -> ChangeProvenance {
        ChangeProvenance::new(
            "drop the stale TODO in worker.rs",
            "test-model",
            "the TODO is stale",
        )
    }

    fn trivial_proposal() -> PatchProposal {
        PatchProposal::new(
            "stale TODO in worker.rs",
            vec!["worker.rs".to_string()],
            vec![ProposedEdit::new("worker.rs", "pub fn run() {}\n")],
        )
    }

    /// The orchestrator sequences Found → Proposed: `review` surfaces the finding,
    /// then `propose` packages it, and the loop parks at `Proposed`.
    #[test]
    fn advances_found_to_proposed_by_review_then_propose() {
        let _guard = GIT.lock().unwrap();
        let repo = init_repo();
        let root = repo.path();
        let selfdev = tempfile::tempdir().unwrap();
        let orch = Orchestrator::open(root, selfdev.path(), SpySelfDev::default());

        // Found: review is read-only and surfaces the TODO.
        let report = orch.review(&ReviewOptions::default());
        assert!(
            !report.findings.is_empty(),
            "review should find the stale TODO"
        );
        assert!(orch.state().unwrap().is_none(), "no loop is active yet");

        // Proposed: propose packages the change and parks the loop.
        let proposed = orch
            .propose("self-improve-1", &trivial_proposal(), provenance())
            .unwrap();
        assert_eq!(proposed.files, vec!["worker.rs".to_string()]);
        let state = orch.state().unwrap().expect("a loop is now active");
        assert_eq!(state.stage, Stage::Proposed);
        assert_eq!(state.proposal_id.as_deref(), Some("self-improve-1"));
    }

    /// At the gate the orchestrator stops: it cannot advance to `Built` without
    /// approval, it performs no promotion side-effect, and it never mints a token
    /// (there is no method that does).
    #[test]
    fn stops_at_the_gate_with_no_promotion_side_effect() {
        let _guard = GIT.lock().unwrap();
        let repo = init_repo();
        let root = repo.path();
        let before = std::fs::read_to_string(root.join("worker.rs")).unwrap();
        let selfdev = tempfile::tempdir().unwrap();
        let mut orch = Orchestrator::open(root, selfdev.path(), SpySelfDev::default());

        orch.propose("self-improve-1", &trivial_proposal(), provenance())
            .unwrap();

        // Advancing past the gate without approving is an ordering error — build is
        // not the next step at Proposed.
        let err = orch.build(&mut std::io::sink()).unwrap_err();
        assert!(matches!(
            err,
            LoopError::OutOfOrder {
                actual: Stage::Proposed,
                ..
            }
        ));
        // The main branch is untouched: no promotion happened.
        assert_eq!(
            std::fs::read_to_string(root.join("worker.rs")).unwrap(),
            before
        );
        assert_eq!(orch.state().unwrap().unwrap().stage, Stage::Proposed);
    }

    /// After approval + merge, self-dev builds the **approved/merged tree** (the
    /// workspace root), never the patchgen worktree — the sequencing-correctness
    /// guarantee.
    #[test]
    fn builds_the_approved_tree_not_the_proposal_worktree() {
        let _guard = GIT.lock().unwrap();
        let repo = init_repo();
        let root = repo.path().to_path_buf();
        let selfdev = tempfile::tempdir().unwrap();
        let mut orch = Orchestrator::open(&root, selfdev.path(), SpySelfDev::default());

        let proposed = orch
            .propose("self-improve-1", &trivial_proposal(), provenance())
            .unwrap();
        let worktree = proposed.worktree.clone();

        // Cross the gate with a human-minted token (the test is the human here).
        // Test setup starts this build-only fixture after source approval. The
        // real validation/promotion/state flow is exercised by promotion tests.
        git(&root, &["merge", "--ff-only", &proposed.id]);
        let mut state = orch.state().unwrap().unwrap();
        state.stage = Stage::Approved;
        state.reviewer = Some("fixture setup".into());
        orch.write_state(&state).unwrap();
        assert_eq!(orch.state().unwrap().unwrap().stage, Stage::Approved);
        // The fix reached main.
        assert!(!std::fs::read_to_string(root.join("worker.rs"))
            .unwrap()
            .contains("TODO"));

        // Build: the spy records the path it was handed.
        let record = orch.build(&mut std::io::sink()).unwrap();
        assert_eq!(record.label, "spy-1");
        assert_eq!(orch.state().unwrap().unwrap().stage, Stage::Built);

        let built_paths = orch.selfdev.built_paths.borrow();
        assert_eq!(built_paths.len(), 1);
        let built = &built_paths[0];
        assert_eq!(
            built, &root,
            "self-dev must build the promoted workspace root"
        );
        assert_ne!(
            built, &worktree,
            "self-dev must NOT build the proposal worktree"
        );
        assert!(
            !built.starts_with(root.join(".localpilot")),
            "the built path must be the tree root, not an isolated worktree"
        );
    }

    /// An existing proposal without passing local execution cannot be reviewed
    /// as approvable; its stage remains Proposed.
    #[test]
    fn approve_requires_locally_completed_validation() {
        let _guard = GIT.lock().unwrap();
        let repo = init_repo();
        let selfdev = tempfile::tempdir().unwrap();
        let orch = Orchestrator::open(repo.path(), selfdev.path(), SpySelfDev::default());
        orch.propose("self-improve-1", &trivial_proposal(), provenance())
            .unwrap();

        let err = crate::promotion_review(repo.path(), "self-improve-1").unwrap_err();
        assert!(matches!(
            err,
            LoopError::Validation(
                localpilot_patchgen::validation::ValidationError::NotValidated(_)
            )
        ));
        assert_eq!(orch.state().unwrap().unwrap().stage, Stage::Proposed);
    }

    fn seeded_stage(stage: Stage) -> LoopState {
        LoopState {
            stage,
            proposal_id: Some("controlled".into()),
            built_label: Some("fixture-build".into()),
            reviewer: Some("fixture owner".into()),
        }
    }

    #[test]
    fn every_state_writer_refuses_a_competing_common_transaction() {
        use localpilot_patchgen::validation::ValidationError;
        let repo = init_repo();
        let selfdev = tempfile::tempdir().unwrap();
        let mut orch = Orchestrator::open(repo.path(), selfdev.path(), SpySelfDev::default());
        orch.write_state(&seeded_stage(Stage::Proposed)).unwrap();
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let _held = authority.try_transaction().unwrap();
        assert!(matches!(
            orch.reset(),
            Err(LoopError::Validation(ValidationError::Busy))
        ));
        assert!(matches!(
            orch.propose("blocked", &trivial_proposal(), provenance()),
            Err(LoopError::Validation(ValidationError::Busy))
        ));
        for stage in [Stage::Approved, Stage::Built] {
            let state = seeded_stage(stage);
            orch.write_state(&state).unwrap(); // fixture setup, not a production writer
            let result = if stage == Stage::Approved {
                orch.build(&mut Vec::new()).map(|_| ())
            } else {
                orch.reload(&mut Vec::new())
            };
            assert!(matches!(
                result,
                Err(LoopError::Validation(ValidationError::Busy))
            ));
            assert_eq!(orch.state().unwrap().unwrap(), state);
        }
        assert!(orch.selfdev.built_paths.borrow().is_empty());
    }

    #[test]
    fn pending_authority_blocks_reset_propose_build_and_reload() {
        use localpilot_patchgen::validation::{OutwardApprovalToken, ValidationError};
        let repo = init_repo();
        let selfdev = tempfile::tempdir().unwrap();
        let mut orch = Orchestrator::open(repo.path(), selfdev.path(), SpySelfDev::default());
        orch.write_state(&seeded_stage(Stage::Proposed)).unwrap();
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let mut tx = authority.try_transaction().unwrap();
        let draft = localpilot_patchgen::OutwardDraft {
            schema: localpilot_patchgen::OUTWARD_SCHEMA.into(),
            id: "pending".into(),
            kind: localpilot_patchgen::OutwardKind::Issue,
            target_repo: "fixture/repo".into(),
            title: "fixture".into(),
            body: "fixture body".into(),
            head_branch: None,
        };
        let token = OutwardApprovalToken::approve(
            &tx.repository_identity(),
            &draft,
            "fixture-account",
            "owner",
            "controlled pending effect",
        )
        .unwrap();
        tx.reserve_outward(token, &draft, "fixture-account")
            .unwrap();
        drop(tx);
        assert!(matches!(
            orch.reset(),
            Err(LoopError::Validation(ValidationError::RecoveryRequired))
        ));
        assert!(matches!(
            orch.propose("blocked", &trivial_proposal(), provenance()),
            Err(LoopError::Validation(ValidationError::RecoveryRequired))
        ));
        for stage in [Stage::Approved, Stage::Built] {
            let state = seeded_stage(stage);
            orch.write_state(&state).unwrap();
            let result = if stage == Stage::Approved {
                orch.build(&mut Vec::new()).map(|_| ())
            } else {
                orch.reload(&mut Vec::new())
            };
            assert!(matches!(
                result,
                Err(LoopError::Validation(ValidationError::RecoveryRequired))
            ));
            assert_eq!(orch.state().unwrap().unwrap(), state);
        }
        assert!(orch.selfdev.built_paths.borrow().is_empty());
    }

    struct HandoffStage {
        root: PathBuf,
        after_launch: bool,
    }
    impl SelfDevStage for HandoffStage {
        fn build_and_promote(
            &mut self,
            _: &Path,
            _: &mut dyn Write,
        ) -> Result<BuildRecord, StageError> {
            unreachable!("reload-only fixture")
        }
        fn reload_onto(&mut self, _: &BuildRecord, _: &mut dyn Write) -> Result<(), StageError> {
            unreachable!("handoff adapter required")
        }
        fn reload_with_handoff(
            &mut self,
            _: &BuildRecord,
            _: &mut dyn Write,
            launched: &mut dyn FnMut(),
        ) -> Result<(), StageError> {
            use localpilot_patchgen::validation::ValidationError;
            let mut competing = RepositoryAuthority::open(&self.root).unwrap();
            assert!(matches!(
                competing.try_transaction(),
                Err(ValidationError::Busy)
            ));
            assert_eq!(
                read_loop_state(&self.root).unwrap().unwrap().stage,
                Stage::Reloaded
            );
            if self.after_launch {
                launched();
                let _held = competing.try_transaction().unwrap();
                Err(StageError("fixture: wait failed after launch".into()))
            } else {
                Err(StageError("fixture: launch failed".into()))
            }
        }
    }

    #[test]
    fn reload_failure_rolls_back_only_when_no_successor_was_launched() {
        for after_launch in [false, true] {
            let repo = init_repo();
            let selfdev = tempfile::tempdir().unwrap();
            let mut orch = Orchestrator::open(
                repo.path(),
                selfdev.path(),
                HandoffStage {
                    root: repo.path().into(),
                    after_launch,
                },
            );
            orch.write_state(&seeded_stage(Stage::Built)).unwrap();
            let error = orch.reload(&mut Vec::new()).unwrap_err();
            assert_eq!(
                orch.state().unwrap().unwrap().stage,
                if after_launch {
                    Stage::Reloaded
                } else {
                    Stage::Built
                }
            );
            assert_eq!(
                error.to_string().contains("successor was launched"),
                after_launch
            );
        }
    }

    #[test]
    #[ignore = "subprocess helper for the reload handoff test"]
    fn reload_handoff_worker() {
        let root =
            std::path::PathBuf::from(std::env::var_os("LOCALPILOT_TEST_HANDOFF_ROOT").unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let mut authority = RepositoryAuthority::open(&root).unwrap();
            if let Ok(_held) = authority.try_transaction() {
                std::fs::create_dir_all(root.join(".localpilot")).unwrap();
                std::fs::write(
                    root.join(".localpilot/handoff-observed"),
                    "successor acquired the repository lock",
                )
                .unwrap();
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "parent retained lock while waiting for successor"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn spawned_successor_can_acquire_the_lock_before_parent_wait_completes() {
        let repo = init_repo();
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let mut held = Some(authority.try_transaction().unwrap());
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "orchestrator::tests::reload_handoff_worker",
                "--ignored",
                "--nocapture",
            ])
            .env("LOCALPILOT_TEST_HANDOFF_ROOT", repo.path())
            .spawn()
            .unwrap();
        let status = wait_after_handoff(child, &mut || {
            drop(held.take());
        })
        .unwrap();
        assert!(status.success());
        assert!(repo.path().join(".localpilot/handoff-observed").is_file());
    }
}
