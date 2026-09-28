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

    /// The engine as a command, for runs that need extra environment or to
    /// run in the background.
    fn engine(&self, extra: &[&str], env: &[(&str, &str)]) -> Command {
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
                "--timeout",
                "30",
            ])
            .args(extra)
            .current_dir(&self.anchor)
            .env_remove("PAIR_REPO")
            .env_remove("PAIR_ENDPOINT_TOKEN")
            .env("APPDATA", &self.config)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("LOCALPILOT_MESH__WRITER", "native");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd
    }

    /// The reference's output without asserting success.
    fn reference_out(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(&self.py[0]);
        cmd.args(&self.py[1..])
            .arg(suite().join("reference").join("pair.py"))
            .arg("--repo")
            .arg(&self.anchor)
            .args(args)
            .env_remove("PAIR_REPO")
            .env_remove("PAIR_ENDPOINT_TOKEN")
            .env("PYTHONIOENCODING", "utf-8");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    /// A participant's endpoint record, if any.
    fn endpoint_record(&self, role: &str) -> Option<Value> {
        let p = self
            .session_file()
            .parent()
            .unwrap()
            .join("endpoints")
            .join(format!("{role}.json"));
        std::fs::read_to_string(p)
            .ok()
            .map(|s| serde_json::from_str(&s).unwrap())
    }

    fn facts(&self, area: &str, role: &str) -> Vec<Value> {
        let p = self
            .session_file()
            .parent()
            .unwrap()
            .join(area)
            .join(format!("{role}.jsonl"));
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn fingerprint(&self, rel: &str) -> String {
        fingerprint_of(&self.anchor, rel).unwrap().unwrap()
    }

    fn session_file(&self) -> PathBuf {
        // The newest schema-2 session, read by its directory: a closed
        // session has no active pointer any more.
        let sessions = self.anchor.join(".pair-programming").join("sessions");
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&sessions)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("session.v2.json").exists())
            .collect();
        dirs.sort();
        dirs.pop().unwrap().join("session.v2.json")
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

// --- the owner path (`--own`) ---------------------------------------------------

const DONE: &str = r#"Done. {"kind": "REVIEW_REQUEST", "body": "added b.txt and checked it"}"#;

/// A model that writes `b.txt` with `content`, then asks for review.
async fn mount_owner_turn(server: &MockServer, content: &str, priority: u8) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(calls_tool(
            "write_file",
            &json!({"path": "b.txt", "content": content}),
        ))
        .up_to_n_times(1)
        .with_priority(priority)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(DONE))
        .up_to_n_times(1)
        .with_priority(priority + 1)
        .mount(server)
        .await;
}

fn session_status(f: &Fixture) -> String {
    let s: Value =
        serde_json::from_str(&std::fs::read_to_string(f.session_file()).unwrap()).unwrap();
    s["status"].as_str().unwrap_or_default().to_owned()
}

fn verdict(f: &Fixture, request: &str, body: &str) {
    f.reference(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "VERDICT",
        "--reply-to",
        request,
        "--body",
        body,
    ]);
}

#[tokio::test]
async fn an_owner_accepts_implements_asks_for_review_and_closes_on_agreement() {
    let server = server().await;
    mount_owner_turn(&server, "beta\n", 1).await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    f.reference(&["handoff-offer", "--role", "claude"]);

    let out = f.run(&["--own"]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("ACCEPTED claude:"), "{}", text(&out));
    let requests = f.posted("REVIEW_REQUEST");
    assert_eq!(requests.len(), 1, "{}", text(&out));
    let body = requests[0]["body"].as_str().unwrap();
    let want = format!(
        "added b.txt and checked it\n\nFingerprints:\nb.txt={}",
        f.fingerprint("b.txt")
    );
    assert_eq!(body, want);
    assert_eq!(requests[0]["to"], json!(["claude"]));

    // Waiting on the reviewer: another run posts nothing.
    let out = f.run(&["--own"]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(f.posted("REVIEW_REQUEST").len(), 1, "{}", text(&out));

    let id = requests[0]["msg_id"].as_str().unwrap().to_owned();
    verdict(&f, &id, "AGREE round=1 blocking=0 important=0\nfine");
    let out = f.run(&["--own"]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("CLOSED on every reviewer's agreement"),
        "{}",
        text(&out)
    );
    assert_eq!(session_status(&f), "completed");
}

#[tokio::test]
async fn a_revise_verdict_gets_a_second_round_with_its_findings() {
    let server = server().await;
    mount_owner_turn(&server, "beta\n", 1).await;
    mount_owner_turn(&server, "beta, fixed\n", 3).await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    f.reference(&["handoff-offer", "--role", "claude"]);
    let out = f.run(&["--own"]).await;
    assert!(out.status.success(), "{}", text(&out));
    let first = f.posted("REVIEW_REQUEST")[0]["msg_id"]
        .as_str()
        .unwrap()
        .to_owned();
    verdict(
        &f,
        &first,
        "REVISE round=1 blocking=1 important=0\n- b.txt [blocking] say fixed",
    );

    let out = f.run(&["--own"]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("round=2"), "{}", text(&out));
    assert_eq!(f.posted("REVIEW_REQUEST").len(), 2);
    assert_eq!(
        std::fs::read_to_string(f.anchor.join("b.txt")).unwrap(),
        "beta, fixed\n"
    );
    // The model was shown the findings.
    let requests = server.received_requests().await.unwrap();
    let last = String::from_utf8_lossy(&requests.last().unwrap().body).into_owned();
    assert!(last.contains("say fixed"), "{last}");
}

#[tokio::test]
async fn an_owner_that_changes_nothing_escalates_after_one_retry() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(DONE))
        .expect(2)
        .mount(&server)
        .await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    f.reference(&["handoff-offer", "--role", "claude"]);
    let out = f.run(&["--own"]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert!(f.posted("REVIEW_REQUEST").is_empty());
    let esc = f.posted("ESCALATE");
    assert_eq!(esc.len(), 1, "{}", text(&out));
    assert!(
        esc[0]["body"].as_str().unwrap().contains("nothing changed"),
        "{}",
        esc[0]["body"]
    );
    // The model saw why its first answer was refused.
    let requests = server.received_requests().await.unwrap();
    let second = String::from_utf8_lossy(&requests[1].body).into_owned();
    assert!(second.contains("nothing changed in the unit"), "{second}");
}

#[tokio::test]
async fn without_own_a_handoff_is_declined_and_nothing_is_written() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(DONE))
        .expect(0)
        .mount(&server)
        .await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    f.reference(&["handoff-offer", "--role", "claude"]);
    let out = f.run(&[]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(f.posted("NOTE").len(), 1, "{}", text(&out));
    assert!(f.posted("HANDOFF_ACCEPT").is_empty());
}

#[tokio::test]
async fn a_handoff_whose_tree_moved_is_not_accepted() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(DONE))
        .expect(0)
        .mount(&server)
        .await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    f.reference(&["handoff-offer", "--role", "claude"]);
    std::fs::write(f.anchor.join("a.txt"), "moved after the offer\n").unwrap();
    let out = f.run(&["--own"]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("NOT_ACCEPTED"), "{}", text(&out));
    assert!(f.posted("HANDOFF_ACCEPT").is_empty());
    let notes = f.posted("NOTE");
    assert!(
        notes[0]["body"]
            .as_str()
            .unwrap()
            .contains("could not accept this handoff"),
        "{notes:?}"
    );
}

/// A model that, while it works, loses the tree: the session's owner moves
/// back before its write lands.
struct LosesTheTree(PathBuf);

impl Respond for LosesTheTree {
    fn respond(&self, _: &MockRequest) -> ResponseTemplate {
        let mut s: Value =
            serde_json::from_str(&std::fs::read_to_string(&self.0).unwrap()).unwrap();
        s["owner"] = json!("claude");
        s["ownership_epoch"] = json!(s["ownership_epoch"].as_i64().unwrap_or(0) + 1);
        std::fs::write(&self.0, s.to_string()).unwrap();
        calls_tool("write_file", &json!({"path": "b.txt", "content": "late\n"}))
    }
}

#[tokio::test]
async fn an_owner_that_loses_the_tree_mid_turn_writes_nothing_more_and_posts_nothing() {
    let server = server().await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(LosesTheTree(f.session_file()))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(DONE))
        .mount(&server)
        .await;
    f.reference(&["handoff-offer", "--role", "claude"]);
    let out = f.run(&["--own"]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        !f.anchor.join("b.txt").exists(),
        "a write landed after the lease was lost"
    );
    assert!(f.posted("REVIEW_REQUEST").is_empty(), "{}", text(&out));
    let requests = server.received_requests().await.unwrap();
    let second = String::from_utf8_lossy(&requests[1].body).into_owned();
    assert!(
        second.contains("does not let this participant write"),
        "{second}"
    );
}

#[tokio::test]
async fn a_reviewer_escalation_stops_the_owner() {
    let server = server().await;
    mount_owner_turn(&server, "beta\n", 1).await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    f.reference(&["handoff-offer", "--role", "claude"]);
    let out = f.run(&["--own"]).await;
    assert!(out.status.success(), "{}", text(&out));
    let id = f.posted("REVIEW_REQUEST")[0]["msg_id"]
        .as_str()
        .unwrap()
        .to_owned();
    f.reference(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "ESCALATE",
        "--reply-to",
        &id,
        "--body",
        "needs a person",
    ]);
    let out = f.run(&["--own"]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("STOPPED as owner: claude ESCALATE"),
        "{}",
        text(&out)
    );
    assert_eq!(f.posted("REVIEW_REQUEST").len(), 1);
}

#[tokio::test]
async fn own_is_refused_in_a_session_without_version_control() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(DONE))
        .expect(0)
        .mount(&server)
        .await;
    let Some(py) = python_or_skip("the mesh run tests") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let anchor = dir.path().join("plain");
    std::fs::create_dir_all(&anchor).unwrap();
    std::fs::write(anchor.join("a.txt"), "alpha\n").unwrap();
    let st = Command::new(&py[0])
        .args(&py[1..])
        .arg(suite().join("reference").join("pair.py"))
        .arg("--repo")
        .arg(&anchor)
        .args([
            "start",
            "--role",
            "claude",
            "--with",
            "localpilot",
            "--no-vcs",
            "--task",
            "t",
        ])
        .env_remove("PAIR_REPO")
        .env("PYTHONIOENCODING", "utf-8")
        .output()
        .unwrap();
    assert!(st.status.success(), "{st:?}");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_localpilot"));
    cmd.arg("mesh")
        .arg("--repo")
        .arg(&anchor)
        .args([
            "run",
            "--role",
            "localpilot",
            "--model",
            "m",
            "--once",
            "--own",
            "--timeout",
            "30",
        ])
        .env_remove("PAIR_REPO")
        .env("LOCALPILOT_MESH__WRITER", "native");
    let out = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{}", text(&out));
    assert!(
        text(&out).contains("--own needs a Git anchor"),
        "{}",
        text(&out)
    );
}

#[tokio::test]
async fn a_stop_already_acknowledged_still_stops_a_restarted_owner() {
    // Bug it prevents: a restarted --own run finding no unread STOP and
    // starting to write again.
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(DONE))
        .expect(0)
        .mount(&server)
        .await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    // The STOP comes after localpilot owns the unit and before any request.
    // (One posted before the accept is below claude's new verdict floor and
    // no longer stands, spec U-4.)
    f.reference(&["join", "--role", "localpilot"]);
    f.reference(&["handoff-offer", "--role", "claude"]);
    f.reference(&["handoff-accept", "--role", "localpilot"]);
    f.reference(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "STOP",
        "--to",
        "localpilot",
        "--body",
        "halt",
    ]);
    let out = f.run(&["--own"]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("STOPPED by claude:"), "{}", text(&out));
    // Restarted: the STOP is acknowledged, but it still stands.
    let out = f.run(&["--own"]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("STOPPED as owner: claude STOP"),
        "{}",
        text(&out)
    );
    assert!(!f.anchor.join("b.txt").exists());
    assert!(f.posted("REVIEW_REQUEST").is_empty());
}

/// Send one raw wake request to an endpoint address and return the reply.
async fn wake_raw(transport: &str, address: &str, line: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut reply = Vec::new();
    #[cfg(windows)]
    {
        assert_eq!(transport, "pipe");
        let mut c = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(address)
            .unwrap();
        c.write_all(line.as_bytes()).await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), c.read_to_end(&mut reply))
            .await;
    }
    #[cfg(unix)]
    {
        assert_eq!(transport, "unix");
        let mut c = tokio::net::UnixStream::connect(address).await.unwrap();
        c.write_all(line.as_bytes()).await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), c.read_to_end(&mut reply))
            .await;
    }
    String::from_utf8_lossy(&reply).trim().to_owned()
}

async fn until<F: Fn() -> bool>(what: &str, ok: F) {
    for _ in 0..200 {
        if ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn a_listening_engine_renews_through_a_long_model_turn_then_retires_its_endpoint() {
    // Renewal runs on its own clock: the lease is renewed while the loop is
    // parked in a 3 s model turn, and a clean exit unregisters it.
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(AGREE).set_delay(std::time::Duration::from_secs(3)))
        .mount(&server)
        .await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    std::fs::write(f.anchor.join("a.txt"), "beta\n").unwrap();
    f.request_review(&format!("a.txt={}", f.fingerprint("a.txt")));
    let mut cmd = f.engine(
        &["--once", "--listen"],
        &[("LOCALPILOT_TEST_MESH_RENEW_MS", "200")],
    );
    let out = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("LISTENING role=localpilot"),
        "{}",
        text(&out)
    );
    assert_eq!(f.posted("VERDICT").len(), 1, "{}", text(&out));
    let ep = f.endpoint_record("localpilot").unwrap();
    assert_eq!(ep["active"], false, "{ep}");
    assert!(
        ep["generation"].as_i64().unwrap() >= 5,
        "renewed only to {ep}"
    );
}

#[tokio::test]
async fn a_listening_engine_acts_on_a_wake_and_the_endpoint_holds_against_other_processes() {
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
    // Poll once an hour: anything that happens promptly came from a wake.
    let mut engine = f
        .engine(&["--listen", "--poll", "3600"], &[])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    until("the endpoint", || {
        f.endpoint_record("localpilot")
            .is_some_and(|e| e["active"] == true)
    })
    .await;
    let ep = f.endpoint_record("localpilot").unwrap();
    let (transport, address) = (
        ep["transport"].as_str().unwrap().to_owned(),
        ep["address"].as_str().unwrap().to_owned(),
    );
    let generation = ep["generation"].as_i64().unwrap();
    let sid = ep["session_id"].as_str().unwrap().to_owned();

    // A post by claude wakes it: the push is sent and the endpoint accepts.
    f.reference(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "NOTE",
        "--to",
        "localpilot",
        "--body",
        "hello",
    ]);
    let posted = f.journal("claude").last().unwrap()["msg_id"]
        .as_str()
        .unwrap()
        .to_owned();
    until("the receipt", || {
        f.facts("receipts", "localpilot")
            .iter()
            .any(|r| r["msg_id"] == posted.as_str())
    })
    .await;
    assert!(f
        .facts("pushes", "claude")
        .iter()
        .any(|p| p["msg_id"] == posted.as_str() && p["outcome"] == "sent"));

    // D009: another process can neither replace the live endpoint nor accept
    // for it, with no token or a wrong one.
    let steal = f.reference_out(
        &[
            "endpoint",
            "--role",
            "localpilot",
            "--register",
            "--transport",
            "pipe",
            "--address",
            r"\\.\pipe\thief",
        ],
        &[],
    );
    assert_eq!(steal.status.code(), Some(5), "{steal:?}");
    let gen = generation.to_string();
    let accept = [
        "accept",
        "--role",
        "localpilot",
        "--msg-id",
        posted.as_str(),
        "--generation",
        gen.as_str(),
    ];
    let no_token = f.reference_out(&accept, &[]);
    assert_eq!(no_token.status.code(), Some(5), "{no_token:?}");
    assert!(String::from_utf8_lossy(&no_token.stderr).contains("no_token"));
    let wrong = f.reference_out(&accept, &[("PAIR_ENDPOINT_TOKEN", &"0".repeat(64))]);
    assert_eq!(wrong.status.code(), Some(5), "{wrong:?}");
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("bad_token"));
    assert_eq!(
        f.endpoint_record("localpilot").unwrap()["address"],
        address.as_str()
    );

    // Wakes that are malformed or not for this endpoint are refused by code.
    let wake = |to: &str, session: &str, generation: i64| {
        json!({"v": 1, "op": "wake", "session_id": session, "to": to, "generation": generation,
               "msg_id": posted, "from": "claude"})
        .to_string()
            + "\n"
    };
    for (line, code) in [
        ("not json\n".to_owned(), "bad_request"),
        (wake("codex", &sid, generation), "wrong_role"),
        (
            wake("localpilot", "20000101T000000Z-00000000", generation),
            "wrong_session",
        ),
        (wake("localpilot", &sid, generation - 1), "stale_generation"),
    ] {
        let reply = wake_raw(&transport, &address, &line).await;
        assert!(
            reply.contains(&format!("\"reason\":\"{code}\"")),
            "{line} -> {reply}"
        );
    }
    assert!(
        wake_raw(&transport, &address, &wake("localpilot", &sid, generation))
            .await
            .contains("\"ok\":true")
    );

    // A STOP reaches it by wake too, and it retires its endpoint on the way out.
    f.reference(&[
        "post",
        "--role",
        "claude",
        "--kind",
        "STOP",
        "--to",
        "localpilot",
        "--body",
        "halt",
    ]);
    let status = tokio::task::spawn_blocking(move || {
        for _ in 0..150 {
            if let Some(s) = engine.try_wait().unwrap() {
                return Some(s);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = engine.kill();
        None
    })
    .await
    .unwrap();
    assert!(
        status.is_some_and(|s| s.success()),
        "the engine did not stop on the STOP"
    );
    assert_eq!(f.endpoint_record("localpilot").unwrap()["active"], false);
}

#[tokio::test]
async fn an_endpoint_taken_over_elsewhere_is_never_taken_back() {
    // Renewal is refused once the record belongs to someone else: the engine
    // stops listening and leaves that endpoint alone on exit.
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(AGREE).set_delay(std::time::Duration::from_secs(3)))
        .mount(&server)
        .await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    std::fs::write(f.anchor.join("a.txt"), "beta\n").unwrap();
    f.request_review(&format!("a.txt={}", f.fingerprint("a.txt")));
    let mut cmd = f.engine(
        &["--once", "--listen"],
        &[("LOCALPILOT_TEST_MESH_RENEW_MS", "200")],
    );
    let run = tokio::task::spawn_blocking(move || cmd.output().unwrap());
    until("the endpoint", || {
        f.endpoint_record("localpilot")
            .is_some_and(|e| e["active"] == true)
    })
    .await;
    let mut foreign = f.endpoint_record("localpilot").unwrap();
    foreign["generation"] = json!(999);
    foreign["token_sha256"] = json!("0".repeat(64));
    foreign["expires_at"] = json!("2099-01-01T00:00:00Z");
    let path = f
        .session_file()
        .parent()
        .unwrap()
        .join("endpoints")
        .join("localpilot.json");
    std::fs::write(&path, foreign.to_string()).unwrap();
    let out = run.await.unwrap();
    assert!(
        text(&out).contains("LISTEN_STOPPED role=localpilot"),
        "{}",
        text(&out)
    );
    let ep = f.endpoint_record("localpilot").unwrap();
    assert_eq!(
        (ep["active"].clone(), ep["generation"].clone()),
        (json!(true), json!(999)),
        "{ep}"
    );
}

/// Makes a participant's endpoint record unwritable while it lives: a
/// read-only file on Windows (a rename cannot replace it), a read-only
/// `endpoints/` directory elsewhere. Undone on drop.
struct Frozen(PathBuf);

impl Frozen {
    fn new(record: &Path) -> Self {
        let target = if cfg!(windows) {
            record.to_path_buf()
        } else {
            record.parent().unwrap().to_path_buf()
        };
        set_writable(&target, false);
        Self(target)
    }
}

impl Drop for Frozen {
    fn drop(&mut self) {
        set_writable(&self.0, true);
    }
}

#[cfg(windows)]
fn set_writable(p: &Path, on: bool) {
    let mut perm = std::fs::metadata(p).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(!on);
    std::fs::set_permissions(p, perm).unwrap();
}

#[cfg(unix)]
fn set_writable(p: &Path, on: bool) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        p,
        std::fs::Permissions::from_mode(if on { 0o755 } else { 0o555 }),
    )
    .unwrap();
}

#[tokio::test]
async fn a_lease_that_runs_out_while_renewal_fails_is_never_registered_again() {
    // A 2 s lease renewed every 200 ms. Renewal is made to fail (the record
    // cannot be written) for longer than the lease, then allowed again: the
    // engine must stop listening, not register over the expired record.
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(AGREE).set_delay(std::time::Duration::from_secs(5)))
        .mount(&server)
        .await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    std::fs::write(f.anchor.join("a.txt"), "beta\n").unwrap();
    f.request_review(&format!("a.txt={}", f.fingerprint("a.txt")));
    let mut cmd = f.engine(
        &["--once", "--listen"],
        &[
            ("LOCALPILOT_TEST_MESH_RENEW_MS", "200"),
            ("LOCALPILOT_TEST_MESH_TTL_S", "2"),
        ],
    );
    let run = tokio::task::spawn_blocking(move || cmd.output().unwrap());
    until("the endpoint", || {
        f.endpoint_record("localpilot")
            .is_some_and(|e| e["active"] == true)
    })
    .await;
    let record = f
        .session_file()
        .parent()
        .unwrap()
        .join("endpoints")
        .join("localpilot.json");
    let frozen_at = {
        let freeze = Frozen::new(&record);
        let generation = f.endpoint_record("localpilot").unwrap()["generation"].clone();
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        drop(freeze);
        generation
    };
    let out = run.await.unwrap();
    let text = text(&out);
    // Whether the local guard stops the next renewal or the renewal is
    // refused under the lock as expired depends on scheduling; either way the
    // listener must stop, and nothing may register again.
    assert!(text.contains("LISTEN_STOPPED role=localpilot:"), "{text}");
    let ep = f.endpoint_record("localpilot").unwrap();
    assert_eq!(
        ep["generation"], frozen_at,
        "registered again after the lease ran out: {ep}"
    );
}

#[tokio::test]
async fn a_renewal_that_waits_on_the_lock_past_the_lease_does_not_reclaim_it() {
    // The race a pre-call check cannot close: the renewal passes its guard,
    // then waits on the role lock until the lease has run out. Under the lock
    // it must find the lease expired and refuse, not register afresh.
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(says(AGREE).set_delay(std::time::Duration::from_secs(5)))
        .mount(&server)
        .await;
    let Some(f) = Fixture::new(&server) else {
        return;
    };
    std::fs::write(f.anchor.join("a.txt"), "beta\n").unwrap();
    f.request_review(&format!("a.txt={}", f.fingerprint("a.txt")));
    let mut cmd = f.engine(
        &["--once", "--listen"],
        &[
            ("LOCALPILOT_TEST_MESH_RENEW_MS", "200"),
            ("LOCALPILOT_TEST_MESH_TTL_S", "2"),
        ],
    );
    let run = tokio::task::spawn_blocking(move || cmd.output().unwrap());
    until("the endpoint", || {
        f.endpoint_record("localpilot")
            .is_some_and(|e| e["active"] == true)
    })
    .await;
    // Hold localpilot's role lock (the protocol's lock file) across the lease.
    let lock = f
        .session_file()
        .parent()
        .unwrap()
        .join("journal")
        .join("localpilot.lock");
    let held_at = loop {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            use std::io::Write as _;
            let _ = writeln!(file, "{{\"pid\": {}}}", std::process::id());
            break f.endpoint_record("localpilot").unwrap()["generation"].clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    tokio::time::sleep(std::time::Duration::from_millis(2600)).await;
    std::fs::remove_file(&lock).unwrap();
    let out = run.await.unwrap();
    let text = text(&out);
    assert!(text.contains("LISTEN_STOPPED role=localpilot"), "{text}");
    let ep = f.endpoint_record("localpilot").unwrap();
    assert_eq!(
        ep["generation"], held_at,
        "the lease was reclaimed after it ran out: {ep}"
    );
}
