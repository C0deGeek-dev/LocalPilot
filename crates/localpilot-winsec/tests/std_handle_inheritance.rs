//! A long-lived grandchild must not hold the caller's pipe open once the
//! parent that started it has cleared its standard handles' inherit flag,
//! and a child given inherited streams must still write to them.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const ROLE: &str = "WINSEC_INHERIT_TEST_ROLE";

/// Run inside the re-executed test binary: optionally clear the inherit
/// flags, let a child write through inherited streams, start a grandchild
/// that outlives us with no streams of its own, print its PID and exit.
fn act_as_parent(clear: bool) {
    if clear {
        localpilot_winsec::keep_std_handles_from_children().unwrap();
    }
    let status = Command::new("cmd")
        .args(["/C", "echo inherited-ok"])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .unwrap();
    assert!(status.success());
    let grandchild = Command::new("ping")
        .args(["-n", "30", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    println!("grandchild={}", grandchild.id());
    std::process::exit(0);
}

/// Start this test binary as the parent and read its stdout line by line
/// until EOF or `limit`; return what was read and whether EOF arrived.
fn run_parent(test: &str, mode: &str, limit: Duration) -> (String, bool) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads", "1"])
        .env(ROLE, mode)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(Some(line)).is_err() {
                return;
            }
        }
        let _ = tx.send(None);
    });
    let _ = child.wait();
    let deadline = std::time::Instant::now() + limit;
    let mut text = String::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(Some(line)) => {
                text.push_str(&line);
                text.push('\n');
            }
            Ok(None) => return (text, true),
            Err(_) => return (text, false),
        }
    }
}

fn kill_grandchild(text: &str) {
    if let Some(pid) = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("grandchild="))
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/PID", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[test]
fn a_cleared_parent_lets_the_caller_read_to_the_end() {
    match std::env::var(ROLE).as_deref() {
        Ok("clear") => act_as_parent(true),
        Ok(_) => return,
        Err(_) => {}
    }
    let (text, ended) = run_parent(
        "a_cleared_parent_lets_the_caller_read_to_the_end",
        "clear",
        Duration::from_secs(15),
    );
    kill_grandchild(&text);
    assert!(
        ended,
        "the caller's read did not end while the grandchild ran"
    );
    assert!(
        text.contains("inherited-ok"),
        "inherited streams lost: {text:?}"
    );
    assert!(text.contains("grandchild="), "{text:?}");
}

#[test]
fn without_clearing_the_grandchild_holds_the_pipe() {
    match std::env::var(ROLE).as_deref() {
        Ok("keep") => act_as_parent(false),
        Ok(_) => return,
        Err(_) => {}
    }
    let (text, ended) = run_parent(
        "without_clearing_the_grandchild_holds_the_pipe",
        "keep",
        Duration::from_secs(3),
    );
    kill_grandchild(&text);
    assert!(text.contains("grandchild="), "{text:?}");
    assert!(!ended, "this control should show the leak the fix removes");
}
