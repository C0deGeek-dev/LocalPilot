//! Tighten-only precondition checks evaluated before a tool runs.
//!
//! A tool's [`ToolContract`](localpilot_tools::ToolContract) may declare
//! preconditions the runtime must confirm before the call proceeds. These checks
//! can only *refuse* a call — they never grant one the permission engine would
//! deny — so they tighten the safety model without weakening it. The flagship
//! check is `RequiresPriorRead`: a destructive overwrite of an existing file is
//! blocked unless that file was read this session, so a write is grounded in
//! current evidence rather than the model's memory.

use std::path::Path;

use localpilot_sandbox::Workspace;
use localpilot_tools::{string_arg, Precondition, StatePredicate};
use serde_json::Value;

use crate::evidence::{CallOutcome, EvidenceLedger};

/// Evaluate a tool's preconditions against the session evidence and workspace.
///
/// Returns `Err(reason)` with a model-visible message when a precondition is
/// unmet; the caller turns that into a blocked tool result. Unmodelled
/// precondition kinds are treated as satisfied for forward compatibility.
///
/// # Errors
/// Returns the model-visible block reason when a precondition is not satisfied.
pub fn evaluate(
    preconditions: &[Precondition],
    input: &Value,
    ledger: &EvidenceLedger,
    workspace: &Workspace,
    enforce_prior_read: bool,
) -> Result<(), String> {
    for precondition in preconditions {
        match precondition {
            Precondition::RequiresPriorRead { path_arg } if enforce_prior_read => {
                requires_prior_read(path_arg, input, ledger, workspace)?;
            }
            Precondition::State(predicate) => state(predicate, input, workspace)?,
            _ => {}
        }
    }
    Ok(())
}

fn state(predicate: &StatePredicate, input: &Value, workspace: &Workspace) -> Result<(), String> {
    match predicate {
        StatePredicate::FileExists { path_arg } => file_exists(path_arg, input, workspace),
        _ => Ok(()),
    }
}

fn file_exists(path_arg: &str, input: &Value, workspace: &Workspace) -> Result<(), String> {
    let path = string_arg(input, path_arg)
        .ok_or_else(|| format!("state precondition requires string argument `{path_arg}`"))?;
    let resolved = workspace
        .resolve(Path::new(path))
        .map_err(|error| format!("cannot evaluate state for `{path}`: {error}"))?;
    if resolved.is_file() {
        Ok(())
    } else {
        Err(format!(
            "`{path}` must be an existing file before this tool can run"
        ))
    }
}

/// A path the contract marks `RequiresPriorRead` must have been read this session
/// before it is overwritten — but only when it already exists (writing a *new*
/// file has nothing to read).
fn requires_prior_read(
    path_arg: &str,
    input: &Value,
    ledger: &EvidenceLedger,
    workspace: &Workspace,
) -> Result<(), String> {
    let Some(path) = string_arg(input, path_arg) else {
        return Ok(());
    };
    let exists = workspace
        .normalize(Path::new(path))
        .map(|resolved| resolved.exists())
        .unwrap_or(false);
    if !exists {
        return Ok(());
    }
    if read_this_session(path_arg, path, ledger) {
        return Ok(());
    }
    Err(format!(
        "`{path}` already exists but was not read this session; read it first so the \
         overwrite is grounded in its current contents, not assumed ones"
    ))
}

/// Whether the evidence ledger shows a successful read of `path` this session.
fn read_this_session(path_arg: &str, path: &str, ledger: &EvidenceLedger) -> bool {
    ledger.calls().iter().any(|call| {
        call.name == "read_file"
            && call.outcome == CallOutcome::Ok
            && string_arg(&call.input, path_arg) == Some(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_core::{ContentBlock, EventId, Message, Role, ToolCall, ToolResult};
    use localpilot_store::{
        MessageOrigin, SessionEvent, SessionEventKind, SESSION_EVENT_FORMAT_VERSION,
    };

    fn event(kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            v: SESSION_EVENT_FORMAT_VERSION,
            id: EventId::new(),
            parent_id: None,
            at_unix: 0,
            kind,
        }
    }

    fn read_call_ledger(path: &str) -> EvidenceLedger {
        let call = ToolCall::new(
            "r1".into(),
            "read_file",
            serde_json::json!({ "path": path }),
        );
        let read = event(SessionEventKind::Message {
            message: Message::new(Role::Assistant, vec![ContentBlock::ToolUse(call)]),
            origin: MessageOrigin::Assistant,
        });
        let result = event(SessionEventKind::Message {
            message: Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult(ToolResult::success(
                    "r1".into(),
                    "contents",
                ))],
            ),
            origin: MessageOrigin::ToolResult,
        });
        EvidenceLedger::project(&[read, result])
    }

    fn workspace_with_file(name: &str) -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(name), "existing\n").unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        (dir, ws)
    }

    const PRIOR_READ: &[Precondition] = &[Precondition::RequiresPriorRead { path_arg: "path" }];

    #[test]
    fn overwriting_an_existing_unread_file_is_blocked() {
        let (_dir, ws) = workspace_with_file("data.txt");
        let input = serde_json::json!({ "path": "data.txt", "content": "new" });
        let verdict = evaluate(PRIOR_READ, &input, &EvidenceLedger::default(), &ws, true);
        let reason = verdict.expect_err("an unread overwrite must be blocked");
        assert!(reason.contains("data.txt"));
        assert!(reason.contains("read it first"));
    }

    #[test]
    fn overwriting_after_a_read_proceeds() {
        let (_dir, ws) = workspace_with_file("data.txt");
        let input = serde_json::json!({ "path": "data.txt", "content": "new" });
        let ledger = read_call_ledger("data.txt");
        assert!(evaluate(PRIOR_READ, &input, &ledger, &ws, true).is_ok());
    }

    #[test]
    fn writing_a_new_file_needs_no_prior_read() {
        let (_dir, ws) = workspace_with_file("other.txt");
        let input = serde_json::json!({ "path": "fresh.txt", "content": "new" });
        assert!(evaluate(PRIOR_READ, &input, &EvidenceLedger::default(), &ws, true).is_ok());
    }

    #[test]
    fn prior_read_can_be_disabled_without_disabling_state_checks() {
        let (_dir, ws) = workspace_with_file("data.txt");
        let input = serde_json::json!({ "path": "data.txt" });
        let both = &[
            Precondition::State(StatePredicate::FileExists { path_arg: "path" }),
            Precondition::RequiresPriorRead { path_arg: "path" },
        ];

        assert!(evaluate(both, &input, &EvidenceLedger::default(), &ws, false).is_ok());
    }

    #[test]
    fn missing_file_fails_the_state_precondition() {
        let (_dir, ws) = workspace_with_file("other.txt");
        let input = serde_json::json!({ "path": "missing.txt" });
        let state = &[Precondition::State(StatePredicate::FileExists {
            path_arg: "path",
        })];

        let reason = evaluate(state, &input, &EvidenceLedger::default(), &ws, false)
            .expect_err("a missing file must be blocked");
        assert!(reason.contains("missing.txt"));
        assert!(reason.contains("existing file"));
    }
}
