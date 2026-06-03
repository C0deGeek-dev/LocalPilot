//! Provider registry: resolve configuration into live providers.

use std::collections::HashMap;
use std::sync::Arc;

use unshackled_config::{Config, ProviderConfig};

use crate::anthropic::AnthropicProvider;
use crate::error::ProviderError;
use crate::openai::OpenAiProvider;
use crate::provider::{ModelProvider, SourceType};

const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";

/// A set of constructed providers keyed by id, with a configured default.
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn ModelProvider>>,
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
        for (id, entry) in &config.providers {
            let credential = config.resolve_credential(id);
            let provider = build_provider(id, entry, credential)?;
            providers.insert(id.clone(), provider);
        }
        Ok(Self {
            providers,
            default_id: config.provider.default.clone(),
        })
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
    credential: Option<unshackled_core::Secret>,
) -> Result<Arc<dyn ModelProvider>, ProviderError> {
    // Anthropic speaks a different wire protocol, so it has its own adapter.
    if entry.kind == "anthropic" {
        let base_url = entry
            .base_url
            .clone()
            .unwrap_or_else(|| ANTHROPIC_DEFAULT_BASE_URL.to_string());
        return Ok(Arc::new(AnthropicProvider::new(
            id, id, base_url, credential,
        )));
    }

    let (source_type, base_url) = match entry.kind.as_str() {
        "openai" => (
            SourceType::OfficialApi,
            entry
                .base_url
                .clone()
                .unwrap_or_else(|| OPENAI_DEFAULT_BASE_URL.to_string()),
        ),
        "openai-compatible" | "local" => (SourceType::LocalServer, require_base_url(id, entry)?),
        "custom" | "custom-user-endpoint" => {
            (SourceType::CustomUserEndpoint, require_base_url(id, entry)?)
        }
        other => {
            return Err(ProviderError::UnsupportedFeature(format!(
                "unknown provider kind '{other}' for provider '{id}'"
            )))
        }
    };
    Ok(Arc::new(OpenAiProvider::new(
        id,
        id,
        source_type,
        base_url,
        credential,
    )))
}

fn require_base_url(id: &str, entry: &ProviderConfig) -> Result<String, ProviderError> {
    entry
        .base_url
        .clone()
        .ok_or_else(|| ProviderError::InvalidRequest {
            message: format!("provider '{id}' of kind '{}' requires base_url", entry.kind),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use unshackled_config::ProviderConfig;

    fn entry(kind: &str, base_url: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            kind: kind.to_string(),
            base_url: base_url.map(str::to_string),
            api_key_env: None,
            model: None,
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
