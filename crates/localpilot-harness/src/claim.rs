//! The no-unsupported-claim gate over a final reply.
//!
//! A final reply may assert that an *action completed* only if a verified tool
//! call this turn supports it. The gate is deterministic and conservative: it
//! looks only for past-tense action-completion language, leaves analysis and
//! fact statements untouched, and flags an action claim only when no tool call
//! was `Verified`. It never silently drops content — it appends a visible
//! correction so the reader (and the model, on the next turn) sees the claim is
//! unsupported.

use crate::evidence::EvidenceLedger;

/// Past-tense markers that signal a *completed action* claim (as opposed to a
/// plan, a question, or a statement of fact).
const ACTION_MARKERS: &[&str] = &[
    "created",
    "saved",
    "wrote",
    "written",
    "updated",
    "edited",
    "deleted",
    "removed",
    "renamed",
    "committed",
    "applied",
    "installed",
    "fixed",
];

/// Review a final reply against the turn's evidence ledger.
///
/// Returns `Some(rewritten)` when the reply asserts a completed action that no
/// `Verified` call supports; returns `None` when the reply makes no action
/// claim, or a verified call backs it (leave it untouched).
#[must_use]
pub fn review_final_reply(text: &str, ledger: &EvidenceLedger) -> Option<String> {
    if !asserts_completed_action(text) {
        return None;
    }
    if ledger
        .calls()
        .iter()
        .any(|call| call.verdict.as_deref() == Some("verified"))
    {
        return None;
    }
    Some(format!(
        "{}\n\n[unverified] No successful, verified tool call this turn supports a \
         completed-action claim above — treat the action as not done until it is verified.",
        text.trim_end()
    ))
}

/// Whether the text reads as a claim that an action completed.
fn asserts_completed_action(text: &str) -> bool {
    let lower = text.to_lowercase();
    ACTION_MARKERS
        .iter()
        .any(|marker| contains_word(&lower, marker))
}

/// Whether `haystack` contains `word` as a whole alphanumeric word, so "created"
/// matches but "uncreated"/"creates" do not trip on a substring.
fn contains_word(haystack: &str, word: &str) -> bool {
    haystack
        .split(|c: char| !c.is_alphanumeric())
        .any(|token| token == word)
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_core::{ContentBlock, EventId, Message, Role, ToolCall};
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

    fn ledger_with_write(verdict: &str) -> EvidenceLedger {
        let call = ToolCall::new(
            "w1".into(),
            "write_file",
            serde_json::json!({ "path": "f" }),
        );
        EvidenceLedger::project(&[
            event(SessionEventKind::Message {
                message: Message::new(Role::Assistant, vec![ContentBlock::ToolUse(call)]),
                origin: MessageOrigin::Assistant,
            }),
            event(SessionEventKind::ToolVerified {
                id: "w1".to_string(),
                verdict: verdict.to_string(),
            }),
        ])
    }

    #[test]
    fn an_action_claim_without_a_verified_call_is_flagged() {
        let ledger = ledger_with_write("failed");
        let reviewed = review_final_reply("I created the file.", &ledger).unwrap();
        assert!(reviewed.contains("[unverified]"));
        assert!(reviewed.contains("I created the file."));
    }

    #[test]
    fn an_action_claim_with_a_verified_call_is_left_alone() {
        let ledger = ledger_with_write("verified");
        assert!(review_final_reply("I created the file.", &ledger).is_none());
    }

    #[test]
    fn analysis_is_left_untouched() {
        let ledger = ledger_with_write("failed");
        // A statement of fact, not a completed-action claim.
        assert!(review_final_reply("The function returns 42.", &ledger).is_none());
    }
}
