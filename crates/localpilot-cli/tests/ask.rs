//! End-to-end test for `localpilot ask` against a mock OpenAI-compatible server.
//! This exercises the Phase 2 "done when": a text-only ask streams an answer
//! through configuration, the registry, and the provider — offline.

use assert_cmd::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn ask_streams_text_from_a_configured_local_provider() {
    let server = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi \"}}]}\n\n\
               data: {\"choices\":[{\"delta\":{\"content\":\"there\"}}]}\n\n\
               data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".localpilot.toml"),
        format!(
            "[provider]\ndefault = \"local\"\n\n[providers.local]\nkind = \"openai-compatible\"\nbase_url = \"{}\"\n",
            server.uri()
        ),
    )
    .unwrap();

    let workdir = dir.path().to_path_buf();
    // assert_cmd is blocking; run it off the async reactor so the mock server
    // keeps serving while the spawned binary connects to it.
    let output = tokio::task::spawn_blocking(move || {
        localpilot_cmd()
            .current_dir(&workdir)
            .args([
                "ask",
                "--provider",
                "local",
                "--model",
                "test-model",
                "hello",
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    assert!(output.status.success(), "ask failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Hi there"), "unexpected output: {stdout}");
}

fn localpilot_cmd() -> Command {
    // The prebuilt test binary — never `cargo run` inside a test: nested
    // cargo fights the build-dir lock under nextest (a hang on Linux, an
    // exe-in-use failure on Windows) and re-resolves features.
    Command::new(env!("CARGO_BIN_EXE_localpilot"))
}
