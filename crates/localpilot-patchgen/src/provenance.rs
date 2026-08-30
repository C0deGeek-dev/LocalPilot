//! The change-provenance record.
//!
//! Every agent-authored patch carries one. It is a durable, inspectable record of
//! *how* and *why* a change was proposed — the prompt and model behind it, the
//! tools used, the test evidence, the rationale, the risks, the rollback, and the
//! lessons. It is meant to live alongside the proposal (e.g. in the private hub),
//! not in shipped code. An eval result is attached once the eval gate has run.

use serde::{Deserialize, Serialize};

/// Schema tag so a consumer can pin the record shape.
pub const PROVENANCE_SCHEMA: &str = "localpilot-change-provenance-v1";

/// The outcome of the eval gate over a proposed patch (attached once the gate
/// lands). Kept deliberately small and host-neutral here: a pass/fail plus a
/// short scorecard summary string the gate produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalResult {
    /// Whether the gate passed the patch through to the human queue.
    pub passed: bool,
    /// A short, human-readable scorecard summary from the gate.
    pub summary: String,
}

/// A structured record of how a patch was produced and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeProvenance {
    /// Schema tag (`PROVENANCE_SCHEMA`).
    pub schema: String,
    /// The prompt/instruction that drove the change.
    pub prompt: String,
    /// The model that authored the change (id/name).
    pub model: String,
    /// The tools the agent used to produce the change.
    pub tools_used: Vec<String>,
    /// Evidence the change was tested (commands run, results observed).
    pub test_evidence: String,
    /// Why the change is the right fix for the finding.
    pub rationale: String,
    /// Known risks the change carries.
    pub risks: String,
    /// How to roll the change back (for the loop: drop the branch/worktree).
    pub rollback_notes: String,
    /// Lessons worth recording from producing the change.
    pub lessons: Vec<String>,
    /// The eval-gate result, attached once the gate has run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_result: Option<EvalResult>,
}

impl ChangeProvenance {
    /// A new record with the required narrative fields; lists default empty and
    /// the eval result is attached later.
    #[must_use]
    pub fn new(
        prompt: impl Into<String>,
        model: impl Into<String>,
        rationale: impl Into<String>,
    ) -> Self {
        Self {
            schema: PROVENANCE_SCHEMA.to_string(),
            prompt: prompt.into(),
            model: model.into(),
            tools_used: Vec::new(),
            test_evidence: String::new(),
            rationale: rationale.into(),
            risks: String::new(),
            rollback_notes: "drop the proposal branch/worktree".to_string(),
            lessons: Vec::new(),
            eval_result: None,
        }
    }

    /// Whether the record carries the minimum a reviewer needs: a prompt, a model,
    /// a rationale, and rollback notes. Used by the proposal path to reject a
    /// patch with an empty provenance record.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.prompt.trim().is_empty()
            && !self.model.trim().is_empty()
            && !self.rationale.trim().is_empty()
            && !self.rollback_notes.trim().is_empty()
    }

    /// Serialize as stable JSON for storage alongside the proposal.
    ///
    /// # Errors
    /// Returns a [`serde_json::Error`] only if serialization fails.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}
