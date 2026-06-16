//! Deterministic evaluation of a contract's postconditions after execution.
//!
//! Each postcondition resolves to one of three checks: it held, it did not, or
//! it could not be proven cheaply. A `ReadBack`/`ConfirmRead` postcondition
//! issues only a workspace-contained read — never a re-execution of the side
//! effect.

use std::path::Path;

use localpilot_sandbox::Workspace;
use localpilot_tools::{string_arg, ContentExpectation, PathEffectKind, Postcondition};
use serde_json::Value;

/// The result of checking one postcondition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Check {
    /// The postcondition held.
    Satisfied,
    /// The postcondition was checked and did not hold.
    Unsatisfied,
    /// The postcondition could not be proven cheaply (e.g. proving a file's
    /// content changed needs a before-snapshot we do not keep).
    Unknown,
}

/// Evaluate one postcondition against the call input and the workspace.
pub(crate) fn evaluate(
    postcondition: &Postcondition,
    input: &Value,
    workspace: &Workspace,
) -> Check {
    match postcondition {
        // The call did not error (the caller checks that first), so its status
        // is a success.
        Postcondition::ResultStatus => Check::Satisfied,
        Postcondition::PathEffect { path_arg, kind } => {
            path_effect(path_arg, *kind, input, workspace)
        }
        Postcondition::ConfirmRead { path_arg, expect } => {
            confirm_read(path_arg, *expect, input, workspace)
        }
        // A future postcondition kind this build does not understand cannot be
        // proven — never claim success for it.
        _ => Check::Unknown,
    }
}

fn resolved(path_arg: &str, input: &Value, workspace: &Workspace) -> Option<std::path::PathBuf> {
    let path = string_arg(input, path_arg)?;
    workspace.normalize(Path::new(path)).ok()
}

fn path_effect(
    path_arg: &str,
    kind: PathEffectKind,
    input: &Value,
    workspace: &Workspace,
) -> Check {
    let Some(path) = resolved(path_arg, input, workspace) else {
        return Check::Unknown;
    };
    let exists = path.exists();
    match kind {
        PathEffectKind::Exists => yes_no(exists),
        PathEffectKind::Deleted => yes_no(!exists),
        // Proving a modification needs a before-snapshot; the most we can prove
        // deterministically is that the file still exists (a vanished file is a
        // clear failure, a present one is unproven).
        PathEffectKind::Modified => {
            if exists {
                Check::Unknown
            } else {
                Check::Unsatisfied
            }
        }
        // A future kind this build does not understand cannot be proven.
        _ => Check::Unknown,
    }
}

fn confirm_read(
    path_arg: &str,
    expect: ContentExpectation,
    input: &Value,
    workspace: &Workspace,
) -> Check {
    let Some(path) = resolved(path_arg, input, workspace) else {
        return Check::Unknown;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Check::Unsatisfied;
    };
    match expect {
        ContentExpectation::NonEmpty => yes_no(!content.trim().is_empty()),
        ContentExpectation::Contains(needle) => yes_no(content.contains(needle)),
        // A future expectation this build does not understand cannot be proven.
        _ => Check::Unknown,
    }
}

fn yes_no(satisfied: bool) -> Check {
    if satisfied {
        Check::Satisfied
    } else {
        Check::Unsatisfied
    }
}
