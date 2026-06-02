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
    if estimate_tokens(&messages) <= token_limit {
        return messages;
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
        exchanges.remove(0);
    }

    system
        .into_iter()
        .chain(exchanges.into_iter().flatten())
        .collect()
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
}
