//! Read-only detection of a local LocalBox install and a running LocalBox model
//! server.
//!
//! Answers two questions so `localpilot models` (and, later, the interactive
//! model flow) can point a user at a local model without hand-editing config:
//! is a `localbox` binary on `PATH`, and is a LocalBox server already serving —
//! at which endpoint? It never spawns a server, offers to start one, or writes
//! config; that authority lives in later, permission-gated steps. When LocalBox
//! is absent, detection returns [`LocalBoxState::NotInstalled`] after only a
//! cheap `PATH` scan, so every existing flow is left byte-for-byte unchanged.
//!
//! LocalBox exposes no machine-readable status — `localbox status` prints a
//! prose health line on its default ports, carrying no endpoint that could be
//! discovered from it. So the authoritative "is it serving, and where" signal
//! is a read-only probe of LocalBox's documented default proxy endpoint via the
//! existing [`localpilot_llm::discover_models`] — not a parse of that prose,
//! which would couple LocalPilot to a cross-repo wording that can drift.

use std::path::PathBuf;

use localpilot_llm::discover_models;

/// LocalBox's documented default no-think proxy endpoint. LocalBox launch and
/// `status` default to proxy `:11435` and backend `:8080` (`--proxy-port` /
/// `--server-port` override them); the proxy serves the OpenAI-compatible `/v1`
/// surface probed here. Mirrored from LocalBox's public defaults — never
/// imported from it.
const DEFAULT_PROXY_BASE_URL: &str = "http://127.0.0.1:11435/v1";

/// Where a detected LocalBox stands. Read-only; no runtime path can panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalBoxState {
    /// No `localbox` binary on `PATH`.
    NotInstalled,
    /// `localbox` is on `PATH`, but no server answered the endpoint probe.
    InstalledNotRunning,
    /// A LocalBox server is serving at `endpoint`; `model` is the first model it
    /// reports, when it reports one.
    Running {
        endpoint: String,
        model: Option<String>,
    },
}

/// Resolve `localbox` on `PATH` without a `which` dependency. Returns the first
/// `PATH` entry holding an executable named `localbox` (with the platform's
/// executable suffix), or `None` when none exists.
pub(crate) fn localbox_on_path() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path_var| localbox_in_paths(&path_var))
}

/// The `PATH`-splitting core of [`localbox_on_path`], taking the `PATH` value so
/// it can be tested without mutating the process environment.
fn localbox_in_paths(path_var: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path_var).find_map(|dir| {
        let candidate = dir.join(format!("localbox{}", std::env::consts::EXE_SUFFIX));
        candidate.is_file().then_some(candidate)
    })
}

/// Detect LocalBox: presence on `PATH`, then a read-only probe of the documented
/// default proxy endpoint. Never spawns, offers, or writes config.
pub(crate) async fn detect() -> LocalBoxState {
    detect_at(DEFAULT_PROXY_BASE_URL, localbox_on_path()).await
}

/// The endpoint-parameterised core of [`detect`]. `on_path` is the resolved
/// binary (or `None` when LocalBox is not installed); `base_url` is the endpoint
/// to probe. Kept separate so tests drive it against a mock server without a real
/// `localbox` on `PATH`.
async fn detect_at(base_url: &str, on_path: Option<PathBuf>) -> LocalBoxState {
    if on_path.is_none() {
        // Not installed: return immediately without probing, so an absent LocalBox
        // adds no network effect and changes no existing behaviour.
        return LocalBoxState::NotInstalled;
    }
    match discover_models(base_url, None).await {
        Ok(models) => LocalBoxState::Running {
            endpoint: base_url.to_string(),
            model: models.into_iter().next().map(|model| model.id),
        },
        // Installed but the endpoint did not answer as an OpenAI-compatible model
        // listing: not running (or not ready). Never a guess.
        Err(_) => LocalBoxState::InstalledNotRunning,
    }
}

/// What to offer a user who has hit a no-usable-model dead-end, given LocalBox
/// detection. This is the *decision* only — surfacing it (an enriched message at
/// startup, or a permission-gated prompt in the running session) and acting on it
/// belong to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelOffer {
    /// Nothing to offer — the caller shows its existing no-model error unchanged.
    NoOffer,
    /// A LocalBox server is already serving at `endpoint`; point the user at it.
    Running { endpoint: String },
    /// LocalBox is installed but not serving; suggest starting it.
    InstalledNotRunning,
}

/// Decide what to offer at a no-usable-model dead-end. Never overrides a working
/// model: when `usable_model_present`, the answer is always [`ModelOffer::NoOffer`],
/// so the offer only ever appears where the user is genuinely stuck.
pub(crate) fn offer_for(usable_model_present: bool, state: LocalBoxState) -> ModelOffer {
    if usable_model_present {
        return ModelOffer::NoOffer;
    }
    match state {
        LocalBoxState::Running { endpoint, .. } => ModelOffer::Running { endpoint },
        LocalBoxState::InstalledNotRunning => ModelOffer::InstalledNotRunning,
        LocalBoxState::NotInstalled => ModelOffer::NoOffer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn offers_only_when_no_usable_model_and_localbox_present() {
        assert_eq!(
            offer_for(
                false,
                LocalBoxState::Running {
                    endpoint: "http://127.0.0.1:11435/v1".to_string(),
                    model: None,
                },
            ),
            ModelOffer::Running {
                endpoint: "http://127.0.0.1:11435/v1".to_string(),
            }
        );
        assert_eq!(
            offer_for(false, LocalBoxState::InstalledNotRunning),
            ModelOffer::InstalledNotRunning
        );
        // Not installed → no offer (the caller keeps its exact legacy error).
        assert_eq!(
            offer_for(false, LocalBoxState::NotInstalled),
            ModelOffer::NoOffer
        );
    }

    #[test]
    fn no_offer_when_a_model_is_reachable() {
        // A usable model already exists — never offer, even if LocalBox is running.
        assert_eq!(
            offer_for(
                true,
                LocalBoxState::Running {
                    endpoint: "http://127.0.0.1:11435/v1".to_string(),
                    model: Some("qwen-coder".to_string()),
                },
            ),
            ModelOffer::NoOffer
        );
    }

    #[test]
    fn detects_localbox_absent_when_not_on_path() {
        // A PATH with no `localbox` binary resolves to None.
        let empty = tempfile::tempdir().unwrap();
        let path_var = std::env::join_paths([empty.path()]).unwrap();
        assert_eq!(localbox_in_paths(&path_var), None);
    }

    #[test]
    fn finds_localbox_on_path_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir
            .path()
            .join(format!("localbox{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&bin, b"").unwrap();
        let path_var = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(localbox_in_paths(&path_var), Some(bin));
    }

    #[tokio::test]
    async fn running_server_is_detected_via_default_endpoint_probe() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [ { "id": "qwen-coder", "object": "model" } ]
            })))
            .mount(&server)
            .await;
        let endpoint = format!("{}/v1", server.uri());
        let state = detect_at(&endpoint, Some(PathBuf::from("localbox"))).await;
        assert_eq!(
            state,
            LocalBoxState::Running {
                endpoint,
                model: Some("qwen-coder".to_string()),
            }
        );
    }

    #[tokio::test]
    async fn unreachable_endpoint_degrades_to_installed_not_running() {
        // Installed (path present) but nothing serving on the endpoint → not
        // running, reported as such rather than guessed.
        let state = detect_at("http://127.0.0.1:1/v1", Some(PathBuf::from("localbox"))).await;
        assert_eq!(state, LocalBoxState::InstalledNotRunning);
    }

    #[tokio::test]
    async fn absent_localbox_never_probes_and_is_not_installed() {
        // With no binary on PATH, detection returns NotInstalled without probing:
        // the pre-existing behaviour is untouched (the seam is inert).
        let state = detect_at("http://127.0.0.1:1/v1", None).await;
        assert_eq!(state, LocalBoxState::NotInstalled);
    }
}
