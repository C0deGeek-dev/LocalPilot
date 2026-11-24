//! Review snapshots across implementations (spec RS-1..RS-6): the native
//! participant as the snapshot owner with the reference as its reviewer, and
//! the reference as the owner with the native participant checking what it
//! is asked to agree to.
//!
//! The reference is the vendored, pinned copy. Needs Python 3.9+ and Git;
//! without Python this is skipped, and fails under `CI=true`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use localpilot_mesh::ops::PostArgs;
use localpilot_mesh::Mesh;
use serde_json::Value;

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

struct Pair {
    py: Vec<String>,
    _dir: tempfile::TempDir,
    no_hooks: PathBuf,
    reference: PathBuf,
    anchor: PathBuf,
}

impl Pair {
    /// A Git anchor with one commit and a strict snapshot session between
    /// `claude` (the reference) and `localpilot` (native), owned by claude.
    fn new() -> Option<Self> {
        let Some(py) = python() else {
            assert!(
                std::env::var("CI").is_err(),
                "Python 3.9+ is required under CI"
            );
            return None;
        };
        let dir = tempfile::tempdir().unwrap();
        // The vendored reference itself, exactly as pinned.
        let reference = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("conformance")
            .join("reference")
            .join("pair.py");
        let anchor = dir.path().join("anchor");
        std::fs::create_dir(&anchor).unwrap();
        let no_hooks = dir.path().join("no-hooks");
        std::fs::create_dir(&no_hooks).unwrap();
        let p = Self {
            py,
            no_hooks,
            _dir: dir,
            reference,
            anchor,
        };
        p.git(&["init", "-q", "-b", "main"]);
        std::fs::write(p.anchor.join("a.txt"), "alpha\n").unwrap();
        std::fs::write(p.anchor.join("old.txt"), "moved\n").unwrap();
        p.git(&["add", "-A"]);
        p.git(&["commit", "-q", "-m", "base"]);
        p.ok(&[
            "start",
            "--role",
            "claude",
            "--with",
            "localpilot",
            "--task",
            "t",
            "--review-snapshot",
        ]);
        let out = p
            .mesh()
            .join("localpilot", 5, Duration::from_millis(100))
            .unwrap();
        assert_eq!(out.code, 0, "{out:?}");
        Some(p)
    }

    fn mesh(&self) -> Mesh {
        Mesh::at(&self.anchor, "flag")
    }

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.anchor)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "-c",
                "core.autocrlf=false",
            ])
            // A fixture repository: no developer's hooks run in it.
            .arg("-c")
            .arg(format!("core.hooksPath={}", self.no_hooks.display()))
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(&self.py[0])
            .args(&self.py[1..])
            .arg(&self.reference)
            .arg("--repo")
            .arg(&self.anchor)
            .args(args)
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("PAIR_NO_PUSH", "1")
            .env("PAIR_HOST_USAGE", "off")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "pair.py {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn session(&self) -> Value {
        let mb = localpilot_mesh::Mailbox::at(&self.anchor);
        let p = localpilot_mesh::session::pointer(&mb).unwrap().unwrap();
        let file = mb.session_dir(&p.session_id).join("session.v2.json");
        serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap()
    }

    /// The reference's digest of the published snapshot, recomputed from its
    /// trees in Python: proof the two implementations hash the same bytes.
    fn reference_digest_of_published(&self) -> String {
        let trees = self.session()["review"]["trees"].to_string();
        let code = "import importlib.util,json,sys\n\
spec=importlib.util.spec_from_file_location('pairref',sys.argv[1])\n\
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)\n\
print(m.snapshot_digest(json.loads(sys.stdin.read())))";
        let mut child = Command::new(&self.py[0])
            .args(&self.py[1..])
            .args(["-c", code])
            .arg(&self.reference)
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write as _;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(trees.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn post(&self, role: &str, kind: &str, body: &str, reply_to: Option<&str>) -> String {
        let out = self
            .mesh()
            .post(
                role,
                &PostArgs {
                    kind: kind.into(),
                    body: body.into(),
                    expect_reply: kind == "REVIEW_REQUEST",
                    reply_to: reply_to.map(str::to_owned),
                    ..PostArgs::default()
                },
            )
            .unwrap();
        assert_eq!(out.code, 0, "{out:?}");
        out.stderr
    }
}

#[test]
fn a_native_owner_closes_only_on_the_bytes_the_reference_agreed_to() {
    let Some(p) = Pair::new() else {
        return;
    };
    p.ok(&["handoff-offer", "--role", "claude"]);
    let out = p.mesh().handoff_accept("localpilot").unwrap();
    assert_eq!(out.code, 0, "{out:?}");

    // A committed rename (both sides count), a changed file, and an
    // untracked file under a non-ASCII directory with a character beyond the
    // BMP, so the digest's ASCII escaping is exercised.
    p.git(&["mv", "old.txt", "new.txt"]);
    p.git(&["commit", "-q", "-m", "rename"]);
    std::fs::write(p.anchor.join("a.txt"), "beta\n").unwrap();
    std::fs::create_dir(p.anchor.join("\u{e9}t\u{e9}")).unwrap();
    std::fs::write(p.anchor.join("\u{e9}t\u{e9}").join("\u{1d4b3}.txt"), "x\n").unwrap();

    p.post("localpilot", "REVIEW_REQUEST", "review the change", None);
    let s = p.session();
    let review = &s["review"];
    assert_eq!(review["request"], "localpilot:3", "{s}");
    let anchor = review["trees"]["anchor"].as_object().unwrap();
    let mut paths: Vec<&str> = anchor.keys().map(String::as_str).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        ["a.txt", "new.txt", "old.txt", "\u{e9}t\u{e9}/\u{1d4b3}.txt"],
        "{review}"
    );
    assert_eq!(anchor["old.txt"], "gone");
    assert_eq!(
        review["digest"].as_str().unwrap(),
        p.reference_digest_of_published(),
        "the reference hashes the published snapshot to the same digest"
    );
    let status = p.mesh().status().unwrap().stdout;
    assert!(
        status.contains(&format!(
            "REVIEW_SNAPSHOT on request=localpilot:3 digest={}",
            review["digest"].as_str().unwrap()
        )),
        "{status}"
    );
    // The owner is warned when it writes over a current review.
    let guard = p.mesh().guard_write("localpilot", None).unwrap();
    assert!(
        guard
            .stderr
            .starts_with("REVIEW_PENDING request=localpilot:3"),
        "{guard:?}"
    );

    p.ok(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "VERDICT",
        "--body",
        "AGREE round=1 blocking=0 important=0",
        "--reply-to",
        "localpilot:3",
    ]);
    // The reference, recomputing the tree itself, agrees the published
    // snapshot is current.
    let brief = p.ok(&["brief", "--role", "claude"]);
    assert!(brief.contains("status=current"), "{brief}");

    // Bytes changed after the agreement: the native close refuses.
    std::fs::write(p.anchor.join("a.txt"), "gamma\n").unwrap();
    let err = p.mesh().complete("localpilot").unwrap_err().to_string();
    assert!(
        err.contains("REVIEW_SNAPSHOT_CHANGED request=localpilot:3 paths=a.txt"),
        "{err}"
    );
    // A superseding request: the old AGREE no longer answers it.
    p.post("localpilot", "REVIEW_REQUEST", "review again", None);
    let err = p.mesh().complete("localpilot").unwrap_err().to_string();
    assert!(
        err.contains("REVIEW_SNAPSHOT_STALE request=localpilot:4"),
        "{err}"
    );
    p.ok(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "VERDICT",
        "--body",
        "AGREE round=2 blocking=0 important=0",
        "--reply-to",
        "localpilot:4",
    ]);
    // Committing the reviewed bytes changes nothing (RS-1: against the base).
    p.git(&["add", "-A"]);
    p.git(&["commit", "-q", "-m", "work"]);
    let out = p.mesh().complete("localpilot").unwrap();
    assert_eq!(out.code, 0, "{out:?}");
}

#[test]
fn a_withdrawn_native_request_leaves_nothing_to_close_on() {
    let Some(p) = Pair::new() else {
        return;
    };
    p.ok(&["handoff-offer", "--role", "claude"]);
    p.mesh().handoff_accept("localpilot").unwrap();
    std::fs::write(p.anchor.join("a.txt"), "beta\n").unwrap();
    p.post("localpilot", "REVIEW_REQUEST", "review", None);
    p.ok(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "VERDICT",
        "--body",
        "AGREE round=1 blocking=0 important=0",
        "--reply-to",
        "localpilot:3",
    ]);
    let out = p
        .mesh()
        .review_withdraw("localpilot", Some("more to do"))
        .unwrap();
    assert_eq!(out.stdout, "REVIEW_WITHDRAWN request=localpilot:3\n");
    let s = p.session();
    assert!(s["review"].is_null(), "{s}");
    assert_eq!(s["review_floor"]["localpilot"], 4);
    let err = p.mesh().complete("localpilot").unwrap_err().to_string();
    assert!(err.contains("REVIEW_SNAPSHOT_MISSING"), "{err}");
    // The reference reads the native withdrawal the same way.
    let out = p.run(&["status"]);
    let status = String::from_utf8_lossy(&out.stdout);
    assert!(
        status.lines().any(|l| l.trim_end() == "REVIEW_SNAPSHOT on"),
        "{status}"
    );
}

#[test]
fn a_native_reviewer_agrees_only_to_the_published_bytes() {
    let Some(p) = Pair::new() else {
        return;
    };
    std::fs::write(p.anchor.join("a.txt"), "beta\n").unwrap();
    let fp = |rel: &str| {
        localpilot_mesh::ops::engine::fingerprint_of(&p.anchor, rel)
            .unwrap()
            .unwrap()
    };
    let body = format!("review a.txt\n\nFingerprints:\na.txt={}\n", fp("a.txt"));
    p.ok(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "REVIEW_REQUEST",
        "--body",
        &body,
        "--expect-reply",
    ]);
    let m = p.mesh();
    let none = Duration::ZERO;
    assert!(m
        .snapshot_review_problems("localpilot", "claude:1", none)
        .unwrap()
        .is_empty());
    // The verdict tool's and the engine's view: open while the bytes hold.
    let request = m
        .open_review("localpilot", "claude:1")
        .unwrap()
        .expect("the request is open");
    assert!(m.recheck_review("localpilot", &request).unwrap().is_none());

    // Bytes changed between the request and the verdict, and changed back
    // to text whose textual fingerprint still matches: only the snapshot
    // notices a CRLF the fingerprint reads as LF.
    std::fs::write(p.anchor.join("a.txt"), "beta\r\n").unwrap();
    assert_eq!(
        fp("a.txt"),
        request.body.lines().last().unwrap()["a.txt=".len()..]
    );
    let problems = m
        .snapshot_review_problems("localpilot", "claude:1", none)
        .unwrap();
    assert_eq!(
        problems,
        ["a.txt: changed since the request's snapshot was published"]
    );
    let why = m
        .open_review("localpilot", "claude:1")
        .unwrap()
        .unwrap_err();
    assert!(why.contains("review snapshot does not hold"), "{why}");
    let revise = m
        .recheck_review("localpilot", &request)
        .unwrap()
        .expect("the engine revises instead of agreeing");
    assert!(revise.body.starts_with("REVISE "), "{}", revise.body);

    // A superseding request retires the first.
    p.ok(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "REVIEW_REQUEST",
        "--body",
        "again",
        "--expect-reply",
    ]);
    let problems = m
        .snapshot_review_problems("localpilot", "claude:1", none)
        .unwrap();
    assert!(
        problems[0].contains("no longer the current review request"),
        "{problems:?}"
    );
    assert!(m
        .snapshot_review_problems("localpilot", "claude:2", none)
        .unwrap()
        .is_empty());
    // A withdrawal retires the current one.
    p.ok(&["review-withdraw", "--role", "claude"]);
    let problems = m
        .snapshot_review_problems("localpilot", "claude:2", none)
        .unwrap();
    assert!(
        problems[0].contains("no longer the current review request"),
        "{problems:?}"
    );
}

#[test]
fn a_native_reviewer_reads_crlf_and_links_as_the_snapshot_does() {
    let Some(p) = Pair::new() else {
        return;
    };
    // Exact bytes, as the pairing skill fingerprints them: SHA-256 of the
    // working-tree bytes, first 12 hex, CRLF kept.
    std::fs::write(p.anchor.join("a.txt"), "beta\r\n").unwrap();
    let exact = {
        use sha2::Digest as _;
        let d = sha2::Sha256::digest(std::fs::read(p.anchor.join("a.txt")).unwrap());
        d.iter().map(|b| format!("{b:02x}")).collect::<String>()[..12].to_owned()
    };
    let linked = std::fs::write(p.anchor.join("target.txt"), "t\n").is_ok()
        && symlink(&p.anchor.join("target.txt"), &p.anchor.join("link.txt"));
    let mut body = format!("review\n\nFingerprints:\na.txt={exact}\n");
    if linked {
        body.push_str("link.txt=0123456789ab\ntarget.txt=0123456789ab\n");
    }
    p.ok(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "REVIEW_REQUEST",
        "--body",
        &body,
        "--expect-reply",
    ]);
    let m = p.mesh();
    // The published snapshot is the inventory: no REVISE for a CRLF file
    // whose textual fingerprint differs, nor for a link.
    let request = m
        .open_review("localpilot", "claude:1")
        .unwrap()
        .expect("the request is open");
    let mut want = vec!["a.txt".to_owned()];
    if linked {
        want.extend(["link.txt".to_owned(), "target.txt".to_owned()]);
    }
    assert_eq!(request.files, want);
    assert!(m.recheck_review("localpilot", &request).unwrap().is_none());
}

/// A file symlink, when this platform and account may make one.
fn symlink(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }
}

#[test]
fn a_durable_withdrawal_retires_a_request_whose_state_was_not_updated() {
    let Some(p) = Pair::new() else {
        return;
    };
    std::fs::write(p.anchor.join("a.txt"), "beta\n").unwrap();
    p.ok(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "REVIEW_REQUEST",
        "--body",
        "review",
        "--expect-reply",
    ]);
    let m = p.mesh();
    assert!(m.open_review("localpilot", "claude:1").unwrap().is_ok());
    // The owner's withdrawal note landed, but its session rewrite did not
    // (an interrupted `review-withdraw`): the published state still names
    // claude:1 as current.
    p.post("claude", "NOTE", "REVIEW_WITHDRAWN request=claude:1", None);
    assert_eq!(p.session()["review"]["request"], "claude:1");
    let why = m
        .open_review("localpilot", "claude:1")
        .unwrap()
        .unwrap_err();
    assert!(
        why.contains("no longer the current review request"),
        "{why}"
    );
    let problems = m
        .snapshot_review_problems("localpilot", "claude:1", Duration::ZERO)
        .unwrap();
    assert!(
        problems[0].contains("no longer the current review request"),
        "{problems:?}"
    );
}

#[test]
fn the_native_owner_requests_review_of_what_the_snapshot_publishes() {
    let Some(p) = Pair::new() else {
        return;
    };
    p.ok(&["handoff-offer", "--role", "claude"]);
    p.mesh().handoff_accept("localpilot").unwrap();
    std::fs::write(p.anchor.join("a.txt"), "beta\r\n").unwrap();
    // A dangling link to a target outside the tree: reviewable as link
    // data, though no textual fingerprint could follow it.
    let linked = symlink(
        &p.anchor.join("..").join("missing.txt"),
        &p.anchor.join("away.txt"),
    );
    let m = p.mesh();
    let args = m
        .review_request("localpilot", "the change")
        .unwrap()
        .expect("a snapshot request can be built");
    let exact = {
        use sha2::Digest as _;
        let d = sha2::Sha256::digest(std::fs::read(p.anchor.join("a.txt")).unwrap());
        d.iter().map(|b| format!("{b:02x}")).collect::<String>()[..12].to_owned()
    };
    assert!(
        args.body.contains(&format!("a.txt={exact}\n")),
        "{}",
        args.body
    );
    if linked {
        assert!(args.body.contains("away.txt=link:"), "{}", args.body);
    }
    let out = m.post("localpilot", &args).unwrap();
    assert_eq!(out.code, 0, "{out:?}");
    let s = p.session();
    let request = s["review"]["request"].as_str().unwrap().to_owned();
    if linked {
        assert!(s["review"]["trees"]["anchor"]["away.txt"]
            .as_str()
            .unwrap()
            .starts_with("link:"));
    }
    p.ok(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "VERDICT",
        "--body",
        "AGREE round=1 blocking=0 important=0",
        "--reply-to",
        &request,
    ]);
    let out = m.complete("localpilot").unwrap();
    assert_eq!(out.code, 0, "{out:?}");
}
