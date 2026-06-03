//! Agent-mode session runtime integration tests, driven by the fake provider.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use unshackled_harness::{RuntimeEvent, SessionConfig, SessionRuntime, StopReason};
use unshackled_llm::{FakeProvider, ModelEvent};
use unshackled_recovery::{RecoveryBudget, RecoveryEngine};
use unshackled_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use unshackled_store::Store;
use unshackled_tools::ToolRegistry;

struct Harness {
    _dir: tempfile::TempDir,
    runtime: SessionRuntime,
    events: broadcast::Sender<RuntimeEvent>,
    cancel: CancellationToken,
    store: Store,
}

fn build(provider: FakeProvider, files: &[(&str, &str)], config: SessionConfig) -> Harness {
    build_with(provider, files, config, Profile::Default)
}

fn build_with(
    provider: FakeProvider,
    files: &[(&str, &str)],
    config: SessionConfig,
    profile: Profile,
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    for (rel, contents) in files {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }
    let store = Store::open(dir.path());
    let runtime = SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(profile, Vec::new()),
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

fn build_from_arc(
    provider: Arc<FakeProvider>,
    files: &[(&str, &str)],
    config: SessionConfig,
    profile: Profile,
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    for (rel, contents) in files {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }
    let store = Store::open(dir.path());
    let runtime = SessionRuntime::new(
        provider,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(profile, Vec::new()),
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

fn drain(rx: &mut broadcast::Receiver<RuntimeEvent>) -> Vec<RuntimeEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

#[tokio::test]
async fn loop_reads_a_file_then_produces_a_final_answer() {
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "src/lib.rs" }))
        .text("the file says hello");
    let mut h = build(
        provider,
        &[("src/lib.rs", "hello world")],
        SessionConfig::default(),
    );

    let reason = h
        .runtime
        .run_turn("read the file", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::Done);

    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    // user, assistant(tool_use), tool(result), assistant(final).
    assert_eq!(transcript.len(), 4);
}

#[tokio::test]
async fn first_request_carries_the_agent_system_prompt_once() {
    let provider = Arc::new(FakeProvider::new().text("ok"));
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig::default(),
        Profile::Default,
    );

    let reason = h.runtime.run_turn("hello", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let messages = &requests[0].messages;
    assert_eq!(
        messages.first().map(|message| message.role),
        Some(unshackled_core::Role::System)
    );
    let system_text = messages
        .first()
        .and_then(|message| message.content.first())
        .and_then(|block| match block {
            unshackled_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap();
    assert!(system_text.contains("Available tools:"));
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == unshackled_core::Role::System)
            .count(),
        1
    );
}

#[tokio::test]
async fn aborts_a_degenerate_output_flood_early() {
    // A punctuation flood arriving as many small deltas (real streaming shape).
    let mut script: Vec<_> = (0..300)
        .map(|_| Ok(ModelEvent::TextDelta("/".to_string())))
        .collect();
    script.push(Ok(ModelEvent::Done));
    let provider = FakeProvider::new().script(script);
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;
    // A degenerate turn never completes as a clean answer.
    assert_ne!(reason, StopReason::Done);

    let events = drain(&mut rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RuntimeEvent::Warning(m) if m.contains("degenerate"))),
        "the live guard should warn about degenerate output"
    );
    let streamed: usize = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::Text(t) => Some(t.len()),
            _ => None,
        })
        .sum();
    assert!(
        streamed < 300,
        "the flood should be cut short, got {streamed} chars"
    );
}

#[tokio::test]
async fn update_plan_tool_emits_a_plan_event() {
    let provider = FakeProvider::new()
        .tool_call(
            "p1",
            "update_plan",
            json!({ "steps": [
                { "title": "investigate", "status": "done" },
                { "title": "fix", "status": "in_progress" }
            ] }),
        )
        .text("on it");
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let plans: Vec<_> = drain(&mut rx)
        .into_iter()
        .filter_map(|event| match event {
            RuntimeEvent::Plan(steps) => Some(steps),
            _ => None,
        })
        .collect();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].len(), 2);
    assert_eq!(plans[0][0].status, "done");
    assert_eq!(plans[0][1].title, "fix");
}

#[tokio::test]
async fn context_usage_event_is_emitted_before_request() {
    let provider = FakeProvider::new().text("ok");
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    assert!(drain(&mut rx).iter().any(|event| matches!(
        event,
        RuntimeEvent::ContextUsage { used, limit } if *used > 0 && *limit == SessionConfig::default().context_token_limit
    )));
}

#[tokio::test]
async fn malformed_tool_call_is_reported_and_reprompted() {
    let provider = FakeProvider::new()
        .tool_call("", "read_file", json!({ "path": "a" }))
        .text("fixed");
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    assert!(drain(&mut rx).iter().any(|event| matches!(
        event,
        RuntimeEvent::Warning(message) if message.contains("missing tool-call id")
    )));
}

#[tokio::test(start_paused = true)]
async fn retries_a_transient_connection_failure_then_succeeds() {
    // Two connection failures, then a normal response: within the retry budget.
    let provider = FakeProvider::new().fail_open(2).text("recovered");
    let mut h = build(provider, &[], SessionConfig::default());

    let reason = h.runtime.run_turn("hi", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);
}

#[tokio::test(start_paused = true)]
async fn gives_up_after_exhausting_connection_retries() {
    // More failures than the retry budget: the turn ends as a provider error.
    let provider = FakeProvider::new().fail_open(10).text("never reached");
    let mut h = build(
        provider,
        &[],
        SessionConfig {
            max_stream_retries: 2,
            ..SessionConfig::default()
        },
    );

    let reason = h.runtime.run_turn("hi", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::ProviderError);
}

#[tokio::test]
async fn reasoning_is_emitted_as_metadata_distinct_from_text() {
    let provider = FakeProvider::new().script(vec![
        Ok(ModelEvent::ReasoningDelta("let me think".to_string())),
        Ok(ModelEvent::TextDelta("the answer".to_string())),
        Ok(ModelEvent::Done),
    ]);
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    h.runtime.run_turn("hi", &h.events, &h.cancel).await;

    let events = drain(&mut rx);
    assert!(events
        .iter()
        .any(|e| matches!(e, RuntimeEvent::Reasoning(r) if r == "let me think")));
    assert!(events
        .iter()
        .any(|e| matches!(e, RuntimeEvent::Text(t) if t == "the answer")));
}

#[tokio::test]
async fn a_denied_tool_call_becomes_an_error_result_not_a_crash() {
    // A destructive shell command, non-interactive, is denied; the loop keeps
    // going and the next turn produces a final answer.
    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "run_shell",
            json!({ "program": "rm", "args": ["-rf", "x"] }),
        )
        .text("could not delete");
    let config = SessionConfig {
        interactivity: Interactivity::NonInteractive,
        ..SessionConfig::default()
    };
    let mut h = build(provider, &[], config);

    let reason = h.runtime.run_turn("delete it", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    let tool_result = transcript
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            unshackled_core::ContentBlock::ToolResult(r) => Some(r.clone()),
            _ => None,
        });
    let result = tool_result.expect("a tool result was recorded");
    assert!(result.is_error);
}

#[tokio::test]
async fn transcript_is_persisted_with_redaction() {
    let secret = "sk-abcdefghijklmnopqrstuvwxyz0123";
    let provider = FakeProvider::new().text("ok");
    let mut h = build(provider, &[], SessionConfig::default());

    h.runtime
        .run_turn(&format!("my key is {secret}"), &h.events, &h.cancel)
        .await;

    let raw = std::fs::read_to_string(
        h._dir
            .path()
            .join(".unshackled")
            .join("sessions")
            .join(format!("{}.jsonl", h.runtime.session_id())),
    )
    .unwrap();
    assert!(
        !raw.contains(secret),
        "secret reached the transcript: {raw}"
    );
    assert!(raw.contains("[REDACTED]"));
}

#[tokio::test]
async fn cancellation_leaves_a_consistent_transcript() {
    let provider = FakeProvider::new().text("never reached");
    let mut h = build(provider, &[], SessionConfig::default());
    h.cancel.cancel();

    let reason = h.runtime.run_turn("hello", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Cancelled);

    // Only the complete user message is persisted; the transcript still parses.
    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    assert_eq!(transcript.len(), 1);
    assert_eq!(transcript[0].role, unshackled_core::Role::User);
}

#[tokio::test]
async fn loop_stops_at_the_turn_cap() {
    // A provider that always asks for a tool never produces a final answer.
    let provider = FakeProvider::new()
        .tool_call("c1", "git_status", json!({}))
        .tool_call("c2", "git_status", json!({}))
        .tool_call("c3", "git_status", json!({}));
    let config = SessionConfig {
        max_turns: 2,
        ..SessionConfig::default()
    };
    let mut h = build_with(provider, &[], config, Profile::Bypass);

    let reason = h
        .runtime
        .run_turn("loop forever", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::MaxTurns);
}

#[tokio::test]
async fn loop_stops_at_the_tool_call_cap() {
    let provider = FakeProvider::new()
        .tool_call("c1", "git_status", json!({}))
        .tool_call("c2", "git_status", json!({}));
    let config = SessionConfig {
        max_turns: 10,
        max_tool_calls: 1,
        ..SessionConfig::default()
    };
    let mut h = build_with(provider, &[], config, Profile::Bypass);

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::MaxToolCalls);
}
