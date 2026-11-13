//! Recipe drafts from commands that succeeded in a session: which runs
//! qualify, what the draft says, and that nothing is written, run or
//! committed by asking for one.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_skills::read_recipes;
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// A dependency-free crate that builds offline in a moment.
fn project(dir: &Path) {
    let project = dir.join("project");
    write(
        &project,
        "Cargo.toml",
        "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    );
    write(&project, "src/lib.rs", "pub fn one() -> u8 { 1 }\n");
}

fn build() -> Value {
    json!({"program": "cargo", "args": ["build", "--offline", "--quiet", "--target-dir", "target"]})
}

async fn session(dir: &Path, calls: Vec<(&str, Value)>) -> SessionRuntime {
    let mut provider = FakeProvider::new();
    for (id, input) in calls {
        provider = provider.tool_call(id, "run_shell", input);
    }
    let provider = Arc::new(provider.text("done"));
    let mut runtime = SessionRuntime::new(
        provider as Arc<dyn localpilot_llm::ModelProvider>,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(&dir.join("store")),
        Workspace::new(&dir.join("project")).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    );
    let (events, _rx) = broadcast::channel::<RuntimeEvent>(256);
    runtime
        .run_turn("Run the commands.", &events, &CancellationToken::new())
        .await;
    runtime
}

fn tree(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            out.insert(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
            );
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    out
}

#[tokio::test]
async fn the_latest_successful_command_becomes_a_valid_reviewable_draft() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let runtime = session(dir.path(), vec![("build-1", build())]).await;
    let before = tree(&dir.path().join("project"));

    let draft = runtime.recipe_draft(None, None).unwrap();

    // It names the run it came from.
    assert!(draft.source.contains("build-1"), "{}", draft.source);
    assert!(
        draft.source.contains("cargo build --offline"),
        "{}",
        draft.source
    );
    // It is a recipe the reader accepts, with the directory the command ran in.
    let manifest = format!(
        "name = \"x\"\ndescription = \"d\"\nversion = \"1\"\n{}",
        draft.text
    );
    let recipes = read_recipes(&manifest);
    assert!(
        recipes.invalid.is_empty(),
        "{:?}\n{}",
        recipes.invalid,
        draft.text
    );
    let recipe = &recipes.recipes[0];
    assert_eq!(recipe.name, "cargo-build");
    assert_eq!(recipe.program, "cargo");
    assert_eq!(recipe.working_directory, ".");
    assert_eq!(recipe.component, ".");
    assert_eq!(recipe.cleanup, localpilot_skills::Cleanup::Nothing);
    // Validated against the manifest as it was when the command finished.
    assert!(recipe
        .validated_against
        .iter()
        .any(|(file, _)| file == "Cargo.toml"));
    assert_eq!(
        recipe.validation(&dir.path().join("project")),
        localpilot_skills::RecipeValidation::Current
    );
    // Asking for a draft wrote, ran and committed nothing.
    assert_eq!(tree(&dir.path().join("project")), before);
    assert!(!dir.path().join("project/.localpilot").exists());
}

#[tokio::test]
async fn read_only_failed_and_shell_syntax_commands_are_not_drafted_and_say_why() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let runtime = session(
        dir.path(),
        vec![
            ("probe-1", json!({"program": "cargo", "args": ["--version"]})),
            ("fail-1", json!({"program": "cargo", "args": ["build", "--offline", "--manifest-path", "missing/Cargo.toml"]})),
            ("chain-1", json!({"command": "cargo build --offline --quiet --target-dir target; cargo --version"})),
        ],
    )
    .await;

    assert!(
        runtime.recipe_candidates().is_empty(),
        "{:?}",
        runtime.recipe_candidates()
    );
    let reason = runtime.recipe_draft(None, None).unwrap_err();
    assert!(reason.contains("it only reads"), "{reason}");
    assert!(reason.contains("it did not exit 0"), "{reason}");
    assert!(reason.contains("shell syntax"), "{reason}");
}

#[tokio::test]
async fn a_run_can_be_picked_by_number_and_the_component_named() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let check = json!({"program": "cargo", "args": ["check", "--offline", "--quiet", "--target-dir", "target"]});
    let runtime = session(dir.path(), vec![("build-1", build()), ("check-1", check)]).await;

    let candidates = runtime.recipe_candidates();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].call, "build-1");

    let first = runtime.recipe_draft(Some(1), Some("crates/api")).unwrap();
    assert!(first.source.contains("build-1"));
    assert!(first.text.contains("name = \"cargo-build\""));
    // The component is named; the directory the command ran in is not.
    assert!(
        first
            .text
            .contains("component = \"crates/api\"\nworking_directory = \".\""),
        "{}",
        first.text
    );
    let latest = runtime.recipe_draft(None, None).unwrap();
    assert!(latest.text.contains("name = \"cargo-check\""));

    assert!(runtime
        .recipe_draft(Some(0), None)
        .unwrap_err()
        .contains("from 1"));
    assert!(runtime
        .recipe_draft(Some(3), None)
        .unwrap_err()
        .contains("there is no run 3; 2 can"));
}

#[tokio::test]
async fn a_file_changed_after_the_run_is_not_recorded_as_validated() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let runtime = session(dir.path(), vec![("build-1", build())]).await;
    write(
        &dir.path().join("project"),
        "Cargo.toml",
        "[package]\nname = \"p\"\nversion = \"0.2.0\"\nedition = \"2021\"\n\n[workspace]\n",
    );

    let draft = runtime.recipe_draft(None, None).unwrap();

    assert!(
        draft
            .notes
            .iter()
            .any(|n| n.contains("Cargo.toml changed after the run")),
        "{:?}",
        draft.notes
    );
    // The lock file did not change, so it stays recorded; the manifest does not.
    assert!(!draft.text.contains("\"Cargo.toml\" ="), "{}", draft.text);
    assert!(draft.text.contains("\"Cargo.lock\" ="), "{}", draft.text);
}

#[tokio::test]
async fn no_command_at_all_says_so() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let runtime = session(dir.path(), Vec::new()).await;

    assert_eq!(
        runtime.recipe_draft(None, None).unwrap_err(),
        "no shell command has run in this session"
    );
}
