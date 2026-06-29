//! Tool registry, permission, and builtin-tool behaviour tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use localpilot_core::{ToolCall, ToolResult, ToolUseId};
use localpilot_sandbox::{
    Effect, Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace,
};
use localpilot_tools::{
    Reversibility, Tool, ToolContext, ToolContract, ToolError, ToolOutput, ToolRegistry,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
        retention: None,
        processes: None,
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
    assert_eq!(registry.names().len(), 21);
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
async fn read_file_returns_a_placeholder_for_binary_content() {
    let (_dir, ws) = workspace_with(&[]);
    // GLB-style header: ASCII magic followed by raw NUL/control bytes — the
    // shape that previously leaked control characters into the model context.
    std::fs::write(
        ws.root().join("race.glb"),
        b"glTF\x02\x00\x00\x00\x10\x00\x00\x00rest",
    )
    .unwrap();
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);
    let result = dispatch(
        &registry,
        "read_file",
        json!({ "path": "race.glb" }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!result.is_error);
    assert!(result.output.contains("binary data"));
    // Raw NUL must never reach the model-visible output.
    assert!(!result.output.contains('\u{00}'));
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
async fn edit_file_matches_lf_old_text_against_a_crlf_file() {
    // The model emits multi-line `old_text` with `\n`, but the file on disk is
    // CRLF. The edit must still land (not fail "old_text was not found" and push
    // the model to rewrite the whole file), and the CRLF style must be preserved.
    let (dir, ws) = workspace_with(&[("win.txt", "line one\r\nline two\r\nline three\r\n")]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);

    let res = dispatch(
        &registry,
        "edit_file",
        json!({
            "path": "win.txt",
            "old_text": "line one\nline two\n",
            "new_text": "line one\nLINE TWO\n"
        }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!res.is_error, "{}", res.output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("win.txt")).unwrap(),
        "line one\r\nLINE TWO\r\nline three\r\n"
    );
}

#[tokio::test]
async fn append_file_creates_then_concatenates() {
    let (dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);

    // The first append to a missing path creates the file.
    let first = dispatch(
        &registry,
        "append_file",
        json!({ "path": "doc.md", "content": "# Part 1\n" }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!first.is_error, "{}", first.output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("doc.md")).unwrap(),
        "# Part 1\n"
    );

    // A second append concatenates after the first — the chunked-write path.
    let second = dispatch(
        &registry,
        "append_file",
        json!({ "path": "doc.md", "content": "# Part 2\n" }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!second.is_error, "{}", second.output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("doc.md")).unwrap(),
        "# Part 1\n# Part 2\n"
    );
}

#[tokio::test]
async fn append_file_is_not_idempotent() {
    let (dir, ws) = workspace_with(&[("log.txt", "start\n")]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);

    for _ in 0..2 {
        let r = dispatch(
            &registry,
            "append_file",
            json!({ "path": "log.txt", "content": "line\n" }),
            &c,
            &default_engine(),
            &ScriptedApprover::always(),
        )
        .await;
        assert!(!r.is_error, "{}", r.output);
    }
    // Each append adds another copy: re-running is not a no-op.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("log.txt")).unwrap(),
        "start\nline\nline\n"
    );
}

#[tokio::test]
async fn append_file_preserves_crlf_newline_style() {
    // A CRLF file stays CRLF when an LF-content append is normalized to match —
    // tier-1 parity (the same bytes on every platform).
    let (dir, ws) = workspace_with(&[("win.txt", "a\r\nb\r\n")]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);

    let r = dispatch(
        &registry,
        "append_file",
        json!({ "path": "win.txt", "content": "c\nd\n" }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("win.txt")).unwrap(),
        "a\r\nb\r\nc\r\nd\r\n"
    );
}

#[tokio::test]
async fn append_file_refuses_a_non_utf8_file() {
    let (dir, ws) = workspace_with(&[]);
    std::fs::write(dir.path().join("bin.dat"), [0xFF, 0xFE, 0x00, 0x01]).unwrap();
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);

    let r = dispatch(
        &registry,
        "append_file",
        json!({ "path": "bin.dat", "content": "text" }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(r.is_error);
    assert!(r.output.contains("not a UTF-8 text file"));
    // The binary file is untouched.
    assert_eq!(
        std::fs::read(dir.path().join("bin.dat")).unwrap(),
        [0xFF, 0xFE, 0x00, 0x01]
    );
}

#[tokio::test]
async fn append_file_approval_prompt_shows_the_path() {
    let (_dir, ws) = workspace_with(&[("existing.txt", "old\n")]);
    let registry = ToolRegistry::with_builtins();
    // Untrusted workspace so the project write asks instead of auto-allowing.
    let c = ctx(&ws, Interactivity::Interactive, false);
    let approver = RecordingApprover::new();

    let call = ToolCall::new(
        ToolUseId::from("c1"),
        "append_file",
        json!({ "path": "existing.txt", "content": "more\n" }),
    );
    let _ = registry
        .dispatch(&call, &c, &default_engine(), &approver)
        .await;

    let seen = approver.seen();
    assert!(
        !seen.is_empty(),
        "a project write must prompt when untrusted"
    );
    assert_eq!(seen[0].detail, "existing.txt");
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

    // A metachar-free shell command classifies by its leading program (`echo` →
    // read-only); an inline `cmd /c …` is opaque and gated (Unknown → denied
    // non-interactively), so it is the destructive case here.
    #[cfg(windows)]
    let (read_only, destructive) = (
        json!({ "command": "echo hello" }),
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
async fn run_shell_runs_relative_paths_against_the_workspace_not_a_fallback_dir() {
    // The de-verbatim cwd fix (01): a relative path in a model-issued shell
    // command must resolve against the workspace, never a fallback like
    // `C:\Windows` (the Windows verbatim-`\\?\`-cwd bug that ran every build/test
    // command outside the workspace). Prove it by reading a workspace marker file
    // by its *relative* name through the platform shell — which only succeeds when
    // the child's cwd is the workspace.
    let marker = "cwd-marker-7be3a1.txt";
    let body = "marker-body-in-workspace";
    let (_dir, ws) = workspace_with(&[(marker, body)]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);

    #[cfg(windows)]
    let command = format!("Get-Content {marker}");
    #[cfg(not(windows))]
    let command = format!("cat {marker}");

    // Bypass the classifier (an unknown shell verb is otherwise gated), as the
    // sibling `$PWD` test does; the cwd is what is under test, not the gate.
    let result = dispatch(
        &registry,
        "run_shell",
        json!({ "command": command }),
        &c,
        &bypass_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(
        !result.is_error,
        "a relative read failed — the shell did not run in the workspace: {}",
        result.output
    );
    assert!(
        result.output.contains(body),
        "a relative path resolved outside the workspace cwd: {}",
        result.output
    );
}

#[tokio::test]
async fn run_shell_accepts_simple_command_strings_and_builtin_reads() {
    let (_dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);
    // The shell runs in the de-verbatim spawn cwd (`process_dir`), so `$PWD`/`pwd`
    // report that form — not the verbatim containment `root()`.
    let cwd = ws.process_dir().display().to_string();

    let pwd = dispatch(
        &registry,
        "run_shell",
        json!({ "program": "pwd" }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!pwd.is_error, "{}", pwd.output);
    assert!(pwd.output.contains(&cwd));

    let echo_pwd = dispatch(
        &registry,
        "run_shell",
        json!({ "command": "echo $PWD" }),
        &c,
        &bypass_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!echo_pwd.is_error, "{}", echo_pwd.output);
    assert!(echo_pwd.output.contains(&cwd));

    let quoted = dispatch(
        &registry,
        "run_shell",
        json!({ "program": "echo \"hello world\"" }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!quoted.is_error, "{}", quoted.output);
    assert!(quoted.output.contains("hello world"));
}

#[tokio::test]
async fn run_shell_command_field_uses_the_platform_shell() {
    let (_dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);

    #[cfg(windows)]
    let command = "Write-Output hello | Select-String hello";
    #[cfg(not(windows))]
    let command = "printf hello | cat";

    let result = dispatch(
        &registry,
        "run_shell",
        json!({ "command": command }),
        &c,
        &bypass_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!result.is_error, "{}", result.output);
    assert!(result.output.contains("hello"));
}

#[tokio::test]
async fn run_shell_runs_and_chained_commands_on_an_and_capable_shell() {
    // The `&&` chain (02): on a `&&`-capable shell both commands run. Unix
    // `sh -lc` always supports `&&`; on Windows it works only when PowerShell 7+
    // (`pwsh`) is the selected shell — Windows PowerShell 5.1 lacks the operator,
    // so the fallback path is exercised and the chain is reported as a failure
    // rather than silently dropping the second command.
    let (_dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);

    // `&&` is a shell metachar, so the command classifies Unknown and is gated;
    // bypass clears the gate so the shell behaviour itself is what is tested.
    let result = dispatch(
        &registry,
        "run_shell",
        json!({ "command": "echo chain-a && echo chain-b" }),
        &c,
        &bypass_engine(),
        &ScriptedApprover::always(),
    )
    .await;

    #[cfg(windows)]
    let and_capable = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("pwsh.exe").is_file()))
        .unwrap_or(false);
    #[cfg(not(windows))]
    let and_capable = true;

    if and_capable {
        assert!(
            !result.is_error,
            "an &&-capable shell must run a chained command: {}",
            result.output
        );
        assert!(
            result.output.contains("chain-a") && result.output.contains("chain-b"),
            "both halves of the `&&` chain must run: {}",
            result.output
        );
    } else {
        // No PS7 on this host: the 5.1 fallback rejects `&&`. The contract we pin
        // is that it does not *pretend* the chain succeeded — it surfaces an error
        // rather than running only the first half and reporting success.
        assert!(
            result.is_error,
            "the PS5.1 fallback must report a `&&` chain as failed, not silently succeed: {}",
            result.output
        );
    }
}

#[tokio::test]
async fn run_shell_classifies_normalized_command_strings_before_running() {
    let (_dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::NonInteractive, true);

    #[cfg(windows)]
    let command = "cmd /c del x";
    #[cfg(not(windows))]
    let command = "rm -rf x";

    let denied = dispatch(
        &registry,
        "run_shell",
        json!({ "program": command }),
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

/// An approver that records each request it is consulted for, then approves.
struct RecordingApprover {
    requests: std::sync::Mutex<Vec<localpilot_sandbox::PermissionRequest>>,
}

impl RecordingApprover {
    fn new() -> Self {
        Self {
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }
    fn seen(&self) -> Vec<localpilot_sandbox::PermissionRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl localpilot_sandbox::Approver for RecordingApprover {
    fn approve<'a>(
        &'a self,
        request: &'a localpilot_sandbox::PermissionRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + 'a>> {
        self.requests.lock().unwrap().push(request.clone());
        Box::pin(async { true })
    }
}

#[tokio::test]
async fn run_shell_approval_prompt_shows_the_full_command_line() {
    let (_dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::Interactive, true);
    let approver = RecordingApprover::new();

    // A destructive command asks; the prompt must carry program + args.
    let call = ToolCall::new(
        ToolUseId::from("c1"),
        "run_shell",
        json!({ "program": "rm", "args": ["-rf", "build"] }),
    );
    let _ = registry
        .dispatch(&call, &c, &default_engine(), &approver)
        .await;

    let seen = approver.seen();
    assert!(!seen.is_empty(), "a destructive command must prompt");
    for request in &seen {
        assert_eq!(
            request.detail, "rm -rf build",
            "the user must see what they are approving"
        );
    }
}

#[tokio::test]
async fn write_file_approval_prompt_shows_the_path() {
    let (_dir, ws) = workspace_with(&[("existing.txt", "old")]);
    let registry = ToolRegistry::with_builtins();
    // Untrusted workspace so the write asks instead of auto-allowing.
    let c = ctx(&ws, Interactivity::Interactive, false);
    let approver = RecordingApprover::new();

    let call = ToolCall::new(
        ToolUseId::from("c1"),
        "write_file",
        json!({ "path": "existing.txt", "content": "new" }),
    );
    let _ = registry
        .dispatch(&call, &c, &default_engine(), &approver)
        .await;

    let seen = approver.seen();
    assert!(!seen.is_empty());
    assert_eq!(seen[0].detail, "existing.txt");
}

#[tokio::test]
async fn allowlisted_run_shell_still_prompts_for_destructive_commands() {
    let (_dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::Interactive, true);
    let relaxed = PermissionEngine::new(Profile::Relaxed, vec!["run_shell".to_string()]);

    // Destructive: the allowlist must not lift the gate — the approver is
    // consulted.
    let approver = RecordingApprover::new();
    let call = ToolCall::new(
        ToolUseId::from("c1"),
        "run_shell",
        json!({ "program": "rm", "args": ["-rf", "build"] }),
    );
    let _ = registry.dispatch(&call, &c, &relaxed, &approver).await;
    assert!(
        !approver.seen().is_empty(),
        "allowlisting run_shell must not auto-approve destructive commands"
    );

    // A wrapped command never classifies below Unknown, so it prompts too.
    let approver = RecordingApprover::new();
    let call = ToolCall::new(
        ToolUseId::from("c2"),
        "run_shell",
        json!({ "program": "bash", "args": ["-c", "echo hi"] }),
    );
    let _ = registry.dispatch(&call, &c, &relaxed, &approver).await;
    assert!(
        !approver.seen().is_empty(),
        "shell wrappers are never auto-allowed"
    );
}

#[tokio::test]
async fn write_file_refuses_to_clobber_a_binary_file_when_overwrite_is_false() {
    let dir = tempfile::tempdir().unwrap();
    // A non-UTF-8 target: read_to_string fails on it, but it exists.
    std::fs::write(dir.path().join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::Interactive, true);

    let result = dispatch(
        &registry,
        "write_file",
        json!({ "path": "blob.bin", "content": "text", "overwrite": false }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;

    assert!(result.is_error, "{}", result.output);
    assert!(result.output.contains("exists and overwrite is false"));
    // The binary content is untouched.
    assert_eq!(
        std::fs::read(dir.path().join("blob.bin")).unwrap(),
        vec![0u8, 159, 146, 150]
    );
}

/// An in-memory retention sink for spill tests.
#[derive(Default)]
struct MemoryRetention {
    entries: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl localpilot_tools::OutputRetention for MemoryRetention {
    fn retain(&self, id: &str, output: &str) -> Result<(), String> {
        self.entries
            .lock()
            .unwrap()
            .insert(id.to_string(), output.to_string());
        Ok(())
    }
    fn fetch(&self, id: &str) -> Result<Option<String>, String> {
        Ok(self.entries.lock().unwrap().get(id).cloned())
    }
}

#[tokio::test]
async fn apply_patch_applies_create_update_and_delete_atomically() {
    let (_dir, ws) = workspace_with(&[
        ("src/lib.rs", "fn old() {}\nfn keep() {}\n"),
        ("obsolete.txt", "bye"),
    ]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::Interactive, true);

    let result = dispatch(
        &registry,
        "apply_patch",
        json!({ "operations": [
            { "action": "update", "path": "src/lib.rs", "hunks": [
                { "old_text": "fn old() {}", "new_text": "fn renamed() {}" }
            ]},
            { "action": "create", "path": "src/new.rs", "content": "fn fresh() {}\n" },
            { "action": "delete", "path": "obsolete.txt" }
        ]}),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;

    assert!(!result.is_error, "{}", result.output);
    let lib = std::fs::read_to_string(ws.root().join("src/lib.rs")).unwrap();
    assert!(lib.contains("fn renamed()"));
    assert!(std::fs::read_to_string(ws.root().join("src/new.rs"))
        .unwrap()
        .contains("fn fresh()"));
    assert!(!ws.root().join("obsolete.txt").exists());
}

#[tokio::test]
async fn apply_patch_rejects_the_whole_patch_when_one_hunk_misses() {
    let (_dir, ws) = workspace_with(&[("a.txt", "alpha\n")]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::Interactive, true);

    let result = dispatch(
        &registry,
        "apply_patch",
        json!({ "operations": [
            { "action": "create", "path": "b.txt", "content": "beta\n" },
            { "action": "update", "path": "a.txt", "hunks": [
                { "old_text": "does not exist", "new_text": "x" }
            ]}
        ]}),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;

    assert!(result.is_error);
    assert!(
        result.output.contains("operation 2") && result.output.contains("was not found"),
        "the error names the failing operation: {}",
        result.output
    );
    // Nothing was applied: the create did not happen either.
    assert!(!ws.root().join("b.txt").exists());
}

#[tokio::test]
async fn apply_patch_approval_detail_previews_the_operations() {
    let (_dir, ws) = workspace_with(&[("a.txt", "alpha\n")]);
    let registry = ToolRegistry::with_builtins();
    // Untrusted workspace: writes ask, so the approver sees the detail.
    let c = ctx(&ws, Interactivity::Interactive, false);
    let approver = RecordingApprover::new();

    let call = ToolCall::new(
        ToolUseId::from("c1"),
        "apply_patch",
        json!({ "operations": [
            { "action": "update", "path": "a.txt", "hunks": [
                { "old_text": "alpha", "new_text": "beta" }
            ]}
        ]}),
    );
    let _ = registry
        .dispatch(&call, &c, &default_engine(), &approver)
        .await;

    let seen = approver.seen();
    assert!(!seen.is_empty());
    assert!(
        seen[0].detail.contains("update a.txt (1 hunks)"),
        "detail: {}",
        seen[0].detail
    );
}

#[tokio::test]
async fn oversized_output_is_bounded_and_spilled_to_retention() {
    let big = "line of output\n".repeat(4000); // ~60 KB, beyond the context bound
    let (_dir, ws) = workspace_with(&[("big.txt", &big)]);
    let registry = ToolRegistry::with_builtins();
    let retention = MemoryRetention::default();
    let c = ToolContext {
        workspace: &ws,
        interactivity: Interactivity::Interactive,
        trusted: true,
        retention: Some(&retention),
        processes: None,
    };

    let result = dispatch(
        &registry,
        "read_file",
        json!({ "path": "big.txt" }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;

    assert!(!result.is_error, "{}", result.output);
    assert!(
        result.output.len() < big.len() / 2,
        "context output is bounded"
    );
    assert!(result.output.contains("output truncated"));
    assert!(result.output.contains("read_tool_output"));
    // Head and tail both survive.
    assert!(result.output.starts_with("tool: read_file"));
    assert!(result.output.trim_end().ends_with("line of output"));

    // The full output is retained under the call id and fetchable.
    let fetched = dispatch(
        &registry,
        "read_tool_output",
        json!({ "id": "c1", "start_line": 1, "end_line": 2 }),
        &c,
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!fetched.is_error, "{}", fetched.output);
    assert!(fetched.output.contains("line of output"));
}

#[tokio::test]
async fn fetch_returns_body_when_network_is_approved() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello from the web"))
        .mount(&server)
        .await;

    let (_dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    let result = dispatch(
        &registry,
        "fetch",
        json!({ "url": format!("{}/page", server.uri()) }),
        &ctx(&ws, Interactivity::Interactive, true),
        &default_engine(),
        &ScriptedApprover::new(vec![true]),
    )
    .await;
    assert!(!result.is_error, "{}", result.output);
    assert!(result.output.contains("hello from the web"));
}

#[tokio::test]
async fn fetch_is_denied_non_interactive_without_hitting_the_network() {
    // No mock is mounted: a denied request must never reach a server.
    let server = MockServer::start().await;
    let (_dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    let result = dispatch(
        &registry,
        "fetch",
        json!({ "url": format!("{}/page", server.uri()) }),
        &ctx(&ws, Interactivity::NonInteractive, true),
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(result.is_error);
    assert!(result.output.contains("permission denied"));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "a denied fetch must not reach the network"
    );
}

#[tokio::test]
async fn fetch_approval_prompt_shows_the_url() {
    let (_dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    let c = ctx(&ws, Interactivity::Interactive, true);
    let approver = RecordingApprover::new();

    let url = "https://example.invalid/doc";
    let call = ToolCall::new(ToolUseId::from("c1"), "fetch", json!({ "url": url }));
    let _ = registry
        .dispatch(&call, &c, &default_engine(), &approver)
        .await;

    let seen = approver.seen();
    assert!(!seen.is_empty(), "a network fetch must prompt");
    assert_eq!(seen[0].detail, url);
}

#[tokio::test]
async fn fetch_rejects_non_http_schemes_without_a_network_call() {
    let (_dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    let result = dispatch(
        &registry,
        "fetch",
        json!({ "url": "file:///etc/passwd" }),
        // Bypass would allow the network effect; the scheme check must still fire.
        &ctx(&ws, Interactivity::Interactive, true),
        &bypass_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(result.is_error);
    assert!(result.output.contains("invalid input"));
    assert!(result.output.contains("http or https"));
}

#[tokio::test]
async fn replace_in_file_literal_changes_only_the_target() {
    let (dir, ws) = workspace_with(&[("f.txt", "alpha\nbeta\nalpha gamma\n")]);
    let registry = ToolRegistry::with_builtins();
    let result = dispatch(
        &registry,
        "replace_in_file",
        json!({ "path": "f.txt", "find": "alpha", "replace": "ALPHA" }),
        &ctx(&ws, Interactivity::NonInteractive, true),
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!result.is_error, "{}", result.output);
    let content = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
    assert_eq!(content, "ALPHA\nbeta\nALPHA gamma\n");
}

#[tokio::test]
async fn replace_in_file_regex_mode_matches_a_pattern() {
    let (dir, ws) = workspace_with(&[("f.txt", "alpha\nbeta\n")]);
    let registry = ToolRegistry::with_builtins();
    let result = dispatch(
        &registry,
        "replace_in_file",
        json!({ "path": "f.txt", "find": "al.ha", "replace": "X", "regex": true }),
        &ctx(&ws, Interactivity::NonInteractive, true),
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!result.is_error, "{}", result.output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        "X\nbeta\n"
    );
}

#[tokio::test]
async fn replace_in_file_no_match_leaves_the_file_unchanged() {
    let (dir, ws) = workspace_with(&[("f.txt", "alpha\nbeta\n")]);
    let registry = ToolRegistry::with_builtins();
    let result = dispatch(
        &registry,
        "replace_in_file",
        json!({ "path": "f.txt", "find": "zzz", "replace": "Y" }),
        &ctx(&ws, Interactivity::NonInteractive, true),
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!result.is_error, "{}", result.output);
    assert!(result.output.contains("no match"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        "alpha\nbeta\n"
    );
}

#[tokio::test]
async fn replace_in_file_denied_outside_the_workspace() {
    let (_dir, ws) = workspace_with(&[]);
    let registry = ToolRegistry::with_builtins();
    let outside_dir = tempfile::tempdir().unwrap();
    let outside = outside_dir.path().join("escape.txt");
    std::fs::write(&outside, "alpha\n").unwrap();
    let result = dispatch(
        &registry,
        "replace_in_file",
        json!({ "path": outside.to_str().unwrap(), "find": "alpha", "replace": "X" }),
        &ctx(&ws, Interactivity::NonInteractive, true),
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(result.is_error);
    assert!(result.output.contains("permission denied"));
    // The file outside the workspace is untouched.
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "alpha\n");
}

#[tokio::test]
async fn replace_in_file_treats_shell_metacharacters_as_literal_data() {
    // A replacement full of shell metacharacters must be inserted verbatim and
    // must not run as a command: the sibling file survives.
    let (dir, ws) = workspace_with(&[("f.txt", "beta\n"), ("keep.txt", "keep")]);
    let registry = ToolRegistry::with_builtins();
    // A multi-line payload full of shell metacharacters: must be inert data.
    let payload = "\"; rm -rf .\n$(echo hi)\ndel *; `whoami` #";
    let result = dispatch(
        &registry,
        "replace_in_file",
        json!({ "path": "f.txt", "find": "beta", "replace": payload }),
        &ctx(&ws, Interactivity::NonInteractive, true),
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!result.is_error, "{}", result.output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        format!("{payload}\n")
    );
    // No command ran: the sibling file is still there.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("keep.txt")).unwrap(),
        "keep"
    );
}

#[tokio::test]
async fn replace_in_file_replaces_a_multiline_block() {
    let (dir, ws) = workspace_with(&[("lib.rs", "fn old() {\n    work();\n}\n\nfn keep() {}\n")]);
    let registry = ToolRegistry::with_builtins();
    let result = dispatch(
        &registry,
        "replace_in_file",
        json!({
            "path": "lib.rs",
            "find": "fn old() {\n    work();\n}",
            "replace": "fn renamed() {\n    work();\n    extra();\n}",
        }),
        &ctx(&ws, Interactivity::NonInteractive, true),
        &default_engine(),
        &ScriptedApprover::always(),
    )
    .await;
    assert!(!result.is_error, "{}", result.output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("lib.rs")).unwrap(),
        "fn renamed() {\n    work();\n    extra();\n}\n\nfn keep() {}\n"
    );
}

/// Every builtin whose contract declares a side effect must also declare how its
/// result is verified — a postcondition, or an explicit `Unverifiable` admission.
/// A side effect with no declared verification could be claimed as success
/// without evidence, which the discipline track forbids.
#[test]
fn every_side_effecting_builtin_declares_verification() {
    let registry = ToolRegistry::with_builtins();
    for name in registry.names() {
        let tool = registry.get(name).expect("registered tool is retrievable");
        let contract = tool.contract();
        if contract.has_side_effect() {
            assert!(
                contract.is_verification_declared(),
                "{name} declares a side effect but no postcondition or Unverifiable admission"
            );
        }
    }
}

/// A tool with a benign in-workspace read effect (which the relaxed profile
/// auto-allows) and a configurable reversibility, to exercise reversibility-
/// aware confirmation.
struct TaggedTool {
    reversibility: Reversibility,
}

#[async_trait]
impl Tool for TaggedTool {
    fn name(&self) -> &str {
        "tagged"
    }
    fn description(&self) -> &str {
        "a tagged test tool"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    fn effects(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::ReadPath {
            inside_workspace: true,
            secret_like: false,
        }])
    }
    async fn invoke(
        &self,
        _input: serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok("done"))
    }
    fn contract(&self) -> ToolContract {
        ToolContract {
            reversibility: self.reversibility,
            ..ToolContract::default()
        }
    }
}

fn registry_with_tagged(reversibility: Reversibility) -> ToolRegistry {
    let mut registry = ToolRegistry::with_builtins();
    registry.register(Box::new(TaggedTool { reversibility }));
    registry
}

#[tokio::test]
async fn irreversible_tool_prompts_under_relaxed_where_reversible_auto_approves() {
    let (_dir, ws) = workspace_with(&[]);
    let context = ctx(&ws, Interactivity::Interactive, true);
    let relaxed = PermissionEngine::new(Profile::Relaxed, Vec::new());

    // Irreversible: the auto-allow is raised to a prompt. The approver denies,
    // so the call is refused — proving it was asked.
    let registry = registry_with_tagged(Reversibility::Irreversible);
    let denied = dispatch(
        &registry,
        "tagged",
        json!({}),
        &context,
        &relaxed,
        &ScriptedApprover::new(vec![false]),
    )
    .await;
    assert!(
        denied.is_error,
        "an irreversible tool must prompt under relaxed"
    );
    assert!(denied.output.contains("permission denied"));

    // Reversible: the relaxed profile auto-allows without a prompt, so the same
    // denying approver is never consulted and the call succeeds.
    let registry = registry_with_tagged(Reversibility::Reversible);
    let allowed = dispatch(
        &registry,
        "tagged",
        json!({}),
        &context,
        &relaxed,
        &ScriptedApprover::new(vec![false]),
    )
    .await;
    assert!(
        !allowed.is_error,
        "a reversible tool auto-approves under relaxed"
    );
}

#[tokio::test]
async fn bypass_scope_is_unchanged_by_reversibility() {
    let (_dir, ws) = workspace_with(&[]);
    let context = ctx(&ws, Interactivity::Interactive, true);

    // Under bypass, even an irreversible tool is not prompted: the denying
    // approver is never consulted, so the call still succeeds.
    let registry = registry_with_tagged(Reversibility::Irreversible);
    let result = dispatch(
        &registry,
        "tagged",
        json!({}),
        &context,
        &bypass_engine(),
        &ScriptedApprover::new(vec![false]),
    )
    .await;
    assert!(
        !result.is_error,
        "bypass must not prompt, even for an irreversible tool"
    );
}
