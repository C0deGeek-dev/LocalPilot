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

/// A coarse category of completed action. A claim of a given kind is supported
/// only by a verified tool call capable of producing that effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionKind {
    /// Create or modify a file (`created`, `wrote`, `edited`, `added`, …).
    Write,
    /// Remove a file (`deleted`, `removed`).
    Delete,
    /// Relocate a file (`renamed`, `moved`, `copied`).
    Move,
    /// Version-control state (`committed`, `pushed`, `merged`).
    Vcs,
    /// Execute or install (`ran`, `installed`).
    Run,
}

/// Past-tense / past-participle markers that signal a *completed action* claim,
/// each mapped to the action category a backing tool call must satisfy. Only
/// unambiguous completed-action forms appear: present tense (`creates`) and
/// gerunds (`creating`) are deliberately absent so plans, analysis, and
/// statements of fact stay untouched.
const ACTION_MARKERS: &[(&str, ActionKind)] = &[
    ("created", ActionKind::Write),
    ("saved", ActionKind::Write),
    ("wrote", ActionKind::Write),
    ("written", ActionKind::Write),
    ("updated", ActionKind::Write),
    ("edited", ActionKind::Write),
    ("added", ActionKind::Write),
    ("generated", ActionKind::Write),
    ("implemented", ActionKind::Write),
    ("configured", ActionKind::Write),
    ("formatted", ActionKind::Write),
    ("refactored", ActionKind::Write),
    ("applied", ActionKind::Write),
    ("wired", ActionKind::Write),
    ("fixed", ActionKind::Write),
    ("deleted", ActionKind::Delete),
    ("removed", ActionKind::Delete),
    ("renamed", ActionKind::Move),
    ("moved", ActionKind::Move),
    ("copied", ActionKind::Move),
    ("committed", ActionKind::Vcs),
    ("pushed", ActionKind::Vcs),
    ("merged", ActionKind::Vcs),
    ("installed", ActionKind::Run),
    ("ran", ActionKind::Run),
];

/// Review a final reply against the turn's evidence ledger.
///
/// Returns `Some(rewritten)` when the reply asserts a completed action that no
/// *compatible* verified call supports — matching is per-claim, so one verified
/// action no longer excuses a different, unverified one. Returns `None` when the
/// reply makes no action claim, or every action claim it makes is backed.
#[must_use]
pub fn review_final_reply(text: &str, ledger: &EvidenceLedger) -> Option<String> {
    let verified: Vec<&str> = ledger
        .calls()
        .iter()
        .filter(|call| call.verdict.as_deref() == Some("verified"))
        .map(|call| call.name.as_str())
        .collect();
    let unsupported = unsupported_claims(text, &verified);
    if unsupported.is_empty() {
        return None;
    }
    let label = if unsupported.len() == 1 {
        "this completed-action claim"
    } else {
        "these completed-action claims"
    };
    Some(format!(
        "{}\n\n[unverified] No successful, verified tool call this turn supports {}: \
         {} — treat as not done until verified.",
        text.trim_end(),
        label,
        unsupported.join("; ")
    ))
}

/// The completed-action claims in `text` that no verified tool can back. A
/// sentence is reported when it asserts an action of a category for which no
/// verified call is capable of producing the effect.
fn unsupported_claims(text: &str, verified_tools: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for sentence in split_sentences(text) {
        let lower = sentence.to_ascii_lowercase();
        let kinds = claim_kinds(&lower);
        if kinds.is_empty() {
            continue;
        }
        let any_unbacked = kinds.iter().any(|kind| {
            !verified_tools
                .iter()
                .any(|name| tool_satisfies(name, *kind))
        });
        if any_unbacked {
            out.push(sentence.trim().to_string());
        }
    }
    out
}

/// Every distinct action category asserted in one lowercased sentence.
fn claim_kinds(lower_sentence: &str) -> Vec<ActionKind> {
    let mut kinds = Vec::new();
    for (marker, kind) in ACTION_MARKERS {
        if contains_word(lower_sentence, marker) && !kinds.contains(kind) {
            kinds.push(*kind);
        }
    }
    kinds
}

/// Whether a verified tool named `name` can produce an effect of `kind`. A
/// shell/command tool is opaque — a successful command could produce any effect,
/// so it backs any category; the structured file tools are matched by name.
fn tool_satisfies(name: &str, kind: ActionKind) -> bool {
    let n = name.to_ascii_lowercase();
    if contains_any(&n, &["shell", "command", "exec", "bash", "terminal"]) {
        return true;
    }
    match kind {
        ActionKind::Write => contains_any(
            &n,
            &[
                "write", "edit", "create", "save", "patch", "apply", "insert", "append", "format",
            ],
        ),
        ActionKind::Delete => contains_any(&n, &["delete", "remove", "trash"]),
        ActionKind::Move => contains_any(&n, &["move", "rename", "copy"]),
        // A commit/push/merge or an execution/install is only provable by a
        // (handled-above) shell/command call; no structured tool proves it.
        ActionKind::Vcs | ActionKind::Run => false,
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

/// Split text into sentence-like units on terminal punctuation and newlines, so
/// a backed claim and an unbacked claim in different sentences are judged apart.
fn split_sentences(text: &str) -> impl Iterator<Item = &str> {
    text.split(['.', '!', '?', '\n', '\r', ';'])
        .filter(|s| !s.trim().is_empty())
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

    fn ledger_with_tool(name: &'static str, verdict: &str) -> EvidenceLedger {
        let call = ToolCall::new("c1".into(), name, serde_json::json!({}));
        EvidenceLedger::project(&[
            event(SessionEventKind::Message {
                message: Message::new(Role::Assistant, vec![ContentBlock::ToolUse(call)]),
                origin: MessageOrigin::Assistant,
            }),
            event(SessionEventKind::ToolVerified {
                id: "c1".to_string(),
                verdict: verdict.to_string(),
            }),
        ])
    }

    #[test]
    fn a_verified_write_does_not_excuse_an_unverified_delete() {
        // The mixing bug: one verified action must not back a different,
        // unverified one. Only the delete claim is unsupported.
        let ledger = ledger_with_write("verified");
        let reviewed =
            review_final_reply("I created foo.txt. I deleted the database.", &ledger).unwrap();
        assert!(reviewed.contains("[unverified]"));
        assert!(reviewed.contains("I deleted the database"));
        // The backed claim is not listed as unsupported.
        assert!(!reviewed.contains("supports this completed-action claim: I created foo.txt"));
    }

    #[test]
    fn a_write_tool_does_not_back_a_delete_claim() {
        let ledger = ledger_with_write("verified");
        assert!(review_final_reply("I removed the temp directory.", &ledger).is_some());
    }

    #[test]
    fn a_verified_shell_backs_any_action_category() {
        // A shell command is opaque — it can produce any effect, so it backs a
        // commit/push claim a structured tool could not.
        let ledger = ledger_with_tool("run_shell", "verified");
        assert!(review_final_reply("I committed and pushed the changes.", &ledger).is_none());
    }

    #[test]
    fn expanded_markers_are_recognized() {
        let ledger = ledger_with_write("failed");
        assert!(review_final_reply("I implemented the parser.", &ledger).is_some());
        assert!(review_final_reply("I ran the test suite.", &ledger).is_some());
    }

    #[test]
    fn present_tense_and_gerunds_are_not_flagged() {
        // The plan/analysis boundary: only completed past-tense forms flag.
        let ledger = ledger_with_write("failed");
        assert!(review_final_reply("The factory creates widgets.", &ledger).is_none());
        assert!(review_final_reply("I am creating the file now.", &ledger).is_none());
        assert!(review_final_reply("I will add the handler next.", &ledger).is_none());
    }
}
