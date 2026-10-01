//! Dynamic model discovery on OpenAI-compatible servers.
//!
//! Queries the public `GET /models` endpoint (the OpenAI-compatible model
//! listing implemented by Ollama, vLLM, llama.cpp's server, and local
//! gateways) so `localpilot models` lists what is actually loaded. Context
//! length is read best-effort from the non-standard fields common servers
//! attach; absence degrades to `None`, never an error.

use std::time::Duration;

use localpilot_core::Secret;
use serde_json::Value;

use localpilot_llm_core::auth::AuthProvider;
use localpilot_llm_core::error::ProviderError;

/// A model reported by a server's model listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    /// The model id as the server reports it (what `--model` expects).
    pub id: String,
    /// The model's context window in tokens, when the server reports one.
    pub context_window: Option<u64>,
    /// Whether the loaded model accepts image (vision) input, from a best-effort
    /// read-only server probe ([`probe_vision`]). `None` when the server was not
    /// probed or exposes no vision signal — never a guessed value. The listing
    /// itself does not report this, so [`discover_models`] leaves it `None`; a
    /// caller that probes stamps it on.
    pub vision: Option<bool>,
}

/// Default timeout for a discovery request: listing models is interactive
/// metadata, not inference.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// The metadata endpoint that reported a served window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContextWindowSource {
    ServerProps,
    ModelListing,
}

impl ContextWindowSource {
    /// A stable diagnostic label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ServerProps => "server_props",
            Self::ModelListing => "model_listing",
        }
    }
}

/// Positive context capacity reported by the selected model's server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerContextWindow {
    pub tokens: u64,
    pub source: ContextWindowSource,
}

/// Probe a server's served context window without running inference.
///
/// For a user-run compatible server, `probe_props` enables the documented
/// llama.cpp `/props` endpoint before the matching `/models` entry. Router GETs
/// name the model and disable autoload. Official APIs use only their listing.
/// Both requests together have a two-second ceiling and never follow redirects.
/// Missing, malformed or unreachable metadata returns `None`; training context
/// is not a served window.
pub async fn probe_context_window(
    base_url: &str,
    model: &str,
    api_key: Option<&Secret>,
    probe_props: bool,
) -> Option<ServerContextWindow> {
    probe_context_with_auth(
        base_url,
        model,
        DiscoveryAuth::from_api_key(api_key),
        probe_props,
        Duration::from_secs(2),
    )
    .await
}

/// [`probe_context_window`] using a dynamic bearer-token provider. Token
/// acquisition shares the probe's timeout; failures retain the normal fallback.
pub async fn probe_context_window_with_auth_provider(
    base_url: &str,
    model: &str,
    auth_provider: &dyn AuthProvider,
    probe_props: bool,
) -> Option<ServerContextWindow> {
    probe_context_with_auth(
        base_url,
        model,
        DiscoveryAuth::Dynamic(auth_provider),
        probe_props,
        Duration::from_secs(2),
    )
    .await
}

async fn probe_context_with_auth(
    base_url: &str,
    model: &str,
    auth: DiscoveryAuth<'_>,
    probe_props: bool,
    timeout: Duration,
) -> Option<ServerContextWindow> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    tokio::time::timeout(timeout, async {
        if probe_props {
            let request = client
                .get(format!("{}/props", server_root(base_url)))
                .query(&[("model", model), ("autoload", "false")]);
            if let Some(body) = probe_json(request, &auth).await {
                if let Some(tokens) = positive_tokens(&body["default_generation_settings"]["n_ctx"])
                {
                    return Some(ServerContextWindow {
                        tokens,
                        source: ContextWindowSource::ServerProps,
                    });
                }
            }
        }
        let request = client.get(format!("{}/models", base_url.trim_end_matches('/')));
        let body = probe_json(request, &auth).await?;
        let entry = body["data"]
            .as_array()?
            .iter()
            .find(|entry| entry["id"].as_str() == Some(model))?;
        context_window_of(entry).map(|tokens| ServerContextWindow {
            tokens,
            source: ContextWindowSource::ModelListing,
        })
    })
    .await
    .ok()
    .flatten()
}

async fn probe_json(
    mut request: reqwest::RequestBuilder,
    auth: &DiscoveryAuth<'_>,
) -> Option<Value> {
    match auth {
        DiscoveryAuth::None => {}
        DiscoveryAuth::ApiKey(key) => request = request.bearer_auth(key.expose()),
        DiscoveryAuth::Dynamic(provider) => {
            let token = provider.access_token().await.ok()?;
            request = request.bearer_auth(token.expose());
        }
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

fn positive_tokens(value: &Value) -> Option<u64> {
    value.as_u64().filter(|tokens| *tokens > 0)
}

/// List the models an OpenAI-compatible server reports.
///
/// # Errors
/// Returns [`ProviderError`] when the server cannot be reached or the
/// response is not a model listing.
pub async fn discover_models(
    base_url: &str,
    api_key: Option<&Secret>,
) -> Result<Vec<DiscoveredModel>, ProviderError> {
    discover_models_with_auth(base_url, DiscoveryAuth::from_api_key(api_key)).await
}

/// List models using a dynamic bearer token provider.
///
/// # Errors
/// Returns [`ProviderError`] when the server cannot be reached, authentication
/// cannot produce a token, or the response is not a model listing.
pub async fn discover_models_with_auth_provider(
    base_url: &str,
    auth_provider: &dyn AuthProvider,
) -> Result<Vec<DiscoveredModel>, ProviderError> {
    discover_models_with_auth(base_url, DiscoveryAuth::Dynamic(auth_provider)).await
}

enum DiscoveryAuth<'a> {
    None,
    ApiKey(&'a Secret),
    Dynamic(&'a dyn AuthProvider),
}

impl<'a> DiscoveryAuth<'a> {
    fn from_api_key(api_key: Option<&'a Secret>) -> Self {
        api_key.map_or(Self::None, Self::ApiKey)
    }
}

async fn discover_models_with_auth(
    base_url: &str,
    auth: DiscoveryAuth<'_>,
) -> Result<Vec<DiscoveredModel>, ProviderError> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(DISCOVERY_TIMEOUT)
        .build()
        .map_err(|e| ProviderError::Network(e.to_string()))?;
    let mut request = client.get(&url);
    match auth {
        DiscoveryAuth::None => {}
        DiscoveryAuth::ApiKey(key) => {
            // The credential is set as a header here and never logged.
            request = request.bearer_auth(key.expose());
        }
        DiscoveryAuth::Dynamic(provider) => {
            let token = provider.access_token().await?;
            request = request.bearer_auth(token.expose());
        }
    }
    let response = request.send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(ProviderError::from_http(
            status.as_u16(),
            None,
            None,
            localpilot_llm_core::error::QuotaInfo::default(),
        ));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|e| ProviderError::StreamDecode(e.to_string()))?;
    let entries = body["data"]
        .as_array()
        .ok_or_else(|| ProviderError::StreamDecode("model listing has no `data` array".into()))?;
    Ok(entries.iter().filter_map(parse_model).collect())
}

fn parse_model(entry: &Value) -> Option<DiscoveredModel> {
    let id = entry["id"].as_str()?.to_string();
    Some(DiscoveredModel {
        id,
        context_window: context_window_of(entry),
        // The model listing carries no vision signal; a caller probes for it.
        vision: None,
    })
}

/// Best-effort, read-only vision probe of a llama.cpp `llama-server`.
///
/// `llama-server` exposes a documented `GET /props` endpoint that reports the
/// loaded model's `modalities` (set when a multimodal projector is loaded via
/// `--mmproj`). This reads `modalities.vision` and runs **no model inference**.
/// `/props` is served at the server root, while the OpenAI-compatible endpoints
/// live under `/v1`, so a trailing `/v1` is stripped from `base_url` first.
///
/// Returns `Some(true|false)` only when the server reports the field; `None` when
/// the server is unreachable, returns a non-success status, or exposes no such
/// field (an older server, or a different OpenAI-compatible backend). It never
/// returns an error — an unknown capability is `None`, never a guess.
///
/// Provenance: implemented from the public llama.cpp server documentation
/// (`tools/server/README.md`, the `GET /props` `modalities` field). No private or
/// undocumented endpoint behaviour is used.
pub async fn probe_vision(base_url: &str, api_key: Option<&Secret>) -> Option<bool> {
    let url = format!("{}/props", server_root(base_url));
    let client = reqwest::Client::builder()
        .timeout(DISCOVERY_TIMEOUT)
        .build()
        .ok()?;
    let mut request = client.get(&url);
    if let Some(key) = api_key {
        // The credential rides as a header and is never logged.
        request = request.bearer_auth(key.expose());
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    body.get("modalities")
        .and_then(|modalities| modalities.get("vision"))
        .and_then(Value::as_bool)
}

/// The server root for a `/props` probe. `llama-server` serves `/props` at the
/// root, while the OpenAI-compatible endpoints live under `/v1`; strip a trailing
/// `/v1` (with or without a trailing slash) so the probe targets the right path.
fn server_root(base_url: &str) -> &str {
    let trimmed = base_url.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed)
}

/// Best-effort context length from the non-standard fields common servers
/// attach to their model listings.
fn context_window_of(entry: &Value) -> Option<u64> {
    if let Some(value) = positive_tokens(&entry["meta"]["n_ctx"]) {
        return Some(value);
    }
    for key in [
        "context_length",
        "max_model_len",
        "max_context_length",
        "n_ctx",
    ] {
        if let Some(value) = positive_tokens(&entry[key]) {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn context_props_are_model_routed_no_autoload_and_authoritative_per_slot() {
        use wiremock::matchers::{header, query_param};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .and(query_param("model", "model/a:Q4"))
            .and(query_param("autoload", "false"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "default_generation_settings": {"n_ctx": 262144}, "total_slots": 4,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let secret = Secret::new("test-token");
        let window = probe_context_window(
            &format!("{}/v1/", server.uri()),
            "model/a:Q4",
            Some(&secret),
            true,
        )
        .await
        .unwrap();
        assert_eq!(window.tokens, 262_144);
        assert_eq!(window.source, ContextWindowSource::ServerProps);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn context_listing_fallback_accepts_served_metadata_and_legacy_vllm_fields() {
        for entry in [
            serde_json::json!({"id":"selected", "meta":{"n_ctx":32768, "n_ctx_train":131072}, "context_length":65536}),
            serde_json::json!({"id":"selected", "max_model_len":32768}),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/props"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v1/models"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":[
                        {"id":"other", "n_ctx":999999}, entry
                    ]})),
                )
                .mount(&server)
                .await;
            let window =
                probe_context_window(&format!("{}/v1", server.uri()), "selected", None, true)
                    .await
                    .unwrap();
            assert_eq!(window.tokens, 32768);
            assert_eq!(window.source, ContextWindowSource::ModelListing);
        }
    }

    #[tokio::test]
    async fn context_rejects_training_only_zero_negative_and_wrong_model_metadata() {
        for entry in [
            serde_json::json!({"id":"selected", "meta":{"n_ctx_train":131072}}),
            serde_json::json!({"id":"selected", "meta":{"n_ctx":0}, "n_ctx":0}),
            serde_json::json!({"id":"selected", "n_ctx":-1}),
            serde_json::json!({"id":"other", "n_ctx":131072}),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/models"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":[entry]})),
                )
                .mount(&server)
                .await;
            assert_eq!(
                probe_context_window(&server.uri(), "selected", None, false).await,
                None
            );
        }
    }

    #[tokio::test]
    async fn context_probe_has_one_timeout_for_the_whole_sequence_and_refuses_redirects() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
            .mount(&server)
            .await;
        assert_eq!(
            probe_context_with_auth(
                &server.uri(),
                "selected",
                DiscoveryAuth::None,
                true,
                Duration::from_millis(50)
            )
            .await,
            None
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        let redirect = MockServer::start().await;
        let target = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/models", target.uri())),
            )
            .mount(&redirect)
            .await;
        assert_eq!(
            probe_context_window(&redirect.uri(), "selected", None, false).await,
            None
        );
        assert!(target.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn lists_models_with_best_effort_context_length() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    { "id": "qwen-coder", "object": "model", "max_model_len": 32768 },
                    { "id": "llama-small", "object": "model" },
                ]
            })))
            .mount(&server)
            .await;

        let models = discover_models(&format!("{}/v1", server.uri()), None)
            .await
            .unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "qwen-coder");
        assert_eq!(models[0].context_window, Some(32_768));
        assert_eq!(models[1].context_window, None);
    }

    #[tokio::test]
    async fn a_non_listing_response_is_a_typed_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })),
            )
            .mount(&server)
            .await;
        assert!(matches!(
            discover_models(&format!("{}/v1", server.uri()), None).await,
            Err(ProviderError::StreamDecode(_))
        ));
    }

    #[tokio::test]
    async fn an_error_status_maps_through_the_taxonomy() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        assert!(matches!(
            discover_models(&format!("{}/v1", server.uri()), None).await,
            Err(ProviderError::Auth { .. })
        ));
    }

    async fn props_server(body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        // `/props` is served at the root, not under `/v1`.
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn props_reporting_a_loaded_projector_probes_vision_true() {
        let server = props_server(serde_json::json!({
            "modalities": { "vision": true },
            "total_slots": 1
        }))
        .await;
        // A `/v1` base is stripped to the server root before probing `/props`.
        assert_eq!(
            probe_vision(&format!("{}/v1", server.uri()), None).await,
            Some(true)
        );
    }

    #[tokio::test]
    async fn props_reporting_no_projector_probes_vision_false() {
        let server = props_server(serde_json::json!({
            "modalities": { "vision": false }
        }))
        .await;
        assert_eq!(probe_vision(&server.uri(), None).await, Some(false));
    }

    #[tokio::test]
    async fn props_without_a_modalities_field_is_unknown() {
        let server = props_server(serde_json::json!({ "total_slots": 1 })).await;
        assert_eq!(probe_vision(&server.uri(), None).await, None);
    }

    #[tokio::test]
    async fn a_missing_props_endpoint_is_unknown() {
        // A server with no `/props` (a 404) — an older build or a different
        // OpenAI-compatible backend — yields `None`, never a guessed capability.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        assert_eq!(
            probe_vision(&format!("{}/v1", server.uri()), None).await,
            None
        );
    }

    #[tokio::test]
    async fn an_unreachable_server_is_unknown() {
        // A closed port resolves to `None` (best-effort), not an error.
        assert_eq!(probe_vision("http://127.0.0.1:1/v1", None).await, None);
    }
}
