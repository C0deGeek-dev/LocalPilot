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

/// The placeholder substituted for a secret found verbatim in untrusted text.
///
/// Matches the shared pattern redactor's placeholder so a reader cannot tell
/// which of the two layers caught a value — and so neither layer's output hints
/// at what the other missed.
pub const REDACTED_EXACT: &str = "[REDACTED]";

/// The shortest secret worth matching verbatim.
///
/// Exact-value redaction is a blunt instrument: it replaces every occurrence of
/// a known value, wherever it appears. Below this length a "secret" is likely to
/// occur in ordinary prose, and blanking unrelated text is a worse outcome than
/// not matching a value no credential system should have issued. The floor is
/// the same order as the shared pattern redactor's own length floors (6–12
/// characters depending on the family), so the two layers agree on what is too
/// short to be a credible credential. Shorter values still get pattern
/// redaction; they simply never become an exact-match needle.
pub const MIN_EXACT_REDACTION_LEN: usize = 8;

/// Whether `secret` is long enough to be matched verbatim in untrusted text.
#[must_use]
pub fn is_exact_redactable(secret: &Secret) -> bool {
    secret.expose().chars().count() >= MIN_EXACT_REDACTION_LEN
}

/// Replace every verbatim occurrence of each secret in `text`.
///
/// This is the exact half of the workspace's two-layer redaction: the shared
/// pattern detector catches credentials by *shape* and this catches the specific
/// values we handed out, which is what makes it useful against a process we gave
/// a credential to and cannot otherwise constrain. Values shorter than
/// [`MIN_EXACT_REDACTION_LEN`] are skipped (see that constant).
///
/// Returns `text` unchanged when nothing matched, so a caller can avoid an
/// allocation on the overwhelmingly common clean path.
#[must_use]
pub fn redact_exact<'a>(text: &'a str, secrets: &[Secret]) -> std::borrow::Cow<'a, str> {
    let mut out = std::borrow::Cow::Borrowed(text);
    for secret in secrets {
        if !is_exact_redactable(secret) {
            continue;
        }
        let value = secret.expose();
        if out.contains(value) {
            out = std::borrow::Cow::Owned(out.replace(value, REDACTED_EXACT));
        }
    }
    out
}

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
    fn exact_redaction_replaces_every_occurrence_of_a_long_secret() {
        let secrets = vec![Secret::new("super-secret-credential-value")];
        let text = "sent super-secret-credential-value twice: super-secret-credential-value";
        let out = redact_exact(text, &secrets);
        assert!(!out.contains("super-secret"));
        assert_eq!(out.matches(REDACTED_EXACT).count(), 2);
    }

    #[test]
    fn exact_redaction_skips_values_below_the_length_floor() {
        // A short "secret" occurs in ordinary text; matching it verbatim would
        // corrupt the output the user is trying to read, which is worse than not
        // matching a value no credential system should have issued.
        let secrets = vec![Secret::new("the")];
        let text = "the quick brown fox";
        assert_eq!(redact_exact(text, &secrets), text);
        assert!(!is_exact_redactable(&Secret::new("short")));
        assert!(is_exact_redactable(&Secret::new("longenough")));
    }

    #[test]
    fn the_length_floor_sits_where_a_credible_credential_starts() {
        // Pinned so the threshold is a decision, not an accident: seven
        // characters is prose, eight is the shortest thing treated as a secret.
        assert_eq!(MIN_EXACT_REDACTION_LEN, 8);
        assert!(!is_exact_redactable(&Secret::new("1234567")));
        assert!(is_exact_redactable(&Secret::new("12345678")));
    }

    #[test]
    fn clean_text_is_returned_without_allocating() {
        let secrets = vec![Secret::new("super-secret-credential-value")];
        // The overwhelmingly common case: nothing matched, so the input is
        // handed back borrowed rather than copied.
        assert!(matches!(
            redact_exact("nothing to see here", &secrets),
            std::borrow::Cow::Borrowed(_)
        ));
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
