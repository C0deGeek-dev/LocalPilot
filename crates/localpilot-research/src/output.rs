//! Rendering a [`ResearchReport`] into the two outputs: a human-readable
//! Markdown artefact and review-gated memory-candidate specs.
//!
//! Both are pure and host-neutral. Writing the artefact to disk and enqueuing
//! the candidates into LocalMind's review queue happen in the binding layer
//! (subject 04): a candidate is never auto-accepted into durable memory.

use crate::{flatten_whitespace, ClaimStatus, Provenance, ResearchReport};

/// Longest raw evidence shown inline in the Markdown artefact before it is
/// truncated; the finding still carries the full text.
const MAX_EVIDENCE_CHARS: usize = 4000;

/// A host-neutral memory-candidate proposal derived from a finding. The binding
/// layer maps this onto LocalMind's `CandidateLesson` and routes it through the
/// review queue — it is a *proposal*, never accepted memory.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateSpec {
    /// The claim text proposed as a lesson.
    pub body: String,
    /// The evidence backing it, carried so the reviewer sees provenance.
    pub provenance: Vec<Provenance>,
    /// Prior confidence to attach; kept low because research findings are
    /// machine-derived and unreviewed.
    pub confidence: f32,
}

/// Derive review-queue candidate specs from a report.
///
/// Only **supported** findings with at least one provenance become candidates,
/// so unsupported or unbacked claims never reach the review queue.
#[must_use]
pub fn candidates_from(report: &ResearchReport, confidence: f32) -> Vec<CandidateSpec> {
    report
        .findings
        .iter()
        .filter(|f| f.status == ClaimStatus::Supported && !f.supporting.is_empty())
        .map(|f| CandidateSpec {
            body: f.statement.clone(),
            provenance: f.supporting.clone(),
            confidence,
        })
        .collect()
}

/// Render a report as a human-readable Markdown artefact. Deterministic: the
/// same report always renders identically. Every finding shows its support
/// status, and a supported finding lists its provenance.
#[must_use]
pub fn render_markdown(report: &ResearchReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Research: {}\n\n", report.topic));

    out.push_str("## Sub-questions\n\n");
    if report.questions.is_empty() {
        out.push_str("_None._\n\n");
    } else {
        for question in &report.questions {
            out.push_str(&format!("- {question}\n"));
        }
        out.push('\n');
    }

    out.push_str("## Findings\n\n");
    if report.findings.is_empty() {
        out.push_str("_No findings._\n\n");
    } else {
        for (index, finding) in report.findings.iter().enumerate() {
            let tag = match finding.status {
                ClaimStatus::Supported => "supported",
                ClaimStatus::Unsupported => "unsupported — no evidence found",
            };
            // Flatten defensively: a statement must never break the heading's
            // `_(tag)_` suffix or the following `Sources:` block, even if a
            // caller hands render an unsanitised report.
            out.push_str(&format!(
                "### {}. {} _({tag})_\n",
                index + 1,
                flatten_whitespace(&finding.statement)
            ));
            if finding.supporting.is_empty() {
                out.push('\n');
            } else {
                out.push_str("Sources:\n");
                for provenance in &finding.supporting {
                    match &provenance.locator {
                        Some(locator) => {
                            out.push_str(&format!("- {}: {locator}\n", provenance.source));
                        }
                        None => out.push_str(&format!("- {}\n", provenance.source)),
                    }
                }
                out.push('\n');
            }
            if let Some(evidence) = &finding.evidence {
                push_evidence_block(&mut out, evidence);
            }
        }
    }

    if !report.open_questions.is_empty() {
        out.push_str("## Open questions\n\n");
        for question in &report.open_questions {
            out.push_str(&format!("- {question}\n"));
        }
        out.push('\n');
    }

    out
}

/// Append a finding's raw evidence as a fenced block. The fence is chosen longer
/// than any backtick run in the content, so a snippet that itself contains
/// ``` ``` ``` can never break out of the block. Over-long evidence is truncated.
fn push_evidence_block(out: &mut String, evidence: &str) {
    let truncated: String = evidence.chars().take(MAX_EVIDENCE_CHARS).collect();
    let clipped = evidence.chars().count() > MAX_EVIDENCE_CHARS;
    let fence = backtick_fence(&truncated);
    out.push_str("Evidence:\n");
    out.push_str(&fence);
    out.push('\n');
    out.push_str(&truncated);
    if clipped {
        out.push_str("\n… (truncated)");
    }
    out.push('\n');
    out.push_str(&fence);
    out.push_str("\n\n");
}

/// A backtick fence at least one longer than the longest backtick run in `text`
/// (minimum three), so `text` cannot terminate the fenced block early.
fn backtick_fence(text: &str) -> String {
    let mut longest = 0;
    let mut current = 0;
    for ch in text.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Finding;

    fn finding(statement: &str, status: ClaimStatus, supporting: Vec<Provenance>) -> Finding {
        Finding {
            statement: statement.to_string(),
            status,
            supporting,
            evidence: None,
        }
    }

    #[test]
    fn evidence_renders_in_a_fence_that_survives_backticks_in_the_snippet() {
        let mut report = ResearchReport::new("t");
        let mut f = finding(
            "Excerpt from web: some claim",
            ClaimStatus::Supported,
            vec![Provenance::new("web", None)],
        );
        f.evidence = Some("```js\nfn();\n```".to_string());
        report.findings = vec![f];

        let md = render_markdown(&report);
        assert!(md.contains("Evidence:"));
        // The claim heading stays on one clean line.
        assert!(md.contains("### 1. Excerpt from web: some claim _(supported)_"));
        // A fence longer than the ``` inside the snippet wraps it.
        assert!(md.contains("````"), "fence escapes inner backticks: {md}");
    }

    #[test]
    fn multiline_statement_never_breaks_the_heading() {
        let mut report = ResearchReport::new("t");
        report.findings = vec![finding(
            "line one\nline two",
            ClaimStatus::Supported,
            vec![Provenance::new("memory", Some("m1".to_string()))],
        )];
        let md = render_markdown(&report);
        assert!(md.contains("### 1. line one line two _(supported)_"));
        assert!(md.contains("Sources:"));
    }

    #[test]
    fn render_includes_topic_and_sections() {
        let mut report = ResearchReport::new("caching");
        report.questions = vec!["what is it".to_string()];
        report.findings = vec![finding(
            "caches speed reads",
            ClaimStatus::Supported,
            vec![Provenance::new("memory", Some("mem_1".to_string()))],
        )];
        report.open_questions = vec!["eviction policy".to_string()];

        let md = render_markdown(&report);
        assert!(md.starts_with("# Research: caching"));
        assert!(md.contains("## Sub-questions"));
        assert!(md.contains("- what is it"));
        assert!(md.contains("### 1. caches speed reads _(supported)_"));
        assert!(md.contains("- memory: mem_1"));
        assert!(md.contains("## Open questions"));
        assert!(md.contains("- eviction policy"));
    }

    #[test]
    fn unsupported_finding_renders_without_sources() {
        let mut report = ResearchReport::new("t");
        report.findings = vec![finding(
            "unbacked claim",
            ClaimStatus::Unsupported,
            Vec::new(),
        )];
        let md = render_markdown(&report);
        assert!(md.contains("_(unsupported — no evidence found)_"));
        assert!(!md.contains("Sources:"));
    }

    #[test]
    fn candidates_only_from_supported_findings() {
        let mut report = ResearchReport::new("t");
        report.findings = vec![
            finding(
                "backed",
                ClaimStatus::Supported,
                vec![Provenance::new("knowledge", Some("a.rs:1-3".to_string()))],
            ),
            finding("unbacked", ClaimStatus::Unsupported, Vec::new()),
            finding("supported-but-empty", ClaimStatus::Supported, Vec::new()),
        ];
        let candidates = candidates_from(&report, 0.3);
        assert_eq!(
            candidates.len(),
            1,
            "only a supported, backed finding qualifies"
        );
        assert_eq!(candidates[0].body, "backed");
        assert_eq!(candidates[0].confidence, 0.3);
        assert_eq!(candidates[0].provenance.len(), 1);
    }

    #[test]
    fn empty_report_renders_placeholders() {
        let md = render_markdown(&ResearchReport::new("empty"));
        assert!(md.contains("_None._"));
        assert!(md.contains("_No findings._"));
    }
}
