//! A headless unit checkpoint needs an unchanged workspace through both
//! checks, runnable unit checks, and leaves a failed full check outstanding.
#![allow(clippy::unwrap_used)]

use std::{path::Path, process::Command, sync::Arc};

use localpilot_config::GranularityConfig;
use localpilot_harness::{CheckStatus, SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::{
    FakeProvider, ModelEvent, ModelEventStream, ModelProvider, ModelRequest, ProviderDeclaration,
    ProviderError,
};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::{SessionEventKind, Store};
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn python() -> bool {
    Command::new("python")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap()
        .success());
}

/// A repository whose verifier is `python <verifier>`; the verifier script
/// is committed with the project.
fn repository(verifier_body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("money.py"), "x = 1\n").unwrap();
    std::fs::write(root.join("verify.py"), verifier_body).unwrap();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Fixture"]);
    git(root, &["config", "user.email", "fixture@example.invalid"]);
    git(root, &["config", "core.autocrlf", "false"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "fixture"]);
    dir
}

fn runtime(
    root: &Path,
    provider: FakeProvider,
    profile: Profile,
    approver: ScriptedApprover,
    verify_before_done: bool,
) -> SessionRuntime {
    SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(profile, Vec::new()),
        Box::new(approver),
        Store::ephemeral(),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            granularity: Some(GranularityConfig {
                headless_units: 2,
                ..GranularityConfig::default()
            }),
            verify_before_done,
            verify_command: Some("python verify.py".to_string()),
            ..SessionConfig::default()
        },
        Vec::new(),
    )
}

/// Edit x, then a y edit the one-region unit refuses, then plain answers.
fn walled_then_answers(answers: usize) -> FakeProvider {
    stubborn(
        &[edit("x", "x = 1", "x = 2"), edit("y", "y = 1", "y = 2")],
        answers,
    )
}

async fn units(runtime: &mut SessionRuntime) -> StopReason {
    let (events, _rx) = broadcast::channel(1024);
    runtime
        .run_turn_in_units("Change x to 2.", &events, &CancellationToken::new())
        .await
}

const FAILS: &str = "import sys\nsys.exit(1)\n";

#[tokio::test]
async fn rename_admission_requires_read_authority_and_an_attested_clean_base() {
    let before = "def old_name(x):\n    return x\n\n\nvalue = old_name(1)\n";
    for scenario in ["authorized", "no-read-tool", "dirty"] {
        let dir = repository(FAILS);
        std::fs::write(dir.path().join("money.py"), before).unwrap();
        git(dir.path(), &["add", "money.py"]);
        git(dir.path(), &["commit", "-qm", "rename fixture"]);
        if scenario == "dirty" {
            std::fs::write(dir.path().join("unrelated.txt"), "preexisting").unwrap();
        }
        let provider = FakeProvider::new()
            .tool_call("read", "read_file", json!({"path":"money.py"}))
            .tool_call(
                "whole",
                "write_file",
                json!({
                    "path":"money.py", "content":before.replace("old_name", "new_name")
                }),
            )
            .text("done");
        let registry = ToolRegistry::with_builtins();
        let registry = if scenario == "no-read-tool" {
            registry.narrowed(&["write_file".to_owned()])
        } else {
            registry
        };
        let mut runtime = SessionRuntime::new(
            Arc::new(provider),
            registry,
            PermissionEngine::new(Profile::Bypass, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::ephemeral(),
            Workspace::new(dir.path()).unwrap(),
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig {
                granularity: Some(GranularityConfig {
                    headless_rename_units: 2,
                    ..GranularityConfig::default()
                }),
                ..SessionConfig::default()
            },
            Vec::new(),
        );
        let (events, mut receiver) = broadcast::channel(1024);
        let stop = runtime
            .run_turn_in_units(
                "Rename old_name to new_name everywhere.",
                &events,
                &CancellationToken::new(),
            )
            .await;
        assert_ne!(stop, StopReason::Done);
        let mut guided = false;
        while let Ok(event) = receiver.try_recv() {
            if let localpilot_harness::RuntimeEvent::ToolFinished { output, .. } = event {
                guided |= output.contains("Nothing was written");
            }
        }
        assert_eq!(guided, scenario == "authorized", "{scenario}");
        if scenario == "authorized" {
            assert_eq!(
                std::fs::read_to_string(dir.path().join("money.py")).unwrap(),
                before
            );
        }
    }
}

#[tokio::test]
async fn selective_evidence_requires_read_authority_and_stable_checks() {
    if !python() {
        return;
    }
    for (with_read, moving_verifier, expected) in
        [(true, false, 1), (false, false, 0), (true, true, 0)]
    {
        let verifier = if moving_verifier {
            "import sys\nopen('other.py','w').write('x = 1\\n')\nsys.exit(1)\n"
        } else {
            FAILS
        };
        let dir = repository(verifier);
        std::fs::write(
            dir.path().join("money.py"),
            "def old_name():\n    return 1\n",
        )
        .unwrap();
        git(dir.path(), &["add", "money.py"]);
        git(dir.path(), &["commit", "-qm", "function fixture"]);
        let provider = FakeProvider::new()
            .tool_call(
                "rename",
                "edit_file",
                json!({"path":"money.py","old_text":"def old_name","new_text":"def new_name"}),
            )
            .tool_call(
                "next",
                "edit_file",
                json!({"path":"money.py","old_text":"return 1","new_text":"return 2"}),
            )
            .text("done")
            .text("done")
            .text("done")
            .text("done")
            .text("done");
        let registry = ToolRegistry::with_builtins();
        let registry = if with_read {
            registry
        } else {
            registry.narrowed(&["edit_file".to_owned()])
        };
        let mut runtime = SessionRuntime::new(
            Arc::new(provider),
            registry,
            PermissionEngine::new(Profile::Unrestricted, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::ephemeral(),
            Workspace::new(dir.path()).unwrap(),
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig {
                granularity: Some(GranularityConfig {
                    headless_rename_units: 2,
                    ..GranularityConfig::default()
                }),
                verify_before_done: true,
                verify_command: Some("python verify.py".into()),
                ..SessionConfig::default()
            },
            Vec::new(),
        );
        let (events, _rx) = broadcast::channel(1024);
        let stop = runtime
            .run_turn_in_units(
                "Rename old_name to new_name everywhere.",
                &events,
                &CancellationToken::new(),
            )
            .await;
        assert_ne!(stop, StopReason::Done);
        let checkpoints=runtime.store().read_events(runtime.session_id()).unwrap().into_iter()
            .filter(|e| matches!(&e.kind,SessionEventKind::TurnEnded {stop,..} if stop=="UnitCheckpoint")).count();
        assert_eq!(
            checkpoints, expected,
            "read={with_read}, moved={moving_verifier}"
        );
    }
}

/// Statuses of the checks this session actually ran, in order.
fn check_statuses(runtime: &SessionRuntime) -> Vec<String> {
    runtime
        .store()
        .read_events(runtime.session_id())
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::CheckRan { status, .. } => Some(status),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_verifier_that_rewrites_the_unit_earns_no_checkpoint() {
    if !python() {
        return;
    }
    for verifier in [
        // Grows the changed file beyond the unit's bounds, syntax-valid.
        "import sys\nopen('money.py', 'w').write('x = 2\\n' + 'y = 0\\n' * 150)\nsys.exit(1)\n",
        // Writes another path.
        "import sys\nopen('other.py', 'w').write('z = 1\\n')\nsys.exit(1)\n",
        // Moves HEAD.
        "import subprocess, sys\nsubprocess.run(['git', 'commit', '-qam', 'by verifier'])\nsys.exit(1)\n",
    ] {
        let dir = two_site_repository(verifier);
        let mut runtime = runtime(
            dir.path(),
            walled_then_answers(6),
            Profile::Unrestricted,
            ScriptedApprover::always(),
            true,
        );
        let stop = units(&mut runtime).await;
        assert_eq!(stop, StopReason::NoProgress, "{verifier}");
        // Refused right after the full check moved the workspace: no unit
        // check ran and no second unit opened.
        assert_eq!(check_statuses(&runtime), ["failed"], "{verifier}");
    }
}

#[tokio::test]
async fn a_unit_check_that_cannot_run_stops_with_its_status() {
    if !python() {
        return;
    }
    let dir = two_site_repository(FAILS);
    // Approve the full check; deny the unit check that follows it.
    let approver = ScriptedApprover::new(vec![true, false]);
    let mut runtime = runtime(
        dir.path(),
        walled_then_answers(6),
        Profile::Default,
        approver,
        true,
    );
    let stop = units(&mut runtime).await;
    assert_eq!(stop, StopReason::NoProgress);
    assert_eq!(
        runtime.current_turn_verification(),
        Some(CheckStatus::Denied)
    );
    // The full check ran and failed; the unit check after it was denied.
    let statuses = check_statuses(&runtime);
    assert_eq!(statuses.len(), 2, "{statuses:?}");
    assert!(statuses[0].contains("failed"), "{statuses:?}");
    assert!(statuses[1].contains("denied"), "{statuses:?}");
}

#[tokio::test]
async fn the_failed_full_check_stays_outstanding_after_an_unfinished_task() {
    if !python() {
        return;
    }
    for verify_before_done in [true, false] {
        let dir = two_site_repository(FAILS);
        // Unit 1 checkpoints at its refused edit; unit 2 answers without
        // edits until it stops; a later turn answers without touching anything.
        let mut runtime = runtime(
            dir.path(),
            walled_then_answers(12),
            Profile::Unrestricted,
            ScriptedApprover::always(),
            verify_before_done,
        );
        let stop = units(&mut runtime).await;
        assert_ne!(stop, StopReason::Done);
        let saved = format!(
            "{:?}",
            runtime.store().read_events(runtime.session_id()).unwrap()
        );
        assert!(saved.contains("A checkpoint does not mean"), "{saved}");
        let statuses = check_statuses(&runtime);
        assert!(
            statuses.len() >= 2 && statuses[1].contains("passed"),
            "{statuses:?}"
        );
        let ran = statuses.len();
        let (events, _rx) = broadcast::channel(1024);
        let resumed = runtime
            .run_turn("Is anything left?", &events, &CancellationToken::new())
            .await;
        assert_ne!(
            resumed,
            StopReason::Done,
            "verify_before_done={verify_before_done}: unresolved work finished without a passing full check"
        );
        // The unchanged resumed turn still ran the full check.
        let statuses = check_statuses(&runtime);
        assert!(statuses.len() > ran, "{statuses:?}");
        assert!(
            statuses[ran..].iter().all(|s| s.contains("failed")),
            "{statuses:?}"
        );
    }
}

#[tokio::test]
async fn a_failed_check_before_any_refusal_returns_its_diagnostics_first() {
    if !python() {
        return;
    }
    // No write was refused when the model answered, so the failed check goes
    // back to the same unit as ordinary feedback instead of a checkpoint. The
    // conservative unit refuses the repair, which then ends the unit; the
    // repair lands in the next unit.
    let dir = repository(
        "import sys
sys.exit(0 if 'x = 2' in open('money.py').read() else 1)
",
    );
    let repair = json!({"path": "money.py", "old_text": "x = 3", "new_text": "x = 2"});
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "wrong",
                "edit_file",
                json!({"path": "money.py", "old_text": "x = 1", "new_text": "x = 3"}),
            )
            .text("done")
            .tool_call("refused-repair", "edit_file", repair.clone())
            .tool_call("repair", "edit_file", repair)
            .text("done")
            .text("done"),
    );
    let mut runtime = SessionRuntime::new(
        provider.clone(),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Unrestricted, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::ephemeral(),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            granularity: Some(GranularityConfig {
                headless_units: 2,
                ..GranularityConfig::default()
            }),
            verify_before_done: true,
            verify_command: Some("python verify.py".to_string()),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    assert_eq!(units(&mut runtime).await, StopReason::Done);
    // Full failed (feedback), full failed + unit check passed at the refused
    // repair (checkpoint), then full passed.
    assert_eq!(
        check_statuses(&runtime),
        ["failed", "failed", "passed", "passed"]
    );
    assert_eq!(unit_checkpoints(&runtime), 1);
    // The request after the first answer carried the failure, not a unit note.
    let third = format!("{:?}", provider.requests()[2]);
    assert!(!third.contains("of at most"), "{third}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("money.py")).unwrap(),
        "x = 2
"
    );
}

/// Turns this session ended as unit checkpoints.
fn unit_checkpoints(runtime: &SessionRuntime) -> usize {
    runtime
        .store()
        .read_events(runtime.session_id())
        .unwrap()
        .into_iter()
        .filter(|e| {
            matches!(&e.kind, SessionEventKind::TurnEnded { stop, .. } if stop == "UnitCheckpoint")
        })
        .count()
}

/// A two-region file: `x` near the top, `y` far below, so editing both
/// exceeds a one-region unit.
fn two_site_repository(verifier: &str) -> tempfile::TempDir {
    let dir = repository(verifier);
    let filler = "\n".repeat(12);
    std::fs::write(
        dir.path().join("money.py"),
        format!("x = 1\n{filler}y = 1\n"),
    )
    .unwrap();
    git(dir.path(), &["commit", "-qam", "two sites"]);
    dir
}

fn edit(id: &str, old: &str, new: &str) -> (String, serde_json::Value) {
    (
        id.to_string(),
        json!({"path": "money.py", "old_text": old, "new_text": new}),
    )
}

/// A model that never stops on its own: it keeps sending its next edit.
fn stubborn(edits: &[(String, serde_json::Value)], tail: usize) -> FakeProvider {
    let mut provider = FakeProvider::new();
    for (id, input) in edits {
        provider = provider.tool_call(id, "edit_file", input.clone());
    }
    for _ in 0..tail {
        provider = provider.text("done");
    }
    provider
}

const BOTH: &str = "import sys\ntext = open('money.py').read()\nsys.exit(0 if 'x = 2' in text and 'y = 2' in text else 1)\n";
const PASSES: &str = "import sys\nsys.exit(0)\n";

#[tokio::test]
async fn the_host_ends_a_unit_at_a_refused_edit_and_the_model_finishes_later() {
    if !python() {
        return;
    }
    let dir = two_site_repository(BOTH);
    // x (allowed), y (refused: second region) -> host ends unit 1; unit 2:
    // y (allowed), then the model's own final answer.
    let edits = [
        edit("x", "x = 1", "x = 2"),
        edit("y1", "y = 1", "y = 2"),
        edit("y2", "y = 1", "y = 2"),
    ];
    let mut runtime = runtime(
        dir.path(),
        stubborn(&edits, 3),
        Profile::Unrestricted,
        ScriptedApprover::always(),
        true,
    );
    assert_eq!(units(&mut runtime).await, StopReason::Done);
    // Full failed + unit check passed (checkpoint), then full passed.
    let statuses = check_statuses(&runtime);
    assert_eq!(statuses, ["failed", "passed", "passed"], "{statuses:?}");
}

#[tokio::test]
async fn a_passing_full_check_at_a_refused_edit_opens_no_further_unit() {
    if !python() {
        return;
    }
    // The checker passes from the start. The host-ended unit settles without
    // a new unit; the repeated y edit is refused in the same unit without
    // another host end or check; only the final answer runs the check again.
    let dir = two_site_repository(PASSES);
    let edits = [
        edit("x", "x = 1", "x = 2"),
        edit("y1", "y = 1", "y = 2"),
        edit("y2", "y = 1", "y = 2"),
    ];
    let provider = Arc::new(stubborn(&edits, 3));
    let mut runtime = SessionRuntime::new(
        provider.clone(),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Unrestricted, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::ephemeral(),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            granularity: Some(GranularityConfig {
                headless_units: 2,
                ..GranularityConfig::default()
            }),
            verify_before_done: true,
            verify_command: Some("python verify.py".to_string()),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let (events, _rx) = broadcast::channel(1024);
    assert_eq!(
        runtime
            .run_turn_in_units(
                "Change both x and y to 2.",
                &events,
                &CancellationToken::new()
            )
            .await,
        StopReason::Done
    );
    // One check settles the host-ended unit; one checks the final answer.
    assert_eq!(check_statuses(&runtime), ["passed", "passed"]);
    assert_eq!(unit_checkpoints(&runtime), 0);
    // As without units, the refused y stays unapplied.
    let text = std::fs::read_to_string(dir.path().join("money.py")).unwrap();
    assert!(text.contains("x = 2") && text.contains("y = 1"), "{text}");
    let all = format!("{:?}", provider.requests());
    assert!(!all.contains("of at most"), "a unit note was sent: {all}");
    // Only the first refusal ended the unit; the second keeps the default step.
    let seen = format!("{:?}", provider.requests().last().unwrap());
    assert_eq!(
        seen.matches("this call was not executed; the harness")
            .count(),
        1,
        "{seen}"
    );
    assert!(seen.contains("checkpoint before continuing"), "{seen}");
}

#[tokio::test]
async fn selective_units_open_no_unit_after_a_passing_check() {
    if !python() {
        return;
    }
    let dir = repository(PASSES);
    std::fs::write(
        dir.path().join("money.py"),
        "def old_name():\n    return 1\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("app.py"), "from money import old_name\n").unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "caller fixture"]);
    let caller = json!({"path":"app.py","old_text":"from money import old_name","new_text":"from money import new_name"});
    let provider = FakeProvider::new()
        .tool_call(
            "declaration",
            "edit_file",
            json!({"path":"money.py","old_text":"def old_name","new_text":"def new_name"}),
        )
        .tool_call("refused-caller", "edit_file", caller.clone())
        .tool_call("needed-caller", "edit_file", caller)
        .text("done");
    let mut runtime = SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Unrestricted, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::ephemeral(),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            granularity: Some(GranularityConfig {
                headless_rename_units: 2,
                ..GranularityConfig::default()
            }),
            verify_before_done: true,
            verify_command: Some("python verify.py".to_string()),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let (events, _rx) = broadcast::channel(1024);
    assert_eq!(
        runtime
            .run_turn_in_units(
                "Rename old_name to new_name in both files.",
                &events,
                &CancellationToken::new()
            )
            .await,
        StopReason::Done
    );
    // This check passes with only the declaration renamed. Without units the
    // caller edit stays refused, so it stays refused here too.
    assert_eq!(check_statuses(&runtime), ["passed", "passed"]);
    assert_eq!(unit_checkpoints(&runtime), 0);
    assert!(std::fs::read_to_string(dir.path().join("money.py"))
        .unwrap()
        .contains("def new_name"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("app.py")).unwrap(),
        "from money import old_name\n"
    );
}

#[tokio::test]
async fn the_provider_receives_current_checkpoint_review_under_context_pressure() {
    if !python() {
        return;
    }
    let dir = two_site_repository(BOTH);
    let provider = FakeProvider::new()
        .tool_call(
            "x",
            "edit_file",
            json!({"path":"money.py","old_text":"x = 1","new_text":"x = 2"}),
        )
        .script(vec![
            Ok(ModelEvent::TextDelta("old unit reasoning ".repeat(12_000))),
            Ok(ModelEvent::ToolCall {
                id: "refused-y".into(),
                name: "edit_file".into(),
                input_json: json!({"path":"money.py","old_text":"y = 1","new_text":"y = 2"}),
                provider_metadata: None,
            }),
            Ok(ModelEvent::Done),
        ])
        .tool_call(
            "y",
            "edit_file",
            json!({"path":"money.py","old_text":"y = 1","new_text":"y = 2"}),
        )
        .text("Both x and y are 2.");
    let observed = Arc::new(provider);
    let mut runtime = SessionRuntime::new(
        observed.clone(),
        ToolRegistry::with_builtins().narrowed(&["edit_file".to_string(), "read_file".to_string()]),
        PermissionEngine::new(Profile::Unrestricted, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::ephemeral(),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            context_token_limit: 16_384,
            granularity: Some(GranularityConfig {
                headless_units: 2,
                ..GranularityConfig::default()
            }),
            verify_before_done: true,
            verify_command: Some("python verify.py".to_string()),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let task = "Change x and y to 2.";
    let (events, _rx) = broadcast::channel(1024);
    assert_eq!(
        runtime
            .run_turn_in_units(task, &events, &CancellationToken::new())
            .await,
        StopReason::Done
    );
    let requests = observed.requests();
    let request = requests.last().unwrap();
    let task_position = request
        .messages
        .iter()
        .rposition(|message| {
            message.role == localpilot_core::Role::User
                && !message.is_synthetic()
                && format!("{message:?}").contains(task)
        })
        .unwrap();
    let review_position = request.messages.iter().rposition(|message| {
        message.metadata.synthetic.as_deref() == Some("unit checkpoint")
            && format!("{message:?}").contains("It is still pending")
    });
    assert!(
        review_position.is_some(),
        "the actual provider request lost the checkpoint review"
    );
    let review_position = review_position.unwrap();
    let review = format!("{:?}", request.messages[review_position]);
    assert!(
        review.contains("Unexecuted request"),
        "the handoff must carry the refused request, not reference discarded history: {review}"
    );
    assert!(
        review.contains("money.py") && review.contains("y = 1") && review.contains("y = 2"),
        "{review}"
    );
    assert!(
        review_position > task_position,
        "review must follow the unchanged task"
    );
    let saved = runtime.store().read_events(runtime.session_id()).unwrap();
    assert!(
        saved
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::Compacted { .. })),
        "context pressure must exercise compaction"
    );
    let text = std::fs::read_to_string(dir.path().join("money.py")).unwrap();
    assert!(text.contains("x = 2") && text.contains("y = 2"));
}

#[tokio::test]
async fn calls_queued_after_a_refused_edit_do_not_run() {
    if !python() {
        return;
    }
    let dir = two_site_repository(BOTH);
    let outside = tempfile::tempdir().unwrap();
    let marker = outside.path().join("ran.txt");
    let provider = FakeProvider::new()
        .tool_call(
            "x",
            "edit_file",
            json!({"path": "money.py", "old_text": "x = 1", "new_text": "x = 2"}),
        )
        .script(vec![
            Ok(ModelEvent::ToolCall {
                id: "y".into(),
                name: "edit_file".into(),
                input_json: json!({"path": "money.py", "old_text": "y = 1", "new_text": "y = 2"}),
                provider_metadata: None,
            }),
            Ok(ModelEvent::ToolCall {
                id: "touch".into(),
                name: "write_file".into(),
                input_json: json!({"path": marker.display().to_string(), "content": "ran"}),
                provider_metadata: None,
            }),
            Ok(ModelEvent::Done),
        ])
        .text("done")
        .text("done")
        .text("done");
    let provider = Arc::new(provider);
    let mut runtime = SessionRuntime::new(
        provider.clone(),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Unrestricted, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::ephemeral(),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            granularity: Some(GranularityConfig {
                headless_units: 2,
                ..GranularityConfig::default()
            }),
            verify_before_done: true,
            verify_command: Some("python verify.py".to_string()),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let _ = units(&mut runtime).await;
    assert!(!marker.exists(), "a queued call ran after the unit ended");
    // The queued call was answered as skipped, once, without dispatch.
    let seen = format!("{:?}", provider.requests().last().unwrap());
    assert_eq!(
        seen.matches("skipped: this unit ended at a refused edit")
            .count(),
        1,
        "{seen}"
    );
}

#[tokio::test]
async fn host_ended_units_respect_the_cap_and_the_off_setting() {
    if !python() {
        return;
    }
    for units_setting in [0u8, 1] {
        let dir = two_site_repository(BOTH);
        let edits = [
            edit("x", "x = 1", "x = 2"),
            edit("y1", "y = 1", "y = 2"),
            edit("y2", "y = 1", "y = 2"),
            edit("y3", "y = 1", "y = 2"),
        ];
        let mut runtime = SessionRuntime::new(
            Arc::new(stubborn(&edits, 4)),
            ToolRegistry::with_builtins(),
            PermissionEngine::new(Profile::Unrestricted, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::ephemeral(),
            Workspace::new(dir.path()).unwrap(),
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig {
                granularity: Some(GranularityConfig {
                    headless_units: units_setting,
                    ..GranularityConfig::default()
                }),
                verify_before_done: true,
                verify_command: Some("python verify.py".to_string()),
                ..SessionConfig::default()
            },
            Vec::new(),
        );
        let stop = units(&mut runtime).await;
        assert_ne!(stop, StopReason::Done, "units={units_setting}");
        assert_ne!(stop, StopReason::UnitCheckpoint, "units={units_setting}");
        let text = std::fs::read_to_string(dir.path().join("money.py")).unwrap();
        assert!(!text.contains("y = 2"), "units={units_setting}: {text}");
    }
}

/// Delegates to a scripted provider and fires `hook` while serving request
/// number `at` (1-based), i.e. while the batch it returns is streaming.
struct Signalling {
    inner: FakeProvider,
    at: usize,
    served: std::sync::atomic::AtomicUsize,
    hook: Box<dyn Fn() + Send + Sync>,
}

#[async_trait::async_trait]
impl ModelProvider for Signalling {
    fn declaration(&self) -> &ProviderDeclaration {
        self.inner.declaration()
    }

    async fn stream(&self, request: ModelRequest) -> Result<ModelEventStream, ProviderError> {
        let n = self
            .served
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if n == self.at {
            (self.hook)();
        }
        self.inner.stream(request).await
    }
}

#[derive(Clone, Copy, Debug)]
enum Signal {
    Cancel,
    Quiesce,
    Steer,
    UrgentSteer,
}

#[tokio::test]
async fn signals_at_a_host_unit_boundary_are_handled_before_any_settlement() {
    if !python() {
        return;
    }
    for signal in [
        Signal::Cancel,
        Signal::Quiesce,
        Signal::Steer,
        Signal::UrgentSteer,
    ] {
        let dir = two_site_repository(BOTH);
        // Request 1: allowed edit of x. Request 2: refused edit of y plus a
        // queued read; the signal fires while request 2 streams.
        let inner = FakeProvider::new()
            .tool_call("x", "edit_file", json!({"path": "money.py", "old_text": "x = 1", "new_text": "x = 2"}))
            .script(vec![
                Ok(ModelEvent::ToolCall {
                    id: "y".into(),
                    name: "edit_file".into(),
                    input_json: json!({"path": "money.py", "old_text": "y = 1", "new_text": "y = 2"}),
                    provider_metadata: None,
                }),
                Ok(ModelEvent::ToolCall {
                    id: "read".into(),
                    name: "read_file".into(),
                    input_json: json!({"path": "money.py"}),
                    provider_metadata: None,
                }),
                Ok(ModelEvent::Done),
            ])
            .text("noted")
            .text("noted")
            .text("noted")
            .text("noted");
        let cancel = CancellationToken::new();
        let slot: Arc<
            std::sync::Mutex<
                Option<(
                    localpilot_harness::SteerQueue,
                    localpilot_harness::QuiesceSignal,
                )>,
            >,
        > = Arc::new(std::sync::Mutex::new(None));
        let hook_slot = slot.clone();
        let hook_cancel = cancel.clone();
        let provider = Arc::new(Signalling {
            inner,
            at: 2,
            served: std::sync::atomic::AtomicUsize::new(0),
            hook: Box::new(move || {
                let guard = hook_slot.lock().unwrap();
                let (steer, quiesce) = guard.as_ref().unwrap();
                match signal {
                    Signal::Cancel => hook_cancel.cancel(),
                    Signal::Quiesce => quiesce.request(),
                    Signal::Steer => steer.push("steering: stop and report"),
                    Signal::UrgentSteer => {
                        steer.push_interrupt(localpilot_harness::SoftInterrupt {
                            content: "urgent: stop and report".into(),
                            source: localpilot_harness::SoftInterruptSource::User,
                            urgent: true,
                        })
                    }
                }
            }),
        });
        let mut runtime = SessionRuntime::new(
            provider.clone(),
            ToolRegistry::with_builtins(),
            PermissionEngine::new(Profile::Unrestricted, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::ephemeral(),
            Workspace::new(dir.path()).unwrap(),
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig {
                granularity: Some(GranularityConfig {
                    headless_units: 2,
                    ..GranularityConfig::default()
                }),
                verify_before_done: true,
                verify_command: Some("python verify.py".to_string()),
                ..SessionConfig::default()
            },
            Vec::new(),
        );
        *slot.lock().unwrap() = Some((runtime.steer_queue(), runtime.quiesce_signal()));
        let (events, _rx) = broadcast::channel(1024);
        let stop = runtime
            .run_turn_in_units("Change x and y to 2.", &events, &cancel)
            .await;
        let served = provider.served.load(std::sync::atomic::Ordering::SeqCst);
        match signal {
            Signal::Cancel => assert_eq!(stop, StopReason::Cancelled, "{signal:?}"),
            Signal::Quiesce => assert_eq!(stop, StopReason::Quiesced, "{signal:?}"),
            Signal::Steer | Signal::UrgentSteer => {
                // The steering reached the model before any host settlement.
                assert!(served >= 3, "{signal:?}: served {served}");
            }
        }
        if matches!(signal, Signal::Cancel | Signal::Quiesce) {
            // Nothing ran after the signal: no check, no further request.
            assert!(check_statuses(&runtime).is_empty(), "{signal:?}");
            assert_eq!(served, 2, "{signal:?}");
        } else {
            // The host boundary did not outrank the steer: the very next
            // request carries the steer and no unit was settled before it.
            let third = format!("{:?}", provider.inner.requests()[2]);
            assert!(third.contains("stop and report"), "{signal:?}");
            assert!(!third.contains("of at most"), "{signal:?}: settled first");
        }
        // Every tool_use sent so far was answered exactly once.
        if let Some(last) = provider.inner.requests().last() {
            let seen = format!("{last:?}");
            assert_eq!(
                seen.matches("ToolUse(").count(),
                seen.matches("ToolResult(").count(),
                "{signal:?}"
            );
        }
    }
}

/// The tool-result text of the first work-envelope refusal the model saw.
fn first_refusal(provider: &FakeProvider) -> String {
    let seen = format!("{:?}", provider.requests());
    let start = seen.find("work envelope");
    assert!(start.is_some(), "no work-envelope refusal was seen");
    let rest = &seen[start.unwrap_or_default()..];
    rest[..rest.find('"').unwrap_or(rest.len())].to_string()
}

async fn refusal_with_units(dir: &Path, units_setting: u8, provider: FakeProvider) -> String {
    let provider = Arc::new(provider);
    let mut runtime = SessionRuntime::new(
        provider.clone(),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Unrestricted, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::ephemeral(),
        Workspace::new(dir).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            granularity: Some(GranularityConfig {
                headless_units: units_setting,
                ..GranularityConfig::default()
            }),
            verify_before_done: true,
            verify_command: Some("python verify.py".to_string()),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let _ = units(&mut runtime).await;
    first_refusal(&provider)
}

#[tokio::test]
async fn a_refusal_that_ends_no_unit_keeps_the_default_text() {
    if !python() {
        return;
    }
    // A regex replacement after a real change: the host does not end the
    // unit for it, so the model must not be told its unit is complete.
    let script = || {
        FakeProvider::new()
            .tool_call(
                "x",
                "edit_file",
                json!({"path": "money.py", "old_text": "x = 1", "new_text": "x = 2"}),
            )
            .tool_call(
                "regex",
                "replace_in_file",
                json!({"path": "money.py", "find": "y = 1", "replace": "y = 2"}),
            )
            .text("done")
            .text("done")
            .text("done")
    };
    let off = refusal_with_units(two_site_repository(BOTH).path(), 0, script()).await;
    let on = refusal_with_units(two_site_repository(BOTH).path(), 2, script()).await;
    assert!(!on.contains("this call was not executed"), "{on}");
    assert_eq!(on, off);
}

#[tokio::test]
async fn a_refusal_that_ends_the_unit_says_so() {
    if !python() {
        return;
    }
    let edits = [edit("x", "x = 1", "x = 2"), edit("y", "y = 1", "y = 2")];
    let on = refusal_with_units(two_site_repository(BOTH).path(), 2, stubborn(&edits, 3)).await;
    assert!(on.contains("this call was not executed"), "{on}");
    let off = refusal_with_units(two_site_repository(BOTH).path(), 0, stubborn(&edits, 3)).await;
    assert!(!off.contains("this call was not executed"), "{off}");
}

/// Every request the model saw, rendered for byte comparison.
async fn requests_with_units(units_setting: u8, verifier: &str, provider: FakeProvider) -> String {
    let dir = two_site_repository(verifier);
    let provider = Arc::new(provider);
    let mut runtime = SessionRuntime::new(
        provider.clone(),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Unrestricted, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::ephemeral(),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            granularity: Some(GranularityConfig {
                headless_units: units_setting,
                ..GranularityConfig::default()
            }),
            verify_before_done: true,
            verify_command: Some("python verify.py".to_string()),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let _ = units(&mut runtime).await;
    let rendered = format!("{:?}", provider.requests());
    // Per-run temporary names are not behaviour: mask the workspace root
    // and every per-session scratch name before comparing.
    let mut masked = rendered;
    // Render both ordinary and extended Windows roots at every nesting level
    // before replacing: a shorter spelling can otherwise leave an escaped
    // extended-path prefix whose spelling differs from the system note.
    // The tool resolves Windows short user names (RUNNER~1) to the canonical
    // long path; the system note can still contain the original temp root.
    let roots = [
        dir.path().to_path_buf(),
        std::fs::canonicalize(dir.path()).unwrap(),
    ];
    let mut bases = Vec::new();
    for root in roots {
        let root = root.display().to_string();
        bases.push(root.clone());
        bases.push(format!(r"\\?\{root}"));
    }
    let mut spellings = Vec::new();
    for base in bases {
        let mut spelling = base;
        for _ in 0..6 {
            spellings.push(spelling.clone());
            spelling = spelling.replace('\\', "\\\\");
        }
    }
    spellings.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for spelling in spellings {
        masked = masked.replace(&spelling, "<root>");
    }
    mask_after(&masked, "localpilot-", &['\\', '"', ' '])
}

/// Replace each run of characters after `marker` (up to any `stops`) with `*`.
fn mask_after(text: &str, marker: &str, stops: &[char]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(marker) {
        out.push_str(&rest[..at + marker.len()]);
        rest = &rest[at + marker.len()..];
        let end = rest.find(|c| stops.contains(&c)).unwrap_or(rest.len());
        out.push('*');
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

#[tokio::test]
async fn units_that_never_engage_leave_every_request_unchanged() {
    if !python() {
        return;
    }
    // A read, a refused regex replacement, one bounded edit and a final
    // answer the full check accepts: no unit ends, so the setting must not
    // change a single byte the model sees.
    let script = || {
        FakeProvider::new()
            .tool_call("read", "read_file", json!({"path": "money.py"}))
            .tool_call(
                "regex",
                "replace_in_file",
                json!({"path": "money.py", "find": "x = 1", "replace": "x = 2"}),
            )
            .tool_call(
                "huge",
                "edit_file",
                json!({"path": "money.py", "old_text": "x = 1", "new_text": "x = 2
".repeat(400)}),
            )
            .tool_call(
                "x",
                "edit_file",
                json!({"path": "money.py", "old_text": "x = 1", "new_text": "x = 2"}),
            )
            .text("done")
            .text("done")
    };
    let off = requests_with_units(0, PASSES, script()).await;
    let on = requests_with_units(8, PASSES, script()).await;
    assert!(
        off.contains("work envelope"),
        "the regex refusal was exercised"
    );
    assert_eq!(on, off);
}

#[tokio::test]
async fn a_whole_unit_session_runs_no_repository_helper() {
    if !python() {
        return;
    }
    let dir = two_site_repository(BOTH);
    let outside = tempfile::tempdir().unwrap();
    let filter_marker = outside.path().join("filter.txt");
    let monitor_marker = outside.path().join("monitor.txt");
    let script = |name: &str, marker: &Path, body: &str| {
        let path = outside.path().join(name);
        std::fs::write(
            &path,
            format!(
                "import sys\nopen(r'{}', 'a').write('ran')\n{body}\n",
                marker.display()
            ),
        )
        .unwrap();
        format!("python {}", path.display()).replace('\\', "/")
    };
    let clean = script(
        "clean.py",
        &filter_marker,
        "sys.stdout.write(sys.stdin.read())",
    );
    let monitor = script("monitor.py", &monitor_marker, "");
    std::fs::write(dir.path().join(".gitattributes"), "*.py filter=probe\n").unwrap();
    git(dir.path(), &["add", ".gitattributes"]);
    git(dir.path(), &["commit", "-qm", "attributes"]);
    git(dir.path(), &["config", "filter.probe.clean", &clean]);
    git(dir.path(), &["config", "filter.probe.required", "true"]);
    git(dir.path(), &["config", "core.fsmonitor", &monitor]);
    let _ = std::fs::remove_file(&filter_marker);
    let _ = std::fs::remove_file(&monitor_marker);
    // Edit x, a refused y edit ends the unit (checkpoint), then y in unit 2.
    let edits = [
        edit("x", "x = 1", "x = 2"),
        edit("y1", "y = 1", "y = 2"),
        edit("y2", "y = 1", "y = 2"),
    ];
    let mut runtime = runtime(
        dir.path(),
        stubborn(&edits, 3),
        Profile::Unrestricted,
        ScriptedApprover::always(),
        true,
    );
    let _ = units(&mut runtime).await;
    assert!(
        check_statuses(&runtime).iter().any(|s| s == "passed"),
        "the session reached its checks: {:?}",
        check_statuses(&runtime)
    );
    assert!(!filter_marker.exists(), "a clean filter ran");
    assert!(!monitor_marker.exists(), "the filesystem monitor ran");
}
