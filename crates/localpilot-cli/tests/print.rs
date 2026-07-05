//! End-to-end test for `localpilot print` (non-interactive agent run).
#![allow(clippy::unwrap_used)]

use assert_cmd::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn print_mode_emits_an_answer_and_makes_no_workspace_writes() {
    let server = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"the answer\"}}]}\n\n\
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
    let output = tokio::task::spawn_blocking(move || {
        localpilot_cmd()
            .current_dir(&workdir)
            .args(["print", "--model", "m", "summarize"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    assert!(output.status.success(), "print failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("the answer"), "unexpected output: {stdout}");

    // No source files were created by a text-only run (only the config and the
    // gitignored .localpilot state dir exist).
    let source_files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != ".localpilot" && n != ".localpilot.toml")
        .collect();
    assert!(
        source_files.is_empty(),
        "unexpected writes: {source_files:?}"
    );
}

fn localpilot_cmd() -> Command {
    // The prebuilt test binary — never `cargo run` inside a test: nested
    // cargo fights the build-dir lock under nextest (a hang on Linux, an
    // exe-in-use failure on Windows) and re-resolves features.
    Command::new(env!("CARGO_BIN_EXE_localpilot"))
}
