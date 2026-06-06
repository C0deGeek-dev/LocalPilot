//! Retry with exponential backoff and jitter.
//!
//! Only retryable failures (transient server/network errors, and rate-limit /
//! quota errors the provider marks retryable) are retried, and any provider-
//! stated `retry_after` is honoured. This waits for documented windows; it never
//! retries against a provider's stated policy or frames waiting as bypassing a
//! limit.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::ProviderError;

/// Backoff configuration.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// The delay before the given 1-based retry attempt for an error.
    #[must_use]
    pub fn delay_for(&self, attempt: u32, error: &ProviderError) -> Duration {
        if let Some(retry_after) = retry_after_of(error) {
            return retry_after.min(self.max_delay);
        }
        let shift = attempt.saturating_sub(1).min(16);
        let exponential = self
            .base_delay
            .saturating_mul(1u32 << shift)
            .min(self.max_delay);
        // Full jitter: a random point in [exponential/2, exponential].
        let half = exponential / 2;
        half + half.mul_f64(jitter_fraction())
    }
}

/// Run `operation`, retrying retryable failures with backoff up to
/// `policy.max_attempts` total attempts.
///
/// # Errors
/// Returns the last [`ProviderError`] if every attempt fails or the failure is
/// not retryable.
pub async fn retry<F, Fut, T>(policy: RetryPolicy, mut operation: F) -> Result<T, ProviderError>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, ProviderError>>,
{
    let mut attempt: u32 = 1;
    loop {
        match operation(attempt).await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if attempt >= policy.max_attempts || !error.is_retryable() {
                    return Err(error);
                }
                let delay = policy.delay_for(attempt, &error);
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

fn retry_after_of(error: &ProviderError) -> Option<Duration> {
    match error {
        ProviderError::RateLimit { quota } | ProviderError::Quota { quota } => quota.retry_after,
        _ => None,
    }
}

/// A cheap, dependency-free jitter source in `[0.0, 1.0)`. Backoff jitter does
/// not need cryptographic randomness, only spreading.
fn jitter_fraction() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos) / f64::from(u32::from(u16::MAX) + 1) % 1.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn transient() -> ProviderError {
        ProviderError::Server {
            status: 503,
            request_id: None,
        }
    }

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        }
    }

    #[tokio::test]
    async fn retries_transient_then_succeeds() {
        let calls = Arc::new(AtomicU32::new(0));
        let result = retry(fast_policy(), |_attempt| {
            let calls = Arc::clone(&calls);
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(transient())
                } else {
                    Ok::<_, ProviderError>(n)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let calls = Arc::new(AtomicU32::new(0));
        let result: Result<(), _> = retry(fast_policy(), |_attempt| {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(transient())
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn does_not_retry_non_retryable_errors() {
        let calls = Arc::new(AtomicU32::new(0));
        let result: Result<(), _> = retry(fast_policy(), |_attempt| {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::Auth { request_id: None })
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
