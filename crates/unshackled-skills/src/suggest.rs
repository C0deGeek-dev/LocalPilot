//! Usage-pattern tracking and generated skill *drafts*.
//!
//! Repeated workflows produce skill drafts that are always created disabled and
//! require explicit user review of content, permissions, and triggers before
//! they take effect. A per-pattern cooldown prevents re-suggesting the same
//! pattern. Nothing is created silently outside a disabled draft.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A generated skill draft. Always disabled until a user reviews and enables it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDraft {
    pub name: String,
    pub source_pattern: String,
    pub occurrences: u32,
    /// Always `false` on creation.
    pub enabled: bool,
}

/// Tracks repeated usage patterns and emits disabled drafts past a threshold.
#[derive(Debug, Clone)]
pub struct SuggestionEngine {
    threshold: u32,
    occurrences: HashMap<String, u32>,
    suggested: HashMap<String, ()>,
}

impl SuggestionEngine {
    /// An engine that suggests a draft after a pattern repeats `threshold` times.
    #[must_use]
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold,
            occurrences: HashMap::new(),
            suggested: HashMap::new(),
        }
    }

    /// Record one occurrence of a workflow pattern (a normalized signature of a
    /// command sequence, setup, error-fix loop, or prompt template). Returns a
    /// disabled draft the first time the pattern crosses the threshold; the
    /// cooldown then suppresses further suggestions for the same pattern.
    pub fn record(&mut self, pattern: &str) -> Option<SkillDraft> {
        let count = self.occurrences.entry(pattern.to_string()).or_insert(0);
        *count += 1;
        let occurrences = *count;

        if occurrences < self.threshold || self.suggested.contains_key(pattern) {
            return None;
        }
        self.suggested.insert(pattern.to_string(), ());
        Some(SkillDraft {
            name: draft_name(pattern),
            source_pattern: pattern.to_string(),
            occurrences,
            enabled: false,
        })
    }
}

fn draft_name(pattern: &str) -> String {
    let slug: String = pattern
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    format!("draft-{}", &slug[..slug.len().min(40)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_pattern_produces_a_disabled_draft() {
        let mut engine = SuggestionEngine::new(3);
        assert!(engine.record("cargo test then cargo fmt").is_none());
        assert!(engine.record("cargo test then cargo fmt").is_none());
        let draft = engine.record("cargo test then cargo fmt").expect("a draft");
        assert!(!draft.enabled, "drafts are created disabled");
        assert_eq!(draft.occurrences, 3);
    }

    #[test]
    fn the_cooldown_suppresses_repeat_suggestions() {
        let mut engine = SuggestionEngine::new(2);
        assert!(engine.record("setup db").is_none());
        assert!(engine.record("setup db").is_some());
        // Already suggested: no further drafts for the same pattern.
        assert!(engine.record("setup db").is_none());
        assert!(engine.record("setup db").is_none());
    }

    #[test]
    fn distinct_patterns_are_tracked_separately() {
        let mut engine = SuggestionEngine::new(2);
        engine.record("a");
        engine.record("b");
        assert!(engine.record("a").is_some());
        assert!(engine.record("b").is_some());
    }
}
