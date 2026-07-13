//! The bounded, coverage-driven research loop:
//! decompose → gather (multi-round) → cross-check → synthesise.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{
    flatten_whitespace, html_to_text, markdown_to_text, ClaimStatus, CoverageVerdict, Evidence,
    Finding, Provenance, QuestionCoverage, ResearchError, ResearchReport, SourceError, SourceSet,
    Synthesizer,
};

/// Longest a finding statement may be before it is treated as an over-long blob
/// and reduced to an excerpt, with the full text preserved as evidence.
const MAX_STATEMENT_CHARS: usize = 240;

/// Evidence below this relevance does not count toward coverage. Conservative:
/// the flat web relevance (0.5) and any bm25-derived score above noise pass.
const COVERAGE_RELEVANCE_FLOOR: f32 = 0.25;
/// A question is covered when at least this many floor-passing snippets…
const COVERED_MIN_EVIDENCE: usize = 2;
/// …come from at least this many distinct origins.
const COVERED_MIN_ORIGINS: usize = 2;
/// Follow-up queries asked per targeted question per round (the unmodified
/// original question is always retried alongside them).
const REFORMULATIONS_PER_ROUND: usize = 1;
/// Ceiling on the per-round retrieval-depth escalation multiplier.
const ESCALATION_MAX_FACTOR: usize = 3;
/// Word-shingle Jaccard similarity at or above which two snippets are the
/// same content (mirrors, syndication, overlapping chunks).
const NEAR_DUP_JACCARD: f32 = 0.7;
/// Words per shingle for near-duplicate detection.
const SHINGLE_WORDS: usize = 3;
/// Soft cap on snippets one origin may contribute to one question while other
/// origins are also answering it.
const DIVERSITY_ORIGIN_CAP: usize = 3;

/// Bounds on a research run. A host maps its resolved rails (ADR-0055) into
/// these so the loop cannot run unbounded.
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    /// Maximum number of sub-questions to pursue.
    pub max_questions: usize,
    /// Maximum evidence snippets to take from each source per question.
    pub per_source_evidence: usize,
    /// Maximum retrieval rounds. `1` reproduces the single-pass behaviour;
    /// later rounds re-query only questions that are not yet covered.
    pub max_rounds: usize,
    /// Hard cap on total evidence snippets across the whole run.
    pub max_total_evidence: usize,
    /// Optional wall-clock budget for the retrieval phase.
    pub time_budget: Option<Duration>,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            max_questions: 6,
            per_source_evidence: 5,
            max_rounds: 3,
            max_total_evidence: 120,
            time_budget: None,
        }
    }
}

/// External control over a running loop.
#[derive(Debug, Clone, Default)]
pub struct RunControl {
    /// When set and flipped true, the loop stops at the next question boundary
    /// and returns a partial (but well-formed) outcome.
    pub stop: Option<Arc<AtomicBool>>,
}

impl RunControl {
    fn stop_requested(&self) -> bool {
        self.stop
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }
}

/// One retrieval round's account, for progress display and the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundSummary {
    /// 1-based round number.
    pub round: usize,
    /// Questions this round re-queried.
    pub targeted: usize,
    /// Previously-unseen evidence snippets the round produced.
    pub new_evidence: usize,
    /// Running total of evidence snippets.
    pub total_evidence: usize,
    /// Coverage tallies after the round.
    pub covered: usize,
    pub weak: usize,
    pub open: usize,
}

/// The outcome of a run: the report plus any non-fatal source errors so the
/// caller can surface a degraded gather without failing the run.
#[derive(Debug)]
pub struct RunOutcome {
    /// The synthesised report.
    pub report: ResearchReport,
    /// Errors from individual sources that were skipped (best-effort gather).
    pub source_errors: Vec<SourceError>,
    /// Per-round retrieval account, in order.
    pub rounds: Vec<RoundSummary>,
}

/// Per-question retrieval state across rounds.
struct QuestionState {
    question: String,
    evidence: Vec<Evidence>,
}

/// Run the bounded research loop for `topic` with default control (no external
/// stop flag).
pub async fn run_research(
    topic: &str,
    sources: &SourceSet,
    synth: &dyn Synthesizer,
    bounds: Bounds,
) -> Result<RunOutcome, ResearchError> {
    run_research_controlled(topic, sources, synth, bounds, RunControl::default()).await
}

/// Run the bounded, coverage-driven research loop for `topic`.
///
/// Round 1 gathers evidence for every sub-question. Each later round re-queries
/// only the questions that are not yet covered — retrying the original question
/// and up to one reformulated query proposed by the synthesizer (deterministic
/// pseudo-relevance expansion by default). The loop stops when every question
/// is covered, a round yields no new evidence (saturation), the round cap,
/// evidence cap, or time budget is reached, or an external stop is requested —
/// always returning a well-formed outcome with per-question coverage.
pub async fn run_research_controlled(
    topic: &str,
    sources: &SourceSet,
    synth: &dyn Synthesizer,
    bounds: Bounds,
    control: RunControl,
) -> Result<RunOutcome, ResearchError> {
    let started = Instant::now();
    let mut report = ResearchReport::new(topic);
    let mut source_errors = Vec::new();
    let mut rounds = Vec::new();

    let questions: Vec<String> = synth
        .decompose(topic, bounds.max_questions)
        .await?
        .into_iter()
        .take(bounds.max_questions)
        .collect();
    let mut states: Vec<QuestionState> = questions
        .iter()
        .map(|question| QuestionState {
            question: question.clone(),
            evidence: Vec::new(),
        })
        .collect();
    report.questions = questions;

    // One key per evidence snippet ever seen, across rounds and reformulated
    // queries — dedup is against *seen*, not kept, so a round that only
    // re-finds old ground reads as saturation.
    let mut seen: HashSet<String> = HashSet::new();
    let mut total_evidence = 0usize;
    // Loud-cap accounting (never silent): folds, diversity drops, cap hits.
    let mut near_dup_folds = 0usize;
    let mut diversity_drops = 0usize;
    let mut evidence_cap_hit = false;
    let mut time_budget_hit = false;
    let mut stopped_early = false;

    'rounds: for round in 1..=bounds.max_rounds.max(1) {
        let targets: Vec<usize> = states
            .iter()
            .enumerate()
            .filter(|(_, state)| round == 1 || assess(state).verdict != CoverageVerdict::Covered)
            .map(|(index, _)| index)
            .collect();
        if targets.is_empty() {
            break; // everything covered
        }
        let targeted = targets.len();
        let mut round_new = 0usize;

        for index in targets {
            if control.stop_requested() {
                stopped_early = true;
            } else if over_budget(&bounds, started) {
                time_budget_hit = true;
            } else if total_evidence >= bounds.max_total_evidence {
                evidence_cap_hit = true;
            }
            if stopped_early || time_budget_hit || evidence_cap_hit {
                summarize_round(
                    &mut rounds,
                    round,
                    targeted,
                    round_new,
                    total_evidence,
                    &states,
                );
                break 'rounds;
            }
            let question = states[index].question.clone();
            let mut queries = vec![question.clone()];
            if round > 1 {
                let follow_ups = synth
                    .reformulate(&question, &states[index].evidence, REFORMULATIONS_PER_ROUND)
                    .await
                    .unwrap_or_default();
                queries.extend(follow_ups);
            }
            // Escalate retrieval depth for questions that keep coming back:
            // round 1 gathers at the configured per-source depth, later rounds
            // widen it (a re-queried source can surface hits past the first
            // page of results), still inside the total-evidence cap.
            let depth = bounds
                .per_source_evidence
                .saturating_mul(round.min(ESCALATION_MAX_FACTOR));
            for query in queries {
                let (evidence, mut errors) = sources.gather_all(&query, depth).await;
                source_errors.append(&mut errors);
                for mut item in evidence {
                    if total_evidence >= bounds.max_total_evidence {
                        evidence_cap_hit = true;
                        break;
                    }
                    if !seen.insert(evidence_key(&item)) {
                        continue; // exact re-find of known ground
                    }
                    // Near-duplicate content from a *different* origin folds
                    // into the snippet it duplicates — its provenance rides
                    // along (also_from), it just doesn't repeat the content.
                    if let Some(kept) = find_near_duplicate(&states[index].evidence, &item.snippet)
                    {
                        states[index].evidence[kept]
                            .also_from
                            .push(item.provenance.clone());
                        near_dup_folds += 1;
                        continue;
                    }
                    // Evidence groups under the original question, not the
                    // reformulated query that happened to retrieve it.
                    item.question = question.clone();
                    states[index].evidence.push(item);
                    round_new += 1;
                    total_evidence += 1;
                }
            }
            // Soft per-origin diversity cap, applied after the question's full
            // gather so it cannot depend on source order: once more than one
            // origin is answering, no origin keeps more than its share — but a
            // lone origin is never capped.
            let dropped = enforce_diversity(&mut states[index].evidence);
            if dropped > 0 {
                diversity_drops += dropped;
                round_new = round_new.saturating_sub(dropped);
                total_evidence = total_evidence.saturating_sub(dropped);
            }
        }

        summarize_round(
            &mut rounds,
            round,
            targeted,
            round_new,
            total_evidence,
            &states,
        );
        if round_new == 0 {
            break; // saturation: the round found nothing new anywhere
        }
    }

    report.rounds_run = rounds.len();
    if near_dup_folds > 0 {
        report.retrieval_notes.push(format!(
            "folded {near_dup_folds} near-duplicate snippet(s); their provenance is kept on the surviving evidence"
        ));
    }
    if diversity_drops > 0 {
        report.retrieval_notes.push(format!(
            "dropped {diversity_drops} snippet(s) beyond the per-origin diversity cap ({DIVERSITY_ORIGIN_CAP} per question per origin)"
        ));
    }
    if evidence_cap_hit {
        report.retrieval_notes.push(format!(
            "evidence cap reached ({} snippets) — retrieval stopped before the sources were exhausted",
            bounds.max_total_evidence
        ));
    }
    if time_budget_hit {
        report
            .retrieval_notes
            .push("time budget reached — retrieval stopped early".to_string());
    }
    if stopped_early {
        report
            .retrieval_notes
            .push("stopped by cancellation — results are partial".to_string());
    }
    report.coverage = states.iter().map(assess).collect();
    report.open_questions = report
        .coverage
        .iter()
        .filter(|coverage| coverage.verdict == CoverageVerdict::Open)
        .map(|coverage| coverage.question.clone())
        .collect();

    let all_evidence: Vec<Evidence> = states
        .into_iter()
        .flat_map(|state| state.evidence)
        .collect();
    let mut findings = synth.synthesize(topic, &all_evidence).await?;
    sanitize_findings(&mut findings);
    cross_check(&mut findings);
    report.findings = findings;

    Ok(RunOutcome {
        report,
        source_errors,
        rounds,
    })
}

fn over_budget(bounds: &Bounds, started: Instant) -> bool {
    bounds
        .time_budget
        .is_some_and(|budget| started.elapsed() >= budget)
}

fn summarize_round(
    rounds: &mut Vec<RoundSummary>,
    round: usize,
    targeted: usize,
    new_evidence: usize,
    total_evidence: usize,
    states: &[QuestionState],
) {
    let mut covered = 0;
    let mut weak = 0;
    let mut open = 0;
    for state in states {
        match assess(state).verdict {
            CoverageVerdict::Covered => covered += 1,
            CoverageVerdict::Weak => weak += 1,
            CoverageVerdict::Open => open += 1,
        }
    }
    rounds.push(RoundSummary {
        round,
        targeted,
        new_evidence,
        total_evidence,
        covered,
        weak,
        open,
    });
}

/// Deterministic per-question coverage scoring: floor-passing evidence counts,
/// and independence is measured in distinct origins.
fn assess(state: &QuestionState) -> QuestionCoverage {
    let strong: Vec<&Evidence> = state
        .evidence
        .iter()
        .filter(|item| item.relevance >= COVERAGE_RELEVANCE_FLOOR)
        .collect();
    // Folded near-duplicates still count — both as independent origins and as
    // corroborating observations: the same content found on a second origin
    // is exactly the independence signal the covered bar asks for.
    let mut origins: HashSet<String> = HashSet::new();
    let mut observations = 0usize;
    for item in &strong {
        origins.insert(origin_key(item));
        observations += 1 + item.also_from.len();
        for extra in &item.also_from {
            origins.insert(provenance_origin(extra));
        }
    }
    let verdict = if observations >= COVERED_MIN_EVIDENCE && origins.len() >= COVERED_MIN_ORIGINS {
        CoverageVerdict::Covered
    } else if state.evidence.is_empty() {
        CoverageVerdict::Open
    } else {
        CoverageVerdict::Weak
    };
    QuestionCoverage {
        question: state.question.clone(),
        verdict,
        evidence_count: state.evidence.len(),
        strong_evidence: observations,
        distinct_origins: origins.len(),
    }
}

/// Identity of one evidence snippet for cross-round dedup: origin plus a
/// snippet prefix (whitespace-normalized), so re-fetching the same content via
/// a reformulated query does not count as progress.
fn evidence_key(item: &Evidence) -> String {
    let prefix: String = flatten_whitespace(&item.snippet).chars().take(80).collect();
    format!(
        "{}|{}|{prefix}",
        item.provenance.source,
        item.provenance.locator.as_deref().unwrap_or_default(),
    )
}

/// Enforce the per-origin diversity cap on one question's evidence: keep at
/// most [`DIVERSITY_ORIGIN_CAP`] snippets per origin (highest relevance
/// first), never capping when a single origin is the only one answering.
/// Returns how many snippets were dropped.
fn enforce_diversity(evidence: &mut Vec<Evidence>) -> usize {
    let origins: HashSet<String> = evidence.iter().map(origin_key).collect();
    if origins.len() <= 1 {
        return 0;
    }
    let mut order: Vec<usize> = (0..evidence.len()).collect();
    order.sort_by(|&a, &b| {
        evidence[b]
            .relevance
            .partial_cmp(&evidence[a].relevance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut kept_per_origin: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut drop_flags = vec![false; evidence.len()];
    for index in order {
        let origin = origin_key(&evidence[index]);
        let count = kept_per_origin.entry(origin).or_default();
        if *count >= DIVERSITY_ORIGIN_CAP {
            drop_flags[index] = true;
        } else {
            *count += 1;
        }
    }
    let before = evidence.len();
    let mut flags = drop_flags.into_iter();
    evidence.retain(|_| !flags.next().unwrap_or(false));
    before - evidence.len()
}

/// Index of a kept snippet that `snippet` near-duplicates, if any: word-shingle
/// Jaccard at or above [`NEAR_DUP_JACCARD`]. Deterministic and std-only.
fn find_near_duplicate(kept: &[Evidence], snippet: &str) -> Option<usize> {
    let incoming = shingles(snippet);
    if incoming.is_empty() {
        return None;
    }
    kept.iter().position(|existing| {
        let existing = shingles(&existing.snippet);
        jaccard(&incoming, &existing) >= NEAR_DUP_JACCARD
    })
}

/// Hashed word `SHINGLE_WORDS`-grams of whitespace-normalized, lowercased text.
/// Short texts fall back to single-word shingles so they still compare.
fn shingles(text: &str) -> HashSet<u64> {
    use std::hash::{Hash, Hasher};
    let normalized = flatten_whitespace(text).to_ascii_lowercase();
    let words: Vec<&str> = normalized.split(' ').filter(|w| !w.is_empty()).collect();
    let width = if words.len() >= SHINGLE_WORDS {
        SHINGLE_WORDS
    } else {
        1
    };
    words
        .windows(width)
        .map(|window| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            window.hash(&mut hasher);
            hasher.finish()
        })
        .collect()
}

fn jaccard(a: &HashSet<u64>, b: &HashSet<u64>) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let score = intersection as f32 / union as f32;
    score
}

/// Origin of one snippet for independence counting: for web evidence the host
/// of its URL, otherwise the source label plus locator (a file, a memory id).
fn origin_key(item: &Evidence) -> String {
    provenance_origin(&item.provenance)
}

fn provenance_origin(provenance: &Provenance) -> String {
    let locator = provenance.locator.as_deref().unwrap_or_default();
    if provenance.source == "web" {
        if let Some(rest) = locator.split_once("://").map(|(_, rest)| rest) {
            let host = rest.split('/').next().unwrap_or(rest);
            return format!("web|{host}");
        }
    }
    format!("{}|{locator}", provenance.source)
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

/// Keep findings readable: a statement that is a code/HTML/Markdown blob or
/// too long is no claim — its raw text is preserved as `evidence` and the
/// statement is replaced with a concise, single-line excerpt titled with its
/// source. A clean statement is only flattened to one line. This runs on every
/// finding, so neither the rendered report nor the enqueued memory candidates
/// can carry a raw source chunk.
fn sanitize_findings(findings: &mut [Finding]) {
    for finding in findings.iter_mut() {
        let flat = flatten_whitespace(&finding.statement);
        if looks_like_markup(&finding.statement) || flat.chars().count() > MAX_STATEMENT_CHARS {
            preserve_evidence(finding);
            // The excerpt is distilled from the *original* multi-line text:
            // Markdown markers are positional (a `# ` or ``` only means
            // anything at line start), so flattening first would leave them
            // unstrippable.
            finding.statement = titled_excerpt(&finding.statement, &finding.supporting);
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

/// Derive a short claim from a raw blob: strip crude markup and Markdown
/// syntax, take a leading excerpt, and title it with its source so the reader
/// knows it is a source excerpt, not a synthesised conclusion. The full text
/// stays in `evidence`.
fn titled_excerpt(raw: &str, supporting: &[Provenance]) -> String {
    let source = supporting
        .first()
        .map_or("source", |provenance| provenance.source.as_str());
    let stripped = strip_markup(raw);
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
/// survive as junk), strip the remaining tags, then flatten Markdown syntax —
/// web evidence now arrives as Markdown, so fences, heading/list markers, and
/// `[text](url)` link syntax would otherwise leak into the one-line claim.
/// Delegates to [`html_to_text`] then [`markdown_to_text`], and flattens the
/// line breaks away for the heading-safe excerpt.
fn strip_markup(text: &str) -> String {
    flatten_whitespace(&markdown_to_text(&html_to_text(text)))
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
        assert!(
            findings[0].statement.starts_with("Excerpt from memory:"),
            "an over-long statement is titled as an excerpt like any other blob: {}",
            findings[0].statement
        );
        assert!(findings[0].statement.chars().count() <= MAX_STATEMENT_CHARS + 32);
        assert!(findings[0].evidence.is_some());
    }

    #[test]
    fn markdown_evidence_yields_a_prose_excerpt_without_markdown_syntax() {
        // Web evidence now arrives as Markdown; the one-line claim must not
        // leak heading markers, link syntax, or fences into the report heading
        // or the review queue.
        let raw = "# Tokio guide\n\nUse [the docs](https://docs.rs/tokio) first.\n\n```\nlet rt = Runtime::new();\n```\n"
            .repeat(4);
        let mut findings = vec![finding(&raw, vec![Provenance::new("web", None)])];
        sanitize_findings(&mut findings);
        let statement = &findings[0].statement;
        assert!(statement.starts_with("Excerpt from web:"), "{statement}");
        assert!(!statement.contains('#'), "{statement}");
        assert!(!statement.contains("```"), "{statement}");
        assert!(
            statement.contains("Use the docs first."),
            "link collapses to its text: {statement}"
        );
        assert!(!statement.contains("https://docs.rs"), "{statement}");
        assert_eq!(findings[0].evidence.as_deref(), Some(raw.as_str()));
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
