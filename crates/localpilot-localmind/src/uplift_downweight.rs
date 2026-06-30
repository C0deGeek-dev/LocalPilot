//! Outcome-aware down-weight wired to the uplift eval (ADR-0046).
//!
//! `flag_unhelpful_lesson` (the engine's reasoned route-to-review flag,
//! D-LM-0016) was built but never wired to an outcome signal. This module wires
//! it to the **uplift A/B eval**, not a live turn: when an arm that injected a
//! set of lessons under-performs its control, those lessons are routed to review
//! (never deleted) so a human re-judges them. A single live turn is too weak a
//! signal to condemn a lesson on (ADR-0046), so the join is the A/B verdict, keyed
//! by the per-turn `memories_used` audit — the same audit the inspector reads.
//!
//! The whole path is **off by default** (`[memory] outcome_downweight`) and
//! reversible (route-to-review, never delete), so enabling it can only surface
//! lessons for a human, never silently lose one.

use std::collections::BTreeSet;
use std::path::Path;

use localpilot_store::MemoryUsed;

use crate::error::LearningError;

/// One arm's uplift-eval outcome: the A/B verdict plus the memories injected into
/// the arm's turns. The memories come from the `memories_used` audit (e.g.
/// [`crate::last_turn_memories_used`]) — the canonical join key.
#[derive(Debug, Clone)]
pub struct UpliftArmOutcome {
    /// The A/B verdict: this arm scored worse than its control.
    pub underperformed_control: bool,
    /// The memories injected into this arm's turns (the `memories_used` audit).
    pub memories_used: Vec<MemoryUsed>,
}

impl UpliftArmOutcome {
    /// Construct from a verdict and a turn's used-memory audit.
    #[must_use]
    pub fn new(underperformed_control: bool, memories_used: Vec<MemoryUsed>) -> Self {
        Self {
            underperformed_control,
            memories_used,
        }
    }
}

/// The audit layer that marks an injected accepted-memory lesson (as opposed to a
/// repository primer, an ingest chunk, or a rule cue). Only these ids are
/// re-judgeable accepted lessons, so only these are eligible for down-weight.
const MEMORY_LAYER: &str = "memory";

/// Route the accepted-memory lessons injected into an under-performing arm to
/// review (D-LM-0016) — never deleting them. A no-op unless `enabled` (config
/// `outcome_downweight`, default off) **and** the arm actually under-performed its
/// control; otherwise nothing is flagged. Only `memory`-layer ids are eligible (a
/// primer/ingest/rule-cue id is not a re-judgeable lesson), and ids are
/// de-duplicated so one lesson is flagged once. Returns the ids that matched an
/// active memory and were newly flagged.
///
/// # Errors
/// Returns [`LearningError::Memory`] if the store cannot be opened or updated.
pub fn downweight_unhelpful_lessons(
    project_root: &Path,
    outcome: &UpliftArmOutcome,
    enabled: bool,
) -> Result<Vec<String>, LearningError> {
    if !enabled || !outcome.underperformed_control {
        return Ok(Vec::new());
    }
    let mut seen = BTreeSet::new();
    let mut flagged = Vec::new();
    for memory in &outcome.memories_used {
        if memory.layer != MEMORY_LAYER || !seen.insert(memory.id.clone()) {
            continue;
        }
        if crate::ops::flag_unhelpful_lesson(project_root, &memory.id)? {
            flagged.push(memory.id.clone());
        }
    }
    Ok(flagged)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::ops::{lessons_flagged_for_review, memory_list};

    fn seed(root: &Path, body: &str) -> String {
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        let lesson = crate::SeedLesson {
            body: body.to_string(),
            category: Some("Process".to_string()),
            confidence: Some(0.8),
            related_files: Vec::new(),
            related_entities: Vec::new(),
            evidence: None,
            tags: Vec::new(),
        };
        crate::seed_memory(root, &[lesson], false).unwrap();
        memory_list(root).unwrap()[0].id.clone()
    }

    fn used(id: &str, layer: &str) -> MemoryUsed {
        MemoryUsed {
            id: id.to_string(),
            score: 0,
            layer: layer.to_string(),
        }
    }

    #[test]
    fn an_underperforming_arms_lesson_is_flagged_for_review_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let id = seed(root, "a lesson that did not help on the eval");

        let outcome = UpliftArmOutcome::new(true, vec![used(&id, MEMORY_LAYER)]);
        let flagged = downweight_unhelpful_lessons(root, &outcome, true).unwrap();
        assert_eq!(flagged, vec![id.clone()], "the injected lesson is flagged");
        assert!(
            lessons_flagged_for_review(root).unwrap().contains(&id),
            "it surfaces in the review list"
        );
        assert!(
            memory_list(root).unwrap().iter().any(|m| m.id == id),
            "down-weighting must never delete the memory"
        );
    }

    #[test]
    fn nothing_is_flagged_when_the_lever_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let id = seed(root, "a lesson that did not help on the eval");

        let outcome = UpliftArmOutcome::new(true, vec![used(&id, MEMORY_LAYER)]);
        // Off by default: even an under-performing arm flags nothing.
        let flagged = downweight_unhelpful_lessons(root, &outcome, false).unwrap();
        assert!(flagged.is_empty(), "the default-off lever flags nothing");
        assert!(lessons_flagged_for_review(root).unwrap().is_empty());
    }

    #[test]
    fn an_arm_that_met_or_beat_control_flags_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let id = seed(root, "a lesson that helped on the eval");

        let outcome = UpliftArmOutcome::new(false, vec![used(&id, MEMORY_LAYER)]);
        let flagged = downweight_unhelpful_lessons(root, &outcome, true).unwrap();
        assert!(
            flagged.is_empty(),
            "a lesson is only down-weighted when its arm underperformed control"
        );
    }

    #[test]
    fn the_join_key_selects_only_memory_layer_ids() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let id = seed(root, "a lesson that did not help on the eval");

        // A primer/ingest/rule-cue id rides in the same audit but is not a
        // re-judgeable accepted lesson, so it is never flagged; the memory-layer id
        // is. A repeated memory id is flagged once.
        let outcome = UpliftArmOutcome::new(
            true,
            vec![
                used("<repository-primer>", "primer"),
                used("some/chunk", "ingest"),
                used(&id, MEMORY_LAYER),
                used(&id, MEMORY_LAYER),
            ],
        );
        let flagged = downweight_unhelpful_lessons(root, &outcome, true).unwrap();
        assert_eq!(
            flagged,
            vec![id],
            "only the memory-layer id is flagged, exactly once"
        );
    }
}
