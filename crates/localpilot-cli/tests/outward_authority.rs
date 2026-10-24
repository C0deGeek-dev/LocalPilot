//! Actual host seam with a compiled local stand-in for gh. No network is used.
#![allow(clippy::unwrap_used)]

use localpilot_patchgen::validation::{AuthorityPurpose, RepositoryAuthority, ValidationError};
use localpilot_patchgen::{
    ChangeProvenance, DraftRequest, OutwardDraft, OutwardKind, OutwardPolicy,
};
use std::path::Path;
use std::process::Command;

fn fixture(root: &Path) {
    assert!(Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(root)
        .output()
        .unwrap()
        .status
        .success());
    std::fs::write(
        root.join(".localpilot.toml"),
        "[self_improvement]\nenabled=true\noutward_targets=['fixture/repository']\n",
    )
    .unwrap();
    OutwardDraft::new(
        DraftRequest {
            id: "reviewed-draft".into(),
            kind: OutwardKind::Issue,
            target_repo: "fixture/repository".into(),
            title: "fixture title".into(),
            description: "fixture body".into(),
            head_branch: None,
        },
        &ChangeProvenance::new("fixture", "scripted", "publication authority"),
        &OutwardPolicy::new(true, vec!["fixture/repository".into()]),
    )
    .unwrap()
    .persist(root)
    .unwrap();
}

#[test]
fn publication_holds_distinct_authority_and_ambiguous_effects_block_fresh_attempts() {
    let host = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let source = host.path().join("stand_in.rs");
    std::fs::write(&source, r#"
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--version") {
        println!("fixture gh");
    } else if args.first().map(String::as_str) == Some("auth") {
        assert_eq!(args, ["auth", "status", "--active", "--hostname", "github.com", "--json", "hosts"]);
        println!("{{\"hosts\":{{\"github.com\":[{{\"host\":\"github.com\",\"state\":\"success\",\"active\":true,\"login\":\"fixture-human\"}}]}}}}");
    } else {
        assert_eq!(&args[..2], &["issue", "create"]);
        assert_eq!(std::env::var("GH_HOST").unwrap(), "github.com");
        assert_eq!(std::env::var("GH_PROMPT_DISABLED").unwrap(), "1");
        use std::io::Write;
        let mut marker = std::fs::OpenOptions::new().create(true).append(true).open(std::env::var_os("FIXTURE_MARKER").unwrap()).unwrap();
        writeln!(marker, "publication effect").unwrap();
        marker.sync_all().unwrap();
        if std::env::var_os("FIXTURE_AMBIGUOUS").is_some() { std::process::exit(1); }
        if std::env::var_os("FIXTURE_MISSING_URL").is_some() { return; }
        println!("https://github.com/fixture/repository/issues/1");
    }
}
"#).unwrap();
    let program = host
        .path()
        .join(if cfg!(windows) { "gh.exe" } else { "gh" });
    let compiled = Command::new("rustc")
        .arg("--edition=2021")
        .arg(&source)
        .arg("-o")
        .arg(&program)
        .output()
        .unwrap();
    assert!(compiled.status.success(), "{compiled:?}");
    let mut paths = vec![host.path().to_path_buf()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(paths).unwrap();

    for mode in ["success", "failed", "missing-url"] {
        let ambiguous = mode != "success";
        let repo = tempfile::tempdir().unwrap();
        fixture(repo.path());
        let marker = repo.path().join("publication.marker");
        let run = |approved: bool| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_localpilot"));
            command.args([
                "self-review",
                "emit-draft",
                "--id",
                "reviewed-draft",
                "--reviewer",
                "fixture-human",
            ]);
            if approved {
                command.arg("--approve");
            }
            if mode == "failed" {
                command.env("FIXTURE_AMBIGUOUS", "1");
            }
            if mode == "missing-url" {
                command.env("FIXTURE_MISSING_URL", "1");
            }
            command
                .env("FIXTURE_MARKER", &marker)
                .env("PATH", &path)
                .env("GH_HOST", "github.com")
                .env("APPDATA", config.path())
                .env("XDG_CONFIG_HOME", config.path())
                .current_dir(repo.path())
                .output()
                .unwrap()
        };
        let preview = run(false);
        assert!(preview.status.success(), "{preview:?}");
        assert!(!marker.exists());
        let result = run(true);
        assert_eq!(result.status.success(), !ambiguous, "{result:?}");
        assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        {
            let tx = authority.try_transaction().unwrap();
            let event = tx.events().next().unwrap();
            assert_eq!(
                event.reservation.purpose,
                AuthorityPurpose::OutwardPublication
            );
            assert_eq!(event.completed, !ambiguous);
            assert_eq!(
                event.publication.as_ref().unwrap().account,
                "github.com/fixture-human"
            );
            if ambiguous {
                assert!(matches!(
                    tx.require_idle(),
                    Err(ValidationError::RecoveryRequired)
                ));
            }
        }
        if ambiguous {
            let retry = run(true);
            assert!(!retry.status.success());
            assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
        }
    }
}
