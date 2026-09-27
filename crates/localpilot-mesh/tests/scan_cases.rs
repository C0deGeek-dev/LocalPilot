//! The no-VCS scanner agrees with the reference on the boundary cases: links
//! recorded and never followed, unreadable paths refused by name, and
//! `.pairignore` pruning. Each case is built once by the vendored
//! `scan_cases.py`; this crate scans the tree and the reference scans the same
//! tree, and the rows and digest (or the refused path) must be identical.
//!
//! Needs Python 3.9+: `LOCALPILOT_CONFORMANCE_PYTHON`, else `python3`,
//! `python`, `py -3`. Without one this fails under `CI=true` and is skipped
//! with a notice elsewhere.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use localpilot_mesh::tree;
use serde_json::{json, Value};

fn suite() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance")
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

fn cases(py: &[String], args: &[&str]) -> String {
    let out = Command::new(&py[0])
        .args(&py[1..])
        .arg(suite().join("scan_cases.py"))
        .args(args)
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("run scan_cases.py");
    assert!(
        out.status.success(),
        "scan_cases.py {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 output")
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// This crate's outcome in the reference's JSON shape: each path as the hex
/// of its exact encoding.
fn native(root: &Path) -> Value {
    let pats = tree::pairignore(root).expect("read .pairignore");
    match tree::scan_rows(root, &pats) {
        Ok(rows) => json!({
            "ok": true,
            "rows": rows.iter().map(|r| json!([hex(&r.raw), r.kind.to_string(), r.size, r.sha256])).collect::<Vec<_>>(),
            "digest": tree::tree_digest(&rows),
        }),
        Err(e) => json!({"ok": false, "path": e.path}),
    }
}

#[test]
fn the_scanner_matches_the_reference_on_every_boundary_case() {
    let Some(py) = python() else {
        let msg = "no Python 3.9+ found (set LOCALPILOT_CONFORMANCE_PYTHON); the scanner boundary cases did not run";
        assert!(std::env::var("CI").as_deref() != Ok("true"), "{msg}");
        eprintln!("NOTICE: {msg}");
        return;
    };
    let names: Vec<String> = cases(&py, &["list"]).lines().map(str::to_owned).collect();
    assert!(
        names.iter().any(|n| n == "link-to-outside-canary"),
        "the link case runs on every platform: {names:?}"
    );
    for name in &names {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("case");
        let base_s = base.to_string_lossy().into_owned();
        cases(&py, &["build", name, &base_s]);
        let want: Value = serde_json::from_str(&cases(&py, &["expect", name, &base_s])).unwrap();
        let got = native(&base.join("tree"));
        cases(&py, &["restore", &base_s]);
        assert_eq!(got, want, "case {name}: native differs from the reference");
    }
}
