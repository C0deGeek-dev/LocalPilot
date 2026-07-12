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
    /// The full raw source text this claim was distilled from, when the
    /// sanitize pass reduced a blob/over-long finding to a one-line excerpt.
    /// `None` for a clean synthesised claim (its body *is* the content). The
    /// binding layer renders it as a fenced evidence block under the claim, so
    /// the reviewer can read the full source without it breaking queue layout —
    /// the review item shows the same claim + evidence the Markdown report does.
    pub evidence: Option<String>,
    /// Confidence to attach: the finding's own relevance-derived
    /// `confidence`, capped by the caller's `confidence_cap` (never above it,
    /// however strong the match, because research findings are
    /// machine-derived and unreviewed).
    pub confidence: f32,
}

/// Derive review-queue candidate specs from a report.
///
/// Only **supported** findings with at least one provenance become candidates,
/// so unsupported or unbacked claims never reach the review queue. The candidate
/// `body` is the finding's `statement`, which the sanitize pass has already
/// reduced to a readable, single-line, length-capped excerpt titled with its
/// source. When that reduction happened, the finding's raw source blob is carried
/// through in `evidence` so the reviewer can read the full source it was distilled
/// from — the binding layer renders it as a fenced block under the claim, exactly
/// as the Markdown report does. This keeps the queue readable (a distilled claim
/// leads, never a raw log or code chunk) while carrying the full content the
/// reviewer needs to judge and reuse the finding — synthesis is
/// provenance-preserving and heuristic, so a finding *being* an excerpt is the
/// common case, not a reason to discard it or drop its source.
///
/// Each candidate's `confidence` is the finding's own relevance-derived
/// `confidence`, capped at `confidence_cap` — never a single flat value
/// applied uniformly, so a strong multi-source match reads as more
/// trustworthy than a weak, single-incidental-word one, without ever
/// exceeding the caller's low-trust ceiling for unreviewed candidates.
#[must_use]
pub fn candidates_from(report: &ResearchReport, confidence_cap: f32) -> Vec<CandidateSpec> {
    report
        .findings
        .iter()
        .filter(|f| f.status == ClaimStatus::Supported && !f.supporting.is_empty())
        .map(|f| CandidateSpec {
            body: f.statement.clone(),
            provenance: f.supporting.clone(),
            evidence: f.evidence.clone(),
            confidence: f.confidence.clamp(0.0, 1.0).min(confidence_cap),
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

/// Render a finding's raw evidence as a self-contained fenced block titled
/// `Evidence:`. The fence is chosen longer than any backtick run in the content,
/// so a snippet that itself contains ``` ``` ``` can never break out of the block.
/// Over-long evidence is truncated to [`MAX_EVIDENCE_CHARS`]. Shared by the
/// Markdown report and the review-queue candidate so both show the reviewer the
/// same full source under the distilled claim.
#[must_use]
pub fn evidence_block(evidence: &str) -> String {
    let truncated: String = evidence.chars().take(MAX_EVIDENCE_CHARS).collect();
    let clipped = evidence.chars().count() > MAX_EVIDENCE_CHARS;
    let fence = backtick_fence(&truncated);
    let mut out = String::with_capacity(truncated.len() + fence.len() * 2 + 16);
    out.push_str("Evidence:\n");
    out.push_str(&fence);
    out.push('\n');
    out.push_str(&truncated);
    if clipped {
        out.push_str("\n… (truncated)");
    }
    out.push('\n');
    out.push_str(&fence);
    out
}

/// Append a finding's raw evidence as a fenced block to a Markdown report.
fn push_evidence_block(out: &mut String, evidence: &str) {
    out.push_str(&evidence_block(evidence));
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
            confidence: 1.0,
        }
    }

    #[test]
    fn sanitized_excerpt_findings_become_candidates_with_the_distilled_statement() {
        // A supported, backed finding whose statement was reduced from a raw
        // source blob (the sanitize pass set `evidence`) is still a candidate:
        // the body is the already-distilled, readable `statement`, and the raw
        // blob in `evidence` stays in the rendered report only. Dropping these
        // would zero the queue, because provenance-preserving heuristic
        // synthesis makes almost every finding an excerpt.
        let mut report = ResearchReport::new("t");
        let claim = finding(
            "Caches speed up repeated reads.",
            ClaimStatus::Supported,
            vec![Provenance::new("web", None)],
        );
        let raw_blob = "TypeError: Cannot read properties of undefined";
        let mut excerpt = finding(
            "Excerpt from knowledge: TypeError: Cannot read properties of undefined…",
            ClaimStatus::Supported,
            vec![Provenance::new(
                "knowledge",
                Some("console.log:1-97".to_string()),
            )],
        );
        excerpt.evidence = Some(raw_blob.to_string());
        report.findings = vec![claim, excerpt];

        let candidates = candidates_from(&report, 0.4);
        assert_eq!(candidates.len(), 2, "both backed findings qualify");
        assert_eq!(candidates[0].body, "Caches speed up repeated reads.");
        // A clean claim carries no separate evidence: its body is the content.
        assert_eq!(candidates[0].evidence, None);
        // The excerpt's body is its distilled statement, never the raw blob.
        assert_eq!(
            candidates[1].body,
            "Excerpt from knowledge: TypeError: Cannot read properties of undefined…"
        );
        assert_ne!(
            candidates[1].body, raw_blob,
            "raw blob never becomes the body"
        );
        // …but the full raw source is carried through as evidence, so the
        // reviewer can read what the excerpt was distilled from.
        assert_eq!(
            candidates[1].evidence.as_deref(),
            Some(raw_blob),
            "the full source rides the candidate as evidence"
        );
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
    fn candidate_confidence_is_capped_but_reflects_the_findings_own_relevance() {
        // Bug it prevents: every research candidate reading the same flat
        // confidence regardless of how strong (or weak/incidental) the
        // underlying match actually was.
        let mut report = ResearchReport::new("t");
        let mut weak = finding(
            "weak match",
            ClaimStatus::Supported,
            vec![Provenance::new("knowledge", Some("a.rs:1-3".to_string()))],
        );
        weak.confidence = 0.1;
        let mut strong = finding(
            "strong match",
            ClaimStatus::Supported,
            vec![Provenance::new("knowledge", Some("b.rs:1-3".to_string()))],
        );
        strong.confidence = 0.9;
        report.findings = vec![weak, strong];

        let candidates = candidates_from(&report, 0.4);
        assert_eq!(candidates[0].confidence, 0.1, "weak match stays low");
        assert_eq!(
            candidates[1].confidence, 0.4,
            "strong match is capped at the ceiling, not let through uncapped"
        );
    }

    #[test]
    fn empty_report_renders_placeholders() {
        let md = render_markdown(&ResearchReport::new("empty"));
        assert!(md.contains("_None._"));
        assert!(md.contains("_No findings._"));
    }
}
