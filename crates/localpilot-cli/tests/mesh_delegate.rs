//! The native-writer kill switch: with `[mesh] writer = "delegate"`,
//! `localpilot mesh` hands each participant operation to the configured
//! delegate unchanged and returns its exit code, never falling back to the
//! native writer. The delegate here is the vendored reference behind a
//! wrapper that marks each call, and both it and the anchor live under paths
//! with spaces.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use support::{native, python_or_skip, suite, tool};

const MARK: &str = "DELEGATED-BY-TEST";

struct Fixture {
    _dir: tempfile::TempDir,
    anchor: PathBuf,
    /// The delegate argv as a TOML/JSON array for the environment layer.
    delegate: String,
    py: Vec<String>,
}

fn json_array(items: &[String]) -> String {
    serde_json::to_string(items).unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn fixture() -> Option<Fixture> {
    let py = python_or_skip("the mesh delegate test")?;
    let dir = tempfile::tempdir().unwrap();
    let tools = dir.path().join("dir with space");
    std::fs::create_dir_all(&tools).unwrap();
    let wrapper = tools.join("delegate wrapper.py");
    let reference = suite().join("reference").join("pair.py");
    std::fs::write(
        &wrapper,
        format!(
            "import subprocess, sys\nsys.stderr.write({MARK:?} + '\\n'); sys.stderr.flush()\n\
             sys.exit(subprocess.run([sys.executable, {reference:?}, *sys.argv[1:]]).returncode)\n",
            reference = reference.to_string_lossy()
        ),
    )
    .unwrap();
    let anchor = dir.path().join("anchor with space");
    std::fs::create_dir_all(&anchor).unwrap();
    git(&anchor, &["init", "-q"]);
    git(&anchor, &["config", "user.email", "pair@example.invalid"]);
    git(&anchor, &["config", "user.name", "pair-test"]);
    std::fs::write(anchor.join("README.md"), "base\n").unwrap();
    git(&anchor, &["add", "README.md"]);
    git(&anchor, &["commit", "-qm", "base"]);
    let mut argv = py.clone();
    argv.push(wrapper.to_string_lossy().into_owned());
    Some(Fixture {
        _dir: dir,
        anchor,
        delegate: json_array(&argv),
        py,
    })
}

impl Fixture {
    fn mesh(&self, delegate: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_localpilot"))
            .arg("mesh")
            .arg("--repo")
            .arg(&self.anchor)
            .args(args)
            .env_remove("PAIR_REPO")
            .env("LOCALPILOT_MESH__WRITER", "delegate")
            .env("LOCALPILOT_MESH__DELEGATE_COMMAND", delegate)
            .env("PYTHONIOENCODING", "utf-8")
            .output()
            .unwrap()
    }

    fn reference(&self, args: &[&str]) {
        let st = Command::new(&self.py[0])
            .args(&self.py[1..])
            .arg(suite().join("reference").join("pair.py"))
            .arg("--repo")
            .arg(&self.anchor)
            .args(args)
            .env_remove("PAIR_REPO")
            .status()
            .unwrap();
        assert!(st.success(), "reference {args:?}");
    }

    fn mailbox(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![self.anchor.join(".pair-programming")];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push((p.clone(), std::fs::read(&p).unwrap()));
                }
            }
        }
        out.sort();
        out
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn every_operation_goes_to_the_delegate_with_its_exit_code() {
    let Some(f) = fixture() else { return };
    f.reference(&["start", "--role", "claude", "--task", "delegate test"]);

    let join = f.mesh(&f.delegate, &["join", "--role", "codex", "--timeout", "5"]);
    assert_eq!(join.status.code(), Some(0), "{}", text(&join.stderr));
    assert!(
        text(&join.stdout).starts_with("JOINED "),
        "{}",
        text(&join.stdout)
    );
    assert!(
        text(&join.stderr).contains(MARK),
        "the delegate, not native, ran"
    );

    let post = f.mesh(
        &f.delegate,
        &[
            "post",
            "--role",
            "codex",
            "--kind",
            "NOTE",
            "--body",
            "via the delegate",
        ],
    );
    assert_eq!(post.status.code(), Some(0), "{}", text(&post.stderr));
    assert!(text(&post.stderr).contains(MARK));

    // A refusal's exit code and line come through unchanged.
    let accept = f.mesh(
        &f.delegate,
        &[
            "accept",
            "--role",
            "codex",
            "--msg-id",
            "claude:1",
            "--generation",
            "1",
        ],
    );
    assert_eq!(accept.status.code(), Some(5), "{}", text(&accept.stderr));
    assert!(text(&accept.stderr).contains("REFUSED no_active_endpoint"));
    assert!(text(&accept.stderr).contains(MARK));
}

#[test]
fn full_only_operations_stay_refused_and_a_broken_delegate_never_falls_back() {
    let Some(f) = fixture() else { return };
    f.reference(&["start", "--role", "claude", "--task", "delegate test"]);
    let before = f.mailbox();

    let start = f.mesh(
        &f.delegate,
        &["start", "--role", "claude", "--task", "again"],
    );
    assert_eq!(start.status.code(), Some(2));
    assert!(
        !text(&start.stderr).contains(MARK),
        "rejected before delegation"
    );

    for broken in ["[]", "[\"no such program for the mesh test\"]"] {
        let out = f.mesh(broken, &["join", "--role", "codex", "--timeout", "1"]);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{broken}: {}",
            text(&out.stderr)
        );
        assert!(
            text(&out.stderr).contains("delegate"),
            "{}",
            text(&out.stderr)
        );
        assert!(
            out.stdout.is_empty(),
            "native must not have run: {}",
            text(&out.stdout)
        );
    }
    assert_eq!(f.mailbox(), before, "nothing wrote the mailbox");
}

#[test]
fn a_repository_cannot_choose_the_program_mesh_runs() {
    // A cloned repository's `.localpilot.toml` names a delegate. It must not
    // run: only the user's config and environment may select one.
    let Some(f) = fixture() else { return };
    f.reference(&["start", "--role", "claude", "--task", "untrusted repo"]);
    std::fs::write(
        f.anchor.join(".localpilot.toml"),
        format!(
            "[mesh]\nwriter = \"delegate\"\ndelegate_command = {}\n",
            f.delegate
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_localpilot"))
        .arg("mesh")
        .arg("--repo")
        .arg(&f.anchor)
        .arg("status")
        .current_dir(&f.anchor)
        .env_remove("PAIR_REPO")
        .env_remove("LOCALPILOT_MESH__WRITER")
        .env_remove("LOCALPILOT_MESH__DELEGATE_COMMAND")
        .output()
        .unwrap();
    assert!(
        !text(&out.stderr).contains(MARK),
        "the repository's delegate ran: {}",
        text(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).starts_with("SESSION "),
        "the native writer answered: {}",
        text(&out.stdout)
    );
}

#[test]
fn the_delegate_passes_the_conformance_subset_and_a_short_soak() {
    let Some(f) = fixture() else { return };
    let fixtures = [
        "two-party-golden",
        "two-party-handoff",
        "delivery-sender-cannot-spoof-acceptance",
        "n-party-forward",
        "version-unknown-major-record-is-refused",
    ];
    let native = native();
    let mut run = tool(&f.py, "run.py");
    run.arg("--participant")
        .arg(format!("codex={native}"))
        .arg("--participant")
        .arg(format!("localpilot={native}"))
        .env("LOCALPILOT_MESH__WRITER", "delegate")
        .env("LOCALPILOT_MESH__DELEGATE_COMMAND", &f.delegate);
    for id in fixtures {
        run.arg(suite().join("fixtures").join(format!("{id}.json")));
    }
    let out = run.output().unwrap();
    let stdout = text(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("SKIPPED 0 / FAILED 0"),
        "{stdout}\n{}",
        text(&out.stderr)
    );

    let out = tool(&f.py, "soak.py")
        .arg("--impl-b")
        .arg(native)
        .arg("--posts")
        .arg("5")
        .env("LOCALPILOT_MESH__WRITER", "delegate")
        .env("LOCALPILOT_MESH__DELEGATE_COMMAND", &f.delegate)
        .output()
        .unwrap();
    assert!(
        out.status.success() && text(&out.stdout).contains("SOAK ok"),
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
}
