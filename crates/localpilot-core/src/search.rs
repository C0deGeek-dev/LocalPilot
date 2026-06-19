//! Shared relevance primitives for pull-based discovery surfaces.
//!
//! `skill_search` and the tool broker both rank candidates by the same core
//! signal: how many of the query's words appear in a candidate's text. Each
//! surface then adds its own thin policy bonus (a command-trigger match, a
//! direct name hit, a learned re-rank, a deprecation de-rank). This module owns
//! the shared word-overlap core and the minimal locator shape so the policy
//! wrappers stay small and the core has one definition and one test home.

/// Count how many of `query_words` occur as substrings in `haystack`.
///
/// The caller lowercases both sides and pre-splits the query into matchable
/// words (the shared convention is alphanumeric runs longer than two chars), so
/// this stays a pure, allocation-free count. It is the relevance core under the
/// per-surface scorers; callers add their own bonuses on top.
#[must_use]
pub fn word_overlap(haystack: &str, query_words: &[&str]) -> u32 {
    query_words
        .iter()
        .filter(|word| haystack.contains(**word))
        .count() as u32
}

/// A minimal ranked match: a name, a one-line summary, and a relevance score.
///
/// The shared shape for surfaces that return exactly this (e.g. `skill_search`).
/// Surfaces that carry extra policy state (e.g. the tool broker's deprecation
/// fields) keep their own richer struct; this is the common base, not a ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locator {
    /// The candidate's exact name (what a follow-up `*_load`/call uses).
    pub name: String,
    /// A bounded one-line summary (see [`crate::text::one_line`]).
    pub summary: String,
    /// Relevance score; higher is more relevant. Surface-defined composition.
    pub score: u32,
}

impl Locator {
    /// Construct a locator.
    #[must_use]
    pub fn new(name: impl Into<String>, summary: impl Into<String>, score: u32) -> Self {
        Self {
            name: name.into(),
            summary: summary.into(),
            score,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_overlap_counts_distinct_query_words_present() {
        let haystack = "format json output for the report";
        assert_eq!(word_overlap(haystack, &["json", "report"]), 2);
        assert_eq!(word_overlap(haystack, &["json", "missing"]), 1);
        assert_eq!(word_overlap(haystack, &[]), 0);
    }

    #[test]
    fn locator_new_sets_fields() {
        let loc = Locator::new("skill-a", "does a thing", 3);
        assert_eq!(loc.name, "skill-a");
        assert_eq!(loc.summary, "does a thing");
        assert_eq!(loc.score, 3);
    }
}
