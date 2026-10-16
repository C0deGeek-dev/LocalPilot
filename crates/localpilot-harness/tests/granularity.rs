//! Offline provider scenarios exercise the real prompt, dispatch and checkpoint paths.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use localpilot_config::GranularityConfig;
use localpilot_harness::{
    draft_plan_with_profile,
    granularity::{ContextCapacity, Reliability, WorkProfile},
    persist_approved_plan, resume_one_step, Brief, BriefRevision, CheckStatus, PlanApproval,
    Progress, RuleEngine, RuntimeEvent, SessionConfig, SessionRuntime, StopReason,
};
use localpilot_llm::{FakeProvider, ModelEvent, ProviderError};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::json;
use std::{fmt::Write as _, path::Path, process::Command, sync::Arc};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

const BRIEF: &str = "# Brief: bounded\n\n## Summary\n\nDo it.\n\n## Requirements\n\n- It works\n\n## Constraints\n\n- Be small\n\n## Non-Goals\n\n- Else\n\n## Acceptance Criteria\n\n- It works\n";
const PLAN: &str = "# Progress: bounded\nBranch: feature/bounded\n\n## Steps\n\n- [ ] 1. Create first.txt\n  - covers: AC1\n  - verify: none - prose fixture\n  - scope: 1, 1, 1, 4\n  - depends: none\n- [ ] 2. Create second.txt\n  - covers: none\n  - verify: none - prose fixture\n  - scope: 1, 1, 1, 4\n  - depends: 1\n";

fn runtime(
    root: &Path,
    provider: Arc<FakeProvider>,
    profile: Profile,
    verify: Option<String>,
) -> SessionRuntime {
    runtime_with_seed(root, provider, profile, verify, Vec::new())
}

fn runtime_with_seed(
    root: &Path,
    provider: Arc<FakeProvider>,
    profile: Profile,
    verify: Option<String>,
    seed: Vec<localpilot_core::Message>,
) -> SessionRuntime {
    SessionRuntime::new(
        provider,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(profile, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(root),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            model: "arbitrary-not-a-capability-signal".to_string(),
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            context_token_limit: 8_000,
            granularity: Some(GranularityConfig::default()),
            verify_command: verify,
            ..SessionConfig::default()
        },
        seed,
    )
}

async fn turn(runtime: &mut SessionRuntime) -> (StopReason, Vec<RuntimeEvent>) {
    let (tx, mut rx) = broadcast::channel(256);
    let reason = runtime
        .run_turn("make one coherent change", &tx, &CancellationToken::new())
        .await;
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    (reason, events)
}

#[tokio::test]
async fn short_whole_file_read_uses_line_and_byte_limits_independently() {
    let dir = tempfile::tempdir().unwrap();
    let short = "ordinary source line with several words\n".repeat(18);
    assert!(short.len() > 80);
    std::fs::write(dir.path().join("short.txt"), &short).unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("short", "read_file", json!({"path":"short.txt"}))
            .text("reviewed the short file"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::ReadOnly, None);
    let (_, events) = turn(&mut agent).await;
    assert!(events.iter().any(|e| matches!(e, RuntimeEvent::ToolFinished { id, is_error: false, output, .. } if id == "short" && output.contains(&short))));
}

#[tokio::test]
async fn byte_small_whole_file_with_too_many_lines_returns_a_page() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("dense.txt"), "x\n".repeat(81)).unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("whole", "read_file", json!({"path":"dense.txt"}))
            .tool_call(
                "page",
                "read_file",
                json!({"path":"dense.txt", "start_line":1, "end_line":80}),
            )
            .text("used an explicit page"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::ReadOnly, None);
    let (_, events) = turn(&mut agent).await;
    assert_next_page(&events, "whole", "dense.txt", 81, 160);
    assert!(events.iter().any(
        |e| matches!(e, RuntimeEvent::ToolFinished { id, is_error: false, .. } if id == "page")
    ));
}

#[tokio::test]
async fn whole_file_exception_keeps_byte_and_tightened_line_bounds() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("long.txt"), "z".repeat(5000)).unwrap();
    std::fs::write(dir.path().join("three.txt"), "source line\n".repeat(3)).unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("long", "read_file", json!({"path":"long.txt"}))
            .tool_call("three", "read_file", json!({"path":"three.txt"}))
            .tool_call(
                "two",
                "read_file",
                json!({"path":"three.txt", "start_line":1, "end_line":2}),
            )
            .text("used a tightened page"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::ReadOnly, None);
    agent.set_granularity(GranularityConfig {
        max_read_lines: Some(2),
        ..GranularityConfig::default()
    });
    let (_, events) = turn(&mut agent).await;
    assert!(events.iter().any(
        |e| matches!(e, RuntimeEvent::ToolFinished { id, is_error: true, .. } if id == "long")
    ));
    assert_page_hint(&events, "long", "long.txt", 2);
    assert_next_page(&events, "three", "three.txt", 3, 4);
    assert!(events.iter().any(
        |e| matches!(e, RuntimeEvent::ToolFinished { id, is_error: false, .. } if id == "two")
    ));
}

#[tokio::test]
async fn denied_whole_file_read_is_refused_before_content_dependent_limits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "private-fixture\n".repeat(600)).unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("secret", "read_file", json!({"path":".env"}))
            .text("access denied"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::ReadOnly, None);
    let (_, events) = turn(&mut agent).await;
    assert!(events.iter().any(|e| matches!(e, RuntimeEvent::ToolFinished { id, is_error: true, output, .. } if id == "secret" && output.contains("denied") && !output.contains("private-fixture") && !output.contains("explicit start_line"))));
}

#[tokio::test]
async fn giant_whole_file_read_returns_first_page_and_explicit_pages_work() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("large.txt"),
        (1..=400).fold(String::new(), |mut s, n| {
            writeln!(s, "line {n}").unwrap();
            s
        }),
    )
    .unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("whole", "read_file", json!({"path":"large.txt"}))
            .tool_call(
                "page1",
                "read_file",
                json!({"path":"large.txt", "start_line":1, "end_line":80}),
            )
            .tool_call(
                "page2",
                "read_file",
                json!({"path":"large.txt", "start_line":81, "end_line":160}),
            )
            .text("read two bounded pages"),
    );
    let mut agent = runtime(dir.path(), provider.clone(), Profile::Bypass, None);
    let (reason, events) = turn(&mut agent).await;
    assert_eq!(reason, StopReason::Done);
    assert_next_page(&events, "whole", "large.txt", 81, 160);
    for id in ["page1", "page2"] {
        assert!(events.iter().any(|e| matches!(e, RuntimeEvent::ToolFinished { id: actual, is_error: false, .. } if actual == id)));
    }
    let requests = provider.requests();
    assert!(requests
        .first()
        .unwrap()
        .messages
        .iter()
        .any(|m| format!("{m:?}").contains("Work unit: change at most")));
}

fn assert_next_page(events: &[RuntimeEvent], id: &str, path: &str, start: u64, end: u64) {
    let output = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ToolFinished {
                id: actual,
                is_error: false,
                output,
                ..
            } if actual == id => Some(output),
            _ => None,
        })
        .expect("successful bounded read");
    assert!(output.contains("[bounded read]"));
    let call: serde_json::Value =
        serde_json::from_str(output.split_once("Retry with: ").expect("next page").1)
            .expect("valid JSON call");
    assert_eq!(
        call,
        json!({"name":"read_file", "arguments":{"path":path,"start_line":start,"end_line":end}})
    );
}

#[tokio::test]
async fn implicit_missing_file_reports_authorized_io_error_without_a_paging_hint() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("missing", "read_file", json!({"path":"missing.txt"}))
            .text("missing file"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::ReadOnly, None);
    let (_, events) = turn(&mut agent).await;
    assert!(events.iter().any(|e| matches!(e, RuntimeEvent::ToolFinished { id, is_error: true, output, .. } if id == "missing" && output.contains("missing.txt") && !output.contains("Retry with:") && !output.contains("work envelope"))));
}

fn assert_page_hint(events: &[RuntimeEvent], id: &str, path: &str, end: u64) {
    let output = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ToolFinished {
                id: actual,
                is_error: true,
                output,
                ..
            } if actual == id => Some(output),
            _ => None,
        })
        .expect("refused read");
    let call: serde_json::Value =
        serde_json::from_str(output.split_once("Retry with: ").expect("copyable hint").1)
            .expect("valid JSON call");
    assert_eq!(
        call,
        json!({"name":"read_file", "arguments":{"path":path, "start_line":1, "end_line":end}})
    );
}

#[tokio::test]
async fn cumulative_small_edits_cannot_evade_one_region_budget() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "before\n").unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "one",
                "edit_file",
                json!({"path":"a.txt", "old_text":"before", "new_text":"after"}),
            )
            .tool_call(
                "two",
                "edit_file",
                json!({"path":"a.txt", "old_text":"after", "new_text":"wider"}),
            )
            .text("checkpoint"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::Bypass, None);
    let (_, events) = turn(&mut agent).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "after\n"
    );
    assert!(events.iter().any(|e| matches!(e, RuntimeEvent::ToolFinished { id, is_error: true, output, .. } if id == "two" && output.contains("cumulative"))));
}

#[tokio::test]
async fn oversized_create_and_large_file_overwrite_preserve_original() {
    let dir = tempfile::tempdir().unwrap();
    let large = "original\n".repeat(500);
    std::fs::write(dir.path().join("existing.txt"), &large).unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "giant",
                "write_file",
                json!({"path":"new.txt", "content":"x".repeat(20_000)}),
            )
            .tool_call(
                "erase",
                "write_file",
                json!({"path":"existing.txt", "content":"tiny", "overwrite":true}),
            )
            .text("needs smaller exact edits"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::Bypass, None);
    let (_, events) = turn(&mut agent).await;
    assert!(!dir.path().join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("existing.txt")).unwrap(),
        large
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, RuntimeEvent::ToolFinished { is_error: true, .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn mixed_external_patch_cannot_skip_tracked_deletion_bounds_in_either_order() {
    for external_first in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let external = outside.path().join("header.py");
        std::fs::write(dir.path().join("reports.py"), "before\n").unwrap();
        project(dir.path());
        let create = json!({"action":"create", "path":external, "content":"header\n"});
        let delete = json!({"action":"delete", "path":"reports.py"});
        let operations = if external_first {
            vec![create, delete]
        } else {
            vec![delete, create]
        };
        let provider = Arc::new(
            FakeProvider::new()
                .tool_call("mixed", "apply_patch", json!({"operations":operations}))
                .text("checkpoint"),
        );
        let mut agent = runtime(dir.path(), provider, Profile::Unrestricted, None);
        let (_, events) = turn(&mut agent).await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("reports.py"))
                .ok()
                .as_deref(),
            Some("before\n"),
            "mixed patch must be refused before either write, external_first={external_first}"
        );
        assert!(
            !external.exists(),
            "refusal must not dispatch a partial patch"
        );
        assert!(events.iter().any(|event| matches!(event,
            RuntimeEvent::ToolFinished { id, is_error:true, output, .. }
            if id == "mixed" && output.contains("work envelope:")
        )));
    }
}

#[tokio::test]
async fn permitted_external_write_is_bounded_without_changing_permission_scope() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let external = outside.path().join("small.txt");
    project(dir.path());
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "external",
                "write_file",
                json!({"path":external, "content":"small\n"}),
            )
            .tool_call(
                "second",
                "write_file",
                json!({"path":"second.txt", "content":"another\n"}),
            )
            .text("checkpoint"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::Unrestricted, None);
    let (_, events) = turn(&mut agent).await;
    assert_eq!(std::fs::read_to_string(external).unwrap(), "small\n");
    assert!(!dir.path().join("second.txt").exists());
    assert!(events.iter().any(|event| matches!(event,
        RuntimeEvent::ToolFinished { id, is_error:true, output, .. }
        if id == "second" && output.contains("work envelope:")
    )));
}

#[tokio::test]
async fn bounded_write_refusal_blocks_opaque_retry_but_keeps_reads_and_verification() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "before\n").unwrap();
    project(dir.path());
    let command = if cfg!(windows) {
        "[IO.File]::WriteAllText('a.txt', '')"
    } else {
        ": > a.txt"
    };
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "one",
                "edit_file",
                json!({"path":"a.txt", "old_text":"before", "new_text":"after"}),
            )
            .tool_call(
                "two",
                "edit_file",
                json!({"path":"a.txt", "old_text":"after", "new_text":"wider"}),
            )
            .tool_call("opaque", "run_shell", json!({"command":command}))
            .tool_call(
                "background",
                "run_background",
                json!({"command":command, "grace_secs":0}),
            )
            .tool_call("inspect", "read_file", json!({"path":"a.txt"}))
            .tool_call("list", "run_background", json!({"action":"list"}))
            .text("checkpoint")
            .tool_call(
                "fresh",
                "run_shell",
                json!({"command":"git status --short"}),
            )
            .text("fresh unit"),
    );
    let mut agent = runtime(
        dir.path(),
        provider,
        Profile::Unrestricted,
        Some("git status --short".to_string()),
    );
    let (_, events) = turn(&mut agent).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "after\n",
        "a refused bounded edit must not be followed by destructive opaque retries"
    );
    for id in ["opaque", "background"] {
        assert!(events.iter().any(|e| matches!(e, RuntimeEvent::ToolFinished { id: call_id, is_error:true, output, .. } if call_id == id && output.contains("bounded write was refused"))));
    }
    assert!(events.iter().any(|e| matches!(e, RuntimeEvent::ToolFinished { id, is_error:false, output, .. } if id == "inspect" && output.contains("after"))));
    assert_eq!(agent.current_turn_verification(), Some(CheckStatus::Passed));
    assert!(events.iter().any(
        |e| matches!(e, RuntimeEvent::ToolFinished { id, is_error:false, .. } if id == "list")
    ));
    let (_, next_events) = turn(&mut agent).await;
    assert!(next_events.iter().any(
        |e| matches!(e, RuntimeEvent::ToolFinished { id, is_error:false, .. } if id == "fresh")
    ));
}

#[tokio::test]
async fn smaller_bounded_repair_restores_opaque_checks_after_bounds_refusal() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "before\n").unwrap();
    project(dir.path());
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "large",
                "edit_file",
                json!({"path":"a.txt", "old_text":"before", "new_text":"x".repeat(20_000)}),
            )
            .tool_call(
                "blocked",
                "run_shell",
                json!({"command":"git status --short"}),
            )
            .tool_call(
                "repair",
                "edit_file",
                json!({"path":"a.txt", "old_text":"before", "new_text":"after"}),
            )
            .tool_call(
                "check",
                "run_shell",
                json!({"command":"git status --short"}),
            )
            .text("checkpoint"),
    );
    let mut agent = runtime(
        dir.path(),
        provider,
        Profile::Unrestricted,
        Some("git status --short".to_string()),
    );
    let (_, events) = turn(&mut agent).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "after\n"
    );
    assert!(events.iter().any(|e| matches!(e, RuntimeEvent::ToolFinished { id, is_error:true, output, .. } if id == "blocked" && output.contains("bounded write was refused"))));
    for call in ["repair", "check"] {
        assert!(events.iter().any(
            |e| matches!(e, RuntimeEvent::ToolFinished { id, is_error:false, .. } if id == call)
        ));
    }
}

#[tokio::test]
async fn replacement_shape_guidance_does_not_block_opaque_checks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "before\n").unwrap();
    project(dir.path());
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "shape",
                "replace_in_file",
                json!({"path":"a.txt", "find":"before", "replace":"after"}),
            )
            .tool_call(
                "check",
                "run_shell",
                json!({"command":"git status --short"}),
            )
            .tool_call(
                "repair",
                "edit_file",
                json!({"path":"a.txt", "old_text":"before", "new_text":"after"}),
            )
            .text("checkpoint"),
    );
    let mut agent = runtime(
        dir.path(),
        provider,
        Profile::Unrestricted,
        Some("git status --short".to_string()),
    );
    let (_, events) = turn(&mut agent).await;
    assert!(events.iter().any(|e| matches!(e, RuntimeEvent::ToolFinished { id, is_error:true, output, .. } if id == "shape" && output.contains("bounded exact"))));
    for call in ["check", "repair"] {
        assert!(events.iter().any(
            |e| matches!(e, RuntimeEvent::ToolFinished { id, is_error:false, .. } if id == call)
        ));
    }
}

#[tokio::test]
async fn failed_bounded_repair_does_not_restore_opaque_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "before\n").unwrap();
    project(dir.path());
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "large",
                "edit_file",
                json!({"path":"a.txt", "old_text":"before", "new_text":"x".repeat(20_000)}),
            )
            .tool_call(
                "bad_repair",
                "edit_file",
                json!({"path":"a.txt", "old_text":"absent", "new_text":"after"}),
            )
            .tool_call(
                "opaque",
                "run_shell",
                json!({"command":"git status --short"}),
            )
            .text("blocked"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::Unrestricted, None);
    let (_, events) = turn(&mut agent).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "before\n"
    );
    assert!(events.iter().any(
        |e| matches!(e, RuntimeEvent::ToolFinished { id, is_error:true, .. } if id == "bad_repair")
    ));
    assert!(events.iter().any(|e| matches!(e, RuntimeEvent::ToolFinished { id, is_error:true, output, .. } if id == "opaque" && output.contains("bounded write was refused"))));
}

#[tokio::test]
async fn long_line_output_is_retained_and_projection_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("large.txt"), "α".repeat(10_000)).unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "longline",
                "read_file",
                json!({"path":"large.txt", "start_line":1, "end_line":1}),
            )
            .text("page too large in bytes"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::Bypass, None);
    let (_, events) = turn(&mut agent).await;
    let output = events
        .iter()
        .find_map(|e| match e {
            RuntimeEvent::ToolFinished { id, output, .. } if id == "longline" => Some(output),
            _ => None,
        })
        .unwrap();
    assert!(output.len() < 4_500);
    assert!(output.contains("retained under id longline"));
    assert!(output.contains("output truncated"));
}

#[tokio::test]
async fn unavailable_immediate_verification_cannot_finalize_a_changed_unit() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "write",
                "write_file",
                json!({"path":"a.txt", "content":"small"}),
            )
            .text("done"),
    );
    // An unavailable verifier cannot become a successful signal.
    let verify = "granularity-unavailable-verifier";
    let mut agent = runtime(
        dir.path(),
        provider,
        Profile::Bypass,
        Some(verify.to_string()),
    );
    let (reason, events) = turn(&mut agent).await;
    assert!(dir.path().join("a.txt").exists());
    assert_eq!(reason, StopReason::NoProgress);
    assert!(matches!(
        agent.current_turn_verification(),
        Some(CheckStatus::Denied | CheckStatus::Errored)
    ));
    assert!(events
        .iter()
        .any(|e| matches!(e, RuntimeEvent::Warning(text) if text.contains("remains unverified"))));
}

#[tokio::test]
async fn oversized_plan_is_visible_but_cannot_replace_saved_work() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("brief.md"), BRIEF).unwrap();
    let brief = Brief::parse(BRIEF).unwrap();
    let profile = WorkProfile::resolve(
        ContextCapacity {
            used: 0,
            limit: 8_000,
            provenance: localpilot_harness::granularity::ContextProvenance::CallerSupplied,
        },
        Reliability::Unknown,
        &GranularityConfig::default(),
    );
    let oversized = PLAN.replace("scope: 1, 1, 1, 4", "scope: 3, 5, 4, 800");
    let draft = draft_plan_with_profile(
        &FakeProvider::new().text(&oversized),
        "any-model-name",
        &brief,
        BriefRevision::of(&brief).as_str(),
        "repository",
        Some(profile),
    )
    .await
    .unwrap();
    assert!(matches!(
        persist_approved_plan(dir.path(), &draft, &brief, None),
        Err(PlanApproval::NotSatisfied(_))
    ));
    assert!(!dir.path().join("PROGRESS.md").exists());
    assert_eq!(draft.progress.steps[1].depends, Some(vec![1]));
    assert_eq!(draft.progress.steps[0].covers, Some(vec![1]));
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn project(root: &Path) {
    project_with_plan(root, PLAN);
}

fn project_with_plan(root: &Path, plan: &str) {
    std::fs::write(root.join("brief.md"), BRIEF).unwrap();
    let mut progress = Progress::parse(plan).unwrap();
    progress.bind_to_brief(BriefRevision::of(&Brief::parse(BRIEF).unwrap()).as_str());
    std::fs::write(root.join("PROGRESS.md"), progress.render()).unwrap();
    git(root, &["init"]);
    // Fixtures must not execute a developer's personal hook implementation.
    git(
        root,
        &["config", "core.hooksPath", ".git/fixture-empty-hooks"],
    );
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "initial"]);
}

#[tokio::test]
async fn bounded_checkpoint_is_durable_and_next_step_resumes_fresh() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    for (number, path) in [(1, "first.txt"), (2, "second.txt")] {
        let provider = Arc::new(
            FakeProvider::new()
                .tool_call(
                    "write",
                    "write_file",
                    json!({"path":path,"content":"small"}),
                )
                .text("checkpoint"),
        );
        let mut agent = runtime(dir.path(), provider, Profile::Bypass, None);
        let outcome = resume_one_step(
            &mut agent,
            dir.path(),
            &RuleEngine::with_baseline(&Default::default()),
            None,
            &[],
            3,
        )
        .await
        .unwrap();
        assert!(outcome.committed, "{:?}", outcome.blocked_reason);
        assert_eq!(outcome.step_number, number);
        let progress =
            Progress::parse(&std::fs::read_to_string(dir.path().join("PROGRESS.md")).unwrap())
                .unwrap();
        assert!(progress.steps[number - 1].done);
        assert!(!progress.steps[number - 1].sessions.is_empty());
        if number == 1 {
            assert!(!dir.path().join("second.txt").exists());
            assert_eq!(progress.next_incomplete().unwrap().number, 2);
        }
    }
}

#[tokio::test]
async fn opaque_shell_giant_diff_cannot_commit_or_claim_completion() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let command = if cfg!(windows) {
        "[IO.File]::WriteAllText('first.txt', ('x' * 20000))"
    } else {
        "printf '%020000d' 0 > first.txt"
    };
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("opaque", "run_shell", json!({"command":command}))
            .text("done"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::Unrestricted, None);
    let outcome = resume_one_step(
        &mut agent,
        dir.path(),
        &RuleEngine::with_baseline(&Default::default()),
        None,
        &[],
        1,
    )
    .await
    .unwrap();
    assert!(!outcome.committed);
    assert!(
        dir.path().join("first.txt").exists(),
        "oversized work is retained for review"
    );
    let progress =
        Progress::parse(&std::fs::read_to_string(dir.path().join("PROGRESS.md")).unwrap()).unwrap();
    assert!(!progress.steps[0].done);
}

#[tokio::test]
async fn immediate_verification_is_durably_recorded_without_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "write",
                "write_file",
                json!({"path":"first.txt","content":"small"}),
            )
            .text("done")
            .text("next turn without another mutation"),
    );
    let mut agent = runtime(
        dir.path(),
        provider,
        Profile::Bypass,
        Some("git status --short".to_string()),
    );
    assert_eq!(turn(&mut agent).await.0, StopReason::Done);
    assert_eq!(agent.current_turn_verification(), Some(CheckStatus::Passed));
    let events = agent.store().read_events(agent.session_id()).unwrap();
    assert!(events.iter().any(|e| matches!(&e.kind, localpilot_store::SessionEventKind::CheckRan { status, .. } if status == "passed")));
    assert_eq!(
        agent.work_profile().unwrap().reliability,
        Reliability::Unknown,
        "one passing check cannot establish strong capability"
    );
    assert_eq!(turn(&mut agent).await.0, StopReason::Done);
    assert_eq!(
        agent.current_turn_verification(),
        None,
        "a later unchecked turn cannot inherit a passing signal"
    );
}

#[tokio::test]
async fn a_denied_legacy_check_is_observed_separately_from_done() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let provider = Arc::new(FakeProvider::new().text("done"));
    let mut agent = runtime(
        dir.path(),
        provider,
        Profile::ReadOnly,
        Some("git add first.txt".to_string()),
    );
    agent.set_verify_before_done(true, None);
    assert_eq!(turn(&mut agent).await.0, StopReason::Done);
    assert_eq!(agent.current_turn_verification(), Some(CheckStatus::Denied));
}

#[tokio::test]
async fn malformed_input_and_stream_recovery_cannot_upgrade_capability() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("malformed", "read_file", json!({"path":42}))
            .tool_call(
                "valid",
                "write_file",
                json!({"path":"small.txt","content":"small"}),
            )
            .text("bounded checkpoint"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::Bypass, None);
    turn(&mut agent).await;
    assert_eq!(
        agent.work_profile().unwrap().reliability,
        Reliability::Malformed
    );
    agent.start_new_session();
    assert_eq!(
        agent.work_profile().unwrap().reliability,
        Reliability::Unknown
    );

    let provider = Arc::new(
        FakeProvider::new()
            .script(vec![
                Ok(ModelEvent::TextDelta("incomplete reply".to_string())),
                Err(ProviderError::StreamTruncated {
                    detail: "offline truncated stream".to_string(),
                }),
            ])
            .text("recovered"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::Bypass, None);
    assert_eq!(turn(&mut agent).await.0, StopReason::Done);
    assert_eq!(
        agent.work_profile().unwrap().reliability,
        Reliability::Unknown
    );
    assert_eq!(agent.work_profile().unwrap().max_regions, 1);
}

#[tokio::test]
async fn compaction_does_not_excuse_an_oversized_active_patch() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "oversized",
                "write_file",
                json!({"path":"huge.txt","content":"x".repeat(20_000)}),
            )
            .text("must split"),
    );
    let seed = (0..4)
        .flat_map(|_| {
            [
                localpilot_core::Message::text(
                    localpilot_core::Role::User,
                    "old context ".repeat(3_000),
                ),
                localpilot_core::Message::text(
                    localpilot_core::Role::Assistant,
                    "previous complete answer",
                ),
            ]
        })
        .collect();
    let mut agent = runtime_with_seed(dir.path(), provider, Profile::Bypass, None, seed);
    let (_, events) = turn(&mut agent).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RuntimeEvent::Compacted { .. })),
        "the shared compaction path ran"
    );
    assert!(!dir.path().join("huge.txt").exists());
    assert!(events.iter().any(
        |e| matches!(e, RuntimeEvent::ToolFinished { id, is_error:true, .. } if id == "oversized")
    ));
    assert_eq!(agent.work_profile().unwrap().max_changed_lines, 80);
}

#[tokio::test]
async fn an_opaque_commit_cannot_hide_work_from_the_harness_gate() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let command = if cfg!(windows) {
        "[IO.File]::WriteAllText('first.txt', ('x' * 20000)); git add -- first.txt; git commit -m premature"
    } else {
        "printf '%020000d' 0 > first.txt; git add -- first.txt; git commit -m premature"
    };
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("opaque", "run_shell", json!({"command":command}))
            .text("done"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::Unrestricted, None);
    let outcome = resume_one_step(
        &mut agent,
        dir.path(),
        &RuleEngine::with_baseline(&Default::default()),
        None,
        &[],
        3,
    )
    .await
    .unwrap();
    assert!(!outcome.committed);
    assert!(dir.path().join("first.txt").exists());
    let events = agent.store().read_events(agent.session_id()).unwrap();
    assert!(events.iter().any(|e| matches!(&e.kind, localpilot_store::SessionEventKind::TurnEnded { detail: Some(detail), .. } if detail.contains("committed before harness verification"))));
    let progress =
        Progress::parse(&std::fs::read_to_string(dir.path().join("PROGRESS.md")).unwrap()).unwrap();
    assert!(!progress.steps[0].done);
}

#[tokio::test]
async fn contract_refusal_does_not_spend_a_mutation_that_never_dispatched() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "before").unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "unread",
                "edit_file",
                json!({"path":"a.txt", "old_text":"before", "new_text":"after"}),
            )
            .tool_call(
                "read",
                "read_file",
                json!({"path":"a.txt", "start_line":1, "end_line":1}),
            )
            .tool_call(
                "repair",
                "edit_file",
                json!({"path":"a.txt", "old_text":"before", "new_text":"after"}),
            )
            .text("checkpoint"),
    );
    let mut agent = SessionRuntime::new(
        provider,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            enforce_prior_read: true,
            granularity: Some(GranularityConfig::default()),
            trusted: true,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let (_, events) = turn(&mut agent).await;
    assert!(events.iter().any(
        |e| matches!(e, RuntimeEvent::ToolFinished { id, is_error:true, .. } if id == "unread")
    ));
    assert!(events.iter().any(
        |e| matches!(e, RuntimeEvent::ToolFinished { id, is_error:false, .. } if id == "repair")
    ));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "after"
    );
}

#[tokio::test]
async fn unicode_and_space_paths_share_the_same_checkpoint_limits() {
    let dir = tempfile::tempdir().unwrap();
    let name = "résumé file.txt";
    project_with_plan(dir.path(), &PLAN.replace("first.txt", name));
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "write",
                "write_file",
                json!({"path":name,"content":"small"}),
            )
            .text("checkpoint"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::Bypass, None);
    let outcome = resume_one_step(
        &mut agent,
        dir.path(),
        &RuleEngine::with_baseline(&Default::default()),
        None,
        &[],
        3,
    )
    .await
    .unwrap();
    assert!(outcome.committed, "{:?}", outcome.blocked_reason);
    assert_eq!(
        std::fs::read_to_string(dir.path().join(name)).unwrap(),
        "small"
    );
}

#[tokio::test]
async fn refused_mutation_cannot_complete_an_empty_harness_step() {
    for profile in [Profile::Bypass, Profile::ReadOnly] {
        let dir = tempfile::tempdir().unwrap();
        project(dir.path());
        let provider = Arc::new(
            FakeProvider::new()
                .tool_call(
                    "oversized",
                    "write_file",
                    json!({"path":"first.txt", "content":"x".repeat(20_000)}),
                )
                .text("done"),
        );
        let mut agent = runtime(dir.path(), provider, profile, None);
        let outcome = resume_one_step(
            &mut agent,
            dir.path(),
            &RuleEngine::with_baseline(&Default::default()),
            None,
            &[],
            3,
        )
        .await
        .unwrap();
        assert!(!outcome.committed);
        assert!(!dir.path().join("first.txt").exists());
        let progress =
            Progress::parse(&std::fs::read_to_string(dir.path().join("PROGRESS.md")).unwrap())
                .unwrap();
        assert!(!progress.steps[0].done);
        let events = agent.store().read_events(agent.session_id()).unwrap();
        assert!(events.iter().any(|e| matches!(
            &e.kind,
            localpilot_store::SessionEventKind::TurnEnded { detail: Some(detail), .. }
                if detail.contains("requested mutation was refused")
        )));
    }
}

#[tokio::test]
async fn read_only_step_scope_does_not_leak_into_a_new_session() {
    let dir = tempfile::tempdir().unwrap();
    project_with_plan(
        dir.path(),
        &PLAN.replace("scope: 1, 1, 1, 4", "scope: 0, 0, 1, 0"),
    );
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "refused",
                "write_file",
                json!({"path":"first.txt", "content":"small"}),
            )
            .text("done")
            .tool_call(
                "fresh",
                "write_file",
                json!({"path":"first.txt", "content":"small"}),
            )
            .text("checkpoint"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::Bypass, None);
    let outcome = resume_one_step(
        &mut agent,
        dir.path(),
        &RuleEngine::with_baseline(&Default::default()),
        None,
        &[],
        3,
    )
    .await
    .unwrap();
    assert!(!outcome.committed);
    assert!(!dir.path().join("first.txt").exists());
    agent.start_new_session();
    let (_, events) = turn(&mut agent).await;
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolFinished { id, is_error: false, .. } if id == "fresh"
    )));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("first.txt")).unwrap(),
        "small"
    );
}

#[tokio::test]
async fn changed_unit_without_a_verifier_stops_with_a_durable_next_action() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "write",
                "write_file",
                json!({"path":"small.txt", "content":"small"}),
            )
            .text("done"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::Bypass, None);
    agent.set_work_unit_verification(&localpilot_harness::Verification::Command(
        "rustc --version".to_string(),
    ));
    agent.start_new_session();
    assert_eq!(turn(&mut agent).await.0, StopReason::NoProgress);
    assert!(dir.path().join("small.txt").exists());
    let events = agent.store().read_events(agent.session_id()).unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        localpilot_store::SessionEventKind::TurnEnded { detail: Some(detail), .. }
            if detail.contains("no applicable verification target")
    )));
}

#[tokio::test]
async fn failed_changed_unit_verification_narrows_reliability() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "write",
                "write_file",
                json!({"path":"small.txt", "content":"small"}),
            )
            .text("done")
            .text("done")
            .text("done")
            .text("done"),
    );
    let mut agent = runtime(
        dir.path(),
        provider,
        Profile::Bypass,
        Some("rustc --invalid-bounded-fixture-option".to_string()),
    );
    assert_eq!(turn(&mut agent).await.0, StopReason::NoProgress);
    assert_eq!(agent.work_profile().unwrap().reliability, Reliability::Weak);
    assert_eq!(agent.current_turn_verification(), Some(CheckStatus::Failed));
    assert!(dir.path().join("small.txt").exists());
    let events = agent.store().read_events(agent.session_id()).unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        localpilot_store::SessionEventKind::CheckRan { status, .. }
            if status == "failed"
    )));
}

fn binary_and_artifact_command() -> &'static str {
    if cfg!(windows) {
        "[IO.File]::WriteAllBytes('asset.bin', [byte[]](0,255,7)); [IO.Directory]::CreateDirectory('__pycache__'); [IO.File]::WriteAllBytes('__pycache__/fixture.pyc', [byte[]](0,255))"
    } else {
        "printf '\\000\\377\\007' > asset.bin; mkdir -p __pycache__; printf '\\000\\377' > __pycache__/fixture.pyc"
    }
}

fn binary_check(root: &Path, produce_artifact: bool) -> String {
    let artifact = if produce_artifact {
        "\nPath('__pycache__').mkdir(exist_ok=True)\nPath('__pycache__/from_check.pyc').write_bytes(bytes([0, 255]))\n"
    } else {
        ""
    };
    std::fs::write(root.join("check.py"), format!(
        "from pathlib import Path\nassert Path('asset.bin').read_bytes() == bytes([0, 255, 7])\n{artifact}"
    )).unwrap();
    git(root, &["add", "check.py"]);
    git(root, &["commit", "-m", "fixture verifier"]);
    format!(
        "{} -B check.py",
        if cfg!(windows) { "python" } else { "python3" }
    )
}

#[tokio::test]
async fn binary_and_generated_artifact_reach_explicit_verification_and_durable_evidence() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let verify = binary_check(dir.path(), false);
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "binary",
                "run_shell",
                json!({"command": binary_and_artifact_command()}),
            )
            .text("done"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::Unrestricted, Some(verify));
    let (stop, notices) = turn(&mut agent).await;
    assert_eq!(stop, StopReason::Done);
    assert_eq!(agent.current_turn_verification(), Some(CheckStatus::Passed));
    assert!(notices
        .iter()
        .any(|event| matches!(event, RuntimeEvent::Warning(detail)
        if detail.contains("__pycache__/fixture.pyc") && detail.contains("old + new bytes"))));
    let durable = agent.store().read_events(agent.session_id()).unwrap();
    assert!(durable.iter().any(|event| matches!(&event.kind,
        localpilot_store::SessionEventKind::WorkUnitInspected { ignored_generated, binary_changes, binary_bytes: 3, blocked: None }
        if ignored_generated == &["__pycache__/fixture.pyc"] && binary_changes.len() == 1
            && binary_changes[0].path == "asset.bin"
            && binary_changes[0].after.as_ref().unwrap().sha256.len() == 64)));
}

#[tokio::test]
async fn generated_only_opaque_invocation_still_requires_verification() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let command = if cfg!(windows) {
        "[IO.Directory]::CreateDirectory('__pycache__'); [IO.File]::WriteAllBytes('__pycache__/fixture.pyc', [byte[]](0,255))"
    } else {
        "mkdir -p __pycache__; printf '\\000\\377' > __pycache__/fixture.pyc"
    };
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("artifact", "run_shell", json!({"command":command}))
            .text("done"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::Unrestricted, None);
    assert_eq!(turn(&mut agent).await.0, StopReason::NoProgress);
    let durable = agent.store().read_events(agent.session_id()).unwrap();
    assert!(durable.iter().any(|event| matches!(&event.kind,
        localpilot_store::SessionEventKind::WorkUnitInspected { ignored_generated, binary_changes, .. }
        if ignored_generated == &["__pycache__/fixture.pyc"] && binary_changes.is_empty())));
    assert!(durable.iter().any(|event| matches!(&event.kind,
        localpilot_store::SessionEventKind::TurnEnded { detail: Some(detail), .. }
        if detail.contains("no applicable verification target"))));
}

#[tokio::test]
async fn checkpoint_reinspection_excludes_artifacts_from_the_passing_check_and_commit() {
    let dir = tempfile::tempdir().unwrap();
    // A real binary task, with an explicit content check rather than an exemption.
    let plan = PLAN
        .replace("Create first.txt", "Create asset.bin")
        .replace(
            "verify: none - prose fixture",
            &format!(
                "verify: {} -B check.py",
                if cfg!(windows) { "python" } else { "python3" }
            ),
        );
    project_with_plan(dir.path(), &plan);
    let verify = binary_check(dir.path(), true);
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "binary",
                "run_shell",
                json!({"command":binary_and_artifact_command()}),
            )
            .text("done"),
    );
    let mut agent = runtime(dir.path(), provider, Profile::Unrestricted, Some(verify));
    let outcome = resume_one_step(
        &mut agent,
        dir.path(),
        &RuleEngine::with_baseline(&Default::default()),
        None,
        &[],
        3,
    )
    .await
    .unwrap();
    assert!(outcome.committed, "{:?}", outcome.blocked_reason);
    assert_eq!(agent.current_turn_verification(), Some(CheckStatus::Passed));
    assert_eq!(
        std::fs::read(dir.path().join("asset.bin")).unwrap(),
        [0, 255, 7]
    );
    assert!(dir.path().join("__pycache__/from_check.pyc").exists());
    let tracked = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", "HEAD"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let tracked = String::from_utf8(tracked.stdout).unwrap();
    assert!(tracked.lines().any(|path| path == "asset.bin"));
    assert!(!tracked.contains("__pycache__"));
    let durable = agent.store().read_events(agent.session_id()).unwrap();
    assert!(durable.iter().any(|event| matches!(&event.kind,
        localpilot_store::SessionEventKind::WorkUnitInspected { ignored_generated, binary_changes, .. }
        if ignored_generated.contains(&"__pycache__/from_check.pyc".to_string()) && binary_changes.len() == 1)));
}
