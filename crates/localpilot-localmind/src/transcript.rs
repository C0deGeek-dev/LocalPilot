//! Reading session transcripts into retrievable spans.
//!
//! # What a transcript is
//!
//! Not one format. Measured across the whole corpus, three record schemas
//! appear, and the two that carry almost everything are **both JSONL** — which
//! is why the reader dispatches on the record schema rather than on the file:
//!
//! | Schema | Share of bytes |
//! |---|---|
//! | Claude Code JSONL | 75.9% |
//! | Codex JSONL | 23.5% |
//! | LocalPilot plain text | 0.4% |
//!
//! A reader that understands only the first shape drops a quarter of the corpus
//! while appearing to work on the rest. That failure is silent, which is why
//! [`RecoveryReport`] separates *unparseable* from *unrecognised* from *another
//! format*: collapsing them into one "skipped" counter hides exactly this.
//!
//! # What a span is
//!
//! One bounded slice of one record. Spans do not overlap: the index stores no
//! text (the span index is contentless), so a caller widens context by fetching
//! neighbours through the locator rather than by reading duplicated bytes. That
//! makes overlap pure cost.
//!
//! Spans are labelled by [`SpanKind`], because a command's output and a user's
//! question answer different questions and ranking should be able to tell them
//! apart. Command output is by far the largest single content type in the
//! corpus — around a sixth of all bytes — so it is bounded per span rather than
//! stored whole.
//!
//! # Determinism
//!
//! The same transcript produces byte-identical spans on every run and platform:
//! no map iteration, no clock, no locale. Splitting respects UTF-8 character
//! boundaries and prefers line boundaries, so a span never cuts a code point in
//! half and rarely cuts a line.

use serde::{Deserialize, Serialize};

/// The span contract's version.
///
/// A locator recorded under one version is not guaranteed to address the same
/// text under another, so a change here is a migration to detect rather than a
/// silent re-chunk. Bump it whenever record boundaries, span boundaries, the
/// bound, or the indexed-content rules change.
pub const SPAN_CHUNKING_VERSION: u32 = 1;

/// The largest span the chunker will emit, in bytes.
///
/// Chosen to bound the corpus's outlier: command-execution records average
/// around 22 KB and reach far higher. A span is a retrieval unit and a thing a
/// person reads, and neither wants 22 KB.
pub const MAX_SPAN_BYTES: usize = 4096;

/// Speaker labels that begin a record in the plain-text format.
///
/// A **closed set**, deliberately. Transcript bodies are full of lines that look
/// like headers — `status:`, `output:`, `exit:`, `use std:`, and markdown such as
/// `case ([SPLADE v2](https:` — and every one of them is body, not a boundary.
/// A record is not a line.
///
/// This mirrors the set already encoded in `ingest::is_transcript_echo`, which
/// keeps raw conversation out of the distilled-facts path. That guard is not
/// weakened by anything here: this module reads transcripts *as transcripts*,
/// which is a different question from what may be presented as knowledge.
const SPEAKER_ROLES: &[&str] = &[
    "user",
    "assistant",
    "system",
    "tool",
    "tool result",
    "tool error",
    "user shell",
];

/// Which record schema a transcript file uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptSchema {
    /// Claude Code traces: one JSON object per line, discriminated by `type`,
    /// with conversation under `message.content`.
    ClaudeJsonl,
    /// Codex traces: one JSON object per line, discriminated by `type` with a
    /// `payload`, where conversation is under `response_item`.
    CodexJsonl,
    /// LocalPilot's own line-oriented `role: content` rendering, where one
    /// record spans many lines.
    PlainText,
}

/// What a span contains, so ranking can weigh a question differently from the
/// output of a command that answered it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    /// Something a person typed.
    UserMessage,
    /// Something the assistant said.
    AssistantMessage,
    /// Assistant reasoning, where the model's own account of its intent lives.
    Reasoning,
    /// A tool or command invocation, including its arguments.
    ToolCall,
    /// What a tool or command produced. The corpus's largest content type.
    ToolOutput,
    /// Harness-level narration that is neither party speaking.
    System,
}

/// A record boundary found in a transcript, before it is split into spans.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptRecord {
    /// What this record contains.
    pub kind: SpanKind,
    /// The record's text, already extracted from whatever envelope carried it.
    pub text: String,
    /// 1-based line in the source file where the record begins.
    pub start_line: usize,
    /// 1-based line where it ends, inclusive.
    pub end_line: usize,
}

/// A retrievable unit: one bounded slice of one record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Span {
    /// Position within the transcript, counting every span from zero. Stable for
    /// a given transcript and [`SPAN_CHUNKING_VERSION`], which is what makes it
    /// usable in a locator.
    pub ordinal: usize,
    /// What this span contains.
    pub kind: SpanKind,
    /// The span's text.
    pub text: String,
    /// 1-based line in the source file where the span's record begins.
    pub start_line: usize,
    /// 1-based line where the span's record ends, inclusive.
    pub end_line: usize,
    /// Which slice of its record this is, counting from zero. A record short
    /// enough to fit in one span has exactly one, numbered zero.
    pub part: usize,
    /// The contract this span was produced under.
    pub chunking_version: u32,
}

/// What the reader could not use, kept separate by *reason*.
///
/// Collapsing these into one "skipped" counter is how a reader that silently
/// ignores a quarter of the corpus still looks healthy. Each field answers a
/// different question, and only the first is a defect:
///
/// - `unparseable_lines` — genuine corruption. Rare (~0.02% of the real corpus)
///   and not confined to one export, so it is reported per transcript.
/// - `unrecognised_records` — a well-formed record of a `type` this reader does
///   not know. The trace formats gained record types during the corpus's own
///   lifetime and will gain more; this is news, not damage.
/// - `control_records` — well-formed, understood, and deliberately not indexed:
///   mode changes, titles, token counts. Reported so "most of the file was
///   skipped" can be recognised as correct rather than alarming.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryReport {
    /// Lines that should have been JSON and were not.
    pub unparseable_lines: usize,
    /// Records understood structurally but of an unknown `type`.
    pub unrecognised_records: usize,
    /// Records understood and intentionally excluded.
    pub control_records: usize,
    /// Records that produced at least one span.
    pub indexed_records: usize,
}

impl RecoveryReport {
    /// Whether anything at all was recovered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.indexed_records == 0
    }
}

/// The result of reading one transcript.
#[derive(Clone, Debug)]
pub struct TranscriptRead {
    /// Which schema the file was read as.
    pub schema: TranscriptSchema,
    /// The spans, in file order.
    pub spans: Vec<Span>,
    /// What was not indexed, and why.
    pub recovery: RecoveryReport,
}

/// Identify a transcript's schema.
///
/// Reads the whole input, and requires a line to *begin with* `{` **and** parse
/// as a JSON object before counting it as JSONL. Both halves matter, and both
/// come from getting this wrong:
///
/// - A quoted string is valid JSON. A plain-text transcript containing pasted
///   source has lines like `"    configured model: {configured}"`, and a check
///   that accepts "anything that parses" reads them as records.
/// - Sampling a prefix is not enough. In the file that produced the above, the
///   lines in question begin past line 450 — a 400-line probe would have
///   classified it with no evidence either way.
///
/// The decision is **structural, not a ratio**: whichever record shape occurs
/// more often wins. Measuring the corpus found JSONL sessions above 99% JSON
/// lines and plain-text ones below 1%, with nothing between — but using that
/// gap as the *rule* makes a corrupted JSONL file fall out of its own class and
/// be read as prose, which yields no records and reports no corruption. Silent,
/// and precisely the failure this module exists to prevent. A file is JSONL
/// because it is made of JSON records, not because few of them are broken.
#[must_use]
pub fn detect_schema(text: &str) -> TranscriptSchema {
    let mut objects = 0_usize;
    let mut speaker_lines = 0_usize;
    let mut codex = 0_usize;
    let mut claude = 0_usize;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with('{') {
            if speaker_kind(line).is_some() {
                speaker_lines += 1;
            }
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if !value.is_object() {
            continue;
        }
        objects += 1;
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("event_msg" | "response_item" | "turn_context" | "session_meta") => codex += 1,
            Some(_) => claude += 1,
            None => {}
        }
    }
    if objects == 0 || speaker_lines >= objects {
        return TranscriptSchema::PlainText;
    }
    if codex > claude {
        TranscriptSchema::CodexJsonl
    } else {
        TranscriptSchema::ClaudeJsonl
    }
}

/// Read a transcript into spans.
#[must_use]
pub fn read_transcript(text: &str) -> TranscriptRead {
    let schema = detect_schema(text);
    let (records, recovery) = match schema {
        TranscriptSchema::ClaudeJsonl => read_claude_records(text),
        TranscriptSchema::CodexJsonl => read_codex_records(text),
        TranscriptSchema::PlainText => read_plain_records(text),
    };
    TranscriptRead {
        schema,
        spans: spans_from_records(&records),
        recovery,
    }
}

/// Split records into bounded, non-overlapping spans.
fn spans_from_records(records: &[TranscriptRecord]) -> Vec<Span> {
    let mut spans = Vec::new();
    for record in records {
        for (part, text) in split_bounded(&record.text).into_iter().enumerate() {
            spans.push(Span {
                ordinal: spans.len(),
                kind: record.kind,
                text,
                start_line: record.start_line,
                end_line: record.end_line,
                part,
                chunking_version: SPAN_CHUNKING_VERSION,
            });
        }
    }
    spans
}

/// Split text into pieces of at most [`MAX_SPAN_BYTES`], preferring line
/// boundaries and never splitting a UTF-8 character.
///
/// A single line longer than the bound — one enormous line of command output is
/// routine — is split at the last character boundary that fits. Empty and
/// whitespace-only text yields nothing.
fn split_bounded(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed.len() <= MAX_SPAN_BYTES {
        return vec![trimmed.to_string()];
    }
    let mut pieces = Vec::new();
    let mut current = String::new();
    for line in trimmed.split_inclusive('\n') {
        if line.len() > MAX_SPAN_BYTES {
            if !current.trim().is_empty() {
                pieces.push(current.trim_end().to_string());
            }
            current = String::new();
            pieces.extend(split_hard(line));
            continue;
        }
        if current.len().saturating_add(line.len()) > MAX_SPAN_BYTES && !current.trim().is_empty() {
            pieces.push(current.trim_end().to_string());
            current = String::new();
        }
        current.push_str(line);
    }
    if !current.trim().is_empty() {
        pieces.push(current.trim_end().to_string());
    }
    pieces
}

/// Split a single over-long line at character boundaries.
fn split_hard(line: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = line;
    while rest.len() > MAX_SPAN_BYTES {
        // Walk back to a character boundary so a multi-byte code point is never
        // cut in half. At most three bytes of movement.
        let mut cut = MAX_SPAN_BYTES;
        while cut > 0 && !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut == 0 {
            break;
        }
        let (head, tail) = rest.split_at(cut);
        if !head.trim().is_empty() {
            pieces.push(head.trim_end().to_string());
        }
        rest = tail;
    }
    if !rest.trim().is_empty() {
        pieces.push(rest.trim_end().to_string());
    }
    pieces
}

/// Read Claude Code traces.
fn read_claude_records(text: &str) -> (Vec<TranscriptRecord>, RecoveryReport) {
    let mut records = Vec::new();
    let mut recovery = RecoveryReport::default();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let line_number = index + 1;
        let Some(value) = parse_object(trimmed, &mut recovery) else {
            continue;
        };
        let Some(kind) = value.get("type").and_then(serde_json::Value::as_str) else {
            recovery.unrecognised_records += 1;
            continue;
        };
        let role = match kind {
            "user" => SpanKind::UserMessage,
            "assistant" => SpanKind::AssistantMessage,
            "system" => SpanKind::System,
            // Control state: mode changes, titles, queue operations, file
            // history. Understood, and deliberately not conversation.
            "mode"
            | "permission-mode"
            | "bridge-session"
            | "ai-title"
            | "last-prompt"
            | "queue-operation"
            | "file-history-snapshot"
            | "file-history-delta"
            | "compacted"
            | "attachment"
            | "pr-link"
            | "agent-name"
            | "custom-title"
            | "atis-latch" => {
                recovery.control_records += 1;
                continue;
            }
            _ => {
                recovery.unrecognised_records += 1;
                continue;
            }
        };
        let before = records.len();
        if let Some(message) = value.get("message") {
            push_message_content(message, role, line_number, &mut records);
        }
        if records.len() > before {
            recovery.indexed_records += 1;
        } else {
            recovery.control_records += 1;
        }
    }
    (records, recovery)
}

/// Read Codex traces.
///
/// Codex writes every turn twice — once as a `response_item` and again as an
/// `event_msg` carrying a completed item. Reasoning appears exactly the same
/// number of times in both, which is duplication rather than overlap. Only
/// `response_item` is read, so a hit returns one span rather than two spans of
/// the same moment; a duplicate-heavy result set reads as a ranking failure
/// while actually being an ingestion one.
fn read_codex_records(text: &str) -> (Vec<TranscriptRecord>, RecoveryReport) {
    let mut records = Vec::new();
    let mut recovery = RecoveryReport::default();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let line_number = index + 1;
        let Some(value) = parse_object(trimmed, &mut recovery) else {
            continue;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("response_item") => {}
            // The duplicate carrier, plus genuine control traffic (token counts,
            // task start/stop, thread settings).
            // `world_state` and `compacted` were surfaced by the recovery
            // counter on the real corpus (26 and 13 records) — classified here
            // rather than left as perpetual "news".
            Some("event_msg" | "turn_context" | "session_meta" | "world_state" | "compacted") => {
                recovery.control_records += 1;
                continue;
            }
            Some(_) | None => {
                recovery.unrecognised_records += 1;
                continue;
            }
        }
        let Some(payload) = value.get("payload") else {
            recovery.unrecognised_records += 1;
            continue;
        };
        let before = records.len();
        match payload.get("type").and_then(serde_json::Value::as_str) {
            Some("message") => {
                let role = match payload.get("role").and_then(serde_json::Value::as_str) {
                    Some("user") => SpanKind::UserMessage,
                    Some("assistant") => SpanKind::AssistantMessage,
                    _ => SpanKind::System,
                };
                push_content_blocks(payload.get("content"), role, line_number, &mut records);
            }
            Some("reasoning") => {
                push_content_blocks(
                    payload.get("summary").or_else(|| payload.get("content")),
                    SpanKind::Reasoning,
                    line_number,
                    &mut records,
                );
            }
            Some("custom_tool_call" | "function_call" | "local_shell_call") => {
                push_text(
                    codex_call_text(payload),
                    SpanKind::ToolCall,
                    line_number,
                    &mut records,
                );
            }
            Some(
                "custom_tool_call_output" | "function_call_output" | "local_shell_call_output",
            ) => {
                push_text(
                    payload.get("output").map(render_scalar).unwrap_or_default(),
                    SpanKind::ToolOutput,
                    line_number,
                    &mut records,
                );
            }
            Some(_) | None => {
                recovery.unrecognised_records += 1;
                continue;
            }
        }
        if records.len() > before {
            recovery.indexed_records += 1;
        } else {
            recovery.control_records += 1;
        }
    }
    (records, recovery)
}

/// Render a Codex tool call as its name and arguments.
fn codex_call_text(payload: &serde_json::Value) -> String {
    let name = payload
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("tool");
    let arguments = payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .map(render_scalar)
        .unwrap_or_default();
    if arguments.is_empty() {
        name.to_string()
    } else {
        format!("{name}: {arguments}")
    }
}

/// Read LocalPilot's line-oriented rendering, where a record spans many lines.
fn read_plain_records(text: &str) -> (Vec<TranscriptRecord>, RecoveryReport) {
    let mut records: Vec<TranscriptRecord> = Vec::new();
    let mut recovery = RecoveryReport::default();
    let mut current: Option<(SpanKind, String, usize)> = None;
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if let Some(kind) = speaker_kind(line) {
            if let Some((kind, body, start)) = current.take() {
                push_plain(
                    kind,
                    body,
                    start,
                    line_number - 1,
                    &mut records,
                    &mut recovery,
                );
            }
            current = Some((kind, format!("{line}\n"), line_number));
            continue;
        }
        match current.as_mut() {
            Some((_, body, _)) => {
                body.push_str(line);
                body.push('\n');
            }
            // Text before the first speaker header: a preamble, not a record.
            None => recovery.control_records += 1,
        }
    }
    let total_lines = text.lines().count();
    if let Some((kind, body, start)) = current.take() {
        push_plain(kind, body, start, total_lines, &mut records, &mut recovery);
    }
    (records, recovery)
}

fn push_plain(
    kind: SpanKind,
    body: String,
    start_line: usize,
    end_line: usize,
    records: &mut Vec<TranscriptRecord>,
    recovery: &mut RecoveryReport,
) {
    if body.trim().is_empty() {
        recovery.control_records += 1;
        return;
    }
    records.push(TranscriptRecord {
        kind,
        text: body,
        start_line,
        end_line: end_line.max(start_line),
    });
    recovery.indexed_records += 1;
}

/// Whether a line opens a record in the plain-text format, and as what.
///
/// Requires the label at column 0 and drawn from [`SPEAKER_ROLES`]. An indented
/// `assistant:` inside pasted output is body — which is also the honest limit of
/// this format: a *non*-indented one in pasted output would start a false
/// record, and nothing in the rendering distinguishes it.
fn speaker_kind(line: &str) -> Option<SpanKind> {
    if line.starts_with([' ', '\t']) {
        return None;
    }
    let (prefix, _) = line.split_once(':')?;
    let prefix = prefix.trim().to_ascii_lowercase();
    let reasoning = prefix.ends_with(" (reasoning)");
    let role = prefix
        .split_once(" calls ")
        .map_or(prefix.as_str(), |(role, _)| role)
        .trim_end_matches(" (reasoning)")
        .trim();
    if !SPEAKER_ROLES.contains(&role) {
        return None;
    }
    let calls_a_tool = prefix.contains(" calls ");
    Some(match role {
        _ if reasoning => SpanKind::Reasoning,
        _ if calls_a_tool => SpanKind::ToolCall,
        "user" => SpanKind::UserMessage,
        "assistant" => SpanKind::AssistantMessage,
        "tool" | "tool result" | "tool error" | "user shell" => SpanKind::ToolOutput,
        _ => SpanKind::System,
    })
}

/// Parse one line as a JSON object, counting a failure as corruption.
fn parse_object(line: &str, recovery: &mut RecoveryReport) -> Option<serde_json::Value> {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(value) if value.is_object() => Some(value),
        // Well-formed JSON that is not an object is not a record either, but it
        // is not corruption: a bare string is what a pasted source line looks
        // like to a permissive parser.
        Ok(_) => {
            recovery.unrecognised_records += 1;
            None
        }
        Err(_) => {
            recovery.unparseable_lines += 1;
            None
        }
    }
}

/// Extract content from a Claude Code `message` envelope.
fn push_message_content(
    message: &serde_json::Value,
    role: SpanKind,
    line_number: usize,
    records: &mut Vec<TranscriptRecord>,
) {
    push_content_blocks(message.get("content"), role, line_number, records);
}

/// Extract content that may be a bare string or a list of typed blocks.
fn push_content_blocks(
    content: Option<&serde_json::Value>,
    role: SpanKind,
    line_number: usize,
    records: &mut Vec<TranscriptRecord>,
) {
    match content {
        Some(serde_json::Value::String(text)) => {
            push_text(text.clone(), role, line_number, records);
        }
        Some(serde_json::Value::Array(blocks)) => {
            for block in blocks {
                let (kind, text) = block_content(block, role);
                push_text(text, kind, line_number, records);
            }
        }
        _ => {}
    }
}

/// Classify one content block and pull its text out.
fn block_content(block: &serde_json::Value, role: SpanKind) -> (SpanKind, String) {
    match block.get("type").and_then(serde_json::Value::as_str) {
        Some("text" | "input_text" | "output_text") => (
            role,
            block
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        Some("thinking" | "reasoning" | "summary_text") => (
            SpanKind::Reasoning,
            block
                .get("thinking")
                .or_else(|| block.get("text"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        Some("tool_use") => {
            let name = block
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool");
            let input = block.get("input").map(render_scalar).unwrap_or_default();
            (SpanKind::ToolCall, format!("{name}: {input}"))
        }
        Some("tool_result") => (
            SpanKind::ToolOutput,
            block.get("content").map(render_scalar).unwrap_or_default(),
        ),
        _ => (role, String::new()),
    }
}

/// Render a JSON value as the text a reader would want to search.
///
/// A string renders as itself, so an ordinary command output does not arrive
/// wrapped in quotes and escapes. Anything structured renders compactly.
fn render_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(render_scalar)
            .collect::<Vec<_>>()
            .join("\n"),
        serde_json::Value::Object(_) => {
            // A tool-result object usually wraps its text in a `content` or
            // `output` field; prefer that over the JSON envelope.
            value
                .get("content")
                .or_else(|| value.get("output"))
                .or_else(|| value.get("text"))
                .map_or_else(|| value.to_string(), render_scalar)
        }
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn push_text(
    text: String,
    kind: SpanKind,
    line_number: usize,
    records: &mut Vec<TranscriptRecord>,
) {
    if text.trim().is_empty() {
        return;
    }
    records.push(TranscriptRecord {
        kind,
        text,
        start_line: line_number,
        end_line: line_number,
    });
}
