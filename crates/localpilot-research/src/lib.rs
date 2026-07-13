//! A host-neutral research loop for LocalPilot.
//!
//! The loop decomposes a topic into sub-questions, gathers evidence across a
//! set of [`Source`]s (best-effort), synthesises findings with a
//! [`Synthesizer`], and independently cross-checks each finding's support. It
//! carries no filesystem, network, or engine handles: concrete sources and the
//! model-backed synthesizer are supplied by the binding layer (the CLI), which
//! keeps this crate dependency-light and unit-testable with fakes.

mod engine;
mod html;
mod output;
mod report;
mod source;
mod synth;
mod web;

pub use engine::{
    run_research, run_research_controlled, Bounds, ProgressFn, RoundSummary, RunControl, RunOutcome,
};
pub use html::{html_to_markdown, html_to_text, markdown_to_text};
pub use output::{candidates_from, evidence_block, render_markdown, CandidateSpec};
pub use report::{
    flatten_whitespace, ClaimStatus, CoverageVerdict, Evidence, Finding, Provenance,
    QuestionCoverage, ResearchReport,
};
pub use source::{Source, SourceSet};
pub use synth::{expansion_queries, term_overlap_relevance, HeuristicSynthesizer, Synthesizer};
pub use web::{host_allowed, host_matches, prepare_query, AuditEntry, FetchDecision, WebAccess};

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
                        relevance: 1.0,
                        also_from: Vec::new(),
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
                evidence: None,
                confidence: 1.0,
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
            ..Bounds::default()
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
        // The zero-evidence round reads as saturation: exactly one round ran.
        assert_eq!(outcome.report.rounds_run, 1);
        assert_eq!(outcome.report.coverage[0].verdict, CoverageVerdict::Open);
    }

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// A source whose every call returns `per_call` brand-new snippets from
    /// brand-new origins, and logs each query it is asked.
    struct EndlessSource {
        counter: AtomicUsize,
        per_call: usize,
        log: Mutex<Vec<String>>,
    }

    impl EndlessSource {
        fn new(per_call: usize) -> Self {
            Self {
                counter: AtomicUsize::new(0),
                per_call,
                log: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Source for EndlessSource {
        fn label(&self) -> &str {
            "endless"
        }
        async fn gather(&self, question: &str, limit: usize) -> Result<Vec<Evidence>, SourceError> {
            if let Ok(mut log) = self.log.lock() {
                log.push(question.to_string());
            }
            Ok((0..self.per_call.min(limit))
                .map(|_| {
                    let n = self.counter.fetch_add(1, Ordering::Relaxed);
                    Evidence::new(
                        question,
                        format!("unique snippet number {n} entirely distinct words {n}"),
                        Provenance::new("endless", Some(format!("origin-{n}"))),
                        1.0,
                    )
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn all_covered_stops_the_loop_early() {
        // Two fresh origins per call ⇒ every question is covered in round 1,
        // so the loop stops without spending its round budget.
        let source = EndlessSource::new(2);
        let set = SourceSet::new().with(Box::new(source));
        let synth = WideSynth { questions: 2 };
        let bounds = Bounds {
            max_questions: 2,
            max_rounds: 5,
            ..Bounds::default()
        };
        let outcome = run_research("t", &set, &synth, bounds).await.unwrap();
        assert_eq!(outcome.report.rounds_run, 1, "no wasted rounds");
        assert!(outcome
            .report
            .coverage
            .iter()
            .all(|c| c.verdict == CoverageVerdict::Covered));
        assert!(outcome.report.open_questions.is_empty());
    }

    #[tokio::test]
    async fn saturation_stops_when_a_round_finds_nothing_new() {
        // The same single snippet every call: round 1 gathers it, round 2
        // re-finds only known ground ⇒ saturation, well short of max_rounds.
        let set = SourceSet::new().with(Box::new(FakeSource {
            label: "static".to_string(),
            reply: Some("the one snippet".to_string()),
        }));
        let synth = WideSynth { questions: 1 };
        let bounds = Bounds {
            max_questions: 1,
            max_rounds: 6,
            ..Bounds::default()
        };
        let outcome = run_research("t", &set, &synth, bounds).await.unwrap();
        assert_eq!(
            outcome.report.rounds_run, 2,
            "one round of no progress ends it"
        );
        assert_eq!(outcome.rounds[1].new_evidence, 0);
        // One origin only ⇒ the question stays weak, honestly.
        assert_eq!(outcome.report.coverage[0].verdict, CoverageVerdict::Weak);
    }

    #[tokio::test]
    async fn later_rounds_requery_only_uncovered_questions() {
        // One fresh origin per call: a question needs two rounds to reach two
        // origins. Once covered, the next round targets nothing and the loop
        // stops — no round is spent on a covered question.
        let source = EndlessSource::new(1);
        let set = SourceSet::new().with(Box::new(source));
        let synth = WideSynth { questions: 1 };
        let bounds = Bounds {
            max_questions: 1,
            max_rounds: 4,
            ..Bounds::default()
        };
        let outcome = run_research("t", &set, &synth, bounds).await.unwrap();
        assert_eq!(outcome.report.coverage[0].verdict, CoverageVerdict::Covered);
        assert_eq!(
            outcome.report.rounds_run, 2,
            "covered after round 2; round 3 never runs"
        );
        assert!(
            outcome.rounds[1].new_evidence > 0,
            "round 2's follow-up retrieval made the difference"
        );
    }

    #[tokio::test]
    async fn evidence_cap_bounds_the_run() {
        let source = EndlessSource::new(5);
        let set = SourceSet::new().with(Box::new(source));
        let synth = WideSynth { questions: 3 };
        let bounds = Bounds {
            max_questions: 3,
            per_source_evidence: 5,
            max_rounds: 10,
            max_total_evidence: 7,
            ..Bounds::default()
        };
        let outcome = run_research("t", &set, &synth, bounds).await.unwrap();
        let total: usize = outcome
            .report
            .coverage
            .iter()
            .map(|c| c.evidence_count)
            .sum();
        assert!(total <= 7, "the hard evidence cap holds: {total}");
    }

    #[tokio::test]
    async fn stop_flag_yields_a_partial_but_well_formed_outcome() {
        let source = EndlessSource::new(2);
        let set = SourceSet::new().with(Box::new(source));
        let synth = WideSynth { questions: 2 };
        let stop = Arc::new(AtomicBool::new(true)); // stop before any gather
        let control = RunControl {
            stop: Some(stop),
            ..RunControl::default()
        };
        let outcome = run_research_controlled(
            "t",
            &set,
            &synth,
            Bounds {
                max_questions: 2,
                ..Bounds::default()
            },
            control,
        )
        .await
        .unwrap();
        assert_eq!(outcome.report.questions.len(), 2, "questions recorded");
        assert!(
            outcome
                .report
                .coverage
                .iter()
                .all(|c| c.verdict == CoverageVerdict::Open),
            "nothing gathered before the stop"
        );
        assert_eq!(
            outcome.report.rounds_run, 1,
            "the interrupted round is accounted"
        );
    }

    #[test]
    fn term_overlap_scores_fraction_with_two_term_floor() {
        // Three content terms, all present: full score.
        assert!(
            (term_overlap_relevance("tokio async runtime", "the tokio async runtime docs") - 1.0)
                .abs()
                < f32::EPSILON
        );
        // One incidental match on a multi-term question floors at 0.1 (the
        // term-coverage rule applied to fetched pages).
        assert!(
            (term_overlap_relevance("animation mixer clips", "a page about animation of cats")
                - 0.1)
                .abs()
                < f32::EPSILON
        );
        // Two of three terms: 2/3.
        let score = term_overlap_relevance("animation mixer clips", "mixer clips workshop");
        assert!((score - 2.0 / 3.0).abs() < 0.01, "{score}");
    }

    #[tokio::test]
    async fn near_duplicates_fold_and_keep_both_origins() {
        // Two sources return the same content from different origins: one
        // snippet survives, the duplicate's provenance rides along, coverage
        // counts both origins, and the fold is loudly noted.
        struct MirrorSource {
            label: String,
            origin: String,
        }
        #[async_trait]
        impl Source for MirrorSource {
            fn label(&self) -> &str {
                &self.label
            }
            async fn gather(
                &self,
                question: &str,
                _limit: usize,
            ) -> Result<Vec<Evidence>, SourceError> {
                Ok(vec![Evidence::new(
                    question,
                    "the animation mixer blends clip weights across the skeleton every frame",
                    Provenance::new("web", Some(self.origin.clone())),
                    1.0,
                )])
            }
        }
        let set = SourceSet::new()
            .with(Box::new(MirrorSource {
                label: "a".to_string(),
                origin: "https://a.example/page".to_string(),
            }))
            .with(Box::new(MirrorSource {
                label: "b".to_string(),
                origin: "https://b.example/mirror".to_string(),
            }));
        let synth = WideSynthKeep;
        let outcome = run_research(
            "t",
            &set,
            &synth,
            Bounds {
                max_questions: 1,
                max_rounds: 1,
                ..Bounds::default()
            },
        )
        .await
        .unwrap();
        // One finding (the fold), carrying both origins.
        assert_eq!(outcome.report.findings.len(), 1);
        assert_eq!(
            outcome.report.findings[0].supporting.len(),
            2,
            "the folded duplicate's provenance is kept: {:?}",
            outcome.report.findings[0].supporting
        );
        assert_eq!(
            outcome.report.coverage[0].distinct_origins, 2,
            "a mirror on a second origin is an independence signal"
        );
        assert_eq!(outcome.report.coverage[0].verdict, CoverageVerdict::Covered);
        assert!(
            outcome
                .report
                .retrieval_notes
                .iter()
                .any(|n| n.contains("near-duplicate")),
            "the fold is loud: {:?}",
            outcome.report.retrieval_notes
        );
    }

    /// A synthesizer that decomposes to one question and synthesizes via the
    /// heuristic (so folds show up in findings).
    struct WideSynthKeep;

    #[async_trait]
    impl Synthesizer for WideSynthKeep {
        async fn decompose(&self, topic: &str, _max: usize) -> Result<Vec<String>, ResearchError> {
            Ok(vec![topic.to_string()])
        }
        async fn synthesize(
            &self,
            topic: &str,
            evidence: &[Evidence],
        ) -> Result<Vec<Finding>, ResearchError> {
            HeuristicSynthesizer.synthesize(topic, evidence).await
        }
    }

    #[tokio::test]
    async fn one_origin_cannot_saturate_a_question() {
        // A single origin returning many distinct snippets is soft-capped once
        // another origin is present; the drop is noted.
        struct FloodSource {
            origin: String,
            counter: AtomicUsize,
            per_call: usize,
        }
        #[async_trait]
        impl Source for FloodSource {
            fn label(&self) -> &str {
                "flood"
            }
            async fn gather(
                &self,
                question: &str,
                limit: usize,
            ) -> Result<Vec<Evidence>, SourceError> {
                Ok((0..self.per_call.min(limit))
                    .map(|_| {
                        let n = self.counter.fetch_add(1, Ordering::Relaxed);
                        Evidence::new(
                            question,
                            format!("flood snippet {n} entirely different words here {n}"),
                            Provenance::new("web", Some(format!("{}/page{n}", self.origin))),
                            1.0,
                        )
                    })
                    .collect())
            }
        }
        let set = SourceSet::new()
            .with(Box::new(FloodSource {
                origin: "https://flood.example".to_string(),
                counter: AtomicUsize::new(0),
                per_call: 5,
            }))
            .with(Box::new(FakeSource {
                label: "memory".to_string(),
                reply: Some("independent memory snippet".to_string()),
            }));
        let synth = WideSynthKeep;
        let outcome = run_research(
            "t",
            &set,
            &synth,
            Bounds {
                max_questions: 1,
                max_rounds: 1,
                per_source_evidence: 5,
                ..Bounds::default()
            },
        )
        .await
        .unwrap();
        // flood.example may keep at most 3 snippets for the question.
        let flood_kept = outcome
            .report
            .findings
            .iter()
            .filter(|f| {
                f.supporting
                    .iter()
                    .any(|p| p.locator.as_deref().unwrap_or("").contains("flood.example"))
            })
            .count();
        assert!(flood_kept <= 3, "diversity cap holds: {flood_kept}");
        assert!(
            outcome
                .report
                .retrieval_notes
                .iter()
                .any(|n| n.contains("diversity cap")),
            "the drop is loud: {:?}",
            outcome.report.retrieval_notes
        );
    }
}
