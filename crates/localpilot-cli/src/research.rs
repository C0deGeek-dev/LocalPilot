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
    candidates_from, evidence_block, html_to_markdown, prepare_query, render_markdown,
    run_research_controlled, term_overlap_relevance, AuditEntry, Bounds, CoverageVerdict, Evidence,
    FetchDecision, Finding, HeuristicSynthesizer, Provenance, ResearchError, ResearchReport,
    RunControl, Source, SourceError, SourceSet, Synthesizer, WebAccess,
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
         sub-questions that together cover it. Output one sub-question per line, \
         with no numbering, bullets, or commentary.\n\nTopic: {topic}"
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

    let web = web_override.unwrap_or(true);
    let mut sources = build_local_sources(root);
    if web {
        let web_source = build_web_source(root, &config, model.clone(), out).await?;
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
    let outcome = loop {
        tokio::select! {
            result = &mut run => break result?,
            Some(summary) = progress_rx.recv() => write_round_line(out, &summary)?,
        }
    };
    // The run holds no sender anymore; drain whatever raced the completion.
    while let Ok(summary) = progress_rx.try_recv() {
        write_round_line(out, &summary)?;
    }

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
    let weak = count_verdict(&outcome.report, CoverageVerdict::Weak);
    writeln!(
        out,
        "findings: {}  coverage: {covered} covered, {weak} weak, {} open  rounds: {}",
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

/// Assemble the local source set: ingested knowledge + accepted memory.
fn build_local_sources(root: &Path) -> SourceSet {
    SourceSet::new()
        .with(Box::new(KnowledgeSource {
            root: root.to_path_buf(),
        }))
        .with(Box::new(MemorySource {
            root: root.to_path_buf(),
        }))
}

struct KnowledgeSource {
    root: PathBuf,
}

#[async_trait]
impl Source for KnowledgeSource {
    fn label(&self) -> &str {
        "knowledge"
    }
    async fn gather(&self, question: &str, limit: usize) -> Result<Vec<Evidence>, SourceError> {
        let hits = localpilot_localmind::knowledge_search(&self.root, question)
            .map_err(|error| SourceError::new("knowledge", error.to_string()))?;
        // Normalize relative to this query's best hit: raw bm25-derived units
        // are corpus-dependent (IDF collapses on a degenerate corpus), so the
        // absolute values cannot meet the engine's admission floor honestly —
        // the strongest local hit of a query reads as fully relevant and the
        // rest proportionally, preserving the index's internal order.
        let max_relevance = hits
            .iter()
            .take(limit)
            .map(|hit| hit.relevance)
            .fold(0.0_f32, f32::max);
        Ok(hits
            .into_iter()
            .take(limit)
            .map(|hit| map_knowledge_hit(question, &hit, max_relevance))
            .collect())
    }
}

fn map_knowledge_hit(
    question: &str,
    hit: &localpilot_localmind::KnowledgeHit,
    max_relevance: f32,
) -> Evidence {
    // Unreviewed, machine-scored: carries the hit's relevance relative to the
    // query's best hit, so a weak match reads as weak without the raw scale's
    // corpus dependence.
    let relevance = if max_relevance > 0.0 {
        (hit.relevance / max_relevance).clamp(0.0, 1.0)
    } else {
        0.0
    };
    Evidence::new(
        question,
        hit.snippet.clone(),
        Provenance::new(
            "knowledge",
            Some(format!("{}:{}-{}", hit.path, hit.start_line, hit.end_line)),
        ),
        relevance,
    )
}

struct MemorySource {
    root: PathBuf,
}

#[async_trait]
impl Source for MemorySource {
    fn label(&self) -> &str {
        "memory"
    }
    async fn gather(&self, question: &str, limit: usize) -> Result<Vec<Evidence>, SourceError> {
        let hits = localpilot_localmind::search_readonly(&self.root, question)
            .map_err(|error| SourceError::new("memory", error.to_string()))?;
        Ok(hits
            .into_iter()
            .take(limit)
            .map(|hit| map_memory_hit(question, &hit))
            .collect())
    }
}

fn map_memory_hit(question: &str, hit: &localpilot_localmind::SearchHit) -> Evidence {
    Evidence::new(
        question,
        hit.snippet.clone(),
        Provenance::new("memory", Some(hit.memory_id.clone())),
        MEMORY_EVIDENCE_RELEVANCE,
    )
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
    config: &Config,
    model: Option<ModelHandle>,
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
    // Model-backed relevance admission over fetched pages: reuse-only model
    // resolution (LocalMind [inference] chat first, the host's default
    // provider second). Absent both, the deterministic term-overlap path and
    // the engine's admission floor govern.
    let admission = AdmissionJudge::resolve(root, model.as_ref());
    if admission.is_none() {
        writeln!(
            out,
            "  no model available for relevance admission — deterministic term-overlap              scoring with the admission floor applies"
        )?;
    }
    WebSource::new(access, audit_log, model, search, admission)
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
    client: reqwest::Client,
    access: WebAccess,
    audit_log: PathBuf,
    model: Option<ModelHandle>,
    search: Option<McpSearchProposer>,
    politeness: std::sync::Mutex<HostPoliteness>,
    /// Model-backed relevance admission for fetched pages, when any already-
    /// configured model resolves (LocalMind `[inference]` chat first, the
    /// host's default provider as fallback). `None` degrades to the
    /// deterministic term-overlap score plus the engine's admission floor.
    admission: Option<AdmissionJudge>,
}

/// Bound on page content sent to the admission classifier — enough to judge
/// relevance, far below the full evidence bound.
const ADMISSION_CONTENT_CHARS: usize = 4_000;

/// The model score a page must reach to enter the evidence pool — the same
/// bar the engine applies deterministically, so the model path and the
/// fallback share one threshold.
const ADMISSION_MIN_SCORE: f32 = localpilot_research::COVERAGE_RELEVANCE_FLOOR;

/// The strict-JSON classification a fetched page must clear to enter the
/// evidence pool when a model judge is available.
#[derive(Debug, Clone, Copy, PartialEq)]
struct AdmissionVerdict {
    relevant: bool,
    score: f32,
}

/// The model that classifies fetched web content against a sub-question. It
/// judges relevance only — it never authors, rewrites, or summarizes the
/// finding. Reuses existing configuration exclusively (no research-specific
/// model setting): LocalMind's `[inference]` chat model when configured with
/// the research feature enabled, else the host's resolved default provider.
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

    /// Classify bounded page content against the sub-question. `None` when
    /// the model is unavailable or its output is not the agreed strict JSON —
    /// the caller keeps the deterministic path rather than guessing.
    async fn classify(&self, question: &str, content: &str) -> Option<AdmissionVerdict> {
        let bounded: String = content.chars().take(ADMISSION_CONTENT_CHARS).collect();
        let instruction = format!(
            "Classify whether the fetched web content below actually helps answer the \
             research sub-question. Judge relevance only; do not rewrite or summarize the \
             content. Return ONLY this JSON object and nothing else: \
             {{\"relevant\": true|false, \"score\": <0.0-1.0>, \"reason\": \"<short>\"}}\n\n\
             Sub-question: {question}\n\nContent:\n{bounded}"
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
/// around exactly one JSON object; anything else is unusable (`None`).
fn parse_admission(reply: &str) -> Option<AdmissionVerdict> {
    #[derive(serde::Deserialize)]
    struct Raw {
        relevant: bool,
        score: f32,
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
    })
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
        access: WebAccess,
        audit_log: PathBuf,
        model: Option<ModelHandle>,
        search: Option<McpSearchProposer>,
        admission: Option<AdmissionJudge>,
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
            client,
            access,
            audit_log,
            model,
            search,
            politeness: std::sync::Mutex::new(HostPoliteness::default()),
            admission,
        })
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
        let Some(verdict) = judge.classify(&found.question, &found.snippet).await else {
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

    /// Fetch one allowlisted `url`, auditing the outbound request first. Returns
    /// evidence on a success status, `None` otherwise (the request still
    /// happened, so it is still audited).
    async fn fetch(
        &self,
        url: &str,
        host: &str,
        question: &str,
        query: &str,
    ) -> Result<Option<Evidence>, SourceError> {
        // A host that rate-limited or errored earlier in the run stays cooled
        // down — 429/5xx are host-level signals, not per-URL ones.
        if self.host_cooled(host) {
            append_audit(
                &self.audit_log,
                &audit_entry(url, host, "host-cooldown", query),
            )?;
            return Ok(None);
        }
        // Pace repeat visits: the delay adapts to the host's own last
        // response time, clamped to a sane window.
        if let Some(pause) = self.pause_before(host) {
            tokio::time::sleep(pause).await;
        }
        append_audit(&self.audit_log, &audit_entry(url, host, "allowed", query))?;
        let fetch_started = std::time::Instant::now();
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| SourceError::new("web", format!("fetch failed: {error}")))?;
        let status = response.status();
        let cool_down = status.as_u16() == 429 || status.is_server_error();
        self.record_fetch(host, fetch_started.elapsed(), cool_down);
        if cool_down {
            return Ok(None);
        }
        // A redirect is never followed (the target host is unvetted); audit and
        // skip it so it can't become an un-allowlisted egress channel.
        if status.is_redirection() {
            append_audit(
                &self.audit_log,
                &audit_entry(url, host, "redirect-not-followed", query),
            )?;
            return Ok(None);
        }
        // Capture the content type before `text()` consumes the response, so a
        // fetched HTML page can be reduced to readable prose below.
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let body = response
            .text()
            .await
            .map_err(|error| SourceError::new("web", format!("read body failed: {error}")))?;
        if !status.is_success() {
            return Ok(None);
        }
        // An HTML document becomes evidence as readable Markdown, not raw
        // markup: otherwise script/style bodies and tags leak into the finding
        // and its evidence block as junk, and the length budget is spent on
        // chrome rather than content. Markdown (rather than flat text) keeps
        // the page's headings, links, lists, and code blocks readable for the
        // reviewer and the model alike. Non-HTML bodies (plain text, Markdown,
        // JSON) are kept verbatim.
        let text = if is_html(&content_type, &body) {
            html_to_markdown(&body)
        } else {
            body
        };
        let snippet = bound_body(&text, WEB_MAX_BODY_BYTES);
        // Scored against the kept content, not a flat constant: a page that
        // barely mentions the question's terms reads as weak evidence and
        // stays below the coverage floor (the term-coverage rule applied to
        // fetched pages).
        let relevance = term_overlap_relevance(question, &snippet);
        Ok(Some(Evidence::new(
            question,
            snippet,
            Provenance::new("web", Some(url.to_string())),
            relevance,
        )))
    }
}

#[async_trait]
impl Source for WebSource {
    fn label(&self) -> &str {
        "web"
    }

    async fn gather(&self, question: &str, limit: usize) -> Result<Vec<Evidence>, SourceError> {
        // Fail-closed: with no active consent, do nothing — not even propose
        // URLs (which would touch the model or a search server). This is the
        // `Disabled` path.
        if !self.access.is_active() {
            return Ok(Vec::new());
        }
        // Only the redacted sub-question leaves the machine — never evidence.
        let query = prepare_query(localpilot_config::redact::redact, question);
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
            let Some(host) = parse_host(&url) else {
                continue;
            };
            match self.access.decide_host(&host) {
                FetchDecision::Allowed => {
                    if let Some(found) = self.fetch(&url, &host, question, &query).await? {
                        if let Some(admitted) = self.admit(&url, &host, &query, found).await? {
                            evidence.push(admitted);
                        }
                    }
                }
                FetchDecision::NeedsConfirmation => {
                    append_audit(
                        &self.audit_log,
                        &audit_entry(&url, &host, "skipped", &query),
                    )?;
                }
                FetchDecision::Disabled => return Ok(evidence),
            }
        }
        Ok(evidence)
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
        let body = format!(
            "{}\n\n(research finding; sources: {})",
            spec.body,
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
    fn knowledge_hit_maps_to_path_line_provenance() {
        let evidence = map_knowledge_hit("how", &knowledge_hit(), 1.0);
        assert_eq!(evidence.snippet, "fn foo() {}");
        assert_eq!(evidence.provenance.source, "knowledge");
        assert_eq!(
            evidence.provenance.locator.as_deref(),
            Some("src/lib.rs:4-9")
        );
    }

    #[test]
    fn memory_hit_maps_to_id_provenance() {
        let evidence = map_memory_hit("how", &memory_hit());
        assert_eq!(evidence.provenance.source, "memory");
        assert_eq!(evidence.provenance.locator.as_deref(), Some("mem_7"));
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
                score: 0.8
            })
        );
        // Prose around one JSON object is tolerated…
        assert_eq!(
            parse_admission("Sure: {\"relevant\": false, \"score\": 0.1} done"),
            Some(AdmissionVerdict {
                relevant: false,
                score: 0.1
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
                score: 1.0
            })
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

        let evidence = source.gather("how to use tokio", 3).await.unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].provenance.source, "web");
        assert_eq!(
            evidence[0].provenance.locator.as_deref(),
            Some(url.as_str())
        );
        assert!(evidence[0].snippet.contains("documentation body"));

        let log = std::fs::read_to_string(&audit).unwrap();
        assert_eq!(log.lines().count(), 1, "one audited request");
        assert!(log.contains("decision=allowed"));
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

        let evidence = source.gather("what is tokio", 3).await.unwrap();
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

        let evidence = source.gather("q", 3).await.unwrap();
        assert!(evidence.is_empty(), "a non-allowlisted host is not fetched");
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

        let evidence = source.gather("q", 3).await.unwrap();
        assert!(evidence.is_empty(), "a redirect yields no evidence");
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
        let _source = build_web_source(dir.path(), &config, None, &mut out)
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
        let _source = build_web_source(dir.path(), &config, None, &mut out)
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
        let _source = build_web_source(dir.path(), &config, None, &mut out)
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
            access,
            audit.clone(),
            None,
            Some(scripted_proposer(&format!("Result\n   URL: {url}\n"))),
            None,
        )
        .unwrap();

        let evidence = source.gather("how do skin matrices work", 3).await.unwrap();
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
            access,
            audit.clone(),
            None,
            Some(scripted_proposer(&format!("URL: {url}"))),
            None,
        )
        .unwrap();

        let evidence = source.gather("q", 3).await.unwrap();
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
        let source = WebSource::new(access, audit.clone(), None, Some(proposer), None).unwrap();

        let evidence = source.gather("q", 3).await.unwrap();
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
            access,
            audit.clone(),
            Some(model_handle(Arc::clone(&fake))),
            Some(scripted_proposer(&format!("URL: {url}"))),
            None,
        )
        .unwrap();

        let evidence = source.gather("q", 1).await.unwrap();
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
            let source = WebSource::new(access, audit, None, search, None).unwrap();
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
        let source = WebSource::new(access, audit.clone(), None, Some(proposer), None).unwrap();

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
            access,
            audit.clone(),
            Some(model_handle(Arc::clone(&fake))),
            None,
            None,
        )
        .unwrap();

        let evidence = source.gather("q", 3).await.unwrap();
        assert!(
            evidence.is_empty(),
            "the 500 yields nothing and the follow-up URL is skipped"
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

        let evidence = source.gather("q", 3).await.unwrap();
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
