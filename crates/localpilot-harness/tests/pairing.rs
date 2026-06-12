//! Tool-pairing invariant: after any `run_turn` return — success, rejected
//! batch, tool-budget exhaustion, cancellation, stream error — every
//! `tool_use` id in the persisted history has exactly one matching
//! `tool_result` id, in call order. Providers reject a history that violates
//! this, so the loop must hold it on every exit path.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use localpilot_core::{ContentBlock, Message};
use localpilot_harness::{SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::{FakeProvider, ModelEvent, ProviderError};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct Harness {
    _dir: tempfile::TempDir,
    runtime: SessionRuntime,
    events: broadcast::Sender<localpilot_harness::RuntimeEvent>,
    cancel: CancellationToken,
    store: Store,
}

fn build(provider: FakeProvider, config: SessionConfig) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "contents").unwrap();
    let store = Store::open(dir.path());
    let runtime = SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        config,
        Vec::new(),
    );
    let (events, _rx) = broadcast::channel(256);
    Harness {
        _dir: dir,
        runtime,
        events,
        cancel: CancellationToken::new(),
        store,
    }
}

/// Assert the pairing invariant over a persisted transcript.
fn assert_pairing(messages: &[Message]) {
    let call_ids: Vec<String> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolUse(call) => Some(call.id.to_string()),
            _ => None,
        })
        .collect();
    let result_ids: Vec<String> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult(result) => Some(result.id.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        call_ids, result_ids,
        "every tool_use must be answered by a tool_result, in order"
    );
}

async fn run_and_check(provider: FakeProvider, config: SessionConfig) -> StopReason {
    let mut h = build(provider, config);
    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;
    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    assert_pairing(&transcript);
    reason
}

#[tokio::test]
async fn success_path_pairs_every_call() {
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "a.txt" }))
        .text("done");
    let reason = run_and_check(provider, SessionConfig::default()).await;
    assert_eq!(reason, StopReason::Done);
}

#[tokio::test]
async fn rejected_batch_with_blank_id_leaves_no_orphan() {
    let provider = FakeProvider::new()
        .tool_call("", "read_file", json!({ "path": "a.txt" }))
        .text("recovered");
    let reason = run_and_check(provider, SessionConfig::default()).await;
    assert_eq!(reason, StopReason::Done);
}

#[tokio::test]
async fn rejected_batch_with_blank_name_synthesizes_a_rejection_result() {
    let provider = FakeProvider::new()
        .tool_call("c1", "", json!({ "path": "a.txt" }))
        .text("recovered");
    let mut h = build(provider, SessionConfig::default());
    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    assert_pairing(&transcript);
    let rejection = transcript
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::ToolResult(r) if r.is_error => Some(r.output.clone()),
            _ => None,
        })
        .expect("a synthesized rejection result");
    assert!(rejection.contains("tool call rejected"));
}

#[tokio::test]
async fn rejected_batch_with_non_object_input_leaves_no_orphan() {
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!("not an object"))
        .text("recovered");
    let reason = run_and_check(provider, SessionConfig::default()).await;
    assert_eq!(reason, StopReason::Done);
}

#[tokio::test]
async fn cancellation_leaves_no_orphan() {
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "a.txt" }))
        .text("never reached");
    let mut h = build(provider, SessionConfig::default());
    h.cancel.cancel();
    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Cancelled);
    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    assert_pairing(&transcript);
}

#[tokio::test]
async fn stream_error_leaves_no_orphan() {
    let provider = FakeProvider::new()
        .script(vec![
            Ok(ModelEvent::ToolCall {
                id: "c1".to_string(),
                name: "read_file".to_string(),
                input_json: json!({ "path": "a.txt" }),
            }),
            Err(ProviderError::StreamDecode("scripted failure".to_string())),
        ])
        .text("recovered");
    let reason = run_and_check(provider, SessionConfig::default()).await;
    assert_eq!(reason, StopReason::Done);
}

/// One scripted model turn for the property: each variant exercises a
/// different exit path of the loop.
#[derive(Debug, Clone)]
enum TurnShape {
    Text,
    ValidCalls(u8),
    BlankIdCall,
    BlankNameCall,
    NonObjectInputCall,
    StreamError,
}

fn provider_for(turns: &[TurnShape]) -> FakeProvider {
    let mut provider = FakeProvider::new();
    let mut next_id = 0u32;
    for turn in turns {
        provider = match turn {
            TurnShape::Text => provider.text("a plain answer"),
            TurnShape::ValidCalls(count) => {
                let mut script: Vec<Result<ModelEvent, ProviderError>> = Vec::new();
                for _ in 0..*count {
                    next_id += 1;
                    script.push(Ok(ModelEvent::ToolCall {
                        id: format!("call_{next_id}"),
                        name: "git_status".to_string(),
                        input_json: json!({}),
                    }));
                }
                script.push(Ok(ModelEvent::Done));
                provider.script(script)
            }
            TurnShape::BlankIdCall => {
                provider.tool_call("", "read_file", json!({ "path": "a.txt" }))
            }
            TurnShape::BlankNameCall => {
                next_id += 1;
                provider.tool_call(&format!("call_{next_id}"), "", json!({}))
            }
            TurnShape::NonObjectInputCall => {
                next_id += 1;
                provider.tool_call(&format!("call_{next_id}"), "read_file", json!(7))
            }
            TurnShape::StreamError => provider.script(vec![Err(ProviderError::StreamDecode(
                "scripted failure".to_string(),
            ))]),
        };
    }
    // A final clean answer so well-behaved scripts can reach `Done`.
    provider.text("done")
}

mod property {
    use super::*;
    use proptest::prelude::*;

    fn turn_shape() -> impl Strategy<Value = TurnShape> {
        prop_oneof![
            Just(TurnShape::Text),
            (1u8..4).prop_map(TurnShape::ValidCalls),
            Just(TurnShape::BlankIdCall),
            Just(TurnShape::BlankNameCall),
            Just(TurnShape::NonObjectInputCall),
            Just(TurnShape::StreamError),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        // The pairing invariant holds for arbitrary interleavings of clean
        // turns, valid/invalid tool batches, and stream errors.
        #[test]
        fn pairing_invariant_holds_for_arbitrary_scripts(
            turns in proptest::collection::vec(turn_shape(), 0..5),
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let provider = provider_for(&turns);
                let config = SessionConfig::default();
                let mut h = build(provider, config);
                let _ = h.runtime.run_turn("go", &h.events, &h.cancel).await;
                let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
                assert_pairing(&transcript);
            });
        }
    }
}
