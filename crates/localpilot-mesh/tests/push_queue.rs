//! A message that reached the journal is owed its wakes even when a later
//! step of the post fails (spec P-3): the push is queued right after the
//! append, not at the end of the post.
//!
//! Needs Python 3.9+ for the reference implementation, found as in
//! `scan_cases.rs`; without one this is skipped, and fails under `CI=true`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use localpilot_mesh::ops::push::Transport;
use localpilot_mesh::ops::PostArgs;
use localpilot_mesh::{Mailbox, Mesh};
use serde_json::Value;

fn reference() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("conformance")
        .join("reference")
        .join("pair.py")
}

fn python() -> Option<Vec<String>> {
    let mut candidates: Vec<Vec<String>> = Vec::new();
    if let Ok(p) = std::env::var("LOCALPILOT_CONFORMANCE_PYTHON") {
        if !p.trim().is_empty() {
            candidates.push(vec![p]);
        }
    }
    candidates.push(vec!["python3".into()]);
    candidates.push(vec!["python".into()]);
    candidates.push(vec!["py".into(), "-3".into()]);
    candidates.into_iter().find(|c| {
        Command::new(&c[0])
            .args(&c[1..])
            .args(["-c", "import sys; sys.exit(sys.version_info < (3, 9))"])
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

fn pair(py: &[String], anchor: &Path, args: &[&str]) {
    let out = Command::new(&py[0])
        .args(&py[1..])
        .arg(reference())
        .arg("--repo")
        .arg(anchor)
        .args(args)
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PAIR_NO_PUSH", "1")
        .output()
        .expect("run the reference");
    assert!(
        out.status.success(),
        "pair.py {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_message_in_the_journal_is_owed_its_wake_even_if_the_post_then_fails() {
    let Some(py) = python() else {
        assert!(
            std::env::var("CI").is_err(),
            "Python 3.9+ is required under CI"
        );
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let anchor = dir.path();
    pair(
        &py,
        anchor,
        &["start", "--role", "claude", "--task", "t", "--no-vcs"],
    );
    pair(&py, anchor, &["join", "--role", "codex", "--timeout", "1"]);
    // A dialable address that nothing listens on: nothing is dialled here.
    let (transport, address) = if cfg!(windows) {
        ("pipe", r"\\.\pipe\lp-push-queue-never".to_owned())
    } else {
        (
            "unix",
            anchor.join("never.sock").to_string_lossy().into_owned(),
        )
    };
    pair(
        &py,
        anchor,
        &[
            "endpoint",
            "--role",
            "claude",
            "--register",
            "--transport",
            transport,
            "--address",
            &address,
        ],
    );
    let active: Value = serde_json::from_str(
        &std::fs::read_to_string(anchor.join(".pair-programming").join("active.json")).unwrap(),
    )
    .unwrap();
    let sid = active["session_id"].as_str().unwrap().to_owned();
    // Break the step after the append: `latest` stays readable (the post
    // reads it first) but cannot be replaced.
    let latest = Mailbox::at(anchor).latest(&sid, "codex");
    let guard = FreezeLatest::new(&latest);

    let mesh = Mesh::at(anchor, "flag");
    let args = PostArgs {
        kind: "NOTE".into(),
        body: "for claude".into(),
        ..PostArgs::default()
    };
    assert!(
        mesh.post("codex", &args).is_err(),
        "the post should fail after its append"
    );
    let journal = std::fs::read_to_string(Mailbox::at(anchor).journal(&sid, "codex")).unwrap();
    assert!(
        journal.contains("for claude"),
        "the message is in the journal"
    );

    let jobs = mesh.take_push_jobs();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].to, "claude");
    assert_eq!(jobs[0].sender, "codex");
    assert_eq!(jobs[0].msg_id, "codex:2");
    assert_eq!(
        jobs[0].transport,
        if cfg!(windows) {
            Transport::Pipe
        } else {
            Transport::Unix
        }
    );
    assert!(
        mesh.take_push_jobs().is_empty(),
        "taking the jobs drains them"
    );
    drop(guard);
}

/// Makes `latest` unreplaceable while it lives: a read-only file on Windows
/// (a rename cannot replace it), a read-only directory elsewhere (no temp file
/// can be made beside it). Undone on drop, so the temp dir can be removed.
struct FreezeLatest(PathBuf);

impl FreezeLatest {
    fn new(latest: &Path) -> Self {
        #[cfg(windows)]
        let target = latest.to_path_buf();
        #[cfg(not(windows))]
        let target = latest.parent().unwrap().to_path_buf();
        set_readonly(&target, true);
        Self(target)
    }
}

impl Drop for FreezeLatest {
    fn drop(&mut self) {
        set_readonly(&self.0, false);
    }
}

#[cfg(windows)]
fn set_readonly(p: &Path, on: bool) {
    let mut perm = std::fs::metadata(p).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(on);
    std::fs::set_permissions(p, perm).unwrap();
}

#[cfg(not(windows))]
fn set_readonly(p: &Path, on: bool) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        p,
        std::fs::Permissions::from_mode(if on { 0o555 } else { 0o755 }),
    )
    .unwrap();
}
