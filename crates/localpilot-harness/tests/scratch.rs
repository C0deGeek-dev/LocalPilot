//! Session construction and lifecycle report and release owned scratch.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;

use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScratchRoot, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;

fn runtime(root: &std::path::Path, policy: ScratchRoot) -> SessionRuntime {
    let mut workspace = Workspace::new(root).unwrap();
    workspace.set_scratch_root(policy);
    SessionRuntime::new(
        Arc::new(FakeProvider::new()),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::new(Vec::new())),
        Store::open(root),
        workspace,
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    )
}

fn scratch(runtime: &SessionRuntime) -> PathBuf {
    let prompt = runtime.system_prompt_text();
    assert_eq!(prompt.matches("<session-scratch>").count(), 1);
    PathBuf::from(
        prompt
            .lines()
            .find_map(|line| line.strip_prefix("Private session scratch directory: "))
            .unwrap(),
    )
}

#[test]
fn sessions_and_prompt_replacement_keep_one_current_root_then_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let mut session = runtime(dir.path(), ScratchRoot::Parent(parent.path().to_path_buf()));
    let original = session.session_id();
    let first = scratch(&session);
    assert!(first.exists());
    std::fs::write(first.join("fixture"), "data").unwrap();
    session.replace_system_prompt("host guidance");
    session.append_system_prompt("additional guidance");
    assert_eq!(first, scratch(&session));
    session.start_new_session();
    assert!(!first.exists());
    let second = scratch(&session);
    assert_ne!(first, second);
    session.load_session(original).unwrap();
    assert!(!second.exists());
    let resumed = scratch(&session);
    session.fork_session(true).unwrap();
    assert!(!resumed.exists());
    let fork = scratch(&session);
    session.close();
    assert!(!fork.exists());
    assert!(!session.system_prompt_text().contains("<session-scratch>"));
    assert!(parent.path().exists());
    session.start_new_session();
    let last = scratch(&session);
    drop(session);
    assert!(!last.exists());
    assert!(parent.path().exists());
}

#[test]
fn disabled_or_failed_creation_never_reports_or_grants_a_root() {
    let dir = tempfile::tempdir().unwrap();
    for policy in [
        ScratchRoot::Disabled,
        ScratchRoot::Parent(dir.path().join("missing")),
    ] {
        let session = runtime(dir.path(), policy);
        assert!(!session.system_prompt_text().contains("<session-scratch>"));
    }
}
