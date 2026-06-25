//! HTTP-adapter tests against a local mock server. No real credentials or
//! network access; every response is scripted by `wiremock`.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use localpilot_core::{Message, Role};
use localpilot_llm::{
    AccessToken, AuthProvider, ModelEvent, ModelEventStream, ModelProvider, ModelRequest,
    OpenAiProvider, ProviderError, SourceType, ToolSpec,
};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Debug)]
struct StaticAuthProvider {
    token: String,
}

#[async_trait::async_trait]
impl AuthProvider for StaticAuthProvider {
    async fn access_token(&self) -> Result<AccessToken, ProviderError> {
        Ok(AccessToken::new(self.token.clone()))
    }
}

/// `ModelEventStream` is not `Debug`, so `unwrap_err` is unavailable; extract the
/// error by matching instead.
fn into_err(result: Result<ModelEventStream, ProviderError>) -> ProviderError {
    match result {
        Ok(_) => panic!("expected an error, got a stream"),
        Err(e) => e,
    }
}

fn provider(base_url: String) -> OpenAiProvider {
    OpenAiProvider::new("local", "Local", SourceType::LocalServer, base_url, None)
}

fn request() -> ModelRequest {
    ModelRequest::new("test-model", vec![Message::text(Role::User, "hi")])
}

async fn mock(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(response)
        .mount(server)
        .await;
}

#[tokio::test]
async fn dynamic_auth_provider_sets_bearer_token_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer adc-access-token",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: [DONE]\n\n"),
        )
        .mount(&server)
        .await;

    let auth = Arc::new(StaticAuthProvider {
        token: "adc-access-token".to_string(),
    });
    let provider = OpenAiProvider::new_with_auth_provider(
        "gemini",
        "Gemini",
        SourceType::OfficialApi,
        server.uri(),
        auth,
    );

    let events: Vec<_> = provider.stream(request()).await.unwrap().collect().await;
    assert!(matches!(events.last(), Some(Ok(ModelEvent::Done))));
}

#[tokio::test]
async fn streams_text_from_a_chunked_sse_response() {
    let server = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n\
               data: {\"choices\":[{\"delta\":{\"content\":\", world\"}}]}\n\n\
               data: {\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2},\"choices\":[{\"delta\":{}}]}\n\n\
               data: [DONE]\n\n";
    mock(
        &server,
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse),
    )
    .await;

    let events: Vec<_> = provider(server.uri())
        .stream(request())
        .await
        .unwrap()
        .collect()
        .await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            Ok(ModelEvent::TextDelta(t)) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello, world");
    assert!(events
        .iter()
        .any(|e| matches!(e, Ok(ModelEvent::Usage(u)) if u.output_tokens == 2)));
    assert!(matches!(events.last(), Some(Ok(ModelEvent::Done))));
}

#[tokio::test]
async fn quota_exhaustion_is_classified_with_reset_metadata() {
    let server = MockServer::start().await;
    mock(
        &server,
        ResponseTemplate::new(429)
            .insert_header("retry-after", "30")
            .insert_header("x-request-id", "req_123")
            .set_body_string("{\"error\":{\"code\":\"insufficient_quota\"}}"),
    )
    .await;

    let err = into_err(provider(server.uri()).stream(request()).await);
    match err {
        ProviderError::Quota { quota } => {
            assert_eq!(quota.retry_after, Some(Duration::from_secs(30)));
            assert!(quota.retryable);
        }
        other => panic!("expected Quota, got {other:?}"),
    }
}

#[tokio::test]
async fn rate_limit_without_quota_code_is_a_rate_limit() {
    let server = MockServer::start().await;
    mock(
        &server,
        ResponseTemplate::new(429)
            .insert_header("retry-after", "5")
            .set_body_string("{\"error\":{\"code\":\"rate_limit_exceeded\"}}"),
    )
    .await;

    let err = into_err(provider(server.uri()).stream(request()).await);
    assert!(matches!(err, ProviderError::RateLimit { .. }));
}

#[tokio::test]
async fn invalid_request_preserves_provider_message() {
    let server = MockServer::start().await;
    mock(
        &server,
        ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"code":"bad_request","message":"messages[2].role is unsupported"}}"#,
        ),
    )
    .await;

    let err = into_err(provider(server.uri()).stream(request()).await);
    match err {
        ProviderError::InvalidRequest { message } => {
            assert!(message.contains("messages[2].role is unsupported"));
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn auth_failure_is_classified() {
    let server = MockServer::start().await;
    mock(
        &server,
        ResponseTemplate::new(401).set_body_string("{\"error\":{\"code\":\"invalid_api_key\"}}"),
    )
    .await;

    let err = into_err(provider(server.uri()).stream(request()).await);
    assert!(matches!(err, ProviderError::Auth { .. }));
}

#[tokio::test]
async fn malformed_stream_body_yields_a_typed_decode_error() {
    let server = MockServer::start().await;
    mock(
        &server,
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string("data: {definitely not json}\n\n"),
    )
    .await;

    let events: Vec<_> = provider(server.uri())
        .stream(request())
        .await
        .unwrap()
        .collect()
        .await;
    assert!(events
        .iter()
        .any(|e| matches!(e, Err(ProviderError::StreamDecode(_)))));
}

#[tokio::test]
async fn response_body_decode_error_during_stream_is_not_reported_as_network() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut socket, _peer)) = listener.accept().await {
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            let response = concat!(
                "HTTP/1.1 200 OK\r\n",
                "content-type: text/event-stream\r\n",
                "transfer-encoding: chunked\r\n",
                "\r\n",
                "not-a-valid-chunk-size\r\n"
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });

    let events: Vec<_> = provider(format!("http://{addr}"))
        .stream(request())
        .await
        .unwrap()
        .collect()
        .await;

    assert!(events.iter().any(|event| matches!(
        event,
        Err(ProviderError::StreamDecode(message))
            if message.contains("response body read failed after stream opened")
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        Err(ProviderError::Network(message)) if message.contains("decoding response body")
    )));
}

/// Boundary fixture: the exact stream shape LocalBox's gateway proxy emits
/// for an Anthropic-format session (its own test suite pins the producer
/// side). Characteristics the adapter must tolerate: think-stripped deltas
/// that arrive empty, a synthetic `content_block_delta` the proxy injects
/// ahead of `content_block_stop` to flush held-back text, and the proxy's
/// `[no output]` fallback marker for all-reasoning blocks.
#[tokio::test]
async fn anthropic_adapter_consumes_a_localbox_proxy_shaped_stream() {
    use localpilot_llm::AnthropicProvider;

    let server = MockServer::start().await;
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":7}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let provider =
        AnthropicProvider::new("localbox", "LocalBox", format!("{}/v1", server.uri()), None);
    let events: Vec<_> = provider.stream(request()).await.unwrap().collect().await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            Ok(ModelEvent::TextDelta(t)) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello world");
    assert!(matches!(events.last(), Some(Ok(ModelEvent::Done))));
}

#[tokio::test]
async fn a_rejected_constraint_falls_back_to_native_tool_calling() {
    let server = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
               data: [DONE]\n\n";

    // A request carrying the schema constraint (response_format) is rejected.
    Mock::given(method("POST"))
        .and(body_string_contains("response_format"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string("{\"error\":{\"message\":\"unsupported response_format\"}}"),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    // The unconstrained retry (native tool-calling) succeeds.
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .with_priority(5)
        .mount(&server)
        .await;

    let req = request()
        .with_tools(vec![ToolSpec {
            name: "read_file".to_string(),
            description: "read".to_string(),
            input_schema: serde_json::json!({ "type": "object" }),
        }])
        .with_tool_constraint(Some(serde_json::json!({ "oneOf": [] })));

    let events: Vec<_> = provider(server.uri())
        .stream(req)
        .await
        .expect("the rejected constraint must fall back, not break the turn")
        .collect()
        .await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            Ok(ModelEvent::TextDelta(t)) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text, "ok",
        "the turn completed via native tool-calling after the constraint was rejected"
    );
}
