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

    /// Authentication could not be configured or refreshed locally.
    #[error("authentication setup failed: {message}")]
    AuthConfig { message: String },

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

    /// The response stream ended before it completed — the server dropped the
    /// connection mid-response, or closed the body without a completion marker.
    /// Distinct from [`StreamDecode`](Self::StreamDecode), which is a malformed
    /// but fully-framed event: a truncation is an infrastructure fault (a local
    /// server that crashed or ran out of VRAM), so it is retryable and is
    /// **not** treated as a bad model turn by the recovery ladder. A server
    /// that goes *silent* without closing is a
    /// [`StreamStalled`](Self::StreamStalled), not a truncation.
    #[error("the response stream ended early: {detail}")]
    StreamTruncated { detail: String },

    /// No data arrived from the provider for the configured stall window while
    /// the connection stayed open. Distinct from
    /// [`StreamTruncated`](Self::StreamTruncated) (the server *closed* the
    /// response early): a stalled server is most likely still working, just
    /// slower than the window — on a local server, CPU-fallback inference is
    /// the usual cause. Not retryable: re-issuing the identical request cannot
    /// complete any faster (and on models without reusable prompt cache it
    /// restarts prompt processing from zero, making every retry strictly
    /// worse), so the turn stops with guidance instead of burning retries.
    #[error(
        "no data from the model server for {waited_secs}s (the connection stayed open); the \
         server may be hung, or working slower than the stall window — for a local model, \
         check that GPU offload is active (CPU-speed inference is the usual cause), or raise \
         the provider's `request_timeout_secs` to wait longer"
    )]
    StreamStalled { waited_secs: u64 },

    /// A tool call's streamed arguments did not parse as JSON. Carries the tool
    /// name and the byte length of the unparseable arguments, so the harness can
    /// recover an oversized write (e.g. steer the model to chunk the write)
    /// rather than only re-prompting blindly. Like [`StreamDecode`](Self::StreamDecode),
    /// it does not stop the turn.
    #[error("malformed arguments for tool `{tool}` ({bytes} bytes): {reason}")]
    MalformedToolArguments {
        tool: String,
        bytes: usize,
        reason: String,
    },

    /// The provider does not support a requested feature.
    #[error("unsupported feature: {0}")]
    UnsupportedFeature(String),
}

impl ProviderError {
    /// Whether this invalid request is likely a context-window overflow.
    #[must_use]
    pub fn is_context_length_error(&self) -> bool {
        let ProviderError::InvalidRequest { message } = self else {
            return false;
        };
        let lower = message.to_ascii_lowercase();
        lower.contains("context")
            || lower.contains("token")
            || lower.contains("too large")
            || lower.contains("too long")
            || lower.contains("maximum length")
            || lower.contains("max length")
    }

    /// Classify an error raised while reading an already-open response body.
    ///
    /// `reqwest` uses decode errors for invalid or truncated HTTP body framing,
    /// so a decode error here means the stream was cut mid-response — classify it
    /// as a [`StreamTruncated`](Self::StreamTruncated) (an infrastructure fault
    /// the turn loop can retry), not a generic network failure.
    #[must_use]
    pub fn from_response_body_error(err: reqwest::Error) -> Self {
        let message = format!("response body read failed after stream opened: {err}");
        if err.is_decode() {
            ProviderError::StreamTruncated { detail: message }
        } else {
            ProviderError::Network(message)
        }
    }

    /// Build a [`StreamStalled`](Self::StreamStalled) for a silence that
    /// exhausted `waited` — shared by the providers so the user-facing
    /// guidance is written once.
    #[must_use]
    pub fn stream_stalled(waited: std::time::Duration) -> Self {
        ProviderError::StreamStalled {
            waited_secs: waited.as_secs(),
        }
    }

    /// The quota/rate-limit metadata carried by a [`RateLimit`](Self::RateLimit)
    /// or [`Quota`](Self::Quota) error, used to schedule a precise pause window.
    #[must_use]
    pub fn quota(&self) -> Option<&QuotaInfo> {
        match self {
            ProviderError::RateLimit { quota } | ProviderError::Quota { quota } => Some(quota),
            _ => None,
        }
    }

    /// Whether retrying the request (after any indicated wait) may succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::RateLimit { quota } | ProviderError::Quota { quota } => quota.retryable,
            ProviderError::Server { .. }
            | ProviderError::Network(_)
            | ProviderError::StreamTruncated { .. } => true,
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
    fn stalled_stream_is_not_retryable_and_names_the_remedy() {
        let err = ProviderError::stream_stalled(Duration::from_secs(600));
        // Retrying an identical request into a stalled-but-working server
        // cannot finish faster — the turn must stop with guidance instead.
        assert!(!err.is_retryable());
        let text = err.to_string();
        assert!(text.contains("600s"), "waited window missing: {text}");
        assert!(
            text.contains("request_timeout_secs") && text.contains("GPU offload"),
            "guidance missing: {text}"
        );
    }

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
    fn quota_metadata_is_exposed_only_for_limit_errors() {
        let quota = QuotaInfo {
            retryable: true,
            retry_after: Some(Duration::from_secs(30)),
            ..QuotaInfo::default()
        };
        assert_eq!(
            ProviderError::RateLimit {
                quota: quota.clone()
            }
            .quota()
            .and_then(|q| q.retry_after),
            Some(Duration::from_secs(30))
        );
        assert!(ProviderError::Quota { quota }.quota().is_some());
        assert!(ProviderError::Auth { request_id: None }.quota().is_none());
        assert!(ProviderError::Network("down".to_string()).quota().is_none());
    }

    #[test]
    fn retryability_matches_taxonomy() {
        assert!(ProviderError::Server {
            status: 500,
            request_id: None
        }
        .is_retryable());
        assert!(ProviderError::Network("down".to_string()).is_retryable());
        // A mid-stream truncation is an infrastructure fault the turn loop retries.
        assert!(ProviderError::StreamTruncated {
            detail: "cut".to_string()
        }
        .is_retryable());
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
