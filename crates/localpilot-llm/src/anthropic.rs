//! Anthropic Messages API provider adapter.
//!
//! Implemented from the public Anthropic Messages API documentation. It speaks
//! the documented official endpoint only; no private or undocumented behaviour
//! is used. The wire shape differs from the OpenAI adapter: a top-level `system`
//! string, `tool_use` / `tool_result` content blocks, a required `max_tokens`,
//! and a typed server-sent-event stream (`message_start`, `content_block_*`,
//! `message_delta`, `message_stop`).
//!
//! Provenance: request and streaming shapes implemented from the public
//! Anthropic API reference (<https://docs.anthropic.com/en/api/messages>). No
//! vendor SDK code, prompts, or identifiers were copied.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use futures::StreamExt;
use indexmap::IndexMap;
use serde_json::{json, Value};
use localpilot_core::{ContentBlock, Message, Role, Secret, TokenUsage};

use crate::error::{ProviderError, QuotaInfo};
use crate::event::{split_inline_thinking, ModelEvent, ModelEventStream};
use crate::provider::{
    AuthRequirement, Capabilities, InputBlockKind, ModelProvider, ProviderDeclaration,
    ReasoningShape, SourceType, ToolCallShape,
};
use crate::request::{ModelRequest, ToolSpec};

/// The documented Messages API version header value.
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// `max_tokens` is required by the API; used when the request does not set one.
const DEFAULT_MAX_TOKENS: u64 = 4096;

/// An Anthropic Messages API provider.
pub struct AnthropicProvider {
    declaration: ProviderDeclaration,
    client: reqwest::Client,
    base_url: String,
    api_key: Option<Secret>,
    default_options: IndexMap<String, Value>,
}

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

impl AnthropicProvider {
    /// Build a provider against `base_url` (without a trailing `/messages`).
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        display_name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: Option<Secret>,
    ) -> Self {
        let auth = if api_key.is_some() {
            AuthRequirement::ApiKey
        } else {
            AuthRequirement::None
        };
        Self {
            declaration: ProviderDeclaration {
                id: id.into(),
                display_name: display_name.into(),
                source_type: SourceType::OfficialApi,
                supported_input_blocks: vec![
                    InputBlockKind::Text,
                    InputBlockKind::Reasoning,
                    InputBlockKind::ToolResult,
                ],
                tool_call_shape: ToolCallShape::AnthropicToolUse,
                reasoning_shape: ReasoningShape::Content,
                capabilities: Capabilities {
                    parallel_tool_calls: true,
                    incremental_tool_json: true,
                    reasoning: true,
                    usage_during_stream: true,
                    per_request_tool_disable: true,
                    quota_reset_metadata: true,
                    needs_no_tool_prompt_path: false,
                },
                max_context_tokens: None,
                auth,
                rate_limit_behavior: None,
            },
            client: reqwest_client(None),
            base_url: base_url.into(),
            api_key,
            default_options: IndexMap::new(),
        }
    }

    /// Override the HTTP request timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.client = reqwest_client(timeout);
        self
    }

    /// Provider-level request options merged into every request body before
    /// request-specific options.
    #[must_use]
    pub fn with_default_options(mut self, options: IndexMap<String, Value>) -> Self {
        self.default_options = options;
        self
    }

    /// Build the JSON request body sent to `/messages`.
    #[must_use]
    pub fn build_body(&self, request: &ModelRequest) -> Value {
        let (system, messages) = translate_messages(&request.messages);
        let max_tokens = request
            .options
            .get("max_tokens")
            .or_else(|| self.default_options.get("max_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_TOKENS);

        let mut body = json!({
            "model": request.model,
            "max_tokens": max_tokens,
            "messages": messages,
            "stream": true,
        });
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(request.tools.iter().map(translate_tool).collect());
        }
        if let Value::Object(map) = &mut body {
            for (key, value) in self.default_options.iter().chain(request.options.iter()) {
                if key != "max_tokens" && key != "suppress_thinking" {
                    map.insert(key.clone(), value.clone());
                }
            }
        }
        body
    }

    fn endpoint(&self) -> String {
        // Normalize to the documented `/v1/messages` path so a base URL given in
        // either the Anthropic-SDK convention (no `/v1`, e.g. from
        // `ANTHROPIC_BASE_URL`) or with a trailing `/v1` both resolve correctly.
        let base = self.base_url.trim_end_matches('/');
        let base = base.strip_suffix("/v1").unwrap_or(base);
        format!("{base}/v1/messages")
    }
}

fn reqwest_client(timeout: Option<Duration>) -> reqwest::Client {
    let builder = reqwest::Client::builder().timeout(timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT));
    match builder.build() {
        Ok(client) => client,
        Err(err) => {
            tracing::warn!(error = %err, "failed to build configured HTTP client");
            reqwest::Client::new()
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for AnthropicProvider {
    fn declaration(&self) -> &ProviderDeclaration {
        &self.declaration
    }

    async fn stream(&self, request: ModelRequest) -> Result<ModelEventStream, ProviderError> {
        let mut builder = self
            .client
            .post(self.endpoint())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&self.build_body(&request));
        if let Some(key) = &self.api_key {
            // The credential is set as a header here and never logged.
            builder = builder.header("x-api-key", key.expose());
        }
        tracing::debug!(model = %request.model, "starting provider stream");

        let response = builder.send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_error_response(status.as_u16(), response).await);
        }

        let body = response.bytes_stream();
        Ok(into_event_stream(body))
    }
}

fn translate_tool(tool: &ToolSpec) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.input_schema,
    })
}

/// Translate the internal message list into Anthropic's `(system, messages)`
/// pair. System content is hoisted to the top-level string; tool results become
/// `tool_result` blocks in a user message; consecutive messages that map to the
/// same role are merged, since the API requires alternating roles.
fn translate_messages(messages: &[Message]) -> (String, Vec<Value>) {
    let mut system = String::new();
    let mut turns: Vec<(&'static str, Vec<Value>)> = Vec::new();

    for message in messages {
        if message.role == Role::System {
            for block in &message.content {
                if let ContentBlock::Text { text } = block {
                    if !system.is_empty() {
                        system.push('\n');
                    }
                    system.push_str(text);
                }
            }
            continue;
        }

        let role = anthropic_role(message.role);
        let blocks = translate_blocks(message);
        if blocks.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((last_role, last_blocks)) if *last_role == role => last_blocks.extend(blocks),
            _ => turns.push((role, blocks)),
        }
    }

    let messages = turns
        .into_iter()
        .map(|(role, content)| json!({ "role": role, "content": anthropic_content(content) }))
        .collect();
    (system, messages)
}

fn anthropic_content(blocks: Vec<Value>) -> Value {
    let mut text_parts = Vec::new();
    for block in &blocks {
        let Some(text) = block
            .as_object()
            .filter(|obj| obj.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|obj| obj.get("text"))
            .and_then(Value::as_str)
        else {
            return Value::Array(blocks);
        };
        text_parts.push(text);
    }
    json!(text_parts.join("\n"))
}

fn translate_blocks(message: &Message) -> Vec<Value> {
    let mut blocks = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } => blocks.push(json!({ "type": "text", "text": text })),
            // A thinking block may only be sent back with its signature; one
            // without a signature is dropped from the request.
            ContentBlock::Reasoning {
                text,
                signature: Some(signature),
                ..
            } => blocks.push(json!({
                "type": "thinking",
                "thinking": text,
                "signature": signature,
            })),
            ContentBlock::ToolUse(call) => blocks.push(json!({
                "type": "tool_use",
                "id": call.id.as_str(),
                "name": call.name,
                "input": call.input,
            })),
            ContentBlock::ToolResult(result) => blocks.push(json!({
                "type": "tool_result",
                "tool_use_id": result.id.as_str(),
                "content": result.output,
                "is_error": result.is_error,
            })),
            _ => {}
        }
    }
    blocks
}

fn anthropic_role(role: Role) -> &'static str {
    match role {
        // Tool results are delivered to the model in a user turn.
        Role::User | Role::Tool => "user",
        Role::Assistant => "assistant",
        // System is hoisted out before this is called; treat as user defensively.
        Role::System => "user",
    }
}

async fn classify_error_response(status: u16, response: reqwest::Response) -> ProviderError {
    let request_id = response
        .headers()
        .get("request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let quota = quota_from_headers(response.headers());
    let body = response.text().await.unwrap_or_default();
    // Surface the provider's error payload (e.g. on a 500) for the run log. The
    // body is the API's own error JSON and never echoes the credential.
    tracing::error!(
        status,
        request_id = request_id.as_deref().unwrap_or("-"),
        body = %body,
        "anthropic provider returned an error response"
    );
    let code = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["error"]["type"].as_str().map(str::to_string));
    ProviderError::from_http(status, code.as_deref(), request_id, quota)
}

fn quota_from_headers(headers: &reqwest::header::HeaderMap) -> QuotaInfo {
    let retry_after = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let limit_kind = headers
        .get("anthropic-ratelimit-requests-limit")
        .map(|_| "requests".to_string());
    QuotaInfo {
        retry_after,
        reset_at: None,
        limit_kind,
        retryable: true,
        raw_provider_code: None,
    }
}

fn into_event_stream<S, B>(body: S) -> ModelEventStream
where
    S: futures::Stream<Item = reqwest::Result<B>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
{
    struct StreamState<S> {
        body: std::pin::Pin<Box<S>>,
        decoder: SseDecoder,
        queue: VecDeque<Result<ModelEvent, ProviderError>>,
    }

    let state = StreamState {
        body: Box::pin(body),
        decoder: SseDecoder::default(),
        queue: VecDeque::new(),
    };

    futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(item) = state.queue.pop_front() {
                return Some((item, state));
            }
            match state.body.next().await {
                Some(Ok(bytes)) => state.decoder.push(bytes.as_ref(), &mut state.queue),
                Some(Err(err)) => state
                    .queue
                    .push_back(Err(ProviderError::Network(err.to_string()))),
                None => {
                    state.decoder.finish(&mut state.queue);
                    return state.queue.pop_front().map(|item| (item, state));
                }
            }
        }
    })
    .boxed()
}

type EventQueue = VecDeque<Result<ModelEvent, ProviderError>>;

/// Incremental decoder for Anthropic's typed server-sent events. Tool input
/// arrives as `input_json_delta` fragments accumulated per content-block index
/// and assembled into a single [`ModelEvent::ToolCall`] at `content_block_stop`.
#[derive(Default)]
struct SseDecoder {
    buf: String,
    tools: BTreeMap<u64, ToolAccum>,
    open_blocks: BTreeSet<u64>,
    closed_blocks: usize,
    saw_content_delta: bool,
    input_tokens: u64,
    done: bool,
    warned_stop_reason: bool,
    saw_stop_reason: bool,
}

#[derive(Default)]
struct ToolAccum {
    id: String,
    name: String,
    input: String,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8], out: &mut EventQueue) {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        while let Some(pos) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=pos).collect();
            self.process_line(line.trim(), out);
        }
    }

    fn finish(&mut self, out: &mut EventQueue) {
        if !self.buf.trim().is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.process_line(line.trim(), out);
        }
        if !self.done {
            if self.saw_stop_reason && self.open_blocks.is_empty() && self.content_complete() {
                self.emit_done(out);
            } else {
                self.done = true;
                out.push_back(Err(ProviderError::StreamDecode(
                    "stream ended before a completion marker".to_string(),
                )));
            }
        }
    }

    fn content_complete(&self) -> bool {
        !self.saw_content_delta || self.closed_blocks > 0
    }

    fn emit_done(&mut self, out: &mut EventQueue) {
        if !self.done {
            self.done = true;
            out.push_back(Ok(ModelEvent::Done));
        }
    }

    fn process_line(&mut self, line: &str, out: &mut EventQueue) {
        // Only `data:` lines carry JSON; the `event:` line duplicates the
        // payload's `type` field, so it is ignored.
        let Some(payload) = line.strip_prefix("data:") else {
            return;
        };
        let payload = payload.trim();
        if payload.is_empty() {
            return;
        }
        match serde_json::from_str::<Value>(payload) {
            Ok(event) => self.handle_event(&event, out),
            Err(e) => out.push_back(Err(ProviderError::StreamDecode(e.to_string()))),
        }
    }

    fn handle_event(&mut self, event: &Value, out: &mut EventQueue) {
        match event["type"].as_str() {
            Some("message_start") => {
                self.input_tokens = event["message"]["usage"]["input_tokens"]
                    .as_u64()
                    .unwrap_or(0);
            }
            Some("content_block_start") => {
                let index = event["index"].as_u64().unwrap_or(0);
                self.open_blocks.insert(index);
                if event["content_block"]["type"].as_str() == Some("tool_use") {
                    let accum = self.tools.entry(index).or_default();
                    accum.id = event["content_block"]["id"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    accum.name = event["content_block"]["name"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                }
            }
            Some("content_block_delta") => {
                self.saw_content_delta = true;
                self.handle_delta(event, out);
            }
            Some("content_block_stop") => {
                let index = event["index"].as_u64().unwrap_or(0);
                if self.open_blocks.remove(&index) {
                    self.closed_blocks = self.closed_blocks.saturating_add(1);
                }
                self.flush_tool(index, out);
            }
            Some("message_delta") => {
                if let Some(output_tokens) = event["usage"]["output_tokens"].as_u64() {
                    out.push_back(Ok(ModelEvent::Usage(TokenUsage {
                        input_tokens: self.input_tokens,
                        output_tokens,
                    })));
                }
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    self.saw_stop_reason = true;
                    self.warn_for_stop_reason(reason, out);
                }
            }
            Some("message_stop") => {
                if self.open_blocks.is_empty() && self.content_complete() {
                    self.emit_done(out);
                } else {
                    self.done = true;
                    out.push_back(Err(ProviderError::StreamDecode(
                        "stream stopped before content lifecycle completed".to_string(),
                    )));
                }
            }
            Some("error") => {
                let message = event["error"]["message"]
                    .as_str()
                    .unwrap_or("stream error")
                    .to_string();
                out.push_back(Err(ProviderError::StreamDecode(message)));
            }
            // `ping` and unknown event types are ignored.
            _ => {}
        }
    }

    fn handle_delta(&mut self, event: &Value, out: &mut EventQueue) {
        let delta = &event["delta"];
        match delta["type"].as_str() {
            Some("text_delta") => {
                if let Some(text) = delta["text"].as_str() {
                    if !text.is_empty() {
                        for event in split_inline_thinking(text) {
                            out.push_back(Ok(event));
                        }
                    }
                }
            }
            Some("thinking_delta") => {
                if let Some(text) = delta["thinking"].as_str() {
                    if !text.is_empty() {
                        out.push_back(Ok(ModelEvent::ReasoningDelta(text.to_string())));
                    }
                }
            }
            Some("input_json_delta") => {
                if let Some(fragment) = delta["partial_json"].as_str() {
                    let index = event["index"].as_u64().unwrap_or(0);
                    self.tools
                        .entry(index)
                        .or_default()
                        .input
                        .push_str(fragment);
                }
            }
            _ => {}
        }
    }

    fn flush_tool(&mut self, index: u64, out: &mut EventQueue) {
        let Some(accum) = self.tools.remove(&index) else {
            return;
        };
        if accum.name.is_empty() {
            return;
        }
        let input_json = if accum.input.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str::<Value>(&accum.input) {
                Ok(value) => value,
                Err(e) => {
                    out.push_back(Err(ProviderError::StreamDecode(format!("tool input: {e}"))));
                    return;
                }
            }
        };
        out.push_back(Ok(ModelEvent::ToolCall {
            id: accum.id,
            name: accum.name,
            input_json,
        }));
    }

    fn warn_for_stop_reason(&mut self, reason: &str, out: &mut EventQueue) {
        if self.warned_stop_reason {
            return;
        }
        let message = match reason {
            "end_turn" | "tool_use" => None,
            "max_tokens" => {
                Some("provider stopped at max_tokens; output may be truncated".to_string())
            }
            "pause_turn" => Some(
                "provider paused during server-tool processing; a continuation may be required"
                    .to_string(),
            ),
            "refusal" => Some("provider refused the request".to_string()),
            "stop_sequence" => Some(
                "provider stopped at a configured stop sequence; output may be partial".to_string(),
            ),
            other if other.trim().is_empty() => None,
            other => Some(format!("provider stopped with reason `{other}`")),
        };
        if let Some(message) = message {
            self.warned_stop_reason = true;
            out.push_back(Ok(ModelEvent::ProviderWarning { message }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn collect_sse(chunks: &[&str]) -> Vec<Result<ModelEvent, ProviderError>> {
        let mut decoder = SseDecoder::default();
        let mut out = EventQueue::new();
        for chunk in chunks {
            decoder.push(chunk.as_bytes(), &mut out);
        }
        decoder.finish(&mut out);
        out.into_iter().collect()
    }

    #[test]
    fn parses_streaming_text_and_usage() {
        let events = collect_sse(&[
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n",
            "data: {\"type\":\"message_stop\"}\n",
        ]);
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                Ok(ModelEvent::TextDelta(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        assert!(events.iter().any(|e| matches!(
            e,
            Ok(ModelEvent::Usage(u)) if u.input_tokens == 7 && u.output_tokens == 5
        )));
        assert!(matches!(events.last(), Some(Ok(ModelEvent::Done))));
    }

    #[test]
    fn assembles_incremental_tool_use() {
        let events = collect_sse(&[
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\",\"input\":{}}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.rs\\\"}\"}}\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "data: {\"type\":\"message_stop\"}\n",
        ]);
        let call = events.iter().find_map(|e| match e {
            Ok(ModelEvent::ToolCall {
                id,
                name,
                input_json,
            }) => Some((id.clone(), name.clone(), input_json.clone())),
            _ => None,
        });
        let (id, name, input) = call.expect("a tool call was emitted");
        assert_eq!(id, "toolu_1");
        assert_eq!(name, "read_file");
        assert_eq!(input["path"], "a.rs");
    }

    #[test]
    fn parses_thinking_delta() {
        let events = collect_sse(&[
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n",
            "data: {\"type\":\"message_stop\"}\n",
        ]);
        assert!(events
            .iter()
            .any(|e| matches!(e, Ok(ModelEvent::ReasoningDelta(r)) if r == "hmm")));
    }

    #[test]
    fn routes_inline_think_tags_to_reasoning() {
        let events = collect_sse(&[
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"visible <think>private</think> tail\"}}\n",
            "data: {\"type\":\"message_stop\"}\n",
        ]);
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                Ok(ModelEvent::TextDelta(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "visible  tail");
        assert!(events
            .iter()
            .any(|e| matches!(e, Ok(ModelEvent::ReasoningDelta(r)) if r == "private")));
    }

    #[test]
    fn error_event_yields_typed_error() {
        let events = collect_sse(&[
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}\n",
        ]);
        assert!(events
            .iter()
            .any(|e| matches!(e, Err(ProviderError::StreamDecode(m)) if m == "overloaded")));
    }

    #[test]
    fn max_tokens_stop_reason_yields_warning() {
        let events = collect_sse(&[
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":5}}\n",
            "data: {\"type\":\"message_stop\"}\n",
        ]);
        assert!(events.iter().any(|e| matches!(
            e,
            Ok(ModelEvent::ProviderWarning { message })
                if message.contains("max_tokens")
        )));
    }

    #[test]
    fn normal_tool_use_stop_reason_is_quiet() {
        let events = collect_sse(&[
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n",
            "data: {\"type\":\"message_stop\"}\n",
        ]);
        assert!(!events
            .iter()
            .any(|e| matches!(e, Ok(ModelEvent::ProviderWarning { .. }))));
    }

    #[test]
    fn rejects_text_when_transport_ends_before_a_completion_marker() {
        let events = collect_sse(&[
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Let me start by understanding the p\"}}\n",
        ]);
        assert!(events.iter().any(|event| matches!(
            event,
            Err(ProviderError::StreamDecode(message))
                if message.contains("completion marker")
        )));
        assert!(!events
            .iter()
            .any(|event| matches!(event, Ok(ModelEvent::Done))));
    }

    #[test]
    fn stop_reason_is_a_completion_marker_for_compatible_servers() {
        let events = collect_sse(&[
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"complete\"}}\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n",
        ]);
        assert!(matches!(events.last(), Some(Ok(ModelEvent::Done))));
    }

    #[test]
    fn stop_reason_does_not_complete_an_open_text_block() {
        let events = collect_sse(&[
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"cut off mid wor\"}}\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n",
        ]);
        assert!(events.iter().any(|event| matches!(
            event,
            Err(ProviderError::StreamDecode(message))
                if message.contains("completion marker")
        )));
        assert!(!events
            .iter()
            .any(|event| matches!(event, Ok(ModelEvent::Done))));
    }

    #[test]
    fn stop_reason_does_not_complete_unframed_text_deltas() {
        let events = collect_sse(&[
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"cut off mid wor\"}}\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n",
        ]);
        assert!(events.iter().any(|event| matches!(
            event,
            Err(ProviderError::StreamDecode(message))
                if message.contains("completion marker")
        )));
        assert!(!events
            .iter()
            .any(|event| matches!(event, Ok(ModelEvent::Done))));
    }

    #[test]
    fn message_stop_does_not_complete_unframed_text_deltas() {
        let events = collect_sse(&[
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"cut off mid wor\"}}\n",
            "data: {\"type\":\"message_stop\"}\n",
        ]);
        assert!(events
            .iter()
            .any(|event| matches!(event, Err(ProviderError::StreamDecode(_)))));
        assert!(!events
            .iter()
            .any(|event| matches!(event, Ok(ModelEvent::Done))));
    }

    #[test]
    fn endpoint_normalizes_to_v1_messages() {
        // Both the SDK convention (no `/v1`) and an explicit `/v1` base resolve
        // to the documented `/v1/messages` path.
        for base in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/",
            "https://api.anthropic.com/v1",
            "http://127.0.0.1:11435",
        ] {
            let provider = AnthropicProvider::new("a", "A", base, None);
            assert!(
                provider.endpoint().ends_with("/v1/messages"),
                "base {base} -> {}",
                provider.endpoint()
            );
            assert!(!provider.endpoint().contains("/v1/v1/"));
        }
    }

    #[test]
    fn build_body_hoists_system_and_sets_max_tokens() {
        let provider = AnthropicProvider::new(
            "anthropic",
            "Anthropic",
            "https://api.anthropic.com/v1",
            None,
        );
        let messages = vec![
            Message::text(Role::System, "be terse"),
            Message::text(Role::User, "hi"),
        ];
        let body = provider.build_body(&ModelRequest::new("claude", messages));
        assert_eq!(body["system"], "be terse");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        // The system message is not duplicated into the messages array.
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn text_only_turns_use_string_content() {
        let provider = AnthropicProvider::new(
            "anthropic",
            "Anthropic",
            "https://api.anthropic.com/v1",
            None,
        );
        let messages = vec![
            Message::text(Role::User, "first"),
            Message::text(Role::User, "second"),
        ];
        let body = provider.build_body(&ModelRequest::new("claude", messages));
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "first\nsecond");
    }

    #[test]
    fn default_options_are_applied_without_internal_switches() {
        let mut options = IndexMap::new();
        options.insert("max_tokens".to_string(), json!(123));
        options.insert("suppress_thinking".to_string(), json!(true));
        let provider = AnthropicProvider::new(
            "anthropic",
            "Anthropic",
            "https://api.anthropic.com/v1",
            None,
        )
        .with_default_options(options);
        let body = provider.build_body(&ModelRequest::new("claude", Vec::new()));
        assert_eq!(body["max_tokens"], 123);
        assert!(body.get("suppress_thinking").is_none());
    }

    #[test]
    fn tool_results_merge_into_one_user_turn() {
        use localpilot_core::{ToolCall, ToolResult, ToolUseId};
        let provider = AnthropicProvider::new(
            "anthropic",
            "Anthropic",
            "https://api.anthropic.com/v1",
            None,
        );
        let messages = vec![
            Message::text(Role::User, "go"),
            Message::new(
                Role::Assistant,
                vec![
                    ContentBlock::ToolUse(ToolCall::new(ToolUseId::from("t1"), "a", json!({}))),
                    ContentBlock::ToolUse(ToolCall::new(ToolUseId::from("t2"), "b", json!({}))),
                ],
            ),
            // The session emits one Tool message per result; they must coalesce
            // into a single user turn for the alternating-role requirement.
            Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult(ToolResult::success(
                    ToolUseId::from("t1"),
                    "one",
                ))],
            ),
            Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult(ToolResult::success(
                    ToolUseId::from("t2"),
                    "two",
                ))],
            ),
        ];
        let body = provider.build_body(&ModelRequest::new("claude", messages));
        let turns = body["messages"].as_array().unwrap();
        // user("go"), assistant(2 tool_use), then one merged user turn with two
        // tool_result blocks.
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[2]["role"], "user");
        assert_eq!(turns[2]["content"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn into_event_stream_rejects_an_empty_body_without_a_completion_marker() {
        let body = futures::stream::iter(Vec::<reqwest::Result<Vec<u8>>>::new());
        let events: Vec<_> = into_event_stream(body).collect().await;
        assert!(matches!(
            events.last(),
            Some(Err(ProviderError::StreamDecode(message)))
                if message.contains("completion marker")
        ));
    }
}
