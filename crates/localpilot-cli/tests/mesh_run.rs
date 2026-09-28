//! `localpilot mesh run` end to end: the reference implementation runs the
//! session as claude, the engine plays localpilot, and the model is a local
//! OpenAI-compatible mock. Configuration lives in a temporary user config
//! directory, never in the reviewed tree.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use localpilot_mesh::ops::engine::fingerprint_of;
use serde_json::{json, Value};
use support::{python_or_skip, suite};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

struct Fixture {
    _dir: tempfile::TempDir,
    anchor: PathBuf,
    config: PathBuf,
    py: Vec<String>,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn sse(chunks: &[Value]) -> ResponseTemplate {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn says(text: &str) -> ResponseTemplate {
    sse(&[json!({"choices": [{"delta": {"content": text}}]})])
}

fn calls_tool(name: &str, args: &Value) -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1",
            "function": {"name": name, "arguments": args.to_string()}}]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

async fn server() -> MockServer {
    MockServer::start().await
}

impl Fixture {
    /// A Git repository with a committed `a.txt`, a schema-2 session that
    /// the reference started as claude with localpilot, and a user config
    /// naming `server` as the default provider.
    fn new(server: &MockServer) -> Option<Self> {
        let py = python_or_skip("the mesh run tests")?;
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("anchor");
        std::fs::create_dir_all(&anchor).unwrap();
        git(&anchor, &["init", "-q"]);
        git(&anchor, &["config", "user.email", "pair@example.invalid"]);
        git(&anchor, &["config", "user.name", "pair-test"]);
        git(&anchor, &["config", "core.autocrlf", "false"]);
        std::fs::write(anchor.join("a.txt"), "alpha\n").unwrap();
        git(&anchor, &["add", "a.txt"]);
        git(&anchor, &["commit", "-qm", "base"]);
        let config = dir.path().join("config");
        std::fs::create_dir_all(config.join("localpilot")).unwrap();
        std::fs::write(
            config.join("localpilot").join("config.toml"),
            format!(
                "[provider]\ndefault = \"local\"\n\n[providers.local]\nkind = \"openai-compatible\"\nbase_url = \"{}\"\n",
                server.uri()
            ),
        )
        .unwrap();
        let f = Fixture {
            _dir: dir,
            anchor,
            config,
            py,
        };
        f.reference(&[
            "start",
            "--role",
            "claude",
            "--with",
            "localpilot",
            "--task",
            "run test",
        ]);
        Some(f)
    }

    fn reference(&self, args: &[&str]) -> String {
        let out = Command::new(&self.py[0])
            .args(&self.py[1..])
            .arg(suite().join("reference").join("pair.py"))
            .arg("--repo")
            .arg(&self.anchor)
            .args(args)
            .env_remove("PAIR_REPO")
            .env("PYTHONIOENCODING", "utf-8")
            .output()
            .unwrap();
        assert!(out.status.success(), "reference {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    async fn run(&self, extra: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_localpilot"));
        cmd.arg("mesh")
            .arg("--repo")
            .arg(&self.anchor)
            .args([
                "run",
                "--role",
                "localpilot",
                "--model",
                "m",
                "--once",
                "--timeout",
                "30",
            ])
            .args(extra)
            .current_dir(&self.anchor)
            .env_remove("PAIR_REPO")
            .env("APPDATA", &self.config)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("LOCALPILOT_MESH__WRITER", "native");
        tokio::task::spawn_blocking(move || cmd.output().unwrap())
            .await
            .unwrap()
    }

    fn fingerprint(&self, rel: &str) -> String {
        fingerprint_of(&self.anchor, rel).unwrap().unwrap()
    }

    fn session_file(&self) -> PathBuf {
        let mb = self.anchor.join(".pair-programming");
        let p: Value =
            serde_json::from_str(&std::fs::read_to_string(mb.join("active.v2.json")).unwrap())
                .unwrap();
        mb.join("sessions")
            .join(p["session_id"].as_str().unwrap())
            .join("session.v2.json")
    }

    fn journal(&self, role: &str) -> Vec<Value> {
        let path = self
            .session_file()
            .parent()
            .unwrap()
            .join("journal")
            .join(format!("{role}.jsonl"));
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn posted(&self, kind: &str) -> Vec<Value> {
        self.journal("localpilot")
            .into_iter()
            .filter(|m| m["kind"] == kind)
            .collect()
    }

    /// Post a review request as claude and return its msg_id.
    fn request_review(&self, manifest: &str) -> String {
        let body = format!("Please review the change to a.txt.\n\nFingerprints:\n{manifest}\n");
        self.reference(&[
            "post",
            "--role",
            "claude",
            "--kind",
            "REVIEW_REQUEST",
            "--to",
            "localpilot",
            "--body",
            &body,
        ]);
        self.journal("claude").last().unwrap()["msg_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// The tree outside the mailbox, as Git sees it.
    fn status(&self) -> String {
        git(
            &self.anchor,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

const AGREE: &str = r#"Looks right. {"kind": "VERDICT", "decision": "AGREE", "findings": [], "body": "The change is what the request says."}"#;

#[tokio::test]
async fn a_review_request_gets_a_verdict_with_the_engines_header_and_is_acknowledged() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(AGREE))
        .expect(1)
        .mount(&server)
        .await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    std::fs::write(f.anchor.join("a.txt"), "beta\n").unwrap();
    let asked = f.request_review(&format!("a.txt={}", f.fingerprint("a.txt")));

    let out = f.run(&[]).await;
    assert!(out.status.success(), "{}", text(&out));
    let verdicts = f.posted("VERDICT");
    assert_eq!(verdicts.len(), 1, "{}", text(&out));
    assert_eq!(verdicts[0]["reply_to"], asked.as_str());
    assert_eq!(
        verdicts[0]["body"],
        "AGREE round=1 blocking=0 important=0\nThe change is what the request says."
    );
    // The run wrote nothing into the tree: only the change under review.
    assert_eq!(f.status(), " M a.txt\n");
    // Acknowledged: a second run finds nothing to do and posts nothing more.
    let again = f.run(&[]).await;
    assert!(again.status.success(), "{}", text(&again));
    assert_eq!(f.posted("VERDICT").len(), 1);
}

#[tokio::test]
async fn a_request_whose_manifest_does_not_hold_is_revised_without_the_model() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(AGREE))
        .expect(0)
        .mount(&server)
        .await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    std::fs::write(f.anchor.join("a.txt"), "beta\n").unwrap();
    f.request_review("a.txt=000000000000");

    let out = f.run(&[]).await;
    assert!(out.status.success(), "{}", text(&out));
    let verdicts = f.posted("VERDICT");
    assert_eq!(verdicts.len(), 1);
    let body = verdicts[0]["body"].as_str().unwrap();
    assert!(
        body.starts_with("REVISE round=1 blocking=1 important=0"),
        "{body}"
    );
    assert!(body.contains("a.txt: fingerprint"), "{body}");
}

#[tokio::test]
async fn two_invalid_answers_escalate_and_never_post_the_models_text() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says("I think it is fine, ship it."))
        .expect(2)
        .mount(&server)
        .await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    std::fs::write(f.anchor.join("a.txt"), "beta\n").unwrap();
    let asked = f.request_review(&format!("a.txt={}", f.fingerprint("a.txt")));

    let out = f.run(&[]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert!(f.posted("VERDICT").is_empty());
    let escalations = f.posted("ESCALATE");
    assert_eq!(escalations.len(), 1, "{}", text(&out));
    assert_eq!(escalations[0]["reply_to"], asked.as_str());
    let body = escalations[0]["body"].as_str().unwrap();
    assert!(!body.contains("ship it"), "{body}");
}

#[tokio::test]
async fn the_judging_model_cannot_write_the_tree() {
    // Bug it prevents: a navigator's model editing the tree it reviews.
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(calls_tool(
            "write_file",
            &json!({"path": "a.txt", "content": "hijacked\n"}),
        ))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(AGREE))
        .mount(&server)
        .await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    std::fs::write(f.anchor.join("a.txt"), "beta\n").unwrap();
    f.request_review(&format!("a.txt={}", f.fingerprint("a.txt")));

    let out = f.run(&[]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        std::fs::read_to_string(f.anchor.join("a.txt")).unwrap(),
        "beta\n"
    );
    assert_eq!(f.status(), " M a.txt\n");
    assert_eq!(f.posted("VERDICT").len(), 1);
    // The write was really attempted, and the model was told it was denied.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "one tool call, then the answer");
    let second = String::from_utf8_lossy(&requests[1].body).into_owned();
    assert!(
        second.contains("permission denied for write_file"),
        "{second}"
    );
}

#[tokio::test]
async fn a_message_already_answered_is_acknowledged_not_answered_again() {
    // The state a crash between the post and the ack leaves behind.
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(AGREE))
        .expect(0)
        .mount(&server)
        .await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    f.reference(&["join", "--role", "localpilot"]);
    f.reference(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "QUESTION",
        "--to",
        "localpilot",
        "--body",
        "ready?",
    ]);
    let asked = f.journal("claude").last().unwrap()["msg_id"]
        .as_str()
        .unwrap()
        .to_owned();
    f.reference(&[
        "post",
        "--role",
        "localpilot",
        "--kind",
        "ANSWER",
        "--reply-to",
        &asked,
        "--body",
        "yes",
    ]);

    let out = f.run(&[]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains(&format!("ACKED {asked} (already answered)")),
        "{}",
        text(&out)
    );
    assert_eq!(f.posted("ANSWER").len(), 1);
}

/// A model that answers AGREE, after the session has moved to another unit.
struct MovesTheUnit(PathBuf);

impl Respond for MovesTheUnit {
    fn respond(&self, _: &MockRequest) -> ResponseTemplate {
        let mut s: Value =
            serde_json::from_str(&std::fs::read_to_string(&self.0).unwrap()).unwrap();
        s["unit_id"] = json!("2-moved-on");
        std::fs::write(&self.0, s.to_string()).unwrap();
        says(AGREE)
    }
}

#[tokio::test]
async fn a_verdict_for_a_unit_that_moved_on_is_not_posted() {
    let server = server().await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(MovesTheUnit(f.session_file()))
        .expect(1)
        .mount(&server)
        .await;
    std::fs::write(f.anchor.join("a.txt"), "beta\n").unwrap();
    let asked = f.request_review(&format!("a.txt={}", f.fingerprint("a.txt")));

    let out = f.run(&[]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert!(f.posted("VERDICT").is_empty(), "{}", text(&out));
    assert!(
        text(&out).contains(&format!("SKIPPED {asked}: STALE the work unit changed")),
        "{}",
        text(&out)
    );
}

#[tokio::test]
async fn a_schema_one_session_is_refused() {
    let server = server().await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    f.reference(&[
        "abandon",
        "--role",
        "claude",
        "--reason",
        "restart as a pair",
    ]);
    f.reference(&["start", "--role", "claude", "--task", "pair"]);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_localpilot"));
    cmd.arg("mesh")
        .arg("--repo")
        .arg(&f.anchor)
        .args([
            "run",
            "--role",
            "codex",
            "--model",
            "m",
            "--once",
            "--timeout",
            "30",
        ])
        .env_remove("PAIR_REPO")
        .env("APPDATA", &f.config)
        .env("XDG_CONFIG_HOME", &f.config)
        .env("LOCALPILOT_MESH__WRITER", "native");
    let out = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{}", text(&out));
    assert!(
        text(&out).contains("needs an N-party (schema 2) session"),
        "{}",
        text(&out)
    );
}

/// A model that answers AGREE after the reviewed file changed under it.
struct ChangesTheFile(PathBuf);

impl Respond for ChangesTheFile {
    fn respond(&self, _: &MockRequest) -> ResponseTemplate {
        std::fs::write(&self.0, "gamma\n").unwrap();
        says(AGREE)
    }
}

#[tokio::test]
async fn a_tree_that_changes_during_the_review_gets_no_agree() {
    // Bug it prevents: an AGREE for content the model never saw, because the
    // file changed within the same unit while it judged.
    let server = server().await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ChangesTheFile(f.anchor.join("a.txt")))
        .expect(1)
        .mount(&server)
        .await;
    std::fs::write(f.anchor.join("a.txt"), "beta\n").unwrap();
    f.request_review(&format!("a.txt={}", f.fingerprint("a.txt")));

    let out = f.run(&[]).await;
    assert!(out.status.success(), "{}", text(&out));
    let verdicts = f.posted("VERDICT");
    assert_eq!(verdicts.len(), 1, "{}", text(&out));
    let body = verdicts[0]["body"].as_str().unwrap();
    assert!(body.starts_with("REVISE round=1 blocking=1"), "{body}");
    assert!(body.contains("a.txt: fingerprint"), "{body}");
    assert!(
        text(&out).contains("the tree moved during review"),
        "{}",
        text(&out)
    );
}

#[tokio::test]
async fn a_poll_interval_out_of_range_is_a_usage_error() {
    for bad in ["inf", "NaN", "0", "-1", "3601"] {
        let out = Command::new(env!("CARGO_BIN_EXE_localpilot"))
            .args([
                "mesh",
                "run",
                "--role",
                "localpilot",
                "--model",
                "m",
                &format!("--poll={bad}"),
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "--poll {bad}: {}", text(&out));
        assert!(text(&out).contains("0.05 to 3600"), "{}", text(&out));
    }
}

#[tokio::test]
async fn the_review_turn_starts_no_configured_mcp_server() {
    // A server's start-up runs outside the permission engine, so the review
    // runtime must not start one at all.
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(AGREE))
        .expect(1)
        .mount(&server)
        .await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    let marker = f.config.join("mcp-started");
    let script = f.config.join("probe_server.py");
    std::fs::write(
        &script,
        format!(
            "open({:?}, 'w').write('started')\n",
            marker.to_string_lossy()
        ),
    )
    .unwrap();
    let cfg = f.config.join("localpilot").join("config.toml");
    let mut toml = std::fs::read_to_string(&cfg).unwrap();
    toml.push_str(&format!(
        "\n[mcp.servers.probe]\ncommand = {:?}\nargs = [{:?}]\n",
        f.py[0],
        script.to_string_lossy()
    ));
    std::fs::write(&cfg, toml).unwrap();
    std::fs::write(f.anchor.join("a.txt"), "beta\n").unwrap();
    f.request_review(&format!("a.txt={}", f.fingerprint("a.txt")));

    let out = f.run(&[]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(f.posted("VERDICT").len(), 1, "{}", text(&out));
    assert!(!marker.exists(), "the review turn started an MCP server");
}

#[tokio::test]
async fn a_command_the_user_vetted_still_cannot_run_in_a_review_turn() {
    // Bug it prevents: a listed command, which `readonly` admits, writing the
    // tree the model reviews.
    let server = server().await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    let script = f.config.join("writes_tree.py");
    std::fs::write(
        &script,
        format!(
            "open({:?}, 'w').write('hijacked')\n",
            f.anchor.join("a.txt").to_string_lossy()
        ),
    )
    .unwrap();
    let script = script.to_string_lossy().into_owned();
    let cfg = f.config.join("localpilot").join("config.toml");
    let mut toml = std::fs::read_to_string(&cfg).unwrap();
    toml.push_str(&format!(
        "\n[[permissions.allow_commands]]\nprogram = {:?}\nargs_prefix = [{:?}]\n",
        f.py[0], script
    ));
    std::fs::write(&cfg, toml).unwrap();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(calls_tool(
            "run_shell",
            &json!({"program": f.py[0], "args": [script]}),
        ))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(AGREE))
        .mount(&server)
        .await;
    std::fs::write(f.anchor.join("a.txt"), "beta\n").unwrap();
    f.request_review(&format!("a.txt={}", f.fingerprint("a.txt")));

    let out = f.run(&[]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        std::fs::read_to_string(f.anchor.join("a.txt")).unwrap(),
        "beta\n"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "one tool call, then the answer");
    let second = String::from_utf8_lossy(&requests[1].body).into_owned();
    assert!(
        second.contains("permission denied for run_shell"),
        "{second}"
    );
    assert_eq!(f.posted("VERDICT").len(), 1);
}
