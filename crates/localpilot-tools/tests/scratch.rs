//! Real tool dispatch pins the same path boundary across file and shell routes.
#![allow(clippy::unwrap_used)]

use std::path::Path;

use localpilot_core::{ToolCall, ToolUseId};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_tools::{BackgroundProcesses, ToolContext, ToolRegistry};
use serde_json::{json, Value};

async fn call(
    ws: &Workspace,
    tool: &str,
    input: Value,
    profile: Profile,
) -> localpilot_core::ToolResult {
    call_with_engine(ws, tool, input, &PermissionEngine::new(profile, Vec::new())).await
}

async fn call_with_engine(
    ws: &Workspace,
    tool: &str,
    input: Value,
    engine: &PermissionEngine,
) -> localpilot_core::ToolResult {
    let processes = BackgroundProcesses::new();
    let context = ToolContext {
        workspace: ws,
        interactivity: Interactivity::NonInteractive,
        trusted: true,
        retention: None,
        processes: Some(&processes),
        agents: None,
        prompter: None,
        peers: None,
    };
    let result = ToolRegistry::with_builtins()
        .dispatch(
            &ToolCall::new(ToolUseId::from("scratch-test"), tool, input),
            &context,
            engine,
            &ScriptedApprover::new(Vec::new()),
        )
        .await;
    // Short-lived background fixtures can still be starting when grace ends.
    // Wait for actual completion instead of racing their first instruction.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while processes.list().iter().any(|process| process.alive) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    processes.kill_all();
    result
}

#[tokio::test]
async fn exact_user_grants_authorize_scripts_but_never_visible_external_targets() {
    use localpilot_sandbox::AllowedCommand;
    let dir = tempfile::tempdir().unwrap();
    let scripts = tempfile::tempdir().unwrap();
    let mut ws = Workspace::new(dir.path()).unwrap();
    ws.start_scratch("vetted").unwrap();
    #[cfg(windows)]
    let (program, filename, body, mut args) = (
        "powershell.exe", "fixture.ps1",
        "Set-Content -LiteralPath (Join-Path $env:LOCALPILOT_SCRATCH_DIR 'vetted.txt') -Value fixture",
        vec!["-NoProfile".to_string(), "-NonInteractive".to_string(), "-File".to_string()],
    );
    #[cfg(not(windows))]
    let (program, filename, body, mut args) = (
        "sh",
        "fixture.sh",
        "echo fixture > \"$LOCALPILOT_SCRATCH_DIR/vetted.txt\"",
        Vec::new(),
    );
    let script = scripts.path().join(filename);
    std::fs::write(&script, body).unwrap();
    args.push(script.display().to_string());
    let engine = PermissionEngine::new(Profile::Default, Vec::new()).with_allowed_commands(vec![
        AllowedCommand {
            program: program.into(),
            args_prefix: args.clone(),
        },
    ]);
    // A visible in-scope argument must not reintroduce contract confirmation
    // after the exact execution was vetted. External arguments still deny.
    args.push(ws.scratch_process_dir().unwrap().display().to_string());
    for tool in ["run_shell", "run_background"] {
        let input = if tool == "run_shell" {
            json!({"program": program, "args": args})
        } else {
            json!({"action": "start", "program": program, "args": args, "grace_secs": 1})
        };
        let result = call_with_engine(&ws, tool, input, &engine).await;
        assert!(
            !result.output.contains("permission denied"),
            "{}",
            result.output
        );
        let marker = ws.scratch_dir().unwrap().join("vetted.txt");
        assert!(marker.exists(), "{}", result.output);
        std::fs::remove_file(marker).unwrap();
        let target = scripts.path().join("external.txt");
        let mut outside_args = args.clone();
        outside_args.push(target.display().to_string());
        let input = if tool == "run_shell" {
            json!({"program": program, "args": outside_args})
        } else {
            json!({"action": "start", "program": program, "args": outside_args, "grace_secs": 1})
        };
        let result = call_with_engine(&ws, tool, input, &engine).await;
        assert!(
            result.output.contains("permission denied"),
            "{}",
            result.output
        );
        assert!(!ws.scratch_dir().unwrap().join("vetted.txt").exists());
        assert!(!target.exists());
    }
}

fn redirect(target: &Path) -> String {
    format!("echo scratch > '{}'", target.display())
}

#[tokio::test]
async fn file_and_shell_dispatch_agree_on_scratch_outside_and_secret_paths() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut ws = Workspace::new(dir.path()).unwrap();
    ws.start_scratch("tools").unwrap();
    let root = ws.scratch_process_dir().unwrap();
    let link = root.join("escape");
    #[cfg(windows)]
    assert!(std::process::Command::new("cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&link)
        .arg(outside.path())
        .output()
        .unwrap()
        .status
        .success());
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    for profile in [Profile::Default, Profile::Bypass] {
        let target = root.join(format!("{profile:?}.txt"));
        let write = call(
            &ws,
            "write_file",
            json!({"path": target, "content": "fixture"}),
            profile,
        )
        .await;
        assert!(!write.is_error(), "{}", write.output);
        let read = call(&ws, "read_file", json!({"path": target}), profile).await;
        assert!(!read.is_error(), "{}", read.output);
        assert!(read.output.contains("fixture"));
    }
    let shell_target = root.join("shell.txt");
    let result = call(
        &ws,
        "run_shell",
        json!({"command": redirect(&shell_target)}),
        Profile::Bypass,
    )
    .await;
    assert!(!result.is_error(), "{}", result.output);
    // Windows PowerShell 5.1 and newer shells use different redirect encodings.
    assert!(!std::fs::read(&shell_target).unwrap().is_empty());
    let background_target = root.join("background.txt");
    let result = call(
        &ws,
        "run_background",
        json!({"action": "start", "command": redirect(&background_target), "grace_secs": 1}),
        Profile::Bypass,
    )
    .await;
    assert!(
        result.output.contains("with code 0")
            || result.output.contains("started background process"),
        "{}",
        result.output
    );
    assert!(background_target.exists());
    for target in [
        outside.path().join("outside.txt"),
        root.join(".env"),
        link.join("escaped.txt"),
    ] {
        for tool in ["write_file", "run_shell", "run_background"] {
            let input = match tool {
                "write_file" => json!({"path": target, "content": "blocked"}),
                "run_shell" => json!({"command": redirect(&target)}),
                _ => json!({"action": "start", "command": redirect(&target), "grace_secs": 0}),
            };
            let result = call(&ws, tool, input, Profile::Bypass).await;
            assert!(result.is_error(), "{tool}: {}", result.output);
            assert!(result.output.contains("permission denied"));
            assert!(!target.exists());
        }
    }
}

#[tokio::test]
async fn opaque_scripts_are_denied_under_bypass_but_child_env_reports_owned_scratch() {
    let dir = tempfile::tempdir().unwrap();
    let mut ws = Workspace::new(dir.path()).unwrap();
    ws.start_scratch("env").unwrap();
    #[cfg(windows)]
    let input = json!({"program": "powershell.exe", "args": ["-NoProfile", "-NonInteractive", "-Command", "$env:LOCALPILOT_SCRATCH_DIR; $env:TEMP; $env:TMP; $env:TMPDIR"]});
    #[cfg(not(windows))]
    let input = json!({"program": "sh", "args": ["-c", "printf '%s\\n' \"$LOCALPILOT_SCRATCH_DIR\" \"$TEMP\" \"$TMP\" \"$TMPDIR\""]});
    let denied = call(&ws, "run_shell", input.clone(), Profile::Bypass).await;
    assert!(denied.is_error());
    assert!(denied.output.contains("file targets cannot be inspected"));
    let result = call(&ws, "run_shell", input, Profile::Unrestricted).await;
    assert!(!result.is_error(), "{}", result.output);
    assert_eq!(
        result
            .output
            .matches(&ws.scratch_process_dir().unwrap().display().to_string())
            .count(),
        4
    );
    let path = ws.scratch_dir().unwrap().to_path_buf();
    drop(ws);
    assert!(!path.exists());
}

#[tokio::test]
async fn an_external_shell_write_is_grantable_only_after_interactive_approval() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let target = outside.path().join("approved.txt");
    let context = ToolContext {
        workspace: &ws,
        interactivity: Interactivity::Interactive,
        trusted: true,
        retention: None,
        processes: None,
        agents: None,
        prompter: None,
        peers: None,
    };
    let call = ToolCall::new(
        ToolUseId::from("approval"),
        "run_shell",
        json!({"command": redirect(&target)}),
    );
    let engine = PermissionEngine::new(Profile::Bypass, Vec::new());
    let registry = ToolRegistry::with_builtins();
    let denied = registry
        .dispatch(
            &call,
            &context,
            &engine,
            &ScriptedApprover::new(vec![false]),
        )
        .await;
    assert!(denied.is_error());
    assert!(!target.exists());
    let allowed = registry
        .dispatch(&call, &context, &engine, &ScriptedApprover::new(vec![true]))
        .await;
    assert!(!allowed.is_error(), "{}", allowed.output);
    assert!(target.exists());
}

#[tokio::test]
async fn a_scratch_shell_write_needs_only_one_command_confirmation_in_default() {
    let dir = tempfile::tempdir().unwrap();
    let mut ws = Workspace::new(dir.path()).unwrap();
    ws.start_scratch("confirmation").unwrap();
    let target = ws.scratch_process_dir().unwrap().join("approved.txt");
    let context = ToolContext {
        workspace: &ws,
        interactivity: Interactivity::Interactive,
        trusted: true,
        retention: None,
        processes: None,
        agents: None,
        prompter: None,
        peers: None,
    };
    let call = ToolCall::new(
        ToolUseId::from("confirmation"),
        "run_shell",
        json!({"command": redirect(&target)}),
    );
    let result = ToolRegistry::with_builtins()
        .dispatch(
            &call,
            &context,
            &PermissionEngine::new(Profile::Default, Vec::new()),
            &ScriptedApprover::new(vec![true]),
        )
        .await;
    assert!(!result.is_error(), "{}", result.output);
    assert!(target.exists());
}
