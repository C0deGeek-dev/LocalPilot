//! Provider registry: resolve configuration into live providers.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use localpilot_config::{Config, ProviderConfig};
use localpilot_core::Secret;

use crate::anthropic::AnthropicProvider;
use crate::error::ProviderError;
use crate::openai::OpenAiProvider;
use crate::provider::{ModelProvider, SourceType};

const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";

/// A set of constructed providers keyed by id, with a configured default. Holds
/// every configured provider built up front, so re-pointing a live session at a
/// different one is a lookup, not a rebuild. Each provider's configured default
/// model is carried alongside so a provider-only switch can resolve a model.
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn ModelProvider>>,
    /// The configured default model per provider id, when one is set.
    default_models: HashMap<String, String>,
    default_id: String,
}

impl ProviderRegistry {
    /// Build providers from configuration, resolving each provider's credential
    /// from its configured environment variable.
    ///
    /// # Errors
    /// Returns [`ProviderError`] if a provider entry is missing a required field
    /// or names an unknown kind.
    pub fn from_config(config: &Config) -> Result<Self, ProviderError> {
        let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
        let mut default_models: HashMap<String, String> = HashMap::new();
        for (id, entry) in &config.providers {
            let credential = config.resolve_credential(id);
            let provider = build_provider(id, entry, credential)?;
            providers.insert(id.clone(), provider);
            if let Some(model) = entry.model.clone() {
                default_models.insert(id.clone(), model);
            }
        }
        Ok(Self {
            providers,
            default_models,
            default_id: config.provider.default.clone(),
        })
    }

    /// Assemble a registry from already-built providers and their default models.
    /// The construction path for callers that build providers themselves (and for
    /// offline tests); [`from_config`](Self::from_config) is the normal entry.
    #[must_use]
    pub fn from_providers(
        providers: HashMap<String, Arc<dyn ModelProvider>>,
        default_models: HashMap<String, String>,
        default_id: impl Into<String>,
    ) -> Self {
        Self {
            providers,
            default_models,
            default_id: default_id.into(),
        }
    }

    /// The provider selected by `[provider].default`, if present.
    #[must_use]
    pub fn default_provider(&self) -> Option<&Arc<dyn ModelProvider>> {
        self.providers.get(&self.default_id)
    }

    /// A provider by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Arc<dyn ModelProvider>> {
        self.providers.get(id)
    }

    /// The configured default model for `id`, when the provider has one.
    #[must_use]
    pub fn default_model(&self, id: &str) -> Option<&str> {
        self.default_models.get(id).map(String::as_str)
    }

    /// The configured provider ids, sorted for stable display.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.providers.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// The number of registered providers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Whether the registry has no providers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

fn build_provider(
    id: &str,
    entry: &ProviderConfig,
    credential: Option<Secret>,
) -> Result<Arc<dyn ModelProvider>, ProviderError> {
    let timeout = entry.request_timeout_secs.map(Duration::from_secs);
    let mut options = entry.options.clone();
    if entry.suppress_thinking == Some(true) {
        options.insert("suppress_thinking".to_string(), serde_json::json!(true));
    }

    // Anthropic speaks a different wire protocol, so it has its own adapter.
    if entry.kind == "anthropic" {
        let base_url = entry
            .base_url
            .clone()
            .or_else(|| env_non_empty("ANTHROPIC_BASE_URL"))
            .unwrap_or_else(|| ANTHROPIC_DEFAULT_BASE_URL.to_string());
        return Ok(Arc::new(
            AnthropicProvider::new(id, id, base_url, credential)
                .with_timeout(timeout)
                .with_default_options(options)
                .with_max_context_tokens(entry.context_window),
        ));
    }

    let (source_type, base_url) = match entry.kind.as_str() {
        "openai" => (
            SourceType::OfficialApi,
            entry
                .base_url
                .clone()
                .or_else(|| env_non_empty("OPENAI_BASE_URL"))
                .unwrap_or_else(|| OPENAI_DEFAULT_BASE_URL.to_string()),
        ),
        "openai-compatible" | "local" => (
            SourceType::LocalServer,
            entry
                .base_url
                .clone()
                .or_else(|| env_non_empty("OPENAI_BASE_URL"))
                .ok_or_else(|| missing_base_url(id, entry))?,
        ),
        "custom" | "custom-user-endpoint" => {
            (SourceType::CustomUserEndpoint, require_base_url(id, entry)?)
        }
        other => {
            return Err(ProviderError::UnsupportedFeature(format!(
                "unknown provider kind '{other}' for provider '{id}'"
            )))
        }
    };
    Ok(Arc::new(
        OpenAiProvider::new(id, id, source_type, base_url, credential)
            .with_timeout(timeout)
            .with_default_options(options)
            .with_max_context_tokens(entry.context_window),
    ))
}

fn require_base_url(id: &str, entry: &ProviderConfig) -> Result<String, ProviderError> {
    entry
        .base_url
        .clone()
        .ok_or_else(|| missing_base_url(id, entry))
}

fn missing_base_url(id: &str, entry: &ProviderConfig) -> ProviderError {
    ProviderError::InvalidRequest {
        message: format!("provider '{id}' of kind '{}' requires base_url", entry.kind),
    }
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_config::ProviderConfig;

    fn entry(kind: &str, base_url: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            kind: kind.to_string(),
            base_url: base_url.map(str::to_string),
            api_key_env: None,
            model: None,
            request_timeout_secs: None,
            context_window: None,
            suppress_thinking: None,
            options: Default::default(),
        }
    }

    #[test]
    fn resolves_local_official_and_custom_providers() {
        let mut config = Config::default();
        config.providers.insert(
            "local".to_string(),
            entry("openai-compatible", Some("http://localhost:11434/v1")),
        );
        config
            .providers
            .insert("openai".to_string(), entry("openai", None));
        config.providers.insert(
            "custom".to_string(),
            entry("custom", Some("https://example.test/v1")),
        );
        config.provider.default = "local".to_string();

        let registry = ProviderRegistry::from_config(&config).unwrap();
        assert_eq!(registry.len(), 3);
        assert_eq!(
            registry
                .default_provider()
                .unwrap()
                .declaration()
                .source_type,
            SourceType::LocalServer
        );
        assert_eq!(
            registry.get("openai").unwrap().declaration().source_type,
            SourceType::OfficialApi
        );
        assert_eq!(
            registry.get("custom").unwrap().declaration().source_type,
            SourceType::CustomUserEndpoint
        );
    }

    #[test]
    fn resolves_the_anthropic_provider() {
        let mut config = Config::default();
        config
            .providers
            .insert("anthropic".to_string(), entry("anthropic", None));
        config.provider.default = "anthropic".to_string();

        let registry = ProviderRegistry::from_config(&config).unwrap();
        let declaration = registry.get("anthropic").unwrap().declaration();
        assert_eq!(declaration.source_type, SourceType::OfficialApi);
        assert_eq!(
            declaration.tool_call_shape,
            crate::provider::ToolCallShape::AnthropicToolUse
        );
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let mut config = Config::default();
        config
            .providers
            .insert("weird".to_string(), entry("mystery", None));
        assert!(matches!(
            ProviderRegistry::from_config(&config),
            Err(ProviderError::UnsupportedFeature(_))
        ));
    }

    #[test]
    fn carries_each_providers_configured_default_model_and_ids() {
        let mut config = Config::default();
        let mut openai = entry("openai", None);
        openai.model = Some("gpt-x".to_string());
        config.providers.insert("openai".to_string(), openai);
        config.providers.insert(
            "local".to_string(),
            entry("openai-compatible", Some("http://localhost:11434/v1")),
        );
        config.provider.default = "local".to_string();

        let registry = ProviderRegistry::from_config(&config).unwrap();
        // The configured model is carried for a provider-only switch to resolve.
        assert_eq!(registry.default_model("openai"), Some("gpt-x"));
        // A provider with no configured model has no default to fall back to.
        assert_eq!(registry.default_model("local"), None);
        // Ids are listed and sorted for stable display.
        assert_eq!(
            registry.ids(),
            vec!["local".to_string(), "openai".to_string()]
        );
    }

    #[test]
    fn local_without_base_url_is_rejected() {
        let mut config = Config::default();
        config
            .providers
            .insert("local".to_string(), entry("local", None));
        assert!(matches!(
            ProviderRegistry::from_config(&config),
            Err(ProviderError::InvalidRequest { .. })
        ));
    }
}
