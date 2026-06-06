//! OpenAI-compatible provider adapter.
//!
//! Implemented from the public OpenAI Chat Completions API documentation. One
//! adapter serves both a local OpenAI-compatible server (Ollama, vLLM,
//! llama.cpp, local gateways) and the official hosted OpenAI API; only the base
//! URL, auth, and declared source type differ. No private or undocumented
//! endpoint behaviour is used.
//!
//! Provenance: request and streaming shapes implemented from the public OpenAI
//! API reference (<https://platform.openai.com/docs/api-reference/chat>). No
//! private endpoint behaviour, prompts, or identifiers were copied.

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

/// An OpenAI-compatible chat-completions provider.
pub struct OpenAiProvider {
    declaration: ProviderDeclaration,
    client: reqwest::Client,
    base_url: String,
    api_key: Option<Secret>,
    default_options: IndexMap<String, Value>,
}

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

impl OpenAiProvider {
    /// Build a provider against `base_url` (without a trailing `/chat/completions`).
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        display_name: impl Into<String>,
        source_type: SourceType,
        base_url: impl Into<String>,
        api_key: Option<Secret>,
    ) -> Self {
        let id = id.into();
        let auth = if api_key.is_some() {
            AuthRequirement::ApiKey
        } else {
            AuthRequirement::None
        };
        Self {
            declaration: ProviderDeclaration {
                id,
                display_name: display_name.into(),
                source_type,
                supported_input_blocks: vec![
                    InputBlockKind::Text,
                    InputBlockKind::Reasoning,
                    InputBlockKind::ToolResult,
                ],
                tool_call_shape: ToolCallShape::OpenAiToolCalls,
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

    /// Build the JSON request body sent to `/chat/completions`.
    #[must_use]
    pub fn build_body(&self, request: &ModelRequest) -> Value {
        let mut body = json!({
            "model": request.model,
            "messages": translate_messages(&request.messages),
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(request.tools.iter().map(translate_tool).collect());
        }
        if self.suppresses_thinking() && !self.has_option("reasoning_effort", request) {
            body["reasoning_effort"] = json!("minimal");
        }
        if let Value::Object(map) = &mut body {
            for (k, v) in self.default_options.iter().chain(request.options.iter()) {
                if k == "suppress_thinking" {
                    continue;
                }
                map.insert(k.clone(), v.clone());
            }
        }
        body
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    fn suppresses_thinking(&self) -> bool {
        self.default_options
            .get("suppress_thinking")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    fn has_option(&self, key: &str, request: &ModelRequest) -> bool {
        self.default_options.contains_key(key) || request.options.contains_key(key)
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
impl ModelProvider for OpenAiProvider {
    fn declaration(&self) -> &ProviderDeclaration {
        &self.declaration
    }

    async fn stream(&self, request: ModelRequest) -> Result<ModelEventStream, ProviderError> {
        let mut builder = self
            .client
            .post(self.endpoint())
            .json(&self.build_body(&request));
        if let Some(key) = &self.api_key {
            // The credential is set as a header here and never logged.
            builder = builder.bearer_auth(key.expose());
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
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.input_schema,
        }
    })
}

fn translate_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for message in messages {
        translate_message(message, &mut out);
    }
    out
}

fn translate_message(message: &Message, out: &mut Vec<Value>) {
    // Tool results become their own role:"tool" messages, one per result.
    if message.role == Role::Tool {
        for block in &message.content {
            if let ContentBlock::ToolResult(result) = block {
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": result.id.as_str(),
                    "content": result.output,
                }));
            }
        }
        return;
    }

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut reasoning: Option<&str> = None;
    let mut reasoning_signature: Option<&str> = None;

    for block in &message.content {
        match block {
            ContentBlock::Text { text: t } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
            ContentBlock::Reasoning {
                text: r, signature, ..
            } => {
                reasoning = Some(r);
                reasoning_signature = signature.as_deref();
            }
            ContentBlock::ToolUse(call) => {
                tool_calls.push(json!({
                    "id": call.id.as_str(),
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": serde_json::to_string(&call.input).unwrap_or_default(),
                    }
                }));
            }
            _ => {}
        }
    }

    let mut obj = serde_json::Map::new();
    obj.insert("role".to_string(), json!(role_str(message.role)));
    if tool_calls.is_empty() {
        obj.insert("content".to_string(), json!(text));
    } else {
        // OpenAI permits null content alongside tool calls.
        obj.insert(
            "content".to_string(),
            if text.is_empty() {
                Value::Null
            } else {
                json!(text)
            },
        );
        obj.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    // Round-trip reasoning content needed for tool-use continuity.
    if let Some(r) = reasoning {
        obj.insert("reasoning_content".to_string(), json!(r));
    }
    if let Some(sig) = reasoning_signature {
        obj.insert("reasoning_signature".to_string(), json!(sig));
    }
    out.push(Value::Object(obj));
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

async fn classify_error_response(status: u16, response: reqwest::Response) -> ProviderError {
    let request_id = response
        .headers()
        .get("x-request-id")
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
        "openai provider returned an error response"
    );
    let code = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["error"]["code"].as_str().map(str::to_string));
    ProviderError::from_http(status, code.as_deref(), request_id, quota)
}

fn quota_from_headers(headers: &reqwest::header::HeaderMap) -> QuotaInfo {
    let retry_after = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let reset_at = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    let limit_kind = headers
        .get("x-ratelimit-limit-requests")
        .map(|_| "requests".to_string());
    QuotaInfo {
        retry_after,
        reset_at,
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
                Some(Ok(bytes)) => {
                    state.decoder.push(bytes.as_ref(), &mut state.queue);
                }
                Some(Err(err)) => {
                    state
                        .queue
                        .push_back(Err(ProviderError::Network(err.to_string())));
                }
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

/// Incremental decoder for OpenAI-style Server-Sent Events. Each `data:` line is
/// a JSON chunk; tool-call arguments arrive in fragments and are accumulated by
/// index before being emitted as a single assembled [`ModelEvent::ToolCall`].
#[derive(Default)]
struct SseDecoder {
    buf: String,
    tools: BTreeMap<u32, ToolAccum>,
    warned_finish_reasons: BTreeSet<String>,
    saw_finish_reason: bool,
    done: bool,
}

#[derive(Default)]
struct ToolAccum {
    id: Option<String>,
    name: Option<String>,
    args: String,
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
        self.flush_tools(out);
        if !self.done {
            if self.saw_finish_reason {
                self.emit_done(out);
            } else {
                self.done = true;
                out.push_back(Err(ProviderError::StreamDecode(
                    "stream ended before a completion marker".to_string(),
                )));
            }
        }
    }

    fn emit_done(&mut self, out: &mut EventQueue) {
        if !self.done {
            self.done = true;
            out.push_back(Ok(ModelEvent::Done));
        }
    }

    fn process_line(&mut self, line: &str, out: &mut EventQueue) {
        if line.is_empty() {
            return;
        }
        let Some(payload) = line.strip_prefix("data:") else {
            return;
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            self.flush_tools(out);
            self.emit_done(out);
            return;
        }
        match serde_json::from_str::<Value>(payload) {
            Ok(chunk) => self.handle_chunk(&chunk, out),
            Err(e) => out.push_back(Err(ProviderError::StreamDecode(e.to_string()))),
        }
    }

    fn handle_chunk(&mut self, chunk: &Value, out: &mut EventQueue) {
        if let Some(choice) = chunk["choices"].get(0) {
            let delta = &choice["delta"];
            if let Some(content) = delta["content"].as_str() {
                if !content.is_empty() {
                    for event in split_inline_thinking(content) {
                        out.push_back(Ok(event));
                    }
                }
            }
            if let Some(reasoning) = delta["reasoning_content"]
                .as_str()
                .or_else(|| delta["reasoning"].as_str())
            {
                if !reasoning.is_empty() {
                    out.push_back(Ok(ModelEvent::ReasoningDelta(reasoning.to_string())));
                }
            }
            if let Some(tool_calls) = delta["tool_calls"].as_array() {
                for tc in tool_calls {
                    let index = u32::try_from(tc["index"].as_u64().unwrap_or(0)).unwrap_or(0);
                    let acc = self.tools.entry(index).or_default();
                    if let Some(id) = tc["id"].as_str() {
                        if !id.is_empty() {
                            acc.id = Some(id.to_string());
                        }
                    }
                    if let Some(name) = tc["function"]["name"].as_str() {
                        if !name.is_empty() {
                            acc.name = Some(name.to_string());
                        }
                    }
                    if let Some(args) = tc["function"]["arguments"].as_str() {
                        acc.args.push_str(args);
                    }
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.saw_finish_reason = true;
                if reason == "tool_calls" {
                    self.flush_tools(out);
                }
                self.warn_for_finish_reason(reason, out);
            }
        }
        if chunk["usage"].is_object() {
            let usage = &chunk["usage"];
            out.push_back(Ok(ModelEvent::Usage(TokenUsage {
                input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
                output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
            })));
        }
    }

    fn flush_tools(&mut self, out: &mut EventQueue) {
        for (_index, acc) in std::mem::take(&mut self.tools) {
            let Some(name) = acc.name else {
                continue;
            };
            let input_json = if acc.args.trim().is_empty() {
                json!({})
            } else {
                match serde_json::from_str::<Value>(&acc.args) {
                    Ok(value) => value,
                    Err(e) => {
                        out.push_back(Err(ProviderError::StreamDecode(format!(
                            "tool arguments: {e}"
                        ))));
                        continue;
                    }
                }
            };
            out.push_back(Ok(ModelEvent::ToolCall {
                id: acc.id.unwrap_or_default(),
                name,
                input_json,
            }));
        }
    }

    fn warn_for_finish_reason(&mut self, reason: &str, out: &mut EventQueue) {
        if !self.warned_finish_reasons.insert(reason.to_string()) {
            return;
        }
        let message = match reason {
            "stop" | "tool_calls" => None,
            "function_call" => Some(
                "provider returned a legacy function_call finish reason; no tool call was decoded"
                    .to_string(),
            ),
            "length" => Some(
                "provider stopped because the token limit was reached; output may be truncated"
                    .to_string(),
            ),
            "content_filter" => Some("provider filtered part or all of the response".to_string()),
            other if other.trim().is_empty() => None,
            other => Some(format!("provider finished with reason `{other}`")),
        };
        if let Some(message) = message {
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
    fn parses_streaming_text_deltas() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n",
            "data: [DONE]\n",
        ]);
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                Ok(ModelEvent::TextDelta(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        assert!(matches!(events.last(), Some(Ok(ModelEvent::Done))));
    }

    #[test]
    fn assembles_incremental_tool_call_arguments() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.rs\\\"}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: [DONE]\n",
        ]);
        let call = events.iter().find_map(|e| match e {
            Ok(ModelEvent::ToolCall {
                name, input_json, ..
            }) => Some((name.clone(), input_json.clone())),
            _ => None,
        });
        let (name, input) = call.expect("a tool call was emitted");
        assert_eq!(name, "read_file");
        assert_eq!(input["path"], "a.rs");
    }

    #[test]
    fn parses_reasoning_and_usage() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":5}}\n",
            "data: [DONE]\n",
        ]);
        assert!(events
            .iter()
            .any(|e| matches!(e, Ok(ModelEvent::ReasoningDelta(r)) if r == "thinking")));
        assert!(events.iter().any(|e| matches!(
            e,
            Ok(ModelEvent::Usage(u)) if u.input_tokens == 3 && u.output_tokens == 5
        )));
    }

    #[test]
    fn routes_inline_think_tags_to_reasoning() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"answer <think>hidden</think> done\"}}]}\n",
            "data: [DONE]\n",
        ]);
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                Ok(ModelEvent::TextDelta(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "answer  done");
        assert!(events
            .iter()
            .any(|e| matches!(e, Ok(ModelEvent::ReasoningDelta(r)) if r == "hidden")));
    }

    #[test]
    fn suppress_thinking_shapes_openai_request() {
        let mut options = IndexMap::new();
        options.insert("suppress_thinking".to_string(), json!(true));
        let provider = OpenAiProvider::new(
            "local",
            "Local",
            SourceType::LocalServer,
            "http://localhost:1234/v1",
            None,
        )
        .with_default_options(options);
        let body = provider.build_body(&ModelRequest::new("m", Vec::new()));
        assert_eq!(body["reasoning_effort"], "minimal");
        assert!(body.get("suppress_thinking").is_none());
    }

    #[test]
    fn malformed_chunk_yields_typed_decode_error() {
        let events = collect_sse(&["data: {not json}\n"]);
        assert!(events
            .iter()
            .any(|e| matches!(e, Err(ProviderError::StreamDecode(_)))));
    }

    #[test]
    fn length_finish_reason_yields_warning() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":\"length\"}]}\n",
            "data: [DONE]\n",
        ]);
        assert!(events.iter().any(|e| matches!(
            e,
            Ok(ModelEvent::ProviderWarning { message })
                if message.contains("token limit")
        )));
    }

    #[test]
    fn normal_tool_calls_finish_reason_is_quiet() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: [DONE]\n",
        ]);
        assert!(!events
            .iter()
            .any(|e| matches!(e, Ok(ModelEvent::ProviderWarning { .. }))));
        assert!(events
            .iter()
            .any(|e| matches!(e, Ok(ModelEvent::ToolCall { name, .. }) if name == "read_file")));
    }

    #[test]
    fn legacy_function_call_finish_reason_yields_warning() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"function_call\"}]}\n",
            "data: [DONE]\n",
        ]);
        assert!(events.iter().any(|e| matches!(
            e,
            Ok(ModelEvent::ProviderWarning { message })
                if message.contains("legacy function_call")
        )));
    }

    #[test]
    fn request_body_round_trips_reasoning_for_continuity() {
        use localpilot_core::{ContentBlock, Message, Role};
        let provider = OpenAiProvider::new(
            "local",
            "Local",
            SourceType::LocalServer,
            "http://localhost:1234/v1",
            None,
        );
        let message = Message::new(
            Role::Assistant,
            vec![ContentBlock::Reasoning {
                text: "deduce".to_string(),
                signature: Some("sig-123".to_string()),
                provider_metadata: None,
            }],
        );
        let body = provider.build_body(&ModelRequest::new("m", vec![message]));
        let serialized = body.to_string();
        assert!(serialized.contains("deduce"));
        assert!(serialized.contains("sig-123"));
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

    #[test]
    fn rejects_text_when_transport_ends_before_a_completion_marker() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"Let me start by understanding the p\"}}]}\n",
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
    fn finish_reason_is_a_completion_marker_for_compatible_servers() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"complete\"},\"finish_reason\":\"stop\"}]}\n",
        ]);
        assert!(matches!(events.last(), Some(Ok(ModelEvent::Done))));
    }
}
