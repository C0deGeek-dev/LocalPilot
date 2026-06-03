//! Tool registry, permission, and builtin-tool behaviour tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use unshackled_core::{ToolCall, ToolResult, ToolUseId};
use unshackled_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use unshackled_tools::{ToolContext, ToolRegistry};

fn workspace_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, contents) in files {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, ws)
}

fn ctx(ws: &Workspace, interactivity: Interactivity, trusted: bool) -> ToolContext<'_> {
    ToolContext {
        workspace: ws,
        interactivity,
        trusted,
    }
}

async fn dispatch(
    registry: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
    ctx: &ToolContext<'_>,
    engine: &PermissionEngine,
    approver: &ScriptedApprover,
) -> ToolResult {
    let call = ToolCall::new(ToolUseId::from("c1"), name, input);
    registry.dispatch(&call, ctx, engine, approver).await
}

fn default_engine() -> PermissionEngine {
    PermissionEngine::new(Profile::Default, Vec::new())
}

fn bypass_engine() -> PermissionEngine {
    PermissionEngine::new(Profile::Bypass, Vec::new())
}

fn init_git_repo(dir: &std::path::Path) {
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@example.invalid"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir)
        .output()
        .unwrap();
}

#[tokio::test]
async fn unknown_tool_returns_an_error_result_not_a_panic() {
    let (_dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    let result = dispatch(
        &registry,
        "no_such_tool",
        json!({}),
        &ctx(&ws, Interactivity::Interactive, true),
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(result.is_error);
    assert!(result.output.contains("unknown tool"));
}

#[test]
fn every_builtin_generates_a_schema() {
    let registry = ToolRegistry::with_builtins();
    assert_eq!(registry.names().len(), 15);
    for (name, schema) in registry.schemas() {
        assert!(schema.is_object(), "{name} produced a non-object schema");
    }
}

#[test]
fn tool_schemas_are_stable() {
    let registry = ToolRegistry::with_builtins();
    let schemas = registry.schemas();
    insta::assert_snapshot!(serde_json::to_string_pretty(&schemas).unwrap());
}

#[tokio::test]
async fn read_file_inside_workspace_is_allowed_and_outside_is_denied() {
    let (_dir, ws) = workspace_with(&[("src/lib.rs", "fn main() {}\n")]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);

    let inside = dispatch(
        &registry,
        "read_file",
        json!({ "path": "src/lib.rs" }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!inside.is_error);
    assert!(inside.output.contains("status: success"));
    assert!(inside.output.contains("fn main"));

    let outside_dir = tempfile::tempdir().unwrap();
    let outside_file = outside_dir.path().join("secret.txt");
    std::fs::write(&outside_file, "x").unwrap();
    let outside = dispatch(
        &registry,
        "read_file",
        json!({ "path": outside_file.to_str().unwrap() }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(outside.is_error);
    assert!(outside.output.contains("status: error"));
    assert!(outside.output.contains("permission denied"));
}

#[tokio::test]
async fn write_file_in_workspace_and_denied_outside() {
    let (dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);

    let ok = dispatch(
        &registry,
        "write_file",
        json!({ "path": "out.txt", "content": "hello" }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!ok.is_error, "{}", ok.output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "hello"
    );

    let outside_dir = tempfile::tempdir().unwrap();
    let outside_path = outside_dir.path().join("escape.txt");
    let outside = dispatch(
        &registry,
        "write_file",
        json!({ "path": outside_path.to_str().unwrap(), "content": "x" }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(outside.is_error);
    assert!(!outside_path.exists());
}

#[tokio::test]
async fn untrusted_overwrite_prompts_for_approval() {
    let (_dir, ws) = workspace_with(&[("f.txt", "old")]);
    let registry = ToolRegistry::with_builtins();
    // Untrusted workspace, interactive: a write asks; a denying approver blocks it.
    let denied = dispatch(
        &registry,
        "write_file",
        json!({ "path": "f.txt", "content": "new" }),
        &ctx(&ws, Interactivity::Interactive, false),
        &default_engine(),
        &ScriptedApprover::new(vec![false]),
    )
    .await;
    assert!(denied.is_error);
    assert!(denied.output.contains("permission denied"));
}

#[tokio::test]
async fn edit_file_exact_match_and_rejects_ambiguous() {
    let (dir, ws) = workspace_with(&[("u.txt", "alpha once"), ("d.txt", "dup dup")]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);

    let ok = dispatch(
        &registry,
        "edit_file",
        json!({ "path": "u.txt", "old_text": "alpha", "new_text": "beta" }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!ok.is_error, "{}", ok.output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("u.txt")).unwrap(),
        "beta once"
    );

    let ambiguous = dispatch(
        &registry,
        "edit_file",
        json!({ "path": "d.txt", "old_text": "dup", "new_text": "x" }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(ambiguous.is_error);
    assert!(ambiguous.output.contains("ambiguous"));
}

#[tokio::test]
async fn multi_edit_applies_all_edits_atomically() {
    let (dir, ws) = workspace_with(&[("u.txt", "alpha beta gamma")]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);

    let ok = dispatch(
        &registry,
        "multi_edit",
        json!({
            "path": "u.txt",
            "edits": [
                { "old_text": "alpha", "new_text": "one" },
                { "old_text": "gamma", "new_text": "three" }
            ]
        }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!ok.is_error, "{}", ok.output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("u.txt")).unwrap(),
        "one beta three"
    );

    let failed = dispatch(
        &registry,
        "multi_edit",
        json!({
            "path": "u.txt",
            "edits": [
                { "old_text": "one", "new_text": "1" },
                { "old_text": "missing", "new_text": "x" }
            ]
        }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(failed.is_error);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("u.txt")).unwrap(),
        "one beta three"
    );
}

#[tokio::test]
async fn list_files_respects_ignore_files() {
    let (_dir, ws) = workspace_with(&[
        ("keep.rs", ""),
        ("target/ignored.rs", ""),
        (".gitignore", "target/\n"),
    ]);
    let registry = ToolRegistry::with_builtins();
    let result = dispatch(
        &registry,
        "list_files",
        json!({}),
        &ctx(&ws, Interactivity::NonInteractive, true),
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!result.is_error);
    assert!(result.output.contains("keep.rs"));
    assert!(
        !result.output.contains("ignored.rs"),
        "ignore file not respected: {}",
        result.output
    );
}

#[tokio::test]
async fn find_files_matches_filename_patterns() {
    let (_dir, ws) = workspace_with(&[
        ("src/main.rs", ""),
        ("src/lib.rs", ""),
        ("README.md", ""),
        ("target/ignored.rs", ""),
        (".gitignore", "target/\n"),
    ]);
    let registry = ToolRegistry::with_builtins();
    let result = dispatch(
        &registry,
        "find_files",
        json!({ "pattern": "*.rs" }),
        &ctx(&ws, Interactivity::NonInteractive, true),
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!result.is_error);
    let output = result.output.replace('\\', "/");
    assert!(output.contains("src/main.rs"));
    assert!(output.contains("src/lib.rs"));
    assert!(!result.output.contains("README.md"));
    assert!(!result.output.contains("ignored.rs"));
}

#[tokio::test]
async fn search_text_finds_matches_within_the_workspace() {
    let (_dir, ws) = workspace_with(&[("a.rs", "fn alpha() {}\n"), ("b.rs", "fn beta() {}\n")]);
    let registry = ToolRegistry::with_builtins();
    let result = dispatch(
        &registry,
        "search_text",
        json!({ "query": "alpha" }),
        &ctx(&ws, Interactivity::NonInteractive, true),
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!result.is_error);
    assert!(result.output.contains("a.rs:1"));
    assert!(!result.output.contains("b.rs"));
}

#[tokio::test]
async fn run_shell_allows_read_only_and_denies_destructive_non_interactive() {
    let (_dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);

    #[cfg(windows)]
    let (read_only, destructive) = (
        json!({ "program": "cmd", "args": ["/c", "echo", "hello"] }),
        json!({ "program": "cmd", "args": ["/c", "del", "x"] }),
    );
    #[cfg(not(windows))]
    let (read_only, destructive) = (
        json!({ "program": "echo", "args": ["hello"] }),
        json!({ "program": "rm", "args": ["-rf", "x"] }),
    );

    let allowed = dispatch(
        &registry,
        "run_shell",
        read_only,
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!allowed.is_error, "{}", allowed.output);
    assert!(allowed.output.contains("hello"));

    let denied = dispatch(
        &registry,
        "run_shell",
        destructive,
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(denied.is_error);
    assert!(denied.output.contains("permission denied"));
}

#[tokio::test]
async fn git_commit_rejects_a_secret_bearing_message() {
    let (_dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    // Bypass clears the permission gate so the tool's own secret check is what fires.
    let result = dispatch(
        &registry,
        "git_commit",
        json!({ "message": "add key sk-abcdefghijklmnopqrstuvwxyz0123" }),
        &ctx(&ws, Interactivity::NonInteractive, true),
        &bypass_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(result.is_error);
    assert!(result.output.contains("secret"));
}

#[tokio::test]
async fn git_diff_and_add_are_gated_by_command_class() {
    let (dir, ws) = workspace_with(&[("tracked.txt", "one\n")]);
    init_git_repo(dir.path());
    std::process::Command::new("git")
        .args(["add", "tracked.txt"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::fs::write(dir.path().join("tracked.txt"), "two\n").unwrap();

    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::Interactive, true);
    let diff = dispatch(
        &registry,
        "git_diff",
        json!({ "paths": ["tracked.txt"] }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!diff.is_error, "{}", diff.output);
    assert!(diff.output.contains("-one"));
    assert!(diff.output.contains("+two"));

    let add = dispatch(
        &registry,
        "git_add",
        json!({ "paths": ["tracked.txt"] }),
        &c,
        &default_engine(),
        &ScriptedApprover::new(vec![true]),
    )
    .await;
    assert!(!add.is_error, "{}", add.output);

    let restore = dispatch(
        &registry,
        "git_restore",
        json!({ "paths": ["tracked.txt"] }),
        &c,
        &default_engine(),
        &ScriptedApprover::new(vec![false]),
    )
    .await;
    assert!(restore.is_error);
    assert!(restore.output.contains("permission denied"));
}

#[tokio::test]
async fn bypass_still_redacts_output_and_keeps_the_workspace_boundary() {
    let secret = "sk-abcdefghijklmnopqrstuvwxyz0123";
    let (_dir, ws) = workspace_with(&[(".env", &format!("OPENAI_API_KEY={secret}"))]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);

    // Bypass allows reading the secret-like file without prompting...
    let read = dispatch(
        &registry,
        "read_file",
        json!({ "path": ".env" }),
        &c,
        &bypass_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!read.is_error, "{}", read.output);
    // ...but the output is still redacted.
    assert!(
        !read.output.contains(secret),
        "secret leaked: {}",
        read.output
    );
    assert!(read.output.contains("[REDACTED]"));

    // ...and the workspace boundary still holds under bypass.
    let outside_dir = tempfile::tempdir().unwrap();
    let outside_path = outside_dir.path().join("bypass-escape.txt");
    let escape = dispatch(
        &registry,
        "write_file",
        json!({ "path": outside_path.to_str().unwrap(), "content": "x" }),
        &c,
        &bypass_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(escape.is_error);
    assert!(escape.output.contains("permission denied"));
    assert!(!outside_path.exists());
}
