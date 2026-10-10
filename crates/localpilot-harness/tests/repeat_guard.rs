//! The same call returning the same result again and again is caught: the
//! second consecutive identical observation is nudged, the third stops the
//! turn. Every call still runs, so a result that changes is never mistaken for
//! a repeat, and anything different in between restarts the count.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use localpilot_core::{ContentBlock, Message, ToolImage};
use localpilot_harness::{
    RuntimeEvent, SessionConfig, SessionRuntime, SoftInterrupt, SoftInterruptSource, SteerQueue,
    StopReason,
};
use localpilot_llm::{FakeProvider, ModelEvent};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{
    Effect, Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace,
};
use localpilot_store::{SessionEventKind, Store};
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// The model-visible notice appended to a repeated observation.
const REPEAT_MARKER: &str = "[repeated call]";

struct AvailabilityTool {
    calls: Arc<AtomicUsize>,
    absent: Vec<bool>,
}

#[async_trait]
impl Tool for AvailabilityTool {
    fn name(&self) -> &str {
        "availability"
    }
    fn description(&self) -> &str {
        "freshly checked fixture backend"
    }
    fn schema(&self) -> Value {
        json!({"type":"object", "additionalProperties":true})
    }
    fn effects(&self, _: &Value, _: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(Vec::new())
    }
    async fn invoke(&self, _: Value, _: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.absent.get(n).copied().unwrap_or(false) {
            Ok(
                ToolOutput::ok(format!("missing fixture index on observation {n}"))
                    .with_unavailable_backend("fixture_index"),
            )
        } else {
            Ok(ToolOutput::ok(if n + 1 == self.absent.len() {
                "valid hit"
            } else {
                "no data"
            }))
        }
    }
}

fn availability_runtime(
    absent: Vec<bool>,
    config: SessionConfig,
) -> (SessionRuntime, tempfile::TempDir, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut provider = FakeProvider::new();
    for i in 0..absent.len() {
        provider = provider.tool_call(
            &format!("q{i}"),
            "availability",
            json!({"query":format!("different{i}")}),
        );
    }
    provider = provider.text("finished");
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(AvailabilityTool {
        calls: Arc::clone(&calls),
        absent,
    }));
    let (runtime, dir) = build(provider, registry, config, &[]);
    (runtime, dir, calls)
}

#[tokio::test]
async fn fresh_backend_absence_counts_varied_queries_and_varied_text() {
    let (runtime, dir, calls) = availability_runtime(vec![true; 5], default_rail());
    let r = run(runtime, &dir).await;
    assert_eq!(r.reason, StopReason::NoProgress);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "each counted observation invokes the tool"
    );
    assert_eq!(r.executed, 3);
    assert_eq!(
        r.detail.as_deref(),
        Some(r#"signal=backend_unavailable tool="availability" backend="fixture_index" count=3"#)
    );
    assert!(!r.results[0].1.contains("[backend unavailable]"));
    assert!(r.results[1].1.contains("[backend unavailable]"));
    assert!(r.results[1]
        .1
        .contains("Changing queries cannot restore it"));
}

#[tokio::test]
async fn a_freshly_restored_backend_breaks_the_unavailable_run() {
    let (runtime, dir, calls) =
        availability_runtime(vec![true, true, false, true, true, false], default_rail());
    let r = run(runtime, &dir).await;
    assert_eq!(r.reason, StopReason::Done);
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    assert!(!r.results[2].1.contains("[backend unavailable]"));
    assert!(!r.results[3].1.contains("[backend unavailable]"));
    assert!(r.results[4].1.contains("[backend unavailable]"));
    assert!(r.results[5].1.contains("valid hit"));
}

#[tokio::test]
async fn distinct_empty_queries_and_a_hit_do_not_mean_backend_absence() {
    let (runtime, dir, calls) = availability_runtime(vec![false; 5], default_rail());
    let r = run(runtime, &dir).await;
    assert_eq!(r.reason, StopReason::Done);
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    assert!(r.results[..4]
        .iter()
        .all(|(_, text)| text.contains("no data") && !text.contains("[backend unavailable]")));
    assert!(r.results[4].1.contains("valid hit"));
}

#[tokio::test]
async fn scripted_replay_opt_out_also_preserves_unavailable_observations() {
    let config = SessionConfig {
        stop_repeated_observations: false,
        ..default_rail()
    };
    let (runtime, dir, calls) = availability_runtime(vec![true; 5], config);
    let r = run(runtime, &dir).await;
    assert_eq!(r.reason, StopReason::Done);
    assert_eq!(calls.load(Ordering::SeqCst), 5);
}

// #234: unrelated work remains allowed while absent queries never execute.
// Use the real knowledge tool, not a text-matching fixture.
#[tokio::test]
async fn stale_knowledge_queries_interleaved_with_progress_do_not_stop_the_turn() {
    let mut provider = FakeProvider::new();
    for i in 0..123 {
        provider = provider
            .tool_call(
                &format!("missing{i}"),
                "knowledge_search",
                json!({"query":format!("different{i}")}),
            )
            .tool_call(&format!("poll{i}"), "poll", json!({}));
    }
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(localpilot_localmind::KnowledgeSearch));
    registry.register(Box::new(PollTool {
        calls: AtomicUsize::new(0),
    }));
    let config = SessionConfig {
        tool_call_budget: Some(300),
        tool_call_budget_max: Some(300),
        tool_budget_explicit: true,
        ..default_rail()
    };
    let (runtime, dir) = build(provider.text("finished"), registry, config, &[]);
    let r = run(runtime, &dir).await;
    assert_eq!(r.reason, StopReason::Done);
    assert_eq!(r.executed, 246);
    assert_eq!(
        r.results
            .iter()
            .filter(|(_, text)| text.contains("no indexed project knowledge"))
            .count(),
        123
    );
    assert!(r
        .results
        .iter()
        .filter(|(id, _)| id.starts_with("missing"))
        .all(|(_, text)| text.contains("was not executed")));
}

#[tokio::test]
async fn consecutive_stale_knowledge_queries_stop_without_execution() {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(localpilot_localmind::KnowledgeSearch));
    let mut provider = FakeProvider::new();
    for i in 0..123 {
        provider = provider.tool_call(
            &format!("missing{i}"),
            "knowledge_search",
            json!({"query":format!("different{i}")}),
        );
    }
    let (runtime, dir) = build(provider.text("finished"), registry, default_rail(), &[]);
    let r = run(runtime, &dir).await;
    assert_eq!(r.reason, StopReason::NoProgress);
    assert_eq!(r.executed, 3);
    assert_eq!(
        r.detail.as_deref(),
        Some(
            r#"signal=backend_unavailable_attempt tool="knowledge_search" backend="project_knowledge_indexes" count=3"#
        )
    );
}

#[tokio::test]
async fn stale_knowledge_queries_interleaved_with_empty_searches_do_not_stop_the_turn() {
    let mut provider = FakeProvider::new();
    for i in 0..123 {
        provider = provider
            .tool_call(
                &format!("missing{i}"),
                "knowledge_search",
                json!({"query":format!("different{i}")}),
            )
            .tool_call(
                &format!("empty{i}"),
                "search_text",
                json!({"path":"f.txt", "query":format!("absent{i}")}),
            );
    }
    let mut registry = ToolRegistry::with_builtins();
    registry.register(Box::new(localpilot_localmind::KnowledgeSearch));
    let config = SessionConfig {
        tool_call_budget: Some(300),
        tool_call_budget_max: Some(300),
        tool_budget_explicit: true,
        ..default_rail()
    };
    let (runtime, dir) = build(
        provider.text("finished"),
        registry,
        config,
        &[("f.txt", "x\n")],
    );
    let r = run(runtime, &dir).await;
    assert_eq!(r.reason, StopReason::Done);
    assert_eq!(r.executed, 246);
    assert_eq!(
        r.results
            .iter()
            .filter(|(_, text)| text == "tool: search_text\nstatus: success\noutput:\n")
            .count(),
        123
    );
    assert_eq!(
        r.results
            .iter()
            .filter(|(_, text)| text.contains("no indexed project knowledge"))
            .count(),
        123
    );
    assert!(r
        .results
        .iter()
        .filter(|(id, _)| id.starts_with("missing"))
        .all(|(_, text)| text.contains("was not executed")));
}

/// A tool whose output changes on every call, like polling a job that advances.
struct PollTool {
    calls: AtomicUsize,
}

#[async_trait]
impl Tool for PollTool {
    fn name(&self) -> &str {
        "poll"
    }
    fn description(&self) -> &str {
        "test tool whose output advances each call"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "additionalProperties": true })
    }
    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(Vec::new())
    }
    async fn invoke(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok(format!("progress {n}")))
    }
}

/// A tool that always fails the same way and, on its second invocation,
/// queues a user steering message — admitted at the next safe boundary.
struct SteeringFailTool {
    calls: AtomicUsize,
    queue: Arc<Mutex<Option<SteerQueue>>>,
}

#[async_trait]
impl Tool for SteeringFailTool {
    fn name(&self) -> &str {
        "fragile"
    }
    fn description(&self) -> &str {
        "test tool that always fails identically"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "additionalProperties": true })
    }
    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(Vec::new())
    }
    async fn invoke(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
            if let Some(queue) = self.queue.lock().unwrap().clone() {
                queue.push_interrupt(SoftInterrupt {
                    content: "try it once more, it should work now".to_string(),
                    source: SoftInterruptSource::User,
                    urgent: false,
                });
            }
        }
        Err(ToolError::Failed("connection refused".to_string()))
    }
}

/// A synthetic image tool: the same description every call, over image bytes
/// that either change each call (`changing`) or stay the same.
struct SnapshotTool {
    calls: AtomicUsize,
    changing: bool,
}

#[async_trait]
impl Tool for SnapshotTool {
    fn name(&self) -> &str {
        "snapshot"
    }
    fn description(&self) -> &str {
        "test tool returning an image with a fixed description"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "additionalProperties": true })
    }
    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(Vec::new())
    }
    async fn invoke(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        // Equal-length payloads, so the description cannot tell them apart.
        let data = if self.changing {
            format!("frame{:03}", n % 1000)
        } else {
            "frame000".to_string()
        };
        Ok(
            ToolOutput::ok("snapshot: 8 bytes, image/png").with_image(ToolImage {
                media_type: "image/png".to_string(),
                data,
            }),
        )
    }
}

fn default_rail() -> SessionConfig {
    SessionConfig {
        interactivity: Interactivity::NonInteractive,
        trusted: true,
        ..SessionConfig::default()
    }
}

fn build(
    provider: FakeProvider,
    registry: ToolRegistry,
    config: SessionConfig,
    files: &[(&str, &str)],
) -> (SessionRuntime, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    for (name, contents) in files {
        std::fs::write(dir.path().join(name), contents).unwrap();
    }
    let runtime = SessionRuntime::new(
        Arc::new(provider),
        registry,
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        config,
        Vec::new(),
    );
    (runtime, dir)
}

struct Readback {
    reason: StopReason,
    detail: Option<String>,
    executed: usize,
    tool_uses: Vec<String>,
    results: Vec<(String, String)>,
    warnings: Vec<String>,
}

async fn run(mut runtime: SessionRuntime, dir: &tempfile::TempDir) -> Readback {
    let (events, mut rx) = broadcast::channel(4096);
    let cancel = CancellationToken::new();
    let reason = runtime.run_turn("go", &events, &cancel).await;
    let mut executed = 0;
    let mut warnings = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if matches!(event, RuntimeEvent::ToolFinished { .. }) {
            executed += 1;
        }
        if let RuntimeEvent::Warning(warning) = event {
            warnings.push(warning);
        }
    }
    let session = runtime.session_id();
    let store = Store::open(dir.path());
    let detail = store
        .read_events(session)
        .unwrap()
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            SessionEventKind::TurnEnded { detail, .. } => Some(detail.clone()),
            _ => None,
        })
        .expect("the turn recorded a TurnEnded event");
    let transcript: Vec<Message> = store.read_transcript(session).unwrap();
    let mut tool_uses = Vec::new();
    let mut results = Vec::new();
    for block in transcript.iter().flat_map(|message| &message.content) {
        match block {
            ContentBlock::ToolUse(call) => tool_uses.push(call.id.to_string()),
            ContentBlock::ToolResult(result) => {
                results.push((result.id.to_string(), result.output.clone()));
            }
            _ => {}
        }
    }
    Readback {
        reason,
        detail,
        executed,
        tool_uses,
        results,
        warnings,
    }
}

fn identical_calls(name: &str, input: &Value, count: usize) -> FakeProvider {
    let mut provider = FakeProvider::new();
    for i in 0..count {
        provider = provider.tool_call(&format!("c{i}"), name, input.clone());
    }
    provider.text("done")
}

#[tokio::test]
async fn three_identical_failing_commands_stop_on_the_third() {
    let provider = identical_calls("run_shell", &json!({ "command": "exit 7" }), 6);
    let (runtime, dir) = build(provider, ToolRegistry::with_builtins(), default_rail(), &[]);
    // Exercise an actual exit failure, rather than bypass's opaque-code denial.
    runtime
        .permission_engine_handle()
        .set(PermissionEngine::new(Profile::Unrestricted, Vec::new()));
    let r = run(runtime, &dir).await;

    assert_eq!(r.reason, StopReason::NoProgress);
    assert_eq!(r.executed, 3, "the fourth identical call never runs");
    // The outcome class depends on how the host runs the command; the guard
    // treats every class alike, so only the shape and count are pinned.
    let detail = r.detail.unwrap();
    assert!(
        detail.starts_with(r#"signal=repeated_observation tool="run_shell" outcome="#)
            && detail.contains(" count=3 call=")
            && detail.contains(" result="),
        "persisted detail: {detail}"
    );
    assert!(
        !r.results[0].1.contains(REPEAT_MARKER),
        "first is not a repeat"
    );
    assert!(r.results[1].1.contains(REPEAT_MARKER), "second is nudged");
}

#[tokio::test]
async fn an_explicit_cost_budget_does_not_disable_the_repeat_stop() {
    let provider = identical_calls("run_shell", &json!({ "command": "exit 7" }), 6);
    let config = SessionConfig {
        tool_call_budget: Some(50),
        tool_call_budget_max: Some(50),
        tool_budget_explicit: true,
        ..default_rail()
    };
    let (runtime, dir) = build(provider, ToolRegistry::with_builtins(), config, &[]);
    let r = run(runtime, &dir).await;

    assert_eq!(r.reason, StopReason::NoProgress);
    assert_eq!(r.executed, 3);
}

#[tokio::test]
async fn identical_calls_in_one_response_stop_mid_batch_with_every_call_answered() {
    let call = |i: usize| {
        Ok(ModelEvent::ToolCall {
            id: format!("b{i}"),
            name: "read_file".to_string(),
            input_json: json!({ "path": "f.txt" }),
            provider_metadata: None,
        })
    };
    let provider = FakeProvider::new()
        .script(vec![
            call(0),
            call(1),
            call(2),
            call(3),
            call(4),
            Ok(ModelEvent::Done),
        ])
        .text("done");
    let (runtime, dir) = build(
        provider,
        ToolRegistry::with_builtins(),
        default_rail(),
        &[("f.txt", "x\n")],
    );
    let r = run(runtime, &dir).await;

    assert_eq!(r.reason, StopReason::NoProgress);
    assert_eq!(r.executed, 3, "exactly three of the five calls run");
    assert!(r
        .detail
        .unwrap()
        .contains(r#"tool="read_file" outcome=Ok count=3"#));
    let answered: Vec<&String> = r.results.iter().map(|(id, _)| id).collect();
    assert_eq!(
        answered,
        r.tool_uses.iter().collect::<Vec<_>>(),
        "every tool_use has exactly one result, in order"
    );
    assert!(r.results[3].1.contains("skipped"));
    assert!(r.results[4].1.contains("skipped"));
}

#[tokio::test]
async fn an_edit_and_rerun_loop_with_the_same_failure_is_not_stopped() {
    // Rerunning the same failing build after each edit observes the same
    // failure, but never twice in a row: real progress, so no nudge, no stop.
    let mut provider = FakeProvider::new();
    for i in 0..3 {
        provider = provider
            .tool_call(
                &format!("e{i}"),
                "write_file",
                json!({ "path": "a.txt", "content": format!("attempt {i}\n") }),
            )
            .tool_call(
                &format!("b{i}"),
                "run_shell",
                json!({ "command": "exit 7" }),
            );
    }
    let (runtime, dir) = build(
        provider.text("still failing; here is what I found"),
        ToolRegistry::with_builtins(),
        default_rail(),
        &[],
    );
    let r = run(runtime, &dir).await;

    assert_eq!(r.reason, StopReason::Done);
    assert_eq!(r.executed, 6);
    assert!(r
        .results
        .iter()
        .all(|(_, out)| !out.contains(REPEAT_MARKER)));
}

#[tokio::test]
async fn polling_whose_output_advances_is_not_stopped() {
    let mut registry = ToolRegistry::with_builtins();
    registry.register(Box::new(PollTool {
        calls: AtomicUsize::new(0),
    }));
    let provider = identical_calls("poll", &json!({ "job": "build-1" }), 5);
    let (runtime, dir) = build(provider, registry, default_rail(), &[]);
    let r = run(runtime, &dir).await;

    assert_eq!(r.reason, StopReason::Done);
    assert_eq!(r.executed, 5);
    assert!(r
        .results
        .iter()
        .all(|(_, out)| !out.contains(REPEAT_MARKER)));
}

#[tokio::test]
async fn a_user_steer_between_repeats_restarts_the_count() {
    // The user steers after the second identical failure, so the run restarts:
    // two more identical failures nudge, and only the fifth call stops it.
    let queue = Arc::new(Mutex::new(None));
    let mut registry = ToolRegistry::with_builtins();
    registry.register(Box::new(SteeringFailTool {
        calls: AtomicUsize::new(0),
        queue: Arc::clone(&queue),
    }));
    let provider = identical_calls("fragile", &json!({}), 8);
    let (runtime, dir) = build(provider, registry, default_rail(), &[]);
    *queue.lock().unwrap() = Some(runtime.steer_queue());
    let r = run(runtime, &dir).await;

    assert_eq!(r.reason, StopReason::NoProgress);
    assert_eq!(r.executed, 5, "2 before the steer, then a fresh run of 3");
}

#[tokio::test]
async fn an_elided_unchanged_reread_is_judged_on_what_the_read_returned() {
    // Read elision replaces a repeated unchanged read with a stub naming the
    // previous call, so the model-visible text differs every time. The guard
    // judges the read's own result, so three identical reads still stop.
    let config = SessionConfig {
        elide_seen_reads: true,
        ..default_rail()
    };
    let provider = identical_calls("read_file", &json!({ "path": "f.txt" }), 6);
    let (runtime, dir) = build(
        provider,
        ToolRegistry::with_builtins(),
        config,
        &[("f.txt", "x\n")],
    );
    let r = run(runtime, &dir).await;

    assert_eq!(r.reason, StopReason::NoProgress);
    assert_eq!(r.executed, 3);
}

fn snapshot_run(changing: bool) -> (FakeProvider, ToolRegistry) {
    let mut registry = ToolRegistry::with_builtins();
    registry.register(Box::new(SnapshotTool {
        calls: AtomicUsize::new(0),
        changing,
    }));
    (identical_calls("snapshot", &json!({}), 5), registry)
}

#[tokio::test]
async fn a_changed_image_behind_the_same_description_is_not_a_repeat() {
    let (provider, registry) = snapshot_run(true);
    let (runtime, dir) = build(provider, registry, default_rail(), &[]);
    let r = run(runtime, &dir).await;

    assert_eq!(r.reason, StopReason::Done);
    assert_eq!(r.executed, 5);
    assert!(r
        .results
        .iter()
        .all(|(_, out)| !out.contains(REPEAT_MARKER)));
}

#[tokio::test]
async fn the_same_image_and_description_three_times_stops() {
    let (provider, registry) = snapshot_run(false);
    let (runtime, dir) = build(provider, registry, default_rail(), &[]);
    let r = run(runtime, &dir).await;

    assert_eq!(r.reason, StopReason::NoProgress);
    assert_eq!(r.executed, 3);
}

// #233: every attempt crosses the permission gate, but timeout/JSON changes
// cannot turn the same refused operation into progress.
#[tokio::test]
async fn equivalent_opaque_denials_nudge_then_stop_without_execution() {
    let mut provider = FakeProvider::new();
    for i in 0..6 {
        provider = provider.tool_call(
            &format!("d{i}"),
            "run_shell",
            json!({"command":"python -m unittest", "timeout_secs":i+1}),
        );
    }
    let (runtime, dir) = build(
        provider.text("done"),
        ToolRegistry::with_builtins(),
        default_rail(),
        &[],
    );
    let r = run(runtime, &dir).await;
    assert_eq!(r.reason, StopReason::NoProgress);
    assert_eq!(r.executed, 3);
    assert!(r
        .warnings
        .iter()
        .any(|warning| warning.contains("Approve it interactively")
            && warning.contains("permissions.allow_commands")));
    assert_eq!(
        r.detail.as_deref(),
        Some(r#"signal=permission_denied tool="run_shell" reason="opaque_command" count=3"#)
    );
    assert!(!r.results[0].1.contains("[permission denied]"));
    assert!(r.results[1].1.contains("[permission denied]"));
    assert!(r
        .results
        .iter()
        .all(|(_, text)| text.contains("This call was not executed")));
    let records = Store::open(dir.path())
        .read_events(Store::open(dir.path()).list_sessions().unwrap()[0].id)
        .unwrap();
    assert_eq!(records.iter().filter(|e| matches!(&e.kind, SessionEventKind::PermissionDecided { decision, detail, .. } if decision == "denied" && detail.contains("Approve it interactively"))).count(), 3);
}

#[tokio::test]
async fn different_opaque_commands_and_interleaved_tools_reset_denial_count() {
    let mut provider = FakeProvider::new();
    for (i, command) in [
        "python -m unittest",
        "python -m unittest",
        "python -m pytest",
        "python -m pytest",
        "python ./other_tests.py",
        "python ./other_tests.py",
    ]
    .iter()
    .enumerate()
    {
        provider = provider.tool_call(&format!("d{i}"), "run_shell", json!({"command":command}));
    }
    provider = provider.tool_call("read", "read_file", json!({"path":"note.txt"}));
    provider = provider.tool_call(
        "d6",
        "run_shell",
        json!({"command":"python ./other_tests.py"}),
    );
    let (runtime, dir) = build(
        provider.text("done"),
        ToolRegistry::with_builtins(),
        default_rail(),
        &[("note.txt", "progress")],
    );
    let r = run(runtime, &dir).await;
    assert_eq!(r.reason, StopReason::Done);
    assert_eq!(r.executed, 8);
    assert!(!r.results.last().unwrap().1.contains("[permission denied]"));
}

#[tokio::test]
async fn matching_structured_grant_runs_after_two_shell_denials() {
    use localpilot_sandbox::AllowedCommand;
    let provider = FakeProvider::new()
        .tool_call("d0", "run_shell", json!({"command":"cargo --version"}))
        .tool_call(
            "d1",
            "run_shell",
            json!({"command":"cargo --version", "timeout_secs":1}),
        )
        .tool_call(
            "corrected",
            "run_shell",
            json!({"program":"cargo", "args":["--version"]}),
        )
        .text("done");
    let (runtime, dir) = build(provider, ToolRegistry::with_builtins(), default_rail(), &[]);
    runtime.permission_engine_handle().set(
        PermissionEngine::new(Profile::ReadOnly, Vec::new()).with_allowed_commands(vec![
            AllowedCommand {
                program: "cargo".to_string(),
                args_prefix: vec!["--version".to_string()],
            },
        ]),
    );
    let r = run(runtime, &dir).await;
    assert_eq!(r.reason, StopReason::Done);
    assert_eq!(r.executed, 3);
    assert!(r.results[1].1.contains("[permission denied]"));
    assert!(r.results[2].1.contains("exit: 0"), "{}", r.results[2].1);
    assert!(!r.results[2].1.contains("[permission denied]"));
}

#[tokio::test]
async fn denied_file_targets_are_distinct() {
    let mut provider = FakeProvider::new();
    for path in ["one.txt", "one.txt", "two.txt", "two.txt", "three.txt"] {
        provider = provider.tool_call(path, "write_file", json!({"path":path, "content":"denied"}));
    }
    let (runtime, dir) = build(
        provider.text("done"),
        ToolRegistry::with_builtins(),
        default_rail(),
        &[],
    );
    runtime
        .permission_engine_handle()
        .set(PermissionEngine::new(Profile::ReadOnly, Vec::new()));
    let r = run(runtime, &dir).await;
    assert_eq!(r.reason, StopReason::Done);
    assert!(!dir.path().join("one.txt").exists());
    assert!(!r.results[2].1.contains("[permission denied]"));
}

#[tokio::test]
async fn denial_stop_pairs_remaining_calls_in_a_batch() {
    let mut calls = (0..5)
        .map(|i| {
            Ok(ModelEvent::ToolCall {
                id: format!("d{i}"),
                name: "run_shell".to_string(),
                input_json: json!({"command":"python -m unittest", "timeout_secs":i+1}),
                provider_metadata: None,
            })
        })
        .collect::<Vec<_>>();
    calls.push(Ok(ModelEvent::Done));
    let provider = FakeProvider::new().script(calls).text("done");
    let (runtime, dir) = build(provider, ToolRegistry::with_builtins(), default_rail(), &[]);
    let r = run(runtime, &dir).await;
    assert_eq!(r.reason, StopReason::NoProgress);
    assert_eq!(r.executed, 3);
    assert_eq!(r.tool_uses.len(), 5);
    assert_eq!(r.results.len(), 5);
    assert!(r.results[3..]
        .iter()
        .all(|(_, text)| text.contains("repeated equivalent permission denials")));
}

#[tokio::test]
async fn replay_opt_out_preserves_equivalent_denied_attempts() {
    let mut provider = FakeProvider::new();
    for i in 0..5 {
        provider = provider.tool_call(
            &format!("d{i}"),
            "run_shell",
            json!({"command":"python -m unittest", "timeout_secs":i+1}),
        );
    }
    let config = SessionConfig {
        stop_repeated_observations: false,
        ..default_rail()
    };
    let (runtime, dir) = build(
        provider.text("done"),
        ToolRegistry::with_builtins(),
        config,
        &[],
    );
    let r = run(runtime, &dir).await;
    assert_eq!(r.reason, StopReason::Done);
    assert_eq!(r.executed, 5);
}

#[tokio::test]
async fn a_preflight_only_backend_attempt_breaks_the_denial_sequence() {
    let provider = FakeProvider::new()
        .tool_call("d0", "run_shell", json!({"command":"python -m unittest"}))
        .tool_call(
            "d1",
            "run_shell",
            json!({"command":"python -m unittest", "timeout_secs":1}),
        )
        .tool_call("absent", "knowledge_search", json!({"query":"anything"}))
        .tool_call(
            "d2",
            "run_shell",
            json!({"command":"python -m unittest", "timeout_secs":2}),
        )
        .text("done");
    let mut registry = ToolRegistry::with_builtins();
    registry.register(Box::new(localpilot_localmind::KnowledgeSearch));
    let (runtime, dir) = build(provider, registry, default_rail(), &[]);
    let r = run(runtime, &dir).await;
    assert_eq!(r.reason, StopReason::Done);
    assert!(!r.results[3].1.contains("[permission denied]"));
}

#[tokio::test]
async fn every_equivalent_retry_rechecks_interactive_approval() {
    let provider = FakeProvider::new()
        .tool_call("d0", "run_shell", json!({"command":"exit 0"}))
        .tool_call(
            "d1",
            "run_shell",
            json!({"command":"exit 0", "timeout_secs":1}),
        )
        .tool_call(
            "approved",
            "run_shell",
            json!({"command":"exit 0", "timeout_secs":2}),
        )
        .text("done");
    let config = SessionConfig {
        interactivity: Interactivity::Interactive,
        ..default_rail()
    };
    let dir = tempfile::tempdir().unwrap();
    let runtime = SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::new(vec![false, false, true])),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        config,
        Vec::new(),
    );
    let r = run(runtime, &dir).await;
    assert_eq!(r.reason, StopReason::Done);
    assert!(r.results[1].1.contains("[permission denied]"));
    assert!(r.results[2].1.contains("exit: 0"));
}

#[test]
fn eval_preflight_requires_existing_permission_and_never_executes_tests() {
    use localpilot_sandbox::AllowedCommand;
    let (mut runtime, dir) = build(
        FakeProvider::new(),
        ToolRegistry::with_builtins(),
        default_rail(),
        &[],
    );
    let command = "python -m unittest discover -s tests";
    runtime.set_verify_before_done(true, Some(command.to_string()));
    assert!(runtime
        .preflight_eval_verification()
        .unwrap_err()
        .contains("file targets cannot be inspected"));
    runtime.permission_engine_handle().set(
        PermissionEngine::new(Profile::Bypass, Vec::new()).with_allowed_commands(vec![
            AllowedCommand {
                program: "python".into(),
                args_prefix: vec![
                    "-m".into(),
                    "unittest".into(),
                    "discover".into(),
                    "-s".into(),
                    "tests".into(),
                ],
            },
        ]),
    );
    let check = runtime.preflight_eval_verification().unwrap().unwrap();
    assert_eq!(check.program, "python");
    assert_eq!(check.args, ["-m", "unittest", "discover", "-s", "tests"]);
    assert!(!dir.path().join("__pycache__").exists());
    // The preflight result is no standing grant: the next snapshot can deny it.
    runtime
        .permission_engine_handle()
        .set(PermissionEngine::new(Profile::Bypass, Vec::new()));
    assert!(runtime.preflight_eval_verification().is_err());
}

#[test]
fn eval_preflight_retains_trust_and_incognito_floors_with_a_grant() {
    use localpilot_sandbox::AllowedCommand;
    for (trusted, incognito) in [(false, false), (true, true)] {
        let config = SessionConfig {
            trusted,
            incognito,
            ..default_rail()
        };
        let (mut runtime, _dir) = build(
            FakeProvider::new(),
            ToolRegistry::with_builtins(),
            config,
            &[],
        );
        runtime.set_verify_before_done(
            true,
            Some("python -m unittest discover -s tests".to_string()),
        );
        let engine = PermissionEngine::new(Profile::Default, Vec::new())
            .with_incognito(incognito)
            .with_allowed_commands(vec![AllowedCommand {
                program: "python".into(),
                args_prefix: vec!["-m".into(), "unittest".into()],
            }]);
        runtime.permission_engine_handle().set(engine);
        assert!(runtime.preflight_eval_verification().is_err());
    }
}

#[test]
fn eval_preflight_checks_the_detected_target_but_preserves_no_target_answers() {
    let (mut runtime, _dir) = build(
        FakeProvider::new(),
        ToolRegistry::with_builtins(),
        default_rail(),
        &[],
    );
    std::fs::create_dir(_dir.path().join("tests")).unwrap();
    std::fs::write(_dir.path().join("tests/test_example.py"), "").unwrap();
    runtime.set_automatic_verify_before_done();
    assert!(runtime.preflight_eval_verification().is_err());
    let (mut runtime, _dir) = build(
        FakeProvider::new(),
        ToolRegistry::with_builtins(),
        default_rail(),
        &[],
    );
    runtime.set_automatic_verify_before_done();
    assert!(runtime.preflight_eval_verification().unwrap().is_none());
}

#[test]
fn eval_test_grant_does_not_grant_an_inspectable_external_target() {
    use localpilot_sandbox::AllowedCommand;
    let (mut runtime, _dir) = build(
        FakeProvider::new(),
        ToolRegistry::with_builtins(),
        default_rail(),
        &[],
    );
    runtime.set_verify_before_done(
        true,
        Some("python -m unittest discover -s ../foreign".to_string()),
    );
    runtime.permission_engine_handle().set(
        PermissionEngine::new(Profile::Bypass, Vec::new()).with_allowed_commands(vec![
            AllowedCommand {
                program: "python".into(),
                args_prefix: vec![
                    "-m".into(),
                    "unittest".into(),
                    "discover".into(),
                    "-s".into(),
                    "../foreign".into(),
                ],
            },
        ]),
    );
    assert!(runtime
        .preflight_eval_verification()
        .unwrap_err()
        .contains("outside the workspace"));
}
