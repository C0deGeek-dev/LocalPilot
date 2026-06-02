//! Provider error taxonomy and quota metadata.

use std::time::Duration;

/// Quota / rate-limit reset metadata a provider may surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuotaInfo {
    /// How long to wait before retrying, if the provider states it.
    pub retry_after: Option<Duration>,
    /// Absolute reset time as a Unix timestamp (seconds), if known.
    pub reset_at: Option<u64>,
    /// The provider's class of limit (e.g. `requests`, `tokens`), if stated.
    pub limit_kind: Option<String>,
    /// Whether the provider indicates the request is safe to retry after waiting.
    pub retryable: bool,
    /// The raw provider error code/category, for diagnostics.
    pub raw_provider_code: Option<String>,
}

/// Errors returned by a provider, classified into a stable taxonomy. The
/// `Display` text is concise and safe to show a user; it never contains secrets.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// Authentication failed (bad or missing credentials).
    #[error("authentication failed")]
    Auth { request_id: Option<String> },

    /// The provider rate-limited the request; retry after the window.
    #[error("rate limited by provider")]
    RateLimit { quota: QuotaInfo },

    /// The account quota is exhausted (distinct from a transient rate limit).
    #[error("provider quota exhausted")]
    Quota { quota: QuotaInfo },

    /// The request was rejected as invalid.
    #[error("invalid request: {message}")]
    InvalidRequest { message: String },

    /// The requested model is unknown to the provider.
    #[error("model not found: {model}")]
    ModelNotFound { model: String },

    /// The provider returned a server-side error.
    #[error("provider server error (status {status})")]
    Server {
        status: u16,
        request_id: Option<String>,
    },

    /// A transport/network failure reaching the provider.
    #[error("network error: {0}")]
    Network(String),

    /// The response stream could not be decoded.
    #[error("stream decode error: {0}")]
    StreamDecode(String),

    /// The provider does not support a requested feature.
    #[error("unsupported feature: {0}")]
    UnsupportedFeature(String),
}

impl ProviderError {
    /// Whether retrying the request (after any indicated wait) may succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::RateLimit { quota } | ProviderError::Quota { quota } => quota.retryable,
            ProviderError::Server { .. } | ProviderError::Network(_) => true,
            _ => false,
        }
    }

    /// Classify an HTTP error response into the taxonomy.
    ///
    /// `code` is the provider's machine-readable error code when present (for
    /// example OpenAI's `insufficient_quota`), used to separate a hard quota
    /// exhaustion from a transient rate limit.
    #[must_use]
    pub fn from_http(
        status: u16,
        code: Option<&str>,
        request_id: Option<String>,
        quota: QuotaInfo,
    ) -> Self {
        match status {
            401 | 403 => ProviderError::Auth { request_id },
            404 => ProviderError::ModelNotFound {
                model: String::new(),
            },
            429 => {
                if code == Some("insufficient_quota") {
                    ProviderError::Quota { quota }
                } else {
                    ProviderError::RateLimit { quota }
                }
            }
            400 | 422 => ProviderError::InvalidRequest {
                message: code.unwrap_or("bad request").to_string(),
            },
            500..=599 => ProviderError::Server { status, request_id },
            _ => ProviderError::Server { status, request_id },
        }
    }
}

impl From<reqwest::Error> for ProviderError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_decode() {
            ProviderError::StreamDecode(err.to_string())
        } else {
            ProviderError::Network(err.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_representative_status_codes() {
        let q = QuotaInfo::default();
        assert!(matches!(
            ProviderError::from_http(401, None, None, q.clone()),
            ProviderError::Auth { .. }
        ));
        assert!(matches!(
            ProviderError::from_http(404, None, None, q.clone()),
            ProviderError::ModelNotFound { .. }
        ));
        assert!(matches!(
            ProviderError::from_http(400, Some("bad"), None, q.clone()),
            ProviderError::InvalidRequest { .. }
        ));
        assert!(matches!(
            ProviderError::from_http(503, None, Some("req_1".to_string()), q),
            ProviderError::Server { status: 503, .. }
        ));
    }

    #[test]
    fn distinguishes_quota_from_rate_limit() {
        let quota = QuotaInfo {
            retryable: true,
            retry_after: Some(Duration::from_secs(2)),
            ..QuotaInfo::default()
        };
        assert!(matches!(
            ProviderError::from_http(429, Some("insufficient_quota"), None, quota.clone()),
            ProviderError::Quota { .. }
        ));
        assert!(matches!(
            ProviderError::from_http(429, Some("rate_limit_exceeded"), None, quota.clone()),
            ProviderError::RateLimit { .. }
        ));
        assert!(matches!(
            ProviderError::from_http(429, None, None, quota),
            ProviderError::RateLimit { .. }
        ));
    }

    #[test]
    fn retryability_matches_taxonomy() {
        assert!(ProviderError::Server {
            status: 500,
            request_id: None
        }
        .is_retryable());
        assert!(ProviderError::Network("down".to_string()).is_retryable());
        assert!(!ProviderError::Auth { request_id: None }.is_retryable());
        assert!(!ProviderError::InvalidRequest {
            message: "x".to_string()
        }
        .is_retryable());
        assert!(ProviderError::RateLimit {
            quota: QuotaInfo {
                retryable: true,
                ..QuotaInfo::default()
            }
        }
        .is_retryable());
    }

    #[test]
    fn display_is_concise_and_carries_no_request_id() {
        let err = ProviderError::Auth {
            request_id: Some("req_secret_lookalike".to_string()),
        };
        assert_eq!(err.to_string(), "authentication failed");
    }
}
