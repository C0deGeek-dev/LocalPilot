//! Quality-gate discovery.
//!
//! Discovery detects the project's stacks, intersects each matching profile's
//! candidate checks with the tools actually available on `PATH`, and returns a
//! *proposal*. It executes nothing and writes nothing: ratification (a separate
//! step) is what turns a proposal into an active, permission-allowed gate.

use std::collections::HashSet;
use std::path::Path;

use localpilot_config::CheckConfig;
use localpilot_sandbox::{classify, CommandClass};

use super::profiles::{builtin_profiles, ToolchainProfile};

/// A check proposed by discovery, tagged with the risk class of its command so
/// ratification can surface destructive/privileged/network checks to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedCheck {
    /// The proposed check.
    pub check: CheckConfig,
    /// The risk class of `check.program` + `check.args`.
    pub class: CommandClass,
}

impl ProposedCheck {
    /// Whether the command's class warrants an explicit warning at ratification.
    #[must_use]
    pub fn needs_explicit_warning(&self) -> bool {
        matches!(
            self.class,
            CommandClass::Destructive | CommandClass::Privileged | CommandClass::Network
        )
    }
}

/// Detect the project's stacks and propose a quality gate: every matching
/// profile's candidate checks whose program is available on `PATH`, de-duplicated
/// by name. Pure discovery — nothing runs and nothing is written.
#[must_use]
pub fn propose_gate(root: &Path) -> Vec<ProposedCheck> {
    propose_gate_with(root, &builtin_profiles(), &program_on_path)
}

/// The testable core: `is_available` decides tool presence so tests need not
/// depend on the host's `PATH`.
fn propose_gate_with(
    root: &Path,
    profiles: &[Box<dyn ToolchainProfile>],
    is_available: &dyn Fn(&str) -> bool,
) -> Vec<ProposedCheck> {
    let mut proposed = Vec::new();
    let mut seen = HashSet::new();
    for profile in profiles {
        if !profile.detects(root) {
            continue;
        }
        for check in profile.candidate_checks() {
            if !is_available(&check.program) {
                continue;
            }
            if !seen.insert(check.name.clone()) {
                continue;
            }
            let class = classify(&check.program, &check.args);
            proposed.push(ProposedCheck { check, class });
        }
    }
    proposed
}

/// Whether `program` resolves on `PATH` without executing it. A path with
/// separators is checked directly; a bare name is searched across `PATH`,
/// honoring `PATHEXT` on Windows.
#[must_use]
pub fn program_on_path(program: &str) -> bool {
    let candidate = Path::new(program);
    if candidate.components().count() > 1 {
        return candidate.is_file();
    }
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    let extensions = path_extensions();
    std::env::split_paths(&path_var).any(|dir| {
        if dir.as_os_str().is_empty() {
            return false;
        }
        extensions.iter().any(|ext| {
            let name = if ext.is_empty() {
                program.to_string()
            } else {
                format!("{program}{ext}")
            };
            dir.join(name).is_file()
        })
    })
}

#[cfg(windows)]
fn path_extensions() -> Vec<String> {
    let mut extensions = vec![String::new()];
    let configured = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string());
    extensions.extend(
        configured
            .split(';')
            .map(|ext| ext.trim().to_ascii_lowercase())
            .filter(|ext| !ext.is_empty()),
    );
    extensions
}

#[cfg(not(windows))]
fn path_extensions() -> Vec<String> {
    vec![String::new()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quality::profiles::{PowerShellProfile, RustProfile, ToolchainProfile};

    fn profiles() -> Vec<Box<dyn ToolchainProfile>> {
        vec![Box::new(RustProfile), Box::new(PowerShellProfile)]
    }

    fn cargo_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        dir
    }

    #[test]
    fn proposes_nothing_when_no_stack_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let proposed = propose_gate_with(dir.path(), &profiles(), &|_| true);
        assert!(proposed.is_empty());
    }

    #[test]
    fn proposes_rust_checks_when_tools_are_available() {
        let dir = cargo_root();
        let proposed = propose_gate_with(dir.path(), &profiles(), &|p| p == "cargo");
        let names: Vec<_> = proposed.iter().map(|p| p.check.name.as_str()).collect();
        assert_eq!(names, ["fmt", "clippy", "test", "deps", "audit"]);
    }

    #[test]
    fn excludes_checks_whose_tool_is_absent() {
        let dir = cargo_root();
        let proposed = propose_gate_with(dir.path(), &profiles(), &|_| false);
        assert!(proposed.is_empty());
    }

    #[test]
    fn tags_each_proposed_check_with_its_command_class() {
        let dir = cargo_root();
        let proposed = propose_gate_with(dir.path(), &profiles(), &|p| p == "cargo");
        // cargo invocations are project-write, not a class that needs a warning.
        assert!(proposed
            .iter()
            .all(|p| p.class == CommandClass::ProjectWrite));
        assert!(proposed.iter().all(|p| !p.needs_explicit_warning()));
    }

    #[test]
    fn warning_classes_are_flagged_regardless_of_platform() {
        // `needs_explicit_warning` keys off the class only, so test it directly
        // rather than through a platform-specific command classification.
        let warns = |class| {
            ProposedCheck {
                check: CheckConfig {
                    name: "c".to_string(),
                    program: "p".to_string(),
                    args: Vec::new(),
                    fix_program: None,
                    fix_args: Vec::new(),
                    cadence: localpilot_config::Cadence::Phase,
                    auto_fix: localpilot_config::AutoFix::No,
                    severity: None,
                },
                class,
            }
            .needs_explicit_warning()
        };
        assert!(warns(CommandClass::Destructive));
        assert!(warns(CommandClass::Privileged));
        assert!(warns(CommandClass::Network));
        assert!(!warns(CommandClass::ProjectWrite));
        assert!(!warns(CommandClass::ReadOnly));
    }

    #[test]
    fn program_on_path_rejects_a_nonexistent_tool() {
        assert!(!program_on_path("definitely-not-a-real-tool-xyzzy-42"));
    }
}
