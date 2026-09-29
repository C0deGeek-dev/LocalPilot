//! `localpilot mesh cockpit --json`: the observer's snapshot of a session the
//! reference ran, read without writing anything.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use support::{python_or_skip, suite};

struct Tree {
    _dir: tempfile::TempDir,
    anchor: PathBuf,
    py: Vec<String>,
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

impl Tree {
    fn new() -> Option<Self> {
        let py = python_or_skip("the mesh cockpit tests")?;
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("anchor");
        std::fs::create_dir_all(&anchor).unwrap();
        git(&anchor, &["init", "-q"]);
        git(&anchor, &["config", "user.email", "pair@example.invalid"]);
        git(&anchor, &["config", "user.name", "pair-test"]);
        git(&anchor, &["config", "core.autocrlf", "false"]);
        std::fs::write(anchor.join("a.txt"), "alpha\n").unwrap();
        git(&anchor, &["add", "a.txt"]);
        git(&anchor, &["commit", "-qm", "base"]);
        Some(Tree {
            _dir: dir,
            anchor,
            py,
        })
    }

    fn reference(&self, args: &[&str]) -> String {
        let out = Command::new(&self.py[0])
            .args(&self.py[1..])
            .arg(suite().join("reference").join("pair.py"))
            .arg("--repo")
            .arg(&self.anchor)
            .args(args)
            .env_remove("PAIR_REPO")
            .env("PYTHONIOENCODING", "utf-8")
            .output()
            .unwrap();
        assert!(out.status.success(), "reference {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn snapshot(&self) -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_localpilot"))
            .arg("mesh")
            .arg("--repo")
            .arg(&self.anchor)
            .args(["cockpit", "--json"])
            .env_remove("PAIR_REPO")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn msg_id(&self, role: &str, kind: &str) -> String {
        let dir = self.anchor.join(".pair-programming").join("sessions");
        for s in std::fs::read_dir(dir).unwrap() {
            let j = s
                .unwrap()
                .path()
                .join("journal")
                .join(format!("{role}.jsonl"));
            if let Ok(text) = std::fs::read_to_string(j) {
                for line in text.lines().rev() {
                    let m: Value = serde_json::from_str(line).unwrap();
                    if m["kind"] == kind {
                        return m["msg_id"].as_str().unwrap().to_owned();
                    }
                }
            }
        }
        panic!("no {kind} from {role}");
    }
}

/// Every file under the mailbox with its size, modification time and bytes.
fn mailbox_state(anchor: &Path) -> BTreeMap<PathBuf, (u64, std::time::SystemTime, Vec<u8>)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![anchor.join(".pair-programming")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let m = std::fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                stack.push(p);
            } else {
                out.insert(
                    p.clone(),
                    (m.len(), m.modified().unwrap(), std::fs::read(&p).unwrap()),
                );
            }
        }
    }
    out
}

#[test]
fn with_no_session_the_snapshot_says_so() {
    let Some(t) = Tree::new() else {
        return;
    };
    let s = t.snapshot();
    assert!(s["session"].is_null(), "{s}");
    assert_eq!(s["consistent"], true);
}

#[test]
fn the_snapshot_shows_authority_mail_and_the_open_review_and_writes_nothing() {
    let Some(t) = Tree::new() else {
        return;
    };
    t.reference(&[
        "start",
        "--role",
        "claude",
        "--with",
        "codex,localpilot",
        "--task",
        "cockpit test",
    ]);
    t.reference(&["join", "--role", "codex", "--timeout", "1"]);
    t.reference(&["join", "--role", "localpilot", "--timeout", "1"]);
    std::fs::write(t.anchor.join("a.txt"), "beta\n").unwrap();
    t.reference(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "REVIEW_REQUEST",
        "--to",
        "codex,localpilot",
        "--expect-reply",
        "--body",
        "please review a.txt",
    ]);
    let request = t.msg_id("claude", "REVIEW_REQUEST");
    t.reference(&["watch", "--role", "codex", "--timeout", "1"]);
    t.reference(&[
        "post",
        "--role",
        "codex",
        "--kind",
        "VERDICT",
        "--reply-to",
        &request,
        "--body",
        "AGREE round=1 blocking=0 important=0\nfine",
    ]);
    // localpilot has the request delivered to it but not acknowledged.
    t.reference(&["watch", "--role", "localpilot", "--timeout", "1"]);

    let before = mailbox_state(&t.anchor);
    let s = t.snapshot();
    assert_eq!(
        mailbox_state(&t.anchor),
        before,
        "the cockpit wrote to the mailbox"
    );

    assert_eq!(s["consistent"], true, "{s}");
    assert!(s["read_errors"].as_array().unwrap().is_empty(), "{s}");
    let v = &s["session"];
    assert_eq!(v["owner"], "claude");
    assert_eq!(
        v["required_reviewers"],
        serde_json::json!(["codex", "localpilot"])
    );
    assert_eq!(v["delivery"], "ack");
    assert!(v["unit_id"].is_string());
    let roles: Vec<&str> = v["participants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["claude", "codex", "localpilot"]);
    let lp = v["participants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["role"] == "localpilot")
        .unwrap();
    assert_eq!(lp["health"], "ready");
    assert!(
        lp["unacked"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u[0] == "claude"),
        "{lp}"
    );
    assert_eq!(v["review"]["request"]["msg_id"], request.as_str());
    let verdicts = v["review"]["verdicts"].as_array().unwrap();
    assert_eq!(verdicts.len(), 1, "{v}");
    assert_eq!(verdicts[0]["role"], "codex");
    assert!(v["handoff"].is_null());
}

#[test]
fn a_pending_handoff_and_a_pause_are_shown() {
    let Some(t) = Tree::new() else {
        return;
    };
    t.reference(&[
        "start",
        "--role",
        "claude",
        "--with",
        "codex",
        "--task",
        "handoff test",
    ]);
    t.reference(&["join", "--role", "codex", "--timeout", "1"]);
    t.reference(&["handoff-offer", "--role", "claude"]);
    t.reference(&[
        "health",
        "--role",
        "codex",
        "--status",
        "rate_limited",
        "--reason",
        "quota",
    ]);
    let v = &t.snapshot()["session"];
    assert_eq!(v["handoff"]["to"], "codex", "{v}");
    let codex = v["participants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["role"] == "codex")
        .unwrap();
    assert_eq!(codex["health"], "rate_limited");
}

#[test]
fn a_verdict_that_names_no_request_is_never_shown_as_its_answer() {
    // A two-party (schema 1) session posts verdicts without `reply_to`. An
    // older verdict followed by a newer request, even within the same second,
    // must not read as that request's answer.
    let Some(t) = Tree::new() else {
        return;
    };
    t.reference(&["start", "--role", "claude", "--task", "legacy pair"]);
    t.reference(&["join", "--role", "codex", "--timeout", "1"]);
    t.reference(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "REVIEW_REQUEST",
        "--expect-reply",
        "--body",
        "first",
    ]);
    t.reference(&["watch", "--role", "codex", "--timeout", "1"]);
    t.reference(&[
        "post",
        "--role",
        "codex",
        "--kind",
        "VERDICT",
        "--body",
        "REVISE round=1 blocking=1 important=0\nfix it",
    ]);
    t.reference(&["watch", "--role", "claude", "--timeout", "1"]);
    t.reference(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "REVIEW_REQUEST",
        "--expect-reply",
        "--body",
        "second",
    ]);
    let v = &t.snapshot()["session"];
    assert_eq!(v["schema"], 1, "{v}");
    assert_eq!(v["review_state"], "open");
    assert_eq!(v["review"]["request"]["body"], "second");
    assert!(
        v["review"]["verdicts"].as_array().unwrap().is_empty(),
        "{v}"
    );
    let unlinked = v["review"]["unlinked"].as_array().unwrap();
    assert_eq!(unlinked.len(), 1);
    assert_eq!(unlinked[0]["role"], "codex");
}

#[test]
fn a_long_journal_is_read_only_in_its_window_and_says_so() {
    let Some(t) = Tree::new() else {
        return;
    };
    t.reference(&[
        "start",
        "--role",
        "claude",
        "--with",
        "codex,localpilot",
        "--task",
        "long",
    ]);
    t.reference(&["join", "--role", "codex", "--timeout", "1"]);
    t.reference(&["join", "--role", "localpilot", "--timeout", "1"]);
    t.reference(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "REVIEW_REQUEST",
        "--to",
        "codex,localpilot",
        "--expect-reply",
        "--body",
        "old request",
    ]);
    // Far more than the snapshot's window after the request: 20,000 records.
    let session = std::fs::read_dir(t.anchor.join(".pair-programming").join("sessions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let journal = session.join("journal").join("claude.jsonl");
    let unit = t.snapshot()["session"]["unit_id"].clone();
    let mut text = std::fs::read_to_string(&journal).unwrap();
    for n in 0..20_000 {
        text.push_str(
            &serde_json::json!({
                "seq": 100 + n, "kind": "NOTE", "role": "claude", "at": "2026-09-29T00:00:00Z",
                "unit_id": unit, "body": "filler filler filler filler filler",
            })
            .to_string(),
        );
        text.push('\n');
    }
    std::fs::write(&journal, text).unwrap();
    let v = &t.snapshot()["session"];
    let claude = v["participants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["role"] == "claude")
        .unwrap();
    assert_eq!(claude["recent"].as_array().unwrap().len(), 50);
    assert_eq!(claude["recent_truncated"], true);
    assert_eq!(claude["recent"][49]["seq"], 100 + 19_999);
    // The request lies before the window: said, not guessed.
    assert_eq!(v["review_state"], "beyond_window", "{}", v["review_state"]);
    assert!(v["review"].is_null());
}
