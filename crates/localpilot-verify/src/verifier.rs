//! The verifier seam and its deterministic default implementation.

use localpilot_sandbox::Workspace;
use localpilot_tools::{ToolContract, VerificationMethod};
use serde_json::Value;

use crate::observation::Observation;
use crate::postcondition::{evaluate, Check};
use crate::verdict::Verdict;

/// Everything the verifier needs to judge one executed tool call. The verifier
/// reads, it never re-executes a side effect (a confirming read-back issues only
/// a workspace-contained read).
pub struct VerificationInput<'a> {
    /// The tool's contract, carrying its postconditions and verification method.
    pub contract: &'a ToolContract,
    /// The arguments the call was made with.
    pub input: &'a Value,
    /// The normalized, untrusted observation of the tool's result.
    pub observation: &'a Observation,
    /// The workspace, for resolving and reading paths a postcondition names.
    pub workspace: &'a Workspace,
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
        if input.observation.is_error() {
            return Verdict::Failed;
        }
        // This guard is also the production consumer that keeps declaration
        // semantics honest: a contract that selected a check-driven method but
        // supplied no checks can never drift into an implicit success.
        if !input.contract.is_verification_declared() {
            return Verdict::Unverified;
        }
        match input.contract.verification {
            VerificationMethod::Postconditions => verify_postconditions(input),
            VerificationMethod::ReadBack { tool: "read_file" } => verify_postconditions(input),
            // No other named read tool has deterministic, permission-free
            // semantics in this crate. Unknown future methods and the optional
            // model critic remain honest non-successes until implemented.
            VerificationMethod::ReadBack { .. }
            | VerificationMethod::ModelCritic
            | VerificationMethod::Unverifiable => Verdict::Unverified,
            _ => Verdict::Unverified,
        }
    }
}

fn verify_postconditions(input: &VerificationInput<'_>) -> Verdict {
    if input.contract.postconditions.is_empty() {
        return Verdict::Unverified;
    }
    // All postconditions must hold to claim `Verified`. One that fails is
    // `Failed`; one that cannot be proven leaves the call `Unverified`.
    let mut any_unknown = false;
    for postcondition in input.contract.postconditions {
        match evaluate(postcondition, input.input, input.workspace) {
            Check::Satisfied => {}
            Check::Unsatisfied => return Verdict::Failed,
            Check::Unknown => any_unknown = true,
        }
    }
    if any_unknown {
        Verdict::Unverified
    } else {
        Verdict::Verified
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::Observation;
    use localpilot_core::{ToolResult, ToolUseId};
    use localpilot_tools::{
        ContentExpectation, PathEffectKind, Postcondition, ToolRegistry, VerificationMethod,
    };

    const PATH_EXISTS: &[Postcondition] = &[Postcondition::PathEffect {
        path_arg: "path",
        kind: PathEffectKind::Exists,
    }];

    fn ok_obs() -> Observation {
        Observation::from_tool_result(&ToolResult::success(ToolUseId::from("c1"), "ok"))
    }

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        (dir, ws)
    }

    #[test]
    fn an_errored_call_is_failed() {
        let contract = ToolContract::default();
        let (_dir, ws) = workspace();
        let obs = Observation::from_tool_result(&ToolResult::error(ToolUseId::from("c1"), "boom"));
        let verdict = DeterministicVerifier.verify(&VerificationInput {
            contract: &contract,
            input: &Value::Null,
            observation: &obs,
            workspace: &ws,
        });
        assert_eq!(verdict, Verdict::Failed);
    }

    #[test]
    fn an_unverifiable_effect_is_unverified_not_success() {
        let contract = ToolContract {
            // Even a satisfied postcondition cannot override an explicit
            // admission that this tool's effect is unverifiable.
            postconditions: &[Postcondition::ResultStatus],
            verification: VerificationMethod::Unverifiable,
            ..ToolContract::default()
        };
        let (_dir, ws) = workspace();
        let obs = ok_obs();
        let verdict = DeterministicVerifier.verify(&VerificationInput {
            contract: &contract,
            input: &Value::Null,
            observation: &obs,
            workspace: &ws,
        });
        assert_eq!(verdict, Verdict::Unverified);
        assert!(!verdict.is_success());
    }

    #[test]
    fn a_satisfied_path_postcondition_is_verified() {
        let contract = ToolContract {
            postconditions: PATH_EXISTS,
            verification: VerificationMethod::Postconditions,
            ..ToolContract::default()
        };
        let (dir, ws) = workspace();
        std::fs::write(dir.path().join("out.txt"), "data").unwrap();
        let input = serde_json::json!({ "path": "out.txt" });
        let obs = ok_obs();
        let verdict = DeterministicVerifier.verify(&VerificationInput {
            contract: &contract,
            input: &input,
            observation: &obs,
            workspace: &ws,
        });
        assert_eq!(verdict, Verdict::Verified);
    }

    #[test]
    fn a_write_whose_postcondition_fails_is_failed() {
        let contract = ToolContract {
            postconditions: PATH_EXISTS,
            verification: VerificationMethod::Postconditions,
            ..ToolContract::default()
        };
        let (_dir, ws) = workspace();
        // The call reported success, but the file the contract requires is absent.
        let input = serde_json::json!({ "path": "missing.txt" });
        let obs = ok_obs();
        let verdict = DeterministicVerifier.verify(&VerificationInput {
            contract: &contract,
            input: &input,
            observation: &obs,
            workspace: &ws,
        });
        assert_eq!(verdict, Verdict::Failed);
    }

    #[test]
    fn a_sabotaged_edit_read_back_is_failed() {
        const CONFIRM_CHANGED: &[Postcondition] = &[Postcondition::ConfirmRead {
            path_arg: "path",
            expect: ContentExpectation::Contains("changed"),
        }];
        let registry = ToolRegistry::with_builtins();
        let edit = registry.get("edit_file").unwrap();
        assert_eq!(
            edit.contract().verification,
            VerificationMethod::ReadBack { tool: "read_file" }
        );

        let contract = ToolContract {
            postconditions: CONFIRM_CHANGED,
            verification: VerificationMethod::ReadBack { tool: "read_file" },
            ..ToolContract::default()
        };
        let (dir, ws) = workspace();
        // Stand in for a tool that reported success while the underlying write
        // was lost: the old content is still present on read-back.
        std::fs::write(dir.path().join("edit.txt"), "original").unwrap();
        let input = serde_json::json!({ "path": "edit.txt", "new_text": "changed" });
        let obs = ok_obs();
        let verdict = DeterministicVerifier.verify(&VerificationInput {
            contract: &contract,
            input: &input,
            observation: &obs,
            workspace: &ws,
        });
        assert_eq!(verdict, Verdict::Failed);
    }

    #[test]
    fn every_file_mutator_with_read_back_declares_confirm_read() {
        let registry = ToolRegistry::with_builtins();
        for name in [
            "write_file",
            "append_file",
            "edit_file",
            "multi_edit",
            "replace_in_file",
        ] {
            let contract = registry.get(name).unwrap().contract();
            assert_eq!(
                contract.verification,
                VerificationMethod::ReadBack { tool: "read_file" },
                "{name} must select read-back verification"
            );
            assert!(
                contract.postconditions.iter().any(|postcondition| matches!(
                    postcondition,
                    Postcondition::ConfirmRead {
                        path_arg: "path",
                        expect: ContentExpectation::NonEmpty,
                    }
                )),
                "{name} must declare the matching ConfirmRead producer"
            );
        }
    }

    #[test]
    fn an_empty_write_fails_the_builtin_confirm_read() {
        let registry = ToolRegistry::with_builtins();
        let contract = registry.get("write_file").unwrap().contract();
        let (dir, ws) = workspace();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        let input = serde_json::json!({ "path": "empty.txt", "content": "expected" });
        let obs = ok_obs();

        let verdict = DeterministicVerifier.verify(&VerificationInput {
            contract: &contract,
            input: &input,
            observation: &obs,
            workspace: &ws,
        });

        assert_eq!(verdict, Verdict::Failed);
    }

    #[test]
    fn a_tool_without_confirm_read_keeps_its_existing_verdict() {
        let registry = ToolRegistry::with_builtins();
        let contract = registry.get("read_file").unwrap().contract();
        assert!(!contract
            .postconditions
            .iter()
            .any(|postcondition| matches!(postcondition, Postcondition::ConfirmRead { .. })));
        let (_dir, ws) = workspace();
        let obs = ok_obs();

        let verdict = DeterministicVerifier.verify(&VerificationInput {
            contract: &contract,
            input: &Value::Null,
            observation: &obs,
            workspace: &ws,
        });

        assert_eq!(verdict, Verdict::Verified);
    }

    #[test]
    fn an_unknown_read_back_tool_is_unverified() {
        let contract = ToolContract {
            postconditions: PATH_EXISTS,
            verification: VerificationMethod::ReadBack {
                tool: "not_a_read_tool",
            },
            ..ToolContract::default()
        };
        let (dir, ws) = workspace();
        std::fs::write(dir.path().join("out.txt"), "data").unwrap();
        let input = serde_json::json!({ "path": "out.txt" });
        let obs = ok_obs();
        let verdict = DeterministicVerifier.verify(&VerificationInput {
            contract: &contract,
            input: &input,
            observation: &obs,
            workspace: &ws,
        });
        assert_eq!(verdict, Verdict::Unverified);
    }
}
