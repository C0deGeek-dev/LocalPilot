//! The worker: step selection, the step-completion gate, and the anti-sunk-cost
//! retry/discard/replan loop.

use serde::{Deserialize, Serialize};

use crate::progress::{Progress, Step};
use crate::quality::CheckOutcome;
use crate::rules::{RuleContext, RuleEngine, Trigger, Verdict};

/// Select the next step to work on: the first incomplete step.
#[must_use]
pub fn select_next_step(progress: &Progress) -> Option<&Step> {
    progress.next_incomplete()
}

/// The result of one attempt at a step, derived from rule verdicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptResult {
    /// The step succeeded.
    Success,
    /// Keep context and retry, feeding back the reason.
    Retry(String),
    /// Reset the working tree and restart with fresh context.
    Discard(String),
}

/// What the anti-sunk-cost loop decides after an attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepDecision {
    /// Commit the completed step.
    Commit,
    /// Retry the same step keeping context, with the reason fed back.
    RetrySameContext(String),
    /// Discard the attempt, reset committed state, restart with fresh context.
    DiscardAndReset(String),
    /// Replan the step using the accumulated attempt logs.
    Replan(Vec<String>),
    /// Give up: the replan cap was reached.
    GiveUp,
}

/// The anti-sunk-cost loop for one step. `retry` keeps context; `discard` resets
/// it; after the per-step attempt cap, the step is replanned with the attempt
/// logs; replans are themselves capped to avoid runaway automation.
#[derive(Debug, Clone)]
pub struct StepLoop {
    max_attempts: u32,
    max_replans: u32,
    attempts: u32,
    replans: u32,
    attempt_logs: Vec<String>,
}

impl StepLoop {
    /// A loop with a per-step attempt cap and a replan cap.
    #[must_use]
    pub fn new(max_attempts: u32, max_replans: u32) -> Self {
        Self {
            max_attempts,
            max_replans,
            attempts: 0,
            replans: 0,
            attempt_logs: Vec::new(),
        }
    }

    /// The number of replans performed.
    #[must_use]
    pub fn replans(&self) -> u32 {
        self.replans
    }

    /// Record an attempt result and decide what to do next.
    pub fn on_attempt(&mut self, result: AttemptResult) -> StepDecision {
        let (reason, keep_context) = match result {
            AttemptResult::Success => return StepDecision::Commit,
            AttemptResult::Retry(reason) => (reason, true),
            AttemptResult::Discard(reason) => (reason, false),
        };

        self.attempts += 1;
        self.attempt_logs.push(reason.clone());

        if self.attempts >= self.max_attempts {
            // The per-step attempt budget is spent: replan, or give up if the
            // replan cap is reached.
            if self.replans >= self.max_replans {
                return StepDecision::GiveUp;
            }
            self.replans += 1;
            self.attempts = 0;
            return StepDecision::Replan(std::mem::take(&mut self.attempt_logs));
        }

        if keep_context {
            StepDecision::RetrySameContext(reason)
        } else {
            StepDecision::DiscardAndReset(reason)
        }
    }
}

/// Inputs to the step-completion gate.
#[derive(Debug, Clone, Default)]
pub struct CompletionInputs {
    pub tests_passed: Option<bool>,
    pub progress_reflects_completion: bool,
    pub commit_message: String,
    pub attempts: u32,
    pub max_attempts: u32,
}

/// The decision of the step-completion gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionDecision {
    /// All post-step rules passed; commit the step.
    Commit,
    /// A blocking rule fired; the reason is surfaced to model and user.
    Blocked(String),
}

/// The action the act-on-findings loop takes after evaluating a completed step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepAction {
    /// All post-step rules passed; commit the step.
    Commit,
    /// A check returned an actionable finding; feed the reason back and retry.
    Retry(String),
    /// A blocking rule fired (a failing test, a dirty commit message, an `audit`
    /// finding). No retry will clear it; surface the reason to model and user.
    Block(String),
}

/// Evaluate the post-step rules — including the `quality_gate` rule fed the
/// `gate_outcomes` for this step — and reduce them to one [`StepAction`]. A
/// `block` verdict wins over a `retry`, which wins over a clean commit; a
/// warning does not stop the step. A `discard` verdict is treated as a retry:
/// resetting context requires a fresh runtime, which the gate never demands
/// (its actionable findings are always `retry`).
#[must_use]
pub fn decide_step(
    engine: &RuleEngine,
    inputs: &CompletionInputs,
    gate_outcomes: Vec<CheckOutcome>,
) -> StepAction {
    let ctx = RuleContext {
        tests_passed: inputs.tests_passed,
        progress_reflects_completion: Some(inputs.progress_reflects_completion),
        commit_message: Some(inputs.commit_message.clone()),
        attempts: inputs.attempts,
        max_attempts: inputs.max_attempts,
        gate_outcomes,
        ..RuleContext::default()
    };

    let mut action = StepAction::Commit;
    for trigger in [Trigger::PostTest, Trigger::PreCommit, Trigger::StepComplete] {
        for (name, verdict) in engine.evaluate(trigger, &ctx) {
            let candidate = match verdict {
                Verdict::Allow | Verdict::Warn(_) => continue,
                Verdict::Retry(reason) | Verdict::Discard(reason) => StepAction::Retry(reason),
                Verdict::Block(reason) => StepAction::Block(format!("{name}: {reason}")),
            };
            if action_rank(&candidate) > action_rank(&action) {
                action = candidate;
            }
        }
    }
    action
}

fn action_rank(action: &StepAction) -> u8 {
    match action {
        StepAction::Commit => 0,
        StepAction::Retry(_) => 1,
        StepAction::Block(_) => 2,
    }
}

/// Run the post-step rules in order (tests, progress, commit message, attempts)
/// and decide whether the step may be committed. This is where `suite_green`
/// gates a commit on passing tests. A convenience wrapper over [`decide_step`]
/// for callers that gate on commit-or-block without the quality gate.
#[must_use]
pub fn evaluate_completion(engine: &RuleEngine, inputs: &CompletionInputs) -> CompletionDecision {
    match decide_step(engine, inputs, Vec::new()) {
        StepAction::Commit => CompletionDecision::Commit,
        StepAction::Retry(reason) | StepAction::Block(reason) => {
            CompletionDecision::Blocked(reason)
        }
    }
}

/// A trace event emitted during a worker step, instrumented via `tracing`. The
/// shape is snapshot-tested. Secret/large fields are never included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepTrace {
    pub step_number: usize,
    pub description: String,
    pub attempts: u32,
    pub committed: bool,
    pub commit: Option<String>,
}

impl StepTrace {
    /// Emit the trace via `tracing` at info level. Only structural metadata is
    /// recorded — never prompt text, tool output, or secrets.
    pub fn emit(&self) {
        tracing::info!(
            step = self.step_number,
            attempts = self.attempts,
            committed = self.committed,
            "harness step"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn engine() -> RuleEngine {
        RuleEngine::with_baseline(&IndexMap::new())
    }

    #[test]
    fn selects_the_first_incomplete_step() {
        let progress = Progress::parse(
            "# Progress: x\nBranch: feature/x\n\n## Steps\n\n- [x] 1. a\n- [ ] 2. b\n- [ ] 3. c\n",
        )
        .unwrap();
        assert_eq!(select_next_step(&progress).map(|s| s.number), Some(2));
    }

    #[test]
    fn retry_keeps_context_within_budget() {
        let mut loop_ = StepLoop::new(3, 1);
        assert_eq!(
            loop_.on_attempt(AttemptResult::Retry("again".into())),
            StepDecision::RetrySameContext("again".into())
        );
    }

    #[test]
    fn discard_resets_context_within_budget() {
        let mut loop_ = StepLoop::new(3, 1);
        assert_eq!(
            loop_.on_attempt(AttemptResult::Discard("reset".into())),
            StepDecision::DiscardAndReset("reset".into())
        );
    }

    #[test]
    fn repeated_failure_triggers_replan_with_attempt_logs() {
        let mut loop_ = StepLoop::new(2, 1);
        loop_.on_attempt(AttemptResult::Retry("first".into()));
        let decision = loop_.on_attempt(AttemptResult::Discard("second".into()));
        match decision {
            StepDecision::Replan(logs) => assert_eq!(logs, vec!["first", "second"]),
            other => panic!("expected Replan, got {other:?}"),
        }
        assert_eq!(loop_.replans(), 1);
    }

    #[test]
    fn replans_are_capped() {
        let mut loop_ = StepLoop::new(1, 1);
        // attempt 1 exhausts the per-step budget -> first replan.
        assert!(matches!(
            loop_.on_attempt(AttemptResult::Retry("a".into())),
            StepDecision::Replan(_)
        ));
        // next exhaustion exceeds the replan cap -> give up.
        assert_eq!(
            loop_.on_attempt(AttemptResult::Retry("b".into())),
            StepDecision::GiveUp
        );
    }

    #[test]
    fn success_commits() {
        let mut loop_ = StepLoop::new(3, 2);
        assert_eq!(
            loop_.on_attempt(AttemptResult::Success),
            StepDecision::Commit
        );
    }

    #[test]
    fn step_trace_shape_is_stable_and_carries_no_payload() {
        let trace = StepTrace {
            step_number: 2,
            description: "Implement parser errors".to_string(),
            attempts: 1,
            committed: true,
            commit: Some("abc1234".to_string()),
        };
        insta::assert_snapshot!(serde_json::to_string_pretty(&trace).unwrap());
    }

    #[test]
    fn completion_gate_blocks_a_commit_when_tests_fail() {
        let inputs = CompletionInputs {
            tests_passed: Some(false),
            progress_reflects_completion: true,
            commit_message: "harness: step 1".to_string(),
            attempts: 1,
            max_attempts: 3,
        };
        assert!(matches!(
            evaluate_completion(&engine(), &inputs),
            CompletionDecision::Blocked(_)
        ));
    }

    #[test]
    fn completion_gate_commits_when_all_rules_pass() {
        let inputs = CompletionInputs {
            tests_passed: Some(true),
            progress_reflects_completion: true,
            commit_message: "harness: step 1".to_string(),
            attempts: 1,
            max_attempts: 3,
        };
        assert_eq!(
            evaluate_completion(&engine(), &inputs),
            CompletionDecision::Commit
        );
    }

    #[test]
    fn completion_gate_blocks_a_secret_bearing_commit_message() {
        let inputs = CompletionInputs {
            tests_passed: Some(true),
            progress_reflects_completion: true,
            commit_message: "add sk-abcdefghijklmnopqrstuvwxyz0123".to_string(),
            attempts: 1,
            max_attempts: 3,
        };
        assert!(matches!(
            evaluate_completion(&engine(), &inputs),
            CompletionDecision::Blocked(_)
        ));
    }

    fn clean_inputs() -> CompletionInputs {
        CompletionInputs {
            tests_passed: Some(true),
            progress_reflects_completion: true,
            commit_message: "harness: step 1".to_string(),
            attempts: 1,
            max_attempts: 3,
        }
    }

    fn gate_outcome(
        name: &str,
        status: crate::quality::CheckStatus,
        severity: Option<unshackled_config::RuleSeverity>,
    ) -> CheckOutcome {
        CheckOutcome {
            name: name.to_string(),
            status,
            detail: String::new(),
            fixed: false,
            severity,
        }
    }

    #[test]
    fn decide_step_commits_when_tests_and_gate_pass() {
        use crate::quality::CheckStatus;
        let action = decide_step(
            &engine(),
            &clean_inputs(),
            vec![gate_outcome("fmt", CheckStatus::Passed, None)],
        );
        assert_eq!(action, StepAction::Commit);
    }

    #[test]
    fn decide_step_retries_an_actionable_gate_failure() {
        use crate::quality::CheckStatus;
        let action = decide_step(
            &engine(),
            &clean_inputs(),
            vec![gate_outcome("clippy", CheckStatus::Failed, None)],
        );
        assert!(matches!(action, StepAction::Retry(_)));
    }

    #[test]
    fn decide_step_blocks_an_audit_finding_over_a_lint_retry() {
        use crate::quality::CheckStatus;
        use unshackled_config::RuleSeverity;
        // A blocking audit finding wins over an actionable lint retry.
        let action = decide_step(
            &engine(),
            &clean_inputs(),
            vec![
                gate_outcome("clippy", CheckStatus::Failed, None),
                gate_outcome("audit", CheckStatus::Failed, Some(RuleSeverity::Block)),
            ],
        );
        assert!(matches!(action, StepAction::Block(reason) if reason.contains("audit")));
    }

    #[test]
    fn decide_step_blocks_when_tests_fail_even_if_gate_is_clean() {
        use crate::quality::CheckStatus;
        let inputs = CompletionInputs {
            tests_passed: Some(false),
            ..clean_inputs()
        };
        let action = decide_step(
            &engine(),
            &inputs,
            vec![gate_outcome("fmt", CheckStatus::Passed, None)],
        );
        assert!(matches!(action, StepAction::Block(_)));
    }
}
