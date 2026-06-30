//! A host-neutral research loop for LocalPilot.
//!
//! The loop decomposes a topic into sub-questions, gathers evidence across a
//! set of [`Source`]s (best-effort), synthesises findings with a
//! [`Synthesizer`], and independently cross-checks each finding's support. It
//! carries no filesystem, network, or engine handles: concrete sources and the
//! model-backed synthesizer are supplied by the binding layer (the CLI), which
//! keeps this crate dependency-light and unit-testable with fakes.

mod engine;
mod report;
mod source;
mod synth;

pub use engine::{run_research, Bounds, RunOutcome};
pub use report::{ClaimStatus, Evidence, Finding, Provenance, ResearchReport};
pub use source::{Source, SourceSet};
pub use synth::{HeuristicSynthesizer, Synthesizer};

/// A fatal error in the research loop (decomposition or synthesis).
#[derive(Debug, thiserror::Error)]
pub enum ResearchError {
    /// The synthesizer could not produce questions or findings.
    #[error("synthesis failed: {0}")]
    Synthesis(String),
}

/// A non-fatal error from a single source. Recorded and surfaced, never fails a
/// run (the gather is best-effort and returns partial results).
///
/// The label field is `source_label`, not `source`: `thiserror` treats a field
/// named `source` as the underlying `std::error::Error` cause, which a `String`
/// is not.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("source `{source_label}` failed: {message}")]
pub struct SourceError {
    /// The label of the source that failed.
    pub source_label: String,
    /// A human-readable reason.
    pub message: String,
}

impl SourceError {
    /// Construct a source error.
    #[must_use]
    pub fn new(source_label: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            source_label: source_label.into(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// A source that returns a fixed snippet per question, or errors.
    struct FakeSource {
        label: String,
        reply: Option<String>,
    }

    #[async_trait]
    impl Source for FakeSource {
        fn label(&self) -> &str {
            &self.label
        }
        async fn gather(&self, question: &str, limit: usize) -> Result<Vec<Evidence>, SourceError> {
            match &self.reply {
                None => Err(SourceError::new(&self.label, "boom")),
                Some(text) => Ok((0..limit.min(1))
                    .map(|_| Evidence {
                        question: question.to_string(),
                        snippet: text.clone(),
                        provenance: Provenance::new(self.label.clone(), Some("loc:1".to_string())),
                    })
                    .collect()),
            }
        }
    }

    /// A synthesizer that emits N questions and one unsupported finding, to
    /// exercise bounds and the cross-check downgrade.
    struct WideSynth {
        questions: usize,
    }

    #[async_trait]
    impl Synthesizer for WideSynth {
        async fn decompose(&self, topic: &str, _max: usize) -> Result<Vec<String>, ResearchError> {
            Ok((0..self.questions)
                .map(|i| format!("{topic} q{i}"))
                .collect())
        }
        async fn synthesize(
            &self,
            topic: &str,
            _evidence: &[Evidence],
        ) -> Result<Vec<Finding>, ResearchError> {
            Ok(vec![Finding {
                statement: format!("claim about {topic}"),
                status: ClaimStatus::Supported, // deliberately wrong; cross-check fixes it
                supporting: Vec::new(),
            }])
        }
    }

    #[test]
    fn report_constructs_empty() {
        let report = ResearchReport::new("topic");
        assert_eq!(report.topic, "topic");
        assert!(report.findings.is_empty());
        assert!(report.open_questions.is_empty());
    }

    #[tokio::test]
    async fn fake_source_carries_provenance() {
        let source = FakeSource {
            label: "memory".to_string(),
            reply: Some("hit".to_string()),
        };
        let hits = source.gather("q", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provenance.source, "memory");
        assert_eq!(hits[0].provenance.locator.as_deref(), Some("loc:1"));
    }

    #[tokio::test]
    async fn gather_all_merges_and_tolerates_failure() {
        let set = SourceSet::new()
            .with(Box::new(FakeSource {
                label: "ok".to_string(),
                reply: Some("a".to_string()),
            }))
            .with(Box::new(FakeSource {
                label: "bad".to_string(),
                reply: None,
            }));
        let (evidence, errors) = set.gather_all("q", 3).await;
        assert_eq!(
            evidence.len(),
            1,
            "the failing source must not drop the good one"
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].source_label, "bad");
    }

    #[tokio::test]
    async fn run_respects_question_bound_and_marks_unsupported() {
        let set = SourceSet::new().with(Box::new(FakeSource {
            label: "mem".to_string(),
            reply: Some("e".to_string()),
        }));
        let synth = WideSynth { questions: 10 };
        let bounds = Bounds {
            max_questions: 2,
            per_source_evidence: 3,
        };
        let outcome = run_research("t", &set, &synth, bounds).await.unwrap();
        assert_eq!(
            outcome.report.questions.len(),
            2,
            "decompose output must be bounded"
        );
        // The synth claimed Supported with no provenance; cross-check downgrades.
        assert_eq!(outcome.report.findings.len(), 1);
        assert_eq!(outcome.report.findings[0].status, ClaimStatus::Unsupported);
    }

    #[tokio::test]
    async fn degrade_path_no_sources_no_model_never_panics() {
        let set = SourceSet::new();
        let outcome = run_research(
            "lonely topic",
            &set,
            &HeuristicSynthesizer,
            Bounds::default(),
        )
        .await
        .unwrap();
        // One question (the topic), no evidence ⇒ it becomes an open question.
        assert_eq!(outcome.report.questions, vec!["lonely topic".to_string()]);
        assert_eq!(
            outcome.report.open_questions,
            vec!["lonely topic".to_string()]
        );
        assert!(outcome.report.findings.is_empty());
        assert!(outcome.source_errors.is_empty());
    }
}
