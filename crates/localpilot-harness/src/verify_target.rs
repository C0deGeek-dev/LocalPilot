//! Workspace verification-target resolution for the verify-before-done gate.
//!
//! General stack detection lives in `localx_eval_core::verify`; this adapter
//! adds evidence-based Python selection and wraps the resolved command in the
//! gate's [`CheckConfig`] shape — a single phase check, never auto-fixed (the
//! model fixes via the loop, not a formatter) — so the quality-gate
//! [`crate::quality::CheckRunner`] can *run* it and there is no second command
//! engine.

use std::io::Read;
use std::path::Path;

use localpilot_config::{AutoFix, Cadence, CheckConfig};
use localx_eval_core::check::CheckCommand;

pub use localx_eval_core::verify::VERIFY_CHECK_NAME;

/// Resolve the verification command for `root`: the `override_cmd` (a single
/// command line, split on whitespace — no shell) when set and non-blank,
/// otherwise the stack-detected command, otherwise `None`.
#[must_use]
pub fn resolve_verify_check(root: &Path, override_cmd: Option<&str>) -> Option<CheckConfig> {
    if override_cmd.is_some() {
        return localx_eval_core::verify::resolve_verify_command(root, override_cmd)
            .map(verify_check);
    }
    detect_verify_command(root)
}

/// Detect a conventional verify command from `root`'s marker files, or `None`
/// when no supported stack is present. Marker files only — no execution.
#[must_use]
pub fn detect_verify_command(root: &Path) -> Option<CheckConfig> {
    let detected = localx_eval_core::verify::detect_verify_command(root);
    let core_python = detected.as_ref().is_some_and(is_pytest_command);
    if detected.is_some() && !core_python {
        return detected.map(verify_check);
    }
    let Some(explicit_pytest) = pytest_marker(root) else {
        // Unreadable/invalid configuration cannot establish that pytest is
        // absent. Retain the core's existing choice rather than switching to a
        // runner that may discover only a subset of the project's tests.
        return detected.map(verify_check);
    };
    if core_python || explicit_pytest || unittest_layout(root) {
        let program = detected.as_ref().map_or(
            if cfg!(windows) { "python" } else { "python3" },
            |command| command.program.as_str(),
        );
        return Some(verify_check(CheckCommand::new(
            program,
            vec![
                "-B".into(),
                "-c".into(),
                PYTHON_VERIFY_SCRIPT.into(),
                if explicit_pytest { "pytest" } else { "auto" }.into(),
            ],
        )));
    }
    None
}

/// Tokens that only mean something to a shell.
const SHELL_TOKENS: [&str; 8] = ["&&", "||", "|", ";", ">", ">>", "<", "2>&1"];

/// Why a plan's `verify:` command cannot run as written, or `None` when nothing
/// stands in its way.
///
/// The gate splits the line on whitespace and starts the first word as a program,
/// with the rest as plain arguments: no shell, so no pipes, `&&`, redirects,
/// quotes, backticks, shell builtins or PowerShell cmdlets. Nothing is run here;
/// the program is only looked up on `PATH`. A program named by a path is not
/// judged: a step may build it.
#[must_use]
pub fn unrunnable_verify_reason(command: &str) -> Option<String> {
    unrunnable_with(
        command,
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("PATHEXT").as_deref(),
    )
}

fn unrunnable_with(
    command: &str,
    path: Option<&std::ffi::OsStr>,
    pathext: Option<&std::ffi::OsStr>,
) -> Option<String> {
    let command = command.trim();
    let mut words = command.split_whitespace();
    let program = words.next()?;
    if command.starts_with('`') || command.ends_with('`') {
        return Some(
            "it is wrapped in backticks, which would be part of the command that is run; write it bare"
                .to_string(),
        );
    }
    let arguments: Vec<&str> = words.collect();
    if let Some(token) = arguments.iter().find(|word| SHELL_TOKENS.contains(word)) {
        return Some(format!(
            "it uses shell syntax ({token}), but the command is run directly, not through a shell"
        ));
    }
    if arguments
        .iter()
        .any(|word| word.starts_with('"') || word.starts_with('\''))
    {
        return Some(
            "it quotes an argument, but the command is split on spaces and the quote characters would reach the program"
                .to_string(),
        );
    }
    if program.contains(['/', '\\']) {
        return None;
    }
    (!program_on_path(program, path, pathext)).then(|| {
        format!(
            "{program} is not a program on this host (a shell builtin or cmdlet cannot be run directly)"
        )
    })
}

/// Whether `program` is an executable in a `PATH` directory, with the extensions
/// `PATHEXT` lists on Windows and the execute bit elsewhere.
fn program_on_path(
    program: &str,
    path: Option<&std::ffi::OsStr>,
    pathext: Option<&std::ffi::OsStr>,
) -> bool {
    let Some(path) = path else {
        return false;
    };
    #[cfg(windows)]
    {
        let extensions: Vec<String> = pathext
            .map_or_else(
                || ".COM;.EXE;.BAT;.CMD".to_string(),
                |value| value.to_string_lossy().into_owned(),
            )
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        let lower = program.to_ascii_lowercase();
        let has_extension = extensions
            .iter()
            .any(|extension| lower.ends_with(extension));
        std::env::split_paths(path).any(|dir| {
            if has_extension {
                return dir.join(program).is_file();
            }
            extensions
                .iter()
                .any(|extension| dir.join(format!("{program}{extension}")).is_file())
        })
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = pathext;
        std::env::split_paths(path).any(|dir| {
            std::fs::metadata(dir.join(program))
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
    }
}

fn is_pytest_command(command: &CheckCommand) -> bool {
    matches!(command.program.as_str(), "python" | "python3")
        && command.args.first().is_some_and(|arg| arg == "-m")
        && command.args.get(1).is_some_and(|arg| arg == "pytest")
}

// Detection never spawns Python. Module availability is decided by the same
// interpreter inside the permission-gated verification child. A configured
// pytest project must report missing dependencies rather than silently falling
// back to a runner that might discover none of its tests.
const PYTHON_VERIFY_SCRIPT: &str = r#"import importlib.util, runpy, sys
mode = sys.argv[1]
if mode == 'pytest' or importlib.util.find_spec('pytest') is not None:
    sys.argv = ['pytest']
    runpy.run_module('pytest', run_name='__main__')
else:
    import unittest
    suite = unittest.defaultTestLoader.discover('tests', pattern='test_*.py')
    if suite.countTestCases() == 0:
        print('verification failed: unittest discovered zero tests', file=sys.stderr)
        raise SystemExit(5)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(0 if result.wasSuccessful() else 1)
"#;

fn regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return false;
            }
        }
        metadata.is_file() && !metadata.file_type().is_symlink()
    })
}

fn pytest_marker(root: &Path) -> Option<bool> {
    if ["pytest.ini", ".pytest.ini", "conftest.py"]
        .iter()
        .any(|name| regular_file(&root.join(name)))
        || regular_directory(&root.join("tests")) && regular_file(&root.join("tests/conftest.py"))
    {
        return Some(true);
    }
    let path = root.join("pyproject.toml");
    if !regular_file(&path) {
        return match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
            _ => None,
        };
    }
    // A malformed or oversized marker is not evidence for a new runner choice.
    // Never interpret a comment or string containing a table heading as config.
    let Ok(file) = std::fs::File::open(path) else {
        return None;
    };
    let mut content = String::new();
    if file
        .take(64 * 1024 + 1)
        .read_to_string(&mut content)
        .is_err()
        || content.len() > 64 * 1024
    {
        return None;
    }
    content.parse::<toml::Value>().ok().map(|value| {
        value
            .get("tool")
            .and_then(|tool| tool.get("pytest"))
            .is_some_and(toml::Value::is_table)
    })
}

fn regular_directory(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    metadata.is_dir() && !metadata.file_type().is_symlink()
}

fn unittest_layout(root: &Path) -> bool {
    let tests = root.join("tests");
    if !regular_directory(&tests) {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(&tests) else {
        return false;
    };
    entries.take(4096).filter_map(Result::ok).any(|entry| {
        let name = entry.file_name();
        name.to_str()
            .is_some_and(|name| name.starts_with("test_") && name.ends_with(".py"))
            && regular_file(&entry.path())
    })
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

    #[test]
    fn python_layouts_preserve_override_and_stack_precedence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("tests")).unwrap();
        touch(dir.path(), "tests/test_sample.py");
        let check = detect_verify_command(dir.path()).unwrap();
        assert_eq!(
            check.program,
            if cfg!(windows) { "python" } else { "python3" }
        );
        assert_eq!(check.args.last().unwrap(), "auto");
        assert_eq!(
            localpilot_sandbox::classify(&check.program, &check.args),
            localpilot_sandbox::CommandClass::Unknown
        );
        assert_eq!(
            resolve_verify_check(dir.path(), Some("python -m custom_check"))
                .unwrap()
                .args,
            ["-m", "custom_check"]
        );
        assert!(resolve_verify_check(dir.path(), Some(" ")).is_none());
        touch(dir.path(), "Cargo.toml");
        assert_eq!(detect_verify_command(dir.path()).unwrap().program, "cargo");
    }

    #[test]
    fn core_pytest_detection_accepts_both_platform_interpreters() {
        for program in ["python", "python3"] {
            assert!(is_pytest_command(&CheckCommand::new(
                program,
                vec!["-m".into(), "pytest".into(), "-q".into()]
            )));
            assert!(!is_pytest_command(&CheckCommand::new(
                program,
                vec!["-m".into(), "unittest".into()]
            )));
        }
        assert!(!is_pytest_command(&CheckCommand::new(
            "other",
            vec!["-m".into(), "pytest".into()]
        )));
    }

    #[test]
    fn pytest_requires_configuration_evidence_not_a_table_name_in_text() {
        let dir = tempfile::tempdir().unwrap();
        for (contents, expected) in [
            ("[project]\nname='example'\n", "auto"),
            ("# [tool.pytest]\n[project]\nname='example'\n", "auto"),
            ("[tool.pytest.ini_options]\ntestpaths=['tests']\n", "pytest"),
            ("[tool.pytest]\ntestpaths=['tests']\n", "pytest"),
        ] {
            std::fs::write(dir.path().join("pyproject.toml"), contents).unwrap();
            assert_eq!(
                detect_verify_command(dir.path())
                    .unwrap()
                    .args
                    .last()
                    .unwrap(),
                expected
            );
        }
        for invalid in ["[invalid", &"x".repeat(64 * 1024 + 1)] {
            std::fs::write(dir.path().join("pyproject.toml"), invalid).unwrap();
            assert_eq!(
                detect_verify_command(dir.path()).unwrap().args,
                localx_eval_core::verify::detect_verify_command(dir.path())
                    .unwrap()
                    .args
            );
        }
        std::fs::remove_file(dir.path().join("pyproject.toml")).unwrap();
        assert!(detect_verify_command(dir.path()).is_none());
        touch(dir.path(), "pytest.ini");
        assert_eq!(
            detect_verify_command(dir.path())
                .unwrap()
                .args
                .last()
                .unwrap(),
            "pytest"
        );
    }

    // Real Python, with site packages disabled so ambient pytest cannot make
    // the standard-library and missing-dependency controls pass accidentally.
    fn run_python_check(root: &Path, mode: &str) -> std::process::Output {
        std::process::Command::new("python")
            .args(["-S", "-B", "-c", PYTHON_VERIFY_SCRIPT, mode])
            .env_remove("PYTHONPATH")
            .current_dir(root)
            .output()
            .unwrap()
    }

    #[test]
    fn actual_python_rejects_empty_import_failure_and_assertion_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("tests")).unwrap();
        let test = dir.path().join("tests/test_sample.py");
        std::fs::write(&test, "# no actual cases\n").unwrap();
        let empty = run_python_check(dir.path(), "auto");
        assert_eq!(empty.status.code(), Some(5));
        assert!(String::from_utf8_lossy(&empty.stderr).contains("zero tests"));
        std::fs::write(&test, "import nonexistent_fixture_dependency\n").unwrap();
        let import = run_python_check(dir.path(), "auto");
        assert_eq!(import.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&import.stderr).contains("nonexistent_fixture_dependency"));
        for (assertion, passed) in [("True", true), ("False", false)] {
            std::fs::write(&test, format!(
                "import unittest\nclass Example(unittest.TestCase):\n    def test_value(self):\n        self.assertTrue({assertion})\n"
            )).unwrap();
            assert_eq!(
                run_python_check(dir.path(), "auto").status.success(),
                passed
            );
        }
        let missing_pytest = run_python_check(dir.path(), "pytest");
        assert!(!missing_pytest.status.success());
        assert!(String::from_utf8_lossy(&missing_pytest.stderr).contains("pytest"));
        // Controlled module-selection/exit-code proof, not a pytest substitute:
        // a discoverable module must be used and its failure must be retained.
        std::fs::write(dir.path().join("pytest.py"),
            "from pathlib import Path\nPath('pytest-selected').write_text('selected')\nraise SystemExit(5)\n"
        ).unwrap();
        assert_eq!(run_python_check(dir.path(), "auto").status.code(), Some(5));
        assert!(dir.path().join("pytest-selected").is_file());
    }

    #[cfg(windows)]
    const FAKE_TOOL: &str = "fake-tool.exe";
    #[cfg(not(windows))]
    const FAKE_TOOL: &str = "fake-tool";

    /// A directory holding one program, as a `PATH` value.
    fn path_with_fake_tool() -> (tempfile::TempDir, std::ffi::OsString) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FAKE_TOOL);
        std::fs::write(&file, b"").unwrap();
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([dir.path()]).unwrap();
        (dir, path)
    }

    #[test]
    fn a_program_on_path_with_plain_arguments_is_runnable() {
        let (_dir, path) = path_with_fake_tool();
        let ext = std::ffi::OsString::from(".EXE;.CMD");
        for command in [
            "fake-tool",
            "fake-tool --flag value index.html",
            "  fake-tool test  ",
        ] {
            assert_eq!(
                unrunnable_with(command, Some(path.as_os_str()), Some(ext.as_os_str())),
                None,
                "{command}"
            );
        }
    }

    #[test]
    fn a_program_that_is_not_on_path_names_itself_and_why() {
        let (_dir, path) = path_with_fake_tool();
        let reason = unrunnable_with("open index.html", Some(path.as_os_str()), None).unwrap();
        assert!(
            reason.starts_with("open is not a program on this host"),
            "{reason}"
        );
        assert!(
            unrunnable_with("anything", None, None).is_some(),
            "no PATH at all"
        );
    }

    #[test]
    fn shell_syntax_quotes_and_backticks_are_refused_before_the_program_is_looked_up() {
        let (_dir, path) = path_with_fake_tool();
        let path = Some(path.as_os_str());
        let reason = unrunnable_with("fake-tool a && fake-tool b", path, None).unwrap();
        assert!(reason.contains("shell syntax (&&)"), "{reason}");
        let reason = unrunnable_with("fake-tool a | fake-tool", path, None).unwrap();
        assert!(reason.contains("shell syntax (|)"), "{reason}");
        let reason = unrunnable_with("fake-tool -q \"Hello\" index.html", path, None).unwrap();
        assert!(reason.contains("quotes an argument"), "{reason}");
        let reason = unrunnable_with("`fake-tool index.html`", path, None).unwrap();
        assert!(reason.contains("backticks"), "{reason}");
    }

    #[test]
    fn a_program_named_by_a_path_is_not_judged() {
        // A step may build it, so a path is left for the step to make true.
        assert_eq!(unrunnable_with("./gradlew test", None, None), None);
        assert_eq!(
            unrunnable_with("target\\debug\\app --check", None, None),
            None
        );
    }
}
