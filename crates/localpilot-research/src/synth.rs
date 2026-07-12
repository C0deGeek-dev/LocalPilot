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
        Ok(evidence
            .iter()
            .map(|e| Finding {
                statement: e.snippet.clone(),
                status: ClaimStatus::Supported,
                supporting: vec![e.provenance.clone()],
                // The loop's sanitize pass splits a raw snippet into a concise
                // claim plus separate evidence; the model-free path leaves that
                // to it rather than guessing a summary here.
                evidence: None,
                confidence: e.relevance.clamp(0.0, 1.0),
            })
            .collect())
    }
}
