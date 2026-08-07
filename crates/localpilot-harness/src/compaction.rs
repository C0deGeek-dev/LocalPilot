//! Context compaction.
//!
//! When the conversation approaches the context limit, the oldest exchanges are
//! dropped — but never in a way that separates a tool call from its result. We
//! group messages into exchanges bounded by user turns and drop whole oldest
//! exchanges, always keeping any leading system messages and the most recent
//! exchange.

use std::collections::{BTreeMap, BTreeSet};

use localpilot_core::{
    ContentBlock, Message, Role, StructuredSummary, SummaryBudget, SummarySection,
    SummarySectionKind, SummarySource, SummarySourceKind,
};

/// Title line for a compaction summary. Shared shape with harness branch
/// summaries via [`StructuredSummary`].
pub(crate) const SUMMARY_TITLE: &str = "Conversation summary for trimmed history:";

/// Floor for a truncated tool-result output during the last-resort truncation
/// pass: enough to keep the head of the output meaningful.
const TRUNCATED_OUTPUT_CHARS: usize = 240;

/// A rough token estimate (~4 characters per token) over message text.
#[must_use]
pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages.iter().map(message_chars).sum::<usize>() / 4
}

fn message_chars(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text, .. } => text.len(),
            ContentBlock::ToolUse(call) => call.name.len() + call.input.to_string().len(),
            ContentBlock::ToolResult(result) => result.output.len(),
            _ => 0,
        })
        .sum()
}

/// Compact `messages` to fit under `token_limit`, preserving tool-call/result
/// pairing and leading system messages.
#[must_use]
pub fn compact(messages: Vec<Message>, token_limit: usize) -> Vec<Message> {
    compact_with_summary(messages, token_limit).messages
}

/// Result of compacting a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionResult {
    /// Messages to send to the provider.
    pub messages: Vec<Message>,
    /// Whether older messages were removed (or oversized outputs truncated).
    pub compacted: bool,
    /// The structured digest of what was trimmed, when anything was.
    pub summary: Option<StructuredSummary>,
    /// Audit metadata for the projection that was selected.
    pub metadata: CompactionMetadata,
}

/// Safe-to-persist compaction audit metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionMetadata {
    pub requested_mode: CompactionMode,
    pub used_mode: CompactionMode,
    pub dropped_exchanges: usize,
    pub kept_messages: usize,
    pub dropped_messages: usize,
    pub digest_estimate_tokens: usize,
    pub fallback_reason: Option<String>,
    pub truncated_tool_results: usize,
}

/// Runtime compaction mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CompactionMode {
    #[default]
    Deterministic,
    SmartWithFallback,
}

/// Compact `messages` and inject a bounded summary when old exchanges are
/// removed. The summary is deterministic and intentionally factual: it keeps the
/// task state visible without asking another model to summarize.
#[must_use]
pub fn compact_with_summary(messages: Vec<Message>, token_limit: usize) -> CompactionResult {
    compact_plan(messages, token_limit).result
}

/// The full compaction plan: the deterministic [`CompactionResult`] plus the raw
/// material an optional smart summarizer needs — the exchanges that were trimmed
/// and any carried prior-summary entries. Building both in one pass keeps the
/// smart path a pure post-step: it never re-derives which exchanges were
/// dropped, and a smart digest that fails validation simply leaves
/// [`CompactionPlan::result`] in force (completed-only cutover).
pub(crate) struct CompactionPlan {
    /// The deterministic result, always valid and within budget.
    pub result: CompactionResult,
    /// Exchanges removed from active history, oldest first. Smart summarization
    /// reads these; nothing else does.
    pub dropped: Vec<Vec<Message>>,
    /// Prior-summary entries folded forward from an earlier compaction.
    pub carried: Vec<String>,
}

pub(crate) fn compact_plan(messages: Vec<Message>, token_limit: usize) -> CompactionPlan {
    if estimate_tokens(&messages) <= token_limit {
        let kept_messages = messages.len();
        return CompactionPlan {
            result: CompactionResult {
                messages,
                compacted: false,
                summary: None,
                metadata: CompactionMetadata {
                    kept_messages,
                    ..CompactionMetadata::default()
                },
            },
            dropped: Vec::new(),
            carried: Vec::new(),
        };
    }

    let system_count = messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    let mut system = messages[..system_count].to_vec();
    let body = &messages[system_count..];

    // Iterative compaction: a summary injected by an earlier compaction is
    // carried forward into the next digest instead of accumulating as extra
    // system messages.
    let mut carried: Vec<String> = Vec::new();
    system.retain(|message| match first_text(message) {
        Some(text) if text.starts_with(SUMMARY_TITLE) => {
            carried.extend(
                text.lines()
                    .skip(1)
                    .map(|line| line.trim_start_matches("- ").to_string()),
            );
            false
        }
        _ => true,
    });
    let fold_carried = |carried: &[String], summary: Option<StructuredSummary>| {
        const MAX_ENTRIES: usize = 8;
        if carried.is_empty() {
            return summary;
        }
        let mut entries = carried.to_vec();
        entries.extend(summary.map(|s| s.entries).unwrap_or_default());
        let excess = entries.len().saturating_sub(MAX_ENTRIES);
        entries.drain(..excess);
        Some(StructuredSummary::new(SUMMARY_TITLE, entries))
    };

    // Group the body into exchanges that each start at a user message, so a tool
    // call and its result always live in the same exchange.
    let mut exchanges: Vec<Vec<Message>> = Vec::new();
    for message in body {
        if message.role == Role::User || exchanges.is_empty() {
            exchanges.push(Vec::new());
        }
        if let Some(last) = exchanges.last_mut() {
            last.push(message.clone());
        }
    }

    let mut dropped = Vec::new();

    // Drop oldest exchanges until under the limit, always keeping the last one.
    while exchanges.len() > 1 {
        let candidate: Vec<Message> = system
            .iter()
            .cloned()
            .chain(exchanges.iter().flatten().cloned())
            .collect();
        if estimate_tokens(&candidate) <= token_limit {
            break;
        }
        dropped.push(exchanges.remove(0));
    }

    let mut summary = fold_carried(&carried, structured_summary(&dropped));

    // If a single very large recent window still exceeds the limit, keep
    // removing whole oldest exchanges before considering the summary. Removing
    // individual messages here can strand a tool_result without its tool_use.
    while exchanges.len() > 1
        && estimate_tokens(&build_messages(&system, summary.as_ref(), &exchanges)) > token_limit
    {
        dropped.push(exchanges.remove(0));
        summary = fold_carried(&carried, structured_summary(&dropped));
    }

    // Split-turn: a single remaining exchange that alone exceeds the budget is
    // split rather than only truncated. Its older sub-turns are digested into
    // the summary and only a budget-fitting recent suffix is kept, cut at a
    // sub-turn boundary so no tool_result is ever orphaned and the kept history
    // still begins at the user turn.
    if exchanges.len() == 1
        && estimate_tokens(&build_messages(&system, summary.as_ref(), &exchanges)) > token_limit
    {
        if let Some((prefix, suffix)) =
            split_oversized_exchange(&system, summary.as_ref(), &exchanges[0], token_limit)
        {
            dropped.push(prefix);
            exchanges[0] = suffix;
            summary = fold_carried(&carried, structured_summary(&dropped));
        }
    }

    let mut out = build_messages(&system, summary.as_ref(), &exchanges);
    if estimate_tokens(&out) > token_limit && !dropped.is_empty() {
        for (max_exchanges, max_user_chars) in [(4, 60), (2, 60), (1, 60), (1, 30)] {
            summary = fold_carried(
                &carried,
                structured_summary_with(&dropped, max_exchanges, max_user_chars),
            );
            out = build_messages(&system, summary.as_ref(), &exchanges);
            if estimate_tokens(&out) <= token_limit {
                break;
            }
        }
        if estimate_tokens(&out) > token_limit {
            summary = None;
            out = build_messages(&system, None, &exchanges);
        }
    }

    // Last resort: nothing left to drop (a single oversized kept message —
    // typically one huge tool result) can still exceed the limit. Truncate
    // tool-result outputs, oldest first, rather than giving up over budget.
    // Truncating only outputs never separates a tool_use from its result.
    let truncated_tool_results = if estimate_tokens(&out) > token_limit {
        truncate_oldest_tool_results(&mut out, token_limit)
    } else {
        0
    };

    // Even when the summary message was dropped to fit the budget, the result
    // digest keeps the carried entries so the event log loses nothing.
    let dropped_messages = dropped.iter().map(Vec::len).sum();
    let digest = finalize_summary(
        summary.or_else(|| fold_carried(&carried, None)),
        &dropped,
        &out,
        token_limit,
        truncated_tool_results,
    );
    let digest_estimate_tokens = digest.render().len() / 4;
    let kept_messages = exchanges.iter().map(Vec::len).sum::<usize>() + system.len();

    CompactionPlan {
        result: CompactionResult {
            messages: out,
            compacted: true,
            summary: Some(digest),
            metadata: CompactionMetadata {
                requested_mode: CompactionMode::Deterministic,
                used_mode: CompactionMode::Deterministic,
                dropped_exchanges: dropped.len(),
                kept_messages,
                dropped_messages,
                digest_estimate_tokens,
                fallback_reason: None,
                truncated_tool_results,
            },
        },
        dropped,
        carried,
    }
}

/// Attempt completed-only cutover to a smart digest. The smart projection is
/// adopted only when it actually replaces the deterministic summary message and
/// still fits `token_limit`; otherwise the deterministic `result` is returned
/// unchanged with an over-budget fallback recorded. A smart attempt therefore
/// can never enlarge or corrupt the active projection.
pub(crate) fn apply_smart_digest(
    mut result: CompactionResult,
    dropped: &[Vec<Message>],
    mut smart: StructuredSummary,
    token_limit: usize,
) -> CompactionResult {
    let Some(swapped) = swap_summary(&result.messages, &smart) else {
        result.metadata.used_mode = CompactionMode::Deterministic;
        result.metadata.fallback_reason =
            Some("deterministic summary was dropped; smart digest not applied".to_string());
        return result;
    };
    let estimated = estimate_tokens(&swapped);
    if estimated > token_limit {
        result.metadata.used_mode = CompactionMode::Deterministic;
        result.metadata.fallback_reason = Some("smart summary exceeded budget".to_string());
        return result;
    }
    smart.budget = SummaryBudget {
        estimated_tokens: estimated,
        original_messages: dropped.iter().map(Vec::len).sum::<usize>() + swapped.len(),
        kept_messages: swapped.len(),
        dropped_messages: dropped.iter().map(Vec::len).sum(),
        truncated_tool_results: result.metadata.truncated_tool_results,
        truncation_reason: None,
    };
    result.metadata.used_mode = CompactionMode::SmartWithFallback;
    result.metadata.digest_estimate_tokens = smart.render().len() / 4;
    result.messages = swapped;
    result.summary = Some(smart);
    result
}

/// Replace the deterministic summary system message with `smart`'s rendering.
/// Returns `None` when the projection has no summary message to replace (the
/// deterministic path dropped it under extreme budget pressure), so the caller
/// never injects an unaccounted-for message.
fn swap_summary(messages: &[Message], smart: &StructuredSummary) -> Option<Vec<Message>> {
    let rendered = smart.render();
    let mut replaced = false;
    let out = messages
        .iter()
        .map(|message| {
            if !replaced && is_summary_message(message) {
                replaced = true;
                Message::text(Role::System, rendered.clone())
            } else {
                message.clone()
            }
        })
        .collect();
    replaced.then_some(out)
}

fn is_summary_message(message: &Message) -> bool {
    matches!(first_text(message), Some(text) if text.starts_with(SUMMARY_TITLE))
}

/// Split one oversized exchange into a digestible prefix and a budget-fitting
/// recent suffix. The exchange begins with a user message; the suffix always
/// keeps that user message and resumes at a later sub-turn boundary (an
/// assistant message whose tool results follow it), so pairing is never broken
/// and the kept history still starts at a user turn. Returns `None` when no
/// non-trivial split helps (for example a single huge message), leaving the
/// truncation backstop to handle it.
fn split_oversized_exchange(
    system: &[Message],
    summary: Option<&StructuredSummary>,
    exchange: &[Message],
    token_limit: usize,
) -> Option<(Vec<Message>, Vec<Message>)> {
    if exchange.first().map(|message| message.role) != Some(Role::User) {
        return None;
    }
    let user = &exchange[0];
    // Cut points: the start of each fresh assistant sub-turn after the user
    // message. Cutting there keeps every tool_use with its tool_result. The
    // smallest cut past the first sub-turn yields the largest fitting suffix.
    for cut in 2..exchange.len() {
        if exchange[cut].role != Role::Assistant
            || !matches!(exchange[cut - 1].role, Role::Tool | Role::User)
        {
            continue;
        }
        let suffix: Vec<Message> = std::iter::once(user.clone())
            .chain(exchange[cut..].iter().cloned())
            .collect();
        let candidate = build_messages(system, summary, std::slice::from_ref(&suffix));
        if estimate_tokens(&candidate) <= token_limit {
            return Some((exchange[1..cut].to_vec(), suffix));
        }
    }
    None
}

/// Truncate kept tool-result outputs, oldest first, stopping as soon as the
/// conversation fits (or every output is already truncated). The pairing
/// invariant is untouched: only the *content* of results shrinks, never their
/// presence.
fn truncate_oldest_tool_results(messages: &mut [Message], token_limit: usize) -> usize {
    let mut truncated = 0;
    for index in 0..messages.len() {
        if estimate_tokens(messages) <= token_limit {
            return truncated;
        }
        for block in &mut messages[index].content {
            if let ContentBlock::ToolResult(result) = block {
                if result.output.chars().count() > TRUNCATED_OUTPUT_CHARS {
                    let mut kept = truncate(&result.output, TRUNCATED_OUTPUT_CHARS);
                    kept.push_str("\n[output truncated during context compaction]");
                    result.output = kept;
                    truncated += 1;
                }
            }
        }
    }
    truncated
}

/// Merge runs of consecutive `Role::System` messages into a single system
/// message, preserving order and content. Compaction injects its summary as a
/// system message right after the agent prompt, which would otherwise reach the
/// provider as two consecutive system messages — fine for the Anthropic adapter
/// (it concatenates all system blocks) but surfaced verbatim by the OpenAI-style
/// adapter. Folding them keeps a single leading system block on every wire.
/// Only *adjacent* system messages merge, so a lone system message elsewhere in
/// the history is left untouched.
#[must_use]
pub fn merge_consecutive_system(messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for message in messages {
        match out.last_mut() {
            Some(last) if last.role == Role::System && message.role == Role::System => {
                last.content.extend(message.content);
            }
            _ => out.push(message),
        }
    }
    out
}

fn build_messages(
    system: &[Message],
    summary: Option<&StructuredSummary>,
    exchanges: &[Vec<Message>],
) -> Vec<Message> {
    system
        .iter()
        .cloned()
        .chain(summary.map(|digest| Message::text(Role::System, digest.render())))
        .chain(exchanges.iter().flatten().cloned())
        .collect()
}

fn structured_summary(dropped: &[Vec<Message>]) -> Option<StructuredSummary> {
    structured_summary_with(dropped, 4, 120)
}

fn structured_summary_with(
    dropped: &[Vec<Message>],
    max_exchanges: usize,
    max_user_chars: usize,
) -> Option<StructuredSummary> {
    if dropped.is_empty() {
        return None;
    }
    let entries = dropped
        .iter()
        .rev()
        .take(max_exchanges)
        .rev()
        .filter_map(|exchange| summarize_exchange(exchange, max_user_chars))
        .collect();
    Some(StructuredSummary::new(SUMMARY_TITLE, entries))
}

fn finalize_summary(
    summary: Option<StructuredSummary>,
    dropped: &[Vec<Message>],
    output: &[Message],
    token_limit: usize,
    truncated_tool_results: usize,
) -> StructuredSummary {
    let mut summary = summary.unwrap_or_else(|| {
        StructuredSummary::new(
            SUMMARY_TITLE,
            vec!["older exchanges were trimmed".to_string()],
        )
    });
    let carried_entries = summary.entries.clone();
    let digest = semantic_digest(dropped);
    if !digest.entries.is_empty() || !carried_entries.is_empty() {
        summary.entries = carried_entries
            .iter()
            .cloned()
            .chain(digest.entries)
            .collect::<Vec<_>>();
    }
    summary.sections = digest.sections;
    if !carried_entries.is_empty() {
        summary.sections.push(SummarySection::new(
            SummarySectionKind::CriticalContext,
            carried_entries,
        ));
    }
    summary.sources = digest.sources;
    summary.budget = SummaryBudget {
        estimated_tokens: estimate_tokens(output),
        original_messages: dropped.iter().map(Vec::len).sum::<usize>() + output.len(),
        kept_messages: output.len(),
        dropped_messages: dropped.iter().map(Vec::len).sum(),
        truncated_tool_results,
        truncation_reason: (estimate_tokens(output) >= token_limit)
            .then(|| "context budget pressure".to_string()),
    };
    summary
}

struct SemanticDigest {
    entries: Vec<String>,
    sections: Vec<SummarySection>,
    sources: Vec<SummarySource>,
}

#[derive(Default)]
struct DigestBuckets {
    sections: BTreeMap<SummarySectionKind, Vec<String>>,
    /// The most recent non-synthetic user intent, overwritten in chronological
    /// order so the digest's Goal is the last real user turn — not the
    /// lexicographically-greatest one, and never a runtime-synthesized notice.
    latest_user_intent: Option<String>,
    command_counts: BTreeMap<String, usize>,
    file_paths: BTreeSet<String>,
    /// Per-path file operations (read/modified/created/deleted) derived from the
    /// tool that touched it, so the digest records what happened to each file.
    file_ops: BTreeMap<String, BTreeSet<String>>,
    failures: BTreeSet<String>,
    stale: BTreeSet<String>,
}

fn semantic_digest(dropped: &[Vec<Message>]) -> SemanticDigest {
    let mut buckets = DigestBuckets::default();
    let mut sources = Vec::new();

    for (exchange_index, exchange) in dropped.iter().enumerate() {
        let range = format!("exchange-{exchange_index}");
        sources.push(SummarySource::new(
            SummarySourceKind::MessageRange,
            range.clone(),
        ));
        for message in exchange {
            if let Some(text) = first_text(message) {
                classify_text(message.role, message.is_synthetic(), text, &mut buckets);
            }
            for block in &message.content {
                match block {
                    ContentBlock::ToolUse(call) => {
                        *buckets.command_counts.entry(call.name.clone()).or_default() += 1;
                        sources.push(SummarySource::new(
                            SummarySourceKind::ToolCall,
                            call.id.as_str(),
                        ));
                        let mut call_paths = BTreeSet::new();
                        collect_json_paths(&call.input, &mut call_paths);
                        if let Some(op) = classify_file_op(&call.name) {
                            for path in &call_paths {
                                buckets
                                    .file_ops
                                    .entry(path.clone())
                                    .or_default()
                                    .insert(op.to_string());
                            }
                        }
                        buckets.file_paths.extend(call_paths);
                    }
                    ContentBlock::ToolResult(result) => {
                        if result.is_error() {
                            buckets.failures.insert(truncate(&result.output, 120));
                        }
                        sources.push(SummarySource::new(
                            SummarySourceKind::ToolResult,
                            result.id.as_str(),
                        ));
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(latest_goal) = buckets.latest_user_intent.take() {
        buckets
            .sections
            .entry(SummarySectionKind::Goal)
            .or_default()
            .push(latest_goal);
    }
    for (name, count) in buckets.command_counts {
        let item = if count > 1 {
            format!("{name} used {count} time(s)")
        } else {
            format!("{name} used")
        };
        buckets
            .sections
            .entry(SummarySectionKind::CommandOutcomes)
            .or_default()
            .push(item);
    }
    let file_paths = std::mem::take(&mut buckets.file_paths);
    let file_ops = std::mem::take(&mut buckets.file_ops);
    for path in file_paths {
        sources
            .push(SummarySource::new(SummarySourceKind::FilePath, path.clone()).with_path(&path));
        let label = match file_ops.get(&path) {
            Some(ops) if !ops.is_empty() => {
                format!(
                    "{} {path}",
                    ops.iter().cloned().collect::<Vec<_>>().join("/")
                )
            }
            _ => path.clone(),
        };
        buckets
            .sections
            .entry(SummarySectionKind::RelevantFiles)
            .or_default()
            .push(label);
    }
    for failure in buckets.failures {
        buckets
            .sections
            .entry(SummarySectionKind::CommandOutcomes)
            .or_default()
            .push(format!("failure observed: {failure}"));
    }
    for item in buckets.stale {
        buckets
            .sections
            .entry(SummarySectionKind::StaleOrSuperseded)
            .or_default()
            .push(item);
    }

    let mut entries = Vec::new();
    for kind in [
        SummarySectionKind::Goal,
        SummarySectionKind::Constraints,
        SummarySectionKind::Progress,
        SummarySectionKind::Decisions,
        SummarySectionKind::NextSteps,
        SummarySectionKind::CriticalContext,
        SummarySectionKind::RelevantFiles,
        SummarySectionKind::CommandOutcomes,
        SummarySectionKind::Risks,
        SummarySectionKind::StaleOrSuperseded,
    ] {
        let Some(items) = buckets.sections.get(&kind) else {
            continue;
        };
        for item in bounded_unique(items, 4) {
            entries.push(format!("{}: {item}", kind.label()));
        }
    }
    let sections = buckets
        .sections
        .into_iter()
        .map(|(kind, items)| SummarySection::new(kind, bounded_unique(&items, 8)))
        .collect();
    SemanticDigest {
        entries,
        sections,
        sources,
    }
}

fn classify_text(role: Role, is_synthetic: bool, text: &str, buckets: &mut DigestBuckets) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    let lower = trimmed.to_ascii_lowercase();
    // A runtime-synthesized message (e.g. the no-progress stop notice) shapes the
    // conversation and is legitimate role-agnostic evidence below, but it is never
    // a user *intent* — skip the goal/Constraints/NextSteps heuristics for it so a
    // synthetic notice can never be elected as the session goal.
    if role == Role::User && !is_synthetic {
        buckets.latest_user_intent = Some(truncate(trimmed, 120));
        if contains_any(
            &lower,
            &["prefer", "always", "never", "constraint", "require"],
        ) {
            buckets
                .sections
                .entry(SummarySectionKind::Constraints)
                .or_default()
                .push(truncate(trimmed, 140));
        }
        if contains_any(&lower, &["next", "todo", "pending", "after that"]) {
            buckets
                .sections
                .entry(SummarySectionKind::NextSteps)
                .or_default()
                .push(truncate(trimmed, 140));
        }
    }
    if contains_any(
        &lower,
        &["decided", "decision", "use ", "switch to", "settled"],
    ) {
        buckets
            .sections
            .entry(SummarySectionKind::Decisions)
            .or_default()
            .push(truncate(trimmed, 140));
    }
    if contains_any(
        &lower,
        &["fixed", "implemented", "changed", "added", "completed"],
    ) {
        buckets
            .sections
            .entry(SummarySectionKind::Progress)
            .or_default()
            .push(truncate(trimmed, 140));
    }
    if contains_any(&lower, &["blocked", "risk", "unknown", "cannot", "failed"]) {
        if lower.contains("failed") || lower.contains("error") {
            buckets.failures.insert(truncate(trimmed, 140));
        } else {
            buckets
                .sections
                .entry(SummarySectionKind::Risks)
                .or_default()
                .push(truncate(trimmed, 140));
        }
    }
    if contains_any(&lower, &["obsolete", "stale", "superseded", "replaced"]) {
        buckets.stale.insert(truncate(trimmed, 140));
    }
    if let Some(command) = trimmed.strip_prefix("$ ") {
        *buckets
            .command_counts
            .entry(truncate(command, 120))
            .or_default() += 1;
    }
    collect_text_paths(trimmed, &mut buckets.file_paths);
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

/// Classify a tool name into the file operation it performs, so the digest can
/// record read/modified/created/deleted per file. Order matters: delete and
/// create are checked before the broader write/edit family.
fn classify_file_op(tool_name: &str) -> Option<&'static str> {
    let name = tool_name.to_ascii_lowercase();
    if contains_any(&name, &["delete", "remove", "unlink", "rmdir", "rm_"]) {
        Some("deleted")
    } else if contains_any(&name, &["create", "new_file", "touch", "mkdir"]) {
        Some("created")
    } else if contains_any(
        &name,
        &[
            "write", "edit", "modify", "replace", "patch", "append", "insert", "apply",
        ],
    ) {
        Some("modified")
    } else if contains_any(&name, &["read", "cat", "open", "view", "show", "load"]) {
        Some("read")
    } else {
        None
    }
}

fn bounded_unique(items: &[String], max: usize) -> Vec<String> {
    let mut seen = BTreeSet::new();
    items
        .iter()
        .filter(|item| seen.insert(item.to_ascii_lowercase()))
        .take(max)
        .cloned()
        .collect()
}

fn collect_json_paths(value: &serde_json::Value, paths: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::String(text) => collect_text_paths(text, paths),
        serde_json::Value::Array(values) => {
            for value in values {
                collect_json_paths(value, paths);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if matches!(key.as_str(), "path" | "file" | "filename" | "target") {
                    if let Some(text) = value.as_str() {
                        collect_text_paths(text, paths);
                    }
                }
                collect_json_paths(value, paths);
            }
        }
        _ => {}
    }
}

fn collect_text_paths(text: &str, paths: &mut BTreeSet<String>) {
    for token in text.split_whitespace() {
        let token = token.trim_matches(|ch: char| {
            matches!(ch, ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']')
        });
        if token.contains('/')
            || token.contains('\\')
            || token.ends_with(".rs")
            || token.ends_with(".toml")
            || token.ends_with(".md")
            || token.ends_with(".json")
        {
            paths.insert(truncate(token, 160));
        }
    }
}

fn summarize_exchange(exchange: &[Message], max_user_chars: usize) -> Option<String> {
    let user = exchange
        .iter()
        .find(|message| message.role == Role::User)
        .and_then(first_text)
        .map(|text| truncate(text, max_user_chars));
    let tools: Vec<String> = exchange
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolUse(call) => Some(call.name.clone()),
            _ => None,
        })
        .collect();
    match (user, tools.is_empty()) {
        (Some(user), true) => Some(format!("user asked: {user}")),
        (Some(user), false) => Some(format!(
            "user asked: {user}; tools used: {}",
            tools.join(", ")
        )),
        (None, false) => Some(format!("tools used: {}", tools.join(", "))),
        (None, true) => None,
    }
}

fn first_text(message: &Message) -> Option<&str> {
    message.content.iter().find_map(|block| match block {
        ContentBlock::Text { text } => Some(text.as_str()),
        _ => None,
    })
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in text.chars().take(max_chars) {
        out.push(ch);
    }
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_core::{ToolCall, ToolResult, ToolUseId};

    fn user(text: &str) -> Message {
        Message::text(Role::User, text)
    }

    fn tool_exchange(id: &str) -> Vec<Message> {
        vec![
            Message::new(
                Role::Assistant,
                vec![ContentBlock::ToolUse(ToolCall::new(
                    ToolUseId::from(id),
                    "read_file",
                    serde_json::json!({ "path": "a" }),
                ))],
            ),
            Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult(ToolResult::success(
                    ToolUseId::from(id),
                    "x".repeat(400),
                ))],
            ),
        ]
    }

    fn synthetic(text: &str) -> Message {
        Message::text(Role::User, text).into_synthetic("no-progress")
    }

    fn section_items(digest: &SemanticDigest, kind: SummarySectionKind) -> Vec<String> {
        digest
            .sections
            .iter()
            .find(|section| section.kind == kind)
            .map(|section| section.items.clone())
            .unwrap_or_default()
    }

    #[test]
    fn goal_election_ignores_synthetic_stop_notice() {
        // A real prompt followed by the guard's own synthetic stop notice: the
        // notice must never be elected as the goal, and never appear as an intent.
        let dropped = vec![vec![
            user("Fix the auth bug"),
            synthetic("no forward progress; changing strategy"),
        ]];
        let digest = semantic_digest(&dropped);
        assert_eq!(
            section_items(&digest, SummarySectionKind::Goal),
            vec!["Fix the auth bug".to_string()]
        );
    }

    #[test]
    fn goal_election_is_chronological_not_lexical() {
        // Lower-case "zzz…" sorts lexically after capitalised "Add…", so the old
        // BTreeSet::next_back() would elect the wrong one; chronology must win.
        let dropped = vec![vec![
            user("zzz refactor the parser"),
            user("Add a health endpoint"),
        ]];
        let digest = semantic_digest(&dropped);
        assert_eq!(
            section_items(&digest, SummarySectionKind::Goal),
            vec!["Add a health endpoint".to_string()]
        );
    }

    #[test]
    fn goal_election_takes_last_occurrence_on_repeat() {
        // Intents A, B, A: the last real turn (A) wins. An insertion-ordered set
        // would keep A in its first slot and wrongly elect B.
        let dropped = vec![vec![
            user("Investigate the flaky test"),
            user("Draft the release notes"),
            user("Investigate the flaky test"),
        ]];
        let digest = semantic_digest(&dropped);
        assert_eq!(
            section_items(&digest, SummarySectionKind::Goal),
            vec!["Investigate the flaky test".to_string()]
        );
    }

    #[test]
    fn synthetic_user_skips_intent_but_keeps_evidence() {
        // One synthetic user payload carrying a constraint token ("never"), a
        // next-step token ("next") AND a decision token ("decided"/"use "):
        // the intent/Constraints/NextSteps heuristics are skipped for it, but
        // the role-agnostic evidence classification still runs.
        let dropped = vec![vec![synthetic(
            "never revert; we decided to use Postgres; next migrate",
        )]];
        let digest = semantic_digest(&dropped);
        assert!(
            section_items(&digest, SummarySectionKind::Goal).is_empty(),
            "synthetic message must not be elected as the goal"
        );
        assert!(
            section_items(&digest, SummarySectionKind::Constraints).is_empty(),
            "synthetic message must not populate Constraints"
        );
        assert!(
            section_items(&digest, SummarySectionKind::NextSteps).is_empty(),
            "synthetic message must not populate NextSteps"
        );
        assert!(
            !section_items(&digest, SummarySectionKind::Decisions).is_empty(),
            "role-agnostic evidence (Decisions) must still classify synthetic text"
        );
    }

    #[test]
    fn merge_consecutive_system_folds_only_adjacent_system_messages() {
        let messages = vec![
            Message::text(Role::System, "agent prompt"),
            Message::text(Role::System, "summary"),
            Message::text(Role::User, "hi"),
            Message::text(Role::System, "late note"),
        ];
        let merged = merge_consecutive_system(messages);

        assert_eq!(
            merged.iter().map(|m| m.role).collect::<Vec<_>>(),
            vec![Role::System, Role::User, Role::System]
        );
        let leading: Vec<&str> = merged[0]
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(leading, vec!["agent prompt", "summary"]);
    }

    #[test]
    fn under_limit_is_unchanged() {
        let messages = vec![user("hi")];
        assert_eq!(compact(messages.clone(), 1000), messages);
    }

    #[test]
    fn compaction_preserves_tool_result_pairing() {
        let mut messages = vec![Message::text(Role::System, "sys")];
        for i in 0..6 {
            messages.push(user(&format!("turn {i}")));
            messages.extend(tool_exchange(&format!("call_{i}")));
        }
        let compacted = compact(messages, 50);

        // Every tool result kept must have its tool call kept too, and vice versa.
        let call_ids: Vec<_> = compacted
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::ToolUse(c) => Some(c.id.clone()),
                _ => None,
            })
            .collect();
        let result_ids: Vec<_> = compacted
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::ToolResult(r) => Some(r.id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(call_ids, result_ids);
        // The system message is always kept.
        assert_eq!(compacted.first().map(|m| m.role), Some(Role::System));
        // It actually dropped something.
        assert!(call_ids.len() < 6);
    }

    #[test]
    fn final_trimming_does_not_orphan_tool_results() {
        let mut messages = vec![Message::text(Role::System, "sys")];
        for i in 0..4 {
            messages.push(user(&format!("turn {i} {}", "x".repeat(200))));
            messages.extend(tool_exchange(&format!("call_{i}")));
        }
        let compacted = compact(messages, 25);

        let call_ids: Vec<_> = compacted
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::ToolUse(c) => Some(c.id.clone()),
                _ => None,
            })
            .collect();
        let result_ids: Vec<_> = compacted
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::ToolResult(r) => Some(r.id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(call_ids, result_ids);
    }

    #[test]
    fn a_single_oversized_exchange_is_truncated_not_given_up_on() {
        // One huge tool result in the only (kept) exchange: nothing can be
        // dropped, so the output itself must shrink to fit the budget.
        let mut messages = vec![
            Message::text(Role::System, "sys"),
            user("read the big file"),
        ];
        messages.push(Message::new(
            Role::Assistant,
            vec![ContentBlock::ToolUse(ToolCall::new(
                ToolUseId::from("big"),
                "read_file",
                serde_json::json!({ "path": "big.txt" }),
            ))],
        ));
        messages.push(Message::new(
            Role::Tool,
            vec![ContentBlock::ToolResult(ToolResult::success(
                ToolUseId::from("big"),
                "x".repeat(64 * 1024),
            ))],
        ));

        let result = compact_with_summary(messages, 500);

        assert!(result.compacted);
        assert!(
            estimate_tokens(&result.messages) <= 500,
            "still over budget: {}",
            estimate_tokens(&result.messages)
        );
        // Pairing held: the tool_use and its (truncated) result both survive.
        let calls = result
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter(|b| matches!(b, ContentBlock::ToolUse(_)))
            .count();
        let results: Vec<&str> = result
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::ToolResult(r) => Some(r.output.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(calls, 1);
        assert_eq!(results.len(), 1);
        assert!(results[0].contains("[output truncated during context compaction]"));
    }

    #[test]
    fn compaction_result_carries_a_structured_summary() {
        let mut messages = vec![Message::text(Role::System, "sys")];
        for i in 0..6 {
            messages.push(user(&format!("turn {i} {}", "x".repeat(120))));
            messages.extend(tool_exchange(&format!("call_{i}")));
        }
        let result = compact_with_summary(messages, 160);
        assert!(result.compacted);
        let summary = result.summary.expect("a digest of what was trimmed");
        assert_eq!(summary.title, SUMMARY_TITLE);
        assert!(!summary.entries.is_empty());
    }

    #[test]
    fn a_previous_summary_feeds_the_next_compaction() {
        // First compaction produces a summary; a manually compacted history
        // carries it as a system message. The next compaction folds those
        // entries into the new digest instead of stacking summary messages.
        let mut messages = vec![
            Message::text(Role::System, "sys"),
            Message::text(
                Role::System,
                format!(
                    "{SUMMARY_TITLE}
- user asked: earlier work"
                ),
            ),
        ];
        for i in 0..6 {
            messages.push(user(&format!("turn {i} {}", "x".repeat(120))));
            messages.extend(tool_exchange(&format!("call_{i}")));
        }

        let result = compact_with_summary(messages, 320);
        assert!(result.compacted);
        let summary = result.summary.expect("a digest");
        assert!(
            summary.entries.iter().any(|e| e.contains("earlier work")),
            "carried entries: {:?}",
            summary.entries
        );
        // Exactly one summary system message in the output.
        let summary_messages = result
            .messages
            .iter()
            .filter(|m| {
                m.role == Role::System
                    && first_text(m).is_some_and(|t| t.starts_with(SUMMARY_TITLE))
            })
            .count();
        assert_eq!(summary_messages, 1);
    }

    #[test]
    fn compaction_injects_a_bounded_summary() {
        let mut messages = vec![Message::text(Role::System, "sys")];
        for i in 0..8 {
            messages.push(user(&format!("turn {i} {}", "x".repeat(80))));
            messages.extend(tool_exchange(&format!("call_{i}")));
        }

        let result = compact_with_summary(messages, 160);
        assert!(result.compacted);
        let system_text: Vec<_> = result
            .messages
            .iter()
            .filter(|message| message.role == Role::System)
            .filter_map(first_text)
            .collect();
        assert!(system_text
            .iter()
            .any(|text| text.contains("Conversation summary for trimmed history")));
        assert!(estimate_tokens(&result.messages) <= 160);
    }

    /// One user turn with several agentic sub-turns that alone blow the budget:
    /// the split keeps the user message and a recent suffix, digests the rest,
    /// and never orphans a tool result.
    #[test]
    fn an_oversized_single_turn_is_split_not_orphaned() {
        let mut messages = vec![
            Message::text(Role::System, "sys"),
            user("big task: refactor the parser"),
        ];
        for i in 0..6 {
            let id = format!("call_{i}");
            messages.push(Message::new(
                Role::Assistant,
                vec![ContentBlock::ToolUse(ToolCall::new(
                    ToolUseId::from(id.as_str()),
                    "read_file",
                    serde_json::json!({ "path": format!("src/mod_{i}.rs") }),
                ))],
            ));
            messages.push(Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult(ToolResult::success(
                    ToolUseId::from(id.as_str()),
                    "x".repeat(200),
                ))],
            ));
        }

        let result = compact_with_summary(messages, 120);
        assert!(result.compacted);
        assert!(
            estimate_tokens(&result.messages) <= 120,
            "over budget: {}",
            estimate_tokens(&result.messages)
        );
        // The split records a trimmed prefix.
        assert!(result.metadata.dropped_exchanges >= 1);

        // Pairing holds across the split.
        let call_ids: Vec<_> = result
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::ToolUse(c) => Some(c.id.clone()),
                _ => None,
            })
            .collect();
        let result_ids: Vec<_> = result
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::ToolResult(r) => Some(r.id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(call_ids, result_ids);
        // The user turn is preserved and a recent suffix is kept.
        assert!(result
            .messages
            .iter()
            .filter_map(first_text)
            .any(|t| t.contains("big task")));
        assert!(!call_ids.is_empty(), "a recent sub-turn suffix is kept");
    }

    fn compacted_with_summary_fixture() -> CompactionResult {
        let mut messages = vec![Message::text(Role::System, "sys")];
        for i in 0..6 {
            messages.push(user(&format!("turn {i} {}", "x".repeat(120))));
            messages.extend(tool_exchange(&format!("call_{i}")));
        }
        let result = compact_with_summary(messages, 200);
        assert!(result.compacted);
        result
    }

    #[test]
    fn digest_records_file_operations_by_tool() {
        let mut messages = vec![Message::text(Role::System, "sys")];
        for i in 0..6 {
            messages.push(user(&format!("turn {i} {}", "x".repeat(60))));
            messages.push(Message::new(
                Role::Assistant,
                vec![ContentBlock::ToolUse(ToolCall::new(
                    ToolUseId::from(format!("w{i}").as_str()),
                    "write_file",
                    serde_json::json!({ "path": "src/edited.rs" }),
                ))],
            ));
            messages.push(Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult(ToolResult::success(
                    ToolUseId::from(format!("w{i}").as_str()),
                    "ok",
                ))],
            ));
        }
        let result = compact_with_summary(messages, 120);
        let digest = result.summary.expect("a digest").render();
        assert!(
            digest.contains("modified src/edited.rs"),
            "digest lacked the file operation: {digest}"
        );
    }

    #[test]
    fn classify_file_op_maps_tool_names() {
        assert_eq!(classify_file_op("read_file"), Some("read"));
        assert_eq!(classify_file_op("write_file"), Some("modified"));
        assert_eq!(classify_file_op("edit_file"), Some("modified"));
        assert_eq!(classify_file_op("delete_file"), Some("deleted"));
        assert_eq!(classify_file_op("create_file"), Some("created"));
        assert_eq!(classify_file_op("run_shell"), None);
    }

    #[test]
    fn smart_digest_cutover_replaces_the_summary_when_it_fits() {
        let result = compacted_with_summary_fixture();
        let smart = StructuredSummary::new(
            SUMMARY_TITLE,
            vec!["goal: ship smart compaction".to_string()],
        );
        let dropped: Vec<Vec<Message>> = vec![vec![user("dropped")]];
        let out = apply_smart_digest(result, &dropped, smart, 200);

        assert_eq!(out.metadata.used_mode, CompactionMode::SmartWithFallback);
        assert!(out.metadata.fallback_reason.is_none());
        assert!(out
            .messages
            .iter()
            .filter_map(first_text)
            .any(|t| t.contains("ship smart compaction")));
        assert!(estimate_tokens(&out.messages) <= 200);
    }

    #[test]
    fn smart_digest_over_budget_keeps_the_deterministic_projection() {
        let result = compacted_with_summary_fixture();
        let before = result.messages.clone();
        let huge = StructuredSummary::new(
            SUMMARY_TITLE,
            vec!["goal: ".to_string() + &"y".repeat(4_000)],
        );
        let dropped: Vec<Vec<Message>> = vec![vec![user("dropped")]];
        let out = apply_smart_digest(result, &dropped, huge, 200);

        assert_eq!(out.metadata.used_mode, CompactionMode::Deterministic);
        assert_eq!(
            out.metadata.fallback_reason.as_deref(),
            Some("smart summary exceeded budget")
        );
        // Active projection is untouched (completed-only cutover).
        assert_eq!(out.messages, before);
    }
}
