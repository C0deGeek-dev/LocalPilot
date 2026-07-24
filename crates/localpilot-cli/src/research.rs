//! Binding layer for the `/research` mode and `localpilot research` subcommand.
//!
//! The host-neutral loop lives in `localpilot-research`; this module supplies
//! the concrete local [`Source`]s over LocalPilot's retrieval primitives and
//! the run orchestrator that renders a report artefact and enqueues
//! review-gated memory candidates. Web research is on by default — disclosed,
//! allowlist-gated, and audited — with `--no-web` and
//! `[research.web].enabled = false` as kill switches.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use localpilot_config::{CliOverrides, Config, ConfigPaths};
use localpilot_core::{Message, Role};
use localpilot_llm::{ModelEvent, ModelProvider, ModelRequest, ProviderRegistry};
use localpilot_mcp::{extract_candidate_urls, McpClient, SearchCallError};
use localpilot_research::{
    candidates_from, evidence_block, html_to_markdown, iframe_sources, prepare_query,
    render_markdown, render_signal, run_research_controlled, term_overlap_relevance,
    AdmissionTrail, AuditEntry, Bounds, CoverageVerdict, Evidence, FetchDecision, Finding,
    Gathered, HeuristicSynthesizer, Provenance, RenderBounds, RenderGate, RenderRequest,
    RenderSignal, Renderer, ResearchError, ResearchReport, RunControl, Source, SourceAccount,
    SourceError, SourceSet, Synthesizer, WebAccess,
};

/// Ceiling on the confidence attached to research-derived memory candidates:
/// low, because they are machine-derived and unreviewed — they route to
/// review, never accepted. Each candidate's actual confidence is its
/// finding's own relevance-derived value, capped here — never a single flat
/// value applied uniformly regardless of match quality.
const RESEARCH_CANDIDATE_CONFIDENCE_CAP: f32 = 0.3;

/// Relevance for accepted-memory evidence: already human-reviewed (ADR-0011),
/// so a hit is trusted at face value rather than scored — unlike a knowledge
/// hit, which is unreviewed and carries its own match-quality signal.
const MEMORY_EVIDENCE_RELEVANCE: f32 = 1.0;

/// A resolved model the binding layer can call for topic decomposition and, on
/// the web path, candidate-URL proposal. Wraps the configured default provider
/// and its model id. Absent when no provider/model is configured, in which case
/// the run degrades to the deterministic heuristic and skips web URL proposal.
#[derive(Clone)]
struct ModelHandle {
    provider: Arc<dyn ModelProvider>,
    model: String,
}

impl ModelHandle {
    /// Resolve the default provider and its configured model from config.
    /// `None` when no model is configured, so callers degrade gracefully rather
    /// than failing a run.
    fn from_config(config: &Config) -> Option<Self> {
        let model = config.resolve_model(None)?;
        let registry = ProviderRegistry::from_config(config).ok()?;
        let provider = Arc::clone(registry.default_provider()?);
        Some(Self { provider, model })
    }

    /// Send a single user prompt and collect the streamed final-answer text.
    /// Reasoning deltas and tool calls are ignored — only the answer is used.
    async fn complete(&self, prompt: &str) -> anyhow::Result<String> {
        let request =
            ModelRequest::new(self.model.clone(), vec![Message::text(Role::User, prompt)]);
        let mut stream = self.provider.stream(request).await?;
        let mut answer = String::new();
        while let Some(event) = stream.next().await {
            match event? {
                ModelEvent::TextDelta(delta) => answer.push_str(&delta),
                ModelEvent::Done => break,
                _ => {}
            }
        }
        Ok(answer)
    }
}

/// The synthesizer for a research run.
///
/// When a model is configured it asks the model to *decompose* the topic into
/// sub-questions; **synthesis always stays the provenance-preserving heuristic**
/// (every finding is a gathered evidence snippet carrying that snippet's
/// provenance), so the model can never inject an unsupported or unbacked claim.
/// With no model it degrades to [`HeuristicSynthesizer`] for decomposition too,
/// yielding the topic as a single question.
struct CliSynthesizer {
    model: Option<ModelHandle>,
}

#[async_trait]
impl Synthesizer for CliSynthesizer {
    async fn decompose(
        &self,
        topic: &str,
        max_questions: usize,
    ) -> Result<Vec<String>, ResearchError> {
        let Some(model) = &self.model else {
            return HeuristicSynthesizer.decompose(topic, max_questions).await;
        };
        let prompt = decompose_prompt(topic, max_questions);
        let questions = match model.complete(&prompt).await {
            Ok(text) => parse_questions(&text, max_questions),
            Err(_) => Vec::new(),
        };
        if questions.is_empty() {
            // Model unavailable, errored, or returned nothing usable: fall back
            // to the heuristic so a run always has at least one question.
            HeuristicSynthesizer.decompose(topic, max_questions).await
        } else {
            Ok(questions)
        }
    }

    async fn synthesize(
        &self,
        topic: &str,
        evidence: &[Evidence],
    ) -> Result<Vec<Finding>, ResearchError> {
        HeuristicSynthesizer.synthesize(topic, evidence).await
    }
}

/// Build the topic-decomposition prompt. One sub-question per line keeps the
/// reply parseable without structured-output support.
fn decompose_prompt(topic: &str, max_questions: usize) -> String {
    format!(
        "Break the research topic below into at most {max_questions} focused \
         sub-questions that together cover it. Each sub-question MUST stay within \
         the topic's scope: keep every load-bearing constraint the topic names — \
         its framework, library, language, runtime, platform, and version — even \
         when a sub-question narrows to one aspect. Do not broaden a sub-question \
         into a general question that a different framework or tool could answer. \
         Output one sub-question per line, with no numbering, bullets, or \
         commentary.\n\nTopic: {topic}"
    )
}

/// Parse a decomposition reply: one sub-question per line. Trim each line, drop
/// empties, and take at most `max_questions`.
fn parse_questions(text: &str, max_questions: usize) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(max_questions)
        .map(str::to_string)
        .collect()
}

/// Resolved options for a research run.
pub struct ResearchOptions {
    /// Maximum sub-questions the run may pursue.
    pub max_questions: usize,
    /// Maximum retrieval rounds (`1` = single-pass).
    pub max_rounds: usize,
    /// Evidence snippets per source per question per query.
    pub per_source_evidence: usize,
    /// Hard cap on total evidence snippets across the run.
    pub max_total_evidence: usize,
    /// Optional wall-clock budget for the retrieval phase.
    pub time_budget: Option<Duration>,
    /// Directory the report artefact is written to.
    pub output_dir: PathBuf,
    /// Whether to write the report artefact.
    pub write_report: bool,
    /// Whether to enqueue review-gated memory candidates.
    pub enqueue_memory: bool,
    /// Whether to also ingest the written report into LocalMind's documentation
    /// index (`doc_chunk`). Only acts when a report is written.
    pub ingest_report: bool,
}

/// Build run options from the `[research]` config. Returns `None` when the
/// research surface is disabled (`[research].enabled = false`).
pub fn options_from_config(
    root: &Path,
    write_report: bool,
    enqueue_memory: bool,
) -> anyhow::Result<Option<ResearchOptions>> {
    let config = localpilot_config::load(&ConfigPaths::standard(root), &CliOverrides::default())?;
    if !config.research.enabled {
        return Ok(None);
    }
    let output_dir = config.research.output_dir.clone().map_or_else(
        || root.join(".localpilot").join("research"),
        |dir| root.join(dir),
    );
    Ok(Some(ResearchOptions {
        max_questions: config.research.max_questions.max(1),
        max_rounds: config.research.max_rounds.max(1),
        per_source_evidence: config.research.per_source_evidence.max(1),
        max_total_evidence: config.research.max_total_evidence.max(1),
        time_budget: config.research.time_budget_secs.map(Duration::from_secs),
        output_dir,
        write_report,
        enqueue_memory,
        ingest_report: config.research.ingest_report,
    }))
}

/// Chat-facing notice for entering persistent research mode. The copy states
/// the actual egress posture of the current configuration (ADR-0076: web
/// research is on by default, disclosed on every surface) instead of
/// asserting a fixed state, so the disclosure requirement of
/// `docs/07-security-and-privacy.md` holds whichever way the config points.
/// A config load failure reads as the default posture (web on) — the safe
/// direction for a disclosure is to over-warn, never to claim "web off"
/// while requests go out.
#[cfg(feature = "tui")]
pub fn research_mode_notice(root: &Path) -> String {
    let web_enabled =
        localpilot_config::load(&ConfigPaths::standard(root), &CliOverrides::default())
            .map(|config| config.research.web.enabled)
            .unwrap_or(true);
    if web_enabled {
        "research mode: type a topic to research (local sources + web, disclosed and audited; \
         [research.web] or --no-web disables). /agent to exit."
            .to_string()
    } else {
        "research mode: type a topic to research (local sources only; web disabled by \
         [research.web].enabled = false). /agent to exit."
            .to_string()
    }
}

/// Run a research pass for `topic` from the interactive surface. Web research
/// follows the same config defaults as the subcommand (on unless
/// `[research.web].enabled = false`), with the egress disclosure written into
/// the transcript output. `stop`, when flipped true (Ctrl+C), ends the run at
/// the next question boundary with a partial report instead of nothing.
#[cfg(feature = "tui")]
pub async fn run_interactive_research(
    root: &Path,
    topic: &str,
    options: &ResearchOptions,
    stop: Arc<std::sync::atomic::AtomicBool>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    run_research_command_controlled(root, topic, options, None, Some(stop), out).await
}

/// Run a research pass for `topic`, gathering across local sources and the
/// disclosed, allowlist-gated web source. Web research is **on by default**;
/// `web_override` carries a per-run override: `Some(false)` (`--no-web`) skips
/// the web source entirely — zero egress, the URL-proposal model call
/// included — while `Some(true)` (`--web`, kept for compatibility) behaves
/// like the default. Synthesises with the model-assisted (decomposition)
/// synthesizer, then (per options) writes a report artefact and enqueues
/// review-gated memory candidates. A short human summary is written to `out`.
///
/// When the web source is built, the egress disclosure is printed and
/// per-session consent recorded before any request; the source stays inert
/// when config disables web, so no flag can override
/// `[research.web].enabled = false`.
pub async fn run_research_command(
    root: &Path,
    topic: &str,
    options: &ResearchOptions,
    web_override: Option<bool>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    run_research_command_controlled(root, topic, options, web_override, None, out).await
}

/// [`run_research_command`] with an optional external stop flag (Ctrl+C →
/// partial report at the next question boundary).
pub async fn run_research_command_controlled(
    root: &Path,
    topic: &str,
    options: &ResearchOptions,
    web_override: Option<bool>,
    stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let config = localpilot_config::load(&ConfigPaths::standard(root), &CliOverrides::default())?;
    let model = ModelHandle::from_config(&config);

    // One shared relevance-admission judge for local and web evidence, so
    // both are judged against the sub-question on the same basis
    // (LocalHub#32): reuse-only model resolution, deterministic term-overlap
    // fallback when no model resolves.
    let admission = AdmissionJudge::resolve(root, model.as_ref()).map(Arc::new);
    if admission.is_none() {
        writeln!(
            out,
            "note: no model available for relevance admission — deterministic \
             term-overlap scoring with the admission floor applies"
        )?;
    }
    let web = web_override.unwrap_or(true);
    let mut sources = build_local_sources(root, topic, admission.clone());
    if web {
        let web_source =
            build_web_source(root, topic, &config, model.clone(), admission, out).await?;
        sources.push(Box::new(web_source));
    } else {
        writeln!(out, "web research: skipped for this run (--no-web)")?;
    }
    let synth = CliSynthesizer { model };

    let bounds = Bounds {
        max_questions: options.max_questions,
        per_source_evidence: options.per_source_evidence,
        max_rounds: options.max_rounds,
        max_total_evidence: options.max_total_evidence,
        time_budget: options.time_budget,
    };
    // Round summaries stream through a channel so long runs show progress as
    // it happens, not only at the end.
    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::unbounded_channel::<localpilot_research::RoundSummary>();
    let control = RunControl {
        stop,
        progress: Some(Arc::new(
            move |summary: &localpilot_research::RoundSummary| {
                let _ = progress_tx.send(summary.clone());
            },
        )),
    };
    let run = run_research_controlled(topic, &sources, &synth, bounds, control);
    tokio::pin!(run);
    let mut outcome = loop {
        tokio::select! {
            result = &mut run => break result?,
            Some(summary) = progress_rx.recv() => write_round_line(out, &summary)?,
        }
    };
    // The run holds no sender anymore; drain whatever raced the completion.
    while let Ok(summary) = progress_rx.try_recv() {
        write_round_line(out, &summary)?;
    }
    // Stamp the run's web posture on the report, so the renderer can mark a
    // question web contributed nothing to as an explicit source gap.
    outcome.report.web_enabled = Some(web);

    for note in &outcome.report.retrieval_notes {
        writeln!(out, "note: {note}")?;
    }
    for error in &outcome.source_errors {
        writeln!(out, "note: {error}")?;
    }
    // `WebSource::gather` returns `Ok(vec![])` — not an `Err` — both when web
    // access isn't active and when no chat model is configured (there's no
    // real search API; a model proposes candidate URLs). Neither shows up in
    // `source_errors`, so without this check a default-on web run can silently
    // contribute nothing and look identical to "queried but found nothing."
    if web && !any_web_evidence(&outcome.report) {
        writeln!(
            out,
            "note: web research produced no evidence (check [research.web].enabled \
             and that a chat model is configured)"
        )?;
    }
    if options.write_report {
        let path = write_report(&options.output_dir, topic, &outcome.report)?;
        writeln!(out, "report: {}", path.display())?;
        if options.ingest_report {
            // Best-effort: a failure to index must never fail the research run.
            match localpilot_localmind::ingest_research_docs(root, &options.output_dir) {
                Ok(summary) => writeln!(
                    out,
                    "doc chunks ingested: {} (total in index: {})",
                    summary.chunks, summary.total_in_index
                )?,
                Err(error) => writeln!(out, "note: research report not ingested: {error}")?,
            }
        }
    }
    if options.enqueue_memory {
        let enqueued = enqueue_candidates(root, &outcome.report)?;
        writeln!(out, "memory candidates enqueued for review: {enqueued}")?;
    }
    let covered = count_verdict(&outcome.report, CoverageVerdict::Covered);
    let single_source = count_verdict(&outcome.report, CoverageVerdict::CoveredSingleSource);
    let weak = count_verdict(&outcome.report, CoverageVerdict::Weak);
    writeln!(
        out,
        "findings: {}  coverage: {covered} covered, {single_source} single-source, \
         {weak} weak, {} open  rounds: {}",
        outcome.report.findings.len(),
        outcome.report.open_questions.len(),
        outcome.report.rounds_run
    )?;
    Ok(())
}

fn count_verdict(report: &ResearchReport, verdict: CoverageVerdict) -> usize {
    report
        .coverage
        .iter()
        .filter(|coverage| coverage.verdict == verdict)
        .count()
}

fn write_round_line(
    out: &mut dyn Write,
    round: &localpilot_research::RoundSummary,
) -> anyhow::Result<()> {
    writeln!(
        out,
        "round {}: targeted {} question(s), {} new evidence ({} total) — \
         {} covered, {} weak, {} open",
        round.round,
        round.targeted,
        round.new_evidence,
        round.total_evidence,
        round.covered,
        round.weak,
        round.open
    )?;
    Ok(())
}

/// Whether any finding in `report` is backed by the `web` source. Every
/// evidence-derived finding keeps its originating source's provenance (the
/// heuristic synthesizer never drops it), so this is a reliable way to tell
/// "web was queried and found nothing" apart from "web was never actually
/// consulted."
fn any_web_evidence(report: &ResearchReport) -> bool {
    report
        .findings
        .iter()
        .any(|finding| finding.supporting.iter().any(|p| p.source == "web"))
}

/// Cap on the full bounded chunk text carried as review-only evidence for one
/// local hit — mirrors the web fetch bound, and sits below the renderer's
/// safety net so local evidence normally rides intact with any cut disclosed
/// here, loudly.
const LOCAL_EVIDENCE_MAX_CHARS: usize = 64 * 1024;

/// Assemble the local source set: ingested knowledge + accepted memory. The
/// shared admission judge (when a model resolves) gives local hits the same
/// question-level relevance admission as fetched web pages (LocalHub#32).
fn build_local_sources(
    root: &Path,
    topic: &str,
    admission: Option<Arc<AdmissionJudge>>,
) -> SourceSet {
    SourceSet::new()
        .with(Box::new(KnowledgeSource {
            root: root.to_path_buf(),
            topic: topic.to_string(),
            admission,
        }))
        .with(Box::new(MemorySource {
            root: root.to_path_buf(),
        }))
}

struct KnowledgeSource {
    root: PathBuf,
    /// The original research topic, so local hits are judged against the
    /// topic's constraints, not only the sub-question (LocalHub#36).
    topic: String,
    /// Model-backed question-level admission, shared with the web source.
    /// `None` degrades to deterministic term-overlap scoring — never to the
    /// within-source rank, which orders hits but must not admit them.
    admission: Option<Arc<AdmissionJudge>>,
}

#[async_trait]
impl Source for KnowledgeSource {
    fn label(&self) -> &str {
        "knowledge"
    }
    async fn gather(&self, question: &str, limit: usize) -> Result<Gathered, SourceError> {
        let hits = localpilot_localmind::knowledge_search(&self.root, question)
            .map_err(|error| SourceError::new("knowledge", error.to_string()))?;
        let candidates: Vec<localpilot_localmind::KnowledgeHit> =
            hits.into_iter().take(limit).collect();
        let mut account = SourceAccount::new("knowledge");
        account.proposed = candidates.len();
        // Within-source rank relative to this query's best hit: preserved for
        // ordering and diagnostics only. It must never be the admission value
        // — a corpus's least-bad hit always ranks 1.0, which says nothing
        // about whether it answers the question (LocalHub#32).
        let max_raw = candidates
            .iter()
            .map(|hit| hit.relevance)
            .fold(0.0_f32, f32::max);
        // One read-only fetch for all candidate chunk bodies: the full
        // bounded chunk both grounds the admission judgment and becomes the
        // review-only "full source evidence" — the search snippet alone is a
        // match window, not full source (LocalHub#34).
        let ids: Vec<String> = candidates.iter().map(|hit| hit.chunk_id.clone()).collect();
        let bodies: std::collections::HashMap<String, localpilot_localmind::FetchedBody> =
            localpilot_localmind::fetch_layer(&self.root, &ids)
                .unwrap_or_default()
                .into_iter()
                .map(|body| (body.id.clone(), body))
                .collect();
        let mut evidence = Vec::new();
        for hit in candidates {
            let rank = if max_raw > 0.0 {
                (hit.relevance / max_raw).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let body = bodies.get(&hit.chunk_id);
            let content = body.map_or(hit.snippet.as_str(), |body| body.body.as_str());
            // Question-level admission: the model judge when available (same
            // strict-JSON contract as web pages), else deterministic
            // significant-term coverage against the sub-question. Either path
            // may admit zero local hits — an honest outcome.
            let judged = match &self.admission {
                Some(judge) => judge.classify(&self.topic, question, content).await,
                None => None,
            };
            let (relevance, reason) = match judged {
                Some(verdict) => {
                    if !verdict.relevant || verdict.score < ADMISSION_MIN_SCORE {
                        account.rejected_relevance += 1;
                        continue;
                    }
                    (verdict.score, admission_reason(&verdict.reason))
                }
                // Topic-scoped so the deterministic fallback is topic-aware: a
                // chunk missing the topic's load-bearing terms floors low even
                // when it matches the generic sub-question (LocalHub#36).
                None => {
                    let scoped = scope_to_topic(&self.topic, question);
                    (
                        term_overlap_relevance(&scoped, content),
                        "term overlap".to_string(),
                    )
                }
            };
            account.admitted += 1;
            evidence.push(
                map_knowledge_hit(question, &hit, relevance)
                    .with_admission(AdmissionTrail {
                        raw: hit.relevance,
                        rank,
                        reason,
                    })
                    .with_full_source(full_chunk_evidence(&hit, body)),
            );
        }
        Ok(Gathered { evidence, account })
    }
}

/// Map one knowledge hit to evidence with its question-level admission
/// relevance. Provenance keeps the human-readable `path:start-end` locator
/// plus the machine-fetchable chunk id.
fn map_knowledge_hit(
    question: &str,
    hit: &localpilot_localmind::KnowledgeHit,
    relevance: f32,
) -> Evidence {
    Evidence::new(
        question,
        hit.snippet.clone(),
        Provenance::new(
            "knowledge",
            Some(format!("{}:{}-{}", hit.path, hit.start_line, hit.end_line)),
        )
        .with_fetch_id(hit.chunk_id.clone()),
        relevance,
    )
}

/// The full bounded chunk text behind a local hit, as review-only evidence:
/// the fetched chunk body when available (stale ingest state disclosed, any
/// cut at the explicit bound disclosed), or an explicit unavailable marker —
/// a search snippet must never silently pose as full source (LocalHub#34).
fn full_chunk_evidence(
    hit: &localpilot_localmind::KnowledgeHit,
    body: Option<&localpilot_localmind::FetchedBody>,
) -> String {
    let Some(body) = body else {
        return format!(
            "[full source unavailable: chunk {} was not found in the knowledge index — \
             only the search snippet is shown]\n\n{}",
            hit.chunk_id, hit.snippet
        );
    };
    let mut text = String::new();
    if hit.stale {
        text.push_str(
            "[stale: the source file changed since ingest — the content below is the \
             ingested version and the line range may no longer match]\n\n",
        );
    }
    let total = body.body.chars().count();
    if total > LOCAL_EVIDENCE_MAX_CHARS {
        text.extend(body.body.chars().take(LOCAL_EVIDENCE_MAX_CHARS));
        text.push_str(&format!(
            "\n… (chunk truncated: first {LOCAL_EVIDENCE_MAX_CHARS} of {total} characters shown)"
        ));
    } else {
        text.push_str(&body.body);
    }
    text
}

struct MemorySource {
    root: PathBuf,
}

#[async_trait]
impl Source for MemorySource {
    fn label(&self) -> &str {
        "memory"
    }
    async fn gather(&self, question: &str, limit: usize) -> Result<Gathered, SourceError> {
        let hits = localpilot_localmind::search_readonly(&self.root, question)
            .map_err(|error| SourceError::new("memory", error.to_string()))?;
        Ok(Gathered::from_evidence(
            "memory",
            hits.into_iter()
                .take(limit)
                .map(|hit| map_memory_hit(question, &hit))
                .collect(),
        ))
    }
}

fn map_memory_hit(question: &str, hit: &localpilot_localmind::SearchHit) -> Evidence {
    Evidence::new(
        question,
        hit.snippet.clone(),
        Provenance::new("memory", Some(hit.memory_id.clone())),
        MEMORY_EVIDENCE_RELEVANCE,
    )
    .with_admission(AdmissionTrail {
        raw: MEMORY_EVIDENCE_RELEVANCE,
        rank: 1.0,
        reason: "reviewed memory".to_string(),
    })
}

// --- web source (off by default; `policies/remote-egress.md`) ----------------

/// Bound on one designated search-tool call, so a hung MCP server can never
/// hang the research run (the stdio transport itself has no call timeout).
const MCP_SEARCH_TIMEOUT_SECS: u64 = 20;

/// Politeness floor between two fetches to the same host within one run.
const POLITENESS_MIN_DELAY_MS: u64 = 250;
/// Politeness ceiling: even a slow host is not padded beyond this.
const POLITENESS_MAX_DELAY_MS: u64 = 3_000;

/// Total request timeout for a web fetch, mirroring the `fetch` builtin's bound.
const WEB_FETCH_TIMEOUT_SECS: u64 = 30;
/// Connect-phase timeout, so a stalled connect fails fast under the total.
const WEB_CONNECT_TIMEOUT_SECS: u64 = 10;
/// Cap on body bytes kept as evidence from one fetch, bounding context cost.
const WEB_MAX_BODY_BYTES: usize = 64 * 1024;

/// Default egress audit-log path when `[research.web].audit_log` is unset.
fn default_audit_log(root: &Path) -> PathBuf {
    root.join(".localpilot")
        .join("research")
        .join("egress-audit.log")
}

/// Print the loud egress disclosure, record the operator's per-session opt-in,
/// and build the networked source.
///
/// The source is **inert** (every host resolves to [`FetchDecision::Disabled`])
/// when `[research.web].enabled` is false, because [`WebAccess::grant_session`]
/// is a no-op against config-off — so no flag can override the config kill
/// switch. With an explicitly empty allowlist every host needs confirmation,
/// which in v1 means skipped, so the disclosure warns that nothing will be
/// fetched.
async fn build_web_source(
    root: &Path,
    topic: &str,
    config: &Config,
    model: Option<ModelHandle>,
    admission: Option<Arc<AdmissionJudge>>,
    out: &mut dyn Write,
) -> anyhow::Result<WebSource> {
    let web_config = &config.research.web;
    let audit_log = web_config
        .audit_log
        .clone()
        .map_or_else(|| default_audit_log(root), |path| root.join(path));
    let mut access = WebAccess::new(
        web_config.enabled,
        web_config.allowlist.clone(),
        web_config.disallowlist.clone(),
    );

    writeln!(out, "web research (egress disclosure):")?;
    writeln!(
        out,
        "  web research is on by default — disable with --no-web for one run \
         or [research.web].enabled = false in config"
    )?;
    writeln!(
        out,
        "  sent off-machine: only the redacted sub-question text \
         — never file contents or gathered evidence"
    )?;
    if web_config.enabled {
        if web_config.allowlist.is_empty() {
            writeln!(
                out,
                "  allowlist: explicitly empty — every host requires confirmation, \
                 so nothing will be fetched this run"
            )?;
        } else if web_config.allowlist.iter().any(|entry| entry.trim() == "*") {
            writeln!(
                out,
                "  reach: open web — restrict with [research.web].allowlist, \
                 block hosts with [research.web].disallowlist"
            )?;
        } else {
            writeln!(
                out,
                "  allowlisted domains: {}",
                web_config.allowlist.join(", ")
            )?;
            writeln!(out, "  non-allowlisted hosts are skipped and logged")?;
        }
        if !web_config.disallowlist.is_empty() {
            writeln!(
                out,
                "  blocked domains (disallowlist wins over allowlist): {}",
                web_config.disallowlist.join(", ")
            )?;
        }
        writeln!(out, "  audit log: {}", audit_log.display())?;
    } else {
        writeln!(
            out,
            "  [research.web].enabled is false — web research stays disabled; \
             no request will be made"
        )?;
    }

    let search = if web_config.enabled {
        connect_search_tools(config, out).await?
    } else {
        None
    };

    access.grant_session();
    let renderer = build_renderer(config.research.render.mode);
    if renderer.is_some() {
        writeln!(
            out,
            "  render fallback: pages needing JavaScript are rendered by a headless \
             system browser, gated by the same allowlist and audited"
        )?;
    }
    Ok(WebSource::new(
        topic,
        config.research.render.mode,
        access,
        audit_log,
        model,
        search,
        admission,
    )?
    .with_renderer(renderer))
}

/// Construct the browser-rendering fallback when the `render-browser` feature is
/// built, a system browser is present, and the mode is not `off`. `None`
/// otherwise, so the run degrades to static extraction + iframe recovery and
/// records an explicit render-required outcome for a page that needed rendering.
#[cfg(feature = "render-browser")]
fn build_renderer(mode: localpilot_config::RenderMode) -> Option<Arc<dyn Renderer>> {
    if matches!(mode, localpilot_config::RenderMode::Off) {
        return None;
    }
    localpilot_render::ChromiumRenderer::available()
        .then(|| Arc::new(localpilot_render::ChromiumRenderer::new()) as Arc<dyn Renderer>)
}

/// Without the `render-browser` feature there is no browser renderer; the
/// always-on detection + iframe recovery still apply.
#[cfg(not(feature = "render-browser"))]
fn build_renderer(_mode: localpilot_config::RenderMode) -> Option<Arc<dyn Renderer>> {
    None
}

/// Connect the `[research.mcp]`-designated search tools and disclose them.
/// Best-effort: a server that fails to spawn or handshake is reported and
/// skipped; the run continues with whatever connected. `None` when nothing is
/// designated or nothing connected.
async fn connect_search_tools(
    config: &Config,
    out: &mut dyn Write,
) -> anyhow::Result<Option<McpSearchProposer>> {
    let designated = &config.research.mcp.tools;
    if designated.is_empty() {
        return Ok(None);
    }
    let mut tools = Vec::new();
    for pair in designated {
        let label = format!("{}.{}", pair.server, pair.tool);
        let Some(server) = config.mcp.servers.get(&pair.server) else {
            writeln!(
                out,
                "  search tool {label}: server '{}' not found under [mcp.servers] — skipped",
                pair.server
            )?;
            continue;
        };
        let transport = match localpilot_mcp::StdioTransport::spawn(&server.command, &server.args) {
            Ok(transport) => Arc::new(transport) as Arc<dyn localpilot_mcp::Transport>,
            Err(error) => {
                writeln!(out, "  search tool {label}: failed to start — {error}")?;
                continue;
            }
        };
        let client = McpClient::new(Arc::clone(&transport));
        let handshake = tokio::time::timeout(
            Duration::from_secs(MCP_SEARCH_TIMEOUT_SECS),
            client.initialize(),
        )
        .await;
        match handshake {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                writeln!(out, "  search tool {label}: handshake failed — {error}")?;
                continue;
            }
            Err(_) => {
                writeln!(out, "  search tool {label}: handshake timed out")?;
                continue;
            }
        }
        // Advisory only: a server that lazily advertises still gets the call.
        if let Ok(Ok(advertised)) = tokio::time::timeout(
            Duration::from_secs(MCP_SEARCH_TIMEOUT_SECS),
            client.list_tools(),
        )
        .await
        {
            if !advertised.iter().any(|t| t.name == pair.tool) {
                writeln!(
                    out,
                    "  search tool {label}: server does not advertise '{}' — calling anyway",
                    pair.tool
                )?;
            }
        }
        tools.push(DesignatedSearchTool {
            label,
            tool: pair.tool.clone(),
            transport,
        });
    }
    if tools.is_empty() {
        writeln!(
            out,
            "  no designated search tool connected — the model proposes candidate URLs"
        )?;
        return Ok(None);
    }
    writeln!(
        out,
        "  designated search tools: {} — the redacted sub-question text is sent to these MCP servers",
        tools
            .iter()
            .map(|tool| tool.label.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    Ok(Some(McpSearchProposer { tools }))
}

/// One designated MCP search tool, connected and ready to propose URLs.
struct DesignatedSearchTool {
    /// `server.tool`, for disclosure and audit lines.
    label: String,
    /// The exact tool name the server advertises.
    tool: String,
    transport: Arc<dyn localpilot_mcp::Transport>,
}

/// Designated MCP search tools acting as candidate-URL proposers.
///
/// Search results are **leads only**: the extracted URLs feed the same
/// [`WebAccess`]-gated, audited fetch path as model-proposed ones — a search
/// result never becomes evidence directly. Calls are best-effort and bounded:
/// a tool that errors, times out, or rate-limits is skipped (audited as
/// `search-error`) and the run continues.
struct McpSearchProposer {
    tools: Vec<DesignatedSearchTool>,
}

impl McpSearchProposer {
    /// Ask every designated tool for candidate URLs answering the redacted
    /// `query`. Returns the merged, order-preserving deduplicated URLs.
    async fn propose(&self, query: &str, limit: usize, audit_log: &Path) -> Vec<String> {
        let mut urls = Vec::new();
        for tool in &self.tools {
            let client = McpClient::new(Arc::clone(&tool.transport));
            let call = client.call_tool_raw(&tool.tool, serde_json::json!({ "query": query }));
            let result =
                tokio::time::timeout(Duration::from_secs(MCP_SEARCH_TIMEOUT_SECS), call).await;
            // The search call itself is egress (the redacted query goes to the
            // server), so it is audited like a fetch — success and failure both.
            let (decision, proposed) = match result {
                Ok(Ok(value)) => {
                    let proposals = extract_candidate_urls(&value);
                    match proposals.error {
                        None => ("search", proposals.urls),
                        Some(SearchCallError::RateLimited(_)) => {
                            ("search-rate-limited", Vec::new())
                        }
                        Some(SearchCallError::Failed(_)) => ("search-error", Vec::new()),
                    }
                }
                Ok(Err(_)) => ("search-error", Vec::new()),
                Err(_) => ("search-timeout", Vec::new()),
            };
            let _ = append_audit(
                audit_log,
                &audit_entry(
                    &format!("mcp://{}", tool.label),
                    &tool.label,
                    decision,
                    query,
                ),
            );
            urls.extend(proposed);
            if urls.len() >= limit {
                break;
            }
        }
        let mut seen = std::collections::HashSet::new();
        urls.retain(|url| seen.insert(url.clone()));
        urls
    }
}

/// A networked evidence source, constructed only when the operator opts in.
///
/// For each sub-question it gathers candidate URLs — from designated MCP
/// search tools first, then the model's proposals — parses each URL's host
/// with a real parser, and consults the [`WebAccess`] gate: allowlisted hosts
/// are fetched and audited; every other host is skipped and logged (v1 is
/// allowlist-only — no interactive per-fetch confirm). Only the redacted
/// sub-question is ever sent off-machine.
struct WebSource {
    /// The original research topic, kept so admission judges each fetched page
    /// against the topic's load-bearing constraints, not only the sub-question
    /// (LocalHub#36) — and so a sub-question that dropped those constraints is
    /// re-scoped before it becomes a search query.
    topic: String,
    /// When the browser-rendering fallback runs (`auto`/`off`/`always`). Also
    /// governs render-signal detection and same-allowlist iframe recovery: `off`
    /// disables both (pure static extraction), `auto` acts on a detected signal,
    /// `always` treats every fetched page as needing rendering (LocalHub#37).
    render_mode: localpilot_config::RenderMode,
    client: reqwest::Client,
    access: WebAccess,
    audit_log: PathBuf,
    model: Option<ModelHandle>,
    search: Option<McpSearchProposer>,
    politeness: std::sync::Mutex<HostPoliteness>,
    /// Model-backed relevance admission for fetched pages, shared with the
    /// local knowledge source so local and web evidence are judged on the
    /// same question-level basis. `None` degrades to the deterministic
    /// term-overlap score plus the engine's admission floor.
    admission: Option<Arc<AdmissionJudge>>,
    /// The browser-rendering fallback, present only when the `render-browser`
    /// feature is built and a system browser was found. `None` degrades to
    /// static extraction plus iframe recovery, recording `renderer unavailable`
    /// for a page that needed rendering (LocalHub#37).
    renderer: Option<Arc<dyn Renderer>>,
}

/// Bound on page content sent to the admission classifier — enough to judge
/// relevance, far below the full evidence bound.
const ADMISSION_CONTENT_CHARS: usize = 4_000;

/// The model score a page must reach to enter the evidence pool — the same
/// bar the engine applies deterministically, so the model path and the
/// fallback share one threshold.
const ADMISSION_MIN_SCORE: f32 = localpilot_research::COVERAGE_RELEVANCE_FLOOR;

/// The strict-JSON classification an evidence candidate (a fetched page, a
/// local chunk) must clear to enter the evidence pool when a model judge is
/// available. Carries the classifier's short reason so "admitted at 0.85" is
/// auditable and the reviewer can see *why* a cross-framework page was kept or
/// dropped, not only the number (LocalHub#36).
#[derive(Debug, Clone, PartialEq)]
struct AdmissionVerdict {
    relevant: bool,
    score: f32,
    reason: String,
}

/// The model that classifies research content (fetched web pages, local
/// knowledge chunks) against a sub-question. It judges relevance only — it
/// never authors, rewrites, or summarizes the finding. Reuses existing
/// configuration exclusively (no research-specific model setting):
/// LocalMind's `[inference]` chat model when configured with the research
/// feature enabled, else the host's resolved default provider.
enum AdmissionJudge {
    LocalMind(std::sync::Arc<localpilot_localmind::ResearchChat>),
    Host(ModelHandle),
}

impl AdmissionJudge {
    fn resolve(root: &Path, host_model: Option<&ModelHandle>) -> Option<Self> {
        if let Some(chat) = localpilot_localmind::ResearchChat::resolve(root) {
            return Some(Self::LocalMind(std::sync::Arc::new(chat)));
        }
        host_model.cloned().map(Self::Host)
    }

    /// Classify bounded page content against the sub-question **within the
    /// original research topic's constraints**. The sub-question is judged only
    /// as a focus inside the topic: content that answers the sub-question but
    /// violates a load-bearing topic constraint (a different framework,
    /// language, runtime, or platform) is rejected unless the topic itself asks
    /// for a comparison or transferable techniques (LocalHub#36). `None` when
    /// the model is unavailable or its output is not the agreed strict JSON —
    /// the caller keeps the deterministic path rather than guessing.
    async fn classify(
        &self,
        topic: &str,
        question: &str,
        content: &str,
    ) -> Option<AdmissionVerdict> {
        let bounded: String = content.chars().take(ADMISSION_CONTENT_CHARS).collect();
        let instruction = format!(
            "Classify whether the research content below actually helps answer the research \
             sub-question WITHIN the constraints of the original research topic. The topic's \
             load-bearing constraints — framework, library, language, runtime, platform, and \
             version — always apply, even when the sub-question does not repeat them. Reject \
             content that answers the sub-question but is about a different framework, engine, \
             language, or platform than the topic names, UNLESS the topic explicitly asks for a \
             comparison, cross-tool techniques, or transferable examples. Judge relevance only; \
             do not rewrite or summarize the content. Return ONLY this JSON object and nothing \
             else: {{\"relevant\": true|false, \"score\": <0.0-1.0>, \"reason\": \"<short>\"}}\n\n\
             Original topic: {topic}\n\nSub-question: {question}\n\nContent:\n{bounded}"
        );
        let reply = match self {
            Self::LocalMind(chat) => {
                let chat = std::sync::Arc::clone(chat);
                let system = "You classify research evidence as strict JSON. Return only the \
                              JSON object, no prose."
                    .to_string();
                tokio::task::spawn_blocking(move || chat.complete(&system, &instruction))
                    .await
                    .ok()?
                    .ok()?
            }
            Self::Host(model) => model.complete(&instruction).await.ok()?,
        };
        parse_admission(&reply)
    }
}

/// Parse the strict admission JSON out of a model reply. Tolerates prose
/// around exactly one JSON object; anything else is unusable (`None`). The
/// classifier's short `reason` is kept so it can ride the admission trail into
/// the report and review surfaces (LocalHub#36); a missing reason is not fatal.
fn parse_admission(reply: &str) -> Option<AdmissionVerdict> {
    #[derive(serde::Deserialize)]
    struct Raw {
        relevant: bool,
        score: f32,
        #[serde(default)]
        reason: String,
    }
    let start = reply.find('{')?;
    let end = reply.rfind('}')?;
    let raw: Raw = serde_json::from_str(reply.get(start..=end)?).ok()?;
    if !raw.score.is_finite() {
        return None;
    }
    Some(AdmissionVerdict {
        relevant: raw.relevant,
        score: raw.score.clamp(0.0, 1.0),
        reason: flatten_reason(&raw.reason),
    })
}

/// Reduce a classifier reason to a short, single-line, bounded string safe to
/// embed in the report's retrieval accounting (content-free by construction —
/// it is the model's own rationale, never page text).
fn flatten_reason(reason: &str) -> String {
    let flat: String = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(160).collect()
}

/// The admission-trail reason for a model-admitted item: `model admission`,
/// with the classifier's own short rationale appended when it gave one, so the
/// report's accounting shows *why* a page was admitted against the topic, not
/// only that a model did it (LocalHub#36).
fn admission_reason(model_reason: &str) -> String {
    if model_reason.is_empty() {
        "model admission".to_string()
    } else {
        format!("model admission — {model_reason}")
    }
}

/// Combine the original research topic with a sub-question so a sub-question
/// that dropped the topic's load-bearing terms still carries them into search
/// and deterministic relevance scoring (LocalHub#36). When the sub-question
/// already contains every significant topic term (case-insensitive), it is
/// returned unchanged — no redundant duplication. Otherwise the topic is
/// prefixed, so a generic sub-question cannot silently become a generic search
/// and a cross-framework page cannot term-overlap its way past the floor.
fn scope_to_topic(topic: &str, question: &str) -> String {
    let topic = topic.trim();
    let question = question.trim();
    if topic.is_empty() {
        return question.to_string();
    }
    let question_terms: std::collections::HashSet<String> = significant_terms(question).collect();
    let carries_constraints = significant_terms(topic).all(|term| question_terms.contains(&term));
    if carries_constraints {
        question.to_string()
    } else {
        format!("{topic} {question}")
    }
}

/// Lowercased alphanumeric terms of length ≥ 3 that are not generic filler —
/// the load-bearing words of a topic or question (a framework, library,
/// language, or noun), used to decide whether a sub-question still carries its
/// topic's constraints.
fn significant_terms(text: &str) -> impl Iterator<Item = String> + '_ {
    const FILLER: [&str; 20] = [
        "the", "and", "for", "with", "how", "are", "use", "from", "into", "what", "why", "that",
        "this", "does", "can", "should", "when", "which", "using", "your",
    ];
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| token.chars().count() >= 3)
        .map(str::to_ascii_lowercase)
        .filter(|token| !FILLER.contains(&token.as_str()))
}

/// Per-run per-host fetch discipline: serialize-and-pace repeat visits
/// (adaptive delay derived from the host's own response time), and cool a
/// host down for the rest of the run after a rate-limit or server error —
/// 429/5xx are host-level signals, not per-URL ones.
#[derive(Default)]
struct HostPoliteness {
    last: std::collections::HashMap<String, (std::time::Instant, Duration)>,
    cooled: std::collections::HashSet<String>,
}

impl WebSource {
    fn new(
        topic: impl Into<String>,
        render_mode: localpilot_config::RenderMode,
        access: WebAccess,
        audit_log: PathBuf,
        model: Option<ModelHandle>,
        search: Option<McpSearchProposer>,
        admission: Option<Arc<AdmissionJudge>>,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(WEB_FETCH_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(WEB_CONNECT_TIMEOUT_SECS))
            // Do not auto-follow redirects: the host allowlist is checked once per
            // proposed URL, so an allowlisted host that 302s to a non-allowlisted
            // (or internal) host would otherwise be fetched un-audited. A 3xx is
            // surfaced to `fetch`, which audits it and does not follow.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            topic: topic.into(),
            render_mode,
            client,
            access,
            audit_log,
            model,
            search,
            politeness: std::sync::Mutex::new(HostPoliteness::default()),
            admission,
            renderer: None,
        })
    }

    /// Attach a browser-rendering fallback. Builder-style so the 7-arg
    /// constructor and its call sites stay unchanged; `None` (the default) keeps
    /// the static-only behaviour.
    fn with_renderer(mut self, renderer: Option<Arc<dyn Renderer>>) -> Self {
        self.renderer = renderer;
        self
    }

    /// Run the model-backed relevance admission over one fetched page, right
    /// after reduction and bounding and before the evidence enters coverage,
    /// synthesis, or the memory-candidate path. With a usable verdict the
    /// model's score becomes the evidence relevance (admitted) or the page is
    /// rejected with an audit record (inspectable, never a silent drop). With
    /// no judge, or an unusable reply, the deterministic term-overlap score
    /// stands and the engine's admission floor governs.
    async fn admit(
        &self,
        url: &str,
        host: &str,
        query: &str,
        mut found: Evidence,
    ) -> Result<Option<Evidence>, SourceError> {
        let Some(judge) = &self.admission else {
            return Ok(Some(found));
        };
        let Some(verdict) = judge
            .classify(&self.topic, &found.question, &found.snippet)
            .await
        else {
            return Ok(Some(found));
        };
        if !verdict.relevant || verdict.score < ADMISSION_MIN_SCORE {
            append_audit(
                &self.audit_log,
                &audit_entry(url, host, "rejected-low-relevance", query),
            )?;
            return Ok(None);
        }
        found.relevance = verdict.score;
        if let Some(trail) = &mut found.admission {
            trail.reason = admission_reason(&verdict.reason);
        }
        Ok(Some(found))
    }

    /// Whether `host` is cooled down for the rest of this run.
    fn host_cooled(&self, host: &str) -> bool {
        self.politeness
            .lock()
            .map(|p| p.cooled.contains(host))
            .unwrap_or(false)
    }

    /// The politeness pause owed before fetching `host` again, if any.
    fn pause_before(&self, host: &str) -> Option<Duration> {
        let politeness = self.politeness.lock().ok()?;
        let (at, took) = politeness.last.get(host)?;
        let delay = (*took)
            .max(Duration::from_millis(POLITENESS_MIN_DELAY_MS))
            .min(Duration::from_millis(POLITENESS_MAX_DELAY_MS));
        delay.checked_sub(at.elapsed())
    }

    fn record_fetch(&self, host: &str, took: Duration, cool_down: bool) {
        if let Ok(mut politeness) = self.politeness.lock() {
            politeness
                .last
                .insert(host.to_string(), (std::time::Instant::now(), took));
            if cool_down {
                politeness.cooled.insert(host.to_string());
            }
        }
    }

    /// Ask the model for candidate URLs answering the redacted `query`.
    async fn propose_urls(
        &self,
        model: &ModelHandle,
        query: &str,
        limit: usize,
    ) -> Result<Vec<String>, SourceError> {
        let prompt = propose_urls_prompt(query, limit);
        let text = model
            .complete(&prompt)
            .await
            .map_err(|error| SourceError::new("web", format!("url proposal failed: {error}")))?;
        Ok(extract_urls(&text))
    }

    /// Fetch one allowlisted `url`, auditing the outbound request first.
    /// Every non-evidence outcome is countable (cooldown, redirect, failure)
    /// rather than a silent `None` or a source-aborting error, so the
    /// per-question account can say *why* web produced nothing — and one bad
    /// URL never discards the evidence already gathered (LocalHub#33).
    async fn fetch(
        &self,
        url: &str,
        host: &str,
        question: &str,
        query: &str,
    ) -> Result<FetchOutcome, SourceError> {
        // A host that rate-limited or errored earlier in the run stays cooled
        // down — 429/5xx are host-level signals, not per-URL ones.
        if self.host_cooled(host) {
            append_audit(
                &self.audit_log,
                &audit_entry(url, host, "host-cooldown", query),
            )?;
            return Ok(FetchOutcome::Cooled);
        }
        // Pace repeat visits: the delay adapts to the host's own last
        // response time, clamped to a sane window.
        if let Some(pause) = self.pause_before(host) {
            tokio::time::sleep(pause).await;
        }
        append_audit(&self.audit_log, &audit_entry(url, host, "allowed", query))?;
        let fetch_started = std::time::Instant::now();
        let response = match self.client.get(url).send().await {
            Ok(response) => response,
            Err(_) => {
                append_audit(
                    &self.audit_log,
                    &audit_entry(url, host, "fetch-error", query),
                )?;
                return Ok(FetchOutcome::Failed);
            }
        };
        let status = response.status();
        let cool_down = status.as_u16() == 429 || status.is_server_error();
        self.record_fetch(host, fetch_started.elapsed(), cool_down);
        if cool_down {
            return Ok(FetchOutcome::Failed);
        }
        // A redirect is never followed (the target host is unvetted); audit and
        // skip it so it can't become an un-allowlisted egress channel.
        if status.is_redirection() {
            append_audit(
                &self.audit_log,
                &audit_entry(url, host, "redirect-not-followed", query),
            )?;
            return Ok(FetchOutcome::Redirected);
        }
        // Capture the content type before `text()` consumes the response, so a
        // fetched HTML page can be reduced to readable prose below.
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let body = match response.text().await {
            Ok(body) => body,
            Err(_) => {
                append_audit(
                    &self.audit_log,
                    &audit_entry(url, host, "fetch-error", query),
                )?;
                return Ok(FetchOutcome::Failed);
            }
        };
        if !status.is_success() {
            return Ok(FetchOutcome::Failed);
        }
        // An HTML document becomes evidence as readable Markdown, not raw
        // markup: otherwise script/style bodies and tags leak into the finding
        // and its evidence block as junk, and the length budget is spent on
        // chrome rather than content. Markdown (rather than flat text) keeps
        // the page's headings, links, lists, and code blocks readable for the
        // reviewer and the model alike. Non-HTML bodies (plain text, Markdown,
        // JSON) are kept verbatim.
        //
        // While the raw HTML is still in hand, detect whether the page's real
        // content is likely missing from the initial HTML (a client-rendered
        // shell, hydration-only markup, or an iframe-only body) so the caller
        // can recover an allowlisted frame or record an explicit render-required
        // outcome instead of admitting a shell as complete (LocalHub#37).
        let (text, lead) = if is_html(&content_type, &body) {
            let reduced = html_to_markdown(&body);
            let lead = self.render_lead(&body, &reduced);
            (reduced, lead)
        } else {
            (body, None)
        };
        let snippet = bound_body(&text, WEB_MAX_BODY_BYTES);
        // Scored against the kept content, not a flat constant: a page that
        // barely mentions the question's terms reads as weak evidence and
        // stays below the coverage floor (the term-coverage rule applied to
        // fetched pages). Scored against the *topic-scoped* question so the
        // deterministic fallback is topic-aware: a page missing the topic's
        // load-bearing terms (a different framework) floors low even when it
        // matches the generic sub-question (LocalHub#36). `admit` upgrades the
        // trail when the model judges.
        let scoped = scope_to_topic(&self.topic, question);
        let relevance = term_overlap_relevance(&scoped, &snippet);
        let evidence = Evidence::new(
            question,
            snippet,
            Provenance::new("web", Some(url.to_string())),
            relevance,
        )
        .with_admission(AdmissionTrail {
            raw: relevance,
            rank: 1.0,
            reason: "term overlap".to_string(),
        });
        Ok(FetchOutcome::Fetched(Box::new(FetchedPage {
            evidence,
            lead,
        })))
    }

    /// Whether a fetched HTML page needs rendering, per the operator's render
    /// mode: `off` never signals (pure static extraction), `auto` reports a
    /// detected [`RenderSignal`], `always` treats every page as needing
    /// rendering (defaulting the reason to thin content). A signal carries the
    /// page's iframe leads so the caller can recover an allowlisted frame.
    fn render_lead(&self, html: &str, reduced: &str) -> Option<RenderLead> {
        let signal = match self.render_mode {
            localpilot_config::RenderMode::Off => return None,
            localpilot_config::RenderMode::Auto => render_signal(html, reduced)?,
            localpilot_config::RenderMode::Always => {
                render_signal(html, reduced).unwrap_or(RenderSignal::ThinContent)
            }
        };
        Some(RenderLead {
            signal,
            iframe_srcs: iframe_sources(html),
        })
    }

    /// Act on a fetched page's render lead: recover its real content from
    /// allowlisted iframes through the ordinary gated fetch path, and if nothing
    /// was recovered, record an explicit render-required outcome (the renderer
    /// is unavailable in the static build) so a page that needed rendering is
    /// inspectable, never silently counted as complete (LocalHub#37).
    async fn handle_render_lead(
        &self,
        ctx: &FrameContext<'_>,
        lead: &RenderLead,
        account: &mut SourceAccount,
        evidence: &mut Vec<Evidence>,
    ) -> Result<(), SourceError> {
        // 1. The browser renderer, when available: the direct fix for a page
        // whose content only appears after JavaScript. A successful render
        // captures the main document and we are done; an empty or failed render
        // falls through to iframe recovery (the rendered main DOM never carries
        // cross-origin frame content).
        if let Some(renderer) = self.renderer.clone() {
            match self
                .render_page(renderer.as_ref(), ctx, lead, account, evidence)
                .await?
            {
                RenderAttempt::Produced => return Ok(()),
                RenderAttempt::Empty | RenderAttempt::Failed => {}
            }
        }

        // 2. Recover allowlisted iframes through the ordinary gated path (works
        // with no browser); if nothing is recovered, record an explicit
        // render-required outcome so the gap is inspectable (LocalHub#37).
        let recovered = self
            .recover_frames(ctx, lead, account, evidence, false)
            .await?;
        if recovered > 0 {
            account.render_notes.push(format!(
                "{} — {}: recovered {recovered} allowlisted frame(s)",
                ctx.parent_url,
                lead.signal.reason()
            ));
        } else {
            account.render_required += 1;
            let cause = if self.renderer.is_some() {
                "no rendered or frame content recovered"
            } else {
                "renderer unavailable, no allowlisted frame recovered"
            };
            account.render_notes.push(format!(
                "{} — {}: {cause}",
                ctx.parent_url,
                lead.signal.reason()
            ));
            append_audit(
                &self.audit_log,
                &audit_entry(
                    ctx.parent_url,
                    ctx.parent_host,
                    "render-required",
                    ctx.query,
                ),
            )?;
        }
        Ok(())
    }

    /// Render the page through the browser fallback and admit its
    /// post-JavaScript main document. Every browser request is gated + audited
    /// through [`WebAccessGate`]. Returns whether substantive content was
    /// produced (`Produced`), the page genuinely rendered empty (`Empty`), or
    /// the render failed (`Failed`) — the latter two fall through to iframe
    /// recovery.
    async fn render_page(
        &self,
        renderer: &dyn Renderer,
        ctx: &FrameContext<'_>,
        lead: &RenderLead,
        account: &mut SourceAccount,
        evidence: &mut Vec<Evidence>,
    ) -> Result<RenderAttempt, SourceError> {
        let gate = WebAccessGate {
            access: self.access.clone(),
            audit_log: self.audit_log.clone(),
            query: ctx.query.to_string(),
        };
        let request = RenderRequest {
            url: ctx.parent_url.to_string(),
            bounds: RenderBounds::default(),
        };
        let doc = match renderer.render(&request, &gate).await {
            Ok(doc) => doc,
            Err(failure) => {
                account.render_notes.push(format!(
                    "{} — {}: {}",
                    ctx.parent_url,
                    lead.signal.reason(),
                    failure.outcome().label()
                ));
                return Ok(RenderAttempt::Failed);
            }
        };

        // The rendered main document.
        let main_produced = self
            .admit_rendered_html(
                &doc.html,
                ctx.parent_url,
                ctx,
                "rendered",
                account,
                evidence,
            )
            .await?;

        // Each accessible rendered frame (same-origin / `srcdoc`), with the
        // frame's own resolved URL — or a parent+srcdoc locator — as provenance.
        // Near-duplicate parent/frame content is folded by the engine, keeping
        // both origins.
        let base = reqwest::Url::parse(ctx.parent_url).ok();
        let mut frames_produced = 0;
        for frame in &doc.frames {
            let locator = match &frame.url {
                Some(src) => base
                    .as_ref()
                    .and_then(|b| b.join(src).ok())
                    .map_or_else(|| src.clone(), |resolved| resolved.to_string()),
                None => format!("{} (srcdoc frame)", ctx.parent_url),
            };
            if self
                .admit_rendered_html(
                    &frame.html,
                    &locator,
                    ctx,
                    "rendered frame",
                    account,
                    evidence,
                )
                .await?
            {
                frames_produced += 1;
            }
        }

        // Cross-origin frames the browser could not read from the main DOM are
        // recovered as documents through the gated HTTP path (same-origin ones
        // are already covered by the render above).
        let http_frames = self
            .recover_frames(ctx, lead, account, evidence, true)
            .await?;

        let total_frames = frames_produced + http_frames;
        if !main_produced && total_frames == 0 {
            account.render_notes.push(format!(
                "{} — {}: no substantive rendered content ({} subresource(s) blocked)",
                ctx.parent_url,
                lead.signal.reason(),
                doc.blocked
            ));
            return Ok(RenderAttempt::Empty);
        }
        account.render_notes.push(format!(
            "{} — rendered main document + {total_frames} frame(s) ({} subresource(s) blocked)",
            ctx.parent_url, doc.blocked
        ));
        Ok(RenderAttempt::Produced)
    }

    /// Reduce a rendered HTML document, score it against the topic-scoped
    /// question, and admit it under `locator`. Returns whether it produced
    /// substantive content (below [`MIN_RENDERED_CHARS`] it is treated as
    /// empty). Shared by the rendered main document and each rendered frame.
    async fn admit_rendered_html(
        &self,
        html: &str,
        locator: &str,
        ctx: &FrameContext<'_>,
        reason: &str,
        account: &mut SourceAccount,
        evidence: &mut Vec<Evidence>,
    ) -> Result<bool, SourceError> {
        let text = html_to_markdown(html);
        let snippet = bound_body(&text, WEB_MAX_BODY_BYTES);
        if snippet.trim().chars().count() < MIN_RENDERED_CHARS {
            return Ok(false);
        }
        let scoped = scope_to_topic(&self.topic, ctx.question);
        let relevance = term_overlap_relevance(&scoped, &snippet);
        let rendered = Evidence::new(
            ctx.question,
            snippet,
            Provenance::new("web", Some(locator.to_string())),
            relevance,
        )
        .with_admission(AdmissionTrail {
            raw: relevance,
            rank: 1.0,
            reason: reason.to_string(),
        });
        // Rendered frames are same-origin/srcdoc, so the parent host governs the
        // admission audit.
        if let Some(admitted) = self
            .admit(locator, ctx.parent_host, ctx.query, rendered)
            .await?
        {
            account.admitted += 1;
            evidence.push(admitted);
        } else {
            account.rejected_relevance += 1;
        }
        Ok(true)
    }

    /// Fetch a page's allowlisted iframe sources through the same gated path as
    /// any other web fetch, admitting recovered frame content as evidence.
    /// Resolves relative/protocol-relative srcs against the parent, enforces
    /// http/https and the allowlist per frame, and does not recurse into a
    /// frame's own iframes (nested/dynamic frames are the renderer's job).
    /// Returns how many frames contributed admitted evidence.
    async fn recover_frames(
        &self,
        ctx: &FrameContext<'_>,
        lead: &RenderLead,
        account: &mut SourceAccount,
        evidence: &mut Vec<Evidence>,
        cross_origin_only: bool,
    ) -> Result<usize, SourceError> {
        const FRAME_RECOVERY_LIMIT: usize = 3;
        let base = reqwest::Url::parse(ctx.parent_url).ok();
        let mut recovered = 0;
        for src in lead.iframe_srcs.iter().take(FRAME_RECOVERY_LIMIT) {
            let Some(resolved) = base.as_ref().and_then(|b| b.join(src).ok()) else {
                continue;
            };
            if !matches!(resolved.scheme(), "http" | "https") {
                continue;
            }
            let Some(frame_host) = resolved.host_str().map(str::to_string) else {
                continue;
            };
            // After a successful browser render, same-origin frames were already
            // extracted from the rendered DOM; only cross-origin frames still
            // need the gated HTTP path.
            if cross_origin_only && frame_host == ctx.parent_host {
                continue;
            }
            let frame_url = resolved.to_string();
            account.proposed += 1; // a frame is a fresh lead considered
            match self.access.decide_host(&frame_host) {
                FetchDecision::Allowed => {
                    match self
                        .fetch(&frame_url, &frame_host, ctx.question, ctx.query)
                        .await?
                    {
                        FetchOutcome::Fetched(page) => {
                            // Ignore the frame's own render lead here — nesting
                            // is the renderer's concern, not static recovery.
                            let FetchedPage { evidence: fev, .. } = *page;
                            if let Some(admitted) =
                                self.admit(&frame_url, &frame_host, ctx.query, fev).await?
                            {
                                account.admitted += 1;
                                recovered += 1;
                                evidence.push(admitted);
                            } else {
                                account.rejected_relevance += 1;
                            }
                        }
                        FetchOutcome::Cooled => account.policy_skipped += 1,
                        FetchOutcome::Redirected => account.redirected += 1,
                        FetchOutcome::Failed => account.failed += 1,
                    }
                }
                FetchDecision::NeedsConfirmation => {
                    account.policy_skipped += 1;
                    append_audit(
                        &self.audit_log,
                        &audit_entry(&frame_url, &frame_host, "skipped", ctx.query),
                    )?;
                }
                FetchDecision::Disabled => {}
            }
        }
        Ok(recovered)
    }
}

/// A statically-fetched page plus the render lead (if any) the caller acts on.
struct FetchedPage {
    /// The reduced, bounded, term-overlap-scored evidence for the page itself.
    evidence: Evidence,
    /// Set when the page's initial HTML looked client-rendered/iframe-embedded
    /// under the active render mode — the caller recovers an allowlisted frame
    /// or records an explicit render-required outcome.
    lead: Option<RenderLead>,
}

/// Why a fetched page looks like it needs rendering, plus its iframe leads.
struct RenderLead {
    signal: RenderSignal,
    iframe_srcs: Vec<String>,
}

/// The fetch context shared by a page and the frames recovered from it: the
/// parent's URL/host and the redacted query the frames are gathered for.
struct FrameContext<'a> {
    parent_url: &'a str,
    parent_host: &'a str,
    question: &'a str,
    query: &'a str,
}

/// Below this many readable characters, a rendered document has no substantive
/// content — an honest empty, never fabricated evidence.
const MIN_RENDERED_CHARS: usize = 100;

/// The result of a browser-render attempt.
enum RenderAttempt {
    /// The render produced substantive content (admitted or judged off-topic).
    Produced,
    /// The render ran but the page had no substantive content.
    Empty,
    /// The render failed (unavailable, timeout, blocked, browser error).
    Failed,
}

/// A [`RenderGate`] over the research [`WebAccess`]: the browser renderer
/// consults it on every navigation, redirect, subresource, and frame, so
/// rendering obeys the same http/https-only + allowlist boundary as a static
/// fetch and every browser request/skip is audited content-free (LocalHub#37).
struct WebAccessGate {
    access: WebAccess,
    audit_log: PathBuf,
    query: String,
}

impl RenderGate for WebAccessGate {
    fn allow(&self, url: &str) -> bool {
        let Ok(parsed) = reqwest::Url::parse(url) else {
            return false;
        };
        let host = parsed.host_str().unwrap_or_default();
        // http/https only: no local-file, loopback-scheme, or other unsafe
        // destination may be rendered, matching the static fetch boundary.
        if !matches!(parsed.scheme(), "http" | "https") {
            let _ = append_audit(
                &self.audit_log,
                &audit_entry(url, host, "render-blocked-scheme", &self.query),
            );
            return false;
        }
        // A rendered page can reference arbitrary subresources; an open-web
        // allowlist must never let the browser reach a loopback, link-local,
        // or private-network address (SSRF). This block is unconditional,
        // ahead of the host allowlist.
        if is_internal_host(host) {
            let _ = append_audit(
                &self.audit_log,
                &audit_entry(url, host, "render-blocked-internal", &self.query),
            );
            return false;
        }
        match self.access.decide_host(host) {
            FetchDecision::Allowed => {
                let _ = append_audit(
                    &self.audit_log,
                    &audit_entry(url, host, "render-request", &self.query),
                );
                true
            }
            FetchDecision::NeedsConfirmation | FetchDecision::Disabled => {
                let _ = append_audit(
                    &self.audit_log,
                    &audit_entry(url, host, "render-blocked", &self.query),
                );
                false
            }
        }
    }
}

/// Whether a host is an internal/private destination the renderer must never
/// reach, regardless of the allowlist: `localhost`, or an IP literal in a
/// loopback, link-local, private, or unspecified range (SSRF guard). A public
/// DNS name returns `false` (its resolved address is the browser's concern; the
/// allowlist governs which names are reachable at all).
fn is_internal_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let Ok(ip) = host.parse::<std::net::IpAddr>() else {
        return false;
    };
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
        }
        std::net::IpAddr::V6(v6) => {
            let segments = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                // Unique-local fc00::/7 and link-local fe80::/10 (is_unique_local /
                // is_unicast_link_local are unstable, so test the prefixes here).
                || (segments[0] & 0xfe00) == 0xfc00
                || (segments[0] & 0xffc0) == 0xfe80
        }
    }
}

/// The countable outcome of one URL's fetch attempt.
enum FetchOutcome {
    /// Reduced, bounded evidence with any render lead (pre-admission). Boxed:
    /// this variant dwarfs the flag variants.
    Fetched(Box<FetchedPage>),
    /// The host is cooling down after an earlier rate-limit/server error.
    Cooled,
    /// A redirect response, never followed.
    Redirected,
    /// A transport error, unsuccessful status, or unreadable body.
    Failed,
}

#[async_trait]
impl Source for WebSource {
    fn label(&self) -> &str {
        "web"
    }

    async fn gather(&self, question: &str, limit: usize) -> Result<Gathered, SourceError> {
        let mut account = SourceAccount::new("web");
        // Fail-closed: with no active consent, do nothing — not even propose
        // URLs (which would touch the model or a search server). This is the
        // `Disabled` path.
        if !self.access.is_active() {
            return Ok(Gathered {
                evidence: Vec::new(),
                account,
            });
        }
        // Only the redacted sub-question leaves the machine — never evidence.
        // A sub-question that dropped the topic's load-bearing constraints is
        // re-scoped with the topic first, so a generic sub-question cannot
        // silently become a generic web search (LocalHub#36).
        let scoped = scope_to_topic(&self.topic, question);
        let query = prepare_query(localpilot_config::redact::redact, &scoped);
        // Designated search tools propose first (real search results); the
        // model's proposals fill any remaining budget. Either may be absent —
        // search works without a model and vice versa.
        let mut urls = Vec::new();
        if let Some(search) = &self.search {
            urls.extend(search.propose(&query, limit, &self.audit_log).await);
        }
        if urls.len() < limit {
            if let Some(model) = &self.model {
                urls.extend(self.propose_urls(model, &query, limit).await?);
            }
        }
        let mut seen = std::collections::HashSet::new();
        urls.retain(|url| seen.insert(url.clone()));

        let mut evidence = Vec::new();
        for url in urls.into_iter().take(limit) {
            account.proposed += 1;
            let Some(host) = parse_host(&url) else {
                account.failed += 1; // an unusable proposed URL is a counted outcome
                continue;
            };
            match self.access.decide_host(&host) {
                FetchDecision::Allowed => match self.fetch(&url, &host, question, &query).await? {
                    FetchOutcome::Fetched(page) => {
                        let FetchedPage {
                            evidence: found,
                            lead,
                        } = *page;
                        if let Some(admitted) = self.admit(&url, &host, &query, found).await? {
                            account.admitted += 1;
                            evidence.push(admitted);
                        } else {
                            account.rejected_relevance += 1;
                        }
                        // A client-rendered/iframe-embedded page: recover its
                        // real content from allowlisted frames through the same
                        // gated path, or record an explicit render-required
                        // outcome so the gap is inspectable (LocalHub#37).
                        if let Some(lead) = lead {
                            let ctx = FrameContext {
                                parent_url: &url,
                                parent_host: &host,
                                question,
                                query: &query,
                            };
                            self.handle_render_lead(&ctx, &lead, &mut account, &mut evidence)
                                .await?;
                        }
                    }
                    FetchOutcome::Cooled => account.policy_skipped += 1,
                    FetchOutcome::Redirected => account.redirected += 1,
                    FetchOutcome::Failed => account.failed += 1,
                },
                FetchDecision::NeedsConfirmation => {
                    account.policy_skipped += 1;
                    append_audit(
                        &self.audit_log,
                        &audit_entry(&url, &host, "skipped", &query),
                    )?;
                }
                FetchDecision::Disabled => return Ok(Gathered { evidence, account }),
            }
        }
        Ok(Gathered { evidence, account })
    }
}

/// Build the URL-proposal prompt. One URL per line keeps the reply parseable.
fn propose_urls_prompt(query: &str, limit: usize) -> String {
    format!(
        "List up to {limit} specific http or https URLs of authoritative \
         documentation or reference pages that would help answer the question \
         below. Output one URL per line and nothing else.\n\nQuestion: {query}"
    )
}

/// Parse the host from a URL with a real parser. `None` when the URL does not
/// parse or carries no host.
fn parse_host(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .host_str()
        .map(str::to_string)
}

/// Extract http/https URLs from a model reply, one per non-empty line. Tolerates
/// leading bullets/numbering and trailing prose by scanning from the scheme.
fn extract_urls(text: &str) -> Vec<String> {
    text.lines().filter_map(extract_url).collect()
}

fn extract_url(line: &str) -> Option<String> {
    let start = line.find("https://").or_else(|| line.find("http://"))?;
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Whether a fetched body should be reduced from HTML to text before it becomes
/// evidence. True on an HTML-ish `Content-Type`, or — when the server sent none
/// — on a body that opens with an HTML marker. A declared non-HTML type (plain
/// text, Markdown, JSON) is kept verbatim so its exact content survives.
fn is_html(content_type: &str, body: &str) -> bool {
    if content_type.contains("text/html") || content_type.contains("application/xhtml") {
        return true;
    }
    if content_type.is_empty() {
        let head: String = body.trim_start().chars().take(512).collect();
        let head = head.to_ascii_lowercase();
        return head.starts_with("<!doctype html") || head.contains("<html");
    }
    false
}

/// Truncate a fetched (already-reduced) body to at most `max_bytes`, never
/// splitting a UTF-8 char, preferring a line boundary, and — when a cut
/// happens — saying so explicitly rather than ending mid-sentence with no
/// explanation (the finding's provenance URL points at the full source).
fn bound_body(body: &str, max_bytes: usize) -> String {
    if body.len() <= max_bytes {
        return body.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let head = &body[..end];
    // A line boundary keeps the cut readable; fall back to the plain cut when
    // the content is one enormous line and a line cut would discard too much.
    let cut = match head.rfind('\n') {
        Some(newline) if newline >= end / 2 => newline,
        _ => end,
    };
    format!(
        "{}\n… (fetched content truncated at the per-fetch bound; full source at the cited URL)",
        &body[..cut]
    )
}

/// One audit record. `question` carries the **redacted** query, never raw text.
fn audit_entry(url: &str, host: &str, decision: &str, question: &str) -> AuditEntry {
    AuditEntry {
        url: url.to_string(),
        host: host.to_string(),
        decision: decision.to_string(),
        question: question.to_string(),
    }
}

/// Append one audit line to the egress log, creating the file and its parent on
/// first use. Append-only, one line per outbound request or skip.
fn append_audit(path: &Path, entry: &AuditEntry) -> Result<(), SourceError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|error| SourceError::new("web", format!("audit dir: {error}")))?;
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| SourceError::new("web", format!("audit open: {error}")))?;
    writeln!(file, "{}", entry.to_line())
        .map_err(|error| SourceError::new("web", format!("audit write: {error}")))?;
    Ok(())
}

/// Render the report and write it (redacted) to `dir`, returning the path.
fn write_report(dir: &Path, topic: &str, report: &ResearchReport) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.md", slugify(topic)));
    let body = localpilot_config::redact::redact(&render_markdown(report));
    std::fs::write(&path, body)?;
    Ok(path)
}

/// Map supported, backed findings to review-queue candidates and enqueue them
/// through the existing review-gated path. Returns the number enqueued. Never
/// writes accepted memory directly.
///
/// The candidate's lesson text is the finding's concise statement plus its
/// source line; a finding distilled from a raw source blob carries the full
/// bounded source **separately** (the candidate's evidence field, rendered by
/// review surfaces under the lesson), and is marked as an excerpt a reviewer
/// must distil before promotion — so the review experience keeps the complete
/// evidence (per the accepted full-evidence review contract) while promotion
/// can only ever write a standalone lesson into searchable memory.
fn enqueue_candidates(root: &Path, report: &ResearchReport) -> anyhow::Result<usize> {
    let mut enqueued = 0;
    for spec in candidates_from(report, RESEARCH_CANDIDATE_CONFIDENCE_CAP) {
        // Surface evidence relevance and candidate trust as two named,
        // truthful numbers: the reviewer sees how strong the match was
        // (relevance) without confusing it with the deliberately low trust an
        // unreviewed machine-derived candidate carries (LocalHub#36).
        let body = format!(
            "{}\n\n(research finding — evidence relevance {:.2}, candidate trust {:.2}; \
             sources: {})",
            spec.body,
            spec.evidence_relevance,
            spec.confidence,
            provenance_summary(&spec.provenance)
        );
        let mut lesson = localpilot_localmind::RetrospectiveLesson::research_finding(
            localpilot_config::redact::redact(&body),
            spec.confidence,
        );
        if let Some(evidence) = &spec.evidence {
            // Distilled excerpt: full source rides the candidate's own
            // evidence field (review-only), and the excerpt must be edited
            // into a standalone lesson before it can promote.
            lesson = lesson
                .with_evidence_text(localpilot_config::redact::redact(&evidence_block(evidence)))
                .requiring_edit();
        } else if looks_like_boilerplate(&spec.body) {
            // A clean-looking statement that is actually navigation chrome /
            // menu text: route it to review with the same edit requirement —
            // never auto-delete it.
            lesson = lesson.requiring_edit();
        }
        if localpilot_localmind::write_retrospective_lesson(root, &lesson)?.is_some() {
            enqueued += 1;
        }
    }
    Ok(enqueued)
}

/// Whether a statement reads as web boilerplate rather than prose: dozens of
/// words with almost no sentence structure is a navigation menu, banner, or
/// link farm — provenance-backed, but not a reusable lesson as-is.
fn looks_like_boilerplate(text: &str) -> bool {
    let words = text.split_whitespace().count();
    if words < 12 {
        return false;
    }
    let sentence_marks = text.matches(['.', '!', '?']).count();
    sentence_marks * 20 < words
}

fn provenance_summary(provenance: &[Provenance]) -> String {
    provenance
        .iter()
        .map(|p| match &p.locator {
            Some(locator) => format!("{}:{locator}", p.source),
            None => p.source.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Turn a topic into a filesystem-safe slug. Falls back to `research` when the
/// topic has no alphanumeric characters.
fn slugify(topic: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in topic.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(60).collect();
    if slug.is_empty() {
        "research".to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_llm::FakeProvider;
    use localpilot_research::ClaimStatus;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Wrap a scripted fake provider as a [`ModelHandle`] for offline tests.
    fn model_handle(provider: Arc<FakeProvider>) -> ModelHandle {
        ModelHandle {
            provider,
            model: "test-model".to_string(),
        }
    }

    fn knowledge_hit() -> localpilot_localmind::KnowledgeHit {
        localpilot_localmind::KnowledgeHit {
            chunk_id: "c1".to_string(),
            path: "src/lib.rs".to_string(),
            score: 10,
            start_line: 4,
            end_line: 9,
            content_hash: "h".to_string(),
            stale: false,
            snippet: "fn foo() {}".to_string(),
            token_estimate: 5,
            inclusion_reason: "match".to_string(),
            skip_reason: None,
            relevance: 0.8,
        }
    }

    fn memory_hit() -> localpilot_localmind::SearchHit {
        localpilot_localmind::SearchHit {
            memory_id: "mem_7".to_string(),
            score: 3,
            path: "memory/7.md".to_string(),
            snippet: "prefer X over Y".to_string(),
            category: "guidance".to_string(),
            cosine: None,
        }
    }

    #[test]
    fn knowledge_hit_maps_to_path_line_provenance_with_fetch_id() {
        let evidence = map_knowledge_hit("how", &knowledge_hit(), 0.6);
        assert_eq!(evidence.snippet, "fn foo() {}");
        assert_eq!(evidence.provenance.source, "knowledge");
        assert_eq!(
            evidence.provenance.locator.as_deref(),
            Some("src/lib.rs:4-9")
        );
        assert_eq!(
            evidence.provenance.fetch_id.as_deref(),
            Some("c1"),
            "the fetchable chunk id survives into research provenance"
        );
        assert!((evidence.relevance - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn memory_hit_maps_to_id_provenance() {
        let evidence = map_memory_hit("how", &memory_hit());
        assert_eq!(evidence.provenance.source, "memory");
        assert_eq!(evidence.provenance.locator.as_deref(), Some("mem_7"));
        assert_eq!(
            evidence
                .admission
                .as_ref()
                .map(|trail| trail.reason.as_str()),
            Some("reviewed memory")
        );
    }

    #[test]
    fn full_chunk_evidence_is_the_chunk_body_not_the_snippet() {
        let hit = knowledge_hit();
        let body = localpilot_localmind::FetchedBody {
            id: "c1".to_string(),
            path: "src/lib.rs".to_string(),
            start_line: 4,
            end_line: 9,
            body: "fn foo() {}\n// the full surrounding chunk with real context\nfn bar() {}"
                .to_string(),
            token_cost: 16,
        };
        let text = full_chunk_evidence(&hit, Some(&body));
        assert!(text.contains("the full surrounding chunk"), "{text}");
        assert!(!text.contains("[stale:"), "fresh chunk carries no warning");
    }

    #[test]
    fn stale_chunk_evidence_is_marked() {
        let mut hit = knowledge_hit();
        hit.stale = true;
        let body = localpilot_localmind::FetchedBody {
            id: "c1".to_string(),
            path: "src/lib.rs".to_string(),
            start_line: 4,
            end_line: 9,
            body: "fn foo() {}".to_string(),
            token_cost: 4,
        };
        let text = full_chunk_evidence(&hit, Some(&body));
        assert!(
            text.starts_with("[stale:"),
            "stale ingest state is disclosed, never silent: {text}"
        );
    }

    #[test]
    fn missing_chunk_evidence_is_an_explicit_unavailable_state() {
        let hit = knowledge_hit();
        let text = full_chunk_evidence(&hit, None);
        assert!(
            text.starts_with("[full source unavailable:"),
            "a search snippet must not silently pose as full source: {text}"
        );
        assert!(text.contains("fn foo() {}"), "the snippet still shows");
    }

    #[test]
    fn oversized_chunk_evidence_is_cut_with_disclosure() {
        let hit = knowledge_hit();
        let body = localpilot_localmind::FetchedBody {
            id: "c1".to_string(),
            path: "src/lib.rs".to_string(),
            start_line: 4,
            end_line: 9,
            body: "x".repeat(LOCAL_EVIDENCE_MAX_CHARS + 10),
            token_cost: 99,
        };
        let text = full_chunk_evidence(&hit, Some(&body));
        assert!(text.contains("chunk truncated"), "the cut is loud");
    }

    #[test]
    fn slugify_is_filesystem_safe() {
        assert_eq!(slugify("Tokio select! macro"), "tokio-select-macro");
        assert_eq!(slugify("  spaced  "), "spaced");
        assert_eq!(slugify("***"), "research");
    }

    #[test]
    fn any_web_evidence_is_false_when_no_finding_cites_web() {
        // Bug it prevents: `--web` silently contributing nothing (inactive
        // access, or no chat model configured) reading identically to "web
        // was queried and found nothing," leaving the user with only a local
        // finding and no sign web was ever attempted.
        let mut report = ResearchReport::new("t");
        report.findings = vec![Finding {
            statement: "a local finding".to_string(),
            status: ClaimStatus::Supported,
            supporting: vec![Provenance::new("knowledge", Some("a.rs:1-3".to_string()))],
            evidence: None,
            confidence: 0.5,
        }];
        assert!(!any_web_evidence(&report));
    }

    #[test]
    fn any_web_evidence_is_true_when_a_finding_cites_web() {
        let mut report = ResearchReport::new("t");
        report.findings = vec![Finding {
            statement: "a web finding".to_string(),
            status: ClaimStatus::Supported,
            supporting: vec![Provenance::new(
                "web",
                Some("https://example.com".to_string()),
            )],
            evidence: None,
            confidence: 0.5,
        }];
        assert!(any_web_evidence(&report));
    }

    #[test]
    fn write_report_writes_rendered_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let mut report = ResearchReport::new("caching");
        report.findings = vec![Finding {
            statement: "caches speed reads".to_string(),
            status: ClaimStatus::Supported,
            supporting: vec![Provenance::new("memory", Some("mem_1".to_string()))],
            evidence: None,
            confidence: 1.0,
        }];
        let path = write_report(dir.path(), "caching", &report).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# Research: caching"));
        assert!(body.contains("caches speed reads"));
        assert!(path.ends_with("caching.md"));
    }

    #[cfg(feature = "tui")]
    #[test]
    fn research_mode_notice_reflects_configured_egress_state() {
        // The project layer overrides the user layer, so writing an explicit
        // project config keeps both branches deterministic on any machine.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".localpilot.toml"),
            "[research.web]\nenabled = true\n",
        )
        .unwrap();
        let notice = research_mode_notice(dir.path());
        assert!(
            notice.contains("local sources + web"),
            "web-on copy must disclose egress: {notice}"
        );

        std::fs::write(
            dir.path().join(".localpilot.toml"),
            "[research.web]\nenabled = false\n",
        )
        .unwrap();
        let notice = research_mode_notice(dir.path());
        assert!(
            notice.contains("local sources only"),
            "web-off copy must state the kill switch: {notice}"
        );
    }

    // --- model-assisted synthesizer (decomposition only) ---------------------

    #[tokio::test]
    async fn no_model_decompose_uses_the_heuristic() {
        let synth = CliSynthesizer { model: None };
        let questions = synth.decompose("async runtimes", 4).await.unwrap();
        assert_eq!(
            questions,
            vec!["async runtimes".to_string()],
            "with no model, decomposition is the single-topic heuristic"
        );
    }

    #[tokio::test]
    async fn model_decompose_splits_lines_and_bounds() {
        let provider = Arc::new(
            FakeProvider::new().text("what is it\nhow does it work\nwhen to use\nextra\n"),
        );
        let synth = CliSynthesizer {
            model: Some(model_handle(provider)),
        };
        let questions = synth.decompose("topic", 3).await.unwrap();
        assert_eq!(
            questions,
            vec![
                "what is it".to_string(),
                "how does it work".to_string(),
                "when to use".to_string(),
            ],
            "lines are trimmed, empties dropped, and the count bounded"
        );
    }

    #[tokio::test]
    async fn model_decompose_empty_reply_falls_back_to_heuristic() {
        let provider = Arc::new(FakeProvider::new().text("   \n\n  "));
        let synth = CliSynthesizer {
            model: Some(model_handle(provider)),
        };
        let questions = synth.decompose("topic", 3).await.unwrap();
        assert_eq!(questions, vec!["topic".to_string()]);
    }

    #[tokio::test]
    async fn synthesize_stays_provenance_preserving() {
        // The model-assisted synthesizer never invents findings: synthesis is the
        // heuristic, so each evidence snippet becomes a supported finding that
        // carries its own provenance.
        let synth = CliSynthesizer { model: None };
        let evidence = vec![Evidence::new(
            "q",
            "caches speed reads",
            Provenance::new("memory", Some("mem_1".to_string())),
            1.0,
        )];
        let findings = synth.synthesize("topic", &evidence).await.unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].statement, "caches speed reads");
        assert_eq!(findings[0].supporting.len(), 1);
        assert_eq!(findings[0].supporting[0].locator.as_deref(), Some("mem_1"));
    }

    #[test]
    fn enqueue_routes_sanitized_excerpt_findings_to_the_review_queue() {
        // A supported, backed finding the sanitize pass reduced to an excerpt
        // (its raw blob moved into `evidence`) must reach the review queue
        // carrying BOTH its distilled statement AND the full source it was
        // distilled from (LocalHub#1) — but as *separate* candidate fields:
        // the full source rides the candidate's review-only evidence, never
        // the lesson text, so a later promotion cannot turn the raw dump into
        // searchable memory (LocalHub#24). The excerpt is also marked as
        // needing a reviewer's edit before promotion.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let mut report = ResearchReport::new("three.js performance");
        let raw = "<div>use InstancedMesh for many identical meshes</div>";
        let excerpt = Finding {
            statement: "Excerpt from knowledge: use InstancedMesh for many identical meshes"
                .to_string(),
            status: localpilot_research::ClaimStatus::Supported,
            supporting: vec![Provenance::new(
                "knowledge",
                Some("perf.md:1-20".to_string()),
            )],
            evidence: Some(raw.to_string()),
            confidence: 0.6,
        };
        report.findings = vec![excerpt];

        let enqueued = enqueue_candidates(root, &report).unwrap();
        assert_eq!(enqueued, 1, "the excerpt finding is enqueued, not dropped");

        let items = localpilot_localmind::review_list(root).unwrap();
        assert_eq!(items.len(), 1, "one review candidate reaches the queue");
        // The distilled claim leads (so `review list` stays scannable)…
        assert!(
            items[0]
                .summary
                .contains("Excerpt from knowledge: use InstancedMesh"),
            "the queue carries the distilled statement: {items:?}"
        );
        // …the lesson text itself no longer tows the raw dump…
        assert!(
            !items[0].summary.contains(raw),
            "the raw source must not ride the promotable lesson text: {items:?}"
        );
        // …the full source rides the candidate's review-only evidence field…
        assert!(
            items[0]
                .evidence_text
                .as_deref()
                .is_some_and(|evidence| evidence.contains(raw)),
            "the full source content reaches the reviewer: {items:?}"
        );
        // …and the excerpt demands a reviewer's edit before promotion.
        assert!(
            items[0].requires_edit,
            "an excerpt is not memory-ready as-is: {items:?}"
        );
    }

    #[test]
    fn boilerplate_statements_are_marked_for_edit_before_promotion() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let mut report = ResearchReport::new("frameworks");
        let chrome = Finding {
            statement: "Home Pricing Docs Blog Careers Contact Sign in Get started Products \
                        Solutions Enterprise Resources"
                .to_string(),
            status: localpilot_research::ClaimStatus::Supported,
            supporting: vec![Provenance::new(
                "web",
                Some("https://example.com/".to_string()),
            )],
            evidence: None,
            confidence: 0.3,
        };
        report.findings = vec![chrome];

        assert_eq!(enqueue_candidates(root, &report).unwrap(), 1);
        let items = localpilot_localmind::review_list(root).unwrap();
        assert!(
            items[0].requires_edit,
            "navigation chrome must be routed to review, not promoted verbatim: {items:?}"
        );
    }

    #[test]
    fn admission_json_is_parsed_strictly() {
        assert_eq!(
            parse_admission(r#"{"relevant": true, "score": 0.8, "reason": "on-topic"}"#),
            Some(AdmissionVerdict {
                relevant: true,
                score: 0.8,
                reason: "on-topic".to_string(),
            })
        );
        // Prose around one JSON object is tolerated; a missing reason is empty.
        assert_eq!(
            parse_admission("Sure: {\"relevant\": false, \"score\": 0.1} done"),
            Some(AdmissionVerdict {
                relevant: false,
                score: 0.1,
                reason: String::new(),
            })
        );
        // …anything else is unusable, so the deterministic path stands.
        assert_eq!(parse_admission("not json"), None);
        assert_eq!(parse_admission(r#"{"relevant": "yes"}"#), None);
        // Out-of-range scores are clamped, non-finite rejected.
        assert_eq!(
            parse_admission(r#"{"relevant": true, "score": 7.0}"#),
            Some(AdmissionVerdict {
                relevant: true,
                score: 1.0,
                reason: String::new(),
            })
        );
    }

    #[test]
    fn scope_to_topic_prepends_when_a_sub_question_dropped_the_constraint() {
        // LocalHub#36: a sub-question that no longer names the framework is
        // re-scoped with the topic, so the search is not silently generic.
        let scoped = scope_to_topic(
            "three.js procedural materials",
            "How are parametric controls exposed to users in real-time?",
        );
        assert!(
            scoped.starts_with("three.js procedural materials "),
            "the topic is prefixed: {scoped}"
        );
        assert!(scoped.contains("parametric controls"));
    }

    #[test]
    fn scope_to_topic_leaves_a_sub_question_that_still_carries_the_constraint() {
        // The framework and both topic nouns are already present — no redundant
        // duplication.
        let question = "How do noise functions generate procedural materials in three.js?";
        let scoped = scope_to_topic("three.js procedural materials", question);
        assert_eq!(scoped, question);
    }

    #[test]
    fn scope_to_topic_is_a_noop_for_an_empty_topic() {
        assert_eq!(scope_to_topic("   ", "some question"), "some question");
    }

    #[test]
    fn decompose_prompt_demands_the_topic_constraints_are_kept() {
        let prompt = decompose_prompt("three.js procedural materials", 6);
        assert!(prompt.contains("framework"), "{prompt}");
        assert!(
            prompt.contains("stay within") || prompt.contains("scope"),
            "the prompt binds sub-questions to the topic scope: {prompt}"
        );
    }

    #[test]
    fn admission_reason_carries_the_model_rationale_when_present() {
        assert_eq!(admission_reason(""), "model admission");
        assert_eq!(
            admission_reason("off-topic engine"),
            "model admission — off-topic engine"
        );
    }

    #[test]
    fn extract_urls_tolerates_bullets_and_prose() {
        let text = "- https://docs.rs/tokio see this\n2. http://example.com/x\nno url here\n";
        assert_eq!(
            extract_urls(text),
            vec![
                "https://docs.rs/tokio".to_string(),
                "http://example.com/x".to_string(),
            ]
        );
    }

    // --- local knowledge admission (LocalHub#32/#34) --------------------------

    /// The issue-#32 reproduction: a corpus sharing only generic terms with
    /// the question must yield **no admitted local evidence** — its least-bad
    /// hit must not be promoted to full relevance by within-source rank.
    /// Adding one genuinely answering chunk admits that chunk without
    /// promoting the weak candidates riding the same corpus.
    #[tokio::test]
    async fn weak_local_corpus_yields_no_admitted_evidence_and_an_answer_admits_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::fs::write(
            dir.path().join("docs/notes.md"),
            "The pipeline logs progress counters for the team.\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("docs/other.md"),
            "Progress pipeline notes collected weekly.\n",
        )
        .unwrap();
        localpilot_localmind::ingest_run(
            dir.path(),
            &localpilot_config::IngestConfig::default(),
            localpilot_localmind::RunMode::Full,
        )
        .unwrap();
        let source = KnowledgeSource {
            root: dir.path().to_path_buf(),
            topic: "test topic".to_string(),
            admission: None,
        };
        let question = "how does the research pipeline report progress for animation \
                        mixer clip weights during gpu skinning";
        let gathered = source.gather(question, 5).await.unwrap();
        for item in &gathered.evidence {
            assert!(
                item.relevance < localpilot_research::COVERAGE_RELEVANCE_FLOOR,
                "a generic-terms-only corpus must stay below the admission floor; \
                 got {} for {:?}",
                item.relevance,
                item.provenance.locator
            );
            let trail = item.admission.as_ref().expect("admission trail present");
            assert_eq!(trail.reason, "term overlap");
        }

        // One genuinely answering chunk: admitted on its own merits, weak
        // corpus siblings unchanged.
        std::fs::write(
            dir.path().join("docs/answer.md"),
            "The animation mixer blends clip weights across the skeleton before \
             gpu skinning deforms the mesh; the research pipeline reports mixer \
             progress per clip.\n",
        )
        .unwrap();
        localpilot_localmind::ingest_run(
            dir.path(),
            &localpilot_config::IngestConfig::default(),
            localpilot_localmind::RunMode::Full,
        )
        .unwrap();
        let gathered = source.gather(question, 5).await.unwrap();
        let admitted: Vec<_> = gathered
            .evidence
            .iter()
            .filter(|item| item.relevance >= localpilot_research::COVERAGE_RELEVANCE_FLOOR)
            .collect();
        assert!(
            !admitted.is_empty(),
            "the answering chunk clears the floor: {:?}",
            gathered
                .evidence
                .iter()
                .map(|item| (item.provenance.locator.clone(), item.relevance))
                .collect::<Vec<_>>()
        );
        for item in &admitted {
            let locator = item.provenance.locator.as_deref().unwrap_or_default();
            assert!(
                locator.contains("answer.md"),
                "only the answering chunk is admitted, not its corpus siblings: {locator}"
            );
            assert!(
                item.provenance.fetch_id.is_some(),
                "the fetchable chunk id rides research provenance"
            );
            let full = item.full_source.as_deref().expect("full chunk fetched");
            assert!(
                full.contains("deforms the mesh"),
                "full source is the chunk body, not only the search window: {full}"
            );
        }
    }

    // --- web source (egress gate) --------------------------------------------

    async fn ok_server(body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        server
    }

    fn web_source(
        server_url: &str,
        allowlist: Vec<String>,
        enabled: bool,
        audit_log: PathBuf,
    ) -> (WebSource, Arc<FakeProvider>) {
        let mut access = WebAccess::new(enabled, allowlist, Vec::new());
        access.grant_session();
        let fake = Arc::new(FakeProvider::new().text(server_url));
        let source = WebSource::new(
            "test topic",
            localpilot_config::RenderMode::Auto,
            access,
            audit_log,
            Some(model_handle(Arc::clone(&fake))),
            None,
            None,
        )
        .unwrap();
        (source, fake)
    }

    #[tokio::test]
    async fn allowlisted_host_is_fetched_and_audited() {
        let server = ok_server("documentation body").await;
        let url = format!("{}/page", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let (source, _fake) = web_source(&url, vec![host], true, audit.clone());

        let gathered = source.gather("how to use tokio", 3).await.unwrap();
        let evidence = gathered.evidence;
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].provenance.source, "web");
        assert_eq!(
            evidence[0].provenance.locator.as_deref(),
            Some(url.as_str())
        );
        assert!(evidence[0].snippet.contains("documentation body"));
        assert_eq!(gathered.account.admitted, 1);

        let log = std::fs::read_to_string(&audit).unwrap();
        assert_eq!(log.lines().count(), 1, "one audited request");
        assert!(log.contains("decision=allowed"));
    }

    #[tokio::test]
    async fn iframe_only_parent_recovers_the_allowlisted_child_frame() {
        // LocalHub#37: a documentation page whose body is an iframe used to be
        // reduced to empty/nav-only evidence, the frame silently discarded. Now
        // the frame src is extracted and fetched through the same gated path, so
        // the child article is recovered with the frame URL as its provenance.
        let server = MockServer::start().await;
        let child_body = "<html><body><article><h1>Procedural materials</h1><p>".to_string()
            + &"procedural materials in three.js use noise. ".repeat(12)
            + "</p></article></body></html>";
        let parent_body = format!(
            "<html><body><nav>menu</nav>\
             <iframe src=\"{}/child\"></iframe></body></html>",
            server.uri()
        );
        wiremock::Mock::given(method("GET"))
            .and(wiremock::matchers::path("/child"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(child_body.as_bytes(), "text/html"),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(method("GET"))
            .and(wiremock::matchers::path("/parent"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(parent_body.as_bytes(), "text/html"),
            )
            .mount(&server)
            .await;
        let parent_url = format!("{}/parent", server.uri());
        let child_url = format!("{}/child", server.uri());
        let host = parse_host(&parent_url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let (source, _fake) = web_source(&parent_url, vec![host], true, audit.clone());

        let gathered = source
            .gather("how do procedural materials work in three.js", 3)
            .await
            .unwrap();
        assert!(
            gathered
                .evidence
                .iter()
                .any(|e| e.provenance.locator.as_deref() == Some(child_url.as_str())),
            "the child frame article is recovered with its own provenance: {:?}",
            gathered
                .evidence
                .iter()
                .map(|e| e.provenance.locator.clone())
                .collect::<Vec<_>>()
        );
        assert!(
            gathered
                .account
                .render_notes
                .iter()
                .any(|note| note.contains("recovered")),
            "a render note records the recovery: {:?}",
            gathered.account.render_notes
        );
    }

    #[tokio::test]
    async fn spa_shell_with_no_frame_records_render_required_not_silent() {
        // LocalHub#37: an empty framework shell must produce an explicit
        // render-required outcome (renderer unavailable in the static build),
        // never be silently counted as complete evidence.
        let server = MockServer::start().await;
        let shell = "<html><body><div id=\"root\"></div>\
                     <script src=\"/app.js\"></script></body></html>";
        wiremock::Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(shell.as_bytes(), "text/html"))
            .mount(&server)
            .await;
        let url = format!("{}/app", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let (source, _fake) = web_source(&url, vec![host], true, audit.clone());

        let gathered = source
            .gather("how does the widget render", 3)
            .await
            .unwrap();
        assert_eq!(
            gathered.account.render_required, 1,
            "the shell is flagged render-required, not treated as complete"
        );
        let log = std::fs::read_to_string(&audit).unwrap();
        assert!(
            log.contains("decision=render-required"),
            "render-required is audited: {log}"
        );
    }

    #[tokio::test]
    async fn render_mode_off_disables_detection_entirely() {
        // The kill switch: with mode `off`, an empty shell is fetched statically
        // and never flagged — no detection, no render-required, no extra audit.
        let server = MockServer::start().await;
        let shell = "<html><body><div id=\"root\"></div>\
                     <script src=\"/a.js\"></script><script src=\"/b.js\"></script></body></html>";
        wiremock::Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(shell.as_bytes(), "text/html"))
            .mount(&server)
            .await;
        let url = format!("{}/app", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let mut access = WebAccess::new(true, vec![host], Vec::new());
        access.grant_session();
        let fake = Arc::new(FakeProvider::new().text(&url));
        let source = WebSource::new(
            "test topic",
            localpilot_config::RenderMode::Off,
            access,
            audit.clone(),
            Some(model_handle(Arc::clone(&fake))),
            None,
            None,
        )
        .unwrap();

        let gathered = source
            .gather("how does the widget render", 3)
            .await
            .unwrap();
        assert_eq!(
            gathered.account.render_required, 0,
            "mode=off performs no render detection"
        );
        let log = std::fs::read_to_string(&audit).unwrap();
        assert!(
            !log.contains("render-required"),
            "no render-required audit under mode=off: {log}"
        );
    }

    /// A renderer stub: returns fixed post-JavaScript HTML (and optional frames)
    /// and exercises the gate, so the WebSource render-admit wiring is tested
    /// without a browser.
    struct FakeRenderer {
        html: String,
        frames: Vec<localpilot_research::RenderedFrame>,
    }

    #[async_trait]
    impl Renderer for FakeRenderer {
        async fn render(
            &self,
            request: &RenderRequest,
            gate: &dyn RenderGate,
        ) -> Result<localpilot_research::RenderedDoc, localpilot_research::RenderFailure> {
            // The real renderer gates every browser request; exercise the gate
            // on the page URL so the wiring path is realistic.
            let _ = gate.allow(&request.url);
            Ok(localpilot_research::RenderedDoc {
                html: self.html.clone(),
                frames: self.frames.clone(),
                blocked: 0,
            })
        }
    }

    #[tokio::test]
    async fn a_render_signal_page_admits_the_rendered_main_document() {
        // LocalHub#37: when a render signal fires and a renderer is present, the
        // page's post-JavaScript content is rendered, admitted, and the page is
        // no longer flagged render-required.
        let server = MockServer::start().await;
        let shell = "<html><body><div id=\"root\"></div>\
                     <script src=\"/app.js\"></script></body></html>";
        wiremock::Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(shell.as_bytes(), "text/html"))
            .mount(&server)
            .await;
        let url = format!("{}/app", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let (source, _fake) = web_source(&url, vec![host], true, audit.clone());
        let rendered_html = format!(
            "<html><body><article>{}</article></body></html>",
            "rendered widget documentation content. ".repeat(10)
        );
        let source = source.with_renderer(Some(Arc::new(FakeRenderer {
            html: rendered_html,
            frames: Vec::new(),
        })));

        let gathered = source
            .gather("how does the widget render", 3)
            .await
            .unwrap();
        assert!(
            gathered
                .evidence
                .iter()
                .any(|e| e.snippet.contains("rendered widget documentation")),
            "the rendered main document is admitted as evidence: {:?}",
            gathered
                .evidence
                .iter()
                .map(|e| &e.snippet)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            gathered.account.render_required, 0,
            "a successful render is not flagged render-required"
        );
        assert!(
            gathered
                .account
                .render_notes
                .iter()
                .any(|note| note.contains("rendered main document")),
            "a render note records the rendered document: {:?}",
            gathered.account.render_notes
        );
    }

    #[tokio::test]
    async fn rendered_frames_are_admitted_with_frame_provenance() {
        // LocalHub#37: a rendered frame's document is admitted as its own
        // evidence, carrying the frame's resolved URL (or a srcdoc locator) as
        // provenance — the frame content is never lost or merged into the parent.
        let server = MockServer::start().await;
        let shell = "<html><body><div id=\"root\"></div>\
                     <iframe src=\"/child\"></iframe></body></html>";
        wiremock::Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(shell.as_bytes(), "text/html"))
            .mount(&server)
            .await;
        let url = format!("{}/app", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let (source, _fake) = web_source(&url, vec![host], true, audit.clone());
        let frame_html = format!(
            "<html><body><article>{}</article></body></html>",
            "framed widget render documentation. ".repeat(10)
        );
        let source = source.with_renderer(Some(Arc::new(FakeRenderer {
            html: "<html><body><nav>shell</nav></body></html>".to_string(),
            frames: vec![
                localpilot_research::RenderedFrame {
                    url: Some("/child".to_string()),
                    html: frame_html,
                },
                localpilot_research::RenderedFrame {
                    url: None,
                    html: format!(
                        "<html><body><p>{}</p></body></html>",
                        "srcdoc widget render notes. ".repeat(10)
                    ),
                },
            ],
        })));

        let gathered = source
            .gather("how does the widget render", 3)
            .await
            .unwrap();
        let locators: Vec<String> = gathered
            .evidence
            .iter()
            .filter_map(|e| e.provenance.locator.clone())
            .collect();
        assert!(
            locators.iter().any(|l| l.ends_with("/child")),
            "the same-origin frame is admitted with its resolved URL: {locators:?}"
        );
        assert!(
            locators.iter().any(|l| l.contains("srcdoc frame")),
            "the srcdoc frame is admitted with a srcdoc locator: {locators:?}"
        );
        assert_eq!(
            gathered.account.render_required, 0,
            "rendered frames count as content, not render-required"
        );
    }

    /// A renderer that counts its invocations, to assert the browser is not
    /// launched for a server-rendered page.
    struct CountingRenderer {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Renderer for CountingRenderer {
        async fn render(
            &self,
            _request: &RenderRequest,
            _gate: &dyn RenderGate,
        ) -> Result<localpilot_research::RenderedDoc, localpilot_research::RenderFailure> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(localpilot_research::RenderedDoc::default())
        }
    }

    #[tokio::test]
    async fn server_rendered_page_does_not_invoke_the_renderer() {
        // LocalHub#37 control: a page with real static content shows no render
        // signal, so the browser is never launched and the static content is
        // admitted unchanged.
        let server = MockServer::start().await;
        let page = format!(
            "<html><body><article><h1>Widget guide</h1><p>{}</p></article></body></html>",
            "SERVER_RENDERED_MARKER real documentation content. ".repeat(15)
        );
        wiremock::Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(page.as_bytes(), "text/html"))
            .mount(&server)
            .await;
        let url = format!("{}/guide", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let (source, _fake) = web_source(&url, vec![host], true, audit);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = source.with_renderer(Some(Arc::new(CountingRenderer {
            calls: Arc::clone(&calls),
        })));

        let gathered = source
            .gather("how does the widget guide work", 3)
            .await
            .unwrap();
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a server-rendered page shows no render signal, so the browser is never launched"
        );
        assert_eq!(gathered.account.render_required, 0);
        assert!(
            gathered
                .evidence
                .iter()
                .any(|e| e.snippet.contains("SERVER_RENDERED_MARKER")),
            "the static content is admitted unchanged"
        );
    }

    #[tokio::test]
    async fn an_empty_render_records_no_substantive_content() {
        // A genuinely empty page (a shell whose render yields nothing) records
        // an explicit no-substantive-content outcome, never fabricated evidence.
        let server = MockServer::start().await;
        let shell = "<html><body><div id=\"root\"></div>\
                     <script src=\"/app.js\"></script></body></html>";
        wiremock::Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(shell.as_bytes(), "text/html"))
            .mount(&server)
            .await;
        let url = format!("{}/app", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let (source, _fake) = web_source(&url, vec![host], true, audit);
        let source = source.with_renderer(Some(Arc::new(FakeRenderer {
            html: "<html><body></body></html>".to_string(),
            frames: Vec::new(),
        })));

        let gathered = source
            .gather("how does the widget render", 3)
            .await
            .unwrap();
        assert!(
            gathered
                .account
                .render_notes
                .iter()
                .any(|note| note.contains("no substantive rendered content")),
            "an empty render is recorded, not fabricated: {:?}",
            gathered.account.render_notes
        );
        assert_eq!(
            gathered.account.render_required, 1,
            "an empty render with no recoverable frame is flagged render-required"
        );
    }

    #[test]
    fn render_gate_enforces_scheme_allowlist_and_blocks_internal() {
        // The browser renderer's egress boundary: http/https only, host
        // allowlist, and an unconditional SSRF block on internal addresses —
        // every decision audited content-free (LocalHub#37, docs/07).
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("gate.log");
        let mut access = WebAccess::new(true, vec!["docs.example".to_string()], Vec::new());
        access.grant_session();
        let gate = WebAccessGate {
            access,
            audit_log: audit.clone(),
            query: "q".to_string(),
        };

        assert!(
            gate.allow("https://docs.example/page"),
            "allowlisted host is allowed"
        );
        assert!(
            !gate.allow("https://evil.example/x"),
            "non-allowlisted host is blocked"
        );
        assert!(
            !gate.allow("file:///etc/passwd"),
            "non-http scheme is blocked"
        );
        assert!(!gate.allow("http://127.0.0.1/x"), "loopback is blocked");
        assert!(
            !gate.allow("http://169.254.169.254/latest/meta-data"),
            "link-local metadata endpoint is blocked"
        );
        assert!(!gate.allow("http://10.0.0.5/x"), "private range is blocked");
        assert!(!gate.allow("http://localhost/x"), "localhost is blocked");

        let log = std::fs::read_to_string(&audit).unwrap();
        assert!(log.contains("decision=render-request"), "{log}");
        assert!(log.contains("decision=render-blocked"), "{log}");
        assert!(log.contains("decision=render-blocked-internal"), "{log}");
        assert!(log.contains("decision=render-blocked-scheme"), "{log}");
    }

    #[test]
    fn is_internal_host_classifies_loopback_private_and_linklocal() {
        for internal in [
            "localhost",
            "127.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "10.1.2.3",
            "169.254.169.254",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::abcd",
        ] {
            assert!(is_internal_host(internal), "{internal} is internal");
        }
        for public in [
            "docs.rs",
            "example.com",
            "93.184.216.34",
            "8.8.8.8",
            "2606:2800:220::1",
        ] {
            assert!(!is_internal_host(public), "{public} is public");
        }
    }

    #[tokio::test]
    async fn html_page_evidence_is_reduced_to_readable_markdown() {
        // Regression (LocalHub#1): a fetched HTML page used to become evidence
        // as raw markup, leaking script/style bodies and tags into the finding
        // and its evidence block as junk; a flat-text reduction then lost all
        // structure. It must now arrive as readable Markdown — headings,
        // links, and code preserved, chrome dropped.
        let html = "<html><head><style>.a{color:red}</style></head><body>\
             <script>var x = leak();</script><h1>Tokio</h1>\
             <p>Tokio is an async runtime. See <a href=\"https://docs.rs/tokio\">the docs</a>.</p>\
             <pre>let rt = Runtime::new();</pre></body></html>";
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(html.as_bytes(), "text/html; charset=utf-8"),
            )
            .mount(&server)
            .await;
        let url = format!("{}/page", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let (source, _fake) = web_source(&url, vec![host], true, audit);

        let evidence = source.gather("what is tokio", 3).await.unwrap().evidence;
        assert_eq!(evidence.len(), 1);
        let snippet = &evidence[0].snippet;
        assert!(
            snippet.contains("Tokio is an async runtime."),
            "prose kept: {snippet}"
        );
        assert!(snippet.contains("# Tokio"), "heading survives: {snippet}");
        assert!(
            snippet.contains("[the docs](https://docs.rs/tokio)"),
            "link survives as Markdown: {snippet}"
        );
        assert!(
            snippet.contains("```\nlet rt = Runtime::new();\n```"),
            "code block survives fenced: {snippet}"
        );
        assert!(
            !snippet.contains("leak()"),
            "script body dropped: {snippet}"
        );
        assert!(
            !snippet.contains("color:red"),
            "style body dropped: {snippet}"
        );
        assert!(!snippet.contains('<'), "no markup remains: {snippet}");
    }

    #[test]
    fn bound_body_cut_is_loud_and_lands_on_a_line_boundary() {
        // A silent mid-sentence cut at the fetch bound reads as lost knowledge
        // (LocalHub#1 round 5); the cut must say what happened.
        let body = "a content line\n".repeat(200); // 3000 bytes
        let bounded = bound_body(&body, 1000);
        assert!(
            bounded.contains("truncated at the per-fetch bound"),
            "{bounded}"
        );
        assert!(
            bounded
                .lines()
                .take_while(|l| l.starts_with('a'))
                .all(|l| l == "a content line"),
            "no mid-line cut: {bounded}"
        );

        let untouched = bound_body("short", 1000);
        assert_eq!(untouched, "short", "under the bound nothing changes");
    }

    #[tokio::test]
    async fn non_allowlisted_host_is_skipped_and_logged() {
        let server = ok_server("body").await;
        let url = format!("{}/page", server.uri());
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        // Allowlist a different domain so the server's loopback host is not on it.
        let (source, _fake) = web_source(&url, vec!["docs.rs".to_string()], true, audit.clone());

        let gathered = source.gather("q", 3).await.unwrap();
        assert!(
            gathered.evidence.is_empty(),
            "a non-allowlisted host is not fetched"
        );
        assert_eq!(
            gathered.account.policy_skipped, 1,
            "the policy skip is countable in the retrieval account"
        );
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "no outbound request reached the host"
        );
        let log = std::fs::read_to_string(&audit).unwrap();
        assert!(log.contains("decision=skipped"));
    }

    #[tokio::test]
    async fn allowlisted_host_redirect_is_not_followed_and_is_audited() {
        // An allowlisted host that 302s to another location must not be followed
        // (that target host was never allowlisted), and the redirect is audited.
        use wiremock::matchers::path;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "https://evil.example/x"),
            )
            .mount(&server)
            .await;
        let url = format!("{}/page", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let (source, _fake) = web_source(&url, vec![host], true, audit.clone());

        let gathered = source.gather("q", 3).await.unwrap();
        assert!(
            gathered.evidence.is_empty(),
            "a redirect yields no evidence"
        );
        assert_eq!(
            gathered.account.redirected, 1,
            "the unfollowed redirect is countable in the retrieval account"
        );
        // The allowlisted host was requested once; the redirect target was not.
        let hits = server.received_requests().await.unwrap();
        assert_eq!(hits.len(), 1, "only the allowlisted host is contacted");
        let log = std::fs::read_to_string(&audit).unwrap();
        assert!(
            log.contains("decision=redirect-not-followed"),
            "the redirect is audited: {log}"
        );
    }

    #[tokio::test]
    async fn disclosure_names_default_on_reach_and_off_switches() {
        // Default config: web on with open-web reach. The banner must say so
        // and name both kill switches before any request could be made.
        let dir = tempfile::tempdir().unwrap();
        let config = localpilot_config::Config::default();
        let mut out = Vec::new();
        let _source = build_web_source(dir.path(), "test topic", &config, None, None, &mut out)
            .await
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("on by default"), "states the posture: {text}");
        assert!(text.contains("--no-web"), "names the run switch: {text}");
        assert!(
            text.contains("[research.web].enabled = false"),
            "names the config kill switch: {text}"
        );
        assert!(text.contains("open web"), "states the reach: {text}");
        assert!(
            text.contains("egress-audit.log"),
            "names the audit destination: {text}"
        );
    }

    #[tokio::test]
    async fn disclosure_warns_on_explicitly_empty_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = localpilot_config::Config::default();
        config.research.web.allowlist.clear();
        let mut out = Vec::new();
        let _source = build_web_source(dir.path(), "test topic", &config, None, None, &mut out)
            .await
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("explicitly empty"),
            "an explicit empty allowlist is a deliberate restriction and the \
             banner says nothing will be fetched: {text}"
        );
    }

    #[tokio::test]
    async fn disclosure_states_disabled_when_config_off() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = localpilot_config::Config::default();
        config.research.web.enabled = false;
        let mut out = Vec::new();
        let _source = build_web_source(dir.path(), "test topic", &config, None, None, &mut out)
            .await
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("web research stays disabled"),
            "config-off is disclosed loudly: {text}"
        );
    }

    /// A proposer over a scripted transport whose `tools/call` returns `text`.
    fn scripted_proposer(text: &str) -> McpSearchProposer {
        let transport = Arc::new(localpilot_mcp::ScriptedTransport::new().with(
            "tools/call",
            serde_json::json!({ "content": [{ "type": "text", "text": text }] }),
        ));
        McpSearchProposer {
            tools: vec![DesignatedSearchTool {
                label: "fixture.search".to_string(),
                tool: "search".to_string(),
                transport,
            }],
        }
    }

    #[tokio::test]
    async fn mcp_proposed_urls_are_fetched_and_audited_without_a_model() {
        // Real search needs no model: the designated tool proposes, the gated
        // fetch path does the rest — and both the search call and the fetch
        // are audited.
        let server = ok_server("search-found body").await;
        let url = format!("{}/page", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let mut access = WebAccess::new(true, vec![host], Vec::new());
        access.grant_session();
        let source = WebSource::new(
            "test topic",
            localpilot_config::RenderMode::Auto,
            access,
            audit.clone(),
            None,
            Some(scripted_proposer(&format!("Result\n   URL: {url}\n"))),
            None,
        )
        .unwrap();

        let evidence = source
            .gather("how do skin matrices work", 3)
            .await
            .unwrap()
            .evidence;
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].provenance.source, "web");
        assert!(evidence[0].snippet.contains("search-found body"));

        let log = std::fs::read_to_string(&audit).unwrap();
        assert!(
            log.contains("decision=search host=fixture.search url=mcp://fixture.search"),
            "the search call itself is audited egress: {log}"
        );
        assert!(
            log.contains("decision=allowed"),
            "the fetch is audited: {log}"
        );
    }

    #[tokio::test]
    async fn mcp_proposed_disallowlisted_url_is_skipped() {
        let server = ok_server("body").await;
        let url = format!("{}/page", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        // Open web, but the fixture's host is disallowlisted: deny wins even
        // for a search-proposed URL.
        let mut access = WebAccess::new(true, vec!["*".to_string()], vec![host]);
        access.grant_session();
        let source = WebSource::new(
            "test topic",
            localpilot_config::RenderMode::Auto,
            access,
            audit.clone(),
            None,
            Some(scripted_proposer(&format!("URL: {url}"))),
            None,
        )
        .unwrap();

        let evidence = source.gather("q", 3).await.unwrap().evidence;
        assert!(
            evidence.is_empty(),
            "a disallowlisted host is never fetched"
        );
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "no outbound request reached the disallowlisted host"
        );
    }

    #[tokio::test]
    async fn erroring_search_tool_never_fails_the_run() {
        // No scripted `tools/call` response: the call errors. The run
        // continues (empty round) and the failure is audited.
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let transport = Arc::new(localpilot_mcp::ScriptedTransport::new());
        let proposer = McpSearchProposer {
            tools: vec![DesignatedSearchTool {
                label: "broken.search".to_string(),
                tool: "search".to_string(),
                transport,
            }],
        };
        let mut access = WebAccess::new(true, vec!["*".to_string()], Vec::new());
        access.grant_session();
        let source = WebSource::new(
            "test topic",
            localpilot_config::RenderMode::Auto,
            access,
            audit.clone(),
            None,
            Some(proposer),
            None,
        )
        .unwrap();

        let evidence = source.gather("q", 3).await.unwrap().evidence;
        assert!(evidence.is_empty());
        let log = std::fs::read_to_string(&audit).unwrap();
        assert!(
            log.contains("decision=search-error"),
            "the failed search call is audited: {log}"
        );
    }

    #[tokio::test]
    async fn search_and_model_proposals_merge_and_dedup() {
        // The search tool and the model propose overlapping URLs; the fetch
        // loop sees each once, search proposals first.
        let server = ok_server("body").await;
        let url = format!("{}/page", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let mut access = WebAccess::new(true, vec![host], Vec::new());
        access.grant_session();
        let fake = Arc::new(FakeProvider::new().text(&url));
        let source = WebSource::new(
            "test topic",
            localpilot_config::RenderMode::Auto,
            access,
            audit.clone(),
            Some(model_handle(Arc::clone(&fake))),
            Some(scripted_proposer(&format!("URL: {url}"))),
            None,
        )
        .unwrap();

        let evidence = source.gather("q", 1).await.unwrap().evidence;
        assert_eq!(evidence.len(), 1, "the duplicate URL is fetched once");
        let hits = server.received_requests().await.unwrap();
        assert_eq!(hits.len(), 1, "one outbound request for the deduped URL");
    }

    #[tokio::test]
    async fn designated_search_turns_an_open_question_into_coverage() {
        // The field scenario that motivated real search: with no model and no
        // search tool the web source proposes nothing and the question stays
        // open; the same run with a designated search tool ends covered, with
        // web-backed findings whose pages actually match the question.
        // Two distinct hosts (independent origins), each answering the
        // question with its own content.
        let page_a = "<html><body><h1>AnimationMixer</h1>\
             <p>The animation mixer blends clips into weighted actions before \
             skinning applies them.</p></body></html>";
        let page_b = "<html><body><h1>Skinning</h1>\
             <p>Skinning binds mixer-driven clips to bone transforms so the \
             animation deforms the mesh.</p></body></html>";
        let server_a = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(page_a.as_bytes(), "text/html"))
            .mount(&server_a)
            .await;
        let server_b = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(page_b.as_bytes(), "text/html"))
            .mount(&server_b)
            .await;
        let url_a = format!("{}/manual/mixer", server_a.uri());
        let url_b = format!("{}/docs/skinning", server_b.uri());
        let question = "how does the animation mixer blend clips for skinning";

        async fn run(
            search: Option<McpSearchProposer>,
            audit: PathBuf,
            question: &str,
        ) -> localpilot_research::RunOutcome {
            let mut access = WebAccess::new(true, vec!["*".to_string()], Vec::new());
            access.grant_session();
            let source = WebSource::new(
                "test topic",
                localpilot_config::RenderMode::Auto,
                access,
                audit,
                None,
                search,
                None,
            )
            .unwrap();
            let sources = SourceSet::new().with(Box::new(source));
            localpilot_research::run_research(
                question,
                &sources,
                &HeuristicSynthesizer,
                localpilot_research::Bounds::default(),
            )
            .await
            .unwrap()
        }

        let dir = tempfile::tempdir().unwrap();
        let baseline = run(None, dir.path().join("a.log"), question).await;
        assert_eq!(
            baseline.report.open_questions.len(),
            1,
            "no model, no search: the question stays open"
        );

        let proposer = scripted_proposer(&format!("URL: {url_a}\nURL: {url_b}"));
        let full = run(Some(proposer), dir.path().join("b.log"), question).await;
        assert!(
            full.report.open_questions.is_empty(),
            "search-proposed pages close the question: {:?}",
            full.report.coverage
        );
        assert_eq!(
            full.report.coverage[0].verdict,
            localpilot_research::CoverageVerdict::Covered,
            "{:?}",
            full.report.coverage
        );
        assert!(full
            .report
            .findings
            .iter()
            .all(|f| f.supporting.iter().any(|p| p.source == "web")));
    }

    #[tokio::test]
    async fn search_tools_receive_only_the_redacted_query() {
        // The designated-search egress carries the same redaction contract as
        // every other outbound research byte: a planted secret in the
        // sub-question must never reach the MCP server or the audit log.
        struct CapturingTransport {
            calls: std::sync::Mutex<Vec<serde_json::Value>>,
        }
        #[async_trait]
        impl localpilot_mcp::Transport for CapturingTransport {
            async fn call(
                &self,
                _method: &str,
                params: serde_json::Value,
            ) -> Result<serde_json::Value, localpilot_mcp::McpError> {
                if let Ok(mut calls) = self.calls.lock() {
                    calls.push(params);
                }
                Ok(serde_json::json!({ "content": [] }))
            }
        }
        let transport = Arc::new(CapturingTransport {
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let proposer = McpSearchProposer {
            tools: vec![DesignatedSearchTool {
                label: "capture.search".to_string(),
                tool: "search".to_string(),
                transport: Arc::clone(&transport) as Arc<dyn localpilot_mcp::Transport>,
            }],
        };
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let mut access = WebAccess::new(true, vec!["*".to_string()], Vec::new());
        access.grant_session();
        let source = WebSource::new(
            "test topic",
            localpilot_config::RenderMode::Auto,
            access,
            audit.clone(),
            None,
            Some(proposer),
            None,
        )
        .unwrap();

        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123";
        let question = format!("how do I rotate {secret} safely");
        let _ = source.gather(&question, 2).await.unwrap();

        let calls = transport.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "the designated tool was consulted");
        let sent = calls[0].to_string();
        assert!(
            !sent.contains(secret),
            "the secret must never reach the search server: {sent}"
        );
        assert!(
            sent.contains("[REDACTED]"),
            "the query is redacted before it leaves the machine: {sent}"
        );
        let log = std::fs::read_to_string(&audit).unwrap();
        assert!(!log.contains(secret), "the audit log stays redacted: {log}");
    }

    #[tokio::test]
    async fn server_error_cools_the_host_for_the_rest_of_the_run() {
        // A 500 is a host-level signal: the errored URL yields nothing and
        // every later URL on that host is skipped and audited as cooled-down.
        use wiremock::matchers::path;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/a"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/b"))
            .respond_with(ResponseTemplate::new(200).set_body_string("fine"))
            .mount(&server)
            .await;
        let url_a = format!("{}/a", server.uri());
        let url_b = format!("{}/b", server.uri());
        let host = parse_host(&url_a).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let mut access = WebAccess::new(true, vec![host], Vec::new());
        access.grant_session();
        let fake = Arc::new(FakeProvider::new().text(&format!("{url_a}\n{url_b}")));
        let source = WebSource::new(
            "test topic",
            localpilot_config::RenderMode::Auto,
            access,
            audit.clone(),
            Some(model_handle(Arc::clone(&fake))),
            None,
            None,
        )
        .unwrap();

        let gathered = source.gather("q", 3).await.unwrap();
        assert!(
            gathered.evidence.is_empty(),
            "the 500 yields nothing and the follow-up URL is skipped"
        );
        assert_eq!(gathered.account.failed, 1, "the 500 is a counted failure");
        assert_eq!(
            gathered.account.policy_skipped, 1,
            "the cooled-down skip is a counted policy outcome"
        );
        let hits = server.received_requests().await.unwrap();
        assert_eq!(hits.len(), 1, "only the first URL reached the host");
        let log = std::fs::read_to_string(&audit).unwrap();
        assert!(
            log.contains("decision=host-cooldown"),
            "the cooled-down skip is audited: {log}"
        );
    }

    #[tokio::test]
    async fn web_disabled_makes_no_request_and_no_audit() {
        let server = ok_server("body").await;
        let url = format!("{}/page", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        // Config-off: grant_session is a no-op, so the source is inert.
        let (source, fake) = web_source(&url, vec![host], false, audit.clone());

        let evidence = source.gather("q", 3).await.unwrap().evidence;
        assert!(evidence.is_empty());
        assert!(
            !audit.exists(),
            "no audit file is created when web is disabled"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
        assert_eq!(
            fake.requests().len(),
            0,
            "the model is not even asked to propose URLs when web is disabled"
        );
    }

    #[tokio::test]
    async fn planted_secret_never_egresses() {
        let server = ok_server("ok").await;
        let url = format!("{}/page", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let (source, fake) = web_source(&url, vec![host], true, audit.clone());

        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123";
        let question = format!("how do I authenticate with {secret} today");
        let _ = source.gather(&question, 2).await.unwrap();

        // The model was asked, but with the redacted query — not the secret.
        let model_requests = format!("{:?}", fake.requests());
        assert!(!model_requests.is_empty());
        assert!(
            !model_requests.contains(secret),
            "the secret must never reach the model"
        );
        assert!(
            model_requests.contains("[REDACTED]"),
            "the sub-question is redacted before the model call"
        );

        // No outbound HTTP request carried the secret.
        for request in server.received_requests().await.unwrap() {
            assert!(!request.url.as_str().contains(secret));
            assert!(!String::from_utf8_lossy(&request.body).contains(secret));
        }

        // The audit log persisted only the redacted query.
        let log = std::fs::read_to_string(&audit).unwrap();
        assert!(!log.contains(secret));
        assert!(log.contains("[REDACTED]"));
    }
}
