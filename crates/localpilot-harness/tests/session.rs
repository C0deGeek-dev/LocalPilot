//! Agent-mode session runtime integration tests, driven by the fake provider.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use localpilot_core::{ContentBlock, Message};
use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::{FakeProvider, ModelEvent, ProviderError, QuotaInfo};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

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

fn message_text(messages: &[Message]) -> String {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
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
        Some(localpilot_core::Role::System)
    );
    let system_text = messages
        .first()
        .and_then(|message| message.content.first())
        .and_then(|block| match block {
            localpilot_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap();
    assert!(system_text.contains("Available tools:"));
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == localpilot_core::Role::System)
            .count(),
        1
    );
}

#[tokio::test]
async fn compaction_summary_does_not_produce_two_system_messages() {
    // A small context limit forces compaction once there are two prior
    // exchanges; compaction injects a summary that must fold into the single
    // leading system block rather than going out as a second system message.
    let provider = Arc::new(FakeProvider::new().text("one").text("two").text("three"));
    // The limit sits above (system prompt + one exchange + summary) but below
    // (system prompt + all three exchanges), so by the third turn the oldest
    // exchanges are dropped and a summary is injected.
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig {
            context_token_limit: 1_200,
            ..SessionConfig::default()
        },
        Profile::Default,
    );

    let filler = "context ".repeat(250); // ~2000 chars per prompt
    for label in ["first", "second", "third"] {
        let prompt = format!("{label} {filler}");
        let reason = h.runtime.run_turn(&prompt, &h.events, &h.cancel).await;
        assert_eq!(reason, StopReason::Done);
    }

    let requests = provider.requests();
    let last = requests.last().expect("at least one request");
    let system_messages: Vec<&str> = last
        .messages
        .iter()
        .filter(|message| message.role == localpilot_core::Role::System)
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            localpilot_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    // Exactly one leading system message, carrying both the agent prompt and the
    // compaction summary.
    assert_eq!(
        last.messages
            .iter()
            .filter(|message| message.role == localpilot_core::Role::System)
            .count(),
        1,
        "the request must not carry two consecutive system messages"
    );
    let combined = system_messages.join("\n");
    assert!(
        combined.contains("Available tools:"),
        "system block keeps the agent prompt"
    );
    assert!(
        combined.contains("Conversation summary for trimmed history"),
        "system block folds in the compaction summary"
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
async fn degenerate_output_retries_without_tool_schemas() {
    let mut flood: Vec<_> = (0..64)
        .map(|_| Ok(ModelEvent::TextDelta("/".to_string())))
        .collect();
    flood.push(Ok(ModelEvent::Done));
    let provider = Arc::new(FakeProvider::new().script(flood).text("recovered"));
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig::default(),
        Profile::Default,
    );

    let reason = h.runtime.run_turn("ping", &h.events, &h.cancel).await;

    assert_eq!(reason, StopReason::Done);
    let requests = provider.requests();
    assert!(!requests[0].tools.is_empty());
    assert!(requests[1].tools.is_empty());
}

#[tokio::test]
async fn output_limit_stop_discards_partial_reply() {
    let provider = FakeProvider::new().script(vec![
        Ok(ModelEvent::TextDelta("partial answer".to_string())),
        Ok(ModelEvent::OutputLimit {
            message: "provider stopped at max_tokens; output may be truncated".to_string(),
        }),
        Ok(ModelEvent::Done),
    ]);
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;

    assert_eq!(reason, StopReason::ProviderError);
    assert!(drain(&mut rx).iter().any(|event| matches!(
        event,
        RuntimeEvent::Warning(message) if message.contains("discarding partial response")
    )));
    let events = h.store.read_events(h.runtime.session_id()).unwrap();
    let transcript = serde_json::to_string(&localpilot_store::transcript_from_events(&events))
        .expect("transcript serializes");
    assert!(
        !transcript.contains("partial answer"),
        "output-limit text must not be persisted as a final assistant reply"
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
async fn clearing_conversation_resets_future_provider_context() {
    let provider = Arc::new(
        FakeProvider::new()
            .text("first answer")
            .text("second answer"),
    );
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig::default(),
        Profile::Default,
    );
    let session_id = h.runtime.session_id();

    let reason = h
        .runtime
        .run_turn("first prompt", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::Done);

    h.runtime.clear_conversation();
    let reason = h
        .runtime
        .run_turn("second prompt", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::Done);

    assert_eq!(h.runtime.session_id(), session_id);
    let requests = provider.requests();
    let second_request = requests.last().expect("second request is recorded");
    let text = message_text(&second_request.messages);
    assert!(text.contains("second prompt"));
    assert!(!text.contains("first prompt"));
    assert!(!text.contains("first answer"));
    assert_eq!(
        second_request
            .messages
            .iter()
            .filter(|message| message.role == localpilot_core::Role::System)
            .count(),
        1
    );
}

#[tokio::test]
async fn manual_compaction_reports_noop_when_context_is_under_limit() {
    let provider = FakeProvider::new();
    let mut h = build(provider, &[], SessionConfig::default());
    let before = h.runtime.context_usage();

    let result = h.runtime.compact_conversation().await;

    assert!(!result.compacted);
    assert_eq!(result.context_limit, before.1);
    assert_eq!(result.context_used, before.0);
}

#[tokio::test]
async fn manual_compaction_stores_a_summary_for_future_turns() {
    let provider = Arc::new(
        FakeProvider::new()
            .text("one")
            .text("two")
            .text("three")
            .text("after compaction"),
    );
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig {
            context_token_limit: 1_200,
            ..SessionConfig::default()
        },
        Profile::Default,
    );

    let filler = "context ".repeat(250);
    for label in ["first", "second", "third"] {
        let prompt = format!("{label} {filler}");
        let reason = h.runtime.run_turn(&prompt, &h.events, &h.cancel).await;
        assert_eq!(reason, StopReason::Done);
    }

    let result = h.runtime.compact_conversation().await;
    assert!(result.compacted);
    assert!(result.context_used <= result.context_limit);

    let reason = h
        .runtime
        .run_turn("after manual compact", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::Done);

    let requests = provider.requests();
    let text = message_text(&requests.last().expect("request after compaction").messages);
    assert!(text.contains("Conversation summary for trimmed history"));
    assert!(text.contains("after manual compact"));
}

#[tokio::test]
async fn clearing_after_manual_compaction_drops_the_compaction_summary() {
    let provider = Arc::new(
        FakeProvider::new()
            .text("one")
            .text("two")
            .text("three")
            .text("after clear"),
    );
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig {
            context_token_limit: 900,
            ..SessionConfig::default()
        },
        Profile::Default,
    );

    let filler = "context ".repeat(250);
    for label in ["first", "second", "third"] {
        let prompt = format!("{label} {filler}");
        let reason = h.runtime.run_turn(&prompt, &h.events, &h.cancel).await;
        assert_eq!(reason, StopReason::Done);
    }
    assert!(h.runtime.compact_conversation().await.compacted);

    h.runtime.clear_conversation();
    let reason = h
        .runtime
        .run_turn("after clear", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::Done);

    let requests = provider.requests();
    let text = message_text(&requests.last().expect("request after clear").messages);
    assert!(text.contains("after clear"));
    assert!(!text.contains("Conversation summary for trimmed history"));
    assert!(!text.contains("first context"));
}

#[tokio::test]
async fn manual_compaction_keeps_tool_call_and_result_pairs_together() {
    let a = "a ".repeat(400);
    let b = "b ".repeat(400);
    let c = "c ".repeat(400);
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("c1", "read_file", json!({ "path": "a.txt" }))
            .text("done one")
            .tool_call("c2", "read_file", json!({ "path": "b.txt" }))
            .text("done two")
            .tool_call("c3", "read_file", json!({ "path": "c.txt" }))
            .text("done three")
            .text("after compaction"),
    );
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[("a.txt", &a), ("b.txt", &b), ("c.txt", &c)],
        SessionConfig {
            context_token_limit: 600,
            ..SessionConfig::default()
        },
        Profile::Default,
    );

    for prompt in ["read a", "read b", "read c"] {
        let reason = h.runtime.run_turn(prompt, &h.events, &h.cancel).await;
        assert_eq!(reason, StopReason::Done);
    }

    let result = h.runtime.compact_conversation().await;
    assert!(result.compacted);

    let reason = h
        .runtime
        .run_turn("after tool compaction", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::Done);

    let requests = provider.requests();
    let messages = &requests.last().expect("request after compaction").messages;
    let call_ids: Vec<_> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolUse(call) => Some(call.id.clone()),
            _ => None,
        })
        .collect();
    let result_ids: Vec<_> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result.id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(call_ids, result_ids);
}

#[tokio::test]
async fn context_boundary_compacts_and_continues_the_turn() {
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("c1", "read_file", json!({ "path": "src/lib.rs" }))
            .text("continued after compaction"),
    );
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[("src/lib.rs", "hello")],
        SessionConfig {
            context_token_limit: 1_000,
            ..SessionConfig::default()
        },
        Profile::Default,
    );

    let prompt = format!("read the file\n{}", "large context ".repeat(2_000));
    let reason = h.runtime.run_turn(&prompt, &h.events, &h.cancel).await;

    assert_eq!(reason, StopReason::Done);
    assert_eq!(provider.requests().len(), 2);
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
async fn blank_reasoning_and_leading_answer_blank_lines_are_not_persisted() {
    let provider = FakeProvider::new().script(vec![
        Ok(ModelEvent::ReasoningDelta("\n\n".to_string())),
        Ok(ModelEvent::TextDelta("\n\nThe answer".to_string())),
        Ok(ModelEvent::Done),
    ]);
    let mut h = build(provider, &[], SessionConfig::default());

    let reason = h.runtime.run_turn("hi", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    let assistant = transcript
        .iter()
        .find(|message| message.role == localpilot_core::Role::Assistant)
        .expect("assistant message is persisted");
    assert_eq!(assistant.content.len(), 1);
    assert!(matches!(
        &assistant.content[0],
        localpilot_core::ContentBlock::Text { text } if text == "The answer"
    ));
}

#[tokio::test]
async fn incomplete_stream_is_retried_and_never_persisted_as_a_finished_reply() {
    let provider = FakeProvider::new()
        .script(vec![Ok(ModelEvent::TextDelta(
            "Let me start by understanding the p".to_string(),
        ))])
        .text("The complete answer.");
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    // The truncated text is never persisted as a finished reply; the repair
    // prompt that shaped the retry *is* persisted, marked synthetic, so the
    // stored transcript equals the history the model saw.
    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    assert_eq!(transcript.len(), 3);
    assert!(
        transcript[1].is_synthetic(),
        "the repair prompt is persisted and marked synthetic"
    );
    let assistant_text = transcript[2]
        .content
        .iter()
        .find_map(|block| match block {
            localpilot_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap();
    assert_eq!(assistant_text, "The complete answer.");
    assert!(!transcript
        .iter()
        .any(|message| message.content.iter().any(|block| matches!(
            block,
            localpilot_core::ContentBlock::Text { text } if text.contains("Let me start")
        ))));
    assert!(drain(&mut rx)
        .iter()
        .any(|event| matches!(event, RuntimeEvent::Recovery { .. })));
}

#[tokio::test]
async fn mid_stream_quota_error_stops_as_provider_error_and_emits_pause() {
    let quota = QuotaInfo {
        retry_after: Some(Duration::from_secs(45)),
        retryable: true,
        raw_provider_code: Some("rate_limit_exceeded".to_string()),
        ..QuotaInfo::default()
    };
    let provider = FakeProvider::new().script(vec![
        Ok(ModelEvent::TextDelta("partial answer".to_string())),
        Err(ProviderError::RateLimit { quota }),
    ]);
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;

    assert_eq!(reason, StopReason::ProviderError);
    assert!(drain(&mut rx)
        .iter()
        .any(|event| matches!(event, RuntimeEvent::QuotaPaused { reset } if reset.contains("45"))));
    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    assert_eq!(
        transcript.len(),
        1,
        "partial assistant text is not persisted"
    );
}

#[tokio::test]
async fn stream_decode_errors_still_use_bad_output_recovery() {
    let provider = Arc::new(FakeProvider::new().malformed().text("recovered"));
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig::default(),
        Profile::Default,
    );
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;

    assert_eq!(reason, StopReason::Done);
    assert_eq!(provider.requests().len(), 2);
    assert!(drain(&mut rx)
        .iter()
        .any(|event| matches!(event, RuntimeEvent::Recovery { .. })));
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
            localpilot_core::ContentBlock::ToolResult(r) => Some(r.clone()),
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
            .join(".localpilot")
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
    assert_eq!(transcript[0].role, localpilot_core::Role::User);
}

#[tokio::test]
async fn transcript_is_derivable_from_the_event_log() {
    // A representative session: a denied destructive tool call, a malformed
    // stream that triggers recovery (and a synthetic repair prompt), context
    // pressure that forces compaction, then a clean answer.
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "c1",
                "run_shell",
                json!({ "program": "rm", "args": ["-rf", "x"] }),
            )
            .malformed()
            .text("recovered and done"),
    );
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig {
            interactivity: Interactivity::NonInteractive,
            context_token_limit: 600,
            ..SessionConfig::default()
        },
        Profile::Default,
    );

    // A large prompt pushes the history over the small limit so compaction
    // runs while shaping the request.
    let prompt = format!("clean up {}", "context ".repeat(500));
    let reason = h.runtime.run_turn(&prompt, &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let session = h.runtime.session_id();
    let events = h.store.read_events(session).unwrap();

    // The transcript rebuilt from events equals the stored transcript,
    // including the synthetic repair prompt.
    let rebuilt = localpilot_store::transcript_from_events(&events);
    let stored = h.store.read_transcript(session).unwrap();
    assert_eq!(rebuilt, stored);
    assert!(stored.iter().any(localpilot_core::Message::is_synthetic));

    // The log carries the full audit trail for this session's shape.
    use localpilot_store::SessionEventKind as Kind;
    let has = |predicate: &dyn Fn(&Kind) -> bool| events.iter().any(|event| predicate(&event.kind));
    assert!(has(&|kind| matches!(kind, Kind::SessionOpened { .. })));
    assert!(has(&|kind| matches!(kind, Kind::TurnStarted { .. })));
    assert!(has(&|kind| matches!(kind, Kind::ToolStarted { .. })));
    assert!(has(&|kind| matches!(
        kind,
        Kind::ToolFinished { is_error: true, .. }
    )));
    assert!(has(&|kind| matches!(kind, Kind::RecoveryDiagnostic { .. })));
    assert!(has(&|kind| matches!(kind, Kind::Compacted { .. })));
    assert!(has(&|kind| matches!(
        kind,
        Kind::Message {
            origin: localpilot_store::MessageOrigin::Synthetic { .. },
            ..
        }
    )));
    assert!(has(&|kind| matches!(kind, Kind::TurnEnded { .. })));

    // The chain is well-formed: every event descends from its predecessor.
    for pair in events.windows(2) {
        assert_eq!(pair[1].parent_id, Some(pair[0].id));
    }
}

#[tokio::test]
async fn user_shell_runs_are_auditable_and_context_exclusion_works() {
    let provider = Arc::new(FakeProvider::new().text("ok"));
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig::default(),
        Profile::Default,
    );
    let session = h.runtime.session_id();

    // An excluded run: permission-gated, in the event log, not in context.
    let excluded = h
        .runtime
        .run_user_shell("git", &["--version".to_string()], true)
        .await;
    assert!(!excluded.is_error, "{}", excluded.output);
    assert!(h.store.read_transcript(session).unwrap().is_empty());

    // An included run lands in the transcript as a shell-role message.
    let included = h
        .runtime
        .run_user_shell("git", &["--version".to_string()], false)
        .await;
    assert!(!included.is_error, "{}", included.output);
    let transcript = h.store.read_transcript(session).unwrap();
    assert_eq!(transcript.len(), 1);
    assert_eq!(transcript[0].role, localpilot_core::Role::UserShell);

    // Both runs are auditable from the event log, and the transcript is still
    // exactly the Message events.
    let events = h.store.read_events(session).unwrap();
    let shell_runs = events
        .iter()
        .filter(|event| {
            matches!(
                &event.kind,
                localpilot_store::SessionEventKind::ToolFinished { name, .. } if name == "run_shell"
            )
        })
        .count();
    assert_eq!(shell_runs, 2);
    assert_eq!(
        localpilot_store::transcript_from_events(&events),
        transcript
    );

    // The next model turn sees the included run but not the excluded one.
    let reason = h.runtime.run_turn("hi", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);
    let request = provider.requests().pop().unwrap();
    let roles: Vec<_> = request.messages.iter().map(|m| m.role).collect();
    assert!(roles.contains(&localpilot_core::Role::UserShell));
}
