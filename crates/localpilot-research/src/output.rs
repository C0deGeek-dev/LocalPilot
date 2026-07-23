//! Rendering a [`ResearchReport`] into the two outputs: a human-readable
//! Markdown artefact and review-gated memory-candidate specs.
//!
//! Both are pure and host-neutral. Writing the artefact to disk and enqueuing
//! the candidates into LocalMind's review queue happen in the binding layer
//! (subject 04): a candidate is never auto-accepted into durable memory.

use crate::{
    flatten_whitespace, ClaimStatus, CoverageVerdict, Provenance, ResearchReport, SourceAccount,
};

/// Ceiling on raw evidence rendered inline in the Markdown artefact and the
/// review candidate. Deliberately sized *above* the largest snippet a source
/// can gather (the web fetch bound is 64 KiB of already-reduced text), so a
/// finding's full source normally rides intact and truncation is a loud
/// safety net, never a display budget — a reviewer needs the whole content
/// (LocalHub#1), and a silent mid-word cut kept resurfacing as "knowledge is
/// cut off."
const MAX_EVIDENCE_CHARS: usize = 100_000;

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

    if !report.coverage.is_empty() {
        out.push_str("## Coverage\n\n");
        out.push_str(&format!(
            "_Retrieval ran {} round(s)._\n\n",
            report.rounds_run
        ));
        out.push_str(
            "| Sub-question | Verdict | Evidence | Corroborations | Origins | Families |\n",
        );
        out.push_str("|---|---|---|---|---|---|\n");
        for coverage in &report.coverage {
            let verdict = match coverage.verdict {
                CoverageVerdict::Covered => "covered",
                CoverageVerdict::CoveredSingleSource => {
                    "covered (single source — not independently corroborated)"
                }
                CoverageVerdict::Weak => "weak",
                CoverageVerdict::Open => "open",
            };
            out.push_str(&format!(
                "| {} | {verdict} | {} | {} | {} | {} |\n",
                flatten_whitespace(&coverage.question).replace('|', "\\|"),
                coverage.evidence_count,
                coverage.strong_evidence,
                coverage.distinct_origins,
                coverage.distinct_families
            ));
        }
        out.push('\n');
        push_retrieval_accounting(&mut out, report);
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

    if !report.retrieval_notes.is_empty() {
        out.push_str("## Retrieval notes\n\n");
        for note in &report.retrieval_notes {
            out.push_str(&format!("- {note}\n"));
        }
        out.push('\n');
    }

    out
}

/// Render the per-question, per-source retrieval accounting: counts and
/// reasons only — what each source proposed, admitted, rejected, skipped, or
/// failed — never source content or unredacted queries. With web enabled, a
/// question that web contributed no admitted evidence to is marked as an
/// explicit source gap so local-only coverage never reads as cross-validated
/// (LocalHub#33).
fn push_retrieval_accounting(out: &mut String, report: &ResearchReport) {
    if report.coverage.iter().all(|c| c.accounts.is_empty()) {
        return;
    }
    out.push_str("### Retrieval accounting\n\n");
    for coverage in &report.coverage {
        if coverage.accounts.is_empty() {
            continue;
        }
        out.push_str(&format!("- {}\n", flatten_whitespace(&coverage.question)));
        for account in &coverage.accounts {
            out.push_str(&format!("  - {}\n", account_line(account)));
            for note in &account.admitted_notes {
                out.push_str(&format!("    - {}\n", flatten_whitespace(note)));
            }
        }
        if report.web_enabled == Some(true) {
            let web_admitted = coverage
                .accounts
                .iter()
                .filter(|account| account.source == "web")
                .map(|account| account.admitted)
                .sum::<usize>();
            if web_admitted == 0 {
                out.push_str(
                    "  - source gap: web was enabled but contributed no admitted evidence — \
                     this question is not independently corroborated online\n",
                );
            }
        }
    }
    out.push('\n');
}

/// One source's account as a single readable line, listing only non-zero
/// outcomes; a source that proposed nothing says so explicitly.
fn account_line(account: &SourceAccount) -> String {
    if account.proposed == 0 && account.admitted == 0 {
        let failure = if account.failed > 0 {
            format!(" ({} call(s) failed)", account.failed)
        } else {
            String::new()
        };
        return format!("{}: no candidates returned{failure}", account.source);
    }
    let mut parts = vec![
        format!("{} proposed", account.proposed),
        format!("{} admitted", account.admitted),
    ];
    if account.rejected_relevance > 0 {
        parts.push(format!(
            "{} rejected (low relevance)",
            account.rejected_relevance
        ));
    }
    if account.below_floor > 0 {
        parts.push(format!("{} below admission floor", account.below_floor));
    }
    if account.policy_skipped > 0 {
        parts.push(format!("{} skipped by policy", account.policy_skipped));
    }
    if account.redirected > 0 {
        parts.push(format!("{} redirect(s) not followed", account.redirected));
    }
    if account.failed > 0 {
        parts.push(format!("{} fetch failure(s)", account.failed));
    }
    format!("{}: {}", account.source, parts.join(", "))
}

/// Render a finding's raw evidence as a self-contained fenced block titled
/// `Evidence:`. The fence is chosen longer than any backtick run in the content,
/// so a snippet that itself contains ``` ``` ``` can never break out of the block.
/// Evidence normally renders in full ([`MAX_EVIDENCE_CHARS`] sits above every
/// gather bound); should the safety net ever trip, the cut lands on a line
/// boundary and says exactly how much was kept — never a silent mid-word `…`.
/// Shared by the Markdown report and the review-queue candidate so both show
/// the reviewer the same full source under the distilled claim.
#[must_use]
pub fn evidence_block(evidence: &str) -> String {
    let (kept, total) = clip_evidence(evidence);
    let fence = backtick_fence(kept);
    let mut out = String::with_capacity(kept.len() + fence.len() * 2 + 16);
    out.push_str("Evidence:\n");
    out.push_str(&fence);
    out.push('\n');
    out.push_str(kept);
    if let Some(total) = total {
        out.push_str(&format!(
            "\n… (evidence truncated: first {} of {} characters shown)",
            kept.chars().count(),
            total
        ));
    }
    out.push('\n');
    out.push_str(&fence);
    out
}

/// Clip evidence to the safety-net ceiling. Returns the kept slice and, when a
/// cut happened, the original character count. The cut prefers the last line
/// boundary inside the budget (so it never lands mid-word) unless that would
/// discard more than half of the budget — a single enormous line — in which
/// case it falls back to a plain character cut.
fn clip_evidence(evidence: &str) -> (&str, Option<usize>) {
    let total = evidence.chars().count();
    if total <= MAX_EVIDENCE_CHARS {
        return (evidence, None);
    }
    let byte_end = evidence
        .char_indices()
        .nth(MAX_EVIDENCE_CHARS)
        .map_or(evidence.len(), |(index, _)| index);
    let head = &evidence[..byte_end];
    let cut = match head.rfind('\n') {
        Some(newline) if newline >= byte_end / 2 => newline,
        _ => byte_end,
    };
    (&evidence[..cut], Some(total))
}

/// Append a finding's raw evidence as a fenced block to a Markdown report.
fn push_evidence_block(out: &mut String, evidence: &str) {
    out.push_str(&evidence_block(evidence));
    out.push_str("\n\n");
}

/// A backtick fence at least one longer than the longest backtick run in `text`
/// (minimum three), so `text` cannot terminate the fenced block early.
pub(crate) fn backtick_fence(text: &str) -> String {
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
    fn evidence_well_past_the_old_display_budget_renders_in_full() {
        // LocalHub#1 round 5: a reduced docs page is routinely tens of
        // kilobytes, and the old 4000-char display budget silently cut it
        // mid-word ("… (truncated)"). Full content must ride the block.
        let evidence = "a line of real content\n".repeat(1000); // ~23k chars
        let block = evidence_block(&evidence);
        assert!(
            !block.contains("truncated"),
            "content under the safety net is never cut"
        );
        assert!(block.contains("a line of real content"));
        assert!(
            block.chars().count() > 20_000,
            "the full content is present: {} chars",
            block.chars().count()
        );
    }

    #[test]
    fn evidence_over_the_safety_net_cuts_on_a_line_boundary_and_says_so() {
        let line = "x".repeat(99); // 100 chars with the newline
        let evidence = format!("{}\n", line).repeat(1100); // 110k chars
        let block = evidence_block(&evidence);
        let tail: String = block
            .chars()
            .rev()
            .take(200)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert!(
            block.contains("evidence truncated: first"),
            "the cut is loud and quantified: {tail}"
        );
        // Every kept line is intact — the cut landed on a boundary, not mid-word.
        let kept = block
            .lines()
            .filter(|l| l.starts_with('x'))
            .collect::<Vec<_>>();
        assert!(!kept.is_empty());
        assert!(
            kept.iter().all(|l| l.chars().count() == 99),
            "no mid-line cut"
        );
    }

    #[test]
    fn empty_report_renders_placeholders() {
        let md = render_markdown(&ResearchReport::new("empty"));
        assert!(md.contains("_None._"));
        assert!(md.contains("_No findings._"));
    }

    #[test]
    fn coverage_table_renders_verdicts_and_rounds() {
        let mut report = ResearchReport::new("t");
        report.rounds_run = 2;
        report.coverage = vec![
            crate::QuestionCoverage {
                question: "how do bones | joints bind".to_string(),
                verdict: CoverageVerdict::Covered,
                evidence_count: 4,
                strong_evidence: 5,
                distinct_origins: 3,
                distinct_families: 2,
                accounts: Vec::new(),
            },
            crate::QuestionCoverage {
                question: "gpu skinning cost".to_string(),
                verdict: CoverageVerdict::Open,
                evidence_count: 0,
                strong_evidence: 0,
                distinct_origins: 0,
                distinct_families: 0,
                accounts: Vec::new(),
            },
        ];
        let rendered = render_markdown(&report);
        assert!(rendered.contains("## Coverage"), "{rendered}");
        assert!(
            rendered.contains("_Retrieval ran 2 round(s)._"),
            "{rendered}"
        );
        assert!(
            rendered.contains("| how do bones \\| joints bind | covered | 4 | 5 | 3 | 2 |"),
            "pipe in the question is escaped: {rendered}"
        );
        assert!(
            rendered.contains("| gpu skinning cost | open | 0 | 0 | 0 | 0 |"),
            "{rendered}"
        );
    }

    #[test]
    fn retrieval_accounting_explains_source_outcomes_and_web_gaps() {
        let mut report = ResearchReport::new("t");
        report.rounds_run = 1;
        report.web_enabled = Some(true);
        let mut knowledge = SourceAccount::new("knowledge");
        knowledge.proposed = 5;
        knowledge.admitted = 2;
        knowledge.below_floor = 3;
        knowledge
            .admitted_notes
            .push("src/lib.rs:4-9 — raw 0.80, rank 1.00, admitted 0.40 (term overlap)".to_string());
        let mut web = SourceAccount::new("web");
        web.proposed = 3;
        web.rejected_relevance = 1;
        web.redirected = 1;
        web.failed = 1;
        report.coverage = vec![crate::QuestionCoverage {
            question: "how does the mixer blend".to_string(),
            verdict: CoverageVerdict::CoveredSingleSource,
            evidence_count: 5,
            strong_evidence: 2,
            distinct_origins: 2,
            distinct_families: 1,
            accounts: vec![knowledge, web],
        }];
        let rendered = render_markdown(&report);
        assert!(
            rendered.contains("covered (single source — not independently corroborated)"),
            "{rendered}"
        );
        assert!(rendered.contains("### Retrieval accounting"), "{rendered}");
        assert!(
            rendered.contains("knowledge: 5 proposed, 2 admitted, 3 below admission floor"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "web: 3 proposed, 0 admitted, 1 rejected (low relevance), \
                 1 redirect(s) not followed, 1 fetch failure(s)"
            ),
            "each zero-web cause is countable: {rendered}"
        );
        assert!(
            rendered.contains("source gap: web was enabled but contributed no admitted evidence"),
            "{rendered}"
        );
        assert!(
            rendered.contains("raw 0.80, rank 1.00, admitted 0.40 (term overlap)"),
            "admission diagnostics ride the report content-free: {rendered}"
        );
    }

    #[test]
    fn accounting_marks_a_source_that_proposed_nothing() {
        let mut report = ResearchReport::new("t");
        report.web_enabled = Some(true);
        let mut web = SourceAccount::new("web");
        web.failed = 1;
        report.coverage = vec![crate::QuestionCoverage {
            question: "q".to_string(),
            verdict: CoverageVerdict::Weak,
            evidence_count: 1,
            strong_evidence: 0,
            distinct_origins: 0,
            distinct_families: 0,
            accounts: vec![web],
        }];
        let rendered = render_markdown(&report);
        assert!(
            rendered.contains("web: no candidates returned (1 call(s) failed)"),
            "{rendered}"
        );
    }
}
