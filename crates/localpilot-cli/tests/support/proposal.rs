//! Real disposable proposal material for interactive host tests. No model or
//! provider configuration is needed; checks remain registered Cargo commands.

#![allow(clippy::unwrap_used)]

use localpilot_patchgen::{ChangeProvenance, PatchProposal, ProposedEdit, ProposedPatch};
use std::path::Path;
use std::process::Command;

pub(crate) struct Fixture {
    pub(crate) root: tempfile::TempDir,
    pub(crate) patch: ProposedPatch,
    pub(crate) base: String,
}

pub(crate) fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

pub(crate) fn fixture(code: &str) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-b", "main"]);
    git(root.path(), &["config", "user.name", "fixture"]);
    git(root.path(), &["config", "user.email", "fixture@localhost"]);
    git(root.path(), &["config", "core.autocrlf", "false"]);
    let toolchain = std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_else(|_| "1.82".into());
    assert!(toolchain
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')));
    std::fs::create_dir(root.path().join("src")).unwrap();
    for (name, content) in [
        (".gitignore", ".localpilot/\ntarget/\n".to_owned()),
        ("Cargo.toml", "[package]\nname='interactive_validation_fixture'\nversion='0.0.0'\nedition='2021'\n".to_owned()),
        ("Cargo.lock", "version = 3\n[[package]]\nname = \"interactive_validation_fixture\"\nversion = \"0.0.0\"\n".to_owned()),
        ("src/lib.rs", "#[test] fn baseline() {}\n".to_owned()),
        ("rust-toolchain.toml", format!("[toolchain]\nchannel='{toolchain}'\n")),
        (".localpilot.toml", "[permissions]\nprofile='bypass'\n[[harness.checks]]\nname='test'\nprogram='cargo'\nargs=['test','--workspace']\n".to_owned()),
    ] { std::fs::write(root.path().join(name), content).unwrap(); }
    git(root.path(), &["add", "-f", "."]);
    git(
        root.path(),
        &["commit", "-m", "controlled interactive validation fixture"],
    );
    let base = git(root.path(), &["rev-parse", "HEAD"]);
    let patch = localpilot_patchgen::propose(
        root.path(),
        "interactive-reviewed",
        &PatchProposal::new(
            "controlled host validation",
            vec!["src/lib.rs".into()],
            vec![ProposedEdit::new("src/lib.rs", code)],
        ),
        ChangeProvenance::new("fixture", "scripted", "interactive host policy"),
    )
    .unwrap();
    let id = patch.id().to_owned();
    patch.persist().unwrap();
    std::fs::create_dir_all(root.path().join(".localpilot/selfimprove")).unwrap();
    std::fs::write(
        root.path().join(".localpilot/selfimprove/state.json"),
        format!("{{\"stage\":\"proposed\",\"proposal_id\":\"{id}\"}}"),
    )
    .unwrap();
    let patch = ProposedPatch::reopen(root.path(), &id).unwrap();
    Fixture { root, patch, base }
}
