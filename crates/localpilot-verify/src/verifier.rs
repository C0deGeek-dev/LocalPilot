//! The verifier seam and its deterministic default implementation.

use localpilot_tools::ToolContract;
use serde_json::Value;

use crate::verdict::Verdict;

/// Everything the verifier needs to judge one executed tool call. The verifier
/// reads, it never re-executes a side effect (a confirming read-back, added in a
/// later slice, issues only a read).
pub struct VerificationInput<'a> {
    /// The tool's contract, carrying its postconditions and verification method.
    pub contract: &'a ToolContract,
    /// The arguments the call was made with.
    pub input: &'a Value,
    /// The tool's recorded textual output.
    pub output: &'a str,
    /// Whether the tool reported the call as an error.
    pub is_error: bool,
}

/// Judges whether an executed tool call did what its contract promised.
pub trait Verifier: Send + Sync {
    /// Produce a [`Verdict`] for one executed call.
    fn verify(&self, input: &VerificationInput<'_>) -> Verdict;
}

/// The deterministic, no-LLM verifier — the default. It judges only from the
/// contract, the input, and the recorded result. A model-critic verifier is a
/// future drop-in behind the same [`Verifier`] trait.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeterministicVerifier;

impl Verifier for DeterministicVerifier {
    fn verify(&self, input: &VerificationInput<'_>) -> Verdict {
        if input.is_error {
            return Verdict::Failed;
        }
        // Postcondition evaluation is added in a later slice. Until a checkable
        // postcondition proves the effect, the honest verdict is `Unverified` —
        // success is never assumed.
        Verdict::Unverified
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(
        contract: &'a ToolContract,
        output: &'a str,
        is_error: bool,
    ) -> VerificationInput<'a> {
        VerificationInput {
            contract,
            input: &Value::Null,
            output,
            is_error,
        }
    }

    #[test]
    fn an_errored_call_is_failed() {
        let contract = ToolContract::default();
        let verdict = DeterministicVerifier.verify(&input(&contract, "boom", true));
        assert_eq!(verdict, Verdict::Failed);
    }

    #[test]
    fn an_unproven_effect_is_unverified_not_success() {
        let contract = ToolContract::default();
        let verdict = DeterministicVerifier.verify(&input(&contract, "ok", false));
        assert_eq!(verdict, Verdict::Unverified);
        assert!(!verdict.is_success());
    }
}
