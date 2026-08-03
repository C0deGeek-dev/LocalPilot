//! Guardrails that hold the loop's safety properties *by construction*, not by
//! convention. They exist because the four stages were deliberately left as
//! islands: a human approves every source change, and the unattended autonomous
//! build→reload loop stays deferred (ADR-0128). This module pins those properties
//! with architecture tests over the shipped source, so a future edit that
//! reintroduces a token mint or a self-advancing loop fails the build.
//!
//! The properties:
//!
//! 1. **No unattended loop** — the orchestrator exposes only single-step advance;
//!    no shipped code path auto-mints an `ApprovalToken` or loops build→reload.
//! 2. **Offline evidence is the bar** — a self-dev advance is reachable only after
//!    the human gate, so it can never auto-satisfy a benchmark/live requirement
//!    (offline evidence is the bar; live runs are opportunistic, never blocking).
//! 3. **Rollback is reused** — a failed self-dev advance drives the *existing*
//!    self-dev circuit breaker; the orchestrator adds no parallel rollback logic.
//! 4. **`ApprovalToken` bypass is impossible** — exactly one promote path exists,
//!    and it takes a token the orchestrator cannot construct.

/// The portion of a source file that ships in the binary — everything before its
/// unit-test module. The guardrail scans run against this so a token mint or a
/// loop *in a test* never masks (or falsely trips) a check on shipped code.
#[cfg(test)]
fn shipped(source: &str) -> &str {
    match source.find("#[cfg(test)]") {
        Some(cut) => &source[..cut],
        None => source,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::shipped;
    use crate::contract::{unattended_next, AwaitingApproval, Stage};
    use crate::orchestrator::{BuildRecord, LoopState, Orchestrator, SelfDevStage, StageError};
    use localpilot_selfdev::AutoReloadBreaker;
    use std::io::Write;
    use std::path::Path;

    const ORCHESTRATOR_SRC: &str = include_str!("orchestrator.rs");
    const CONTRACT_SRC: &str = include_str!("contract.rs");

    /// No unattended loop: the shipped orchestrator mints no token and holds no
    /// self-advancing loop. ADR-0128 keeps the autonomous loop deferred; this is
    /// the guardrail that keeps it deferred.
    #[test]
    fn shipped_orchestrator_has_no_token_mint_and_no_auto_loop() {
        let src = shipped(ORCHESTRATOR_SRC);
        // Match the *call* form (an open paren after the name); a doc reference in
        // backticks — the legitimate way to say where minting happens — never has one.
        assert!(
            !src.contains("ApprovalToken::approve("),
            "the orchestrator must never mint an approval token"
        );
        assert!(
            !shipped(CONTRACT_SRC).contains("ApprovalToken::approve("),
            "the contract must never mint an approval token"
        );
        assert!(
            !src.contains("loop {"),
            "the orchestrator must expose only single-step advance — no loop"
        );
        // No advance method internally chains another advance (single-step only).
        for chain in [
            "self.propose(",
            "self.approve(",
            "self.build(",
            "self.reload(",
        ] {
            assert!(
                !src.contains(chain),
                "single-step only: an advance must not internally call {chain}"
            );
        }
    }

    /// The gate has no tokenless successor (type-level): an unattended step at
    /// `Proposed` is a hard stop, representable only as `AwaitingApproval`.
    #[test]
    fn the_gate_has_no_unattended_successor() {
        assert_eq!(unattended_next(Stage::Proposed), Err(AwaitingApproval));
    }

    /// A spy that always fails its build, to exercise the breaker without a real
    /// cargo build.
    struct FailingSelfDev;
    impl SelfDevStage for FailingSelfDev {
        fn build_and_promote(
            &mut self,
            _root: &Path,
            _out: &mut dyn Write,
        ) -> Result<BuildRecord, StageError> {
            Err(StageError("build failed (exit 1): forced".to_string()))
        }
        fn reload_onto(
            &mut self,
            _built: &BuildRecord,
            _out: &mut dyn Write,
        ) -> Result<(), StageError> {
            Err(StageError("relaunch failed: forced".to_string()))
        }
    }

    /// Force the loop into a given state without running git, so the breaker
    /// behaviour is tested in isolation.
    fn seed_state(root: &Path, state: &LoopState) {
        let dir = root.join(".localpilot").join("selfimprove");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("state.json"),
            serde_json::to_string_pretty(state).unwrap(),
        )
        .unwrap();
    }

    /// Offline evidence is the bar: a self-dev advance cannot run until the loop
    /// has crossed the human gate. With no active loop (no manual approval),
    /// `build` refuses — so a live/self-dev run can never be an auto-fulfilled
    /// precondition; live runs stay opportunistic, never blocking.
    #[test]
    fn a_self_dev_advance_is_unreachable_without_the_manual_gate() {
        let repo = tempfile::tempdir().unwrap();
        let selfdev = tempfile::tempdir().unwrap();
        let mut orch = Orchestrator::open(repo.path(), selfdev.path(), FailingSelfDev);
        // No approval has happened → no active loop → build is not reachable.
        let err = orch.build(&mut std::io::sink()).unwrap_err();
        assert!(
            err.to_string().contains("no active self-improvement loop"),
            "a self-dev advance must require the manual gate first: {err}"
        );
        // And nothing touched the breaker — no attempt was made.
        assert_eq!(AutoReloadBreaker::new(selfdev.path(), 3).count(), 0);
    }

    /// Reuse the self-dev rollback circuit breaker: a failed build during an
    /// orchestrated advance drives the *existing* `AutoReloadBreaker`, and repeated
    /// failures trip it. The orchestrator adds no rollback logic of its own.
    #[test]
    fn a_failed_build_drives_the_existing_circuit_breaker() {
        let repo = tempfile::tempdir().unwrap();
        let selfdev = tempfile::tempdir().unwrap();
        seed_state(
            repo.path(),
            &LoopState {
                stage: Stage::Approved,
                proposal_id: Some("p".to_string()),
                built_label: None,
                reviewer: Some("tester".to_string()),
            },
        );
        let mut orch = Orchestrator::open(repo.path(), selfdev.path(), FailingSelfDev);
        let breaker = AutoReloadBreaker::new(selfdev.path(), 3);

        for expected in 1..=3 {
            // The loop stays at Approved on each failure (no forward progress).
            seed_state(
                repo.path(),
                &LoopState {
                    stage: Stage::Approved,
                    proposal_id: Some("p".to_string()),
                    built_label: None,
                    reviewer: Some("tester".to_string()),
                },
            );
            let err = orch.build(&mut std::io::sink()).unwrap_err();
            assert!(err.to_string().contains("self-dev stage failed"));
            assert_eq!(breaker.count(), expected, "each failed build counts once");
        }
        assert!(breaker.is_tripped(), "three failed builds trip the breaker");

        // Once tripped, a further advance is refused by the breaker, not retried.
        let err = orch.build(&mut std::io::sink()).unwrap_err();
        assert!(err.to_string().contains("circuit breaker tripped"));
    }

    /// `ApprovalToken` bypass is impossible by construction: the shipped
    /// orchestrator has exactly one promote path, and it cannot construct a token.
    #[test]
    fn exactly_one_promote_path_and_no_forgeable_token() {
        let src = shipped(ORCHESTRATOR_SRC);
        let promotes = src.matches(".promote(").count();
        assert_eq!(
            promotes, 1,
            "there must be exactly one promote path in the orchestrator"
        );
        // The single promote path takes a token by reference; the orchestrator
        // never mints one (checked above), so it cannot forge a valid token — it
        // can only be handed one by the human-confirmation path.
        assert!(src.contains("fn approve(&self, token: &ApprovalToken)"));
    }
}
