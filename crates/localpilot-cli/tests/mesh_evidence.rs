//! `localpilot mesh evidence` end to end: a session the reference started,
//! and the evidence service answering its participants.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use sha2::{Digest, Sha256};
use support::{python_or_skip, suite};

struct Tree {
    _dir: tempfile::TempDir,
    anchor: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A tree with `src/lib.rs`, and a session the reference started as claude
/// with localpilot: in a Git repository, or with `--no-vcs`.
fn session(vcs: bool) -> Option<Tree> {
    let py = python_or_skip("the mesh evidence tests")?;
    let dir = tempfile::tempdir().unwrap();
    let anchor = dir.path().join("anchor");
    std::fs::create_dir_all(anchor.join("src")).unwrap();
    std::fs::write(
        anchor.join("src").join("lib.rs"),
        "pub fn alpha() {}\npub fn beta() {}\n",
    )
    .unwrap();
    if vcs {
        git(&anchor, &["init", "-q"]);
        git(&anchor, &["config", "user.email", "pair@example.invalid"]);
        git(&anchor, &["config", "user.name", "pair-test"]);
        git(&anchor, &["config", "core.autocrlf", "false"]);
        git(&anchor, &["add", "."]);
        git(&anchor, &["commit", "-qm", "base"]);
    }
    let mut args = vec!["start", "--role", "claude", "--with", "localpilot"];
    if !vcs {
        args.push("--no-vcs");
    }
    args.extend(["--task", "evidence test"]);
    let st = Command::new(&py[0])
        .args(&py[1..])
        .arg(suite().join("reference").join("pair.py"))
        .arg("--repo")
        .arg(&anchor)
        .args(&args)
        .env_remove("PAIR_REPO")
        .env("PYTHONIOENCODING", "utf-8")
        .output()
        .unwrap();
    assert!(st.status.success(), "{st:?}");
    Some(Tree { _dir: dir, anchor })
}

impl Tree {
    fn evidence(&self, role: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_localpilot"))
            .arg("mesh")
            .arg("--repo")
            .arg(&self.anchor)
            .args(["evidence", "--role", role])
            .args(args)
            .env_remove("PAIR_REPO")
            .output()
            .unwrap()
    }

    fn packet_as(&self, role: &str, args: &[&str]) -> Value {
        let out = self.evidence(role, args);
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

#[test]
fn a_located_line_verifies_until_it_moves_or_goes() {
    let Some(t) = session(true) else {
        return;
    };
    let ask = |args: &[&str]| {
        let out = t.evidence("localpilot", args);
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let found = ask(&["locate", "--query", "pub fn beta"]);
    assert_eq!(found["op"], "locate");
    assert_eq!(found["head"], git(&t.anchor, &["rev-parse", "HEAD"]));
    assert!(found["session_id"].as_str().is_some_and(|s| !s.is_empty()));
    let hit = found["hits"][0].clone();
    assert_eq!(
        (hit["path"].as_str(), hit["start"].as_u64()),
        (Some("src/lib.rs"), Some(2))
    );
    let anchor = serde_json::json!([{"path": hit["path"], "start": hit["start"], "end": hit["end"], "sha": hit["sha"]}]).to_string();
    let check = |a: &str| ask(&["verify", "--anchors", a])["checks"][0]["check"].clone();
    assert_eq!(check(&anchor)["state"], "ok");
    let lib = t.anchor.join("src").join("lib.rs");
    std::fs::write(
        &lib,
        "// new first line\npub fn alpha() {}\npub fn beta() {}\n",
    )
    .unwrap();
    let moved = check(&anchor);
    assert_eq!(
        (moved["state"].as_str(), moved["start"].as_u64()),
        (Some("moved"), Some(3))
    );
    std::fs::write(&lib, "pub fn alpha() {}\npub fn gamma() {}\n").unwrap();
    assert_eq!(check(&anchor)["state"], "stale");
}

#[test]
fn an_anchor_pins_the_lines_it_read() {
    let Some(t) = session(true) else {
        return;
    };
    let out = t.evidence(
        "localpilot",
        &[
            "anchor",
            "--path",
            "src/lib.rs",
            "--start",
            "1",
            "--end",
            "2",
        ],
    );
    assert!(out.status.success(), "{out:?}");
    let a: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(a["text"], "pub fn alpha() {}\npub fn beta() {}");
    let want = format!(
        "{:x}",
        Sha256::digest(b"pub fn alpha() {}\npub fn beta() {}")
    );
    assert_eq!(a["sha"], want);
}

#[test]
fn a_refused_request_exits_one_and_says_why() {
    let Some(t) = session(true) else {
        return;
    };
    std::fs::write(t.anchor.join(".env"), "KEY=secret\n").unwrap();
    let out = t.evidence(
        "localpilot",
        &["anchor", "--path", ".env", "--start", "1", "--end", "1"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("secrets"));
    let out = t.evidence(
        "localpilot",
        &["anchor", "--path", "../x", "--start", "1", "--end", "1"],
    );
    assert_eq!(out.status.code(), Some(1));
    // `.git` and the mailbox in another case (they exist on Windows and
    // macOS under these names too).
    for rel in [".GIT/config", ".PAIR-PROGRAMMING/active.v2.json"] {
        let out = t.evidence(
            "localpilot",
            &["anchor", "--path", rel, "--start", "1", "--end", "1"],
        );
        assert_eq!(out.status.code(), Some(1), "{rel}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("metadata"),
            "{rel}"
        );
        let d = t.packet_as("localpilot", &["diagnostics", "--path", rel]);
        assert!(d["exists"][0]["refused"].is_string(), "{rel}: {d}");
    }
    // More anchors than one request may check.
    let many = serde_json::json!(vec![
        serde_json::json!({"path": "src/lib.rs", "start": 1, "end": 1, "sha": "0".repeat(64)});
        101
    ])
    .to_string();
    let out = t.evidence("localpilot", &["verify", "--anchors", &many]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("at most 100 anchors"));
    // Not a participant of this session.
    let out = t.evidence("codex", &["locate", "--query", "alpha"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

/// Every file under `dir` with its length, modification time and hash.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (u64, std::time::SystemTime, String)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let m = std::fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                stack.push(p);
            } else {
                let bytes = std::fs::read(&p).unwrap();
                out.insert(
                    p,
                    (
                        m.len(),
                        m.modified().unwrap(),
                        format!("{:x}", Sha256::digest(bytes)),
                    ),
                );
            }
        }
    }
    out
}

#[test]
fn diagnostics_read_git_and_change_nothing() {
    let Some(t) = session(true) else {
        return;
    };
    // A dirty tree, and an index that git status would like to refresh.
    std::fs::write(t.anchor.join("src").join("lib.rs"), "pub fn alpha() {}\n").unwrap();
    std::fs::write(t.anchor.join("new.txt"), "n\n").unwrap();
    let git_dir = t.anchor.join(".git");
    let before = snapshot(&git_dir);
    let d = t.packet_as(
        "localpilot",
        &[
            "diagnostics",
            "--path",
            "src/lib.rs",
            "--path",
            "gone.txt",
            "--path",
            ".env",
        ],
    );
    assert_eq!(snapshot(&git_dir), before, "diagnostics changed .git");
    let status = d["status"]["output"].as_str().unwrap();
    assert!(
        status.contains(" M src/lib.rs") && status.contains("?? new.txt"),
        "{status}"
    );
    assert_eq!(d["status"]["exit"], 0);
    assert!(d["diff_stat"]["output"]
        .as_str()
        .unwrap()
        .contains("src/lib.rs"));
    assert_eq!(d["exists"][0]["exists"], "file");
    assert_eq!(d["exists"][1]["exists"], "missing");
    assert!(d["exists"][2]["refused"].is_string());
}

#[test]
fn without_version_control_the_git_diagnostics_are_unavailable() {
    let Some(t) = session(false) else {
        return;
    };
    let d = t.packet_as("localpilot", &["diagnostics"]);
    assert!(d["status"]["unavailable"].is_string(), "{d}");
    assert!(d["diff_stat"]["unavailable"].is_string(), "{d}");
    // No packet rescans the tree for its digest: the head is absent, and
    // the packet says why.
    assert!(d["head"].is_null(), "{d}");
    assert!(d["head_unavailable"].is_string(), "{d}");
}
