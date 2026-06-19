//! Completion-retrospective lesson bridge.
//!
//! The harness completion retrospective (ADR-0035) records advisory lessons to the
//! root `LESSONS.md` — a human-editable mirror. This module *also* offers each lesson
//! to LocalMind's review-gated candidate queue, so a lesson can be promoted to accepted
//! memory by a human instead of living only in an un-gated file. It reuses the existing
//! review-gated path (no new store): a lesson is enqueued as a [`CandidateLesson`];
//! promotion to accepted memory stays a human, review-gated step (ADR-0011), and this
//! bridge never writes accepted memory.
//!
//! Unlike a loop-outcome lesson (a *patch outcome* carrying an accepted/rejected verdict
//! and a change-provenance ref), a retrospective lesson is a free-text advisory note:
//! it sets **no** fabricated outcome or provenance. It enters review with a lower prior
//! confidence than a human-confirmed patch outcome, and the review queue's own
//! canonical-hash dedup keeps a repeated lesson from piling up.

use std::path::Path;

use localmind_core::{
    CandidateLesson, Confidence, EvidenceKind, EvidenceRef, LessonCategory, LessonId,
    SessionId as LearningSessionId, SuggestedAction,
};
use localmind_store::ReviewQueue;

use crate::error::LearningError;
use crate::loop_lesson::fnv_hex;

/// Advisory confidence for a completion-retrospective candidate. Deliberately below the
/// loop-outcome `0.75`: a retrospective lesson is an unverified self-observation, not a
/// human-confirmed patch outcome, so it enters review with lower prior trust.
const RETROSPECTIVE_CONFIDENCE: f32 = 0.4;

/// Minimum trimmed length for a lesson to be worth a review candidate — filters empty
/// or sentinel bullets without trying to judge content.
const MIN_LESSON_CHARS: usize = 8;

/// The review session label retrospective candidates are enqueued under.
const RETROSPECTIVE_SESSION: &str = "completion-retrospective";

/// One advisory lesson from a completion retrospective, ready to offer to review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrospectiveLesson {
    /// The lesson text as written to `LESSONS.md` (one line, already condensed).
    pub text: String,
}

impl RetrospectiveLesson {
    /// A lesson from its text.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    /// Whether the lesson clears the quality bar: long enough to be a real statement
    /// rather than an empty or sentinel bullet.
    fn is_substantive(&self) -> bool {
        self.text.trim().chars().count() >= MIN_LESSON_CHARS
    }

    /// A stable, content-addressed candidate id, so re-offering the same lesson does not
    /// mint a second id (the review queue also dedups by canonical summary hash).
    fn id(&self) -> String {
        format!("retro-{}", fnv_hex(self.text.trim().as_bytes()))
    }
}

/// Offer a completion-retrospective lesson to LocalMind's review-gated queue as a
/// candidate. Returns the enqueued candidate id, or `None` when the lesson is skipped
/// (below the quality bar, or already pending — the queue dedups by canonical hash).
///
/// Advisory and review-gated: the candidate is `PromoteToMemory`, never accepted memory;
/// promotion stays a human step (ADR-0011).
///
/// # Errors
/// [`LearningError::Review`] if the project store cannot be initialized or the review
/// queue enqueue fails.
pub fn write_retrospective_lesson(
    project_root: &Path,
    lesson: &RetrospectiveLesson,
) -> Result<Option<String>, LearningError> {
    if !lesson.is_substantive() {
        return Ok(None);
    }
    crate::initialize(project_root).map_err(|e| LearningError::Review(e.to_string()))?;

    let confidence = Confidence::new(RETROSPECTIVE_CONFIDENCE)
        .map_err(|e| LearningError::Review(e.to_string()))?;
    let id = lesson.id();
    let candidate = CandidateLesson::new(
        LessonId::new(id.clone()),
        lesson.text.trim().to_string(),
        LessonCategory::Process,
        confidence,
        SuggestedAction::PromoteToMemory,
    )
    .with_evidence(
        EvidenceRef::new(
            EvidenceKind::Other("completion_retrospective".to_string()),
            "harness completion retrospective".to_string(),
        )
        .redacted(),
    );

    let queue = ReviewQueue::open_project(project_root)
        .map_err(|e| LearningError::Review(e.to_string()))?;
    let inserted = queue
        .enqueue_candidates(&LearningSessionId::new(RETROSPECTIVE_SESSION), &[candidate])
        .map_err(|e| LearningError::Review(e.to_string()))?;
    // `inserted == 0` means the queue deduped this lesson against an existing pending
    // candidate (same canonical-hash summary): a no-op, not a second entry.
    Ok((inserted > 0).then_some(id))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::ops::{memory_list, promote, review_decide, review_list, ReviewVerdict};

    #[test]
    fn a_substantive_lesson_enqueues_one_review_candidate() {
        // Bug it prevents: a retrospective lesson silently never reaching the
        // review-gated queue (the F-8 gap).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let lesson = RetrospectiveLesson::new(
            "Thread a value between two steps via a column on the row they share.",
        );
        let id = write_retrospective_lesson(root, &lesson).unwrap();
        assert!(id.is_some(), "a substantive lesson should enqueue");

        let items = review_list(root).unwrap();
        assert_eq!(items.len(), 1, "exactly one candidate: {items:?}");
        assert!(items[0].summary.contains("Thread a value"));
    }

    #[test]
    fn the_same_lesson_offered_twice_does_not_duplicate() {
        // Bug it prevents: re-running the retrospective floods the review queue with
        // duplicate candidates.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let lesson = RetrospectiveLesson::new("Reuse the canonical redactor; never re-detect secrets.");

        let first = write_retrospective_lesson(root, &lesson).unwrap();
        assert!(first.is_some());
        let second = write_retrospective_lesson(root, &lesson).unwrap();
        assert!(second.is_none(), "a duplicate is deduped, not re-enqueued");

        assert_eq!(review_list(root).unwrap().len(), 1, "still one candidate");
    }

    #[test]
    fn a_too_short_or_empty_lesson_is_skipped() {
        // Bug it prevents: sentinel/empty bullets ("none", "") becoming review noise.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        assert!(write_retrospective_lesson(root, &RetrospectiveLesson::new("")).unwrap().is_none());
        assert!(write_retrospective_lesson(root, &RetrospectiveLesson::new("none")).unwrap().is_none());
        assert!(write_retrospective_lesson(root, &RetrospectiveLesson::new("  \n ")).unwrap().is_none());
        assert!(
            review_list(root).unwrap().is_empty(),
            "no candidate should have been enqueued"
        );
    }

    #[test]
    fn the_candidate_is_review_gated_not_accepted_memory() {
        // Bug it prevents: the bridge writing accepted memory directly (ADR-0011/0034).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_retrospective_lesson(
            root,
            &RetrospectiveLesson::new("Keep the TUI crate free of domain dependencies."),
        )
        .unwrap();

        // It sits in review, NOT in accepted memory, until a human promotes it.
        assert!(
            memory_list(root).unwrap().is_empty(),
            "nothing is accepted memory before review"
        );
        let item = review_list(root).unwrap().into_iter().next().unwrap();
        review_decide(root, &item.id, ReviewVerdict::Accept, "david", None).unwrap();
        let memory_id = promote(root, &item.id).unwrap();
        assert!(!memory_id.is_empty(), "only a human promotion reaches memory");
    }

    #[test]
    fn a_failing_store_returns_err_not_a_panic() {
        // The host wire is advisory (`if let Ok(Some(_))`): it swallows the result so a
        // finished run is never broken by a review enqueue. That is only safe if a
        // failure surfaces as Err, never a panic — pin that here with a non-directory
        // root (store init/open must fail cleanly).
        let file = tempfile::NamedTempFile::new().unwrap();
        let result = write_retrospective_lesson(
            file.path(),
            &RetrospectiveLesson::new("a lesson that cannot be stored"),
        );
        assert!(
            result.is_err(),
            "a non-directory root must Err cleanly, not panic: {result:?}"
        );
    }
}
