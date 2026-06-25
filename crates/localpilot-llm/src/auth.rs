//! HTTP authentication helpers for provider adapters.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use localpilot_core::Secret;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::error::ProviderError;

const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// A bearer access token. The value is wrapped so it cannot leak through debug
/// output and is exposed only at the HTTP header boundary.
#[derive(Clone)]
pub struct AccessToken {
    token: Secret,
}

impl AccessToken {
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: Secret::new(token.into()),
        }
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        self.token.expose()
    }
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AccessToken").field(&"<redacted>").finish()
    }
}

/// A dynamic bearer-token source used by providers whose credentials refresh.
#[async_trait]
pub trait AuthProvider: Send + Sync + std::fmt::Debug {
    async fn access_token(&self) -> Result<AccessToken, ProviderError>;
}

/// Google Application Default Credentials access-token source for gcloud's
/// `authorized_user` ADC file.
pub struct GoogleAdcAuthProvider {
    adc_path: Option<PathBuf>,
    client: reqwest::Client,
    state: Mutex<GoogleAdcState>,
}

impl std::fmt::Debug for GoogleAdcAuthProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleAdcAuthProvider")
            .field("adc_path", &self.adc_path)
            .field("state", &"<redacted>")
            .finish()
    }
}

impl GoogleAdcAuthProvider {
    #[must_use]
    pub fn new(adc_path: Option<PathBuf>) -> Self {
        Self {
            adc_path,
            client: reqwest::Client::new(),
            state: Mutex::new(GoogleAdcState::default()),
        }
    }

    fn resolved_path(&self) -> Result<PathBuf, ProviderError> {
        if let Some(path) = &self.adc_path {
            return Ok(path.clone());
        }
        if let Some(path) = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS") {
            return Ok(PathBuf::from(path));
        }
        let home = std::env::var_os("HOME").ok_or_else(|| ProviderError::AuthConfig {
            message: "google ADC file could not be resolved: HOME is not set".to_string(),
        })?;
        Ok(PathBuf::from(home)
            .join(".config")
            .join("gcloud")
            .join("application_default_credentials.json"))
    }

    async fn load_credentials(&self) -> Result<AuthorizedUserCredentials, ProviderError> {
        let path = self.resolved_path()?;
        let body =
            tokio::fs::read_to_string(&path)
                .await
                .map_err(|err| ProviderError::AuthConfig {
                    message: format!("google ADC file could not be read: {err}"),
                })?;
        let raw: RawGoogleAdc =
            serde_json::from_str(&body).map_err(|err| ProviderError::AuthConfig {
                message: format!("google ADC file could not be parsed: {err}"),
            })?;
        if raw.kind.as_deref() != Some("authorized_user") {
            return Err(ProviderError::AuthConfig {
                message: "google ADC file type is not supported; expected authorized_user"
                    .to_string(),
            });
        }
        Ok(AuthorizedUserCredentials {
            client_id: required_secret(raw.client_id, "client_id")?,
            client_secret: required_secret(raw.client_secret, "client_secret")?,
            refresh_token: required_secret(raw.refresh_token, "refresh_token")?,
            token_uri: raw
                .token_uri
                .unwrap_or_else(|| DEFAULT_TOKEN_URI.to_string()),
        })
    }

    async fn refresh_token(
        &self,
        credentials: &AuthorizedUserCredentials,
    ) -> Result<CachedToken, ProviderError> {
        let response = self
            .client
            .post(&credentials.token_uri)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", credentials.client_id.expose()),
                ("client_secret", credentials.client_secret.expose()),
                ("refresh_token", credentials.refresh_token.expose()),
            ])
            .send()
            .await
            .map_err(|err| ProviderError::AuthConfig {
                message: format!("google ADC token refresh failed: {err}"),
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(ProviderError::AuthConfig {
                message: format!("google ADC token refresh failed with status {status}"),
            });
        }
        let token: TokenResponse =
            response
                .json()
                .await
                .map_err(|err| ProviderError::AuthConfig {
                    message: format!("google ADC token response could not be parsed: {err}"),
                })?;
        if token.access_token.trim().is_empty() {
            return Err(ProviderError::AuthConfig {
                message: "google ADC token response did not include an access token".to_string(),
            });
        }
        let ttl = token.expires_in.unwrap_or(3600).max(60);
        Ok(CachedToken {
            token: AccessToken::new(token.access_token),
            expires_at: Instant::now() + Duration::from_secs(ttl.saturating_sub(30)),
        })
    }
}

#[async_trait]
impl AuthProvider for GoogleAdcAuthProvider {
    async fn access_token(&self) -> Result<AccessToken, ProviderError> {
        let credentials = {
            let state = self.state.lock().await;
            if let Some(token) = state.cached.as_ref() {
                if Instant::now() < token.expires_at {
                    return Ok(token.token.clone());
                }
            }
            state.credentials.clone()
        };
        let credentials = match credentials {
            Some(credentials) => credentials,
            None => self.load_credentials().await?,
        };
        let token = self.refresh_token(&credentials).await?;
        let exposed = token.token.clone();
        let mut state = self.state.lock().await;
        if state.credentials.is_none() {
            state.credentials = Some(credentials);
        }
        state.cached = Some(token);
        Ok(exposed)
    }
}

#[derive(Default)]
struct GoogleAdcState {
    credentials: Option<AuthorizedUserCredentials>,
    cached: Option<CachedToken>,
}

#[derive(Clone)]
struct AuthorizedUserCredentials {
    client_id: Secret,
    client_secret: Secret,
    refresh_token: Secret,
    token_uri: String,
}

#[derive(Clone)]
struct CachedToken {
    token: AccessToken,
    expires_at: Instant,
}

#[derive(Deserialize)]
struct RawGoogleAdc {
    #[serde(rename = "type")]
    kind: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
    token_uri: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

fn required_secret(value: Option<String>, field: &str) -> Result<Secret, ProviderError> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(Secret::new)
        .ok_or_else(|| ProviderError::AuthConfig {
            message: format!("google ADC file is missing {field}"),
        })
}
