//! Argument-repair feedback into LocalMind's review-gated queue.
//!
//! When `[tools] repair_learning` is on, a closed session's argument-repair
//! events are aggregated into a redacted, per-`(model, tool)` signal and offered
//! to LocalMind as a **review-gated** candidate, so a human can learn "this model
//! tends to send this tool's arguments in the wrong shape — emit the right one."
//!
//! Reuse-only (D009): it wires the previously-unwired [`tool_use_candidate`]
//! producer onto the existing review queue ([`write_retrospective_lesson`]'s
//! ADR-0037 path), stores **no** raw inputs/paths/content (only model id, tool
//! name, malformed-class label, and a count), writes no accepted memory, and adds
//! no new store. A model-specific *rule cue* (always-on injection) is deliberately
//! **not** auto-created here — that is an open question (per-model lesson sprawl);
//! a human may promote an accepted candidate to a cue through the existing
//! `register_rule_cues` path.

use std::collections::BTreeMap;
use std::path::Path;

use localmind_core::{CandidateLesson, SessionId as LearningSessionId};
use localmind_store::ReviewQueue;
use localpilot_store::{SessionEvent, SessionEventKind};

use crate::error::LearningError;
use crate::tool_use::{tool_use_candidate, ToolUseSignal};

/// Advisory confidence for a repair candidate — the same low prior as a
/// completion-retrospective lesson: an unverified self-observation, review-gated.
const REPAIR_CONFIDENCE: f32 = 0.4;

/// The review session label repair candidates are enqueued under.
const REPAIR_SESSION: &str = "tool-input-repair";

/// An aggregate, redacted argument-repair signal for one `(model, tool)` pair:
/// which malformed-argument classes the model needed repaired, and how often.
/// Carries **no** raw values — only identifiers, class labels, and counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairSignal {
    /// The model that produced the malformed arguments.
    pub model: String,
    /// The tool whose arguments were repaired.
    pub tool: String,
    /// Malformed-class label → number of repairs of that class.
    pub classes: BTreeMap<String, usize>,
}

impl RepairSignal {
    /// Total repairs across all classes for this `(model, tool)`.
    #[must_use]
    pub fn total(&self) -> usize {
        self.classes.values().sum()
    }
}

/// Aggregate a session's `ToolInputRepaired` events into per-`(model, tool)`
/// signals. Reads only the redacted event dimensions (model/tool/class) — never
/// any argument value.
#[must_use]
pub fn repair_signals_from_events(events: &[SessionEvent]) -> Vec<RepairSignal> {
    let mut by_pair: BTreeMap<(String, String), BTreeMap<String, usize>> = BTreeMap::new();
    for event in events {
        if let SessionEventKind::ToolInputRepaired {
            tool, model, class, ..
        } = &event.kind
        {
            *by_pair
                .entry((model.clone(), tool.clone()))
                .or_default()
                .entry(class.clone())
                .or_default() += 1;
        }
    }
    by_pair
        .into_iter()
        .map(|((model, tool), classes)| RepairSignal {
            model,
            tool,
            classes,
        })
        .collect()
}

/// A short, human-readable description of a malformed-class label.
fn humanize(class: &str) -> &str {
    match class {
        "bare_string_for_array" => "a single string instead of an array",
        "stringified_json" => "a quoted JSON string instead of a value",
        "object_for_array" => "an object instead of an array",
        "markdown_autolink" => "a markdown link instead of a plain path",
        "missing_required_field" => "with a required field missing",
        "type_mismatch" => "with a field of the wrong type",
        other => other,
    }
}

/// Build a review-gated candidate from a repair signal by wiring the
/// previously-unused [`tool_use_candidate`] producer. The evidence and the
/// recovery hints are aggregate and redacted; the candidate carries the structured
/// tool-use lesson (model in `context_cues`, each class in `failure_recovery`).
/// Returns `None` only if the confidence is rejected at the contract boundary.
#[must_use]
pub fn repair_lesson_candidate(signal: &RepairSignal) -> Option<CandidateLesson> {
    let failure_recovery: Vec<(String, String)> = signal
        .classes
        .iter()
        .map(|(class, count)| {
            (
                format!(
                    "`{}` arguments arrived as {} ({count} time(s))",
                    signal.tool,
                    humanize(class)
                ),
                format!(
                    "emit `{}` arguments in the shape the schema declares, not as {}",
                    signal.tool,
                    humanize(class)
                ),
            )
        })
        .collect();
    let tool_signal = ToolUseSignal {
        session: format!("repair:{}:{}", signal.model, signal.tool),
        tool: signal.tool.clone(),
        tool_version: 1,
        verified: true,
        degraded_or_looping: false,
        context_cues: vec![format!("model {}", signal.model)],
        preconditions: Vec::new(),
        action_sequence: vec![signal.tool.clone()],
        expected_observations: vec![format!(
            "`{}` arguments validate against its schema on the first call",
            signal.tool
        )],
        verification: "arguments re-validated after repair".to_string(),
        failure_recovery,
        confidence: REPAIR_CONFIDENCE,
        // Aggregate + redacted: identifiers and a count only, never a raw value.
        evidence: format!(
            "aggregate repair signal: model={} tool={} repairs={}",
            signal.model,
            signal.tool,
            signal.total()
        ),
    };
    tool_use_candidate(&tool_signal)
}

/// Offer a closed session's argument-repair patterns to LocalMind's review-gated
/// queue. Returns the number of candidates enqueued (the queue dedups by canonical
/// hash, so a repeated pattern is a no-op). Best-effort and reuse-only: it stores
/// no raw inputs and writes no accepted memory.
///
/// # Errors
/// [`LearningError::Review`] if the project store cannot be initialized or the
/// review queue enqueue fails.
pub fn enqueue_repair_signals(
    project_root: &Path,
    events: &[SessionEvent],
) -> Result<usize, LearningError> {
    let signals = repair_signals_from_events(events);
    if signals.is_empty() {
        return Ok(0);
    }
    crate::initialize(project_root).map_err(|e| LearningError::Review(e.to_string()))?;
    let queue = ReviewQueue::open_project(project_root)
        .map_err(|e| LearningError::Review(e.to_string()))?;
    let mut enqueued = 0;
    for signal in &signals {
        if let Some(candidate) = repair_lesson_candidate(signal) {
            enqueued += queue
                .enqueue_candidates(&LearningSessionId::new(REPAIR_SESSION), &[candidate])
                .map_err(|e| LearningError::Review(e.to_string()))?;
        }
    }
    Ok(enqueued)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::ops::{memory_list, review_list};
    use crate::rule_cue::rule_cue_ids;
    use localpilot_core::EventId;
    use localpilot_store::SESSION_EVENT_FORMAT_VERSION;

    fn repaired_event(tool: &str, model: &str, class: &str) -> SessionEvent {
        SessionEvent {
            v: SESSION_EVENT_FORMAT_VERSION,
            id: EventId::new(),
            parent_id: None,
            at_unix: 0,
            kind: SessionEventKind::ToolInputRepaired {
                tool: tool.to_string(),
                provider: "local".to_string(),
                model: model.to_string(),
                class: class.to_string(),
                rules: vec!["wrap_bare_string_as_array".to_string()],
            },
        }
    }

    #[test]
    fn events_aggregate_per_model_tool_with_class_counts() {
        let events = vec![
            repaired_event("git_diff", "q-model", "bare_string_for_array"),
            repaired_event("git_diff", "q-model", "bare_string_for_array"),
            repaired_event("git_diff", "q-model", "stringified_json"),
            repaired_event("git_add", "q-model", "stringified_json"),
        ];
        let signals = repair_signals_from_events(&events);
        assert_eq!(signals.len(), 2, "two (model, tool) pairs");
        let diff = signals.iter().find(|s| s.tool == "git_diff").unwrap();
        assert_eq!(diff.total(), 3);
        assert_eq!(diff.classes["bare_string_for_array"], 2);
        assert_eq!(diff.classes["stringified_json"], 1);
    }

    #[test]
    fn the_signal_and_candidate_carry_no_raw_values() {
        // The aggregate signal holds only identifiers/labels/counts; a candidate
        // built from it has redacted evidence and never an argument value.
        let mut classes = BTreeMap::new();
        classes.insert("bare_string_for_array".to_string(), 2);
        let signal = RepairSignal {
            model: "q-model".to_string(),
            tool: "git_diff".to_string(),
            classes,
        };
        let candidate = repair_lesson_candidate(&signal).expect("a candidate");
        assert_eq!(candidate.category, localmind_core::LessonCategory::ToolUse);
        // Evidence is redacted before it can be persisted.
        assert!(candidate.evidence().iter().all(|e| e.redacted));
    }

    #[test]
    fn enqueue_writes_a_review_gated_candidate_not_accepted_memory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let events = vec![repaired_event(
            "git_diff",
            "q-model",
            "bare_string_for_array",
        )];

        let enqueued = enqueue_repair_signals(root, &events).unwrap();
        assert_eq!(enqueued, 1, "one (model, tool) candidate enqueued");

        // It sits in the review queue, NOT in accepted memory.
        assert!(
            memory_list(root).unwrap().is_empty(),
            "a repair signal is never accepted memory; it is review-gated"
        );
        assert_eq!(review_list(root).unwrap().len(), 1, "one review candidate");

        // And it never auto-becomes an always-on rule cue (no per-model sprawl);
        // a human must promote an accepted candidate through the existing path.
        assert!(
            rule_cue_ids(root).is_empty(),
            "a repair signal does not auto-register a rule cue"
        );
    }

    #[test]
    fn the_same_pattern_offered_twice_does_not_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let events = vec![repaired_event(
            "git_diff",
            "q-model",
            "bare_string_for_array",
        )];
        assert_eq!(enqueue_repair_signals(root, &events).unwrap(), 1);
        // Re-offering the same pattern is deduped by the queue's canonical hash.
        assert_eq!(enqueue_repair_signals(root, &events).unwrap(), 0);
        assert_eq!(review_list(root).unwrap().len(), 1);
    }

    #[test]
    fn no_repair_events_enqueues_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(enqueue_repair_signals(dir.path(), &[]).unwrap(), 0);
    }
}
