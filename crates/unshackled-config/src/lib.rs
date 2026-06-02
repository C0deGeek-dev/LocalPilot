//! Configuration schema and loading contracts.
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub default_provider: String,
    pub default_model: Option<String>,
    pub workspace_trust_required: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_provider: "local".to_string(),
            default_model: None,
            workspace_trust_required: true,
        }
    }
}
