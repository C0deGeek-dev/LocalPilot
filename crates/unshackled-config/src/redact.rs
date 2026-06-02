//! Best-effort secret detection and redaction.
//!
//! This is the single detector the whole workspace shares: store persistence,
//! tool-output capture, logging, and memory writes all redact through
//! [`redact`] rather than re-implementing detection. Detection is best-effort by
//! design; the product's backstop is the user's ability to inspect and delete
//! stored data, never a promise of perfect filtering.

use std::sync::OnceLock;

use regex::Regex;

/// A class of secret the detector recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SecretKind {
    ApiKey,
    BearerToken,
    PrivateKey,
    Password,
    CloudCredential,
    ConnectionString,
}

/// The placeholder substituted for a detected secret.
pub const REDACTED: &str = "[REDACTED]";

struct Pattern {
    kind: SecretKind,
    regex: Regex,
}

// The regexes are constant string literals; a failure to compile is a
// programming error caught by this module's tests, not a runtime condition.
#[allow(clippy::expect_used)]
fn patterns() -> &'static [Pattern] {
    static PATTERNS: OnceLock<Vec<Pattern>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        // Each pattern is intentionally conservative about length so ordinary
        // prose does not match. `build` panics only on a malformed literal regex,
        // which is a compile-time-constant programming error surfaced by tests.
        let build = |kind: SecretKind, re: &str| Pattern {
            kind,
            regex: Regex::new(re).expect("static redaction regex is valid"),
        };
        vec![
            // PEM private key blocks (multi-line).
            build(
                SecretKind::PrivateKey,
                r"(?s)-----BEGIN[A-Z ]*PRIVATE KEY-----.*?-----END[A-Z ]*PRIVATE KEY-----",
            ),
            // `scheme://user:password@host`
            build(
                SecretKind::ConnectionString,
                r"[a-zA-Z][a-zA-Z0-9+.\-]*://[^\s:/@]+:[^\s:/@]+@[^\s]+",
            ),
            // AWS access key id.
            build(SecretKind::CloudCredential, r"AKIA[0-9A-Z]{16}"),
            // `Authorization: Bearer <token>` and bare bearer tokens.
            build(SecretKind::BearerToken, r"(?i)bearer\s+[A-Za-z0-9._\-]{8,}"),
            // OpenAI-style and Google-style API keys.
            build(SecretKind::ApiKey, r"sk-[A-Za-z0-9_\-]{16,}"),
            build(SecretKind::ApiKey, r"AIza[0-9A-Za-z_\-]{20,}"),
            // `password = "..."` / `api_key: ...` / `secret=...` / `token: ...`
            build(
                SecretKind::Password,
                r#"(?i)(password|passwd|pwd)\s*[:=]\s*["']?[^\s"']{6,}"#,
            ),
            build(
                SecretKind::ApiKey,
                r#"(?i)(api[_\-]?key|secret|token)\s*[:=]\s*["']?[A-Za-z0-9_\-]{12,}"#,
            ),
        ]
    })
}

/// Whether `text` appears to contain at least one secret.
#[must_use]
pub fn contains_secret(text: &str) -> bool {
    patterns().iter().any(|p| p.regex.is_match(text))
}

/// The classes of secret detected in `text`, in detector order, de-duplicated.
#[must_use]
pub fn detect(text: &str) -> Vec<SecretKind> {
    let mut kinds = Vec::new();
    for p in patterns() {
        if p.regex.is_match(text) && !kinds.contains(&p.kind) {
            kinds.push(p.kind);
        }
    }
    kinds
}

/// Return `text` with every detected secret replaced by [`REDACTED`].
#[must_use]
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for p in patterns() {
        out = p.regex.replace_all(&out, REDACTED).into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_each_secret_class() {
        assert!(contains_secret("key = sk-abcdefghijklmnopqrstuvwxyz0123"));
        assert!(contains_secret("Authorization: Bearer abcdef123456ghijkl"));
        assert!(contains_secret(
            "-----BEGIN RSA PRIVATE KEY-----\nMIIB\n-----END RSA PRIVATE KEY-----"
        ));
        assert!(contains_secret("password = hunter2hunter2"));
        assert!(contains_secret("id AKIAIOSFODNN7EXAMPLE here"));
        assert!(contains_secret(
            "postgres://admin:s3cretP@db.example.com:5432/app"
        ));
    }

    #[test]
    fn clean_text_passes_through_unchanged() {
        let clean = "The quick brown fox jumps over the lazy dog. secret = ok";
        assert!(!contains_secret(clean));
        assert_eq!(redact(clean), clean);
    }

    #[test]
    fn redaction_removes_the_secret_value() {
        let input = "OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz0123";
        let out = redact(input);
        assert!(!out.contains("sk-abcdefghijklmnopqrstuvwxyz0123"));
        assert!(out.contains(REDACTED));
    }

    #[test]
    fn detect_lists_classes() {
        let kinds = detect("Bearer abcdefgh12345678");
        assert!(kinds.contains(&SecretKind::BearerToken));
    }
}
