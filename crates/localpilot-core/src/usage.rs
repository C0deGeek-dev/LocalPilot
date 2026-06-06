//! Usage accounting.
//!
//! Token counts come from providers and are therefore untrusted; arithmetic uses
//! saturating operations so a hostile or buggy provider cannot cause overflow.

use serde::{Deserialize, Serialize};

/// Token counts for a request or an accumulated session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    /// Total tokens, saturating on overflow.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Add another usage into this one, saturating on overflow.
    pub fn accumulate(&mut self, other: TokenUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }
}

/// A usage summary suitable for the TUI footer: token counts plus elapsed time
/// and an optional cost estimate. Throughput is derived, never stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageSummary {
    pub tokens: TokenUsage,
    pub elapsed_secs: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_estimate_usd: Option<f64>,
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
        };
        assert_eq!(u.total(), u64::MAX);
        u.accumulate(TokenUsage {
            input_tokens: 5,
            output_tokens: 5,
        });
        assert_eq!(u.input_tokens, u64::MAX);
        assert_eq!(u.output_tokens, 15);
    }

    #[test]
    fn summary_roundtrips_and_derives_throughput() {
        let s = UsageSummary {
            tokens: TokenUsage {
                input_tokens: 100,
                output_tokens: 200,
            },
            elapsed_secs: 2.0,
            cost_estimate_usd: Some(0.01),
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
