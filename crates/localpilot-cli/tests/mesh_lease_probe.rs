//! The write-authority escape probe: a participant whose session says it does
//! not own the tree tries every way the model has to write, through the real
//! tool registry and tools, launched with `bypass`, in a real Git repository
//! whose session the reference implementation runs. Nothing may change. Once
//! the reference hands it the tree it can write, and the offer back revokes
//! that at the very next tool call.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use localpilot_core::{ToolCall, ToolResult, ToolUseId};
use localpilot_mesh::ops::{Mesh, SessionLease};
use localpilot_sandbox::{
    Interactivity, Lease, PermissionEngine, Profile, ScriptedApprover, Workspace,
};
use localpilot_tools::{ToolContext, ToolRegistry};
use serde_json::{json, Value};
use support::{python_or_skip, suite};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn reference(py: &[String], anchor: &Path, args: &[&str]) {
    let st = Command::new(&py[0])
        .args(&py[1..])
        .arg(suite().join("reference").join("pair.py"))
        .arg("--repo")
        .arg(anchor)
        .args(args)
        .env_remove("PAIR_REPO")
        .env("PYTHONIOENCODING", "utf-8")
        .status()
        .unwrap();
    assert!(st.success(), "reference {args:?}");
}

/// The tree outside the mailbox: tracked and untracked files with contents.
fn tree(anchor: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![anchor.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            if name == ".git" || name == ".pair-programming" {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((p.clone(), std::fs::read(&p).unwrap()));
            }
        }
    }
    out.sort();
    out
}

async fn call(
    registry: &ToolRegistry,
    ws: &Workspace,
    engine: &PermissionEngine,
    tool: &str,
    input: Value,
) -> ToolResult {
    let ctx = ToolContext {
        workspace: ws,
        interactivity: Interactivity::Interactive,
        trusted: true,
        retention: None,
        processes: None,
        agents: None,
        prompter: None,
        peers: None,
    };
    let call = ToolCall::new(ToolUseId::from("probe"), tool, input);
    // An approver that approves everything: the lease, not a refusal, must
    // be what stops the write.
    registry
        .dispatch(&call, &ctx, engine, &ScriptedApprover::always())
        .await
}

fn lease(anchor: &Path, role: &str) -> Arc<dyn Lease> {
    SessionLease::acquire(Mesh::at(anchor, "flag"), role, Duration::from_secs(3600))
}

#[tokio::test]
async fn a_participant_that_does_not_own_the_tree_cannot_write_it_by_any_tool() {
    let Some(py) = python_or_skip("the write-lease escape probe") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let anchor = dir.path().join("anchor");
    std::fs::create_dir_all(&anchor).unwrap();
    git(&anchor, &["init", "-q"]);
    git(&anchor, &["config", "user.email", "pair@example.invalid"]);
    git(&anchor, &["config", "user.name", "pair-test"]);
    std::fs::write(anchor.join("README.md"), "base\n").unwrap();
    git(&anchor, &["add", "README.md"]);
    git(&anchor, &["commit", "-qm", "base"]);
    reference(
        &py,
        &anchor,
        &[
            "start",
            "--role",
            "claude",
            "--with",
            "localpilot",
            "--task",
            "probe",
        ],
    );
    reference(&py, &anchor, &["join", "--role", "localpilot"]);

    let ws = Workspace::new(&anchor).unwrap();
    let registry = ToolRegistry::with_builtins();
    let navigator =
        PermissionEngine::new(Profile::Bypass, Vec::new()).with_lease(lease(&anchor, "localpilot"));
    let before = tree(&anchor);
    let head = git(&anchor, &["rev-parse", "HEAD"]);

    let attempts = [
        ("write_file", json!({ "path": "new.txt", "content": "x" })),
        (
            "write_file",
            json!({ "path": "README.md", "content": "changed" }),
        ),
        (
            "edit_file",
            json!({ "path": "README.md", "old_text": "base", "new_text": "edited" }),
        ),
        (
            "replace_in_file",
            json!({ "path": "README.md", "find": "base", "replace": "BASE" }),
        ),
        (
            "apply_patch",
            json!({ "operations": [{ "action": "create", "path": "p.txt", "content": "x" }] }),
        ),
        ("run_shell", json!({ "command": "echo x > shell.txt" })),
        (
            "run_shell",
            json!({ "program": "git", "args": ["commit", "--allow-empty", "-m", "x"] }),
        ),
        (
            "run_shell",
            json!({ "program": "python", "args": ["-c", "open('py.txt','w')"] }),
        ),
    ];
    for (tool, input) in &attempts {
        let result = call(&registry, &ws, &navigator, tool, input.clone()).await;
        assert!(result.is_error(), "{tool} {input} ran: {}", result.output);
        assert!(
            result
                .output
                .contains("does not let this participant write"),
            "{tool}: {}",
            result.output
        );
    }
    assert_eq!(tree(&anchor), before, "the navigator changed the tree");
    assert_eq!(
        git(&anchor, &["rev-parse", "HEAD"]),
        head,
        "the navigator committed"
    );

    // The reference hands the tree over; a lease acquired now can write.
    reference(&py, &anchor, &["handoff-offer", "--role", "claude"]);
    reference(&py, &anchor, &["handoff-accept", "--role", "localpilot"]);
    let owner =
        PermissionEngine::new(Profile::Bypass, Vec::new()).with_lease(lease(&anchor, "localpilot"));
    let wrote = call(
        &registry,
        &ws,
        &owner,
        "write_file",
        json!({ "path": "owned.txt", "content": "x" }),
    )
    .await;
    assert!(!wrote.is_error(), "{}", wrote.output);
    assert!(anchor.join("owned.txt").exists());

    // Offering the tree back revokes the same lease at the next tool call.
    reference(&py, &anchor, &["handoff-offer", "--role", "localpilot"]);
    let revoked = call(
        &registry,
        &ws,
        &owner,
        "write_file",
        json!({ "path": "late.txt", "content": "x" }),
    )
    .await;
    assert!(revoked.is_error(), "{}", revoked.output);
    assert!(revoked.output.contains("handoff=yes"), "{}", revoked.output);
    assert!(!anchor.join("late.txt").exists());
}
