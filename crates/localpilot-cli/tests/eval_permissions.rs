//! Evaluation permission setup is validated before a model request or grading.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use assert_cmd::Command;
use std::path::Path;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(root: &Path, config: &Path, server: &str, grant: bool) {
    std::fs::create_dir_all(config.join("localpilot")).unwrap();
    std::fs::write(config.join("localpilot/config.toml"), if grant {
        "[[permissions.allow_commands]]\nprogram = \"python\"\nargs_prefix = [\"-m\", \"unittest\", \"discover\", \"-s\", \"tests\"]\n"
    } else { "" }).unwrap();
    std::fs::write(root.join(".localpilot.toml"), format!("[provider]\ndefault = \"local\"\n[providers.local]\nkind = \"openai-compatible\"\nbase_url = \"{server}\"\n")).unwrap();
    std::fs::create_dir(root.join(".localmind")).unwrap();
    std::fs::write(
        root.join(".localmind.toml"),
        "[learning]\nenabled = false\n",
    )
    .unwrap();
    std::fs::create_dir(root.join("tests")).unwrap();
    std::fs::write(root.join("tests/test_example.py"), "import unittest\nclass Example(unittest.TestCase):\n    def test_pass(self):\n        self.assertTrue(True)\n").unwrap();
    std::fs::write(root.join("README.md"), "Permission fixture\n").unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "README.md"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap()
            .success());
    }
}

async fn run(grant: bool) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n",
                ),
        )
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    fixture(root.path(), config.path(), &server.uri(), grant);
    let workspace = root.path().to_path_buf();
    let user_config = config.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_localpilot"))
            .current_dir(workspace)
            .env("APPDATA", &user_config)
            .env("XDG_CONFIG_HOME", &user_config)
            .args([
                "eval",
                "--model",
                "fixture",
                "--permission",
                "bypass",
                "--verify-command",
                "python -m unittest discover -s tests",
                "inspect the fixture",
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let requests = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| {
            request.url.path() == "/chat/completions" && request.method.as_str() == "POST"
        })
        .collect::<Vec<_>>();
    if grant {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let card: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(card["process"]["exit_reason"], "Done");
        assert!(!requests.is_empty());
        let request: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(request["messages"]
            .to_string()
            .contains("Evaluation test operation"));
        // The gate actually completed the granted command, not just the preflight.
        let store = localpilot_store::Store::open(root.path());
        let session = store.list_sessions().unwrap()[0].id;
        assert!(store
            .read_events(session)
            .unwrap()
            .iter()
            .any(|e| matches!(&e.kind, localpilot_store::SessionEventKind::CheckRan { status, .. } if status == "passed")));
    } else {
        assert!(!output.status.success());
        assert!(
            output.stdout.is_empty(),
            "a denied baseline must not emit a scorecard"
        );
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("evaluation verification permission preflight failed:"));
        assert!(
            requests.is_empty(),
            "do not spend a model request on a denied verifier"
        );
        let index = std::process::Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(index.stdout.is_empty(), "do not stage an invalid baseline");
    }
}

#[tokio::test]
async fn denied_eval_verification_exits_before_the_model_request() {
    run(false).await;
}

#[tokio::test]
async fn granted_eval_test_operation_reaches_the_model_and_verifier() {
    run(true).await;
}
