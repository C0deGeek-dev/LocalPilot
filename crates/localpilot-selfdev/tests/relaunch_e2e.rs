//! The one genuinely OS-divergent step of a reload — the relaunch syscall — run
//! for real on whatever platform the tests run on.
//!
//! `relaunch` on Unix `exec`s (replacing the process), which would take the test
//! harness with it; on Windows it spawns and the caller exits. So this test
//! *re-executes itself*: the parent spawns the test binary again with a marker
//! env var and asserts the child exits with a known code; the child, seeing the
//! marker, calls `relaunch` onto a harmless command that exits with that code.
//! Either path — `exec` replacing the child, or spawn-then-propagate — must
//! deliver the code back to the parent. No cargo build, so it runs in the normal
//! suite rather than behind `--ignored`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::Command;

use localpilot_selfdev::{relaunch, relaunch_plan};

/// The marker that tells a re-executed test binary to be the child helper.
const HELPER_ENV: &str = "SELFDEV_RELAUNCH_HELPER";
/// The exit code the harmless successor carries back through the relaunch.
const SENTINEL_CODE: i32 = 7;

/// A harmless command that exits with [`SENTINEL_CODE`] on this platform.
fn harmless_exit(code: i32) -> (PathBuf, Vec<String>) {
    if cfg!(windows) {
        (
            PathBuf::from("cmd"),
            vec!["/c".to_string(), format!("exit {code}")],
        )
    } else {
        (
            PathBuf::from("sh"),
            vec!["-c".to_string(), format!("exit {code}")],
        )
    }
}

#[test]
fn relaunch_really_swaps_the_process_onto_the_target() {
    if std::env::var(HELPER_ENV).is_ok() {
        // --- child: perform the real relaunch ---
        let (program, args) = harmless_exit(SENTINEL_CODE);
        let plan = relaunch_plan(&program, &args);
        match relaunch(&plan) {
            // Windows: the successor was spawned; wait for it and exit with its
            // code, completing the swap the caller is responsible for.
            Ok(mut child) => {
                let code = child
                    .wait()
                    .expect("wait for successor")
                    .code()
                    .unwrap_or(0);
                std::process::exit(code);
            }
            // Unix reaches here only on failure — `exec` does not return on
            // success. A returned error means the swap itself failed.
            Err(error) => {
                eprintln!("relaunch failed: {error}");
                std::process::exit(101);
            }
        }
        // Unix success replaces this process before here; unreachable.
    }

    // --- parent: re-execute this exact test as the child helper ---
    let exe = std::env::current_exe().expect("test binary path");
    let status = Command::new(exe)
        .args([
            "relaunch_really_swaps_the_process_onto_the_target",
            "--exact",
        ])
        .env(HELPER_ENV, "1")
        .status()
        .expect("spawn the helper");

    assert_eq!(
        status.code(),
        Some(SENTINEL_CODE),
        "the relaunched process must carry the successor's exit code back — proving \
         the real exec/spawn swap ran, not a stub"
    );
}
