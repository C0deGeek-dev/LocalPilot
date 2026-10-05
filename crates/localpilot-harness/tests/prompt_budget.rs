//! The fixed part of every request — the agent system prompt plus the tool
//! specs it advertises — stays under a recorded byte ceiling, so growth in
//! either is a deliberate choice rather than drift.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use localpilot_core::{ContentBlock, Role};
use localpilot_harness::{SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Ceiling for the advertised tool specs of the built-in registry alone, as
/// the compact JSON of the request's `ToolSpec` list (an internal measure: a
/// provider adapter adds its own wrapper around each schema). A component
/// check; the CLI's default registry, LocalMind tools included, has its own
/// gate. Measured at 15,628 bytes once generated annotations were removed and
/// the longest descriptions tightened.
const TOOL_SPEC_CEILING: usize = 16_000;
/// Ceiling for the agent system prompt of a default session in an empty
/// workspace (no instruction files, no hooks). Measured at 2,499 bytes after
/// the prompt was rewritten (3,919 before).
const SYSTEM_PROMPT_CEILING: usize = 2_700;

#[tokio::test]
async fn the_fixed_request_overhead_stays_under_its_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeProvider::new().text("ok"));
    let mut runtime = SessionRuntime::new(
        provider.clone(),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    );
    let (events, _rx) = broadcast::channel(64);
    let reason = runtime
        .run_turn("hi", &events, &CancellationToken::new())
        .await;
    assert_eq!(reason, StopReason::Done);

    let request = provider.requests().remove(0);
    let system: usize = request
        .messages
        .iter()
        .take_while(|message| message.role == Role::System)
        .flat_map(|message| &message.content)
        .map(|block| match block {
            ContentBlock::Text { text } => text.len(),
            _ => 0,
        })
        .sum();
    let mut per_tool: Vec<(usize, &str)> = request
        .tools
        .iter()
        .map(|tool| {
            (
                serde_json::to_string(tool).unwrap().len(),
                tool.name.as_str(),
            )
        })
        .collect();
    let tools = serde_json::to_string(&request.tools).unwrap().len();
    per_tool.sort_unstable_by(|a, b| b.cmp(a));
    let largest: Vec<String> = per_tool
        .iter()
        .take(5)
        .map(|(bytes, name)| format!("{name}={bytes}"))
        .collect();
    eprintln!("system prompt {system} B, tool specs {tools} B; largest: {largest:?}");

    assert!(
        tools <= TOOL_SPEC_CEILING,
        "advertised tool specs grew to {tools} bytes (ceiling {TOOL_SPEC_CEILING}); \
         largest: {largest:?}"
    );
    assert!(
        system <= SYSTEM_PROMPT_CEILING,
        "the agent system prompt grew to {system} bytes (ceiling {SYSTEM_PROMPT_CEILING})"
    );
}
