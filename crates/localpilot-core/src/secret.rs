//! Secret wrapper.
//!
//! A [`Secret`] hides its value from `Debug` and `Display` so a credential
//! cannot reach logs, transcripts, or error messages by accident. The raw value
//! is reachable only through the explicit [`Secret::expose`] call. The wrapper
//! deliberately does not implement `Serialize`, so a secret cannot be persisted
//! without going through code that exposes it on purpose.

use std::fmt;

/// A string credential whose value never appears in formatting output.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a credential value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the raw value. Call sites that use this are the audited places a
    /// secret leaves the wrapper.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the wrapped value is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

const REDACTED: &str = "***";

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Secret").field(&REDACTED).finish()
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_never_reveal_the_value() {
        let secret = Secret::new("sk-super-secret-value");
        assert!(!format!("{secret:?}").contains("super-secret"));
        assert!(!format!("{secret}").contains("super-secret"));
        assert_eq!(format!("{secret}"), "***");
        // The value is still reachable explicitly.
        assert_eq!(secret.expose(), "sk-super-secret-value");
    }

    #[test]
    fn secret_nested_in_debug_struct_stays_hidden() {
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Holder {
            key: Secret,
        }
        let h = Holder {
            key: Secret::new("topsecret"),
        };
        assert!(!format!("{h:?}").contains("topsecret"));
    }
}
