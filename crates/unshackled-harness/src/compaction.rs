//! Context compaction.
//!
//! When the conversation approaches the context limit, the oldest exchanges are
//! dropped — but never in a way that separates a tool call from its result. We
//! group messages into exchanges bounded by user turns and drop whole oldest
//! exchanges, always keeping any leading system messages and the most recent
//! exchange.

use unshackled_core::{ContentBlock, Message, Role};

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
    /// Whether older messages were removed.
    pub compacted: bool,
}

/// Compact `messages` and inject a bounded summary when old exchanges are
/// removed. The summary is deterministic and intentionally factual: it keeps the
/// task state visible without asking another model to summarize.
#[must_use]
pub fn compact_with_summary(messages: Vec<Message>, token_limit: usize) -> CompactionResult {
    if estimate_tokens(&messages) <= token_limit {
        return CompactionResult {
            messages,
            compacted: false,
        };
    }

    let system_count = messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    let system = messages[..system_count].to_vec();
    let body = &messages[system_count..];

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

    let mut summary = summary_message(&dropped);

    // If a single very large recent window still exceeds the limit, keep
    // removing whole oldest exchanges before considering the summary. Removing
    // individual messages here can strand a tool_result without its tool_use.
    while exchanges.len() > 1
        && estimate_tokens(&build_messages(&system, summary.as_ref(), &exchanges)) > token_limit
    {
        dropped.push(exchanges.remove(0));
        summary = summary_message(&dropped);
    }

    let mut out = build_messages(&system, summary.as_ref(), &exchanges);
    if estimate_tokens(&out) > token_limit && !dropped.is_empty() {
        for (max_exchanges, max_user_chars) in [(4, 60), (2, 60), (1, 60), (1, 30)] {
            summary = summary_message_with(&dropped, max_exchanges, max_user_chars);
            out = build_messages(&system, summary.as_ref(), &exchanges);
            if estimate_tokens(&out) <= token_limit {
                break;
            }
        }
        if estimate_tokens(&out) > token_limit {
            out = build_messages(&system, None, &exchanges);
        }
    }

    CompactionResult {
        messages: out,
        compacted: true,
    }
}

fn build_messages(
    system: &[Message],
    summary: Option<&Message>,
    exchanges: &[Vec<Message>],
) -> Vec<Message> {
    system
        .iter()
        .cloned()
        .chain(summary.cloned())
        .chain(exchanges.iter().flatten().cloned())
        .collect()
}

fn summary_message(dropped: &[Vec<Message>]) -> Option<Message> {
    summary_message_with(dropped, 4, 120)
}

fn summary_message_with(
    dropped: &[Vec<Message>],
    max_exchanges: usize,
    max_user_chars: usize,
) -> Option<Message> {
    if dropped.is_empty() {
        return None;
    }
    let mut lines = vec!["Conversation summary for trimmed history:".to_string()];
    for exchange in dropped.iter().rev().take(max_exchanges).rev() {
        if let Some(line) = summarize_exchange(exchange, max_user_chars) {
            lines.push(line);
        }
    }
    Some(Message::text(Role::System, lines.join("\n")))
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
        (Some(user), true) => Some(format!("- user asked: {user}")),
        (Some(user), false) => Some(format!(
            "- user asked: {user}; tools used: {}",
            tools.join(", ")
        )),
        (None, false) => Some(format!("- tools used: {}", tools.join(", "))),
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
    use unshackled_core::{ToolCall, ToolResult, ToolUseId};

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
}
