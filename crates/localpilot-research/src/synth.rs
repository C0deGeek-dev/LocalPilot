//! Topic decomposition and evidence synthesis.
//!
//! The host supplies a model-backed [`Synthesizer`]; [`HeuristicSynthesizer`]
//! is the dependency-free degrade path used when no model is available, so a
//! research run always produces a report with provenance and never panics.

use async_trait::async_trait;

use crate::{ClaimStatus, Evidence, Finding, ResearchError};

/// Turns a topic into sub-questions and gathered evidence into findings.
#[async_trait]
pub trait Synthesizer: Send + Sync {
    /// Break `topic` into at most `max_questions` sub-questions.
    async fn decompose(
        &self,
        topic: &str,
        max_questions: usize,
    ) -> Result<Vec<String>, ResearchError>;

    /// Turn gathered `evidence` into findings. The loop independently
    /// cross-checks support, so an implementation need not be conservative.
    async fn synthesize(
        &self,
        topic: &str,
        evidence: &[Evidence],
    ) -> Result<Vec<Finding>, ResearchError>;

    /// Propose up to `max` follow-up queries for a sub-question that is not
    /// yet covered, given the evidence gathered so far. Retrieval-side
    /// assistance only — reformulated queries never author findings.
    ///
    /// The default is deterministic pseudo-relevance expansion (no model):
    /// salient terms from the question's own evidence, guarded against query
    /// drift by requiring each expansion term to appear in evidence from at
    /// least two distinct origins.
    async fn reformulate(
        &self,
        question: &str,
        evidence: &[Evidence],
        max: usize,
    ) -> Result<Vec<String>, ResearchError> {
        Ok(expansion_queries(question, evidence, max))
    }
}

/// Terms shorter than this never become expansion terms.
const MIN_TERM_LEN: usize = 3;
/// Expansion terms appended to the original question per reformulation.
const EXPANSION_TERMS: usize = 4;
/// An expansion term must appear in evidence from at least this many distinct
/// origins (the query-drift guard: one off-topic page cannot steer the query).
const MIN_TERM_ORIGINS: usize = 2;

/// Common English words that carry no retrieval signal.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "all", "can", "her", "was", "one", "our",
    "out", "day", "get", "has", "him", "his", "how", "man", "new", "now", "old", "see", "two",
    "way", "who", "with", "this", "that", "from", "they", "have", "been", "will", "each", "when",
    "what", "which", "their", "there", "would", "could", "should", "about", "into", "than", "then",
    "them", "these", "those", "some", "such", "only", "over", "also", "after", "before", "between",
    "does", "doing", "during", "under", "while", "where", "why", "your", "more", "most", "other",
    "using", "used", "use",
];

/// Deterministic pseudo-relevance query expansion.
///
/// Tokenizes the question's gathered evidence, keeps alphanumeric terms of
/// length ≥ 3 that are neither stopwords nor already in the question, requires
/// each term to appear in evidence from ≥ 2 distinct origins, ranks by how
/// many origins carry the term (then frequency), and appends the top terms to
/// the original question as one broadened query. Returns at most `max`
/// queries; empty when nothing qualifies (e.g. zero or single-origin
/// evidence), which the loop treats as "nothing better to ask".
#[must_use]
pub fn expansion_queries(question: &str, evidence: &[Evidence], max: usize) -> Vec<String> {
    if max == 0 || evidence.is_empty() {
        return Vec::new();
    }
    let question_terms: std::collections::HashSet<String> = tokenize(question).collect();
    // term → (distinct origins, total occurrences)
    let mut origins_by_term: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for item in evidence {
        let origin = item
            .provenance
            .locator
            .clone()
            .unwrap_or_else(|| item.provenance.source.clone());
        for term in tokenize(&item.snippet) {
            if question_terms.contains(&term) || STOPWORDS.contains(&term.as_str()) {
                continue;
            }
            origins_by_term
                .entry(term.clone())
                .or_default()
                .insert(origin.clone());
            *counts.entry(term).or_default() += 1;
        }
    }
    let mut ranked: Vec<(String, usize, usize)> = origins_by_term
        .into_iter()
        .filter(|(_, origins)| origins.len() >= MIN_TERM_ORIGINS)
        .map(|(term, origins)| {
            let count = counts.get(&term).copied().unwrap_or(0);
            (term, origins.len(), count)
        })
        .collect();
    if ranked.is_empty() {
        return Vec::new();
    }
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then(a.0.cmp(&b.0)));
    let terms: Vec<String> = ranked
        .into_iter()
        .take(EXPANSION_TERMS)
        .map(|(term, _, _)| term)
        .collect();
    vec![format!("{question} {}", terms.join(" "))]
        .into_iter()
        .take(max)
        .collect()
}

/// Lowercased alphanumeric tokens of length ≥ [`MIN_TERM_LEN`].
fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| token.chars().count() >= MIN_TERM_LEN)
        .map(str::to_ascii_lowercase)
}

/// Score how well `text` answers `question` by content-term overlap: the
/// fraction of the question's non-stopword terms present in the text,
/// clamped to `0.1..=1.0`. A multi-term question matched on fewer than two
/// terms floors at `0.1` — the shared search path's term-coverage rule
/// applied to fetched content, so one incidental word cannot make a page
/// read as relevant. A question with no scoreable terms returns a neutral
/// `0.5`.
#[must_use]
pub fn term_overlap_relevance(question: &str, text: &str) -> f32 {
    let question_terms: std::collections::HashSet<String> = tokenize(question)
        .filter(|term| !STOPWORDS.contains(&term.as_str()))
        .collect();
    if question_terms.is_empty() {
        return 0.5;
    }
    let text_terms: std::collections::HashSet<String> = tokenize(text).collect();
    let matched = question_terms
        .iter()
        .filter(|term| text_terms.contains(*term))
        .count();
    if matched < 2 && question_terms.len() >= 2 {
        return 0.1;
    }
    #[allow(clippy::cast_precision_loss)]
    let fraction = matched as f32 / question_terms.len() as f32;
    fraction.clamp(0.1, 1.0)
}

/// Deterministic, model-free synthesizer.
///
/// `decompose` yields the topic as a single question; `synthesize` turns each
/// evidence snippet into a supported finding carrying that snippet's
/// provenance. Used as the graceful-degrade path on a weak or absent model.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicSynthesizer;

#[async_trait]
impl Synthesizer for HeuristicSynthesizer {
    async fn decompose(
        &self,
        topic: &str,
        max_questions: usize,
    ) -> Result<Vec<String>, ResearchError> {
        if max_questions == 0 {
            return Ok(Vec::new());
        }
        Ok(vec![topic.to_string()])
    }

    async fn synthesize(
        &self,
        _topic: &str,
        evidence: &[Evidence],
    ) -> Result<Vec<Finding>, ResearchError> {
        Ok(evidence.iter().map(finding_from).collect())
    }
}

/// One supported finding per evidence snippet, carrying the snippet's own
/// provenance plus that of any near-duplicates folded into it — a folded
/// duplicate's origin is never silently dropped.
fn finding_from(e: &Evidence) -> Finding {
    Finding {
        statement: e.snippet.clone(),
        status: ClaimStatus::Supported,
        supporting: std::iter::once(e.provenance.clone())
            .chain(e.also_from.iter().cloned())
            .collect(),
        // The loop's sanitize pass splits a raw snippet into a concise
        // claim plus separate evidence; the model-free path leaves that
        // to it rather than guessing a summary here.
        evidence: None,
        confidence: e.relevance.clamp(0.0, 1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provenance;

    fn evidence(snippet: &str, origin: &str) -> Evidence {
        Evidence {
            question: "q".to_string(),
            snippet: snippet.to_string(),
            provenance: Provenance::new("web", Some(origin.to_string())),
            relevance: 1.0,
            also_from: Vec::new(),
        }
    }

    #[test]
    fn expansion_requires_terms_from_two_origins() {
        // "skeleton" appears in two distinct origins; "banana" in one only —
        // the drift guard keeps the single-origin term out of the query.
        let pool = vec![
            evidence("the skeleton drives skinning banana", "https://a.example/1"),
            evidence("skeleton bones and skinning weights", "https://b.example/2"),
        ];
        let queries = expansion_queries("how does mesh animation work", &pool, 2);
        assert_eq!(queries.len(), 1);
        assert!(queries[0].contains("skeleton"), "{queries:?}");
        assert!(queries[0].contains("skinning"), "{queries:?}");
        assert!(!queries[0].contains("banana"), "{queries:?}");
        assert!(
            queries[0].starts_with("how does mesh animation work"),
            "original query kept as the base: {queries:?}"
        );
    }

    #[test]
    fn expansion_skips_stopwords_and_question_terms() {
        let pool = vec![
            evidence("the animation with more detail", "https://a.example/1"),
            evidence("more animation and the detail", "https://b.example/2"),
        ];
        let queries = expansion_queries("explain animation", &pool, 2);
        // "animation" is in the question, "the"/"more"/"and"/"with" are
        // stopwords: only "detail" qualifies.
        assert_eq!(queries.len(), 1);
        assert!(queries[0].ends_with("detail"), "{queries:?}");
    }

    #[test]
    fn no_evidence_or_single_origin_yields_no_expansion() {
        assert!(expansion_queries("q", &[], 2).is_empty());
        let single = vec![evidence("unique terms here", "https://a.example/1")];
        assert!(
            expansion_queries("q", &single, 2).is_empty(),
            "single-origin evidence cannot pass the drift guard"
        );
    }
}
