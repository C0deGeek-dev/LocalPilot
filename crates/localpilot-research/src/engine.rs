//! The bounded research loop: decompose → gather → cross-check → synthesise.

use crate::{
    ClaimStatus, Finding, ResearchError, ResearchReport, SourceError, SourceSet, Synthesizer,
};

/// Bounds on a research run. A host maps its resolved rails (ADR-0055) into
/// these so the loop cannot run unbounded.
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    /// Maximum number of sub-questions to pursue.
    pub max_questions: usize,
    /// Maximum evidence snippets to take from each source per question.
    pub per_source_evidence: usize,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            max_questions: 6,
            per_source_evidence: 5,
        }
    }
}

/// The outcome of a run: the report plus any non-fatal source errors so the
/// caller can surface a degraded gather without failing the run.
#[derive(Debug)]
pub struct RunOutcome {
    /// The synthesised report.
    pub report: ResearchReport,
    /// Errors from individual sources that were skipped (best-effort gather).
    pub source_errors: Vec<SourceError>,
}

/// Run the bounded research loop for `topic`.
///
/// Decomposes the topic, gathers evidence across `sources` for each
/// sub-question (best-effort — a failing source is recorded, not fatal),
/// synthesises findings, then independently cross-checks support. A
/// sub-question that gathered nothing becomes an open question.
pub async fn run_research(
    topic: &str,
    sources: &SourceSet,
    synth: &dyn Synthesizer,
    bounds: Bounds,
) -> Result<RunOutcome, ResearchError> {
    let mut report = ResearchReport::new(topic);
    let mut all_evidence = Vec::new();
    let mut source_errors = Vec::new();

    let questions: Vec<String> = synth
        .decompose(topic, bounds.max_questions)
        .await?
        .into_iter()
        .take(bounds.max_questions)
        .collect();

    for question in &questions {
        let (evidence, mut errors) = sources
            .gather_all(question, bounds.per_source_evidence)
            .await;
        source_errors.append(&mut errors);
        if evidence.is_empty() {
            report.open_questions.push(question.clone());
        }
        all_evidence.extend(evidence);
    }
    report.questions = questions;

    let mut findings = synth.synthesize(topic, &all_evidence).await?;
    cross_check(&mut findings);
    report.findings = findings;

    Ok(RunOutcome {
        report,
        source_errors,
    })
}

/// Adversarial pass: a finding with no supporting provenance is downgraded to
/// [`ClaimStatus::Unsupported`] regardless of what the synthesizer asserted.
fn cross_check(findings: &mut [Finding]) {
    for finding in findings.iter_mut() {
        if finding.supporting.is_empty() {
            finding.status = ClaimStatus::Unsupported;
        }
    }
}
