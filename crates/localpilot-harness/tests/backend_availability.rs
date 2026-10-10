//! Backend availability is fresh visibility, never execution authority.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use localpilot_harness::{SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::{FakeProvider, ModelEvent};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Effect, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::{
    Broker, BrokerConfig, Tool, ToolContext, ToolError, ToolLoad, ToolOutput, ToolRegistry,
    ToolSearch, UnavailableBackend,
};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct ProbedTool(Arc<AtomicUsize>);
#[async_trait]
impl Tool for ProbedTool {
    fn name(&self) -> &str {
        "probed"
    }
    fn description(&self) -> &str {
        "fixture with an authoritatively absent backend"
    }
    fn schema(&self) -> Value {
        json!({"type":"object"})
    }
    fn unavailable_backend(&self, _: &Workspace) -> Option<UnavailableBackend> {
        Some(UnavailableBackend {
            key: "fixture",
            reason: "fixture backend absent".into(),
        })
    }
    fn effects(&self, _: &Value, _: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(Vec::new())
    }
    async fn invoke(&self, _: Value, _: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok("must not execute"))
    }
}

// Test-only host fixture: simulate index creation by an external host between
// model observations without adding an ingest tool to the production surface.
struct CreateIndex;
#[async_trait]
impl Tool for CreateIndex {
    fn name(&self) -> &str {
        "create_index"
    }
    fn description(&self) -> &str {
        "simulate host index creation"
    }
    fn schema(&self) -> Value {
        json!({"type":"object"})
    }
    fn effects(&self, _: &Value, _: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(Vec::new())
    }
    async fn invoke(&self, _: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        localpilot_localmind::ingest_run(
            ctx.workspace.root(),
            &Default::default(),
            localpilot_localmind::RunMode::Full,
        )
        .map_err(|e| ToolError::Failed(e.to_string()))?;
        Ok(ToolOutput::ok("host built index"))
    }
}

struct SwitchingBackend {
    enabled: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for SwitchingBackend {
    fn name(&self) -> &str {
        "switching_backend"
    }
    fn description(&self) -> &str {
        "query the fixture backend"
    }
    fn schema(&self) -> Value {
        json!({"type":"object"})
    }
    fn unavailable_backend(&self, _: &Workspace) -> Option<UnavailableBackend> {
        (!self.enabled.load(Ordering::SeqCst)).then(|| UnavailableBackend {
            key: "switching",
            reason: "fixture backend absent".into(),
        })
    }
    fn effects(&self, _: &Value, _: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(Vec::new())
    }
    async fn invoke(&self, _: Value, _: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok("valid hit"))
    }
}
struct Switch(Arc<AtomicBool>);
#[async_trait]
impl Tool for Switch {
    fn name(&self) -> &str {
        "switch"
    }
    fn description(&self) -> &str {
        "simulate host backend change"
    }
    fn schema(&self) -> Value {
        json!({"type":"object", "properties":{"enabled":{"type":"boolean"}}, "required":["enabled"]})
    }
    fn effects(&self, _: &Value, _: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(Vec::new())
    }
    async fn invoke(&self, input: Value, _: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let enabled = input["enabled"].as_bool().unwrap();
        self.0.store(enabled, Ordering::SeqCst);
        Ok(ToolOutput::ok(format!("host availability: {enabled}")))
    }
}

fn runtime_at(
    root: &std::path::Path,
    provider: Arc<FakeProvider>,
    registry: ToolRegistry,
) -> SessionRuntime {
    SessionRuntime::new(
        provider,
        registry,
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(root),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    )
}

#[tokio::test]
async fn a_stale_call_is_not_executed_and_other_work_can_finish() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "useful evidence\n").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("absent", "probed", json!({}))
            .tool_call("read", "read_file", json!({"path":"f.txt"}))
            .text("finished"),
    );
    let mut registry = ToolRegistry::with_builtins();
    registry.register(Box::new(ProbedTool(calls.clone())));
    let mut runtime = runtime_at(dir.path(), provider.clone(), registry);
    let (events, _) = broadcast::channel(64);
    assert_eq!(
        runtime
            .run_turn("go", &events, &CancellationToken::new())
            .await,
        StopReason::Done
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(provider
        .requests()
        .iter()
        .all(|r| r.tools.iter().all(|t| t.name != "probed")));
    let transcript = Store::open(dir.path())
        .read_transcript(runtime.session_id())
        .unwrap();
    let text = serde_json::to_string(&transcript).unwrap();
    assert!(text.contains("was not executed"));
    assert!(text.contains("useful evidence"));
}

#[tokio::test]
async fn stale_attempts_are_bounded_without_invoking_and_reset_on_a_new_turn() {
    for (fresh_turn, interleaved) in [(false, false), (false, true), (true, false)] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "evidence\n").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut provider = FakeProvider::new().tool_call("absent1", "probed", json!({}));
        if interleaved {
            provider = provider.tool_call("read1", "read_file", json!({"path":"f.txt"}));
        }
        provider = provider.tool_call("absent2", "probed", json!({}));
        if fresh_turn {
            provider = provider.text("first turn finished");
        }
        if interleaved {
            provider = provider.tool_call("read2", "read_file", json!({"path":"f.txt"}));
        }
        let provider = Arc::new(
            provider
                .tool_call("absent3", "probed", json!({}))
                .text("finished"),
        );
        let mut registry = ToolRegistry::with_builtins();
        registry.register(Box::new(ProbedTool(calls.clone())));
        let mut runtime = runtime_at(dir.path(), provider.clone(), registry);
        let (events, _) = broadcast::channel(64);
        let cancel = CancellationToken::new();
        let first = runtime.run_turn("go", &events, &cancel).await;
        if fresh_turn {
            assert_eq!(first, StopReason::Done);
            assert_eq!(
                runtime.run_turn("go again", &events, &cancel).await,
                StopReason::Done
            );
        } else if interleaved {
            assert_eq!(first, StopReason::Done);
        } else {
            assert_eq!(first, StopReason::NoProgress);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn real_index_creation_restores_advertisement_and_same_batch_dispatch() {
    for use_broker in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("marker.rs"),
            "pub fn restored_index_marker() {}\n",
        )
        .unwrap();
        let call = |id: &str, name: &str, input_json: Value| {
            Ok(ModelEvent::ToolCall {
                id: id.into(),
                name: name.into(),
                input_json,
                provider_metadata: None,
            })
        };
        let provider = Arc::new(
            FakeProvider::new()
                .tool_call("stale", "knowledge_search", json!({"query":"first"}))
                .script(vec![
                    call("create", "create_index", json!({})),
                    call(
                        "restored",
                        "knowledge_search",
                        json!({"query":"restored_index_marker"}),
                    ),
                    Ok(ModelEvent::Done),
                ])
                .text("finished"),
        );
        let mut registry = ToolRegistry::with_builtins();
        registry.register(Box::new(localpilot_localmind::KnowledgeSearch));
        registry.register(Box::new(CreateIndex));
        let mut config = BrokerConfig::default();
        config.core.push("create_index".into());
        let broker = Broker::new(config);
        registry.register(Box::new(ToolSearch::new(broker.clone())));
        registry.register(Box::new(ToolLoad::new(broker.clone())));
        broker.set_catalog(registry.catalog());
        let mut runtime = runtime_at(dir.path(), provider.clone(), registry);
        if use_broker {
            runtime.set_broker(Some(broker.clone()));
        }
        let (events, _) = broadcast::channel(64);
        assert_eq!(
            runtime
                .run_turn("go", &events, &CancellationToken::new())
                .await,
            StopReason::Done
        );
        let requests = provider.requests();
        assert!(requests[..2]
            .iter()
            .all(|r| r.tools.iter().all(|t| t.name != "knowledge_search")));
        assert!(requests
            .last()
            .unwrap()
            .tools
            .iter()
            .any(|t| t.name == "knowledge_search"));
        let transcript = Store::open(dir.path())
            .read_transcript(runtime.session_id())
            .unwrap();
        assert!(transcript.iter().flat_map(|m| &m.content).any(|b| matches!(b,
            localpilot_core::ContentBlock::ToolResult(r) if r.id.to_string() == "restored" && r.output.contains("marker.rs") && !r.is_error())));
    }
}

#[tokio::test]
async fn broker_search_load_and_graduation_cannot_reveal_absent_knowledge() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("search", "tool_search", json!({"need":"knowledge_search"}))
            .tool_call("load", "tool_load", json!({"name":"knowledge_search"}))
            .text("finished"),
    );
    let broker = Broker::new(BrokerConfig {
        learning_enabled: true,
        ..Default::default()
    });
    let mut registry = ToolRegistry::with_builtins();
    registry.register(Box::new(localpilot_localmind::KnowledgeSearch));
    registry.register(Box::new(ToolSearch::new(broker.clone())));
    registry.register(Box::new(ToolLoad::new(broker.clone())));
    broker.set_catalog(registry.catalog());
    broker.seed_graduated(&["knowledge_search".into()]);
    let mut runtime = runtime_at(dir.path(), provider.clone(), registry);
    runtime.set_broker(Some(broker.clone()));
    let (events, _) = broadcast::channel(64);
    assert_eq!(
        runtime
            .run_turn("go", &events, &CancellationToken::new())
            .await,
        StopReason::Done
    );
    assert!(!broker.is_advertised("knowledge_search"));
    assert!(broker
        .resolve("knowledge_search")
        .iter()
        .all(|hit| hit.name != "knowledge_search"));
    assert!(broker
        .reveal_for_request("knowledge_search")
        .iter()
        .all(|r| r.revealed.as_deref() != Some("knowledge_search")));
    assert!(provider
        .requests()
        .iter()
        .all(|r| r.tools.iter().all(|t| t.name != "knowledge_search")));
    let transcript = Store::open(dir.path())
        .read_transcript(runtime.session_id())
        .unwrap();
    let text = serde_json::to_string(&transcript).unwrap();
    assert!(text.contains("no available tool named"));
    assert!(!text.contains("Revealed `knowledge_search`"));
    assert!(!text.contains("- knowledge_search"));
}

#[tokio::test]
async fn recovery_clears_attempts_and_later_absence_hides_the_tool_again() {
    let dir = tempfile::tempdir().unwrap();
    let enabled = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("stale1", "switching_backend", json!({}))
            .tool_call("stale2", "switching_backend", json!({}))
            .tool_call("enable", "switch", json!({"enabled":true}))
            .tool_call("query", "switching_backend", json!({}))
            .tool_call("disable", "switch", json!({"enabled":false}))
            .tool_call("stale3", "switching_backend", json!({}))
            .tool_call("stale4", "switching_backend", json!({}))
            .text("finished"),
    );
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(SwitchingBackend {
        enabled: enabled.clone(),
        calls: calls.clone(),
    }));
    registry.register(Box::new(Switch(enabled)));
    let mut runtime = runtime_at(dir.path(), provider.clone(), registry);
    let (events, _) = broadcast::channel(64);
    assert_eq!(
        runtime
            .run_turn("go", &events, &CancellationToken::new())
            .await,
        StopReason::Done
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let requests = provider.requests();
    for (i, request) in requests.iter().enumerate() {
        assert_eq!(
            request.tools.iter().any(|t| t.name == "switching_backend"),
            matches!(i, 3 | 4)
        );
    }
}
