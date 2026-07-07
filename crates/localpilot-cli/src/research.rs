//! Binding layer for the `/research` mode and `localpilot research` subcommand.
//!
//! The host-neutral loop lives in `localpilot-research`; this module supplies
//! the concrete local [`Source`]s over LocalPilot's retrieval primitives and
//! the run orchestrator that renders a report artefact and enqueues
//! review-gated memory candidates. Web research is added separately and stays
//! off by default (`policies/remote-egress.md`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use localpilot_config::{CliOverrides, Config, ConfigPaths};
use localpilot_core::{Message, Role};
use localpilot_llm::{ModelEvent, ModelProvider, ModelRequest, ProviderRegistry};
use localpilot_research::{
    candidates_from, prepare_query, render_markdown, run_research, AuditEntry, Bounds, Evidence,
    FetchDecision, Finding, HeuristicSynthesizer, Provenance, ResearchError, ResearchReport,
    Source, SourceError, SourceSet, Synthesizer, WebAccess,
};

/// Confidence attached to research-derived memory candidates: low, because they
/// are machine-derived and unreviewed — they route to review, never accepted.
const RESEARCH_CANDIDATE_CONFIDENCE: f32 = 0.3;

/// Evidence snippets to take from each source per sub-question.
const PER_SOURCE_EVIDENCE: usize = 5;

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
    /// Directory the report artefact is written to.
    pub output_dir: PathBuf,
    /// Whether to write the report artefact.
    pub write_report: bool,
    /// Whether to enqueue review-gated memory candidates.
    pub enqueue_memory: bool,
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
        output_dir,
        write_report,
        enqueue_memory,
    }))
}

/// Run a **local-only** research pass for `topic`. The interactive surface uses
/// this: web research is never reached here (it is a headless `--web` opt-in).
#[cfg(feature = "tui")]
pub async fn run_local_research(
    root: &Path,
    topic: &str,
    options: &ResearchOptions,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    run_research_command(root, topic, options, false, out).await
}

/// Run a research pass for `topic`, gathering across local sources and — only
/// when `web` is requested *and* `[research.web]` permits it — the consented,
/// allowlisted web source. Synthesises with the model-assisted (decomposition)
/// synthesizer, then (per options) writes a report artefact and enqueues
/// review-gated memory candidates. A short human summary is written to `out`.
///
/// With `web` true the egress disclosure is printed and per-session consent is
/// recorded before any request; the source stays inert when config disables web,
/// so `--web` can never override `[research.web].enabled = false`.
pub async fn run_research_command(
    root: &Path,
    topic: &str,
    options: &ResearchOptions,
    web: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let config = localpilot_config::load(&ConfigPaths::standard(root), &CliOverrides::default())?;
    let model = ModelHandle::from_config(&config);

    let mut sources = build_local_sources(root);
    if web {
        let web_source = build_web_source(root, &config, model.clone(), out)?;
        sources.push(Box::new(web_source));
    }
    let synth = CliSynthesizer { model };

    let bounds = Bounds {
        max_questions: options.max_questions,
        per_source_evidence: PER_SOURCE_EVIDENCE,
    };
    let outcome = run_research(topic, &sources, &synth, bounds).await?;

    for error in &outcome.source_errors {
        writeln!(out, "note: {error}")?;
    }
    if options.write_report {
        let path = write_report(&options.output_dir, topic, &outcome.report)?;
        writeln!(out, "report: {}", path.display())?;
    }
    if options.enqueue_memory {
        let enqueued = enqueue_candidates(root, &outcome.report)?;
        writeln!(out, "memory candidates enqueued for review: {enqueued}")?;
    }
    writeln!(
        out,
        "findings: {}  open questions: {}",
        outcome.report.findings.len(),
        outcome.report.open_questions.len()
    )?;
    Ok(())
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
        Ok(hits
            .into_iter()
            .take(limit)
            .map(|hit| map_knowledge_hit(question, &hit))
            .collect())
    }
}

fn map_knowledge_hit(question: &str, hit: &localpilot_localmind::KnowledgeHit) -> Evidence {
    Evidence {
        question: question.to_string(),
        snippet: hit.snippet.clone(),
        provenance: Provenance::new(
            "knowledge",
            Some(format!("{}:{}-{}", hit.path, hit.start_line, hit.end_line)),
        ),
    }
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
    Evidence {
        question: question.to_string(),
        snippet: hit.snippet.clone(),
        provenance: Provenance::new("memory", Some(hit.memory_id.clone())),
    }
}

// --- web source (off by default; `policies/remote-egress.md`) ----------------

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
/// is a no-op against config-off — so `--web` can never override the config
/// kill switch. With an empty allowlist every host needs confirmation, which in
/// v1 means skipped, so the disclosure warns that nothing will be fetched.
fn build_web_source(
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

    writeln!(out, "web research opt-in (egress disclosure):")?;
    writeln!(
        out,
        "  sent off-machine: only the redacted sub-question text \
         — never file contents or gathered evidence"
    )?;
    if web_config.enabled {
        if web_config.allowlist.is_empty() {
            writeln!(
                out,
                "  allowlist: empty — every host requires confirmation, \
                 so nothing will be fetched this run"
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

    access.grant_session();
    WebSource::new(access, audit_log, model)
}

/// A networked evidence source, constructed only when the operator opts in.
///
/// For each sub-question it asks the model to propose candidate URLs, parses
/// each URL's host with a real parser, and consults the [`WebAccess`] gate:
/// allowlisted hosts are fetched and audited; every other host is skipped and
/// logged (v1 is allowlist-only — no interactive per-fetch confirm). Only the
/// redacted sub-question is ever sent off-machine.
struct WebSource {
    client: reqwest::Client,
    access: WebAccess,
    audit_log: PathBuf,
    model: Option<ModelHandle>,
}

impl WebSource {
    fn new(
        access: WebAccess,
        audit_log: PathBuf,
        model: Option<ModelHandle>,
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
        })
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
        append_audit(&self.audit_log, &audit_entry(url, host, "allowed", query))?;
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| SourceError::new("web", format!("fetch failed: {error}")))?;
        let status = response.status();
        // A redirect is never followed (the target host is unvetted); audit and
        // skip it so it can't become an un-allowlisted egress channel.
        if status.is_redirection() {
            append_audit(
                &self.audit_log,
                &audit_entry(url, host, "redirect-not-followed", query),
            )?;
            return Ok(None);
        }
        let body = response
            .text()
            .await
            .map_err(|error| SourceError::new("web", format!("read body failed: {error}")))?;
        if !status.is_success() {
            return Ok(None);
        }
        Ok(Some(Evidence {
            question: question.to_string(),
            snippet: bound_body(&body, WEB_MAX_BODY_BYTES),
            provenance: Provenance::new("web", Some(url.to_string())),
        }))
    }
}

#[async_trait]
impl Source for WebSource {
    fn label(&self) -> &str {
        "web"
    }

    async fn gather(&self, question: &str, limit: usize) -> Result<Vec<Evidence>, SourceError> {
        // Fail-closed: with no active consent, do nothing — not even propose
        // URLs (which would touch the model). This is the `Disabled` path.
        if !self.access.is_active() {
            return Ok(Vec::new());
        }
        let Some(model) = &self.model else {
            return Ok(Vec::new());
        };
        // Only the redacted sub-question leaves the machine — never evidence.
        let query = prepare_query(localpilot_config::redact::redact, question);
        let urls = self.propose_urls(model, &query, limit).await?;

        let mut evidence = Vec::new();
        for url in urls.into_iter().take(limit) {
            let Some(host) = parse_host(&url) else {
                continue;
            };
            match self.access.decide_host(&host) {
                FetchDecision::Allowed => {
                    if let Some(found) = self.fetch(&url, &host, question, &query).await? {
                        evidence.push(found);
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

/// Truncate a fetched body to at most `max_bytes`, never splitting a UTF-8 char.
fn bound_body(body: &str, max_bytes: usize) -> String {
    if body.len() <= max_bytes {
        return body.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    body[..end].to_string()
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
fn enqueue_candidates(root: &Path, report: &ResearchReport) -> anyhow::Result<usize> {
    let mut enqueued = 0;
    for spec in candidates_from(report, RESEARCH_CANDIDATE_CONFIDENCE) {
        let body = format!(
            "{}\n\n(research finding; sources: {})",
            spec.body,
            provenance_summary(&spec.provenance)
        );
        let lesson = localpilot_localmind::RetrospectiveLesson::new(
            localpilot_config::redact::redact(&body),
        );
        if localpilot_localmind::write_retrospective_lesson(root, &lesson)?.is_some() {
            enqueued += 1;
        }
    }
    Ok(enqueued)
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
        let evidence = map_knowledge_hit("how", &knowledge_hit());
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
    fn write_report_writes_rendered_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let mut report = ResearchReport::new("caching");
        report.findings = vec![Finding {
            statement: "caches speed reads".to_string(),
            status: ClaimStatus::Supported,
            supporting: vec![Provenance::new("memory", Some("mem_1".to_string()))],
            evidence: None,
        }];
        let path = write_report(dir.path(), "caching", &report).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# Research: caching"));
        assert!(body.contains("caches speed reads"));
        assert!(path.ends_with("caching.md"));
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
        let evidence = vec![Evidence {
            question: "q".to_string(),
            snippet: "caches speed reads".to_string(),
            provenance: Provenance::new("memory", Some("mem_1".to_string())),
        }];
        let findings = synth.synthesize("topic", &evidence).await.unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].statement, "caches speed reads");
        assert_eq!(findings[0].supporting.len(), 1);
        assert_eq!(findings[0].supporting[0].locator.as_deref(), Some("mem_1"));
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
        let source =
            WebSource::new(access, audit_log, Some(model_handle(Arc::clone(&fake)))).unwrap();
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
