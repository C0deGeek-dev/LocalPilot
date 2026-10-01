//! One context-window resolution for every session host and diagnostic.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, OnceLock, PoisonError},
};

use localpilot_config::Config;
use localpilot_llm::{ContextWindowSource, ServerContextWindow};
use tokio::sync::OnceCell;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Server(ContextWindowSource),
    Config,
    Default,
}

impl Source {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Server(source) => source.as_str(),
            Self::Config => "config",
            Self::Default => "default",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    pub(crate) tokens: u64,
    pub(crate) source: Source,
    pub(crate) server: Option<ServerContextWindow>,
    pub(crate) configured: Option<u64>,
}

impl Window {
    pub(crate) fn from_signals(
        server: Option<ServerContextWindow>,
        configured: Option<u64>,
        fallback: usize,
    ) -> Self {
        let configured = configured.filter(|tokens| *tokens > 0);
        let (tokens, source) = match (server, configured) {
            (Some(server), Some(cap)) if cap < server.tokens => (cap, Source::Config),
            (Some(server), _) => (server.tokens, Source::Server(server.source)),
            (None, Some(cap)) => (cap, Source::Config),
            (None, None) => (u64::try_from(fallback).unwrap_or(u64::MAX), Source::Default),
        };
        Self {
            tokens,
            source,
            server,
            configured,
        }
    }

    /// A default is a prompt budget already, whereas known windows reserve output.
    pub(crate) fn known_window(self) -> Option<u64> {
        (self.source != Source::Default).then_some(self.tokens)
    }

    pub(crate) fn budget(self, fallback: usize, max_output: Option<u64>) -> usize {
        localpilot_harness::effective_context_limit(self.known_window(), fallback, max_output)
    }

    pub(crate) fn warning(self) -> Option<String> {
        let (server, configured) = (self.server?, self.configured?);
        (server.tokens != configured).then(|| {
            format!(
                "configured context_window {configured} differs from the server's {}; using {}",
                server.tokens, self.tokens
            )
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Resolution {
    pub(crate) window: Window,
    pub(crate) fresh: bool,
}

impl Resolution {
    pub(crate) fn warning_once(self) -> Option<String> {
        self.fresh.then(|| self.window.warning()).flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    provider: String,
    endpoint: Option<String>,
    model: String,
    configured: Option<u64>,
    fallback: usize,
    enabled: bool,
}

type Cache = Mutex<HashMap<Key, Arc<OnceCell<Window>>>>;
static CACHE: OnceLock<Cache> = OnceLock::new();

/// The endpoint is configured by the user; metadata carries no prompt content.
/// Credentials resolve only on an uncached probe and are never stored in the key.
pub(crate) async fn resolve(
    config: &Config,
    provider: &str,
    model: &str,
    declared: Option<u64>,
) -> Resolution {
    resolve_with_probe(config, provider, model, declared, async {
        let entry = config.providers.get(provider)?;
        let base = localpilot_llm::model_listing_base_url(entry)?;
        let props = matches!(
            entry.kind.as_str(),
            "local" | "openai-compatible" | "custom" | "custom-user-endpoint"
        );
        match localpilot_llm::discovery_auth_provider_from_config(config, provider).ok()? {
            Some(auth) => {
                localpilot_llm::probe_context_window_with_auth_provider(
                    &base,
                    model,
                    auth.as_ref(),
                    props,
                )
                .await
            }
            None => {
                let credential = config.resolve_credential(provider);
                localpilot_llm::probe_context_window(&base, model, credential.as_ref(), props).await
            }
        }
    })
    .await
}

/// Injectable network seam for hermetic cache and runtime-construction tests.
pub(crate) async fn resolve_with_probe(
    config: &Config,
    provider: &str,
    model: &str,
    declared: Option<u64>,
    probe: impl Future<Output = Option<ServerContextWindow>>,
) -> Resolution {
    let entry = config.providers.get(provider);
    let configured = entry.and_then(|entry| entry.context_window).or(declared);
    let key = Key {
        provider: provider.to_owned(),
        endpoint: entry.and_then(localpilot_llm::model_listing_base_url),
        model: model.to_owned(),
        configured,
        fallback: config.harness.context_token_limit,
        enabled: config.discovery.context_probe,
    };
    let cell = {
        let mut cache = CACHE
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            cache
                .entry(key)
                .or_insert_with(|| Arc::new(OnceCell::new())),
        )
    };
    let mut fresh = false;
    let window = *cell
        .get_or_init(|| async {
            fresh = true;
            let server = if config.discovery.context_probe {
                probe.await
            } else {
                None
            };
            Window::from_signals(server, configured, config.harness.context_token_limit)
        })
        .await;
    Resolution { window, fresh }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use localpilot_config::ProviderConfig;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    pub(crate) fn config_for(base: &str, cap: Option<u64>) -> Config {
        let mut config = Config::default();
        config.providers.insert(
            "context-test".to_owned(),
            ProviderConfig {
                kind: "local".to_owned(),
                base_url: Some(base.to_owned()),
                context_window: cap,
                ..ProviderConfig::default()
            },
        );
        config
    }

    #[test]
    fn context_caps_and_output_reserves_use_the_served_window() {
        for (server, cap, expected, source) in [
            (262_144, Some(8192), 8192, Source::Config),
            (
                32768,
                Some(131_072),
                32768,
                Source::Server(ContextWindowSource::ServerProps),
            ),
            (
                262_144,
                None,
                262_144,
                Source::Server(ContextWindowSource::ServerProps),
            ),
        ] {
            let window = Window::from_signals(
                Some(ServerContextWindow {
                    tokens: server,
                    source: ContextWindowSource::ServerProps,
                }),
                cap,
                24_000,
            );
            assert_eq!(window.tokens, expected);
            assert_eq!(window.source, source);
            assert_eq!(window.warning().is_some(), cap.is_some());
            assert_eq!(
                window.budget(24_000, Some(4096)),
                usize::try_from(expected).unwrap() - 4096
            );
        }
        let configured = Window::from_signals(None, Some(16384), 24_000);
        assert_eq!(configured.source, Source::Config);
        assert_eq!(configured.budget(24_000, Some(4096)), 12288);
        let default = Window::from_signals(None, None, 24_000);
        assert_eq!(default.source, Source::Default);
        assert_eq!(default.known_window(), None);
        assert_eq!(default.budget(24_000, Some(4096)), 24_000);
    }

    #[tokio::test]
    async fn context_concurrent_and_repeated_resolutions_probe_once_and_warn_once() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "default_generation_settings":{"n_ctx":262144}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let base = format!("{}/v1", server.uri());
        let config = config_for(&base, Some(8192));
        let (a, b) = tokio::join!(
            resolve_with_probe(
                &config,
                "context-test",
                "model",
                None,
                localpilot_llm::probe_context_window(&base, "model", None, true)
            ),
            resolve_with_probe(
                &config,
                "context-test",
                "model",
                None,
                localpilot_llm::probe_context_window(&base, "model", None, true)
            ),
        );
        assert_eq!(a.window, b.window);
        assert_ne!(a.fresh, b.fresh);
        assert_eq!(
            [a.warning_once(), b.warning_once()]
                .iter()
                .filter(|warning| warning.is_some())
                .count(),
            1
        );
        let again = resolve(&config, "context-test", "model", None).await;
        assert_eq!(again.window.tokens, 8192);
        assert!(!again.fresh);
        assert_eq!(again.warning_once(), None);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn context_failed_probes_are_cached_and_disabled_probes_do_no_io() {
        let server = MockServer::start().await;
        let base = format!("{}/v1", server.uri());
        let mut config = config_for(&base, Some(8192));
        for _ in 0..2 {
            let resolution = resolve_with_probe(
                &config,
                "context-test",
                "model",
                None,
                localpilot_llm::probe_context_window(&base, "model", None, true),
            )
            .await;
            assert_eq!(resolution.window.tokens, 8192);
            assert_eq!(resolution.window.source, Source::Config);
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        config.discovery.context_probe = false;
        let disabled = resolve_with_probe(
            &config,
            "context-test",
            "other-model",
            None,
            localpilot_llm::probe_context_window(&base, "other-model", None, true),
        )
        .await;
        assert_eq!(disabled.window.source, Source::Config);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        config
            .providers
            .get_mut("context-test")
            .unwrap()
            .context_window = None;
        let default =
            resolve_with_probe(&config, "context-test", "other-model", None, async { None }).await;
        assert_eq!(default.window.source, Source::Default);
        assert_eq!(default.window.tokens, 24_000);
    }

    #[tokio::test]
    async fn context_cache_is_scoped_to_endpoint_and_model() {
        let server = MockServer::start().await;
        let base = format!("{}/v1", server.uri());
        let config = config_for(&base, None);
        let a = resolve_with_probe(&config, "context-test", "a", None, async {
            Some(ServerContextWindow {
                tokens: 32768,
                source: ContextWindowSource::ModelListing,
            })
        })
        .await;
        let b = resolve_with_probe(&config, "context-test", "b", None, async {
            Some(ServerContextWindow {
                tokens: 65536,
                source: ContextWindowSource::ModelListing,
            })
        })
        .await;
        assert_eq!((a.window.tokens, b.window.tokens), (32768, 65536));
        let other = MockServer::start().await;
        let config = config_for(&other.uri(), None);
        let moved = resolve_with_probe(&config, "context-test", "a", None, async { None }).await;
        assert_eq!(moved.window.source, Source::Default);
    }
}
