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

use crate::auth::AuthProvider;
use crate::error::ProviderError;

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
    auth_provider: &(dyn AuthProvider),
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
            crate::error::QuotaInfo::default(),
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
    for key in [
        "context_length",
        "max_model_len",
        "max_context_length",
        "n_ctx",
    ] {
        if let Some(value) = entry[key].as_u64() {
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
