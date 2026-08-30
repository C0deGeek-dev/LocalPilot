//! Opt-in live test against the official OpenAI API. It is skipped unless both
//! `LOCALPILOT_LIVE_TESTS` and `OPENAI_API_KEY` are set, so it never runs in
//! default CI and needs no credentials by default (docs/08 Live Tests).

use futures::StreamExt;
use localpilot_core::{Message, Role, Secret};
use localpilot_llm::{ModelEvent, ModelProvider, ModelRequest, OpenAiProvider, SourceType};

#[tokio::test]
async fn live_openai_text_completion() {
    if std::env::var("LOCALPILOT_LIVE_TESTS").is_err() {
        eprintln!("skipping live test: set LOCALPILOT_LIVE_TESTS to enable");
        return;
    }
    let key = match std::env::var("OPENAI_API_KEY") {
        Ok(key) if !key.trim().is_empty() => key,
        _ => {
            eprintln!("skipping live test: OPENAI_API_KEY is not set");
            return;
        }
    };

    let provider = OpenAiProvider::new(
        "openai",
        "OpenAI",
        SourceType::OfficialApi,
        "https://api.openai.com/v1",
        Some(Secret::new(key)),
    );
    let request = ModelRequest::new(
        "gpt-4o-mini",
        vec![Message::text(
            Role::User,
            "Reply with the single word: hello",
        )],
    );
    let mut stream = provider.stream(request).await.expect("stream should start");

    let mut saw_text = false;
    while let Some(event) = stream.next().await {
        if let Ok(ModelEvent::TextDelta(_)) = event {
            saw_text = true;
        }
    }
    assert!(
        saw_text,
        "expected at least one text delta from the live API"
    );
}
