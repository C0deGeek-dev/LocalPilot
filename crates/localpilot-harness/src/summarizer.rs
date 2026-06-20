//! Optional model-backed smart summarizer with deterministic fallback.
//!
//! The deterministic compactor in [`crate::compaction`] is always the
//! correctness baseline. This module adds an opt-in step that asks a model to
//! synthesize a richer semantic digest from the exchanges compaction trimmed —
//! but it is *untrusted until parsed, validated, budgeted, and source-grounded*.
//! Every failure mode (timeout, cancellation, provider error, output limit,
//! malformed or empty output, an over-budget digest, or a self-check rejection)
//! returns a typed [`FallbackReason`] so the caller keeps the deterministic
//! result (completed-only cutover).
//!
//! The summarizer never runs tools, never embeds raw media or full tool dumps,
//! and clamps its own output to a configured budget before and after dispatch.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use localpilot_core::{
    ContentBlock, Message, Role, StructuredSummary, SummarySection, SummarySectionKind,
    SummarySource, SummarySourceKind,
};
use localpilot_llm::{ModelEvent, ModelProvider, ModelRequest, ProviderError};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::compaction::SUMMARY_TITLE;

/// Per-item character ceiling for a smart-digest entry. An item longer than this
/// is treated as a tool-output replay attempt and rejected, not truncated.
const MAX_ITEM_CHARS: usize = 400;

/// A token (no whitespace) of at least this many base64-class characters is
/// treated as embedded media/binary and rejected.
const MAX_BASE64_RUN: usize = 120;

/// Most carried prior-summary entries to keep when folding into the new digest.
const MAX_CARRIED_ENTRIES: usize = 6;

/// Hard ceiling on summarizer attempts (input shrink + shorter-output retries)
/// so a flapping provider can never spin the loop.
const MAX_ATTEMPTS: usize = 4;

/// Budgets and timeout for one smart-summarization attempt. Built from
/// `[compaction]` config by the caller.
#[derive(Debug, Clone, Copy)]
pub struct SummarizerTuning {
    /// Output budget for the rendered digest, in estimated tokens.
    pub output_token_limit: usize,
    /// Character ceiling on the bounded input pack handed to the model.
    pub input_char_budget: usize,
    /// Wall-clock timeout for the whole attempt (open + stream).
    pub timeout: Duration,
}

impl Default for SummarizerTuning {
    fn default() -> Self {
        Self {
            output_token_limit: 1_024,
            input_char_budget: 32_768,
            timeout: Duration::from_secs(20),
        }
    }
}

impl SummarizerTuning {
    /// Map `[compaction]` configuration onto runtime budgets. The input budget
    /// is the configured input-token allowance converted to characters with the
    /// same ~4-chars-per-token heuristic the compactor uses for estimates.
    #[must_use]
    pub fn from_config(config: &localpilot_config::CompactionConfig) -> Self {
        Self {
            output_token_limit: usize::try_from(config.summary_token_limit).unwrap_or(1_024),
            input_char_budget: usize::try_from(config.summarizer_input_tokens)
                .unwrap_or(8_192)
                .saturating_mul(4),
            timeout: Duration::from_secs(config.summarizer_timeout_secs),
        }
    }
}

/// Why smart summarization did not produce a usable digest. The caller maps this
/// to a deterministic fallback; it is also surfaced in audit metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// Smart mode is off or no summarizer backend is configured.
    Disabled,
    /// Nothing was trimmed, so there is nothing to summarize.
    NothingToSummarize,
    /// The attempt exceeded its timeout.
    Timeout,
    /// The caller cancelled the turn.
    Cancelled,
    /// The provider failed (network, server, auth, decode, unknown model).
    ProviderError,
    /// The model hit its output-token limit even after a shorter retry.
    OutputLimit,
    /// The output was not valid digest JSON.
    Malformed,
    /// The output parsed but contained no usable sections.
    Empty,
    /// The validated digest still exceeded the output budget.
    OverBudget,
    /// A self-check rejected the digest (unknown file claim, media echo, or a
    /// tool-output replay).
    Unsupported,
}

impl FallbackReason {
    /// A stable, log-safe label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FallbackReason::Disabled => "smart summarizer disabled",
            FallbackReason::NothingToSummarize => "nothing trimmed to summarize",
            FallbackReason::Timeout => "smart summarizer timed out",
            FallbackReason::Cancelled => "cancelled",
            FallbackReason::ProviderError => "smart summarizer provider error",
            FallbackReason::OutputLimit => "smart summarizer hit output limit",
            FallbackReason::Malformed => "smart summary was malformed",
            FallbackReason::Empty => "smart summary was empty",
            FallbackReason::OverBudget => "smart summary exceeded budget",
            FallbackReason::Unsupported => "smart summary failed self-check",
        }
    }
}

/// A model-backed summarizer boundary. Object-safe so the session can hold an
/// `Arc<dyn Summarizer>` and swap fakes in tests.
#[async_trait]
pub trait Summarizer: Send + Sync {
    /// Synthesize a validated semantic digest from the `dropped` exchanges,
    /// folding in `carried` prior-summary entries, or return a typed fallback
    /// reason. Implementations must not run tools and must respect `tuning`'s
    /// output cap and timeout and the `cancel` token.
    async fn summarize(
        &self,
        dropped: &[Vec<Message>],
        carried: &[String],
        tuning: SummarizerTuning,
        cancel: &CancellationToken,
    ) -> Result<StructuredSummary, FallbackReason>;
}

/// The default summarizer: one bounded, tool-free request to a model provider.
pub struct ProviderSummarizer {
    provider: Arc<dyn ModelProvider>,
    model: String,
}

impl ProviderSummarizer {
    /// Build a summarizer over `provider`, requesting `model`.
    #[must_use]
    pub fn new(provider: Arc<dyn ModelProvider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
        }
    }

    /// The output budget, clamped to both the configured limit and a fraction of
    /// the provider's advertised context window.
    fn clamped_output_cap(&self, configured: usize) -> usize {
        let model_cap = self
            .provider
            .declaration()
            .max_context_tokens
            .and_then(|c| usize::try_from(c).ok())
            .map_or(usize::MAX, |c| (c / 8).max(256));
        configured.min(model_cap).max(64)
    }

    /// Run one request to completion under a timeout and cancellation, returning
    /// the assembled text or a classified failure.
    async fn run(
        &self,
        request: ModelRequest,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<String, RunError> {
        tokio::select! {
            // Cancellation takes priority: a token already tripped before the
            // request even opens must short-circuit deterministically.
            biased;
            () = cancel.cancelled() => Err(RunError::Cancelled),
            outcome = tokio::time::timeout(timeout, self.run_stream(request)) => match outcome {
                Err(_elapsed) => Err(RunError::Timeout),
                Ok(inner) => inner,
            }
        }
    }

    async fn run_stream(&self, request: ModelRequest) -> Result<String, RunError> {
        let mut stream = match self.provider.stream(request).await {
            Ok(stream) => stream,
            Err(ProviderError::InvalidRequest { .. }) => return Err(RunError::TooLong),
            Err(_) => return Err(RunError::Provider),
        };
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(ModelEvent::TextDelta(delta)) => text.push_str(&delta),
                Ok(ModelEvent::OutputLimit { .. }) => return Err(RunError::OutputLimit),
                Ok(ModelEvent::Done) => break,
                Ok(_) => {}
                Err(ProviderError::InvalidRequest { .. }) => return Err(RunError::TooLong),
                Err(_) => return Err(RunError::Provider),
            }
        }
        Ok(text)
    }
}

/// Internal classification of a single request outcome.
enum RunError {
    Timeout,
    Cancelled,
    OutputLimit,
    /// The provider rejected the request as too large (a missed local estimate).
    TooLong,
    Provider,
}

#[async_trait]
impl Summarizer for ProviderSummarizer {
    async fn summarize(
        &self,
        dropped: &[Vec<Message>],
        carried: &[String],
        tuning: SummarizerTuning,
        cancel: &CancellationToken,
    ) -> Result<StructuredSummary, FallbackReason> {
        if dropped.is_empty() {
            return Err(FallbackReason::NothingToSummarize);
        }
        let allowed = allowed_paths(dropped);
        let mut output_cap = self.clamped_output_cap(tuning.output_token_limit);
        let mut groups = dropped.len();

        for attempt in 0..MAX_ATTEMPTS {
            let messages = build_request_messages(
                dropped,
                carried,
                groups,
                tuning.input_char_budget,
                output_cap,
            );
            let request = ModelRequest::new(self.model.clone(), messages);
            match self.run(request, tuning.timeout, cancel).await {
                Ok(text) => return parse_and_validate(&text, &allowed, output_cap, carried),
                Err(RunError::Cancelled) => return Err(FallbackReason::Cancelled),
                Err(RunError::Timeout) => return Err(FallbackReason::Timeout),
                Err(RunError::Provider) => return Err(FallbackReason::ProviderError),
                Err(RunError::OutputLimit) => {
                    // One shorter retry: ask for half the output budget.
                    if output_cap > 128 && attempt + 1 < MAX_ATTEMPTS {
                        output_cap /= 2;
                        continue;
                    }
                    return Err(FallbackReason::OutputLimit);
                }
                Err(RunError::TooLong) => {
                    // Prompt-too-long: drop the oldest input groups and retry
                    // within a small cap before giving up.
                    if groups > 1 && attempt + 1 < MAX_ATTEMPTS {
                        groups = (groups / 2).max(1);
                        continue;
                    }
                    return Err(FallbackReason::ProviderError);
                }
            }
        }
        Err(FallbackReason::ProviderError)
    }
}

/// The strict output schema. Unknown section kinds and non-object shapes are a
/// parse error, which the caller maps to [`FallbackReason::Malformed`].
#[derive(Debug, Deserialize)]
struct SmartDigest {
    #[serde(default)]
    sections: Vec<SmartSection>,
}

#[derive(Debug, Deserialize)]
struct SmartSection {
    kind: SummarySectionKind,
    #[serde(default)]
    items: Vec<String>,
}

/// Build the two-message request: a system instruction pinning the JSON schema
/// and budget, then a bounded, redacted user pack of the trimmed history.
fn build_request_messages(
    dropped: &[Vec<Message>],
    carried: &[String],
    max_groups: usize,
    input_char_budget: usize,
    output_cap: usize,
) -> Vec<Message> {
    let instruction = format!(
        "You compress trimmed conversation history into a compact, faithful state \
digest. Update the prior summary: keep still-true facts, drop stale or superseded \
ones, merge new evidence, and keep unresolved next steps explicit. Only state facts \
grounded in the material below — never invent files, results, or decisions. Do not \
echo raw file contents, tool output, media, or base64. Reply with ONLY a JSON object \
of shape {{\"sections\":[{{\"kind\":<one of {kinds}>,\"items\":[\"...\"]}}]}}. \
Keep it under about {output_cap} tokens; each item is one short sentence.",
        kinds = SECTION_KINDS.join("|"),
        output_cap = output_cap,
    );

    let mut pack = String::new();
    if !carried.is_empty() {
        pack.push_str("Prior summary to update:\n");
        for entry in carried.iter().take(MAX_CARRIED_ENTRIES) {
            pack.push_str("- ");
            pack.push_str(entry);
            pack.push('\n');
        }
        pack.push('\n');
    }
    pack.push_str("Trimmed history (most recent last):\n");

    // Render the most recent `max_groups` exchanges, oldest first, stopping once
    // the character budget is reached (drop-oldest shrink happens by taking the
    // tail and by truncating here).
    let start = dropped.len().saturating_sub(max_groups.max(1));
    for (offset, exchange) in dropped[start..].iter().enumerate() {
        let rendered = render_exchange(start + offset, exchange);
        if pack.len() + rendered.len() > input_char_budget {
            pack.push_str("- [older exchanges omitted to fit the summarizer budget]\n");
            break;
        }
        pack.push_str(&rendered);
    }

    vec![
        Message::text(Role::System, instruction),
        Message::text(Role::User, pack),
    ]
}

/// Render one trimmed exchange as bounded, media-stripped previews.
fn render_exchange(index: usize, exchange: &[Message]) -> String {
    let mut out = format!("[exchange {index}]\n");
    for message in exchange {
        match message.role {
            Role::User => {
                if let Some(text) = first_text(message) {
                    out.push_str("  user: ");
                    out.push_str(&preview(text, 200));
                    out.push('\n');
                }
            }
            Role::Assistant => {
                if let Some(text) = first_text(message) {
                    if !text.trim().is_empty() {
                        out.push_str("  assistant: ");
                        out.push_str(&preview(text, 200));
                        out.push('\n');
                    }
                }
                for block in &message.content {
                    if let ContentBlock::ToolUse(call) = block {
                        out.push_str("  tool_call ");
                        out.push_str(&call.name);
                        out.push_str(": ");
                        out.push_str(&preview(&strip_media(&call.input.to_string()), 160));
                        out.push('\n');
                    }
                }
            }
            Role::Tool => {
                for block in &message.content {
                    if let ContentBlock::ToolResult(result) = block {
                        out.push_str(if result.is_error {
                            "  tool_error: "
                        } else {
                            "  tool_result: "
                        });
                        out.push_str(&preview(&strip_media(&result.output), 160));
                        out.push('\n');
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Parse the model output and run every self-check before accepting a digest.
fn parse_and_validate(
    text: &str,
    allowed: &BTreeSet<String>,
    output_cap: usize,
    carried: &[String],
) -> Result<StructuredSummary, FallbackReason> {
    let json = extract_json(text).ok_or(FallbackReason::Malformed)?;
    let parsed: SmartDigest = serde_json::from_str(&json).map_err(|_| FallbackReason::Malformed)?;

    let mut sections = Vec::new();
    for section in parsed.sections {
        let mut items = Vec::new();
        for item in section.items {
            let item = item.trim().to_string();
            if item.is_empty() {
                continue;
            }
            if item.chars().count() > MAX_ITEM_CHARS || has_base64_run(&item) {
                return Err(FallbackReason::Unsupported);
            }
            if cites_unknown_path(&item, allowed) {
                return Err(FallbackReason::Unsupported);
            }
            items.push(item);
        }
        if !items.is_empty() {
            items.truncate(8);
            sections.push(SummarySection::new(section.kind, items));
        }
    }
    if sections.is_empty() {
        return Err(FallbackReason::Empty);
    }

    let mut entries: Vec<String> = carried.iter().take(MAX_CARRIED_ENTRIES).cloned().collect();
    for section in &sections {
        for item in &section.items {
            entries.push(format!("{}: {item}", section.kind.label()));
        }
    }
    entries.truncate(16);

    let mut summary = StructuredSummary::new(SUMMARY_TITLE, entries);
    summary.sections = sections;
    summary.sources = sources(allowed, !carried.is_empty());

    if summary.render().len() / 4 > output_cap {
        return Err(FallbackReason::OverBudget);
    }
    Ok(summary)
}

/// The set of file paths the trimmed history actually referenced, used to reject
/// a digest that cites a file nothing touched.
fn allowed_paths(dropped: &[Vec<Message>]) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    for exchange in dropped {
        for message in exchange {
            if let Some(text) = first_text(message) {
                collect_paths(text, &mut paths);
            }
            for block in &message.content {
                match block {
                    ContentBlock::ToolUse(call) => {
                        collect_paths(&call.input.to_string(), &mut paths)
                    }
                    ContentBlock::ToolResult(result) => collect_paths(&result.output, &mut paths),
                    _ => {}
                }
            }
        }
    }
    paths
}

fn sources(allowed: &BTreeSet<String>, carried: bool) -> Vec<SummarySource> {
    let mut sources: Vec<SummarySource> = allowed
        .iter()
        .map(|path| SummarySource::new(SummarySourceKind::FilePath, path.clone()).with_path(path))
        .collect();
    if carried {
        sources.push(SummarySource::new(
            SummarySourceKind::PreviousSummary,
            "carried-summary",
        ));
    }
    sources
}

/// Whether `item` cites a file path that the trimmed history never referenced.
fn cites_unknown_path(item: &str, allowed: &BTreeSet<String>) -> bool {
    for token in item.split(|ch: char| {
        ch.is_whitespace() || matches!(ch, ',' | ';' | '`' | '(' | ')' | '"' | '\'')
    }) {
        if !looks_like_path(token) {
            continue;
        }
        let known = allowed
            .iter()
            .any(|path| path.contains(token) || token.contains(path.as_str()));
        if !known {
            return true;
        }
    }
    false
}

fn looks_like_path(token: &str) -> bool {
    let token = token.trim();
    if token.len() < 3 {
        return false;
    }
    token.contains('/')
        || token.contains('\\')
        || token.ends_with(".rs")
        || token.ends_with(".toml")
        || token.ends_with(".md")
        || token.ends_with(".json")
}

fn collect_paths(text: &str, paths: &mut BTreeSet<String>) {
    for token in text.split_whitespace() {
        let token = token.trim_matches(|ch: char| {
            matches!(
                ch,
                ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}'
            )
        });
        if looks_like_path(token) {
            paths.insert(token.chars().take(160).collect());
        }
    }
}

/// True when `item` contains a long unbroken run of base64-class characters,
/// the signature of an embedded media/binary blob.
fn has_base64_run(item: &str) -> bool {
    let mut run = 0usize;
    for ch in item.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '=') {
            run += 1;
            if run >= MAX_BASE64_RUN {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// Replace base64/media-looking runs with a placeholder before the material ever
/// reaches the model, so the request itself stays bounded and leak-free.
fn strip_media(text: &str) -> String {
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.chars().count() >= MAX_BASE64_RUN {
            out.push_str("[media omitted]");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '=') {
            run.push(ch);
        } else {
            flush(&mut run, &mut out);
            out.push(ch);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// Pull the first balanced-looking JSON object out of a model reply that may be
/// wrapped in prose or code fences.
fn extract_json(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(trimmed[start..=end].to_string())
}

fn first_text(message: &Message) -> Option<&str> {
    message.content.iter().find_map(|block| match block {
        ContentBlock::Text { text } => Some(text.as_str()),
        _ => None,
    })
}

fn preview(text: &str, max_chars: usize) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(max_chars).collect();
    if collapsed.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

/// The accepted section kinds, in canonical order, for the schema instruction.
const SECTION_KINDS: &[&str] = &[
    "goal",
    "constraints",
    "progress",
    "decisions",
    "next_steps",
    "critical_context",
    "relevant_files",
    "command_outcomes",
    "risks",
    "stale_or_superseded",
];

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_core::{ToolCall, ToolResult, ToolUseId};
    use localpilot_llm::FakeProvider;

    fn exchange() -> Vec<Message> {
        vec![
            Message::text(Role::User, "fix the parser in src/parse.rs"),
            Message::new(
                Role::Assistant,
                vec![ContentBlock::ToolUse(ToolCall::new(
                    ToolUseId::from("c1"),
                    "read_file",
                    serde_json::json!({ "path": "src/parse.rs" }),
                ))],
            ),
            Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult(ToolResult::success(
                    ToolUseId::from("c1"),
                    "fn parse() {}",
                ))],
            ),
        ]
    }

    fn tuning() -> SummarizerTuning {
        SummarizerTuning {
            output_token_limit: 512,
            input_char_budget: 8_192,
            timeout: Duration::from_secs(5),
        }
    }

    fn digest_json() -> String {
        serde_json::json!({
            "sections": [
                { "kind": "goal", "items": ["fix the parser in src/parse.rs"] },
                { "kind": "progress", "items": ["read src/parse.rs"] }
            ]
        })
        .to_string()
    }

    #[tokio::test]
    async fn valid_digest_is_accepted_and_grounded() {
        let provider = Arc::new(FakeProvider::new().text(&digest_json()));
        let summarizer = ProviderSummarizer::new(provider, "m");
        let cancel = CancellationToken::new();
        let summary = summarizer
            .summarize(&[exchange()], &[], tuning(), &cancel)
            .await
            .expect("valid digest");
        assert_eq!(summary.title, SUMMARY_TITLE);
        assert!(summary.entries.iter().any(|e| e.contains("parser")));
        assert!(summary
            .sources
            .iter()
            .any(|s| s.path.as_deref() == Some("src/parse.rs")));
    }

    #[tokio::test]
    async fn empty_dropped_history_is_nothing_to_summarize() {
        let provider = Arc::new(FakeProvider::new().text(&digest_json()));
        let summarizer = ProviderSummarizer::new(provider, "m");
        let cancel = CancellationToken::new();
        assert_eq!(
            summarizer.summarize(&[], &[], tuning(), &cancel).await,
            Err(FallbackReason::NothingToSummarize)
        );
    }

    #[tokio::test]
    async fn malformed_json_falls_back() {
        let provider = Arc::new(FakeProvider::new().text("not json at all"));
        let summarizer = ProviderSummarizer::new(provider, "m");
        let cancel = CancellationToken::new();
        assert_eq!(
            summarizer
                .summarize(&[exchange()], &[], tuning(), &cancel)
                .await,
            Err(FallbackReason::Malformed)
        );
    }

    #[tokio::test]
    async fn empty_sections_fall_back() {
        let provider = Arc::new(FakeProvider::new().text(r#"{"sections":[]}"#));
        let summarizer = ProviderSummarizer::new(provider, "m");
        let cancel = CancellationToken::new();
        assert_eq!(
            summarizer
                .summarize(&[exchange()], &[], tuning(), &cancel)
                .await,
            Err(FallbackReason::Empty)
        );
    }

    #[tokio::test]
    async fn hallucinated_file_is_rejected() {
        let body = serde_json::json!({
            "sections": [
                { "kind": "relevant_files", "items": ["edited src/never_touched.rs heavily"] }
            ]
        })
        .to_string();
        let provider = Arc::new(FakeProvider::new().text(&body));
        let summarizer = ProviderSummarizer::new(provider, "m");
        let cancel = CancellationToken::new();
        assert_eq!(
            summarizer
                .summarize(&[exchange()], &[], tuning(), &cancel)
                .await,
            Err(FallbackReason::Unsupported)
        );
    }

    #[tokio::test]
    async fn embedded_base64_is_rejected() {
        let blob = "A".repeat(200);
        let body = serde_json::json!({
            "sections": [ { "kind": "progress", "items": [format!("decoded {blob}")] } ]
        })
        .to_string();
        let provider = Arc::new(FakeProvider::new().text(&body));
        let summarizer = ProviderSummarizer::new(provider, "m");
        let cancel = CancellationToken::new();
        assert_eq!(
            summarizer
                .summarize(&[exchange()], &[], tuning(), &cancel)
                .await,
            Err(FallbackReason::Unsupported)
        );
    }

    #[tokio::test]
    async fn output_limit_falls_back_after_a_shorter_retry() {
        let provider = Arc::new(
            FakeProvider::new()
                .script(vec![Ok(ModelEvent::OutputLimit {
                    message: "limit".to_string(),
                })])
                .script(vec![Ok(ModelEvent::OutputLimit {
                    message: "limit".to_string(),
                })])
                .script(vec![Ok(ModelEvent::OutputLimit {
                    message: "limit".to_string(),
                })])
                .script(vec![Ok(ModelEvent::OutputLimit {
                    message: "limit".to_string(),
                })]),
        );
        let summarizer = ProviderSummarizer::new(provider, "m");
        let cancel = CancellationToken::new();
        assert_eq!(
            summarizer
                .summarize(&[exchange()], &[], tuning(), &cancel)
                .await,
            Err(FallbackReason::OutputLimit)
        );
    }

    #[tokio::test]
    async fn provider_error_falls_back() {
        let provider = Arc::new(FakeProvider::new().malformed());
        let summarizer = ProviderSummarizer::new(provider, "m");
        let cancel = CancellationToken::new();
        assert_eq!(
            summarizer
                .summarize(&[exchange()], &[], tuning(), &cancel)
                .await,
            Err(FallbackReason::ProviderError)
        );
    }

    #[tokio::test]
    async fn cancellation_falls_back() {
        let provider = Arc::new(FakeProvider::new().text(&digest_json()));
        let summarizer = ProviderSummarizer::new(provider, "m");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            summarizer
                .summarize(&[exchange()], &[], tuning(), &cancel)
                .await,
            Err(FallbackReason::Cancelled)
        );
    }

    #[tokio::test]
    async fn prompt_too_long_shrinks_groups_then_falls_back() {
        // Every attempt is rejected as too long; the loop shrinks input groups
        // and then gives up deterministically.
        let mut provider = FakeProvider::new();
        for _ in 0..MAX_ATTEMPTS {
            provider = provider.script(vec![Err(ProviderError::InvalidRequest {
                message: "context length exceeded".to_string(),
            })]);
        }
        let summarizer = ProviderSummarizer::new(Arc::new(provider), "m");
        let cancel = CancellationToken::new();
        let dropped = vec![exchange(), exchange(), exchange(), exchange()];
        assert_eq!(
            summarizer.summarize(&dropped, &[], tuning(), &cancel).await,
            Err(FallbackReason::ProviderError)
        );
    }

    #[tokio::test]
    async fn carried_entries_are_preserved_in_the_digest() {
        let provider = Arc::new(FakeProvider::new().text(&digest_json()));
        let summarizer = ProviderSummarizer::new(provider, "m");
        let cancel = CancellationToken::new();
        let carried = vec!["user prefers tabs".to_string()];
        let summary = summarizer
            .summarize(&[exchange()], &carried, tuning(), &cancel)
            .await
            .expect("valid digest");
        assert!(summary
            .entries
            .iter()
            .any(|e| e.contains("user prefers tabs")));
    }

    #[test]
    fn strip_media_replaces_long_runs() {
        let blob = "Zm9v".repeat(40); // 160 base64 chars
        let stripped = strip_media(&format!("data: {blob} end"));
        assert!(stripped.contains("[media omitted]"));
        assert!(stripped.contains("end"));
    }

    #[test]
    fn extract_json_unwraps_fenced_output() {
        let text = "Here:\n```json\n{\"sections\":[]}\n```\n";
        assert_eq!(extract_json(text).as_deref(), Some(r#"{"sections":[]}"#));
    }
}
