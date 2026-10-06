//! Answer-only requests use current project context without running tools.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use localpilot_core::{ContentBlock, Role};
use localpilot_harness::{
    ContextHook, ContextPlacement, SessionConfig, SessionRuntime, StopReason,
};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::{MemoryUsed, SessionEventKind, Store};
use localpilot_tools::ToolRegistry;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct ProjectContext;

impl ContextHook for ProjectContext {
    fn name(&self) -> &str {
        "project-context"
    }

    fn context_for(&self, prompt: &str) -> Option<String> {
        Some(format!(
            "Relevant accepted project memory: convention for {prompt}"
        ))
    }

    fn memories_used(&self, prompt: &str) -> Vec<MemoryUsed> {
        vec![MemoryUsed {
            id: prompt.to_string(),
            score: 1,
            layer: "memory".to_string(),
        }]
    }
}

fn runtime(
    root: &std::path::Path,
    provider: Arc<FakeProvider>,
    answer_only: bool,
) -> SessionRuntime {
    runtime_with_limit(root, provider, answer_only, 24_000)
}

fn runtime_with_limit(
    root: &std::path::Path,
    provider: Arc<FakeProvider>,
    answer_only: bool,
    context_token_limit: usize,
) -> SessionRuntime {
    let mut runtime = SessionRuntime::new(
        provider,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(root),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            context_token_limit,
            answer_only,
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            // Even a caller that enables verification cannot execute it in an
            // answer-only turn. This command would be observable if it ran.
            verify_before_done: answer_only,
            verify_command: Some("missing-answer-only-verifier".to_string()),
            tool_marker_enabled: true,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    runtime.seed_system("Session instructions remain authoritative.");
    runtime
        .hooks_mut()
        .register_context_hook(Arc::new(ProjectContext));
    runtime
}

fn text(message: &localpilot_core::Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn adjacent_context_is_request_only_current_and_audited_on_every_turn() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .text("first answer")
            .text("second answer"),
    );
    let mut runtime = runtime(dir.path(), provider.clone(), true);
    let session = runtime.session_id();
    let (events, _) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    assert_eq!(
        runtime.run_turn("first question", &events, &cancel).await,
        StopReason::Done
    );
    assert_eq!(
        runtime.run_turn("second question", &events, &cancel).await,
        StopReason::Done
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    for (index, prompt) in ["first question", "second question"].iter().enumerate() {
        let request = &requests[index];
        assert!(request.tools.is_empty());
        let user = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .unwrap();
        assert_eq!(user.content.last(), Some(&ContentBlock::text(*prompt)));
        assert!(text(user).contains(&format!("convention for {prompt}")));
        assert!(!request
            .messages
            .iter()
            .filter(|message| message.role == Role::System)
            .any(|message| text(message).contains("convention for")));
    }
    assert!(!requests[1]
        .messages
        .iter()
        .any(|message| text(message).contains("convention for first question")));
    let logged = Store::open(dir.path()).read_events(session).unwrap();
    let serialized = serde_json::to_string(&logged).unwrap();
    assert!(
        !serialized.contains("convention for"),
        "retrieval must never accumulate in stored history"
    );
    let used: Vec<_> = logged
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::MemoriesUsed { memories } => Some(memories[0].id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(used, ["first question", "second question"]);
    assert_eq!(runtime.turn_tool_calls(), 0);
}

#[tokio::test]
async fn unadvertised_tool_calls_never_execute_even_under_bypass() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeProvider::new().tool_call(
        "write-1",
        "write_file",
        serde_json::json!({"path":"forbidden.txt","content":"host effect"}),
    ));
    let mut runtime = runtime(dir.path(), provider.clone(), true);
    let (events, _) = broadcast::channel(64);
    assert_eq!(
        runtime
            .run_turn("question", &events, &CancellationToken::new())
            .await,
        StopReason::ProviderError
    );
    assert!(provider.requests()[0].tools.is_empty());
    assert!(!dir.path().join("forbidden.txt").exists());
    assert_eq!(runtime.turn_tool_calls(), 0);
}

/// A hook that contributes standing instructions, which stay in the system
/// prompt wherever retrieved context goes.
struct StandingRule;

impl ContextHook for StandingRule {
    fn name(&self) -> &str {
        "standing-rule"
    }

    fn context_for(&self, _prompt: &str) -> Option<String> {
        Some("Standing rule: indent with tabs.".to_string())
    }

    fn placement(&self) -> ContextPlacement {
        ContextPlacement::System
    }
}

/// An ordinary session with a standing-rule hook registered before the
/// retrieval hook, so hook order is observable.
fn with_standing_rule_first(root: &std::path::Path, provider: Arc<FakeProvider>) -> SessionRuntime {
    let mut runtime = SessionRuntime::new(
        provider,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(root),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    runtime.seed_system("Session instructions remain authoritative.");
    runtime
        .hooks_mut()
        .register_context_hook(Arc::new(StandingRule));
    runtime
        .hooks_mut()
        .register_context_hook(Arc::new(ProjectContext));
    runtime
}

fn system_text(request: &localpilot_llm::ModelRequest) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .map(text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn ordinary_sessions_place_retrieved_context_beside_the_question_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeProvider::new().text("answer"));
    let mut runtime = with_standing_rule_first(dir.path(), provider.clone());
    let session = runtime.session_id();
    let (events, _) = broadcast::channel(64);
    assert_eq!(
        runtime
            .run_turn("question", &events, &CancellationToken::new())
            .await,
        StopReason::Done
    );
    let request = &provider.requests()[0];
    assert!(
        !request.tools.is_empty(),
        "an ordinary session keeps its tools"
    );
    let system = system_text(request);
    assert!(
        system.contains("Standing rule: indent with tabs."),
        "{system}"
    );
    assert!(!system.contains("convention for"), "{system}");
    let user = request
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .unwrap();
    assert!(
        text(user).contains("convention for question"),
        "{}",
        text(user)
    );
    assert!(!text(user).contains("Standing rule"));
    assert_eq!(user.content.last(), Some(&ContentBlock::text("question")));
    let logged =
        serde_json::to_string(&Store::open(dir.path()).read_events(session).unwrap()).unwrap();
    assert!(
        !logged.contains("convention for"),
        "retrieval must never accumulate in stored history"
    );
}

#[tokio::test]
async fn switching_the_placement_off_reproduces_the_single_system_block_in_hook_order() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeProvider::new().text("answer"));
    let mut runtime = with_standing_rule_first(dir.path(), provider.clone());
    runtime.set_retrieved_beside_question(false);
    let (events, _) = broadcast::channel(64);
    assert_eq!(
        runtime
            .run_turn("question", &events, &CancellationToken::new())
            .await,
        StopReason::Done
    );
    let request = &provider.requests()[0];
    let system: Vec<String> = request
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .map(text)
        .collect();
    assert_eq!(system.len(), 1, "one leading system message");
    assert!(
        system[0].contains(
            "Standing rule: indent with tabs.\nRelevant accepted project memory: convention for question"
        ),
        "hook order is kept: {}",
        system[0]
    );
    let user = request
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .unwrap();
    assert_eq!(text(user), "question");
}

#[tokio::test]
async fn context_is_sent_again_after_a_tool_call_and_never_stored() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("c1", "list_files", serde_json::json!({ "path": "." }))
            .text("answer"),
    );
    let mut runtime = with_standing_rule_first(dir.path(), provider.clone());
    let session = runtime.session_id();
    let (events, _) = broadcast::channel(64);
    assert_eq!(
        runtime
            .run_turn("question", &events, &CancellationToken::new())
            .await,
        StopReason::Done
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "the tool result needs a second request");
    for request in &requests {
        let question = request
            .messages
            .iter()
            .find(|message| message.role == Role::User && !message.is_synthetic())
            .unwrap();
        assert!(text(question).contains("convention for question"));
        assert!(request
            .messages
            .iter()
            .filter(|message| message.role != Role::User)
            .all(|message| !text(message).contains("convention for")));
    }
    let logged =
        serde_json::to_string(&Store::open(dir.path()).read_events(session).unwrap()).unwrap();
    assert!(!logged.contains("convention for"));
}

#[tokio::test]
async fn ordinary_context_stays_with_the_real_question_through_a_repair() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeProvider::new().malformed().text("answer"));
    let mut runtime = with_standing_rule_first(dir.path(), provider.clone());
    let (events, _) = broadcast::channel(64);
    assert_eq!(
        runtime
            .run_turn("question", &events, &CancellationToken::new())
            .await,
        StopReason::Done
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let question = request
            .messages
            .iter()
            .find(|message| message.role == Role::User && !message.is_synthetic())
            .unwrap();
        assert!(text(question).contains("convention for question"));
        assert!(!request
            .messages
            .iter()
            .filter(|message| message.is_synthetic())
            .any(|message| text(message).contains("convention for")));
    }
}

#[tokio::test]
async fn with_the_placement_off_ordinary_sessions_keep_system_context_and_tool_schemas() {
    // The rollback path: `[context] retrieved_beside_question = false`.
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeProvider::new().text("answer"));
    let mut runtime = runtime(dir.path(), provider.clone(), false);
    runtime.set_retrieved_beside_question(false);
    let (events, _) = broadcast::channel(64);
    assert_eq!(
        runtime
            .run_turn("question", &events, &CancellationToken::new())
            .await,
        StopReason::Done
    );
    let request = &provider.requests()[0];
    assert!(!request.tools.is_empty());
    assert!(request
        .messages
        .iter()
        .any(|message| message.role == Role::System
            && text(message).contains("convention for question")));
    assert_eq!(
        text(
            request
                .messages
                .iter()
                .find(|message| message.role == Role::User)
                .unwrap()
        ),
        "question"
    );
}

#[tokio::test]
async fn context_stays_with_the_question_through_repairs_and_preserves_attachments() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeProvider::new().malformed().text("answer"));
    let mut runtime = runtime(dir.path(), provider.clone(), true);
    let (events, _) = broadcast::channel(64);
    let attachment = ContentBlock::image("image/png", "fixture-image");
    assert_eq!(
        runtime
            .run_turn_with_attachments(
                "question",
                &[attachment.clone()],
                &events,
                &CancellationToken::new()
            )
            .await,
        StopReason::Done
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let question = request
            .messages
            .iter()
            .find(|message| message.role == Role::User && !message.is_synthetic())
            .unwrap();
        assert_eq!(question.content.len(), 3);
        assert!(text(question).contains("convention for question"));
        assert_eq!(question.content[1], ContentBlock::text("question"));
        assert_eq!(question.content[2], attachment);
        assert!(!request
            .messages
            .iter()
            .filter(|message| message.is_synthetic())
            .any(|message| text(message).contains("convention for")));
        assert!(request.tools.is_empty());
    }
}

#[tokio::test]
async fn adjacent_context_is_reserved_when_old_history_needs_compaction() {
    use localpilot_llm::ModelEvent;
    let dir = tempfile::tempdir().unwrap();
    // No fake token-usage report: exercise the runtime's normal initial
    // estimator, rather than calibrating it from the fake provider's 1 token.
    let provider = Arc::new(
        FakeProvider::new()
            .script(vec![
                Ok(ModelEvent::TextDelta("old answer ".repeat(200))),
                Ok(ModelEvent::Done),
            ])
            .script(vec![
                Ok(ModelEvent::TextDelta("recent answer ".repeat(200))),
                Ok(ModelEvent::Done),
            ])
            .script(vec![
                Ok(ModelEvent::TextDelta("answer".to_string())),
                Ok(ModelEvent::Done),
            ]),
    );
    // The runtime's fixed instructions need about 1,000 tokens. Leave room
    // for them and the current context while forcing older answers out.
    let mut runtime = runtime_with_limit(dir.path(), provider.clone(), true, 1_600);
    let (events, _) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    assert_eq!(
        runtime.run_turn("first question", &events, &cancel).await,
        StopReason::Done
    );
    assert_eq!(
        runtime.run_turn("second question", &events, &cancel).await,
        StopReason::Done
    );
    assert_eq!(
        runtime.run_turn("third question", &events, &cancel).await,
        StopReason::Done
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        localpilot_harness::estimate_tokens(&requests[2].messages) <= 1_600,
        "{} tokens: {:?}",
        localpilot_harness::estimate_tokens(&requests[2].messages),
        requests[2]
            .messages
            .iter()
            .map(|message| (message.role, text(message).len()))
            .collect::<Vec<_>>()
    );
    assert!(!requests[2]
        .messages
        .iter()
        .any(|message| text(message).contains(&"old answer ".repeat(200))));
    let user = requests[2]
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .unwrap();
    assert!(text(user).contains("convention for third question"));
    assert!(text(user).ends_with("third question"));
}
