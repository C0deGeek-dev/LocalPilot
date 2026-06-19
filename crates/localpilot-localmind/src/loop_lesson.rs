//! Loop-outcome lesson writeback (LocalMind `D-LM-0014`).
//!
//! When a human accepts or rejects a proposed patch, the outcome is written back
//! as a durable lesson so the next loop run retrieves it and stops repeating a
//! mistake. This reuses the **existing** review-gated path (no new store): a
//! loop-outcome lesson is enqueued as a [`CandidateLesson`]; promotion to
//! accepted memory stays a human, review-gated step (ADR-0011). A **rejected**
//! outcome is a first-class negative signal — an [`LessonCategory::AntiPattern`]
//! candidate that records what was proposed and why it was rejected — not the
//! absence of a lesson. Lessons carry provenance (the change-provenance ref) and
//! outcome; a bad lesson is curated/superseded through the existing memory-delete
//! and review-reject paths, guarding against store pollution.

use std::path::Path;

use localmind_core::{
    CandidateLesson, Confidence, EvidenceKind, EvidenceRef, LessonCategory, LessonId,
    SessionId as LearningSessionId, SuggestedAction,
};
use localmind_store::ReviewQueue;
use serde::{Deserialize, Serialize};

use crate::error::LearningError;

/// The human's decision on a proposed patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopOutcome {
    /// The patch was approved.
    Accepted,
    /// The patch was rejected — recorded as a negative signal.
    Rejected,
}

/// A loop-outcome lesson on top of the existing lesson schema:
/// `{ trigger, what, why, applies_to, outcome, provenance_ref }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopLesson {
    /// What situation the lesson applies to (the retrieval cue).
    pub trigger: String,
    /// What the loop proposed (or what to do / avoid).
    pub what: String,
    /// Why — the rationale behind the outcome.
    pub why: String,
    /// Files/areas the lesson applies to (retrieval + finding-match cues).
    pub applies_to: Vec<String>,
    /// Whether the human accepted or rejected the proposal.
    pub outcome: LoopOutcome,
    /// A reference to the change-provenance record (e.g. its hub path/URI).
    pub provenance_ref: String,
}

impl LoopLesson {
    /// An accepted-outcome lesson.
    #[must_use]
    pub fn accepted(
        trigger: impl Into<String>,
        what: impl Into<String>,
        why: impl Into<String>,
        applies_to: Vec<String>,
        provenance_ref: impl Into<String>,
    ) -> Self {
        Self {
            trigger: trigger.into(),
            what: what.into(),
            why: why.into(),
            applies_to,
            outcome: LoopOutcome::Accepted,
            provenance_ref: provenance_ref.into(),
        }
    }

    /// A rejected-outcome (negative-signal) lesson.
    #[must_use]
    pub fn rejected(
        trigger: impl Into<String>,
        what: impl Into<String>,
        why: impl Into<String>,
        applies_to: Vec<String>,
        provenance_ref: impl Into<String>,
    ) -> Self {
        Self {
            trigger: trigger.into(),
            what: what.into(),
            why: why.into(),
            applies_to,
            outcome: LoopOutcome::Rejected,
            provenance_ref: provenance_ref.into(),
        }
    }

    /// Map the outcome onto an existing lesson category: a rejected outcome is an
    /// anti-pattern (negative signal); an accepted one is a process lesson.
    fn category(&self) -> LessonCategory {
        match self.outcome {
            LoopOutcome::Accepted => LessonCategory::Process,
            LoopOutcome::Rejected => LessonCategory::AntiPattern,
        }
    }

    /// The lesson summary, framed by outcome so a rejected lesson reads as a
    /// steer-away signal at retrieval.
    fn summary(&self) -> String {
        match self.outcome {
            LoopOutcome::Accepted => format!("When {}: {}", self.trigger.trim(), self.what.trim()),
            LoopOutcome::Rejected => {
                format!(
                    "Avoid (rejected): {} — {}",
                    self.what.trim(),
                    self.why.trim()
                )
            }
        }
    }

    /// A stable id derived from the lesson content, so re-writing the same outcome
    /// does not duplicate it.
    fn id(&self) -> String {
        let key = format!("{:?}|{}|{}", self.outcome, self.trigger, self.what);
        format!("loop-{}", fnv_hex(key.as_bytes()))
    }
}

/// Write a loop-outcome lesson as a review candidate through the existing
/// review-gated path. Returns the candidate's lesson id.
///
/// # Errors
/// [`LearningError::Review`] if the project config, confidence value, or review
/// queue enqueue fails.
pub fn write_loop_lesson(
    project_root: &Path,
    lesson: &LoopLesson,
) -> Result<String, LearningError> {
    crate::initialize(project_root).map_err(|e| LearningError::Review(e.to_string()))?;
    // Both outcomes are review-gated; the reviewer adjusts confidence on accept.
    let confidence = Confidence::new(0.75).map_err(|e| LearningError::Review(e.to_string()))?;
    let id = lesson.id();
    let mut candidate = CandidateLesson::new(
        LessonId::new(id.clone()),
        lesson.summary(),
        lesson.category(),
        confidence,
        SuggestedAction::PromoteToMemory,
    );
    candidate.rationale = Some(lesson.why.clone());
    candidate.related_files = lesson.applies_to.clone();
    let mut evidence = EvidenceRef::new(
        EvidenceKind::Other("self_improvement_outcome".to_string()),
        format!("{:?}", lesson.outcome),
    )
    .redacted();
    evidence.uri = Some(lesson.provenance_ref.clone());
    let candidate = candidate.with_evidence(evidence);

    let queue = ReviewQueue::open_project(project_root)
        .map_err(|e| LearningError::Review(e.to_string()))?;
    queue
        .enqueue_candidates(
            &LearningSessionId::new("self-improvement-loop"),
            &[candidate],
        )
        .map_err(|e| LearningError::Review(e.to_string()))?;
    Ok(id)
}

/// A small FNV-1a hex digest for stable lesson ids.
fn fnv_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::ops::{
        memory_delete, memory_list, promote, review_decide, review_list, ReviewVerdict,
    };

    #[test]
    fn accepted_and_rejected_lessons_enqueue_with_the_right_category() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let accepted = LoopLesson::accepted(
            "the registry lags the decision log",
            "bump REGISTRY in the same change as a new ADR",
            "an index that drifts from its source misleads",
            vec!["REGISTRY.md".to_string()],
            "hub://provenance/abc",
        );
        let rejected = LoopLesson::rejected(
            "a TODO in worker.rs",
            "delete the TODO without handling retries",
            "the retry path is still missing; deleting the marker hides real work",
            vec!["src/worker.rs".to_string()],
            "hub://provenance/def",
        );

        let acc_id = write_loop_lesson(root, &accepted).unwrap();
        let rej_id = write_loop_lesson(root, &rejected).unwrap();
        assert_ne!(acc_id, rej_id);

        let items = review_list(root).unwrap();
        assert_eq!(
            items.len(),
            2,
            "both outcomes enqueue a candidate: {items:?}"
        );
        // The rejected lesson is framed as a steer-away signal.
        assert!(
            items
                .iter()
                .any(|i| i.summary.starts_with("Avoid (rejected)")),
            "{items:?}"
        );
    }

    #[test]
    fn an_accepted_loop_lesson_is_retrievable_after_promotion() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let lesson = LoopLesson::accepted(
            "a stale doc link",
            "fix or remove the broken link in docs/guide.md",
            "broken links erode doc trust",
            vec!["docs/guide.md".to_string()],
            "hub://provenance/ghi",
        );
        write_loop_lesson(root, &lesson).unwrap();

        // The reviewer accepts and promotes it: now it is accepted memory and a
        // later self-review run can retrieve it via memory_list (which the CLI
        // feeds into the scan as prior lessons).
        let item = review_list(root).unwrap().into_iter().next().unwrap();
        review_decide(root, &item.id, ReviewVerdict::Accept, "david", None).unwrap();
        let memory_id = promote(root, &item.id).unwrap();
        assert!(!memory_id.is_empty());

        let memories = memory_list(root).unwrap();
        assert!(
            memories.iter().any(|m| m.body.contains("broken link")),
            "the promoted loop lesson must be retrievable: {memories:?}"
        );

        // Curation/supersede: a bad accepted lesson can be deleted.
        memory_delete(root, &memory_id).unwrap();
        assert!(
            !memory_list(root).unwrap().iter().any(|m| m.id == memory_id),
            "curation should remove the polluting lesson"
        );
    }

    #[test]
    fn a_rejected_candidate_can_be_curated_without_reaching_memory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let lesson = LoopLesson::rejected(
            "a risky refactor",
            "rewrite the parser wholesale",
            "too broad; out of scope for the finding",
            vec!["src/parser.rs".to_string()],
            "hub://provenance/jkl",
        );
        write_loop_lesson(root, &lesson).unwrap();
        let item = review_list(root).unwrap().into_iter().next().unwrap();
        // Rejecting the candidate curates it; it never becomes accepted memory.
        review_decide(root, &item.id, ReviewVerdict::Reject, "david", None).unwrap();
        assert!(
            memory_list(root).unwrap().is_empty(),
            "a rejected candidate must not reach accepted memory"
        );
    }
}
