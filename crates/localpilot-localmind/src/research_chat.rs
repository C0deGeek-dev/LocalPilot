//! The project's LocalMind chat endpoint, resolved for research-side
//! classification.
//!
//! The research admission gate reuses **existing** model configuration only:
//! the first choice is LocalMind's `[inference]` chat model (when configured
//! with its research feature enabled), the fallback is the host's own
//! resolved default provider. This module provides the first choice as a
//! small blocking surface; the host wraps it for async use. No new provider,
//! model setting, or credential path is introduced.

use std::path::Path;

use localmind_inference::{ChatMessage, InferenceCapability};

/// A resolved LocalMind chat configuration usable for one-shot research
/// classification calls. Cheap to clone; the endpoint is constructed per call.
#[derive(Clone)]
pub struct ResearchChat {
    settings: localmind_core::InferenceSettings,
}

impl ResearchChat {
    /// Resolve the project's LocalMind chat settings, or `None` when no chat
    /// model is configured or the `[inference]` research feature is disabled —
    /// callers then fall back to the host's own model.
    #[must_use]
    pub fn resolve(project_root: &Path) -> Option<Self> {
        let config = localmind_store::ProjectConfig::discover(project_root).ok()?;
        let settings = config.config.inference.clone()?;
        if !settings.features.research {
            return None;
        }
        settings.chat_base_url.as_ref()?;
        settings.chat_model.as_ref()?;
        Some(Self { settings })
    }

    /// One blocking chat completion (system + user message). Async callers
    /// run this on a blocking task; the endpoint's own timeout bounds it.
    ///
    /// # Errors
    /// Returns a display string when the endpoint is unavailable or the call
    /// fails — the caller degrades to its deterministic fallback.
    pub fn complete(&self, system: &str, user: &str) -> Result<String, String> {
        let capability = InferenceCapability::from_settings(Some(&self.settings))
            .map_err(|error| error.to_string())?;
        let chat = capability
            .chat()
            .ok_or_else(|| "chat endpoint unavailable".to_string())?;
        let completion = chat
            .complete(&[ChatMessage::system(system), ChatMessage::user(user)])
            .map_err(|error| error.to_string())?;
        Ok(completion.content)
    }
}
