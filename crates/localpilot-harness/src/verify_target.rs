//! Workspace verification-target resolution for the verify-before-done gate.
//!
//! Detection (marker files → conventional command, override wins) lives in
//! `localx_eval_core::verify`; this module wraps the resolved command in the
//! gate's [`CheckConfig`] shape — a single phase check, never auto-fixed (the
//! model fixes via the loop, not a formatter) — so the quality-gate
//! [`crate::quality::CheckRunner`] can *run* it and there is no second command
//! engine.

use std::path::Path;

use localpilot_config::{AutoFix, Cadence, CheckConfig};
use localx_eval_core::check::CheckCommand;

pub use localx_eval_core::verify::VERIFY_CHECK_NAME;

/// Resolve the verification command for `root`: the `override_cmd` (a single
/// command line, split on whitespace — no shell) when set and non-blank,
/// otherwise the stack-detected command, otherwise `None`.
#[must_use]
pub fn resolve_verify_check(root: &Path, override_cmd: Option<&str>) -> Option<CheckConfig> {
    localx_eval_core::verify::resolve_verify_command(root, override_cmd).map(verify_check)
}

/// Detect a conventional verify command from `root`'s marker files, or `None`
/// when no supported stack is present. Marker files only — no execution.
#[must_use]
pub fn detect_verify_command(root: &Path) -> Option<CheckConfig> {
    localx_eval_core::verify::detect_verify_command(root).map(verify_check)
}

/// Wrap a resolved command as the verify [`CheckConfig`]: a single phase check,
/// never auto-fixed.
fn verify_check(command: CheckCommand) -> CheckConfig {
    CheckConfig {
        name: VERIFY_CHECK_NAME.to_string(),
        program: command.program,
        args: command.args,
        fix_program: None,
        fix_args: Vec::new(),
        cadence: Cadence::Phase,
        auto_fix: AutoFix::No,
        severity: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, name: &str) {
        std::fs::write(root.join(name), "x").unwrap();
    }

    #[test]
    fn detection_wraps_the_command_as_a_phase_check_with_no_autofix() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "Cargo.toml");
        let check = detect_verify_command(dir.path()).expect("a target");
        assert_eq!(check.name, VERIFY_CHECK_NAME);
        assert_eq!(check.program, "cargo");
        assert_eq!(check.args.first().map(String::as_str), Some("test"));
        assert_eq!(check.cadence, Cadence::Phase);
        assert_eq!(check.auto_fix, AutoFix::No);
        assert!(check.fix_program.is_none());
    }

    #[test]
    fn override_wins_over_detection() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "Cargo.toml");
        let check = resolve_verify_check(dir.path(), Some("ctest --output-on-failure")).unwrap();
        assert_eq!(check.program, "ctest");
        assert_eq!(check.args, vec!["--output-on-failure".to_string()]);
    }

    #[test]
    fn no_target_when_workspace_is_bare() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_verify_command(dir.path()).is_none());
        assert!(resolve_verify_check(dir.path(), Some("   ")).is_none());
    }
}
