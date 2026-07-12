//! The bounded research loop: decompose → gather → cross-check → synthesise.

use crate::{
    flatten_whitespace, html_to_text, ClaimStatus, Finding, Provenance, ResearchError,
    ResearchReport, SourceError, SourceSet, Synthesizer,
};

/// Longest a finding statement may be before it is treated as an over-long blob
/// and reduced to an excerpt, with the full text preserved as evidence.
const MAX_STATEMENT_CHARS: usize = 240;

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
    sanitize_findings(&mut findings);
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

/// Keep findings readable: a statement that is a code/HTML blob or too long is
/// no claim — its raw text is preserved as `evidence` and the statement is
/// replaced with a concise, single-line excerpt. A clean statement is only
/// flattened to one line. This runs on every finding, so neither the rendered
/// report nor the enqueued memory candidates can carry a raw source chunk.
fn sanitize_findings(findings: &mut [Finding]) {
    for finding in findings.iter_mut() {
        let flat = flatten_whitespace(&finding.statement);
        if looks_like_markup(&finding.statement) {
            preserve_evidence(finding);
            finding.statement = titled_excerpt(&flat, &finding.supporting);
        } else if flat.chars().count() > MAX_STATEMENT_CHARS {
            preserve_evidence(finding);
            let excerpt: String = flat.chars().take(MAX_STATEMENT_CHARS).collect();
            finding.statement = format!("{excerpt}…");
        } else {
            finding.statement = flat;
        }
    }
}

/// Stash the finding's current statement as evidence unless one is already set.
fn preserve_evidence(finding: &mut Finding) {
    if finding.evidence.is_none() {
        finding.evidence = Some(finding.statement.clone());
    }
}

/// Whether the text reads as code or markup rather than prose.
fn looks_like_markup(text: &str) -> bool {
    if text.contains("```") || text.contains("</") {
        return true;
    }
    // An opening tag/declaration like `<div`, `<p>`, `<script`, `<!doctype`.
    text.as_bytes()
        .windows(2)
        .any(|pair| pair[0] == b'<' && (pair[1].is_ascii_alphabetic() || pair[1] == b'!'))
}

/// Derive a short claim from a flattened blob: strip crude markup, take a
/// leading excerpt, and title it with its source so the reader knows it is a
/// source excerpt, not a synthesised conclusion. The full text stays in
/// `evidence`.
fn titled_excerpt(flat: &str, supporting: &[Provenance]) -> String {
    let source = supporting
        .first()
        .map_or("source", |provenance| provenance.source.as_str());
    let stripped = strip_markup(flat);
    let body = stripped.trim();
    if body.is_empty() {
        return format!("Excerpt from {source} (see evidence)");
    }
    let excerpt: String = body.chars().take(MAX_STATEMENT_CHARS).collect();
    let ellipsis = if body.chars().count() > MAX_STATEMENT_CHARS {
        "…"
    } else {
        ""
    };
    format!("Excerpt from {source}: {excerpt}{ellipsis}")
}

/// Reduce a markup/code blob to a readable one-line excerpt: drop whole
/// non-content elements and their bodies (so inline script/style text does not
/// survive as junk), strip the remaining tags and code fences, then flatten to
/// a single line. Delegates the element reduction to [`html_to_text`] and
/// flattens its line breaks away for the heading-safe excerpt.
fn strip_markup(text: &str) -> String {
    flatten_whitespace(&html_to_text(text).replace("```", " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(statement: &str, supporting: Vec<Provenance>) -> Finding {
        Finding {
            statement: statement.to_string(),
            status: ClaimStatus::Supported,
            supporting,
            evidence: None,
            confidence: 1.0,
        }
    }

    #[test]
    fn html_blob_becomes_an_excerpt_with_raw_text_in_evidence() {
        let raw = "<script>track();</script><div class=\"x\">Caches speed reads</div>";
        let mut findings = vec![finding(raw, vec![Provenance::new("web", None)])];
        sanitize_findings(&mut findings);

        let f = &findings[0];
        assert!(
            !f.statement.contains('<'),
            "no markup in claim: {}",
            f.statement
        );
        assert!(!f.statement.contains("```"));
        assert!(f.statement.contains("Caches speed reads"));
        assert!(f.statement.chars().count() <= MAX_STATEMENT_CHARS + 32);
        assert_eq!(
            f.evidence.as_deref(),
            Some(raw),
            "raw preserved as evidence"
        );
    }

    #[test]
    fn fenced_code_statement_is_moved_to_evidence() {
        let raw = "```js\nfunction f(){ return 1 }\n```";
        let mut findings = vec![finding(raw, vec![Provenance::new("web", None)])];
        sanitize_findings(&mut findings);
        assert!(!findings[0].statement.contains("```"));
        assert_eq!(findings[0].evidence.as_deref(), Some(raw));
    }

    #[test]
    fn overlong_prose_is_truncated_and_preserved() {
        let raw = "word ".repeat(200);
        let mut findings = vec![finding(&raw, vec![Provenance::new("memory", None)])];
        sanitize_findings(&mut findings);
        assert!(findings[0].statement.ends_with('…'));
        assert!(findings[0].statement.chars().count() <= MAX_STATEMENT_CHARS + 1);
        assert!(findings[0].evidence.is_some());
    }

    #[test]
    fn clean_statement_is_only_flattened() {
        let mut findings = vec![finding(
            "Caching speeds up\n  repeated reads",
            vec![Provenance::new("memory", None)],
        )];
        sanitize_findings(&mut findings);
        assert_eq!(findings[0].statement, "Caching speeds up repeated reads");
        assert!(
            findings[0].evidence.is_none(),
            "clean claim needs no evidence split"
        );
    }
}
