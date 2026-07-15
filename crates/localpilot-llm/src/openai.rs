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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use indexmap::IndexMap;
use localpilot_core::{ContentBlock, Message, Role, Secret, TokenUsage};
use serde_json::{json, Value};

use crate::auth::AuthProvider;
use crate::error::{ProviderError, QuotaInfo};
use crate::event::{InlineThinkingFilter, ModelEvent, ModelEventStream};
use crate::headers::{parse_compact_duration, parse_retry_after};
use crate::provider::{
    AuthRequirement, Capabilities, InputBlockKind, ModelProvider, ProviderDeclaration,
    ReasoningShape, SourceType, ToolCallShape,
};
use crate::request::{ModelRequest, ToolSpec};

/// How a tool-call constraint is encoded in the request body. A local server may
/// accept the OpenAI structured-output `response_format` wrapper, the documented
/// llama.cpp top-level `json_schema` field, or neither (then the F2 fallback drops
/// the constraint and uses native tool-calling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConstraintMode {
    /// OpenAI structured outputs: `response_format: { type: json_schema, ... }`.
    /// The default and the floor — unchanged for every existing provider.
    ResponseFormat,
    /// llama.cpp server extension: a top-level `json_schema` field the server
    /// compiles to a GBNF grammar. Opt-in for a server that rejects the wrapper.
    JsonSchema,
    /// A top-level GBNF `grammar` string. The constraint is emitted as a hand-built
    /// grammar (valid tool call: a known tool name + a JSON-object arguments
    /// payload) rather than a JSON schema. A turboquant `llama-server`'s
    /// lazy-grammar accepts this even when the model emits a `<think>` prefix that
    /// the json-schema→grammar path rejects (live finding, ADR-0044). Argument
    /// payloads are constrained to *valid JSON*, not to each tool's argument
    /// schema (that finer constraint is a follow-up).
    Grammar,
}

/// An OpenAI-compatible chat-completions provider.
pub struct OpenAiProvider {
    declaration: ProviderDeclaration,
    client: reqwest::Client,
    /// Longest silence tolerated while a response is open — from sending the
    /// request to the first byte, and between stream chunks after that. A
    /// liveness bound, not a total-duration bound: a slow local server that
    /// keeps streaming is never cut off mid-response (total turn duration is
    /// governed by `[harness] turn_timeout_secs`, not the HTTP layer).
    stall_timeout: Duration,
    base_url: String,
    auth: OpenAiAuth,
    default_options: IndexMap<String, Value>,
    /// Set once a server that *declares* constrained decoding rejects the schema
    /// constraint (a client error on a constrained request). After that, the
    /// constraint is dropped up-front for the rest of this provider's life rather
    /// than re-sent and rejected every turn. Interior-mutable so a shared
    /// `Arc<dyn ModelProvider>` can flip it.
    constrained_rejected: Arc<AtomicBool>,
}

enum OpenAiAuth {
    None,
    ApiKey(Secret),
    Dynamic(Arc<dyn AuthProvider>),
}

/// Default stall window (`request_timeout_secs`): the longest silence
/// tolerated on an open response before the request is abandoned. At healthy
/// local-inference speeds this is far more prompt-processing time than any
/// realistic context needs; when it trips, the server is hung or running at
/// CPU speed — both worth surfacing over waiting forever.
const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(600);

/// TCP connect budget. Separate from the stall window: an unreachable server
/// should fail in seconds, not minutes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

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
        let auth = api_key.map_or(OpenAiAuth::None, OpenAiAuth::ApiKey);
        Self::new_with_auth(id, display_name, source_type, base_url, auth)
    }

    /// Build a provider whose bearer token is produced dynamically per request.
    #[must_use]
    pub fn new_with_auth_provider(
        id: impl Into<String>,
        display_name: impl Into<String>,
        source_type: SourceType,
        base_url: impl Into<String>,
        auth_provider: Arc<dyn AuthProvider>,
    ) -> Self {
        Self::new_with_auth(
            id,
            display_name,
            source_type,
            base_url,
            OpenAiAuth::Dynamic(auth_provider),
        )
    }

    fn new_with_auth(
        id: impl Into<String>,
        display_name: impl Into<String>,
        source_type: SourceType,
        base_url: impl Into<String>,
        auth: OpenAiAuth,
    ) -> Self {
        let id = id.into();
        let auth_requirement = match &auth {
            OpenAiAuth::None => AuthRequirement::None,
            OpenAiAuth::ApiKey(_) => AuthRequirement::ApiKey,
            OpenAiAuth::Dynamic(_) => AuthRequirement::BearerToken,
        };
        // Image input is the hosted OpenAI vision path; a local OpenAI-compatible
        // server is not assumed to accept images, so gate on the source.
        let mut supported_input_blocks = vec![
            InputBlockKind::Text,
            InputBlockKind::Reasoning,
            InputBlockKind::ToolResult,
        ];
        if matches!(source_type, SourceType::OfficialApi) {
            supported_input_blocks.push(InputBlockKind::Image);
        }
        Self {
            declaration: ProviderDeclaration {
                id,
                display_name: display_name.into(),
                source_type,
                supported_input_blocks,
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
                    // A local OpenAI-compatible server (llama-server) supports a
                    // json_schema constraint; a hosted OpenAI endpoint does not
                    // expose one through this path, so gate on the source.
                    constrained_decoding: matches!(source_type, SourceType::LocalServer),
                },
                max_context_tokens: None,
                auth: auth_requirement,
                rate_limit_behavior: None,
            },
            client: reqwest_client(),
            stall_timeout: DEFAULT_STALL_TIMEOUT,
            base_url: base_url.into(),
            auth,
            default_options: IndexMap::new(),
            constrained_rejected: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Override the stall window (`request_timeout_secs`): the longest
    /// tolerated silence on an open response, not a total request deadline.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.stall_timeout = timeout.unwrap_or(DEFAULT_STALL_TIMEOUT);
        self
    }

    /// Provider-level request options merged into every request body before
    /// request-specific options.
    #[must_use]
    pub fn with_default_options(mut self, options: IndexMap<String, Value>) -> Self {
        self.default_options = options;
        self
    }

    /// Declare the model's context window, consumed by the session budget.
    #[must_use]
    pub fn with_max_context_tokens(mut self, tokens: Option<u64>) -> Self {
        self.declaration.max_context_tokens = tokens;
        self
    }

    /// Resolve image (vision) input from a user/launcher declaration. `Some(true)`
    /// lifts the image-input gate even for a local server (which is otherwise not
    /// assumed to accept images); `Some(false)`/`None` leaves the gate as the
    /// source-type set it, so an undeclared provider is byte-identical to today.
    /// The official-API path already declares `Image`, so this only ever adds it.
    #[must_use]
    pub fn with_declared_vision(mut self, supports_vision: Option<bool>) -> Self {
        if supports_vision == Some(true)
            && !self
                .declaration
                .supported_input_blocks
                .contains(&InputBlockKind::Image)
        {
            self.declaration
                .supported_input_blocks
                .push(InputBlockKind::Image);
        }
        self
    }

    /// Build the JSON request body sent to `/chat/completions`.
    #[must_use]
    pub fn build_body(&self, request: &ModelRequest) -> Value {
        let mut body = json!({
            "model": request.model,
            "messages": translate_messages(&request.messages, self.round_trips_reasoning()),
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(request.tools.iter().map(translate_tool).collect());
        }
        // A constrained-decoding server accepts a JSON-schema constraint. Absent
        // for every other provider, so the body is unchanged for them. Skipped
        // once a server has rejected the constraint (`constrained_rejected`, set
        // in `stream`): it won't be accepted later either, so don't re-send it and
        // pay a rejected round-trip every turn. The encoding is selectable
        // (`constraint_mode` option) because not every local server accepts the
        // OpenAI structured-output `response_format` wrapper.
        if let Some(constraint) = &request.tool_constraint {
            if !self.constrained_rejected.load(Ordering::Relaxed) {
                match self.constraint_mode() {
                    ConstraintMode::JsonSchema => {
                        // A documented llama.cpp server extension: a top-level
                        // `json_schema` field is converted to a GBNF grammar
                        // server-side, engaging the grammar on a llama.cpp build
                        // (e.g. a turboquant server) that rejects the OpenAI
                        // `response_format` structured-output wrapper. See
                        // docs/04-provider-contract.md for provenance.
                        body["json_schema"] = constraint.clone();
                    }
                    ConstraintMode::Grammar => {
                        // Build the GBNF from the tool names (not the JSON-schema
                        // constraint): a turboquant server's lazy-grammar accepts
                        // a top-level `grammar` even with a `<think>` prefix, where
                        // the json-schema path 400s.
                        if !request.tools.is_empty() {
                            body["grammar"] = json!(tool_call_grammar(&request.tools));
                        }
                    }
                    ConstraintMode::ResponseFormat => {
                        body["response_format"] = json!({
                            "type": "json_schema",
                            "json_schema": { "name": "tool_call", "schema": constraint },
                        });
                    }
                }
            }
        }
        if self.suppresses_thinking() && !self.has_option("reasoning_effort", request) {
            body["reasoning_effort"] = json!("minimal");
        }
        if let Value::Object(map) = &mut body {
            for (k, v) in self.default_options.iter().chain(request.options.iter()) {
                if k == "suppress_thinking" || k == "reasoning_round_trip" || k == "constraint_mode"
                {
                    continue;
                }
                map.insert(k.clone(), v.clone());
            }
        }
        // An explicit per-request effort overrides any option default; this is
        // the documented `reasoning_effort` request field on effort-aware
        // OpenAI-compatible servers.
        if let Some(effort) = request.reasoning_effort {
            body["reasoning_effort"] = json!(effort.as_str());
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

    /// How a tool-call constraint is encoded on the wire. Defaults to the OpenAI
    /// structured-output `response_format` wrapper; a provider whose local server
    /// rejects that wrapper (e.g. a turboquant llama.cpp build) opts into the
    /// documented top-level `json_schema` field via the `constraint_mode` option.
    /// An unknown value falls back to the default, so a typo never breaks a turn.
    fn constraint_mode(&self) -> ConstraintMode {
        match self
            .default_options
            .get("constraint_mode")
            .and_then(Value::as_str)
        {
            Some("json_schema") => ConstraintMode::JsonSchema,
            Some("grammar") => ConstraintMode::Grammar,
            _ => ConstraintMode::ResponseFormat,
        }
    }

    fn has_option(&self, key: &str, request: &ModelRequest) -> bool {
        self.default_options.contains_key(key) || request.options.contains_key(key)
    }

    /// Whether assistant reasoning round-trips as `reasoning_content` /
    /// `reasoning_signature` message fields. These keys are a local-inference
    /// convention (e.g. vLLM-style servers), not documented hosted-OpenAI
    /// fields, and strict servers may reject unknown message fields — so they
    /// are sent only to non-official endpoints unless the provider option
    /// `reasoning_round_trip` overrides the default.
    fn round_trips_reasoning(&self) -> bool {
        self.default_options
            .get("reasoning_round_trip")
            .and_then(Value::as_bool)
            .unwrap_or(self.declaration.source_type != SourceType::OfficialApi)
    }
}

/// Build a GBNF grammar constraining the output to a single valid tool call:
/// `{ "name": <one of the tool names>, "arguments": <any JSON object> }`. The
/// JSON sub-grammar is authored from the JSON specification (original; not copied
/// from any project's grammar file). Argument payloads are constrained to valid
/// JSON, not to each tool's own argument schema — the per-schema constraint is a
/// follow-up. `tools` is assumed non-empty (the caller skips the empty case).
fn tool_call_grammar(tools: &[ToolSpec]) -> String {
    // Each tool name as a GBNF double-quoted string literal, `"` and `\` escaped.
    let names = tools
        .iter()
        .map(|tool| {
            let escaped = tool.name.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"\\\"{escaped}\\\"\"")
        })
        .collect::<Vec<_>>()
        .join(" | ");
    format!(
        concat!(
            "root    ::= \"{{\" ws \"\\\"name\\\"\" ws \":\" ws name ws \",\" ws ",
            "\"\\\"arguments\\\"\" ws \":\" ws object ws \"}}\"\n",
            "name    ::= {names}\n",
            "value   ::= object | array | string | number | \"true\" | \"false\" | \"null\"\n",
            "object  ::= \"{{\" ws ( string ws \":\" ws value ( ws \",\" ws string ws \":\" ws value )* )? ws \"}}\"\n",
            "array   ::= \"[\" ws ( value ( ws \",\" ws value )* )? ws \"]\"\n",
            "string  ::= \"\\\"\" ( [^\"\\\\] | \"\\\\\" [\"\\\\/bfnrt] | \"\\\\u\" [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F] )* \"\\\"\"\n",
            "number  ::= \"-\"? ( \"0\" | [1-9] [0-9]* ) ( \".\" [0-9]+ )? ( [eE] [-+]? [0-9]+ )?\n",
            "ws      ::= [ \\t\\n]*\n",
        ),
        names = names,
    )
}

fn reqwest_client() -> reqwest::Client {
    // No whole-request `.timeout()`: it would put a hard deadline on the total
    // duration of a streamed response, cutting off a slow-but-healthy local
    // server mid-generation. Liveness is enforced per await instead — the
    // stall window around opening the response and reading each chunk.
    let builder = reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT);
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
        match &self.auth {
            OpenAiAuth::None => {}
            OpenAiAuth::ApiKey(key) => {
                // The credential is set as a header here and never logged.
                builder = builder.bearer_auth(key.expose());
            }
            OpenAiAuth::Dynamic(provider) => {
                let token = provider.access_token().await?;
                builder = builder.bearer_auth(token.expose());
            }
        }
        tracing::debug!(model = %request.model, "starting provider stream");

        // Opening the response (connect, request write, response headers —
        // which a local server may hold back until prompt processing ends) is
        // bounded by the stall window, not a total-request deadline.
        let response = tokio::time::timeout(self.stall_timeout, builder.send())
            .await
            .map_err(|_| ProviderError::stream_stalled(self.stall_timeout))??;
        let status = response.status();
        // Degrade gracefully: a server that declares the capability but rejects
        // the schema constraint (a client error on a constrained request) must
        // not break the turn. Retry once without the constraint — native
        // tool-calling — recording the fallback reason. The retry carries no
        // constraint, so this guard cannot recurse.
        if status.is_client_error() && request.tool_constraint.is_some() {
            tracing::warn!(
                status = status.as_u16(),
                model = %request.model,
                "constrained-decoding request was rejected; disabling it for this provider and falling back to native tool-calling"
            );
            self.constrained_rejected.store(true, Ordering::Relaxed);
            let mut fallback = request.clone();
            fallback.tool_constraint = None;
            return self.stream(fallback).await;
        }
        if !status.is_success() {
            return Err(classify_error_response(status.as_u16(), response).await);
        }

        let body = response.bytes_stream();
        Ok(into_event_stream(body, self.stall_timeout))
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

fn translate_messages(messages: &[Message], round_trip_reasoning: bool) -> Vec<Value> {
    let mut out = Vec::new();
    let mut in_leading_system = true;
    for message in messages {
        // Only the leading run of system messages is the setup prompt. A system
        // message appearing later (e.g. host-injected retrieved context) keeps
        // its position but is delivered as user-role content: it is not reordered
        // to the front (docs/04 §Late System Messages), and many model chat
        // templates reject a system message that is not the first message.
        let role_override = if message.role == Role::System && !in_leading_system {
            Some("user")
        } else {
            None
        };
        if message.role != Role::System {
            in_leading_system = false;
        }
        translate_message(message, role_override, round_trip_reasoning, &mut out);
    }
    out
}

fn translate_message(
    message: &Message,
    role_override: Option<&'static str>,
    round_trip_reasoning: bool,
    out: &mut Vec<Value>,
) {
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
    let mut images = Vec::new();
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
                let mut tool_call = serde_json::Map::new();
                tool_call.insert("id".to_string(), json!(call.id.as_str()));
                tool_call.insert("type".to_string(), json!("function"));
                tool_call.insert(
                    "function".to_string(),
                    json!({
                        "name": call.name,
                        "arguments": serde_json::to_string(&call.input).unwrap_or_default(),
                    }),
                );
                if let Some(metadata) = call.provider_metadata.as_ref().and_then(Value::as_object) {
                    for (key, value) in metadata {
                        tool_call
                            .entry(key.clone())
                            .or_insert_with(|| value.clone());
                    }
                }
                tool_calls.push(Value::Object(tool_call));
            }
            ContentBlock::Image { media_type, data } => {
                images.push(json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{media_type};base64,{data}") },
                }));
            }
            _ => {}
        }
    }

    let mut obj = serde_json::Map::new();
    obj.insert(
        "role".to_string(),
        json!(role_override.unwrap_or_else(|| role_str(message.role))),
    );
    if !images.is_empty() {
        // Multimodal input: content becomes an ordered parts array (text first,
        // then the images) rather than a bare string.
        let mut parts = Vec::with_capacity(images.len() + 1);
        if !text.is_empty() {
            parts.push(json!({ "type": "text", "text": text }));
        }
        parts.extend(images);
        obj.insert("content".to_string(), Value::Array(parts));
    } else if tool_calls.is_empty() {
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
    // Round-trip reasoning content needed for tool-use continuity, but only to
    // endpoints that opt in to the non-standard fields.
    if round_trip_reasoning {
        if let Some(r) = reasoning {
            obj.insert("reasoning_content".to_string(), json!(r));
        }
        if let Some(sig) = reasoning_signature {
            obj.insert("reasoning_signature".to_string(), json!(sig));
        }
    }
    out.push(Value::Object(obj));
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        // A surfaced user shell run reads to the model as user content.
        Role::User | Role::UserShell => "user",
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
    let (code, message) = serde_json::from_str::<Value>(&body)
        .ok()
        .map(|v| {
            (
                v["error"]["code"].as_str().map(str::to_string),
                v["error"]["message"].as_str().map(str::to_string),
            )
        })
        .unwrap_or((None, None));
    let mut err = ProviderError::from_http(status, code.as_deref(), request_id, quota);
    if matches!(err, ProviderError::InvalidRequest { .. }) {
        // Structured `{"error": {"message": ...}}` extraction failed — the body
        // wasn't JSON, or didn't carry a string `.error.message` (a proxy/gateway
        // error, or a differently-shaped provider error). Surface the raw body
        // instead of the bare "bad request" literal, so the provider's real
        // rejection reason is still visible instead of a dead-end message.
        let detail = message.or_else(|| truncated_error_detail(&body));
        if let Some(message) = detail {
            err = ProviderError::InvalidRequest { message };
        }
    }
    err
}

/// The `InvalidRequest` fallback message when no structured `error.message`
/// could be extracted: the raw response body, trimmed and bounded so an
/// HTML/plain-text error page can't flood the UI. `None` for an empty body,
/// which leaves the generic "bad request" literal from `ProviderError::from_http`.
fn truncated_error_detail(body: &str) -> Option<String> {
    const MAX_ERROR_BODY_CHARS: usize = 500;
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= MAX_ERROR_BODY_CHARS {
        return Some(trimmed.to_string());
    }
    let mut truncated: String = trimmed.chars().take(MAX_ERROR_BODY_CHARS).collect();
    truncated.push_str(" …(truncated)");
    Some(truncated)
}

fn quota_from_headers(headers: &reqwest::header::HeaderMap) -> QuotaInfo {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    // `retry-after` is delay-seconds or an HTTP-date; the documented
    // per-window reset headers carry compact duration strings ("1s", "6m0s").
    // Unparseable values degrade to absent metadata, never an error.
    let retry_after = header("retry-after")
        .and_then(|value| parse_retry_after(value, std::time::SystemTime::now()));
    let requests_reset = header("x-ratelimit-reset-requests").and_then(parse_compact_duration);
    let tokens_reset = header("x-ratelimit-reset-tokens").and_then(parse_compact_duration);
    let (window_reset, limit_kind) = match (requests_reset, tokens_reset) {
        (Some(requests), Some(tokens)) if tokens > requests => {
            (Some(tokens), Some("tokens".to_string()))
        }
        (Some(requests), _) => (Some(requests), Some("requests".to_string())),
        (None, Some(tokens)) => (Some(tokens), Some("tokens".to_string())),
        (None, None) => (None, None),
    };
    QuotaInfo {
        retry_after: retry_after.or(window_reset),
        reset_at: None,
        limit_kind,
        retryable: true,
        raw_provider_code: None,
    }
}

fn into_event_stream<S, B>(body: S, stall_timeout: Duration) -> ModelEventStream
where
    S: futures::Stream<Item = reqwest::Result<B>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
{
    struct StreamState<S> {
        body: std::pin::Pin<Box<S>>,
        decoder: SseDecoder,
        queue: VecDeque<Result<ModelEvent, ProviderError>>,
        /// Set once the stall window elapsed between chunks; the stream ends
        /// after the queued stall error is drained instead of polling a body
        /// that already proved silent.
        stalled: bool,
    }

    let state = StreamState {
        body: Box::pin(body),
        decoder: SseDecoder::default(),
        queue: VecDeque::new(),
        stalled: false,
    };

    futures::stream::unfold(state, move |mut state| async move {
        loop {
            if let Some(item) = state.queue.pop_front() {
                return Some((item, state));
            }
            if state.stalled {
                return None;
            }
            match tokio::time::timeout(stall_timeout, state.body.next()).await {
                Err(_elapsed) => {
                    state.stalled = true;
                    state
                        .queue
                        .push_back(Err(ProviderError::stream_stalled(stall_timeout)));
                }
                Ok(Some(Ok(bytes))) => {
                    state.decoder.push(bytes.as_ref(), &mut state.queue);
                }
                Ok(Some(Err(err))) => {
                    state
                        .queue
                        .push_back(Err(ProviderError::from_response_body_error(err)));
                }
                Ok(None) => {
                    state.decoder.finish(&mut state.queue);
                    return state.queue.pop_front().map(|item| (item, state));
                }
            }
        }
    })
    .boxed()
}

type EventQueue = VecDeque<Result<ModelEvent, ProviderError>>;

/// The accumulation key for a streamed tool call: by `index` when the server
/// provides one, otherwise by `id`, so a server that omits `index` on parallel
/// tool calls cannot merge distinct calls into one accumulator.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ToolKey {
    Index(u64),
    Id(String),
}

/// Incremental decoder for OpenAI-style Server-Sent Events. Each `data:` line is
/// a JSON chunk; tool-call arguments arrive in fragments and are accumulated by
/// index (or id, when the server omits `index`) before being emitted as a single
/// assembled [`ModelEvent::ToolCall`]. Raw bytes are buffered and only complete
/// lines are decoded, so a multi-byte UTF-8 character split across network
/// chunks is never corrupted.
/// Drives the SSE decoder over arbitrary bytes for the fuzz harness: the
/// input is split at a fuzzer-chosen point so chunk-boundary buffering is
/// exercised, then finished, with every produced event consumed.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_sse_decoder(data: &[u8]) {
    let mut out = EventQueue::new();
    let mut decoder = SseDecoder::default();
    let split = data
        .first()
        .map(|byte| usize::from(*byte) % data.len().max(1))
        .unwrap_or(0);
    let (head, tail) = data.split_at(split.min(data.len()));
    decoder.push(head, &mut out);
    decoder.push(tail, &mut out);
    decoder.finish(&mut out);
    out.clear();
}

#[derive(Default)]
struct SseDecoder {
    buf: Vec<u8>,
    thinking: InlineThinkingFilter,
    tools: BTreeMap<ToolKey, ToolAccum>,
    last_keyless: Option<ToolKey>,
    warned_finish_reasons: BTreeSet<String>,
    saw_finish_reason: bool,
    output_limited: bool,
    done: bool,
}

#[derive(Default)]
struct ToolAccum {
    id: Option<String>,
    name: Option<String>,
    args: String,
    provider_metadata: Option<Value>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8], out: &mut EventQueue) {
        // Buffer raw bytes; only complete lines are decoded. A multi-byte
        // character cannot contain a newline byte, so splitting at `\n` never
        // splits a character.
        self.buf.extend_from_slice(bytes);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            self.process_line(line.trim(), out);
        }
    }

    fn finish(&mut self, out: &mut EventQueue) {
        if !self.buf.is_empty() {
            let tail = std::mem::take(&mut self.buf);
            let line = String::from_utf8_lossy(&tail);
            if !line.trim().is_empty() {
                self.process_line(line.trim(), out);
            }
        }
        self.flush_thinking(out);
        if !self.output_limited {
            self.flush_tools(out);
        } else {
            self.discard_tools();
        }
        if !self.done {
            if self.saw_finish_reason {
                self.emit_done(out);
            } else {
                self.done = true;
                out.push_back(Err(ProviderError::StreamTruncated {
                    detail: "stream ended before a completion marker".to_string(),
                }));
            }
        }
    }

    fn flush_thinking(&mut self, out: &mut EventQueue) {
        for event in self.thinking.finish() {
            out.push_back(Ok(event));
        }
    }

    fn emit_done(&mut self, out: &mut EventQueue) {
        if !self.done {
            self.flush_thinking(out);
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
                    for event in self.thinking.push(content) {
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
                    let key = self.tool_key(tc);
                    let acc = self.tools.entry(key).or_default();
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
                    if let Some(extra_content) = tc.get("extra_content") {
                        let mut metadata = acc
                            .provider_metadata
                            .take()
                            .and_then(|value| value.as_object().cloned())
                            .unwrap_or_default();
                        metadata.insert("extra_content".to_string(), extra_content.clone());
                        acc.provider_metadata = Some(Value::Object(metadata));
                    }
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.saw_finish_reason = true;
                if reason == "length" {
                    self.output_limited = true;
                    self.discard_tools();
                }
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

    /// The accumulator key for one tool-call fragment. Servers normally send
    /// `index`; when it is absent, fragments carrying an id key by id, and
    /// id-less continuation fragments attach to the last id-keyed accumulator.
    fn tool_key(&mut self, tc: &Value) -> ToolKey {
        if let Some(index) = tc["index"].as_u64() {
            return ToolKey::Index(index);
        }
        match tc["id"].as_str() {
            Some(id) if !id.is_empty() => {
                let key = ToolKey::Id(id.to_string());
                self.last_keyless = Some(key.clone());
                key
            }
            _ => self
                .last_keyless
                .clone()
                .unwrap_or(ToolKey::Id(String::new())),
        }
    }

    fn flush_tools(&mut self, out: &mut EventQueue) {
        for (_key, acc) in std::mem::take(&mut self.tools) {
            let Some(name) = acc.name else {
                continue;
            };
            let input_json = if acc.args.trim().is_empty() {
                json!({})
            } else {
                match serde_json::from_str::<Value>(&acc.args) {
                    Ok(value) => value,
                    Err(e) => {
                        // Carry the tool name and argument size so the harness can
                        // recover an oversized write specifically, rather than
                        // only re-prompting blindly.
                        out.push_back(Err(ProviderError::MalformedToolArguments {
                            tool: name,
                            bytes: acc.args.len(),
                            reason: e.to_string(),
                        }));
                        continue;
                    }
                }
            };
            out.push_back(Ok(ModelEvent::ToolCall {
                id: acc.id.unwrap_or_default(),
                name,
                input_json,
                provider_metadata: acc.provider_metadata,
            }));
        }
    }

    fn discard_tools(&mut self) {
        self.tools.clear();
        self.last_keyless = None;
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
            if reason == "length" {
                out.push_back(Ok(ModelEvent::OutputLimit { message }));
                return;
            }
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
    fn preserves_gemini_tool_call_extra_content() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\",\"extra_content\":{\"google\":{\"thought_signature\":\"sig-123\"}},\"function\":{\"name\":\"list_files\",\"arguments\":\"{}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: [DONE]\n",
        ]);
        let metadata = events.iter().find_map(|e| match e {
            Ok(ModelEvent::ToolCall {
                provider_metadata, ..
            }) => provider_metadata.clone(),
            _ => None,
        });
        assert_eq!(
            metadata,
            Some(json!({
                "extra_content": {
                    "google": {
                        "thought_signature": "sig-123"
                    }
                }
            }))
        );
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
            Ok(ModelEvent::OutputLimit { message })
                if message.contains("token limit")
        )));
    }

    #[test]
    fn length_finish_discards_partial_tool_arguments_without_decode_error() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n",
            "data: [DONE]\n",
        ]);

        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ModelEvent::OutputLimit { message }) if message.contains("token limit")
        )));
        // The partial arguments are discarded on a length finish, so neither a
        // decode error nor a malformed-arguments error is emitted.
        assert!(!events.iter().any(|event| matches!(
            event,
            Err(ProviderError::StreamDecode(message)) if message.contains("tool arguments")
        )));
        assert!(!events
            .iter()
            .any(|event| matches!(event, Err(ProviderError::MalformedToolArguments { .. }))));
        assert!(!events
            .iter()
            .any(|event| matches!(event, Ok(ModelEvent::ToolCall { .. }))));
    }

    #[test]
    fn malformed_tool_arguments_carry_the_tool_name() {
        // A complete-but-invalid argument payload (finished normally) surfaces a
        // typed MalformedToolArguments naming the tool, so the harness can steer
        // an oversized write to a chunked retry.
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{ not json\"}}]},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: [DONE]\n",
        ]);
        assert!(events.iter().any(|event| matches!(
            event,
            Err(ProviderError::MalformedToolArguments { tool, bytes, .. })
                if tool == "write_file" && *bytes > 0
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

    #[test]
    fn hosted_openai_accepts_images_and_serializes_a_parts_array() {
        use localpilot_core::{ContentBlock, Message, Role};
        let provider = OpenAiProvider::new(
            "openai",
            "OpenAI",
            SourceType::OfficialApi,
            "https://api.openai.com/v1",
            None,
        );
        assert!(provider
            .declaration()
            .supported_input_blocks
            .contains(&InputBlockKind::Image));
        let message = Message::new(
            Role::User,
            vec![
                ContentBlock::text("describe this"),
                ContentBlock::image("image/png", "aGVsbG8="),
            ],
        );
        let body = provider.build_body(&ModelRequest::new("m", vec![message]));
        let content = &body["messages"][0]["content"];
        assert!(content.is_array(), "image input must use a parts array");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );
    }

    #[test]
    fn local_openai_compatible_server_does_not_advertise_image_input() {
        let provider = OpenAiProvider::new(
            "local",
            "Local",
            SourceType::LocalServer,
            "http://localhost:1234/v1",
            None,
        );
        assert!(!provider
            .declaration()
            .supported_input_blocks
            .contains(&InputBlockKind::Image));
    }

    #[test]
    fn a_declared_vision_local_server_advertises_image_input() {
        // A local server keeps text-only until vision is declared; declaring it
        // lifts the gate the official-API path gets for free.
        let provider = OpenAiProvider::new(
            "local",
            "Local",
            SourceType::LocalServer,
            "http://localhost:1234/v1",
            None,
        )
        .with_declared_vision(Some(true));
        assert!(provider
            .declaration()
            .supported_input_blocks
            .contains(&InputBlockKind::Image));
    }

    #[test]
    fn an_undeclared_or_off_vision_local_server_stays_text_only() {
        for declared in [None, Some(false)] {
            let provider = OpenAiProvider::new(
                "local",
                "Local",
                SourceType::LocalServer,
                "http://localhost:1234/v1",
                None,
            )
            .with_declared_vision(declared);
            assert!(
                !provider
                    .declaration()
                    .supported_input_blocks
                    .contains(&InputBlockKind::Image),
                "supports_vision = {declared:?} must not advertise image input"
            );
        }
    }

    #[test]
    fn declaring_vision_on_the_official_api_does_not_duplicate_image_input() {
        // The official API already advertises Image; a redundant declaration must
        // not push a second entry.
        let provider = OpenAiProvider::new(
            "openai",
            "OpenAI",
            SourceType::OfficialApi,
            "https://api.openai.com/v1",
            None,
        )
        .with_declared_vision(Some(true));
        let images = provider
            .declaration()
            .supported_input_blocks
            .iter()
            .filter(|kind| **kind == InputBlockKind::Image)
            .count();
        assert_eq!(images, 1);
    }

    #[test]
    fn constrained_request_is_dropped_after_a_rejection() {
        let provider = OpenAiProvider::new(
            "local",
            "Local",
            SourceType::LocalServer,
            "http://localhost:1234/v1",
            None,
        );
        let mut request = ModelRequest::new("m", Vec::new());
        request.tool_constraint = Some(serde_json::json!({ "type": "object" }));

        // Before any rejection, the schema constraint rides as `response_format`.
        assert!(
            provider
                .build_body(&request)
                .get("response_format")
                .is_some(),
            "the constraint must be sent before any rejection"
        );

        // Once a server has rejected it, the constraint is dropped up-front so it
        // is not re-sent and rejected every turn.
        provider
            .constrained_rejected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            provider
                .build_body(&request)
                .get("response_format")
                .is_none(),
            "the constraint must be dropped after a rejection is recorded"
        );
    }

    #[test]
    fn json_schema_constraint_mode_emits_the_top_level_field() {
        // A server that rejects the OpenAI `response_format` wrapper (e.g. a
        // turboquant llama.cpp build) opts into the documented top-level
        // `json_schema` field, which the server compiles to a grammar.
        let mut options = IndexMap::new();
        options.insert("constraint_mode".to_string(), json!("json_schema"));
        let provider = OpenAiProvider::new(
            "local",
            "Local",
            SourceType::LocalServer,
            "http://localhost:1234/v1",
            None,
        )
        .with_default_options(options);
        let mut request = ModelRequest::new("m", Vec::new());
        let schema = json!({ "type": "object" });
        request.tool_constraint = Some(schema.clone());

        let body = provider.build_body(&request);
        assert_eq!(
            body.get("json_schema"),
            Some(&schema),
            "json_schema mode must send the constraint as a top-level json_schema field"
        );
        assert!(
            body.get("response_format").is_none(),
            "json_schema mode must not also send the response_format wrapper"
        );
        // The mode selector itself must never leak into the request body.
        assert!(
            body.get("constraint_mode").is_none(),
            "constraint_mode is a local selector, not a wire field"
        );
    }

    #[test]
    fn grammar_constraint_mode_emits_a_gbnf_grammar_field() {
        // A server whose json-schema→grammar path rejects a `<think>` prefix opts
        // into a top-level GBNF `grammar` built from the tool names.
        let mut options = IndexMap::new();
        options.insert("constraint_mode".to_string(), json!("grammar"));
        let provider = OpenAiProvider::new(
            "local",
            "Local",
            SourceType::LocalServer,
            "http://localhost:1234/v1",
            None,
        )
        .with_default_options(options);
        let mut request = ModelRequest::new("m", Vec::new());
        request.tools = vec![ToolSpec {
            name: "read_file".to_string(),
            description: "read".to_string(),
            input_schema: json!({ "type": "object" }),
        }];
        request.tool_constraint = Some(json!({ "type": "object" }));

        let body = provider.build_body(&request);
        let grammar = body
            .get("grammar")
            .and_then(Value::as_str)
            .expect("grammar mode must send a top-level grammar string");
        assert!(grammar.contains("root"), "grammar must define a root rule");
        assert!(
            grammar.contains("\\\"read_file\\\""),
            "grammar must constrain the name to the tool: {grammar}"
        );
        assert!(body.get("response_format").is_none());
        assert!(body.get("json_schema").is_none());
    }

    #[test]
    fn default_constraint_mode_is_unchanged_response_format() {
        // No option set: the floor is unchanged — the constraint still rides as the
        // OpenAI `response_format` wrapper, with no top-level json_schema field.
        let provider = OpenAiProvider::new(
            "local",
            "Local",
            SourceType::LocalServer,
            "http://localhost:1234/v1",
            None,
        );
        let mut request = ModelRequest::new("m", Vec::new());
        request.tool_constraint = Some(json!({ "type": "object" }));
        let body = provider.build_body(&request);
        assert!(body.get("response_format").is_some());
        assert!(body.get("json_schema").is_none());
    }

    #[tokio::test]
    async fn a_silent_body_stalls_with_guidance_instead_of_hanging() {
        // A connection that stays open but never delivers a byte must trip the
        // stall window and end the stream — not hang the turn forever, and not
        // read as a server-side truncation.
        let body = futures::stream::pending::<reqwest::Result<Vec<u8>>>();
        let events: Vec<_> = into_event_stream(body, Duration::from_millis(50))
            .collect()
            .await;
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events.first(),
            Some(Err(ProviderError::StreamStalled { .. }))
        ));
    }

    #[tokio::test]
    async fn into_event_stream_rejects_an_empty_body_without_a_completion_marker() {
        let body = futures::stream::iter(Vec::<reqwest::Result<Vec<u8>>>::new());
        let events: Vec<_> = into_event_stream(body, DEFAULT_STALL_TIMEOUT)
            .collect()
            .await;
        assert!(matches!(
            events.last(),
            Some(Err(ProviderError::StreamTruncated { detail }))
                if detail.contains("completion marker")
        ));
    }

    #[test]
    fn rejects_text_when_transport_ends_before_a_completion_marker() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"Let me start by understanding the p\"}}]}\n",
        ]);
        assert!(events.iter().any(|event| matches!(
            event,
            Err(ProviderError::StreamTruncated { detail })
                if detail.contains("completion marker")
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

    fn collected_text(events: &[Result<ModelEvent, ProviderError>]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                Ok(ModelEvent::TextDelta(t)) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    fn collected_reasoning(events: &[Result<ModelEvent, ProviderError>]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                Ok(ModelEvent::ReasoningDelta(t)) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn multibyte_character_split_across_network_chunks_survives() {
        // "日" is e6 97 a5; split it mid-character across two pushes, with an
        // emoji (f0 9f 8e 89) split 1+3 in a later line.
        let line1 = "data: {\"choices\":[{\"delta\":{\"content\":\"日本\"}}]}\n".as_bytes();
        let line2 = "data: {\"choices\":[{\"delta\":{\"content\":\"🎉\"}}]}\n".as_bytes();
        let mut decoder = SseDecoder::default();
        let mut out = EventQueue::new();
        // The content payload starts at byte 39 of each line; splitting at 40
        // lands inside the first multi-byte character.
        let split1 = 40;
        assert!(
            std::str::from_utf8(&line1[..split1]).is_err(),
            "split is mid-character"
        );
        decoder.push(&line1[..split1], &mut out);
        decoder.push(&line1[split1..], &mut out);
        let split2 = 41;
        assert!(
            std::str::from_utf8(&line2[..split2]).is_err(),
            "split is mid-character"
        );
        decoder.push(&line2[..split2], &mut out);
        decoder.push(&line2[split2..], &mut out);
        decoder.push(b"data: [DONE]\n", &mut out);
        decoder.finish(&mut out);
        let events: Vec<_> = out.into_iter().collect();
        let text = collected_text(&events);
        assert_eq!(text, "\u{65e5}\u{672c}\u{1f389}");
        assert!(!text.contains('\u{fffd}'), "no replacement characters");
    }

    #[test]
    fn reasoning_block_spanning_many_deltas_stays_hidden() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"<think>Let me look at\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" the error handling\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"</think>The fix:\"}}]}\n",
            "data: [DONE]\n",
        ]);
        assert_eq!(collected_text(&events), "The fix:");
        assert_eq!(
            collected_reasoning(&events),
            "Let me look at the error handling"
        );
    }

    #[test]
    fn think_tag_split_across_deltas_is_recognized() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"a<thi\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"nk>hidden</thi\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"nk>b\"}}]}\n",
            "data: [DONE]\n",
        ]);
        assert_eq!(collected_text(&events), "ab");
        assert_eq!(collected_reasoning(&events), "hidden");
    }

    #[test]
    fn stream_ending_inside_an_open_think_block_flushes_reasoning() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"<think>cut off\"},\"finish_reason\":\"stop\"}]}\n",
            "data: [DONE]\n",
        ]);
        assert_eq!(collected_text(&events), "");
        assert_eq!(collected_reasoning(&events), "cut off");
    }

    #[test]
    fn parallel_tool_calls_without_index_accumulate_by_id() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_a\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a\\\"}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_b\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"b\\\"}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: [DONE]\n",
        ]);
        let calls: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Ok(ModelEvent::ToolCall { id, input_json, .. }) => {
                    Some((id.clone(), input_json["path"].to_string()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2, "distinct calls must not merge: {calls:?}");
        assert!(calls.iter().any(|(id, _)| id == "call_a"));
        assert!(calls.iter().any(|(id, _)| id == "call_b"));
    }

    #[test]
    fn indexless_continuation_fragments_attach_to_the_last_call() {
        let events = collect_sse(&[
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_a\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"arguments\":\"\\\"a.rs\\\"}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: [DONE]\n",
        ]);
        let call = events.iter().find_map(|e| match e {
            Ok(ModelEvent::ToolCall { input_json, .. }) => Some(input_json.clone()),
            _ => None,
        });
        assert_eq!(call.expect("one assembled call")["path"], "a.rs");
    }

    #[test]
    fn official_api_does_not_send_nonstandard_reasoning_fields() {
        use localpilot_core::{ContentBlock, Message, Role};
        let provider = OpenAiProvider::new(
            "openai",
            "OpenAI",
            SourceType::OfficialApi,
            "https://api.openai.com/v1",
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
        assert!(!serialized.contains("reasoning_content"));
        assert!(!serialized.contains("reasoning_signature"));
    }

    #[test]
    fn reasoning_round_trip_option_overrides_the_source_type_default() {
        use localpilot_core::{ContentBlock, Message, Role};
        let mut options = IndexMap::new();
        options.insert("reasoning_round_trip".to_string(), json!(true));
        let provider = OpenAiProvider::new(
            "openai",
            "OpenAI",
            SourceType::OfficialApi,
            "https://api.openai.com/v1",
            None,
        )
        .with_default_options(options);
        let message = Message::new(
            Role::Assistant,
            vec![ContentBlock::Reasoning {
                text: "deduce".to_string(),
                signature: None,
                provider_metadata: None,
            }],
        );
        let body = provider.build_body(&ModelRequest::new("m", vec![message]));
        assert!(body.to_string().contains("reasoning_content"));
        // The switch itself never reaches the wire.
        assert!(body.get("reasoning_round_trip").is_none());
    }

    #[test]
    fn assistant_tool_call_round_trips_provider_metadata() {
        use localpilot_core::{ContentBlock, Message, Role, ToolCall, ToolUseId};
        let provider = OpenAiProvider::new(
            "gemini",
            "Gemini",
            SourceType::CustomUserEndpoint,
            "https://aiplatform.googleapis.com/v1/projects/p/locations/global/endpoints/openapi",
            None,
        );
        let call = ToolCall::new(ToolUseId::from("call_1"), "list_files", json!({}))
            .with_provider_metadata(json!({
                "extra_content": {
                    "google": {
                        "thought_signature": "sig-123"
                    }
                }
            }));
        let body = provider.build_body(&ModelRequest::new(
            "m",
            vec![Message::new(
                Role::Assistant,
                vec![ContentBlock::ToolUse(call)],
            )],
        ));
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
            "sig-123"
        );
    }

    #[test]
    fn late_system_message_keeps_its_position_on_the_wire() {
        use localpilot_core::{Message, Role};
        let provider = OpenAiProvider::new(
            "local",
            "Local",
            SourceType::LocalServer,
            "http://localhost:1234/v1",
            None,
        );
        let messages = vec![
            Message::text(Role::System, "be terse"),
            Message::text(Role::User, "hi"),
            Message::text(Role::Assistant, "hello"),
            Message::text(Role::System, "project context: uses tokio"),
            Message::text(Role::User, "continue"),
        ];
        let body = provider.build_body(&ModelRequest::new("m", messages));
        let wire = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = wire.iter().map(|m| m["role"].as_str().unwrap()).collect();
        // The late system message keeps its position (index 3, not reordered to
        // the front) but is delivered as user-role content, since many model
        // chat templates reject a non-leading system message.
        assert_eq!(
            roles,
            vec!["system", "user", "assistant", "user", "user"],
            "a late system message keeps its position but is delivered as user content"
        );
        assert_eq!(wire[3]["content"], "project context: uses tokio");
    }

    #[test]
    fn explicit_reasoning_effort_reaches_the_wire_and_overrides_defaults() {
        let mut options = IndexMap::new();
        options.insert("reasoning_effort".to_string(), json!("low"));
        let provider = OpenAiProvider::new(
            "local",
            "Local",
            SourceType::LocalServer,
            "http://localhost:1234/v1",
            None,
        )
        .with_default_options(options);
        let request = ModelRequest::new("m", Vec::new())
            .with_reasoning_effort(Some(crate::request::ReasoningEffort::High));
        let body = provider.build_body(&request);
        assert_eq!(body["reasoning_effort"], "high");
        // Without an explicit request value the option default stands.
        let body = provider.build_body(&ModelRequest::new("m", Vec::new()));
        assert_eq!(body["reasoning_effort"], "low");
    }

    #[test]
    fn quota_headers_parse_duration_string_resets() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-reset-requests", "1s".parse().unwrap());
        headers.insert("x-ratelimit-reset-tokens", "6m0s".parse().unwrap());
        let quota = quota_from_headers(&headers);
        // The longer window is the conservative wait.
        assert_eq!(quota.retry_after, Some(Duration::from_secs(360)));
        assert_eq!(quota.limit_kind.as_deref(), Some("tokens"));
    }

    #[test]
    fn quota_headers_prefer_retry_after_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "30".parse().unwrap());
        headers.insert("x-ratelimit-reset-requests", "1s".parse().unwrap());
        let quota = quota_from_headers(&headers);
        assert_eq!(quota.retry_after, Some(Duration::from_secs(30)));
    }

    #[test]
    fn unparseable_quota_headers_degrade_to_absent_metadata() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "soon".parse().unwrap());
        headers.insert("x-ratelimit-reset-requests", "later".parse().unwrap());
        let quota = quota_from_headers(&headers);
        assert_eq!(quota.retry_after, None);
        assert_eq!(quota.limit_kind, None);
    }
}
