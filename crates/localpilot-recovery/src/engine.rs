//! The recovery ladder and model-health tracking.

use serde::{Deserialize, Serialize};

use crate::detect::BadOutputKind;

/// Whether a bad-output kind is a malformed *structured output* — the model
/// failed to emit a well-formed tool call. These are the kinds a chunked-write
/// instruction can recover, distinct from degenerate-text kinds (slash flood,
/// token loop) where shrinking the output makes no difference.
fn is_malformed_output(kind: BadOutputKind) -> bool {
    matches!(
        kind,
        BadOutputKind::MalformedStructuredOutput | BadOutputKind::MalformedToolCall
    )
}

/// The current health of the provider/model for this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelHealth {
    /// Producing clean output.
    Healthy,
    /// Recovering from a bad turn within budget.
    Recovering,
    /// Recovery exhausted; the provider/model is degraded.
    Degraded,
}

/// One rung of the recovery ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RecoveryAction {
    AbortStream,
    SaveDiagnostic,
    RetryWithRepairPrompt,
    ReduceContext,
    SummarizeOversizedToolResults,
    /// The model could not emit a large structured output — a file-write tool's
    /// arguments — as one well-formed call. Instruct it to split the write into
    /// smaller calls: the first section with `write_file`, each remaining
    /// section appended. The output-side counterpart to `ReduceContext`, which
    /// only shrinks the input.
    RequestChunkedWrite,
    LowerImageCount,
    MarkDegraded,
    StopHarnessProgress,
}

/// A persistable record of a recovery event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryDiagnostic {
    pub kind: BadOutputKind,
    pub attempt: u32,
    pub health: ModelHealth,
    pub actions: Vec<RecoveryAction>,
}

/// The hard budget on repair attempts before declaring the model degraded.
#[derive(Debug, Clone, Copy)]
pub struct RecoveryBudget {
    pub max_repair_attempts: u32,
}

impl Default for RecoveryBudget {
    fn default() -> Self {
        Self {
            max_repair_attempts: 2,
        }
    }
}

/// Tracks model health across turns and drives the recovery ladder.
#[derive(Debug, Clone)]
pub struct RecoveryEngine {
    budget: RecoveryBudget,
    attempts: u32,
    health: ModelHealth,
    last_turn_clean: bool,
}

impl RecoveryEngine {
    /// A fresh engine with the given budget.
    #[must_use]
    pub fn new(budget: RecoveryBudget) -> Self {
        Self {
            budget,
            attempts: 0,
            health: ModelHealth::Healthy,
            last_turn_clean: true,
        }
    }

    /// The current model health.
    #[must_use]
    pub fn health(&self) -> ModelHealth {
        self.health
    }

    /// Record a clean turn: recovery resets and a harness step may proceed.
    pub fn record_clean_turn(&mut self) {
        self.attempts = 0;
        self.last_turn_clean = true;
        if self.health == ModelHealth::Recovering {
            self.health = ModelHealth::Healthy;
        }
    }

    /// Record a bad turn and return the ladder actions to take. Within budget the
    /// model is `Recovering`; once the budget is exhausted it becomes `Degraded`.
    pub fn record_bad_turn(&mut self, kind: BadOutputKind) -> RecoveryDiagnostic {
        self.attempts += 1;
        self.last_turn_clean = false;
        let mut actions = vec![RecoveryAction::AbortStream, RecoveryAction::SaveDiagnostic];

        if self.attempts <= self.budget.max_repair_attempts {
            self.health = ModelHealth::Recovering;
            actions.push(RecoveryAction::RetryWithRepairPrompt);
            // A malformed structured output is the model losing tool-call
            // coherence — typically an oversized write it can't emit in one
            // call. Ask it to chunk the write from the first attempt, so the
            // instruction lands before the budget is spent.
            if is_malformed_output(kind) {
                actions.push(RecoveryAction::RequestChunkedWrite);
            }
            if self.attempts > 1 {
                actions.push(RecoveryAction::ReduceContext);
                actions.push(RecoveryAction::SummarizeOversizedToolResults);
            }
        } else {
            self.health = ModelHealth::Degraded;
            actions.push(RecoveryAction::MarkDegraded);
            actions.push(RecoveryAction::StopHarnessProgress);
        }

        RecoveryDiagnostic {
            kind,
            attempt: self.attempts,
            health: self.health,
            actions,
        }
    }

    /// Whether a harness step may be completed now. A bad or unrecovered turn may
    /// not complete a step, and a degraded model never may.
    #[must_use]
    pub fn step_completable(&self) -> bool {
        self.last_turn_clean && self.health != ModelHealth::Degraded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_turn_blocks_step_completion_until_a_clean_turn() {
        let mut engine = RecoveryEngine::new(RecoveryBudget::default());
        assert!(engine.step_completable());

        let diag = engine.record_bad_turn(BadOutputKind::MalformedToolCall);
        assert!(diag
            .actions
            .contains(&RecoveryAction::RetryWithRepairPrompt));
        assert_eq!(engine.health(), ModelHealth::Recovering);
        assert!(!engine.step_completable());

        engine.record_clean_turn();
        assert_eq!(engine.health(), ModelHealth::Healthy);
        assert!(engine.step_completable());
    }

    #[test]
    fn exhausted_recovery_marks_degraded_and_blocks_steps() {
        let mut engine = RecoveryEngine::new(RecoveryBudget {
            max_repair_attempts: 2,
        });
        engine.record_bad_turn(BadOutputKind::SlashFlood);
        engine.record_bad_turn(BadOutputKind::SlashFlood);
        let diag = engine.record_bad_turn(BadOutputKind::SlashFlood);
        assert_eq!(engine.health(), ModelHealth::Degraded);
        assert!(diag.actions.contains(&RecoveryAction::MarkDegraded));
        assert!(diag.actions.contains(&RecoveryAction::StopHarnessProgress));
        assert!(!engine.step_completable());
    }

    #[test]
    fn malformed_tool_call_triggers_a_repair_attempt() {
        let mut engine = RecoveryEngine::new(RecoveryBudget::default());
        let diag = engine.record_bad_turn(BadOutputKind::MalformedToolCall);
        assert_eq!(diag.kind, BadOutputKind::MalformedToolCall);
        assert!(diag.actions.contains(&RecoveryAction::AbortStream));
        assert!(diag.actions.contains(&RecoveryAction::SaveDiagnostic));
    }

    #[test]
    fn a_malformed_output_requests_a_chunked_write_from_the_first_attempt() {
        for kind in [
            BadOutputKind::MalformedStructuredOutput,
            BadOutputKind::MalformedToolCall,
        ] {
            let mut engine = RecoveryEngine::new(RecoveryBudget::default());
            let diag = engine.record_bad_turn(kind);
            assert!(
                diag.actions.contains(&RecoveryAction::RequestChunkedWrite),
                "{kind:?} should request a chunked write on the first attempt"
            );
        }
    }

    #[test]
    fn a_degenerate_text_kind_does_not_request_a_chunked_write() {
        // Slash flood / token loop are not a too-large output, so chunking the
        // write would not help — only the repair prompt applies.
        let mut engine = RecoveryEngine::new(RecoveryBudget::default());
        let diag = engine.record_bad_turn(BadOutputKind::SlashFlood);
        assert!(!diag.actions.contains(&RecoveryAction::RequestChunkedWrite));
    }

    #[test]
    fn an_exhausted_malformed_output_degrades_without_requesting_a_chunked_write() {
        // Past the budget the turn is degraded and stops; the chunk request is a
        // within-budget recovery, not a terminal action.
        let mut engine = RecoveryEngine::new(RecoveryBudget {
            max_repair_attempts: 1,
        });
        engine.record_bad_turn(BadOutputKind::MalformedStructuredOutput);
        let diag = engine.record_bad_turn(BadOutputKind::MalformedStructuredOutput);
        assert_eq!(engine.health(), ModelHealth::Degraded);
        assert!(diag.actions.contains(&RecoveryAction::MarkDegraded));
        assert!(!diag.actions.contains(&RecoveryAction::RequestChunkedWrite));
    }
}
