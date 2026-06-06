//! Built-in toolchain profiles.
//!
//! A profile is the fixed abstraction (ADR-0009): it declares how to detect a
//! stack and what checks that stack offers. The specific commands are profile
//! data, not literals scattered through the rule engine; discovery turns these
//! candidates into a concrete proposal by intersecting them with the tools that
//! are actually available.

use std::path::Path;

use localpilot_config::{AutoFix, Cadence, CheckConfig, RuleSeverity};

/// A language/toolchain profile: stack detection plus candidate checks.
pub trait ToolchainProfile: Send + Sync {
    /// The profile's stable name.
    fn name(&self) -> &'static str;
    /// Whether this stack is present at `root` (marker files only — no execution).
    fn detects(&self, root: &Path) -> bool;
    /// The checks this stack offers, before tool-availability filtering.
    fn candidate_checks(&self) -> Vec<CheckConfig>;
}

/// The built-in profiles, in priority order.
#[must_use]
pub fn builtin_profiles() -> Vec<Box<dyn ToolchainProfile>> {
    vec![Box::new(RustProfile), Box::new(PowerShellProfile)]
}

fn check(
    name: &str,
    program: &str,
    args: &[&str],
    cadence: Cadence,
    auto_fix: AutoFix,
) -> CheckConfig {
    CheckConfig {
        name: name.to_string(),
        program: program.to_string(),
        args: args.iter().map(|a| (*a).to_string()).collect(),
        fix_program: None,
        fix_args: Vec::new(),
        cadence,
        auto_fix,
        severity: None,
    }
}

fn fixable(mut check: CheckConfig, fix_program: &str, fix_args: &[&str]) -> CheckConfig {
    check.fix_program = Some(fix_program.to_string());
    check.fix_args = fix_args.iter().map(|a| (*a).to_string()).collect();
    check
}

/// A file marker exists in `root` (top level only).
fn has_file(root: &Path, name: &str) -> bool {
    root.join(name).is_file()
}

/// Any top-level file in `root` has one of `exts` (lowercased, without the dot).
fn has_extension(root: &Path, exts: &[&str]) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .path()
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| exts.contains(&ext.to_ascii_lowercase().as_str()))
    })
}

/// The Rust/Cargo profile.
pub struct RustProfile;

impl ToolchainProfile for RustProfile {
    fn name(&self) -> &'static str {
        "rust"
    }
    fn detects(&self, root: &Path) -> bool {
        has_file(root, "Cargo.toml")
    }
    fn candidate_checks(&self) -> Vec<CheckConfig> {
        let mut audit = check("audit", "cargo", &["audit"], Cadence::Phase, AutoFix::No);
        // Advisory findings need a human/dependency decision, not a code edit.
        audit.severity = Some(RuleSeverity::Block);
        vec![
            fixable(
                check(
                    "fmt",
                    "cargo",
                    &["fmt", "--check"],
                    Cadence::Step,
                    AutoFix::Full,
                ),
                "cargo",
                &["fmt"],
            ),
            fixable(
                check(
                    "clippy",
                    "cargo",
                    &[
                        "clippy",
                        "--workspace",
                        "--all-targets",
                        "--",
                        "-D",
                        "warnings",
                    ],
                    Cadence::Step,
                    AutoFix::Safe,
                ),
                "cargo",
                &["clippy", "--fix", "--allow-dirty", "--allow-staged"],
            ),
            check(
                "test",
                "cargo",
                &["test", "--workspace"],
                Cadence::Phase,
                AutoFix::No,
            ),
            check("deps", "cargo", &["machete"], Cadence::Phase, AutoFix::No),
            audit,
        ]
    }
}

/// The PowerShell profile (PSScriptAnalyzer), proving the abstraction is not
/// Rust-shaped.
pub struct PowerShellProfile;

impl ToolchainProfile for PowerShellProfile {
    fn name(&self) -> &'static str {
        "powershell"
    }
    fn detects(&self, root: &Path) -> bool {
        has_extension(root, &["ps1", "psm1", "psd1"])
    }
    fn candidate_checks(&self) -> Vec<CheckConfig> {
        vec![check(
            "script-analyzer",
            "pwsh",
            &[
                "-NoProfile",
                "-Command",
                "Invoke-ScriptAnalyzer -Path . -Recurse -Severity Warning,Error -EnableExit",
            ],
            Cadence::Phase,
            AutoFix::No,
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_profile_detects_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!RustProfile.detects(dir.path()));
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        assert!(RustProfile.detects(dir.path()));
    }

    #[test]
    fn rust_profile_offers_the_expected_checks() {
        let names: Vec<_> = RustProfile
            .candidate_checks()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["fmt", "clippy", "test", "deps", "audit"]);
    }

    #[test]
    fn rust_fmt_is_full_autofix_clippy_is_safe() {
        let checks = RustProfile.candidate_checks();
        let fmt = checks.iter().find(|c| c.name == "fmt").unwrap();
        assert_eq!(fmt.auto_fix, AutoFix::Full);
        assert_eq!(fmt.cadence, Cadence::Step);
        assert!(fmt.fix_program.is_some());
        let clippy = checks.iter().find(|c| c.name == "clippy").unwrap();
        assert_eq!(clippy.auto_fix, AutoFix::Safe);
        let audit = checks.iter().find(|c| c.name == "audit").unwrap();
        assert_eq!(audit.severity, Some(RuleSeverity::Block));
        assert_eq!(audit.cadence, Cadence::Phase);
    }

    #[test]
    fn powershell_profile_detects_script_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!PowerShellProfile.detects(dir.path()));
        std::fs::write(dir.path().join("build.ps1"), "Write-Output 1\n").unwrap();
        assert!(PowerShellProfile.detects(dir.path()));
    }
}
