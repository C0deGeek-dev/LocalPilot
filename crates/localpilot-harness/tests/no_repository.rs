//! Resuming a plan in a folder that is not a git repository stops before any
//! step starts, with an error that names the remedy, and creates nothing.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use localpilot_harness::{
    resume_one_step, Brief, BriefRevision, HarnessError, RuleEngine, SessionConfig, SessionRuntime,
};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;

const BRIEF: &str = "# Brief: no repository\n\n## Summary\n\nDo one thing.\n\n\
## Requirements\n\n- It is done\n\n## Constraints\n\n- Change only what is needed\n\n\
## Non-Goals\n\n- Anything unrelated\n\n## Acceptance Criteria\n\n- It holds\n";

#[test]
fn resume_in_a_folder_without_git_names_the_remedy_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("brief.md"), BRIEF).unwrap();
    let revision = BriefRevision::of(&Brief::parse(BRIEF).unwrap());
    std::fs::write(
        root.join("PROGRESS.md"),
        format!(
            "# Progress: no repository\nBranch: feature/none\nBrief: {revision}\n\n## Steps\n\n- [ ] 1. Do the thing\n"
        ),
    )
    .unwrap();

    let mut runtime = SessionRuntime::new(
        Arc::new(FakeProvider::new().text("done")),
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
    let rules = RuleEngine::with_baseline(&Default::default());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let error = rt
        .block_on(resume_one_step(&mut runtime, root, &rules, None, &[], 3))
        .unwrap_err();

    assert!(
        matches!(error, HarnessError::NotGitRepository),
        "got {error:?}"
    );
    let text = error.to_string();
    assert!(text.contains("not a git repository"), "{text}");
    assert!(text.contains("localpilot init --git"), "{text}");
    assert!(
        !root.join(".git").exists(),
        "the library never creates the repository"
    );
}
