//! End-to-end exercise of the permission boundary through the crate's public
//! API only: a scripted approval run, destructive command denial across all
//! three shells, and workspace path-escape attempts (`..`, symlinks, UNC
//! verbatim prefixes, case collisions, alternate data streams).

use localpilot_sandbox::{
    classify_posix, classify_windows, Approver, CommandClass, Decision, Effect, Interactivity,
    PermissionEngine, PermissionRequest, Profile, ScriptedApprover, Workspace,
};
use std::fs;
use std::path::Path;

fn request(effect: Effect, interactivity: Interactivity, trusted: bool) -> PermissionRequest {
    PermissionRequest {
        tool: "run_shell".to_string(),
        effect,
        interactivity,
        trusted,
        detail: String::new(),
    }
}

#[test]
fn scripted_approval_run_resolves_each_ask_in_order() {
    let engine = PermissionEngine::new(Profile::Default, Vec::new());
    let approver = ScriptedApprover::new(vec![true, false]);

    // An untrusted workspace write prompts; the script approves the first
    // prompt, rejects the second, then the exhausted approver denies.
    let write = request(
        Effect::WritePath {
            inside_workspace: true,
            overwrite: false,
        },
        Interactivity::Interactive,
        false,
    );
    assert_eq!(engine.decide(&write), Decision::Ask);

    let outcomes: Vec<bool> = (0..3)
        .map(|_| futures::executor::block_on(approver.approve(&write)))
        .collect();
    assert_eq!(outcomes, vec![true, false, false]);
}

#[test]
fn destructive_commands_are_never_runnable_without_a_human_on_any_shell() {
    let destructive_per_shell = [
        // POSIX sh
        classify_posix("rm", &["-rf".to_string(), "build".to_string()]),
        // cmd.exe
        classify_windows("cmd", &["/c".to_string(), "rd /s /q build".to_string()]),
        // PowerShell
        classify_windows(
            "pwsh",
            &[
                "-Command".to_string(),
                "Remove-Item -Recurse -Force build".to_string(),
            ],
        ),
    ];

    for (index, class) in destructive_per_shell.into_iter().enumerate() {
        // POSIX `rm -rf` is classified destructive; an inline `cmd /c …` /
        // `pwsh -Command …` is opaque to substring parsing and classified
        // Unknown. Both are human-gated (never auto-allowed) — the property this
        // test pins, proven by the permission decisions below.
        assert!(
            matches!(class, CommandClass::Destructive | CommandClass::Unknown),
            "shell case {index} was auto-allowable: {class:?}"
        );

        // Non-interactive: denied outright, on every profile except bypass —
        // and even an allowlisted tool must not relax it.
        for profile in [Profile::Default, Profile::Relaxed] {
            let engine = PermissionEngine::new(profile, vec!["run_shell".to_string()]);
            let denied = engine.decide(&request(
                Effect::RunCommand(class),
                Interactivity::NonInteractive,
                true,
            ));
            assert_eq!(denied, Decision::Deny, "profile {profile:?} failed to deny");

            // Interactive: the human gate stays in place.
            let asked = engine.decide(&request(
                Effect::RunCommand(class),
                Interactivity::Interactive,
                true,
            ));
            assert_eq!(
                asked,
                Decision::Ask,
                "profile {profile:?} skipped the prompt"
            );
        }
    }
}

#[test]
fn opaque_shell_wrappers_cannot_smuggle_destructive_commands_as_read_only() {
    let wrapped = [
        classify_posix("bash", &["-c".to_string(), "rm -rf /".to_string()]),
        classify_posix(
            "env",
            &["rm".to_string(), "-rf".to_string(), "/".to_string()],
        ),
        classify_windows("bash", &["-c".to_string(), "rm -rf /".to_string()]),
    ];
    for class in wrapped {
        assert!(
            !matches!(class, CommandClass::ReadOnly | CommandClass::ProjectWrite),
            "wrapper classified as benign: {class:?}"
        );
    }
}

#[test]
fn dot_dot_paths_cannot_escape_the_workspace() -> Result<(), Box<dyn std::error::Error>> {
    let outer = tempfile::tempdir()?;
    let root = outer.path().join("workspace");
    fs::create_dir(&root)?;
    fs::write(outer.path().join("secret.txt"), "outside")?;
    let workspace = Workspace::new(&root)?;

    assert!(workspace.resolve(Path::new("../secret.txt")).is_err());
    assert!(workspace
        .resolve(Path::new("nested/../../secret.txt"))
        .is_err());
    assert!(!workspace.contains(Path::new("../secret.txt")));

    // Inside paths still resolve, including through dot-dot that stays inside.
    fs::create_dir(root.join("src"))?;
    fs::write(root.join("inside.txt"), "inside")?;
    assert!(workspace.resolve(Path::new("src/../inside.txt")).is_ok());
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlinks_pointing_outside_are_caught() -> Result<(), Box<dyn std::error::Error>> {
    let outer = tempfile::tempdir()?;
    let root = outer.path().join("workspace");
    fs::create_dir(&root)?;
    let target = outer.path().join("outside-dir");
    fs::create_dir(&target)?;
    fs::write(target.join("data.txt"), "outside")?;
    std::os::unix::fs::symlink(&target, root.join("escape"))?;

    let workspace = Workspace::new(&root)?;
    assert!(workspace.resolve(Path::new("escape/data.txt")).is_err());
    assert!(!workspace.contains(Path::new("escape/data.txt")));
    Ok(())
}

#[cfg(windows)]
#[test]
fn unc_verbatim_and_case_variants_resolve_consistently() -> Result<(), Box<dyn std::error::Error>> {
    let outer = tempfile::tempdir()?;
    let root = outer.path().join("Workspace");
    fs::create_dir(&root)?;
    fs::write(root.join("inside.txt"), "inside")?;
    let workspace = Workspace::new(&root)?;

    // A verbatim (\\?\) absolute form of an inside path is still inside.
    let verbatim = format!(r"\\?\{}", root.join("inside.txt").display());
    assert!(
        workspace.contains(Path::new(&verbatim)),
        "verbatim form of an inside path was rejected"
    );

    // Case-variant spelling of the same inside path is still inside
    // (NTFS is case-insensitive; a naive string compare would split these).
    let upper = root.join("INSIDE.TXT");
    let lower_root = root.display().to_string().replace("Workspace", "WORKSPACE");
    assert!(workspace.contains(&upper));
    assert!(
        workspace.contains(Path::new(&format!(r"{lower_root}\inside.txt"))),
        "case-variant root spelling was rejected"
    );

    // A verbatim path to a sibling outside the root stays outside.
    let outside = format!(r"\\?\{}", outer.path().join("other.txt").display());
    fs::write(outer.path().join("other.txt"), "outside")?;
    assert!(!workspace.contains(Path::new(&outside)));
    Ok(())
}

#[cfg(windows)]
#[test]
fn alternate_data_streams_do_not_bypass_containment() -> Result<(), Box<dyn std::error::Error>> {
    let outer = tempfile::tempdir()?;
    let root = outer.path().join("workspace");
    fs::create_dir(&root)?;
    fs::write(root.join("inside.txt"), "inside")?;
    let workspace = Workspace::new(&root)?;

    // An ADS suffix on an inside file must not resolve to an outside path —
    // either it resolves inside or it errors; both keep containment.
    let ads = root.join("inside.txt:stream");
    if let Ok(resolved) = workspace.resolve(&ads) {
        assert!(resolved.starts_with(workspace.root()));
    }
    Ok(())
}

#[test]
fn bypass_keeps_the_workspace_boundary_for_path_effects() {
    let engine = PermissionEngine::new(Profile::Bypass, Vec::new());

    let outside_write = request(
        Effect::WritePath {
            inside_workspace: false,
            overwrite: true,
        },
        Interactivity::NonInteractive,
        true,
    );
    assert_eq!(engine.decide(&outside_write), Decision::Deny);

    let inside_write = request(
        Effect::WritePath {
            inside_workspace: true,
            overwrite: true,
        },
        Interactivity::NonInteractive,
        true,
    );
    assert_eq!(engine.decide(&inside_write), Decision::Allow);
}
