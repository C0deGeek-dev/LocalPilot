//! Web-egress policy primitives (the gate for `policies/remote-egress.md`).
//!
//! These are pure, host-neutral, and testable: they decide *whether* an
//! outbound request is permitted and how it is recorded, but perform no
//! network I/O and parse no URLs. The binding layer (the CLI) parses a URL into
//! a host with a real parser, asks [`WebAccess`] for a decision, prompts the
//! operator on [`FetchDecision::NeedsConfirmation`], and writes the
//! [`AuditEntry`] — keeping URL parsing and I/O out of this crate.
//!
//! Defaults are fail-closed: a freshly constructed [`WebAccess`] is inactive
//! until both the config switch is on **and** the operator grants per-session
//! consent, and an empty allowlist confirms every host rather than trusting it.

/// What the policy permits for one prospective fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchDecision {
    /// Web research is off (config disabled or no per-session consent). The
    /// host must not fetch.
    Disabled,
    /// Active and the host is on the allowlist — fetch, then audit.
    Allowed,
    /// Active but the host is not allowlisted — the operator must confirm this
    /// host before the fetch; on decline, skip it.
    NeedsConfirmation,
}

/// Per-session web-research access state.
///
/// `enabled` comes from `[research].web.enabled` (static config). `session_opt_in`
/// is the loud, per-session consent the operator grants at runtime; it is never
/// persisted, so every new session starts denied even when config permits.
#[derive(Debug, Clone)]
pub struct WebAccess {
    enabled: bool,
    session_opt_in: bool,
    allowlist: Vec<String>,
}

impl WebAccess {
    /// Construct from config. Starts **inactive**: `session_opt_in` is false
    /// until [`grant_session`](Self::grant_session) is called.
    #[must_use]
    pub fn new(enabled: bool, allowlist: Vec<String>) -> Self {
        Self {
            enabled,
            session_opt_in: false,
            allowlist,
        }
    }

    /// Record the operator's explicit per-session opt-in (the loud consent). A
    /// no-op when config has web disabled — config off can never be overridden
    /// at runtime.
    pub fn grant_session(&mut self) {
        if self.enabled {
            self.session_opt_in = true;
        }
    }

    /// Withdraw per-session consent.
    pub fn revoke_session(&mut self) {
        self.session_opt_in = false;
    }

    /// Whether outbound web research is currently permitted at all: config on
    /// **and** per-session consent granted.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled && self.session_opt_in
    }

    /// Decide what is permitted for `host`. `host` must already be parsed from
    /// the URL by the caller.
    #[must_use]
    pub fn decide_host(&self, host: &str) -> FetchDecision {
        if !self.is_active() {
            return FetchDecision::Disabled;
        }
        if host_allowed(&self.allowlist, host) {
            FetchDecision::Allowed
        } else {
            FetchDecision::NeedsConfirmation
        }
    }
}

/// Whether `host` matches the allowlist: an exact (case-insensitive) match or a
/// subdomain of an allowlisted domain. An empty host or empty allowlist entry
/// never matches, so `evildocs.rs` is not allowed by `docs.rs` and
/// `docs.rs.evil.com` is not allowed by `docs.rs`.
#[must_use]
pub fn host_allowed(allowlist: &[String], host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    allowlist.iter().any(|domain| {
        let domain = domain.trim().to_ascii_lowercase();
        !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")))
    })
}

/// One outbound-request record for the egress audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// The full URL requested.
    pub url: String,
    /// The host parsed from it.
    pub host: String,
    /// The decision that permitted it (`allowed` or `confirmed`).
    pub decision: String,
    /// The sub-question the fetch served.
    pub question: String,
}

impl AuditEntry {
    /// Render a single, newline-free audit line. Field values have their own
    /// newlines flattened to spaces so one request is always one log line.
    #[must_use]
    pub fn to_line(&self) -> String {
        format!(
            "decision={} host={} url={} question={}",
            flatten(&self.decision),
            flatten(&self.host),
            flatten(&self.url),
            flatten(&self.question),
        )
    }
}

fn flatten(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

/// Prepare an outbound query string from a sub-question by applying the host's
/// redactor. Only the sub-question text is ever sent — never gathered evidence
/// or file contents — and this scrubs secrets from it as a second guard.
pub fn prepare_query(redactor: impl Fn(&str) -> String, question: &str) -> String {
    redactor(question)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_access_is_inactive_until_opt_in() {
        let mut access = WebAccess::new(true, vec!["docs.rs".to_string()]);
        assert!(!access.is_active(), "config-on alone must not activate");
        assert_eq!(access.decide_host("docs.rs"), FetchDecision::Disabled);
        access.grant_session();
        assert!(access.is_active());
        assert_eq!(access.decide_host("docs.rs"), FetchDecision::Allowed);
    }

    #[test]
    fn config_off_cannot_be_opted_in() {
        let mut access = WebAccess::new(false, vec!["docs.rs".to_string()]);
        access.grant_session();
        assert!(
            !access.is_active(),
            "config-off is not overridable at runtime"
        );
        assert_eq!(access.decide_host("docs.rs"), FetchDecision::Disabled);
    }

    #[test]
    fn non_allowlisted_host_needs_confirmation() {
        let mut access = WebAccess::new(true, vec!["docs.rs".to_string()]);
        access.grant_session();
        assert_eq!(
            access.decide_host("crates.io"),
            FetchDecision::NeedsConfirmation
        );
    }

    #[test]
    fn empty_allowlist_confirms_everything() {
        let mut access = WebAccess::new(true, Vec::new());
        access.grant_session();
        assert_eq!(
            access.decide_host("docs.rs"),
            FetchDecision::NeedsConfirmation
        );
    }

    #[test]
    fn allowlist_matches_exact_and_subdomain_only() {
        let list = vec!["docs.rs".to_string()];
        assert!(host_allowed(&list, "docs.rs"));
        assert!(host_allowed(&list, "api.docs.rs"));
        assert!(host_allowed(&list, "DOCS.RS"), "match is case-insensitive");
        assert!(!host_allowed(&list, "evildocs.rs"));
        assert!(!host_allowed(&list, "docs.rs.evil.com"));
        assert!(!host_allowed(&list, ""));
        assert!(!host_allowed(&[String::new()], "docs.rs"));
    }

    #[test]
    fn audit_line_is_single_line() {
        let entry = AuditEntry {
            url: "https://docs.rs/x".to_string(),
            host: "docs.rs".to_string(),
            decision: "allowed".to_string(),
            question: "how to\nuse x".to_string(),
        };
        let line = entry.to_line();
        assert!(!line.contains('\n'), "newlines in fields must be flattened");
        assert!(line.contains("host=docs.rs"));
        assert!(line.contains("decision=allowed"));
    }

    #[test]
    fn prepare_query_applies_redactor() {
        let out = prepare_query(|s| s.replace("secret", "[REDACTED]"), "my secret topic");
        assert_eq!(out, "my [REDACTED] topic");
    }
}
