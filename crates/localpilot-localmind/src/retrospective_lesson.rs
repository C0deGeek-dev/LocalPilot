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
    CandidateLesson, Confidence, EvidenceKind, EvidenceRef, HindsightDraft, HindsightOutcome,
    LessonCategory, LessonId, SessionId as LearningSessionId, SuggestedAction,
};
use localmind_inference::ConstraintDisposition;
use localmind_store::{Distillation, ReviewQueue};

use crate::error::LearningError;

/// A small FNV-1a hex digest for stable, content-addressed lesson ids, so every
/// review candidate derives its id the same way.
pub(crate) fn fnv_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Advisory confidence for a completion-retrospective or driver-intervention candidate —
/// origins with no independent match-quality signal to derive one from. Deliberately below
/// the loop-outcome `0.75`: an unverified self-observation or correction, not a
/// human-confirmed patch outcome, so it enters review with lower prior trust. A research
/// finding instead carries its own [`RetrospectiveLesson::research_finding`] confidence.
const RETROSPECTIVE_CONFIDENCE: f32 = 0.4;

/// Minimum trimmed length for a lesson to be worth a review candidate — filters empty
/// or sentinel bullets without trying to judge content.
const MIN_LESSON_CHARS: usize = 8;

/// The review session label retrospective candidates are enqueued under.
const RETROSPECTIVE_SESSION: &str = "completion-retrospective";

/// The review session label research-finding candidates are enqueued under.
const RESEARCH_SESSION: &str = "research";

/// The review session label driver-intervention candidates are enqueued under.
const DRIVER_SESSION: &str = "driver-intervention";

/// Where an offered lesson came from. The queue entry carries this honestly:
/// a `/research` finding or a driver correction must never be presented as a
/// completion retrospective — the reviewer reads the label to judge what they
/// are looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// The harness completion retrospective (ADR-0035/0037).
    Retrospective,
    /// A supported finding from the research loop (ADR-0060).
    Research,
    /// A correction from an external driver steering the session (the MCP
    /// adapter's client).
    Driver,
}

impl Origin {
    fn session(self) -> &'static str {
        match self {
            Origin::Retrospective => RETROSPECTIVE_SESSION,
            Origin::Research => RESEARCH_SESSION,
            Origin::Driver => DRIVER_SESSION,
        }
    }

    fn id_prefix(self) -> &'static str {
        match self {
            Origin::Retrospective => "retro",
            Origin::Research => "research",
            Origin::Driver => "driver",
        }
    }

    fn evidence_kind(self) -> &'static str {
        match self {
            Origin::Retrospective => "completion_retrospective",
            Origin::Research => "research_finding",
            Origin::Driver => "driver_intervention",
        }
    }

    fn evidence_detail(self) -> &'static str {
        match self {
            Origin::Retrospective => "harness completion retrospective",
            Origin::Research => "research loop finding",
            Origin::Driver => "external driver intervention",
        }
    }
}

/// One advisory lesson ready to offer to review — from a completion
/// retrospective, a research finding, or an external driver's correction,
/// all riding the same review-gated queue.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrospectiveLesson {
    /// The lesson text as written to `LESSONS.md` (one line, already condensed).
    pub text: String,
    origin: Origin,
    /// Overrides the origin's generic evidence detail (e.g. names the driving
    /// client), so the reviewer sees exactly who corrected the session.
    evidence_note: Option<String>,
    /// Overrides [`RETROSPECTIVE_CONFIDENCE`] when `Some` — a research finding
    /// carries its own relevance-derived confidence rather than the flat
    /// completion-retrospective prior. `None` for origins with no independent
    /// quality signal (a self-observation, a driver's correction).
    confidence: Option<f32>,
    /// Full carried source evidence (a research finding's bounded page text),
    /// offered to review **separately** from the lesson text so the reviewer
    /// sees the complete source while promotion writes only the lesson.
    evidence_text: Option<String>,
    /// The lesson text is a provenance-backed excerpt, not a standalone
    /// reusable statement: the queue entry demands a reviewer edit before
    /// promotion.
    requires_edit: bool,
    /// Facts captured from the run the lesson came out of, attached as the
    /// candidate's evidence beside its origin reference.
    facts: Vec<EvidenceRef>,
    /// The evidence-linked hindsight the lesson came out of. Its hypotheses cite
    /// the facts above by id.
    hindsight: Option<HindsightDraft>,
    /// Not a lesson: a record of an analysis that could not, or chose not to,
    /// propose one. Queued so a person can see it, and never promotable as is.
    review_only: bool,
}

impl RetrospectiveLesson {
    /// A completion-retrospective lesson from its text.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            origin: Origin::Retrospective,
            evidence_note: None,
            confidence: None,
            evidence_text: None,
            requires_edit: false,
            facts: Vec::new(),
            hindsight: None,
            review_only: false,
        }
    }

    /// A research-loop finding from its text and its own relevance-derived
    /// `confidence` (`0.0..=1.0`, already capped by the caller — see
    /// `localpilot-research`'s `candidates_from`). Same review-gated queue,
    /// honest provenance: the queue entry is labelled `research`, not
    /// `completion-retrospective`, and its confidence reflects the finding's
    /// actual match quality rather than a flat prior.
    #[must_use]
    pub fn research_finding(text: impl Into<String>, confidence: f32) -> Self {
        Self {
            text: text.into(),
            origin: Origin::Research,
            evidence_note: None,
            confidence: Some(confidence.clamp(0.0, 1.0)),
            evidence_text: None,
            requires_edit: false,
            facts: Vec::new(),
            hindsight: None,
            review_only: false,
        }
    }

    /// A correction captured from an external driver steering the session.
    /// Same review-gated queue; the evidence names the driving client so the
    /// candidate never masquerades as the session's own retrospective.
    #[must_use]
    pub fn driver_intervention(text: impl Into<String>, client: impl AsRef<str>) -> Self {
        Self {
            text: text.into(),
            origin: Origin::Driver,
            evidence_note: Some(format!(
                "correction by the driving client {}",
                client.as_ref()
            )),
            confidence: None,
            evidence_text: None,
            requires_edit: false,
            facts: Vec::new(),
            hindsight: None,
            review_only: false,
        }
    }

    /// Attach the finding's full bounded source evidence — carried to review
    /// separately from the lesson text, never into a promoted memory body.
    #[must_use]
    pub fn with_evidence_text(mut self, evidence_text: impl Into<String>) -> Self {
        self.evidence_text = Some(evidence_text.into());
        self
    }

    /// Attach the facts captured from the completed run this lesson came out
    /// of. The facts become the candidate's evidence; the run's gaps — what was
    /// *not* recorded — ride in the carried evidence text, where a reviewer
    /// reads them and nothing can cite them.
    #[must_use]
    pub fn with_run_facts(mut self, run: &crate::RunFacts) -> Self {
        self.facts = run.facts.clone();
        if let Some(gaps) = run.render_gaps() {
            self.evidence_text = Some(match self.evidence_text.take() {
                Some(existing) => format!("{existing}\n\n{gaps}"),
                None => gaps,
            });
        }
        self
    }

    /// What a finished run's hindsight earns in review, or `None` when it earns
    /// nothing.
    ///
    /// A `Candidate` becomes a lesson whose text is the draft's proposed lesson.
    /// `NeedsReview` and `Malformed` become review-only records, so a person sees
    /// what could not be distilled. `UnknownCause` and `NoLesson` are successful
    /// outcomes and queue nothing — unless the project asked to record them, in
    /// which case they too become review-only records. Every record carries the
    /// run's facts, the draft, and — in the carried text — the gaps and how the
    /// analysis went.
    #[must_use]
    pub fn from_hindsight(
        distillation: &Distillation,
        run: &crate::RunFacts,
        run_name: &str,
        record_abstentions: bool,
    ) -> Option<Self> {
        let reasons = distillation
            .reasons
            .iter()
            .map(localmind_store::OutcomeReason::describe)
            .collect::<Vec<_>>()
            .join("; ");
        let (text, review_only) = match distillation.outcome {
            HindsightOutcome::Candidate => (distillation.draft.proposed_lesson.clone()?, false),
            HindsightOutcome::NeedsReview | HindsightOutcome::Malformed => (
                format!("Hindsight on `{run_name}` needs review: {reasons}"),
                true,
            ),
            HindsightOutcome::UnknownCause | HindsightOutcome::NoLesson => {
                if !record_abstentions {
                    return None;
                }
                (
                    format!("Hindsight on `{run_name}` found no lesson: {reasons}"),
                    true,
                )
            }
        };
        let mut lesson = Self::new(text).with_run_facts(run);
        let account = describe_distillation(distillation, &reasons);
        lesson.evidence_text = Some(match lesson.evidence_text.take() {
            Some(gaps) => format!("{account}\n\n{gaps}"),
            None => account,
        });
        lesson.hindsight = Some(distillation.draft.clone());
        lesson.review_only = review_only;
        Some(lesson)
    }

    /// Mark the lesson text as a source excerpt that a reviewer must distil
    /// into a standalone statement before promotion.
    #[must_use]
    pub fn requiring_edit(mut self) -> Self {
        self.requires_edit = true;
        self
    }

    /// Whether the lesson clears the quality bar: long enough to be a real statement
    /// rather than an empty or sentinel bullet.
    fn is_substantive(&self) -> bool {
        self.text.trim().chars().count() >= MIN_LESSON_CHARS
    }

    /// A stable, content-addressed candidate id, so re-offering the same lesson does not
    /// mint a second id (the review queue also dedups by canonical summary hash).
    pub(crate) fn id(&self) -> String {
        format!(
            "{}-{}",
            self.origin.id_prefix(),
            fnv_hex(self.text.trim().as_bytes())
        )
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

    let confidence = Confidence::new(lesson.confidence.unwrap_or(RETROSPECTIVE_CONFIDENCE))
        .map_err(|e| LearningError::Review(e.to_string()))?;
    let id = lesson.id();
    let action = if lesson.review_only {
        SuggestedAction::KeepForSession
    } else {
        SuggestedAction::PromoteToMemory
    };
    let detail = lesson
        .evidence_note
        .clone()
        .unwrap_or_else(|| lesson.origin.evidence_detail().to_string());
    let kind = EvidenceKind::Other(lesson.origin.evidence_kind().to_string());
    // A candidate carrying hindsight is checked against its whole evidence set,
    // and every fact in it must carry a verifiable id — the origin reference
    // included. Without hindsight the origin stays the label it always was.
    let origin = if lesson.hindsight.is_some() {
        EvidenceRef::identified(
            kind,
            detail,
            format!("localpilot:{}", lesson.origin.session()),
            "localpilot:completion",
            format!("fnv:{}", fnv_hex(lesson.text.trim().as_bytes())),
        )
        .redacted()
    } else {
        EvidenceRef::new(kind, detail).redacted()
    };
    let candidate = CandidateLesson::new(
        LessonId::new(id.clone()),
        lesson.text.trim().to_string(),
        LessonCategory::Process,
        confidence,
        action,
    )
    .with_evidence(origin);
    let candidate = lesson
        .facts
        .iter()
        .cloned()
        .fold(candidate, CandidateLesson::with_evidence);
    let candidate = match &lesson.hindsight {
        Some(draft) => {
            let candidate = candidate.with_hindsight(draft.clone());
            candidate.validate_hindsight().map_err(|violations| {
                LearningError::Review(format!(
                    "hindsight does not fit its own evidence: {}",
                    violations
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                ))
            })?;
            candidate
        }
        None => candidate,
    };
    // Carried source evidence rides its own candidate field: review surfaces
    // show it under the lesson, promotion writes only the lesson text
    // (LocalMind D-LM-0029) — the source dump never becomes searchable memory.
    let candidate = match &lesson.evidence_text {
        Some(evidence_text) => candidate.with_evidence_text(evidence_text.clone()),
        None => candidate,
    };
    let candidate = if lesson.requires_edit || lesson.review_only {
        candidate.requiring_edit_before_promotion()
    } else {
        candidate
    };

    let queue = ReviewQueue::open_project(project_root)
        .map_err(|e| LearningError::Review(e.to_string()))?;
    let inserted = queue
        .enqueue_candidates(
            &LearningSessionId::new(lesson.origin.session()),
            &[candidate],
        )
        .map_err(|e| LearningError::Review(e.to_string()))?;
    // `inserted == 0` means the queue deduped this lesson against an existing pending
    // candidate (same canonical-hash summary): a no-op, not a second entry.
    Ok((inserted > 0).then_some(id))
}

/// How a distillation went, for the reviewer: the outcome and why, how many
/// model calls it took, whether the repair pass was spent, and what became of
/// any output constraint.
fn describe_distillation(distillation: &Distillation, reasons: &str) -> String {
    let trace = &distillation.trace;
    let mut out = format!(
        "Hindsight: {:?} — {} model call(s)",
        distillation.outcome, trace.model_calls
    );
    if trace.repair_spent {
        out.push_str(", one repair");
    }
    if trace.fallback {
        out.push_str(", no usable analysis (the draft holds only the run's intent and end state)");
    }
    let refused = trace
        .dispositions
        .iter()
        .any(|disposition| *disposition == ConstraintDisposition::RefusedByTransport);
    let requested = trace
        .dispositions
        .iter()
        .any(|disposition| *disposition == ConstraintDisposition::Requested);
    if refused {
        out.push_str(", the server refused the output schema");
    } else if requested {
        out.push_str(", an output schema was requested (the reply was validated regardless)");
    }
    if trace.excerpts_dropped > 0 {
        out.push_str(&format!(
            ", {} excerpt(s) left out to fit the context",
            trace.excerpts_dropped
        ));
    }
    if !reasons.is_empty() {
        out.push_str(&format!(".\nWhy: {reasons}"));
    }
    out
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
    fn a_research_finding_is_labelled_research_not_completion_retrospective() {
        // Bug it prevents: a /research memory candidate masquerading in the
        // review queue as a completion retrospective, leaving the reviewer
        // unable to tell what they are looking at.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let lesson = RetrospectiveLesson::research_finding(
            "Prefer virtual scrolling for long lists. (research finding; sources: web)",
            0.4,
        );
        let id = write_retrospective_lesson(root, &lesson).unwrap().unwrap();
        assert!(id.starts_with("research-"), "id carries the origin: {id}");

        let items = review_list(root).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].session_id, "research");
    }

    #[test]
    fn a_research_findings_confidence_reflects_its_own_relevance_not_a_flat_prior() {
        // Bug it prevents: every research finding reading the same hardcoded
        // 0.4 in the review queue regardless of how strong (or weak/
        // incidental) the underlying match actually was.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let weak = RetrospectiveLesson::research_finding("A weak, low-relevance match.", 0.05);
        let strong = RetrospectiveLesson::research_finding("A strong, well-matched finding.", 0.9);
        write_retrospective_lesson(root, &weak).unwrap();
        write_retrospective_lesson(root, &strong).unwrap();

        let items = review_list(root).unwrap();
        let weak_item = items
            .iter()
            .find(|item| item.summary.contains("weak"))
            .unwrap();
        let strong_item = items
            .iter()
            .find(|item| item.summary.contains("strong"))
            .unwrap();
        assert!((weak_item.confidence - 0.05).abs() < f32::EPSILON);
        assert!((strong_item.confidence - 0.9).abs() < f32::EPSILON);
        assert_ne!(
            weak_item.confidence, strong_item.confidence,
            "confidence must vary with the finding's own relevance"
        );
    }

    #[test]
    fn a_driver_intervention_names_its_client_and_session_label() {
        // Bug it prevents: a steering client's correction masquerading in the
        // review queue as the session's own retrospective (the reviewer must
        // see who actually said it).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let lesson = RetrospectiveLesson::driver_intervention(
            "Run the failing test before editing; the coach had to redirect a blind fix.",
            "claude-code 2.1.0",
        );
        let id = write_retrospective_lesson(root, &lesson).unwrap().unwrap();
        assert!(id.starts_with("driver-"), "id carries the origin: {id}");

        let items = review_list(root).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].session_id, "driver-intervention");
    }

    #[test]
    fn the_same_lesson_offered_twice_does_not_duplicate() {
        // Bug it prevents: re-running the retrospective floods the review queue with
        // duplicate candidates.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let lesson =
            RetrospectiveLesson::new("Reuse the canonical redactor; never re-detect secrets.");

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

        assert!(
            write_retrospective_lesson(root, &RetrospectiveLesson::new(""))
                .unwrap()
                .is_none()
        );
        assert!(
            write_retrospective_lesson(root, &RetrospectiveLesson::new("none"))
                .unwrap()
                .is_none()
        );
        assert!(
            write_retrospective_lesson(root, &RetrospectiveLesson::new("  \n "))
                .unwrap()
                .is_none()
        );
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
        assert!(
            !memory_id.is_empty(),
            "only a human promotion reaches memory"
        );
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
