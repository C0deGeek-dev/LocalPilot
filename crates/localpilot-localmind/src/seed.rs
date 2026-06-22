//! Seed curated, author-reviewed lessons directly into LocalMind accepted memory.
//!
//! A host with its own review surface may write accepted memory directly through
//! `MemoryPersistence::persist_memory_entry` (sanctioned in `localmind-store`),
//! bypassing the in-session candidate→review→promote queue. This module wraps
//! that for a curated best-practice lesson pack: validated JSON in, idempotent
//! body-level dedup, one accepted memory per new lesson. The lessons are
//! author-reviewed before seeding — the human gate moves to authoring time
//! rather than the per-session queue.

use std::collections::HashSet;
use std::path::Path;

use localmind_core::{
    Confidence, EvidenceKind, EvidenceRef, LessonCategory, MemoryEntry, MemoryEntryId, MemoryScope,
    MemoryStatus,
};
use serde::Deserialize;

use crate::error::LearningError;
use crate::ops::{memory_list, open_memory};

/// One curated lesson in a seed pack (`localpilot learning seed --file`).
#[derive(Debug, Clone, Deserialize)]
pub struct SeedLesson {
    /// The lesson text injected into a turn's context. Required and non-empty.
    pub body: String,
    /// LocalMind lesson category (e.g. `Process`, `AntiPattern`, `ToolUse`,
    /// `DebuggingRecipe`); defaults to `Process`. Unknown names become `Other`.
    #[serde(default)]
    pub category: Option<String>,
    /// Confidence in `0.0..=1.0`; defaults to `0.8`.
    #[serde(default)]
    pub confidence: Option<f32>,
    /// Files this lesson relates to (retrieval / anchoring hints).
    #[serde(default)]
    pub related_files: Vec<String>,
    /// Symbols / entities this lesson relates to.
    #[serde(default)]
    pub related_entities: Vec<String>,
    /// Free-text provenance note, recorded as manual-note evidence.
    #[serde(default)]
    pub evidence: Option<String>,
    /// Retrieval tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A seed-pack file: `{ "lessons": [ SeedLesson, ... ] }`.
#[derive(Debug, Clone, Deserialize)]
pub struct SeedPack {
    /// The curated lessons to seed.
    pub lessons: Vec<SeedLesson>,
}

/// Outcome of a seed run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedReport {
    /// Lessons written (or, on a dry run, that would be written).
    pub seeded: usize,
    /// Lessons skipped because their body already exists or is empty.
    pub skipped: usize,
}

fn parse_category(name: Option<&str>) -> LessonCategory {
    match name.map(str::trim).unwrap_or("Process") {
        "UserPreference" => LessonCategory::UserPreference,
        "ProjectConvention" => LessonCategory::ProjectConvention,
        "ArchitectureRule" => LessonCategory::ArchitectureRule,
        "CodePattern" => LessonCategory::CodePattern,
        "DebuggingRecipe" => LessonCategory::DebuggingRecipe,
        "ToolingNote" => LessonCategory::ToolingNote,
        "TestingStrategy" => LessonCategory::TestingStrategy,
        "DeploymentRule" => LessonCategory::DeploymentRule,
        "AntiPattern" => LessonCategory::AntiPattern,
        "SecurityWarning" => LessonCategory::SecurityWarning,
        "DocumentationUpdate" => LessonCategory::DocumentationUpdate,
        "CandidateSkill" => LessonCategory::CandidateSkill,
        "Process" => LessonCategory::Process,
        "ToolUse" => LessonCategory::ToolUse,
        other => LessonCategory::Other(other.to_string()),
    }
}

/// Lowercase + whitespace-collapse a body for dedup comparison.
fn normalize(body: &str) -> String {
    body.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Stable id derived from the normalized body (FNV-1a 64), so re-seeding the same
/// lesson keys to the same memory id rather than a fresh one.
fn seed_id(body: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in normalize(body).as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("seed-{hash:016x}")
}

/// Seed curated lessons into accepted memory, skipping any whose body already
/// exists (idempotent re-seed) or is empty. With `dry_run`, count without
/// writing anything.
///
/// # Errors
/// Returns [`LearningError::Memory`] if the store cannot be read or written, or
/// if a lesson's confidence is outside `0.0..=1.0`.
pub fn seed_memory(
    project_root: &Path,
    lessons: &[SeedLesson],
    dry_run: bool,
) -> Result<SeedReport, LearningError> {
    let mut seen: HashSet<String> = memory_list(project_root)?
        .into_iter()
        .map(|entry| normalize(&entry.body))
        .collect();
    let persistence = if dry_run {
        None
    } else {
        Some(open_memory(project_root)?)
    };
    let mut report = SeedReport {
        seeded: 0,
        skipped: 0,
    };
    // Ids of lessons opting into always-on rule-cue promotion (the `rule-cue`
    // tag), registered after the loop so a curated lesson can be a terse,
    // always-present rule rather than a retrieval-only memory.
    let mut promoted_cue_ids: Vec<String> = Vec::new();

    for lesson in lessons {
        let key = normalize(&lesson.body);
        if lesson.body.trim().is_empty() || !seen.insert(key) {
            report.skipped += 1;
            continue;
        }
        let id = seed_id(&lesson.body);
        if lesson
            .tags
            .iter()
            .any(|tag| tag == crate::rule_cue::RULE_CUE_TAG)
        {
            promoted_cue_ids.push(id.clone());
        }
        let confidence = Confidence::new(lesson.confidence.unwrap_or(0.8))
            .map_err(|e| LearningError::Memory(format!("invalid confidence: {e}")))?;
        let evidence = lesson
            .evidence
            .as_deref()
            .map(|note| EvidenceRef::new(EvidenceKind::ManualNote, note))
            .into_iter()
            .collect();
        let entry = MemoryEntry {
            id: MemoryEntryId::new(id),
            scope: MemoryScope::Project,
            body: lesson.body.trim().to_string(),
            category: parse_category(lesson.category.as_deref()),
            confidence,
            source_session: None,
            evidence,
            tags: lesson.tags.clone(),
            related_files: lesson.related_files.clone(),
            related_entities: lesson.related_entities.clone(),
            created_at: None,
            updated_at: None,
            supersedes: Vec::new(),
            contradicts: Vec::new(),
            status: MemoryStatus::Active,
        };
        if let Some(persistence) = &persistence {
            persistence
                .persist_memory_entry(&entry)
                .map_err(|e| LearningError::Memory(e.to_string()))?;
        }
        report.seeded += 1;
    }
    // Persist the rule-cue promotions once (host-side registry); a dry run writes
    // nothing, matching the no-store contract above.
    if !dry_run {
        crate::rule_cue::register_rule_cues(project_root, &promoted_cue_ids)?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lesson(body: &str) -> SeedLesson {
        SeedLesson {
            body: body.to_string(),
            category: Some("Process".to_string()),
            confidence: Some(0.7),
            related_files: Vec::new(),
            related_entities: Vec::new(),
            evidence: Some("test".to_string()),
            tags: Vec::new(),
        }
    }

    #[test]
    fn seeds_new_lessons_and_skips_duplicates_on_reseed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();

        let pack = vec![
            lesson("verify a hypothesis with the cheapest discriminating test"),
            lesson("check an existing target before launching your own"),
        ];
        let first = seed_memory(root, &pack, false).unwrap();
        assert_eq!(
            first,
            SeedReport {
                seeded: 2,
                skipped: 0
            }
        );

        // Re-seeding the same pack is idempotent — every lesson already present.
        let second = seed_memory(root, &pack, false).unwrap();
        assert_eq!(
            second,
            SeedReport {
                seeded: 0,
                skipped: 2
            }
        );

        // The seeded lessons are retrievable.
        let hits = crate::ops::search(root, "discriminating test").unwrap();
        assert!(hits.iter().any(|h| h.snippet.contains("discriminating")));
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();

        let pack = vec![lesson("a dry-run lesson that must not persist")];
        let report = seed_memory(root, &pack, true).unwrap();
        assert_eq!(
            report,
            SeedReport {
                seeded: 1,
                skipped: 0
            }
        );
        assert!(memory_list(root).unwrap().is_empty());
    }

    #[test]
    fn a_rule_cue_tagged_lesson_is_registered_for_promotion() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();

        let mut cue = lesson("always run `lark verify` before declaring the suite green");
        cue.tags = vec![crate::rule_cue::RULE_CUE_TAG.to_string()];
        let plain = lesson("a plain retrieval-only lesson with no promotion");
        seed_memory(root, &[cue.clone(), plain], false).unwrap();

        let promoted = crate::rule_cue::rule_cue_ids(root);
        assert_eq!(promoted.len(), 1, "only the tagged lesson is promoted");
        assert_eq!(promoted[0], seed_id(&cue.body));
    }

    #[test]
    fn dry_run_registers_no_cues() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        let mut cue = lesson("a dry-run cue that must not register");
        cue.tags = vec![crate::rule_cue::RULE_CUE_TAG.to_string()];
        seed_memory(root, &[cue], true).unwrap();
        assert!(crate::rule_cue::rule_cue_ids(root).is_empty());
    }

    #[test]
    fn empty_body_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();

        let report = seed_memory(root, &[lesson("   ")], false).unwrap();
        assert_eq!(
            report,
            SeedReport {
                seeded: 0,
                skipped: 1
            }
        );
    }
}
