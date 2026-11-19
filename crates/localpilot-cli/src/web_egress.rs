//! Gated web egress shared by the network-reaching features.
//!
//! The pure policy (which hosts may be reached, consent, the audit-line shape)
//! lives in `localpilot-research`; this module owns the network half: the
//! destination check applied to every request and redirect hop, the redirect
//! chain itself, and the append-only egress audit log. Web research builds on
//! it, so any other feature that fetches on the user's behalf gets the same
//! boundary instead of a second copy of it.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use localpilot_research::{AuditEntry, EgressOutcome, EgressRequest, SourceError, WebAccess};

/// Total request timeout for a web fetch, mirroring the `fetch` builtin's bound.
pub(crate) const WEB_FETCH_TIMEOUT_SECS: u64 = 30;
/// Connect-phase timeout, so a stalled connect fails fast under the total.
pub(crate) const WEB_CONNECT_TIMEOUT_SECS: u64 = 10;

/// Redirect hops one candidate URL may be followed through before the chain is
/// abandoned (LocalHub#42). Generous for the real shapes this exists to serve —
/// a search provider's attribution wrapper, an http→https upgrade, a canonical
/// host move, a relocated documentation page — and small enough that a chain
/// cannot become an unbounded crawl. Cycles are caught separately, so this
/// bounds honest chains rather than loops.
pub(crate) const WEB_MAX_REDIRECT_HOPS: usize = 5;

/// Whether one destination URL may be reached at all, decided independently of
/// how it was arrived at. The browser render gate and the static fetch's
/// redirect loop both route through [`decide_destination`], so a navigation and
/// a 3xx hop enforce the *same* boundary rather than two drifting copies of it
/// (LocalHub#37, LocalHub#42).
pub(crate) enum DestinationVerdict {
    /// Reachable: http/https, not internal, and allowed by the active reach.
    Allowed,
    /// Not http/https (local file, custom scheme, …).
    BlockedScheme,
    /// Loopback, link-local, private-network, or unspecified address — refused
    /// unconditionally, ahead of the allowlist, so an open-web reach can never
    /// be turned into an SSRF channel.
    BlockedInternal,
    /// A public destination the active reach does not permit. A destination
    /// needing confirmation counts as blocked: the grant in hand is the only
    /// authority, and a redirect must never be the thing that widens it.
    Blocked,
}

/// Decide whether `parsed` may be reached under `access`. Pure: it audits
/// nothing and mutates nothing, so each caller can record the outcome in its
/// own vocabulary.
///
/// `origin_host`, when given, is the host that referred us here (the host that
/// issued a redirect). A destination on that same host inherits its permission
/// and skips the internal-address guard: continuing a conversation with a host
/// already being fetched grants no reach that was not already granted, so an
/// internal host someone deliberately allowlisted can still redirect within
/// itself. The guard still refuses a *cross-host* hop to an internal address,
/// which is the case it exists for — an allowlisted public page steering the
/// fetcher at loopback or a metadata endpoint. A caller with no referring host
/// (the browser render gate) passes `None` and gets the strict rule.
pub(crate) fn decide_destination(
    access: &WebAccess,
    parsed: &reqwest::Url,
    origin_host: Option<&str>,
    purpose: &str,
) -> DestinationVerdict {
    if !matches!(parsed.scheme(), "http" | "https") {
        return DestinationVerdict::BlockedScheme;
    }
    let host = parsed.host_str().unwrap_or_default();
    let same_host_as_origin = origin_host.is_some_and(|origin| origin.eq_ignore_ascii_case(host));
    if is_internal_host(host) && !same_host_as_origin {
        return DestinationVerdict::BlockedInternal;
    }
    // A redirect is a *new* destination, so it is asked about like any other.
    // Following a hop the operator never saw, on the strength of an approval they
    // gave for a different host, is exactly the reach the prompt exists to stop.
    match access.decide(&EgressRequest {
        host,
        url: parsed.as_str(),
        purpose,
    }) {
        EgressOutcome::Fetch => DestinationVerdict::Allowed,
        EgressOutcome::Skip | EgressOutcome::Off => DestinationVerdict::Blocked,
    }
}

/// Whether a host is an internal/private destination the renderer must never
/// reach, regardless of the allowlist: `localhost`, or an IP literal in a
/// loopback, link-local, private, or unspecified range (SSRF guard). A public
/// DNS name returns `false` (its resolved address is the browser's concern; the
/// allowlist governs which names are reachable at all).
pub(crate) fn is_internal_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let Ok(ip) = host.parse::<std::net::IpAddr>() else {
        return false;
    };
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
        }
        std::net::IpAddr::V6(v6) => {
            let segments = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                // Unique-local fc00::/7 and link-local fe80::/10 (is_unique_local /
                // is_unicast_link_local are unstable, so test the prefixes here).
                || (segments[0] & 0xfe00) == 0xfc00
                || (segments[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Why a redirect chain ended without producing a page.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RedirectStop {
    /// The destination is not permitted (scheme, internal address, or reach).
    Blocked,
    /// No `Location` header, or one that will not parse against the current URL.
    Malformed,
    /// The chain revisited a URL it had already fetched.
    Cycle,
    /// The chain exceeded [`WEB_MAX_REDIRECT_HOPS`].
    DepthExceeded,
}

impl RedirectStop {
    /// The audit decision and account note wording for this stop.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Blocked => "redirect-blocked",
            Self::Malformed => "redirect-malformed",
            Self::Cycle => "redirect-cycle",
            Self::DepthExceeded => "redirect-depth-exceeded",
        }
    }
}

/// Parse the host from a URL with a real parser. `None` when the URL does not
/// parse or carries no host.
pub(crate) fn parse_host(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .host_str()
        .map(str::to_string)
}

/// Whether a fetched body should be reduced from HTML to text before it becomes
/// evidence. True on an HTML-ish `Content-Type`, or — when the server sent none
/// — on a body that opens with an HTML marker. A declared non-HTML type (plain
/// text, Markdown, JSON) is kept verbatim so its exact content survives.
pub(crate) fn is_html(content_type: &str, body: &str) -> bool {
    if content_type.contains("text/html") || content_type.contains("application/xhtml") {
        return true;
    }
    if content_type.is_empty() {
        let head: String = body.trim_start().chars().take(512).collect();
        let head = head.to_ascii_lowercase();
        return head.starts_with("<!doctype html") || head.contains("<html");
    }
    false
}

/// Truncate a fetched (already-reduced) body to at most `max_bytes`, never
/// splitting a UTF-8 char, preferring a line boundary, and — when a cut
/// happens — saying so explicitly rather than ending mid-sentence with no
/// explanation (the finding's provenance URL points at the full source).
pub(crate) fn bound_body(body: &str, max_bytes: usize) -> String {
    if body.len() <= max_bytes {
        return body.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let head = &body[..end];
    // A line boundary keeps the cut readable; fall back to the plain cut when
    // the content is one enormous line and a line cut would discard too much.
    let cut = match head.rfind('\n') {
        Some(newline) if newline >= end / 2 => newline,
        _ => end,
    };
    format!(
        "{}\n… (fetched content truncated at the per-fetch bound; full source at the cited URL)",
        &body[..cut]
    )
}

/// One audit record. `question` carries the **redacted** query, never raw text.
pub(crate) fn audit_entry(url: &str, host: &str, decision: &str, question: &str) -> AuditEntry {
    AuditEntry {
        url: url.to_string(),
        host: host.to_string(),
        decision: decision.to_string(),
        question: question.to_string(),
    }
}

/// Append one audit line to the egress log, creating the file and its parent on
/// first use. Append-only, one line per outbound request or skip.
pub(crate) fn append_audit(path: &Path, entry: &AuditEntry) -> Result<(), SourceError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|error| SourceError::new("web", format!("audit dir: {error}")))?;
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| SourceError::new("web", format!("audit open: {error}")))?;
    writeln!(file, "{}", entry.to_line())
        .map_err(|error| SourceError::new("web", format!("audit write: {error}")))?;
    Ok(())
}

/// Per-hop policy a caller supplies to [`fetch_chain`]: host pacing and
/// cooldown, and whether a redirect destination may be followed. Research
/// implements it over its run-scoped politeness state and its [`WebAccess`]
/// grant; another caller can bring its own consent rules without copying the
/// chain. `allow_redirect` is async so a caller can ask someone before a hop.
#[async_trait]
pub(crate) trait HopPolicy: Send + Sync {
    /// Whether `host` is cooling down after an earlier rate-limit or server
    /// error; a cooled host is not contacted again.
    fn cooled(&self, host: &str) -> bool;
    /// Wait out any pause owed before contacting `host` again.
    async fn pace(&self, host: &str);
    /// Record one response from `host`: how long it took and whether it asks
    /// for a cooldown (429 or a server error).
    fn record(&self, host: &str, took: Duration, cool_down: bool);
    /// Whether the redirect destination `next`, issued by `origin_host`, may be
    /// requested.
    async fn allow_redirect(&self, next: &reqwest::Url, origin_host: &str) -> bool;
    /// How a URL is written into the audit log. The default keeps it whole.
    fn audit_locator(&self, url: &str) -> String {
        url.to_string()
    }
    /// The most body bytes to read; `None` reads the whole body.
    fn body_cap(&self) -> Option<usize> {
        None
    }
    /// Whether a rate-limit or server-error response ends the chain as a
    /// failure without its body. When `false` the body is read and returned
    /// like any other unsuccessful response.
    fn fail_on_cooldown_status(&self) -> bool {
        true
    }
}

/// A response that ended the chain with a body, whatever its status.
pub(crate) struct ChainPage {
    /// The URL the body actually came from (after any followed redirects).
    pub(crate) final_url: String,
    /// How many redirect hops were followed to reach `final_url`.
    pub(crate) hops: usize,
    pub(crate) status: reqwest::StatusCode,
    /// The response `Content-Type`, lower-cased; empty when absent.
    pub(crate) content_type: String,
    pub(crate) body: String,
    /// Whether the body was cut at the caller's byte cap.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub(crate) truncated: bool,
}

/// How one gated fetch chain ended.
pub(crate) enum ChainOutcome {
    /// A response body was read. Its status may still be unsuccessful; the
    /// caller decides what a non-2xx body means.
    Page(Box<ChainPage>),
    /// The host is cooling down; nothing was requested.
    Cooled,
    /// A redirect chain ended without a page, for the recorded reason.
    Redirected(RedirectStop),
    /// A transport error, a rate-limit or server-error status, or an
    /// unreadable body.
    Failed,
}

/// Fetch `url` on `host`, following redirects only through `policy`, and
/// write an audit line (carrying the redacted `query`) for every request,
/// followed hop, stop, cooldown and failure. The client must not follow
/// redirects itself (`redirect::Policy::none()`): every hop is decided here so
/// none can bypass the gate or the audit log. Each hop gets the same cooldown,
/// pacing and failure handling as the first request.
pub(crate) async fn fetch_chain(
    client: &reqwest::Client,
    policy: &dyn HopPolicy,
    audit_log: &Path,
    url: &str,
    host: &str,
    query: &str,
) -> Result<ChainOutcome, SourceError> {
    let mut current_url = url.to_string();
    let mut current_host = host.to_string();
    // Every URL already requested in this chain, so a loop is caught rather
    // than walked until the hop limit.
    let mut visited: std::collections::BTreeSet<String> =
        std::iter::once(current_url.clone()).collect();
    let mut hops = 0usize;

    loop {
        // A host that rate-limited or errored earlier stays cooled down —
        // 429/5xx are host-level signals, not per-URL ones.
        if policy.cooled(&current_host) {
            append_audit(
                audit_log,
                &audit_entry(
                    &policy.audit_locator(&current_url),
                    &current_host,
                    "host-cooldown",
                    query,
                ),
            )?;
            return Ok(ChainOutcome::Cooled);
        }
        policy.pace(&current_host).await;
        append_audit(
            audit_log,
            &audit_entry(
                &policy.audit_locator(&current_url),
                &current_host,
                "allowed",
                query,
            ),
        )?;
        let fetch_started = Instant::now();
        let response = match client.get(&current_url).send().await {
            Ok(response) => response,
            Err(_) => {
                append_audit(
                    audit_log,
                    &audit_entry(
                        &policy.audit_locator(&current_url),
                        &current_host,
                        "fetch-error",
                        query,
                    ),
                )?;
                return Ok(ChainOutcome::Failed);
            }
        };
        let status = response.status();
        let cool_down = status.as_u16() == 429 || status.is_server_error();
        policy.record(&current_host, fetch_started.elapsed(), cool_down);
        if cool_down && policy.fail_on_cooldown_status() {
            return Ok(ChainOutcome::Failed);
        }
        if status.is_redirection() {
            let stop = match redirect_destination(policy, &current_url, &response).await {
                Ok(next) => {
                    let next_host = next.host_str().unwrap_or_default().to_string();
                    let next_url = next.to_string();
                    if hops + 1 > WEB_MAX_REDIRECT_HOPS {
                        Err(RedirectStop::DepthExceeded)
                    } else if !visited.insert(next_url.clone()) {
                        Err(RedirectStop::Cycle)
                    } else {
                        append_audit(
                            audit_log,
                            &audit_entry(
                                &policy.audit_locator(&next_url),
                                &next_host,
                                "redirect-followed",
                                query,
                            ),
                        )?;
                        hops += 1;
                        current_url = next_url;
                        current_host = next_host;
                        Ok(())
                    }
                }
                Err(stop) => Err(stop),
            };
            match stop {
                Ok(()) => continue,
                Err(stop) => {
                    append_audit(
                        audit_log,
                        &audit_entry(
                            &policy.audit_locator(&current_url),
                            &current_host,
                            stop.reason(),
                            query,
                        ),
                    )?;
                    return Ok(ChainOutcome::Redirected(stop));
                }
            }
        }
        // Capture the content type before `text()` consumes the response.
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let read = match policy.body_cap() {
            None => response.text().await.map(|body| (body, false)),
            Some(cap) => read_capped(response, cap).await,
        };
        let (body, truncated) = match read {
            Ok(read) => read,
            Err(_) => {
                append_audit(
                    audit_log,
                    &audit_entry(
                        &policy.audit_locator(&current_url),
                        &current_host,
                        "fetch-error",
                        query,
                    ),
                )?;
                return Ok(ChainOutcome::Failed);
            }
        };
        return Ok(ChainOutcome::Page(Box::new(ChainPage {
            final_url: current_url,
            hops,
            status,
            content_type,
            body,
            truncated,
        })));
    }
}

/// Resolve one redirect hop's destination from `response` and ask `policy`
/// whether it may be followed. Does not audit: the chain records the hop in its
/// own vocabulary.
async fn redirect_destination(
    policy: &dyn HopPolicy,
    current_url: &str,
    response: &reqwest::Response,
) -> Result<reqwest::Url, RedirectStop> {
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(RedirectStop::Malformed)?;
    // A `Location` may be relative ("/article", "../x"); resolve it against
    // the URL that produced it, exactly as a browser would.
    let base = reqwest::Url::parse(current_url).map_err(|_| RedirectStop::Malformed)?;
    let next = base.join(location).map_err(|_| RedirectStop::Malformed)?;
    let origin_host = base.host_str().unwrap_or_default();
    if policy.allow_redirect(&next, origin_host).await {
        Ok(next)
    } else {
        Err(RedirectStop::Blocked)
    }
}

/// Read at most `cap` body bytes, stopping the download there rather than
/// reading everything and cutting afterwards. Decoded as UTF-8, with invalid
/// bytes replaced and a character split by the cap dropped.
async fn read_capped(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<(String, bool), reqwest::Error> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await? {
        let room = cap.saturating_sub(bytes.len());
        if chunk.len() >= room {
            bytes.extend_from_slice(&chunk[..room]);
            // Stop at the bound without waiting for more: a slow or open
            // stream must not turn a full result into a timeout. Whether more
            // would have followed is unknown, so the body counts as cut.
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    if truncated {
        if let Err(error) = std::str::from_utf8(&bytes) {
            if error.error_len().is_none() {
                bytes.truncate(error.valid_up_to());
            }
        }
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// A policy with no pacing that allows every redirect unless told
    /// otherwise, and records what the chain asked it.
    #[derive(Default)]
    struct TestPolicy {
        cooled: bool,
        refuse_redirects: bool,
        recorded: Mutex<Vec<(String, bool)>>,
    }

    #[async_trait]
    impl HopPolicy for TestPolicy {
        fn cooled(&self, _host: &str) -> bool {
            self.cooled
        }
        async fn pace(&self, _host: &str) {}
        fn record(&self, host: &str, _took: Duration, cool_down: bool) {
            if let Ok(mut recorded) = self.recorded.lock() {
                recorded.push((host.to_string(), cool_down));
            }
        }
        async fn allow_redirect(&self, _next: &reqwest::Url, _origin_host: &str) -> bool {
            !self.refuse_redirects
        }
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    fn decisions(audit: &Path) -> Vec<String> {
        std::fs::read_to_string(audit)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                line.split_whitespace()
                    .find_map(|field| field.strip_prefix("decision=").map(str::to_string))
            })
            .collect()
    }

    async fn redirecting_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/final"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/final"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("final body", "text/plain"))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_permitted_redirect_is_followed_and_every_hop_is_audited() {
        let server = redirecting_server().await;
        let url = format!("{}/start", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let policy = TestPolicy::default();

        let outcome = fetch_chain(&client(), &policy, &audit, &url, &host, "q")
            .await
            .unwrap();
        let ChainOutcome::Page(page) = outcome else {
            panic!("expected a page");
        };
        assert_eq!(page.final_url, format!("{}/final", server.uri()));
        assert_eq!(page.hops, 1);
        assert!(page.status.is_success());
        assert_eq!(page.content_type, "text/plain");
        assert_eq!(page.body, "final body");
        assert_eq!(
            decisions(&audit),
            ["allowed", "redirect-followed", "allowed"],
        );
        assert_eq!(
            policy.recorded.lock().unwrap().len(),
            2,
            "both hops recorded"
        );
    }

    #[tokio::test]
    async fn a_refused_redirect_stops_the_chain_before_the_destination() {
        let server = redirecting_server().await;
        let url = format!("{}/start", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let policy = TestPolicy {
            refuse_redirects: true,
            ..TestPolicy::default()
        };

        let outcome = fetch_chain(&client(), &policy, &audit, &url, &host, "q")
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            ChainOutcome::Redirected(RedirectStop::Blocked)
        ));
        assert_eq!(decisions(&audit), ["allowed", "redirect-blocked"]);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "the destination was never contacted");
    }

    #[tokio::test]
    async fn a_cooled_host_is_not_contacted() {
        let server = redirecting_server().await;
        let url = format!("{}/start", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let policy = TestPolicy {
            cooled: true,
            ..TestPolicy::default()
        };

        let outcome = fetch_chain(&client(), &policy, &audit, &url, &host, "q")
            .await
            .unwrap();
        assert!(matches!(outcome, ChainOutcome::Cooled));
        assert_eq!(decisions(&audit), ["host-cooldown"]);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unsuccessful_status_still_returns_its_body_for_the_caller_to_judge() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_body_raw("not here", "text/plain"))
            .mount(&server)
            .await;
        let url = format!("{}/missing", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");

        let outcome = fetch_chain(&client(), &TestPolicy::default(), &audit, &url, &host, "q")
            .await
            .unwrap();
        let ChainOutcome::Page(page) = outcome else {
            panic!("expected a page");
        };
        assert_eq!(page.status.as_u16(), 404);
        assert_eq!(page.body, "not here");
    }

    #[tokio::test]
    async fn a_rate_limited_response_fails_and_asks_for_a_cooldown() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let url = format!("{}/busy", server.uri());
        let host = parse_host(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let policy = TestPolicy::default();

        let outcome = fetch_chain(&client(), &policy, &audit, &url, &host, "q")
            .await
            .unwrap();
        assert!(matches!(outcome, ChainOutcome::Failed));
        assert_eq!(*policy.recorded.lock().unwrap(), [(host, true)]);
    }
}
