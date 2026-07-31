//! Usage accounting.
//!
//! Token counts come from providers and are therefore untrusted; arithmetic uses
//! saturating operations so a hostile or buggy provider cannot cause overflow.

use serde::{Deserialize, Serialize};

/// Token counts for a request or an accumulated session.
///
/// `input_tokens` is the count a provider bills as fresh input. For providers
/// with prompt caching (Anthropic), it excludes the cached prefix: the cached
/// tokens are reported separately as `cache_read_input_tokens` (served from an
/// existing cache, ~0.1× cost) and `cache_creation_input_tokens` (written to the
/// cache this request, ~1.25× cost). Providers without caching leave both zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens written to the prompt cache this request (0 when uncached).
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// Tokens served from the prompt cache this request (0 when uncached).
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl TokenUsage {
    /// Total tokens, saturating on overflow. Counts the effective input (fresh
    /// input plus the cached prefix that was created or read) plus output, so a
    /// cache hit does not appear to shrink the prompt.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.effective_input_tokens()
            .saturating_add(self.output_tokens)
    }

    /// The whole prompt the model saw this request: fresh input plus the cached
    /// prefix (created + read). Equal to `input_tokens` when caching is off.
    #[must_use]
    pub fn effective_input_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }

    /// Add another usage into this one, saturating on overflow.
    pub fn accumulate(&mut self, other: TokenUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
    }
}

/// A usage summary suitable for the TUI footer: token counts plus elapsed time.
/// Throughput is derived, never stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageSummary {
    pub tokens: TokenUsage,
    pub elapsed_secs: f64,
}

impl UsageSummary {
    /// Output tokens per second, or `0.0` when no time has elapsed.
    #[must_use]
    pub fn output_tokens_per_sec(&self) -> f64 {
        if self.elapsed_secs > 0.0 {
            self.tokens.output_tokens as f64 / self.elapsed_secs
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_and_accumulate_saturate() {
        let mut u = TokenUsage {
            input_tokens: u64::MAX,
            output_tokens: 10,
            ..Default::default()
        };
        assert_eq!(u.total(), u64::MAX);
        u.accumulate(TokenUsage {
            input_tokens: 5,
            output_tokens: 5,
            ..Default::default()
        });
        assert_eq!(u.input_tokens, u64::MAX);
        assert_eq!(u.output_tokens, 15);
    }

    #[test]
    fn cache_fields_accumulate_and_count_toward_effective_input() {
        let mut u = TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: 100,
            cache_read_input_tokens: 1000,
        };
        // Effective input is the whole prompt the model saw, cached prefix
        // included, so a cache hit does not appear to shrink the prompt.
        assert_eq!(u.effective_input_tokens(), 1110);
        assert_eq!(u.total(), 1115);
        u.accumulate(TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 4,
        });
        assert_eq!(u.cache_creation_input_tokens, 103);
        assert_eq!(u.cache_read_input_tokens, 1004);
        // Saturating: a hostile provider cannot overflow the cache fields.
        u.accumulate(TokenUsage {
            cache_read_input_tokens: u64::MAX,
            ..Default::default()
        });
        assert_eq!(u.cache_read_input_tokens, u64::MAX);
    }

    #[test]
    fn cache_fields_default_when_absent_from_json() {
        // A provider that reports no cache fields deserializes to zero (serde
        // default), so persisted pre-caching sessions still load.
        let u: TokenUsage =
            serde_json::from_str(r#"{"input_tokens":7,"output_tokens":3}"#).unwrap();
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.cache_read_input_tokens, 0);
        assert_eq!(u.effective_input_tokens(), 7);
    }

    #[test]
    fn summary_roundtrips_and_derives_throughput() {
        let s = UsageSummary {
            tokens: TokenUsage {
                input_tokens: 100,
                output_tokens: 200,
                ..Default::default()
            },
            elapsed_secs: 2.0,
        };
        assert!((s.output_tokens_per_sec() - 100.0).abs() < f64::EPSILON);
        let back: UsageSummary = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn zero_elapsed_has_zero_throughput() {
        let s = UsageSummary::default();
        assert_eq!(s.output_tokens_per_sec(), 0.0);
    }
}
