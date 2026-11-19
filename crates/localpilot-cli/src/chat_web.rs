//! Web access for ordinary chat: the gateway behind the `fetch` tool.
//!
//! Chat web access is its own opt-in, separate from web research. A session
//! starts with none. Before the first request to a host, the user is asked —
//! on a consent channel only the person at this host answers, never through
//! the permission profile or the shared approver other devices can answer — and a yes covers that host until
//! the session changes. Every request then goes through the shared gated chain:
//! the destination is checked before the first connection and again on every
//! redirect, host names may only resolve to public addresses, proxies are not
//! used, the download is bounded, and an audit line is written before each
//! request leaves. The deny list and audit log are the ones web research uses.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use localpilot_config::{ChatWebMode, Config};
use localpilot_research::{host_matches, html_to_markdown};
use localpilot_sandbox::{Approver, Effect, Interactivity, PermissionRequest};
use localpilot_tools::{redact_url, WebContext, WebFetchRequest, WebGateway, WebPage, WebRefusal};

use crate::web_egress::{
    append_audit, audit_entry, bound_body, fetch_chain, is_html, ChainOutcome, HopPolicy,
    WEB_CONNECT_TIMEOUT_SECS,
};

/// Default and ceiling for one chat request's total time.
const CHAT_DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const CHAT_MAX_TIMEOUT: Duration = Duration::from_secs(60);
/// Most raw bytes downloaded for one chat request, before any reduction.
const CHAT_MAX_DOWNLOAD_BYTES: usize = 2 * 1024 * 1024;
/// What the audit line's question field says for a chat request: chat sends no
/// research question, only the request itself.
const CHAT_AUDIT_PURPOSE: &str = "chat";

/// How a host name is turned into addresses. Production asks the system
/// resolver; tests substitute a fixed table.
type Lookup = Arc<
    dyn Fn(
            String,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>,
        > + Send
        + Sync,
>;

/// A resolver that hands the HTTP client only public addresses, so the
/// connection is made to an address that passed the check — there is no second
/// lookup between checking and connecting.
struct PublicOnlyResolver {
    lookup: Lookup,
    allow_loopback: bool,
}

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let lookup = Arc::clone(&self.lookup);
        let allow_loopback = self.allow_loopback;
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addresses = lookup(host).await?;
            let usable: Vec<SocketAddr> = addresses
                .into_iter()
                .filter(|address| address_allowed(address.ip(), allow_loopback))
                .collect();
            if usable.is_empty() {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the host resolves only to addresses that are not on the public internet",
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(usable.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn address_allowed(ip: IpAddr, allow_loopback: bool) -> bool {
    is_public_ip(ip) || (allow_loopback && ip.is_loopback())
}

fn system_lookup() -> Lookup {
    Arc::new(|host: String| {
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .collect::<Vec<_>>())
        })
    })
}

/// The chat web gateway for one session.
pub(crate) struct ChatWeb {
    mode: ChatWebMode,
    research_web_enabled: bool,
    disallowlist: Vec<String>,
    audit_log: PathBuf,
    /// Hosts the user agreed to for this session, normalized.
    grants: Mutex<HashSet<String>>,
    client: reqwest::Client,
    /// Only a test fixture may reach loopback; production never sets it.
    allow_loopback: bool,
    /// Who answers web consent: the person at this host, on a channel that is
    /// not shared with other devices or automation.
    consent: Arc<dyn Approver>,
}

impl ChatWeb {
    /// The gateway for the workspace at `root` under `config`.
    pub(crate) fn from_config(
        root: &Path,
        config: &Config,
        consent: Arc<dyn Approver>,
    ) -> anyhow::Result<Self> {
        Self::build(root, config, consent, system_lookup(), false)
    }

    fn build(
        root: &Path,
        config: &Config,
        consent: Arc<dyn Approver>,
        lookup: Lookup,
        allow_loopback: bool,
    ) -> anyhow::Result<Self> {
        let web = &config.research.web;
        let audit_log = web.audit_log.clone().map_or_else(
            || crate::research::default_audit_log(root),
            |path| root.join(path),
        );
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(WEB_CONNECT_TIMEOUT_SECS))
            // Every hop is decided by the chain, never by the client.
            .redirect(reqwest::redirect::Policy::none())
            // A proxy would resolve the destination itself, around the
            // public-address check.
            .no_proxy()
            .dns_resolver(Arc::new(PublicOnlyResolver {
                lookup,
                allow_loopback,
            }))
            .build()?;
        Ok(Self {
            mode: config.tools.web,
            research_web_enabled: web.enabled,
            // One spelling on both sides of the comparison.
            disallowlist: web
                .disallowlist
                .iter()
                .map(|entry| normalize_host(entry))
                .collect(),
            audit_log,
            grants: Mutex::new(HashSet::new()),
            client,
            allow_loopback,
            consent,
        })
    }

    fn audit(&self, url: &str, host: &str, decision: &str) {
        // A refusal sends nothing, so a failed write here loses a record but
        // never lets a request through.
        let _ = append_audit(
            &self.audit_log,
            &audit_entry(&redact_url(url), host, decision, CHAT_AUDIT_PURPOSE),
        );
    }

    /// Whether `url` may be requested at all, before anyone is asked: http or
    /// https, a public address when it is written as one, not a local name, and
    /// not on the deny list. Returns the normalized host.
    fn check_destination(&self, url: &reqwest::Url) -> Result<String, WebRefusal> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(WebRefusal::Blocked(
                "Only http and https addresses can be fetched.".to_string(),
            ));
        }
        // The disclosure says only the address is sent; credentials in a link
        // would be sent too.
        if !url.username().is_empty() || url.password().is_some() {
            return Err(WebRefusal::Blocked(
                "Addresses with a user name or password in them are not fetched.".to_string(),
            ));
        }
        let host = normalize_host(url.host_str().unwrap_or_default());
        // An address written into the URL never reaches the resolver, so it is
        // checked here; a name is checked again on every lookup.
        let literal = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .ok();
        let internal = match literal {
            Some(ip) => !address_allowed(ip, self.allow_loopback),
            None => host.is_empty() || host == "localhost" || host.ends_with(".localhost"),
        };
        if internal {
            self.audit(url.as_str(), &host, "chat-blocked-internal");
            return Err(WebRefusal::Blocked(format!(
                "{host} is not on the public internet, so chat web access will not reach it."
            )));
        }
        if host_matches(&self.disallowlist, &host) {
            self.audit(url.as_str(), &host, "chat-disallowed");
            return Err(WebRefusal::Blocked(format!(
                "{host} is on the deny list ([research.web] disallowlist)."
            )));
        }
        Ok(host)
    }

    /// Make sure the user agreed to `host` for this session, asking if not.
    async fn ensure_consent(
        &self,
        host: &str,
        url: &reqwest::Url,
        request: &WebFetchRequest,
    ) -> Result<(), WebRefusal> {
        if self.granted(host) {
            return Ok(());
        }
        if request.interactivity == Interactivity::NonInteractive {
            self.audit(url.as_str(), host, "chat-nobody-to-ask");
            return Err(WebRefusal::NobodyToAsk(format!(
                "Fetching from {host} needs your agreement, and nobody can be asked in this \
                 session, so nothing was sent."
            )));
        }
        let question = PermissionRequest {
            tool: request.tool.clone(),
            effect: Effect::Network,
            interactivity: request.interactivity,
            trusted: request.trusted,
            detail: self.consent_detail(host, url, &request.tool),
        };
        if self.consent.approve(&question).await {
            if let Ok(mut grants) = self.grants.lock() {
                grants.insert(host.to_string());
            }
            self.audit(url.as_str(), host, "chat-consent-given");
            Ok(())
        } else {
            self.audit(url.as_str(), host, "chat-consent-declined");
            Err(WebRefusal::Declined(format!(
                "The user did not allow web access to {host}; nothing was sent. Do not \
                 retry the same site unless the user asks."
            )))
        }
    }

    fn granted(&self, host: &str) -> bool {
        self.grants
            .lock()
            .map(|grants| grants.contains(host))
            .unwrap_or(false)
    }

    fn consent_detail(&self, host: &str, url: &reqwest::Url, tool: &str) -> String {
        format!(
            "Allow web access to {host} for the rest of this session? {tool} will request \
             {} — only that address leaves this computer, nothing from your files or \
             conversation. Each request is recorded in {}.",
            redact_url(url.as_str()),
            self.audit_log.display()
        )
    }
}

/// The per-request policy the shared chain consults for chat: no pacing, a
/// download cap, redacted audit lines, unsuccessful bodies returned, and every
/// redirect checked — and asked about, for a new host — like a first request.
struct ChatHopPolicy<'a> {
    web: &'a ChatWeb,
    request: &'a WebFetchRequest,
}

#[async_trait]
impl HopPolicy for ChatHopPolicy<'_> {
    fn cooled(&self, _host: &str) -> bool {
        false
    }
    async fn pace(&self, _host: &str) {}
    fn record(&self, _host: &str, _took: Duration, _cool_down: bool) {}
    async fn allow_redirect(&self, next: &reqwest::Url, _origin_host: &str) -> bool {
        match self.web.check_destination(next) {
            Ok(host) => self
                .web
                .ensure_consent(&host, next, self.request)
                .await
                .is_ok(),
            Err(_) => false,
        }
    }
    fn audit_locator(&self, url: &str) -> String {
        redact_url(url)
    }
    fn body_cap(&self) -> Option<usize> {
        Some(CHAT_MAX_DOWNLOAD_BYTES)
    }
    fn fail_on_cooldown_status(&self) -> bool {
        false
    }
}

#[async_trait]
impl WebGateway for ChatWeb {
    async fn fetch(
        &self,
        request: WebFetchRequest,
        context: WebContext<'_>,
    ) -> Result<WebPage, WebRefusal> {
        if matches!(self.mode, ChatWebMode::Off) || !self.research_web_enabled {
            return Err(WebRefusal::Disabled(
                "Web access from chat is turned off ([tools] web = \"off\", or \
                 [research.web] enabled = false). Set [tools] web = \"ask\" to be asked \
                 per site, or use /research <topic> where web research is enabled."
                    .to_string(),
            ));
        }
        if context.incognito {
            return Err(WebRefusal::Incognito(
                "Web access is not available while the session is incognito: every web \
                 request is recorded in an audit log, and incognito writes nothing."
                    .to_string(),
            ));
        }
        let url = reqwest::Url::parse(&request.url).map_err(|_| {
            WebRefusal::Blocked("That is not a valid http or https address.".to_string())
        })?;
        let host = self.check_destination(&url)?;
        self.ensure_consent(&host, &url, &request).await?;

        let timeout = request
            .timeout
            .unwrap_or(CHAT_DEFAULT_TIMEOUT)
            .min(CHAT_MAX_TIMEOUT);
        let policy = ChatHopPolicy {
            web: self,
            request: &request,
        };
        let chain = fetch_chain(
            &self.client,
            &policy,
            &self.audit_log,
            url.as_str(),
            &host,
            CHAT_AUDIT_PURPOSE,
        );
        let outcome = match tokio::time::timeout(timeout, chain).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => {
                return Err(WebRefusal::Failed(
                    "The egress audit record could not be written, so the request was not \
                     sent."
                        .to_string(),
                ))
            }
            Err(_) => {
                return Err(WebRefusal::Failed(format!(
                    "The request to {host} did not finish within {} seconds.",
                    timeout.as_secs()
                )))
            }
        };
        let page = match outcome {
            ChainOutcome::Page(page) => page,
            ChainOutcome::Redirected(stop) => {
                return Err(WebRefusal::Blocked(format!(
                    "The site redirected and the redirect was not followed ({}).",
                    stop.reason()
                )))
            }
            ChainOutcome::Cooled | ChainOutcome::Failed => {
                return Err(WebRefusal::Failed(format!(
                    "The request to {host} failed (a network error or an unreadable response)."
                )))
            }
        };
        let text = if is_html(&page.content_type, &page.body) {
            html_to_markdown(&page.body)
        } else {
            page.body
        };
        let text = match request.max_bytes {
            Some(limit) => bound_body(&text, limit),
            None => text,
        };
        Ok(WebPage {
            final_url: redact_url(&page.final_url),
            status: page.status.as_u16(),
            content_type: page.content_type,
            body: text,
            truncated: page.truncated,
        })
    }

    fn reset_consent(&self) {
        if let Ok(mut grants) = self.grants.lock() {
            grants.clear();
        }
    }
}

/// One spelling for a host name everywhere it is compared: lower case, with
/// the trailing dot of a fully qualified name removed.
fn normalize_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// Whether `ip` is an address on the public internet: not loopback, private,
/// link-local, shared, documentation, benchmarking, multicast, broadcast,
/// unspecified or reserved, and not an IPv6 form that wraps such an IPv4
/// address.
fn is_public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_public_v4(v4),
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let segments = v6.segments();
            // NAT64 (64:ff9b::/96) carries an IPv4 address in its low bits.
            if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let [a, b] = segments[6].to_be_bytes();
                let [c, d] = segments[7].to_be_bytes();
                return is_public_v4(std::net::Ipv4Addr::new(a, b, c, d));
            }
            // Only global unicast (2000::/3) can be public, less the special-purpose
            // blocks inside it that tunnel to or stand for other addresses:
            // IETF protocol assignments 2001::/23 (Teredo among them),
            // documentation 2001:db8::/32 and 3fff::/20, and 6to4 2002::/16.
            let special = match segments[0] {
                0x2001 => segments[1] < 0x0200 || segments[1] == 0x0db8,
                0x2002 => true,
                first => (first & 0xfff0) == 0x3ff0,
            };
            (segments[0] & 0xe000) == 0x2000 && !special
        }
    }
}

fn is_public_v4(v4: std::net::Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    !(v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
        // "This network" 0.0.0.0/8.
        || a == 0
        // Shared address space 100.64.0.0/10.
        || (a == 100 && (b & 0xc0) == 64)
        // IETF protocol assignments 192.0.0.0/24.
        || (a == 192 && b == 0 && c == 0)
        // Benchmarking 198.18.0.0/15.
        || (a == 198 && (b & 0xfe) == 18)
        // Reserved 240.0.0.0/4.
        || a >= 240)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use localpilot_sandbox::{Approver, PermissionRequest};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Answers consent questions from a script and counts them.
    struct Answers {
        script: Mutex<Vec<bool>>,
        asked: AtomicUsize,
        details: Mutex<Vec<String>>,
    }

    impl Answers {
        fn new(script: &[bool]) -> Self {
            Self {
                script: Mutex::new(script.iter().rev().copied().collect()),
                asked: AtomicUsize::new(0),
                details: Mutex::new(Vec::new()),
            }
        }
        fn asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    impl Approver for Answers {
        fn approve<'a>(
            &'a self,
            request: &'a PermissionRequest,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.details.lock().unwrap().push(request.detail.clone());
            let answer = self.script.lock().unwrap().pop().unwrap_or(false);
            Box::pin(async move { answer })
        }
    }

    fn table(entries: &[(&'static str, [u8; 4])]) -> Lookup {
        let entries: Vec<(&'static str, [u8; 4])> = entries.to_vec();
        Arc::new(move |host: String| {
            let found: Vec<SocketAddr> = entries
                .iter()
                .filter(|(name, _)| *name == host)
                .map(|(_, ip)| SocketAddr::from((*ip, 0)))
                .collect();
            Box::pin(async move { Ok(found) })
        })
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        audit: PathBuf,
        web: ChatWeb,
        answers: Arc<Answers>,
    }

    fn fixture(
        edit: impl FnOnce(&mut Config),
        lookup: Lookup,
        allow_loopback: bool,
        script: &[bool],
    ) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.log");
        let mut config = Config::default();
        config.research.web.audit_log = Some(audit.to_string_lossy().into_owned());
        edit(&mut config);
        let answers = Arc::new(Answers::new(script));
        let web =
            ChatWeb::build(dir.path(), &config, answers.clone(), lookup, allow_loopback).unwrap();
        Fixture {
            _dir: dir,
            audit,
            web,
            answers,
        }
    }

    /// Reaches the loopback test server; production never allows loopback.
    fn local_fixture(script: &[bool]) -> Fixture {
        fixture(|_| {}, table(&[]), true, script)
    }

    fn request(url: &str) -> WebFetchRequest {
        WebFetchRequest {
            url: url.to_string(),
            max_bytes: None,
            timeout: None,
            tool: "fetch".to_string(),
            interactivity: Interactivity::Interactive,
            trusted: true,
        }
    }

    async fn fetch(
        web: &ChatWeb,
        incognito: bool,
        request: WebFetchRequest,
    ) -> Result<WebPage, WebRefusal> {
        web.fetch(
            request,
            WebContext {
                gateway: web,
                incognito,
            },
        )
        .await
    }

    async fn page_server(status: u16, body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        server
    }

    async fn hits(server: &MockServer) -> usize {
        server.received_requests().await.unwrap().len()
    }

    fn audit_text(fixture: &Fixture) -> String {
        std::fs::read_to_string(&fixture.audit).unwrap_or_default()
    }

    #[tokio::test]
    async fn switched_off_sends_nothing_and_asks_nobody() {
        let server = page_server(200, "page").await;
        for edit in [
            (|config: &mut Config| config.tools.web = ChatWebMode::Off) as fn(&mut Config),
            |config: &mut Config| config.research.web.enabled = false,
        ] {
            let f = fixture(edit, table(&[]), true, &[true]);
            let answers = &f.answers;
            let outcome = fetch(&f.web, false, request(&format!("{}/", server.uri()))).await;
            assert!(
                matches!(outcome, Err(WebRefusal::Disabled(_))),
                "{outcome:?}"
            );
            assert_eq!(answers.asked(), 0);
        }
        assert_eq!(hits(&server).await, 0);
    }

    #[tokio::test]
    async fn incognito_refuses_even_a_host_already_agreed_to() {
        let server = page_server(200, "page").await;
        let f = local_fixture(&[true]);
        let url = format!("{}/", server.uri());
        assert!(fetch(&f.web, false, request(&url)).await.is_ok());
        let outcome = fetch(&f.web, true, request(&url)).await;
        assert!(
            matches!(outcome, Err(WebRefusal::Incognito(_))),
            "{outcome:?}"
        );
        assert_eq!(hits(&server).await, 1, "only the request before incognito");
    }

    #[tokio::test]
    async fn with_nobody_to_ask_nothing_is_sent() {
        let server = page_server(200, "page").await;
        let f = local_fixture(&[true]);
        let answers = &f.answers;
        let mut piped = request(&format!("{}/", server.uri()));
        piped.interactivity = Interactivity::NonInteractive;
        let outcome = fetch(&f.web, false, piped).await;
        assert!(
            matches!(outcome, Err(WebRefusal::NobodyToAsk(_))),
            "{outcome:?}"
        );
        assert_eq!(answers.asked(), 0);
        assert_eq!(hits(&server).await, 0);
    }

    #[tokio::test]
    async fn a_declined_host_is_never_contacted_and_the_refusal_is_audited() {
        let server = page_server(200, "page").await;
        let f = local_fixture(&[false]);
        let outcome = fetch(&f.web, false, request(&format!("{}/", server.uri()))).await;
        assert!(
            matches!(outcome, Err(WebRefusal::Declined(_))),
            "{outcome:?}"
        );
        assert_eq!(hits(&server).await, 0);
        assert!(audit_text(&f).contains("chat-consent-declined"));
    }

    #[tokio::test]
    async fn consent_lasts_for_the_host_until_it_is_reset() {
        let server = page_server(200, "hello").await;
        let f = local_fixture(&[true, true]);
        let answers = &f.answers;
        let url = format!("{}/a", server.uri());
        let page = fetch(&f.web, false, request(&url)).await.unwrap();
        assert_eq!(page.status, 200);
        assert_eq!(page.body, "hello");
        fetch(&f.web, false, request(&url)).await.unwrap();
        assert_eq!(answers.asked(), 1, "one question per host per session");
        f.web.reset_consent();
        fetch(&f.web, false, request(&url)).await.unwrap();
        assert_eq!(answers.asked(), 2, "a new session asks again");
        let detail = answers.details.lock().unwrap()[0].clone();
        assert!(detail.contains("127.0.0.1"), "{detail}");
        assert!(detail.contains("audit.log"), "{detail}");
        let audit = audit_text(&f);
        assert!(audit.contains("chat-consent-given"), "{audit}");
        assert!(audit.contains("allowed"), "{audit}");
    }

    #[tokio::test]
    async fn an_unsuccessful_status_returns_its_body() {
        for status in [404_u16, 429, 503] {
            let server = page_server(status, "not here").await;
            let f = local_fixture(&[true]);
            let page = fetch(&f.web, false, request(&format!("{}/", server.uri())))
                .await
                .unwrap();
            assert_eq!(page.status, status);
            assert_eq!(page.body, "not here");
        }
    }

    #[tokio::test]
    async fn an_address_off_the_public_internet_is_refused_before_anyone_is_asked() {
        let f = fixture(|_| {}, table(&[]), false, &[true]);
        let answers = &f.answers;
        for url in [
            "http://127.0.0.1:9/",
            "http://10.1.2.3/",
            "http://[::1]/",
            "http://[::ffff:192.168.0.1]/",
            "http://169.254.169.254/latest",
            "http://localhost/",
            "http://LOCALHOST./",
        ] {
            let outcome = fetch(&f.web, false, request(url)).await;
            assert!(
                matches!(outcome, Err(WebRefusal::Blocked(_))),
                "{url}: {outcome:?}"
            );
            assert_eq!(answers.asked(), 0, "{url}");
        }
    }

    #[tokio::test]
    async fn a_name_that_resolves_to_a_private_address_is_never_connected() {
        let server = page_server(200, "page").await;
        let port = server.address().port();
        let f = fixture(
            |_| {},
            table(&[("intranet.test", [127, 0, 0, 1])]),
            false,
            &[true],
        );
        let outcome = fetch(
            &f.web,
            false,
            request(&format!("http://intranet.test:{port}/")),
        )
        .await;
        assert!(matches!(outcome, Err(WebRefusal::Failed(_))), "{outcome:?}");
        assert_eq!(hits(&server).await, 0);
    }

    #[tokio::test]
    async fn a_deny_listed_host_is_refused_in_any_spelling() {
        let f = fixture(
            |config| config.research.web.disallowlist = vec!["Blocked.Test.".to_string()],
            table(&[]),
            false,
            &[true],
        );
        let answers = &f.answers;
        for url in ["http://blocked.test/", "http://BLOCKED.test./x"] {
            let outcome = fetch(&f.web, false, request(url)).await;
            assert!(
                matches!(outcome, Err(WebRefusal::Blocked(_))),
                "{url}: {outcome:?}"
            );
            assert_eq!(answers.asked(), 0);
        }
        assert!(audit_text(&f).contains("chat-disallowed"));
    }

    #[tokio::test]
    async fn a_redirect_to_a_private_address_is_not_followed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "http://10.0.0.1/admin"),
            )
            .mount(&server)
            .await;
        let f = local_fixture(&[true, true]);
        let answers = &f.answers;
        let outcome = fetch(&f.web, false, request(&format!("{}/", server.uri()))).await;
        assert!(
            matches!(outcome, Err(WebRefusal::Blocked(_))),
            "{outcome:?}"
        );
        assert_eq!(answers.asked(), 1, "the blocked hop is not asked about");
        assert!(audit_text(&f).contains("chat-blocked-internal"));
    }

    #[tokio::test]
    async fn a_redirect_to_a_new_host_needs_its_own_consent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "http://elsewhere.test/"),
            )
            .mount(&server)
            .await;
        let f = local_fixture(&[true, false]);
        let answers = &f.answers;
        let outcome = fetch(&f.web, false, request(&format!("{}/", server.uri()))).await;
        assert!(
            matches!(outcome, Err(WebRefusal::Blocked(_))),
            "{outcome:?}"
        );
        assert_eq!(answers.asked(), 2);
        assert!(answers.details.lock().unwrap()[1].contains("elsewhere.test"));
    }

    #[tokio::test]
    async fn an_unwritable_audit_log_stops_the_request() {
        let server = page_server(200, "page").await;
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        // A directory where the log file should be: every append fails.
        config.research.web.audit_log = Some(dir.path().to_string_lossy().into_owned());
        let web = ChatWeb::build(
            dir.path(),
            &config,
            Arc::new(Answers::new(&[true])),
            table(&[]),
            true,
        )
        .unwrap();
        let outcome = fetch(&web, false, request(&format!("{}/", server.uri()))).await;
        assert!(matches!(outcome, Err(WebRefusal::Failed(_))), "{outcome:?}");
        assert_eq!(hits(&server).await, 0);
    }

    #[tokio::test]
    async fn the_download_stops_at_the_size_bound() {
        let big = "x".repeat(CHAT_MAX_DOWNLOAD_BYTES + 4096);
        let server = page_server(200, &big).await;
        let f = local_fixture(&[true]);
        let page = fetch(&f.web, false, request(&format!("{}/", server.uri())))
            .await
            .unwrap();
        assert!(page.truncated);
        assert_eq!(page.body.len(), CHAT_MAX_DOWNLOAD_BYTES);
    }

    #[tokio::test]
    async fn a_body_exactly_at_the_bound_arrives_whole() {
        let exact = "y".repeat(CHAT_MAX_DOWNLOAD_BYTES);
        let server = page_server(200, &exact).await;
        let f = local_fixture(&[true]);
        let page = fetch(&f.web, false, request(&format!("{}/", server.uri())))
            .await
            .unwrap();
        assert_eq!(page.body.len(), CHAT_MAX_DOWNLOAD_BYTES);
    }

    #[tokio::test]
    async fn an_address_carrying_credentials_is_refused() {
        let f = local_fixture(&[true]);
        let answers = &f.answers;
        let outcome = fetch(&f.web, false, request("http://user:pw@example.test/")).await;
        assert!(
            matches!(outcome, Err(WebRefusal::Blocked(_))),
            "{outcome:?}"
        );
        assert_eq!(answers.asked(), 0);
    }

    #[tokio::test]
    async fn a_slow_server_is_cut_off_at_the_requested_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("late")
                    .set_delay(Duration::from_secs(5)),
            )
            .mount(&server)
            .await;
        let f = local_fixture(&[true]);
        let mut slow = request(&format!("{}/", server.uri()));
        slow.timeout = Some(Duration::from_secs(1));
        let started = std::time::Instant::now();
        let outcome = fetch(&f.web, false, slow).await;
        assert!(matches!(outcome, Err(WebRefusal::Failed(_))), "{outcome:?}");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn secrets_in_the_url_never_reach_the_audit_log_or_the_question() {
        let server = page_server(200, "page").await;
        let f = local_fixture(&[true]);
        let answers = &f.answers;
        let url = format!("{}/p?token=s3cr3t-value#frag", server.uri());
        let page = fetch(&f.web, false, request(&url)).await.unwrap();
        assert!(!page.final_url.contains("s3cr3t"), "{}", page.final_url);
        assert!(!audit_text(&f).contains("s3cr3t"));
        assert!(!answers.details.lock().unwrap()[0].contains("s3cr3t"));
    }

    #[tokio::test]
    async fn a_bypass_profile_cannot_answer_the_consent_question() {
        use localpilot_sandbox::{PermissionEngine, Profile, Workspace};
        let f = local_fixture(&[false]);
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        // The session's shared approver: bypass never consults it.
        let shared = Answers::new(&[]);
        let context = localpilot_tools::ToolContext {
            workspace: &workspace,
            interactivity: Interactivity::Interactive,
            trusted: true,
            retention: None,
            processes: None,
            agents: None,
            prompter: None,
            peers: None,
            web: Some(WebContext {
                gateway: &f.web,
                incognito: false,
            }),
        };
        let call = localpilot_core::ToolCall::new(
            localpilot_core::ToolUseId::from("c1"),
            "fetch",
            serde_json::json!({ "url": "http://elsewhere.test/" }),
        );
        let registry = localpilot_tools::ToolRegistry::with_builtins();
        let result = registry
            .dispatch(
                &call,
                &context,
                &PermissionEngine::new(Profile::Bypass, Vec::new()),
                &shared,
            )
            .await;
        assert!(result.is_error(), "{}", result.output);
        assert_eq!(shared.asked(), 0);
        assert_eq!(
            f.answers.asked(),
            1,
            "the consent question is still asked, on the host's own channel"
        );
    }

    #[test]
    fn public_addresses_are_told_apart_from_internal_ones() {
        for public in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public_ip(public.parse().unwrap()), "{public}");
        }
        for internal in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "192.0.0.1",
            "192.0.2.1",
            "192.168.1.1",
            "198.18.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
        ] {
            assert!(!is_public_ip(internal.parse().unwrap()), "{internal}");
        }
    }

    #[test]
    fn redaction_keeps_the_shape_of_a_url_but_not_its_secrets() {
        assert_eq!(
            redact_url("https://user:pw@example.com/a/b?key=abc&q=1#top"),
            "https://example.com/a/b?key=redacted&q=redacted"
        );
        assert_eq!(
            redact_url("https://example.com/a?s3cr3t-opaque&x=1"),
            "https://example.com/a?redacted&x=redacted"
        );
        assert_eq!(
            redact_url("https://example.com/plain"),
            "https://example.com/plain"
        );
    }
}
