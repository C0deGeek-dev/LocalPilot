//! Provider runtime for LocalPilot.
//!
//! Connects the agent to models behind one object-safe [`ModelProvider`] trait
//! that hides API differences behind a single internal stream contract while
//! exposing typed capabilities, quota metadata, and a stable error taxonomy.
//! Provider-specific code lives only in this crate; official public APIs and
//! local OpenAI-compatible servers only — never private or undocumented
//! endpoints.
#![forbid(unsafe_code)]

mod discovery;
mod fake;
mod registry;
mod vision;

pub use discovery::{
    discover_models, discover_models_with_auth_provider, probe_vision, DiscoveredModel,
};
pub use fake::FakeProvider;
pub use registry::{discovery_auth_provider_from_config, ProviderRegistry};
pub use vision::{resolve_vision, resolve_vision_with_source, VisionSource};

pub use localpilot_llm_anthropic::AnthropicProvider;
pub use localpilot_llm_openai::OpenAiProvider;

pub use localpilot_llm_core::auth::{AccessToken, AuthProvider, GoogleAdcAuthProvider};
pub use localpilot_llm_core::error::{ProviderError, QuotaInfo};
pub use localpilot_llm_core::event::{ModelEvent, ModelEventStream};
pub use localpilot_llm_core::provider::{
    AuthRequirement, Capabilities, InputBlockKind, ModelProvider, ProviderDeclaration,
    ReasoningShape, SourceType, ToolCallShape,
};
pub use localpilot_llm_core::request::{
    constraint_for, ModelRequest, ReasoningEffort, ReasoningEffortHandle, ToolSpec,
};

/// Fuzzing entry points (enabled by the `fuzzing` feature; not public API).
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzzing {
    pub use localpilot_llm_anthropic::fuzz_sse_decoder as anthropic_sse;
    pub use localpilot_llm_openai::fuzz_sse_decoder as openai_sse;
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn provider_trait_is_object_safe() {
        let _provider: Box<dyn ModelProvider> = Box::new(FakeProvider::new());
    }

    #[tokio::test]
    async fn fake_drives_text_then_tool_call_deterministically() {
        let provider = FakeProvider::new().text("hello").tool_call(
            "c1",
            "read_file",
            serde_json::json!({ "path": "a" }),
        );
        let request = ModelRequest::new("model", Vec::new());

        let first: Vec<_> = provider
            .stream(request.clone())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(matches!(first.first(), Some(Ok(ModelEvent::TextDelta(t))) if t == "hello"));
        assert!(matches!(first.last(), Some(Ok(ModelEvent::Done))));

        let second: Vec<_> = provider.stream(request).await.unwrap().collect().await;
        assert!(matches!(
            second.first(),
            Some(Ok(ModelEvent::ToolCall { name, .. })) if name == "read_file"
        ));
    }

    #[tokio::test]
    async fn fake_can_emit_a_malformed_stream() {
        let provider = FakeProvider::new().malformed();
        let events: Vec<_> = provider
            .stream(ModelRequest::new("m", Vec::new()))
            .await
            .unwrap()
            .collect()
            .await;
        assert!(matches!(
            events.first(),
            Some(Err(ProviderError::StreamDecode(_)))
        ));
    }
}
