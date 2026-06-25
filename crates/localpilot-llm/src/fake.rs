//! A scripted, in-memory provider for deterministic offline tests.
//!
//! This is first-party test support, reused across the workspace: a session or
//! harness test drives it instead of touching the network. Each call to
//! [`ModelProvider::stream`] returns the next scripted response.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use futures::StreamExt;
use localpilot_core::TokenUsage;

use crate::error::ProviderError;
use crate::event::{ModelEvent, ModelEventStream};
use crate::provider::{
    AuthRequirement, Capabilities, InputBlockKind, ModelProvider, ProviderDeclaration,
    ReasoningShape, SourceType, ToolCallShape,
};
use crate::request::ModelRequest;

type Script = Vec<Result<ModelEvent, ProviderError>>;

/// A provider that replays scripted event sequences.
pub struct FakeProvider {
    declaration: ProviderDeclaration,
    scripts: Mutex<VecDeque<Script>>,
    open_failures: Mutex<u32>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl FakeProvider {
    /// A fake with a permissive default declaration and no scripts.
    #[must_use]
    pub fn new() -> Self {
        Self {
            declaration: default_declaration(),
            scripts: Mutex::new(VecDeque::new()),
            open_failures: Mutex::new(0),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Make the next `count` calls to [`ModelProvider::stream`] fail with a
    /// transient network error before any scripted response is served, to
    /// exercise connection-retry behavior.
    #[must_use]
    pub fn fail_open(self, count: u32) -> Self {
        if let Ok(mut failures) = self.open_failures.lock() {
            *failures = count;
        }
        self
    }

    /// Override the declaration (for capability-branching tests).
    #[must_use]
    pub fn with_declaration(mut self, declaration: ProviderDeclaration) -> Self {
        self.declaration = declaration;
        self
    }

    /// Queue a raw scripted response.
    #[must_use]
    pub fn script(self, events: Script) -> Self {
        if let Ok(mut scripts) = self.scripts.lock() {
            scripts.push_back(events);
        }
        self
    }

    /// Queue a plain text response followed by `Done`.
    #[must_use]
    pub fn text(self, text: &str) -> Self {
        self.script(vec![
            Ok(ModelEvent::TextDelta(text.to_string())),
            Ok(ModelEvent::Usage(TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
            })),
            Ok(ModelEvent::Done),
        ])
    }

    /// Queue a single tool call followed by `Done`.
    #[must_use]
    pub fn tool_call(self, id: &str, name: &str, input_json: serde_json::Value) -> Self {
        self.script(vec![
            Ok(ModelEvent::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                input_json,
                provider_metadata: None,
            }),
            Ok(ModelEvent::Done),
        ])
    }

    /// Queue a malformed-stream failure.
    #[must_use]
    pub fn malformed(self) -> Self {
        self.script(vec![Err(ProviderError::StreamDecode(
            "scripted malformed stream".to_string(),
        ))])
    }

    /// Requests received so far, for offline tests.
    #[must_use]
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default()
    }
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ModelProvider for FakeProvider {
    fn declaration(&self) -> &ProviderDeclaration {
        &self.declaration
    }

    async fn stream(&self, request: ModelRequest) -> Result<ModelEventStream, ProviderError> {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request);
        }
        if let Ok(mut failures) = self.open_failures.lock() {
            if *failures > 0 {
                *failures -= 1;
                return Err(ProviderError::Network(
                    "scripted connection failure".to_string(),
                ));
            }
        }
        let script = self
            .scripts
            .lock()
            .ok()
            .and_then(|mut scripts| scripts.pop_front())
            .unwrap_or_else(|| vec![Ok(ModelEvent::Done)]);
        Ok(futures::stream::iter(script).boxed())
    }
}

fn default_declaration() -> ProviderDeclaration {
    ProviderDeclaration {
        id: "fake".to_string(),
        display_name: "Fake Provider".to_string(),
        source_type: SourceType::LocalServer,
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
            usage_during_stream: false,
            per_request_tool_disable: true,
            quota_reset_metadata: true,
            needs_no_tool_prompt_path: false,
            constrained_decoding: false,
        },
        max_context_tokens: Some(8192),
        auth: AuthRequirement::None,
        rate_limit_behavior: None,
    }
}
