//! The thin orchestrator: it sequences the four self-improvement stages by
//! calling their **existing** entrypoints in contract order, persists the loop
//! state so a step can be resumed in a later process, and **stops at the
//! `ApprovalToken` gate — never minting a token.** After approval it builds the
//! *approved, merged* tree, never the proposal worktree.
//!
//! It holds only sequencing and state. The stages stay in their own crates: the
//! read-only find (`localpilot-selfreview`), the human-gated source mutation
//! (`localpilot-patchgen`), and the binary lifecycle (`localpilot-selfdev`) are
//! different concerns with different blast radii, so this layer composes them
//! rather than fusing them.

use std::io::Write;
use std::path::{Path, PathBuf};

use localpilot_patchgen::{
    propose, ApprovalToken, ChangeProvenance, PatchProposal, PromoteOutcome, ProposedPatch,
};
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
            Ok(mut child) => {
                let status = child.wait().map_err(|error| {
                    StageError(format!("waiting for the reloaded process: {error}"))
                })?;
                std::process::exit(status.code().unwrap_or(0));
            }
            Err(error) => Err(StageError(format!("relaunch failed: {error}"))),
        }
    }
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
        self.root
            .join(".localpilot")
            .join("selfimprove")
            .join("state.json")
    }

    /// The persisted loop state, or `None` when no loop is active.
    ///
    /// # Errors
    /// [`LoopError::Io`] / [`LoopError::Serde`] if the record exists but cannot be
    /// read or parsed.
    pub fn state(&self) -> Result<Option<LoopState>, LoopError> {
        let path = self.state_path();
        match std::fs::read_to_string(&path) {
            Ok(json) => {
                let state =
                    serde_json::from_str(&json).map_err(|e| LoopError::Serde(e.to_string()))?;
                Ok(Some(state))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(LoopError::Io { path, source }),
        }
    }

    fn write_state(&self, state: &LoopState) -> Result<(), LoopError> {
        let path = self.state_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| LoopError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let json =
            serde_json::to_string_pretty(state).map_err(|e| LoopError::Serde(e.to_string()))?;
        std::fs::write(&path, json).map_err(|source| LoopError::Io { path, source })
    }

    /// Clear the loop so a fresh one can start. Idempotent.
    ///
    /// # Errors
    /// [`LoopError::Io`] if a present record cannot be removed.
    pub fn reset(&self) -> Result<(), LoopError> {
        let path = self.state_path();
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(LoopError::Io { path, source }),
        }
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
    /// (`Reloaded`) loop is cleared first; any other active loop is an error.
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
        match self.state()? {
            None => {}
            Some(state) if state.stage == Stage::Reloaded => self.reset()?,
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

    /// **Proposed → Approved (THE GATE).** Promote the proposed patch onto the main
    /// branch. The `token` must be handed in by a human-confirmation path; this
    /// method never constructs one, and `promote` itself rejects a token that does
    /// not authorize the proposal.
    ///
    /// # Errors
    /// [`LoopError::AwaitingApproval`]-adjacent ordering errors if not at
    /// `Proposed`; [`LoopError::Patch`] (e.g. `TokenMismatch`, `DirtyTarget`,
    /// `NotFastForward`) if the promotion is refused.
    pub fn approve(&self, token: &ApprovalToken) -> Result<PromoteOutcome, LoopError> {
        let mut state = self.require_stage(Stage::Proposed, "approve")?;
        let id = state.proposal_id.clone().ok_or(LoopError::NoActiveLoop {
            attempted: "approve",
        })?;
        let patch = ProposedPatch::reopen(&self.root, &id)?;
        let outcome = patch.promote(token)?;
        state.stage = Stage::Approved;
        state.reviewer = Some(outcome.reviewer.clone());
        self.write_state(&state)?;
        Ok(outcome)
    }

    /// **Approved → Built.** Build the *approved, promoted* tree (the workspace
    /// root — never the proposal worktree) via the self-dev stage, guarded by the
    /// reused rollback circuit breaker.
    ///
    /// # Errors
    /// [`LoopError::OutOfOrder`] if not at `Approved`; [`LoopError::Build`] if the
    /// breaker is tripped or the build/gauntlet fails.
    pub fn build(&mut self, out: &mut dyn Write) -> Result<BuildRecord, LoopError> {
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
        match self.selfdev.reload_onto(&built, out) {
            Ok(()) => {
                let _ = breaker.reset();
                Ok(())
            }
            Err(error) => {
                state.stage = Stage::Built;
                let _ = self.write_state(&state);
                let _ = breaker.record_attempt();
                Err(LoopError::Build(error.0))
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use localpilot_patchgen::{PatchError, ProposedEdit};
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
        let token = ApprovalToken::approve(&proposed.id, "tester");
        orch.approve(&token).unwrap();
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

    /// The gate rejects a token that authorizes a different patch — the real
    /// `PatchError::TokenMismatch` surfaces, and the loop stays at `Proposed`.
    #[test]
    fn approve_rejects_a_mismatched_token() {
        let _guard = GIT.lock().unwrap();
        let repo = init_repo();
        let selfdev = tempfile::tempdir().unwrap();
        let orch = Orchestrator::open(repo.path(), selfdev.path(), SpySelfDev::default());
        orch.propose("self-improve-1", &trivial_proposal(), provenance())
            .unwrap();

        let wrong = ApprovalToken::approve("some-other-patch", "tester");
        let err = orch.approve(&wrong).unwrap_err();
        assert!(matches!(err, LoopError::Patch(PatchError::TokenMismatch)));
        assert_eq!(orch.state().unwrap().unwrap().stage, Stage::Proposed);
    }
}
