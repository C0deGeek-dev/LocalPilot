//! Workspace verification-target detection for the verify-before-done gate.
//!
//! Given a workspace root, resolve the command that answers "does this code
//! build / do its tests pass?" — the signal the gate runs before a turn is
//! allowed to finalize. Resolution is: an explicit `[harness] verify_command`
//! override wins; otherwise a conventional command is detected from the stack's
//! marker files; otherwise `None` (no detectable target, so the gate is a no-op
//! and the turn finalizes unchanged).
//!
//! This is detection only — it reuses [`CheckConfig`] and the quality-gate
//! [`crate::quality::CheckRunner`] to *run* the command, so there is no second
//! command engine. It deliberately covers a broader language set than the
//! quality-gate toolchain profiles (which target the dev's own Rust/PowerShell
//! gate): the verify gate runs against arbitrary solve workspaces, where the
//! biggest convergence lever is catching a C++/Rust/Go/JS build failure before
//! the loop "submits" code it never compiled.

use std::path::Path;

use localpilot_config::{AutoFix, Cadence, CheckConfig};

/// The check name the verify gate presents. Stable so a scorecard/handoff reader
/// can find the verify outcome among gate outcomes.
pub const VERIFY_CHECK_NAME: &str = "verify";

/// Resolve the verification command for `root`: the `override_cmd` (a single
/// command line, split on whitespace — no shell) when set and non-blank,
/// otherwise the stack-detected command, otherwise `None`.
#[must_use]
pub fn resolve_verify_check(root: &Path, override_cmd: Option<&str>) -> Option<CheckConfig> {
    if let Some(command) = override_cmd {
        if let Some(check) = from_command_line(command) {
            return Some(check);
        }
    }
    detect_verify_command(root)
}

/// Build a verify check from an explicit command line, splitting on whitespace
/// into a program and arguments (no shell interpretation, matching how
/// `test_command` is handled). Returns `None` for a blank command.
#[must_use]
pub fn from_command_line(command: &str) -> Option<CheckConfig> {
    let mut parts = command.split_whitespace();
    let program = parts.next()?.to_string();
    let args = parts.map(str::to_string).collect();
    Some(verify_check(program, args))
}

/// Detect a conventional verify command from `root`'s marker files, or `None`
/// when no supported stack is present. Marker files only — no execution. The
/// first matching stack in priority order wins.
#[must_use]
pub fn detect_verify_command(root: &Path) -> Option<CheckConfig> {
    // Priority order: a language-native test command is preferred over a generic
    // `make`, so a project that carries both is verified by its real test suite.
    if has_file(root, "Cargo.toml") {
        return Some(verify_check(cargo(), vec_of(&["test"])));
    }
    if has_file(root, "go.mod") {
        return Some(verify_check(go(), vec_of(&["test", "./..."])));
    }
    if has_file(root, "pom.xml") {
        return Some(verify_check(maven(), vec_of(&["-q", "test"])));
    }
    if has_file(root, "build.gradle") || has_file(root, "build.gradle.kts") {
        return Some(verify_check(gradle(), vec_of(&["test", "--console=plain"])));
    }
    if has_file(root, "package.json") {
        return Some(verify_check(npm(), vec_of(&["test"])));
    }
    if is_python(root) {
        return Some(verify_check(python(), vec_of(&["-m", "pytest", "-q"])));
    }
    // C/C++ without a language test runner: a top-level Makefile is the most
    // portable single-command build. A CMake-only project needs a configure +
    // build pair that does not fit one program+args call, so it is left to an
    // explicit `verify_command` override (documented).
    if has_file(root, "Makefile") || has_file(root, "makefile") {
        return Some(verify_check(make(), Vec::new()));
    }
    None
}

/// A verify [`CheckConfig`]: a single phase check, never auto-fixed (the model
/// fixes via the loop, not a formatter).
fn verify_check(program: String, args: Vec<String>) -> CheckConfig {
    CheckConfig {
        name: VERIFY_CHECK_NAME.to_string(),
        program,
        args,
        fix_program: None,
        fix_args: Vec::new(),
        cadence: Cadence::Phase,
        auto_fix: AutoFix::No,
        severity: None,
    }
}

fn vec_of(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_string()).collect()
}

fn has_file(root: &Path, name: &str) -> bool {
    root.join(name).is_file()
}

/// A Python project: a build/test marker file, or any top-level `test_*.py` /
/// `*_test.py`.
fn is_python(root: &Path) -> bool {
    const MARKERS: &[&str] = &[
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "requirements.txt",
        "tox.ini",
        "pytest.ini",
        "conftest.py",
    ];
    if MARKERS.iter().any(|m| has_file(root, m)) {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry.file_name().to_str().is_some_and(|name| {
            name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
        })
    })
}

// Cross-platform program names: a Windows shim is invoked through its `.cmd`
// launcher (Command::new spawns the program directly, no shell), while `cargo`,
// `go`, and `make` resolve the same on every tier-1 platform.
fn cargo() -> String {
    "cargo".to_string()
}
fn go() -> String {
    "go".to_string()
}
fn make() -> String {
    "make".to_string()
}
#[cfg(windows)]
fn npm() -> String {
    "npm.cmd".to_string()
}
#[cfg(not(windows))]
fn npm() -> String {
    "npm".to_string()
}
#[cfg(windows)]
fn maven() -> String {
    "mvn.cmd".to_string()
}
#[cfg(not(windows))]
fn maven() -> String {
    "mvn".to_string()
}
#[cfg(windows)]
fn gradle() -> String {
    "gradle.bat".to_string()
}
#[cfg(not(windows))]
fn gradle() -> String {
    "gradle".to_string()
}
#[cfg(windows)]
fn python() -> String {
    "python".to_string()
}
#[cfg(not(windows))]
fn python() -> String {
    "python3".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, name: &str) {
        std::fs::write(root.join(name), "x").unwrap();
    }

    #[test]
    fn no_target_when_workspace_is_bare() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_verify_command(dir.path()).is_none());
    }

    #[test]
    fn detects_each_stack() {
        // (marker file, expected program, expected first arg)
        let cases: &[(&str, &str, &str)] =
            &[("Cargo.toml", "cargo", "test"), ("go.mod", "go", "test")];
        for (marker, program, first_arg) in cases {
            let dir = tempfile::tempdir().unwrap();
            touch(dir.path(), marker);
            let check = detect_verify_command(dir.path()).expect("a target");
            assert_eq!(check.name, VERIFY_CHECK_NAME);
            assert_eq!(check.program, *program, "program for {marker}");
            assert_eq!(check.args.first().map(String::as_str), Some(*first_arg));
            assert_eq!(check.auto_fix, AutoFix::No);
        }
    }

    #[test]
    fn rust_beats_a_generic_makefile() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "Makefile");
        touch(dir.path(), "Cargo.toml");
        let check = detect_verify_command(dir.path()).unwrap();
        assert_eq!(check.program, "cargo");
    }

    #[test]
    fn makefile_only_is_make() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "Makefile");
        assert_eq!(detect_verify_command(dir.path()).unwrap().program, "make");
    }

    #[test]
    fn detects_python_by_test_file() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "solution_test.py");
        let check = detect_verify_command(dir.path()).expect("python target");
        assert_eq!(check.args, vec_of(&["-m", "pytest", "-q"]));
    }

    #[test]
    fn override_wins_over_detection() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "Cargo.toml");
        let check = resolve_verify_check(dir.path(), Some("ctest --output-on-failure")).unwrap();
        assert_eq!(check.program, "ctest");
        assert_eq!(check.args, vec_of(&["--output-on-failure"]));
    }

    #[test]
    fn blank_override_falls_back_to_detection() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "Cargo.toml");
        let check = resolve_verify_check(dir.path(), Some("   ")).unwrap();
        assert_eq!(check.program, "cargo");
    }

    #[test]
    fn override_resolves_with_no_detected_target() {
        let dir = tempfile::tempdir().unwrap();
        let check = resolve_verify_check(dir.path(), Some("bash run-tests.sh")).unwrap();
        assert_eq!(check.program, "bash");
        assert_eq!(check.args, vec_of(&["run-tests.sh"]));
    }
}
