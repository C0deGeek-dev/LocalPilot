//! Mid-session provider switch: the conversation continues against the new
//! provider with its full history, driven by the fake provider offline.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::{FakeProvider, ModelProvider, ProviderDeclaration, ProviderRegistry};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// A fake provider reporting `id` and replaying `text` for each turn.
fn provider(id: &str, text: &str) -> Arc<FakeProvider> {
    let mut declaration: ProviderDeclaration = FakeProvider::new().declaration().clone();
    declaration.id = id.to_string();
    declaration.display_name = id.to_string();
    Arc::new(
        FakeProvider::new()
            .with_declaration(declaration)
            .text(text)
            .text(text),
    )
}

#[tokio::test]
async fn a_session_continues_against_the_new_provider_with_full_history() {
    let dir = tempfile::tempdir().unwrap();
    let first = provider("first", "answer from first");
    let second = provider("second", "answer from second");

    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    providers.insert("first".to_string(), first.clone());
    providers.insert("second".to_string(), second.clone());
    let mut models = HashMap::new();
    models.insert("first".to_string(), "model-1".to_string());
    models.insert("second".to_string(), "model-2".to_string());
    let registry = Arc::new(ProviderRegistry::from_providers(providers, models, "first"));

    let mut runtime = SessionRuntime::new(
        registry.get("first").unwrap().clone(),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            model: "model-1".to_string(),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    runtime.set_registry(Arc::clone(&registry));

    let (events, _rx) = broadcast::channel::<RuntimeEvent>(256);
    let cancel = CancellationToken::new();

    // Turn one runs on the first provider.
    assert_eq!(
        runtime.run_turn("hello on first", &events, &cancel).await,
        StopReason::Done
    );
    assert_eq!(first.requests().len(), 1);
    assert_eq!(second.requests().len(), 0);

    // Switch to the second provider at the (now idle) turn boundary.
    let outcome = runtime.set_active_provider("second").unwrap();
    assert_eq!(outcome.model, "model-2");

    // Turn two runs on the second provider and the request the model received
    // carries the first turn's history — the transcript survived the switch.
    assert_eq!(
        runtime
            .run_turn("continue on second", &events, &cancel)
            .await,
        StopReason::Done
    );
    assert_eq!(first.requests().len(), 1, "first provider gets no new turn");
    assert_eq!(
        second.requests().len(),
        1,
        "second provider runs the new turn"
    );

    let request = second.requests().pop().unwrap();
    assert_eq!(request.model, "model-2", "the new turn uses the new model");
    let text: String = request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            localpilot_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("hello on first"),
        "the new provider sees the prior conversation: {text}"
    );
    assert!(text.contains("answer from first"));
    assert!(text.contains("continue on second"));
}
