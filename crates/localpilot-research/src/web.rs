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
    /// Active but the host carries no standing approval — the operator must be
    /// asked before the fetch, and told no on decline.
    ///
    /// This name used to be a promise the code did not keep: every consumer
    /// treated it as *skip and audit*, so nothing was ever confirmed by anyone.
    /// It is now what it says — see [`resolve`].
    NeedsConfirmation,
}

/// What the operator answered when asked to approve one outbound request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentAnswer {
    /// Permit this one request. The next request asks again.
    AllowOnce,
    /// Refuse this request.
    Deny,
    /// Permit every outbound request for the rest of this session, without
    /// asking again. Never persisted: the next session starts from denied.
    AllowSessionWide,
}

/// One prospective outbound request, described for the person being asked.
///
/// The point of carrying all three is that a host name alone does not let anyone
/// decide anything. *Why* the request is being made is what makes an approval
/// meaningful rather than a reflex.
#[derive(Debug, Clone, Copy)]
pub struct EgressRequest<'a> {
    /// The host that would be contacted.
    pub host: &'a str,
    /// The full URL, so the path is visible and not just the domain.
    pub url: &'a str,
    /// What this request is for, in the caller's own words — the research
    /// query, the skill being looked up, the redirect being followed.
    pub purpose: &'a str,
}

/// The seam that asks a human. Pure policy code never touches a terminal.
pub trait EgressApproval {
    /// Ask about one request. An implementation that cannot ask must answer
    /// [`ConsentAnswer::Deny`] — see [`DenyWhenNobodyToAsk`].
    fn ask(&mut self, request: &EgressRequest<'_>) -> ConsentAnswer;
}

/// The approval used when there is nobody to ask: refuse.
///
/// Every non-interactive surface — a piped run, CI, a scheduled job, an agent
/// with no terminal — gets this. It is a real behaviour change: automated runs
/// that reach non-allowlisted hosts today will stop reaching them. That is the
/// point. An unattended process cannot consent on a person's behalf, and a
/// default that let it would make consent decorative everywhere else.
///
/// The remedy is not a runtime override, which would be the same hole by another
/// name. It is to put the host on the configured allowlist — a deliberate,
/// reviewable, written act.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyWhenNobodyToAsk;

impl EgressApproval for DenyWhenNobodyToAsk {
    fn ask(&mut self, _request: &EgressRequest<'_>) -> ConsentAnswer {
        ConsentAnswer::Deny
    }
}

/// What actually happens to one prospective request, once everyone who gets a
/// say has had it.
///
/// Separate from [`FetchDecision`] on purpose. `FetchDecision` is what *standing
/// policy* knows before anyone is asked, and its `NeedsConfirmation` means
/// exactly that — nobody has been asked yet. This is the answer after asking,
/// and it has three outcomes rather than the same three words meaning something
/// else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressOutcome {
    /// Go ahead.
    Fetch,
    /// Do not send this one — policy refused it, or a person did. **Audit it.**
    /// A refusal nobody can see afterwards is not a record of anything.
    Skip,
    /// Web access is off altogether: config disabled, or no per-session opt-in.
    /// Nothing is attempted and there is nothing to record per request.
    Off,
}

/// A shared handle to whoever is answering. Shared because the research host
/// clones its access into per-source and per-redirect scopes, and every clone
/// has to reach the same person.
pub type SharedApproval = std::sync::Arc<std::sync::Mutex<dyn EgressApproval + Send>>;

/// Wrap an approval for use by [`WebAccess::with_approval`].
#[must_use]
pub fn shared_approval<A: EgressApproval + Send + 'static>(approval: A) -> SharedApproval {
    std::sync::Arc::new(std::sync::Mutex::new(approval))
}

/// Per-session web-research access state.
///
/// `enabled` comes from `[research].web.enabled` (static config). `session_opt_in`
/// is the loud, per-session consent the operator grants at runtime; it is never
/// persisted, so every new session starts denied even when config permits.
#[derive(Clone)]
pub struct WebAccess {
    enabled: bool,
    session_opt_in: bool,
    allowlist: Vec<String>,
    disallowlist: Vec<String>,
    /// Session-wide blanket approval, **shared across clones**.
    ///
    /// Shared deliberately. The research host clones its access into per-source
    /// and per-redirect scopes, and a grant recorded on a copy would be
    /// forgotten the moment that copy went out of scope — so the operator would
    /// be asked again after answering "allow everything", which is the one
    /// outcome that would teach them the prompt is noise.
    allow_all: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whoever answers when a request has no standing approval.
    ///
    /// It lives *inside* the access rather than beside it so that no consumer can
    /// obtain a permission without the consent seam being present. The previous
    /// shape — a decision type whose `NeedsConfirmation` variant every caller was
    /// trusted to honour — was honoured by none of the three.
    ///
    /// Defaults to [`DenyWhenNobodyToAsk`], so a `WebAccess` built without
    /// wiring one up refuses rather than waves things through.
    approval: SharedApproval,
}

impl std::fmt::Debug for WebAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebAccess")
            .field("enabled", &self.enabled)
            .field("session_opt_in", &self.session_opt_in)
            .field("allowlist", &self.allowlist)
            .field("disallowlist", &self.disallowlist)
            .field("session_wide", &self.is_session_wide())
            .finish_non_exhaustive()
    }
}

impl WebAccess {
    /// Construct from config. Starts **inactive**: `session_opt_in` is false
    /// until [`grant_session`](Self::grant_session) is called. `disallowlist`
    /// takes priority over `allowlist` (a disallowlisted host is skipped even
    /// when the allowlist — including `*` — would permit it).
    #[must_use]
    pub fn new(enabled: bool, allowlist: Vec<String>, disallowlist: Vec<String>) -> Self {
        Self {
            enabled,
            session_opt_in: false,
            allowlist,
            disallowlist,
            allow_all: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            approval: shared_approval(DenyWhenNobodyToAsk),
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

    /// Whether outbound web research is currently permitted at all: config on
    /// **and** per-session consent granted.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled && self.session_opt_in
    }

    /// Attach the surface that asks a person. Without this, every request that
    /// has no standing approval is refused.
    #[must_use]
    pub fn with_approval(mut self, approval: SharedApproval) -> Self {
        self.approval = approval;
        self
    }

    /// Decide one prospective request, **asking** when policy has no standing
    /// answer — the seam every egress consumer goes through.
    ///
    /// Returns [`FetchDecision::Allowed`] to proceed or
    /// [`FetchDecision::Disabled`] to skip; the caller audits a skip exactly as
    /// it does today. A poisoned approval lock is a refusal, not a panic: losing
    /// the ability to ask must never become permission.
    #[must_use]
    pub fn decide(&self, request: &EgressRequest<'_>) -> EgressOutcome {
        if !self.is_active() {
            return EgressOutcome::Off;
        }
        match self.decide_host(request.host) {
            FetchDecision::Allowed => EgressOutcome::Fetch,
            // Disallowlisted. A standing refusal the operator configured, so
            // nobody is asked — but it is a refusal of *this request*, not the
            // end of web access, and it is recorded like any other.
            FetchDecision::Disabled => EgressOutcome::Skip,
            FetchDecision::NeedsConfirmation => {
                let Ok(mut approval) = self.approval.lock() else {
                    return EgressOutcome::Skip;
                };
                match approval.ask(request) {
                    ConsentAnswer::AllowOnce => EgressOutcome::Fetch,
                    ConsentAnswer::AllowSessionWide => {
                        drop(approval);
                        self.grant_session_wide();
                        EgressOutcome::Fetch
                    }
                    ConsentAnswer::Deny => EgressOutcome::Skip,
                }
            }
        }
    }

    /// Record a session-wide blanket approval. Shared with every clone of this
    /// access, and never persisted — the next session starts from denied.
    pub fn grant_session_wide(&self) {
        self.allow_all
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether a session-wide blanket approval is in force.
    #[must_use]
    pub fn is_session_wide(&self) -> bool {
        self.allow_all.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Decide what is permitted for `host`, from standing policy alone. A
    /// [`FetchDecision::NeedsConfirmation`] means *nobody has been asked yet* —
    /// go through [`resolve`] to ask. `host` must already be parsed from the URL
    /// by the caller.
    #[must_use]
    pub fn decide_host(&self, host: &str) -> FetchDecision {
        if !self.is_active() {
            return FetchDecision::Disabled;
        }
        // Disallow wins over allow *and* over a session-wide grant: a blocked
        // host stays blocked even after the operator allowed everything. Someone
        // approving requests one at a time is not revoking a standing refusal
        // they configured deliberately.
        if host_matches(&self.disallowlist, host) {
            return FetchDecision::Disabled;
        }
        if self.allow_all.load(std::sync::atomic::Ordering::Relaxed) {
            return FetchDecision::Allowed;
        }
        if host_matches(&self.allowlist, host) {
            FetchDecision::Allowed
        } else {
            FetchDecision::NeedsConfirmation
        }
    }
}

/// Whether `host` matches any pattern in `patterns`. A pattern is one of:
/// `*` (matches every host); `*.example.com` (matches `example.com` and any
/// subdomain); or a bare domain, matched as an exact (case-insensitive) host or
/// a subdomain of it. An empty host or empty pattern never matches, so
/// `evildocs.rs` is not matched by `docs.rs` and `docs.rs.evil.com` is not
/// matched by `docs.rs`.
#[must_use]
pub fn host_matches(patterns: &[String], host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim().to_ascii_lowercase();
        if pattern.is_empty() {
            return false;
        }
        if pattern == "*" {
            return true;
        }
        // `*.domain` matches the domain itself and any subdomain.
        let domain = pattern.strip_prefix("*.").unwrap_or(&pattern);
        !domain.is_empty() && (host == *domain || host.ends_with(&format!(".{domain}")))
    })
}

/// Whether `host` matches the allowlist. Retained as a thin alias over
/// [`host_matches`] for callers that pass a single list.
#[must_use]
pub fn host_allowed(allowlist: &[String], host: &str) -> bool {
    host_matches(allowlist, host)
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
        let mut access = WebAccess::new(true, vec!["docs.rs".to_string()], Vec::new());
        assert!(!access.is_active(), "config-on alone must not activate");
        assert_eq!(access.decide_host("docs.rs"), FetchDecision::Disabled);
        access.grant_session();
        assert!(access.is_active());
        assert_eq!(access.decide_host("docs.rs"), FetchDecision::Allowed);
    }

    #[test]
    fn config_off_cannot_be_opted_in() {
        let mut access = WebAccess::new(false, vec!["docs.rs".to_string()], Vec::new());
        access.grant_session();
        assert!(
            !access.is_active(),
            "config-off is not overridable at runtime"
        );
        assert_eq!(access.decide_host("docs.rs"), FetchDecision::Disabled);
    }

    #[test]
    fn non_allowlisted_host_needs_confirmation() {
        let mut access = WebAccess::new(true, vec!["docs.rs".to_string()], Vec::new());
        access.grant_session();
        assert_eq!(
            access.decide_host("crates.io"),
            FetchDecision::NeedsConfirmation
        );
    }

    #[test]
    fn empty_allowlist_confirms_everything() {
        let mut access = WebAccess::new(true, Vec::new(), Vec::new());
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
    fn star_matches_every_host() {
        let list = vec!["*".to_string()];
        assert!(host_matches(&list, "anything.example.com"));
        assert!(host_matches(&list, "docs.rs"));
        assert!(!host_matches(&list, ""), "empty host still never matches");
    }

    #[test]
    fn star_dot_domain_matches_domain_and_subdomains() {
        let list = vec!["*.pinterest.com".to_string()];
        assert!(host_matches(&list, "pinterest.com"), "apex included");
        assert!(host_matches(&list, "www.pinterest.com"));
        assert!(!host_matches(&list, "notpinterest.com"));
    }

    #[test]
    fn disallowlist_beats_allowlist_including_wildcard() {
        let mut access = WebAccess::new(
            true,
            vec!["*".to_string()],
            vec!["reddit.com".to_string(), "*.pinterest.com".to_string()],
        );
        access.grant_session();
        // `*` allows the open web...
        assert_eq!(access.decide_host("docs.rs"), FetchDecision::Allowed);
        // ...but disallowlisted hosts are skipped outright, subdomains included.
        assert_eq!(access.decide_host("reddit.com"), FetchDecision::Disabled);
        assert_eq!(
            access.decide_host("old.reddit.com"),
            FetchDecision::Disabled
        );
        assert_eq!(
            access.decide_host("www.pinterest.com"),
            FetchDecision::Disabled
        );
    }

    #[test]
    fn disallowlist_beats_an_exact_allow_entry() {
        let mut access = WebAccess::new(
            true,
            vec!["docs.rs".to_string()],
            vec!["docs.rs".to_string()],
        );
        access.grant_session();
        assert_eq!(access.decide_host("docs.rs"), FetchDecision::Disabled);
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod consent_tests {
    use super::*;

    /// An approval that answers from a script and counts how often it was asked.
    struct Scripted {
        answers: Vec<ConsentAnswer>,
        asked: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl EgressApproval for Scripted {
        fn ask(&mut self, _request: &EgressRequest<'_>) -> ConsentAnswer {
            self.asked
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self.answers.is_empty() {
                ConsentAnswer::Deny
            } else {
                self.answers.remove(0)
            }
        }
    }

    fn access_with(
        answers: Vec<ConsentAnswer>,
    ) -> (WebAccess, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut access = WebAccess::new(
            true,
            vec!["allowed.example".to_string()],
            vec!["blocked.example".to_string()],
        );
        access.grant_session();
        let access = access.with_approval(shared_approval(Scripted {
            answers,
            asked: std::sync::Arc::clone(&asked),
        }));
        (access, asked)
    }

    fn request(host: &str) -> EgressRequest<'_> {
        EgressRequest {
            host,
            url: "https://example/x",
            purpose: "a test",
        }
    }

    #[test]
    fn an_allowlisted_host_is_not_asked_about() {
        let (access, asked) = access_with(vec![]);
        assert_eq!(
            access.decide(&request("allowed.example")),
            EgressOutcome::Fetch
        );
        assert_eq!(
            asked.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a host the operator allowlisted deliberately is consent already given"
        );
    }

    #[test]
    fn an_unknown_host_is_asked_about_every_time() {
        let (access, asked) = access_with(vec![ConsentAnswer::AllowOnce, ConsentAnswer::AllowOnce]);
        assert_eq!(
            access.decide(&request("unknown.example")),
            EgressOutcome::Fetch
        );
        assert_eq!(
            access.decide(&request("unknown.example")),
            EgressOutcome::Fetch
        );
        assert_eq!(
            asked.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "allow-once means once; the second request asks again"
        );
    }

    #[test]
    fn a_refusal_is_a_skip_so_it_can_be_audited_not_an_off_switch() {
        // This distinction is load-bearing and was got wrong once: a refused
        // request that reports `Off` is never written to the audit log, so the
        // record of what was refused silently disappears — and in the source
        // loop, `Off` also ends the whole gather rather than skipping one host.
        let (access, _) = access_with(vec![ConsentAnswer::Deny]);
        assert_eq!(
            access.decide(&request("unknown.example")),
            EgressOutcome::Skip
        );
    }

    #[test]
    fn a_session_wide_grant_reaches_every_clone_and_stops_the_asking() {
        let (access, asked) = access_with(vec![ConsentAnswer::AllowSessionWide]);
        // The research host clones its access into per-source and per-redirect
        // scopes. A grant that did not reach the clones would ask again after
        // the operator said "allow everything".
        let clone = access.clone();
        assert_eq!(
            access.decide(&request("first.example")),
            EgressOutcome::Fetch
        );
        assert_eq!(
            clone.decide(&request("second.example")),
            EgressOutcome::Fetch
        );
        assert_eq!(
            asked.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "asked once, granted for the session, and the clone honours it"
        );
        assert!(clone.is_session_wide());
    }

    #[test]
    fn a_disallowlisted_host_stays_blocked_under_a_session_wide_grant() {
        let (access, asked) = access_with(vec![ConsentAnswer::AllowSessionWide]);
        assert_eq!(
            access.decide(&request("first.example")),
            EgressOutcome::Fetch
        );
        // Approving requests one at a time is not revoking a standing refusal
        // configured deliberately.
        assert_eq!(
            access.decide(&request("blocked.example")),
            EgressOutcome::Skip
        );
        assert_eq!(asked.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn with_nobody_to_ask_an_unknown_host_is_refused_and_an_allowlisted_one_is_not() {
        // The default approval. Every non-interactive surface gets this, and the
        // remedy for a run that needs a host is the configured allowlist — not a
        // runtime override, which would be the same hole by another name.
        let mut access = WebAccess::new(true, vec!["allowed.example".to_string()], Vec::new());
        access.grant_session();
        assert_eq!(
            access.decide(&request("unknown.example")),
            EgressOutcome::Skip
        );
        assert_eq!(
            access.decide(&request("allowed.example")),
            EgressOutcome::Fetch
        );
    }

    #[test]
    fn config_off_is_off_and_nobody_is_asked() {
        // Config off can never be overridden at runtime, so there is nothing to
        // ask about — and no per-request record to write.
        let access = WebAccess::new(false, vec!["*".to_string()], Vec::new());
        assert_eq!(
            access.decide(&request("anything.example")),
            EgressOutcome::Off
        );
    }

    #[test]
    fn without_the_per_session_opt_in_nothing_egresses() {
        // Config permits it and the allowlist is `*`; the session opt-in is the
        // consent that has not been given, so there is still nothing to ask.
        let fresh = WebAccess::new(true, vec!["*".to_string()], Vec::new());
        assert_eq!(
            fresh.decide(&request("anything.example")),
            EgressOutcome::Off
        );
    }
}
