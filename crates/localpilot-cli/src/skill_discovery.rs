//! The skill-discovery web lane (LocalHub#41): find *new* public HTTPS GitHub
//! skill repositories for a query and validate them read-only, all within the
//! existing `/research` egress controls.
//!
//! Candidate repository URLs come from a [`RepoSearch`] provider — a configured
//! research search provider when one is designated, otherwise the official public
//! GitHub repository-search API as the fresh-install fallback. Every outbound call
//! is gated by the shared [`WebAccess`] policy (the config switch, per-session
//! opt-in, and the allow/disallow lists), audited, and bounded; `--no-web` or a
//! disabled config makes zero requests. A rate limit or outage yields an honest
//! partial result and never discards what was already found. A validated
//! repository is fed to the read-only [`validate_repo`] check; nothing is
//! registered or installed here.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use localpilot_config::{CliOverrides, ConfigPaths};
use serde_json::Value;

use localpilot_research::{AuditEntry, FetchDecision, WebAccess};
use localpilot_skills::{
    rank, source_id, validate_repo, DiscoveredSkill, GitFetcher, MatchState, ProposalState,
    ProposalStore, Ranked, ReadScope, RepoFetcher, SkillProposal, SkillsManager, ValidatedRepo,
};

use crate::skills_cmd::SkillsOutcome;
use crate::trust;

/// The official public GitHub search host, gated before any search request.
const GITHUB_SEARCH_HOST: &str = "api.github.com";

/// A minimal async HTTP GET seam, so the GitHub search is exercised without live
/// network in tests. Production is [`ReqwestHttp`]; tests inject canned responses.
#[async_trait]
pub trait HttpGet: Send + Sync {
    /// GET `url` with the given headers, returning the status and body text, or a
    /// transport-level error string (never a panic).
    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, String>;
}

/// A minimal HTTP response: status code and body text.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// The production HTTP GET seam over `reqwest`.
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl ReqwestHttp {
    /// Construct with a shared client.
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl HttpGet for ReqwestHttp {
    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, String> {
        let mut request = self.client.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(|e| e.to_string())?;
        Ok(HttpResponse { status, body })
    }
}

/// The outcome of one candidate-URL search: the URLs found, whether the result is
/// partial (rate-limited / timed out / incomplete), and a human note if so.
#[derive(Debug, Clone, Default)]
pub struct RepoSearchResult {
    pub urls: Vec<String>,
    pub partial: bool,
    pub note: Option<String>,
}

/// A source of candidate skill-repository URLs for a query. The default is the
/// official public GitHub repository-search API; a configured research search
/// provider can supersede it.
#[async_trait]
pub trait RepoSearch: Send + Sync {
    /// The single host this provider contacts, gated by [`WebAccess`] before the
    /// provider is ever called.
    fn host(&self) -> &str;
    /// A short label for the egress disclosure and audit log.
    fn label(&self) -> &str;
    /// Return up to `limit` candidate repository URLs for `query`. Best-effort: a
    /// rate limit or outage returns a partial result, never an error.
    async fn search(&self, query: &str, limit: usize) -> RepoSearchResult;
}

/// The official public GitHub repository-search API (`GET /search/repositories`),
/// the fresh-install fallback. Repository search needs no authentication.
pub struct GitHubRepoSearch {
    http: Arc<dyn HttpGet>,
}

impl GitHubRepoSearch {
    /// Construct over an HTTP seam.
    #[must_use]
    pub fn new(http: Arc<dyn HttpGet>) -> Self {
        Self { http }
    }
}

#[async_trait]
impl RepoSearch for GitHubRepoSearch {
    fn host(&self) -> &str {
        GITHUB_SEARCH_HOST
    }

    fn label(&self) -> &str {
        "github-search"
    }

    async fn search(&self, query: &str, limit: usize) -> RepoSearchResult {
        let url = github_search_url(query, limit);
        // GitHub requires a User-Agent; the JSON media type pins the response shape.
        let headers = [
            ("User-Agent", "localpilot"),
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
        ];
        match self.http.get(&url, &headers).await {
            Ok(response) => parse_github_search(response.status, &response.body),
            Err(error) => RepoSearchResult {
                urls: Vec::new(),
                partial: true,
                note: Some(format!("github search request failed: {error}")),
            },
        }
    }
}

/// Build the GitHub repository-search URL for `query`, bounding `per_page` to the
/// API's 1..=100 page size. The query is percent-encoded so spaces and reserved
/// characters travel safely.
#[must_use]
pub fn github_search_url(query: &str, limit: usize) -> String {
    let per_page = limit.clamp(1, 100);
    format!(
        "https://{GITHUB_SEARCH_HOST}/search/repositories?q={}&per_page={per_page}",
        percent_encode(query)
    )
}

/// Parse a GitHub repository-search response into candidate URLs. A 403/429 is
/// treated as a rate limit (partial), a non-2xx as a failed search (partial), and
/// `incomplete_results: true` marks a partial-but-usable result. `clone_url`/
/// `html_url` are read per item, HTTPS only.
#[must_use]
pub fn parse_github_search(status: u16, body: &str) -> RepoSearchResult {
    if status == 403 || status == 429 {
        return RepoSearchResult {
            urls: Vec::new(),
            partial: true,
            note: Some(format!("github search rate-limited (HTTP {status})")),
        };
    }
    if !(200..300).contains(&status) {
        return RepoSearchResult {
            urls: Vec::new(),
            partial: true,
            note: Some(format!("github search returned HTTP {status}")),
        };
    }
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return RepoSearchResult {
            urls: Vec::new(),
            partial: true,
            note: Some("github search returned unparseable JSON".to_string()),
        };
    };
    let mut urls = Vec::new();
    if let Some(items) = value["items"].as_array() {
        for item in items {
            let candidate = item["clone_url"]
                .as_str()
                .or_else(|| item["html_url"].as_str());
            if let Some(url) = candidate {
                if url.starts_with("https://") && !urls.iter().any(|u| u == url) {
                    urls.push(url.to_string());
                }
            }
        }
    }
    let incomplete = value["incomplete_results"].as_bool().unwrap_or(false);
    RepoSearchResult {
        urls,
        partial: incomplete,
        note: incomplete.then(|| "github reported incomplete results".to_string()),
    }
}

/// Percent-encode a query for a URL query component: keep unreserved characters,
/// escape everything else (spaces become `%20`, not `+`, which the API also
/// accepts and is unambiguous).
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Parameters for the web-discovery lane.
pub struct WebDiscoveryConfig {
    /// The shared egress gate; the caller has already applied per-session opt-in.
    pub access: WebAccess,
    /// The egress audit log path.
    pub audit_log: PathBuf,
    /// The staging root under which each candidate is fetched and then removed.
    pub staging_root: PathBuf,
    /// The maximum candidate repositories to search and validate.
    pub limit: usize,
}

/// The outcome of a web-discovery run.
#[derive(Debug, Clone, Default)]
pub struct WebDiscovery {
    /// The validated candidate repositories, ready for ranking and proposal.
    pub repos: Vec<ValidatedRepo>,
    /// True if the search or any validation was partial (rate-limited/failed).
    pub partial: bool,
    /// Human notes about skipped hosts, partial results, or rejected repositories.
    pub notes: Vec<String>,
    /// The number of outbound requests actually made (0 under `--no-web`).
    pub requests_made: usize,
}

/// Discover new public repositories for `query`: gate and run the search, then
/// validate each candidate read-only through [`validate_repo`]. A host that is not
/// allowlisted is skipped and audited; a repository without a valid catalog is
/// noted and dropped, never saved. Local matches (the caller's) are untouched — a
/// rate limit or outage only sets `partial` and adds a note.
pub async fn discover_web_repositories(
    query: &str,
    search: &dyn RepoSearch,
    fetcher: &dyn RepoFetcher,
    cfg: &WebDiscoveryConfig,
) -> WebDiscovery {
    let mut result = WebDiscovery::default();
    // --no-web / config-off / no opt-in: make zero outbound requests.
    if !cfg.access.is_active() {
        result
            .notes
            .push("web discovery disabled — no requests made".to_string());
        return result;
    }

    // Gate the search host before contacting the provider.
    let search_host = search.host();
    match cfg.access.decide_host(search_host) {
        FetchDecision::Allowed => {}
        FetchDecision::NeedsConfirmation => {
            audit(&cfg.audit_log, search_host, search_host, "skipped", query);
            result.notes.push(format!(
                "search host `{search_host}` is not allowlisted — skipped"
            ));
            return result;
        }
        FetchDecision::Disabled => {
            result
                .notes
                .push("web discovery disabled — no requests made".to_string());
            return result;
        }
    }

    let search_result = search.search(query, cfg.limit).await;
    result.requests_made += 1;
    audit(&cfg.audit_log, search.label(), search_host, "search", query);
    if search_result.partial {
        result.partial = true;
        if let Some(note) = search_result.note {
            result.notes.push(note);
        }
    }

    // Validate each candidate, host-gated, up to the limit.
    for url in search_result.urls.into_iter().take(cfg.limit) {
        let host = host_of(&url);
        match cfg.access.decide_host(&host) {
            FetchDecision::Allowed => {}
            _ => {
                audit(&cfg.audit_log, &url, &host, "skipped", query);
                result.notes.push(format!(
                    "candidate host `{host}` is not allowlisted — skipped"
                ));
                continue;
            }
        }
        let staging = cfg.staging_root.join(source_id(&url));
        result.requests_made += 1;
        match validate_repo(fetcher, &url, &staging) {
            Ok(repo) => {
                audit(&cfg.audit_log, &url, &host, "validated", query);
                result.repos.push(repo);
            }
            Err(error) => {
                audit(&cfg.audit_log, &url, &host, "rejected", query);
                result.notes.push(format!(
                    "candidate `{url}` is not a valid skill repository: {error}"
                ));
            }
        }
    }
    result
}

/// Extract the host from a normalized-looking `https://host/path` URL. Returns an
/// empty string for anything that is not HTTPS, which the allowlist then rejects.
fn host_of(url: &str) -> String {
    let after = match url.strip_prefix("https://") {
        Some(rest) => rest,
        None => return String::new(),
    };
    after
        .split(['/', ':'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Append one audit line for an outbound (or skipped) request. Reuses the shared
/// [`AuditEntry`] line format; a write failure is swallowed so auditing never
/// aborts discovery (the request itself is the auditable event).
fn audit(path: &Path, url: &str, host: &str, decision: &str, question: &str) {
    let entry = AuditEntry {
        url: url.to_string(),
        host: host.to_string(),
        decision: decision.to_string(),
        question: question.to_string(),
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write as _;
        let _ = writeln!(file, "{}", entry.to_line());
    }
}

// --- the shared `skills research` service -----------------------------------

/// Run skill discovery for `query`: classify local matches (installed/available),
/// optionally search the web for new repositories, rank everything, save
/// de-duplicated review proposals for any newly discovered repositories, and print
/// a `Relevant skills` report. Read-only — it never registers a source or installs
/// a skill. Shared by `localpilot skills research` and the `/skills research`
/// slash surface so both behave identically (LocalHub#41).
///
/// `web` is the per-run web toggle (`--no-web` sets it false); web discovery also
/// requires `[research.web].enabled`. Without `-g`, `global` is false: the
/// effective project+global catalog is searched and proposals default to project
/// scope. With `-g`, local search is global-only and proposals default to global.
///
/// # Errors
/// Returns an error only if config loading, the proposal store, or output fails; a
/// missing query or a global run without a home is reported as a run failure.
pub async fn run_skill_research(
    root: &Path,
    query: &str,
    global: bool,
    web: bool,
    out: &mut dyn Write,
) -> anyhow::Result<SkillsOutcome> {
    let config = localpilot_config::load(&ConfigPaths::standard(root), &CliOverrides::default())?;
    let audit_log = config.research.web.audit_log.clone().map_or_else(
        || {
            root.join(".localpilot")
                .join("research")
                .join("egress-audit.log")
        },
        |path| root.join(path),
    );
    // Production seams: a real GitHub search over reqwest and a real git fetcher.
    let search = GitHubRepoSearch::new(Arc::new(ReqwestHttp::new(reqwest::Client::new())));
    let fetcher = GitFetcher;
    let deps = DiscoveryDeps {
        search: &search,
        fetcher: &fetcher,
        home: localpilot_skills::user_home(),
        web_enabled: config.research.web.enabled,
        allowlist: config.research.web.allowlist.clone(),
        disallowlist: config.research.web.disallowlist.clone(),
        audit_log,
        autonomous_discovery: config.skills.autonomous_discovery,
    };
    run_discovery(root, query, global, web, &deps, out).await
}

/// Injected dependencies for the discovery service, so the whole flow — local
/// classification, egress-gated web discovery, proposal persistence, ranking, and
/// the autonomous-load gate — is testable with fakes and a hermetic home.
struct DiscoveryDeps<'a> {
    search: &'a dyn RepoSearch,
    fetcher: &'a dyn RepoFetcher,
    /// The user-global home, injected so a test never reads the real one.
    home: Option<std::path::PathBuf>,
    web_enabled: bool,
    allowlist: Vec<String>,
    disallowlist: Vec<String>,
    audit_log: std::path::PathBuf,
    autonomous_discovery: bool,
}

/// The seam-injectable core of [`run_skill_research`].
async fn run_discovery(
    root: &Path,
    query: &str,
    global: bool,
    web: bool,
    deps: &DiscoveryDeps<'_>,
    out: &mut dyn Write,
) -> anyhow::Result<SkillsOutcome> {
    let query = query.trim();
    if query.is_empty() {
        writeln!(out, "error: a query is required (skills research <query>)")?;
        return Ok(SkillsOutcome { had_failure: true });
    }
    let home = deps.home.as_deref();
    let trusted = trust::is_trusted(root);
    let now = unix_now_string();
    let manager = SkillsManager::new(root, home, trusted, deps.fetcher, &now);
    let read = if global {
        ReadScope::GlobalOnly
    } else {
        ReadScope::Effective
    };

    // Local matches first — always available, even with the web off.
    let mut candidates = manager.local_discovery(read)?;

    // Web discovery: bounded, egress-gated, best-effort.
    let web_on = web && deps.web_enabled;
    let mut discovered_repos: Vec<ValidatedRepo> = Vec::new();
    if web_on {
        writeln!(
            out,
            "skill discovery (egress disclosure): web is on — only the query text is sent \
             to {GITHUB_SEARCH_HOST}; --no-web or [research.web].enabled = false disables it. \
             audit: {}",
            deps.audit_log.display()
        )?;
        let mut access = WebAccess::new(
            deps.web_enabled,
            deps.allowlist.clone(),
            deps.disallowlist.clone(),
        );
        access.grant_session();
        let cfg = WebDiscoveryConfig {
            access,
            audit_log: deps.audit_log.clone(),
            staging_root: root.join(".localpilot").join("skill-discovery"),
            limit: 10,
        };
        let found = discover_web_repositories(query, deps.search, deps.fetcher, &cfg).await;
        for note in &found.notes {
            writeln!(out, "note: {note}")?;
        }
        if found.partial {
            writeln!(
                out,
                "note: web discovery returned partial results; local matches are unaffected"
            )?;
        }
        for repo in &found.repos {
            for package in &repo.packages {
                candidates.push(DiscoveredSkill::discovered(package, repo));
            }
        }
        discovered_repos = found.repos;
    } else if web {
        writeln!(
            out,
            "note: web discovery is off ([research.web].enabled = false); local catalog only"
        )?;
    } else {
        writeln!(out, "note: web discovery skipped for this run (--no-web)")?;
    }

    // Persist a review proposal per newly discovered repository (dedup by
    // repo/skill/scope). Never mutates a source or install.
    let scope_label = if global { "global" } else { "project" };
    if let Some(store_path) = proposals_path(root, home, global) {
        let mut store = ProposalStore::load(&store_path)?;
        for repo in &discovered_repos {
            store.upsert(build_proposal(repo, query, scope_label, &now));
        }
        store.save()?;
        if !discovered_repos.is_empty() {
            writeln!(
                out,
                "saved {} review proposal(s) to {} — review and act in LocalMind",
                discovered_repos.len(),
                store_path.display()
            )?;
        }
    } else if !discovered_repos.is_empty() {
        writeln!(
            out,
            "note: no home directory resolves; global proposals cannot be saved"
        )?;
    }

    let ranked = rank(query, candidates, None);
    render_discovery(query, &ranked, out)?;
    report_autoload(&ranked, deps.autonomous_discovery, out)?;
    Ok(SkillsOutcome { had_failure: false })
}

/// Whether a relevant match may be auto-loaded into a research run: only an
/// installed, model-discoverable skill, and only when `[skills].autonomous_discovery`
/// is enabled. An available/discovered match, a user-only skill, or a project with
/// autonomous discovery off all stay report-only (LocalHub#41).
#[must_use]
pub fn should_autoload(skill: &DiscoveredSkill, autonomous_discovery: bool) -> bool {
    autonomous_discovery && skill.state == MatchState::Installed && skill.discoverable
}

/// Report which installed, discoverable skills are loaded into this run under
/// `[skills].autonomous_discovery`; note the report-only posture when the toggle is
/// off but a discoverable installed skill would otherwise have loaded.
fn report_autoload(
    ranked: &Ranked,
    autonomous_discovery: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let relevant = || ranked.matches.iter().filter(|m| m.score > 0.0);
    let loaded: Vec<&str> = relevant()
        .filter(|m| should_autoload(&m.skill, autonomous_discovery))
        .map(|m| m.skill.name.as_str())
        .collect();
    if !loaded.is_empty() {
        writeln!(
            out,
            "loaded into this run ([skills].autonomous_discovery on): {}",
            loaded.join(", ")
        )?;
    } else if !autonomous_discovery
        && relevant().any(|m| m.skill.state == MatchState::Installed && m.skill.discoverable)
    {
        writeln!(
            out,
            "report-only: enable [skills].autonomous_discovery to load a discoverable \
             installed skill into a research run"
        )?;
    }
    Ok(())
}

/// Build a pending review proposal from a validated discovered repository, ranking
/// its own packages to pick the primary recommendation.
fn build_proposal(repo: &ValidatedRepo, query: &str, scope: &str, now: &str) -> SkillProposal {
    let package_candidates: Vec<DiscoveredSkill> = repo
        .packages
        .iter()
        .map(|package| DiscoveredSkill::discovered(package, repo))
        .collect();
    let ranked = rank(query, package_candidates, None);
    let recommendation = ranked.recommendation;
    SkillProposal {
        repo_url: repo.url.clone(),
        commit: repo.commit.clone(),
        catalog_root: repo.catalog_root.clone(),
        available_skills: repo.packages.iter().map(|p| p.name.clone()).collect(),
        recommended_skill: recommendation.as_ref().map(|r| r.name.clone()),
        confidence: recommendation.as_ref().map_or(0.0, |r| r.confidence),
        reason: recommendation.map(|r| r.reason).unwrap_or_default(),
        query: query.to_string(),
        scope: scope.to_string(),
        state: ProposalState::Pending,
        provenance: "github-search".to_string(),
        first_seen: now.to_string(),
        last_seen: now.to_string(),
    }
}

/// The proposal store path for the intended scope: the project `.localpilot` for a
/// project run, the user-global `.localpilot` for `-g`. `None` when a global run
/// has no resolvable home.
fn proposals_path(root: &Path, home: Option<&Path>, global: bool) -> Option<PathBuf> {
    let base = if global {
        home?.join(".localpilot")
    } else {
        root.join(".localpilot")
    };
    Some(base.join("skill-proposals.toml"))
}

/// Print the `Relevant skills` report: the relevant matches grouped by state and
/// the primary recommendation. Only matches that scored above zero are shown, so
/// an irrelevant catalog entry never clutters the report.
fn render_discovery(query: &str, ranked: &Ranked, out: &mut dyn Write) -> anyhow::Result<()> {
    writeln!(out, "\nRelevant skills for \"{query}\":")?;
    let relevant: Vec<_> = ranked.matches.iter().filter(|m| m.score > 0.0).collect();
    if relevant.is_empty() {
        writeln!(out, "  (no relevant skills found)")?;
        return Ok(());
    }
    for m in relevant.iter().take(10) {
        let origin = match m.skill.state {
            MatchState::Installed => String::new(),
            MatchState::Available => m
                .skill
                .repo_url
                .as_deref()
                .map(|u| format!(" ({u})"))
                .unwrap_or_default(),
            MatchState::Discovered => match (&m.skill.repo_url, &m.skill.commit) {
                (Some(url), Some(commit)) => format!(" ({url} @ {})", short_commit(commit)),
                (Some(url), None) => format!(" ({url})"),
                _ => String::new(),
            },
        };
        writeln!(
            out,
            "- [{}] {}{}: {}",
            m.skill.state.label(),
            m.skill.name,
            origin,
            m.skill.description
        )?;
    }
    if let Some(rec) = &ranked.recommendation {
        writeln!(
            out,
            "Recommended: {} (confidence {:.2}) — {}",
            rec.name, rec.confidence, rec.reason
        )?;
    }
    Ok(())
}

/// Shorten a commit for display.
fn short_commit(commit: &str) -> String {
    commit.chars().take(10).collect()
}

/// The current Unix time in seconds as a string, the injected discovery clock.
fn unix_now_string() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use localpilot_skills::{CatalogPackage, SkillError, Snapshot};

    #[test]
    fn search_url_encodes_the_query_and_bounds_the_page() {
        let url = github_search_url("threejs procedural materials", 5);
        assert!(url.starts_with("https://api.github.com/search/repositories?q="));
        assert!(url.contains("threejs%20procedural%20materials"));
        assert!(url.ends_with("per_page=5"));
        // Page size is clamped to the API's 1..=100.
        assert!(github_search_url("x", 0).ends_with("per_page=1"));
        assert!(github_search_url("x", 999).ends_with("per_page=100"));
    }

    #[test]
    fn parse_reads_clone_urls_and_flags_rate_limit_and_incomplete() {
        let body = r#"{"incomplete_results":false,"items":[
            {"full_name":"freshtechbro/claudedesignskills","clone_url":"https://github.com/freshtechbro/claudedesignskills.git","html_url":"https://github.com/freshtechbro/claudedesignskills"},
            {"full_name":"o/other","html_url":"https://github.com/o/other"}
        ]}"#;
        let parsed = parse_github_search(200, body);
        assert_eq!(parsed.urls.len(), 2);
        assert_eq!(
            parsed.urls[0],
            "https://github.com/freshtechbro/claudedesignskills.git"
        );
        assert_eq!(parsed.urls[1], "https://github.com/o/other");
        assert!(!parsed.partial);

        // A 403 is a rate limit → partial, no URLs.
        let limited = parse_github_search(403, "");
        assert!(limited.partial && limited.urls.is_empty());
        // incomplete_results marks a partial-but-usable result.
        let incomplete = parse_github_search(200, r#"{"incomplete_results":true,"items":[]}"#);
        assert!(incomplete.partial);
    }

    /// A search provider returning a fixed URL list, standing in for a configured
    /// research provider (the "provider present" case).
    struct FakeProvider {
        host: String,
        urls: Vec<String>,
    }
    #[async_trait]
    impl RepoSearch for FakeProvider {
        fn host(&self) -> &str {
            &self.host
        }
        fn label(&self) -> &str {
            "fake-provider"
        }
        async fn search(&self, _query: &str, _limit: usize) -> RepoSearchResult {
            RepoSearchResult {
                urls: self.urls.clone(),
                partial: false,
                note: None,
            }
        }
    }

    /// An HTTP seam returning a canned response, for the GitHub fallback path.
    struct FakeHttp {
        status: u16,
        body: String,
    }
    #[async_trait]
    impl HttpGet for FakeHttp {
        async fn get(&self, _url: &str, _headers: &[(&str, &str)]) -> Result<HttpResponse, String> {
            Ok(HttpResponse {
                status: self.status,
                body: self.body.clone(),
            })
        }
    }

    /// A fetcher laying down a `.localpilot/skills` catalog for validated repos.
    /// [`RepoFetcher`] is a synchronous seam (git clone), so this is a plain impl.
    struct FakeFetcher {
        names: Vec<String>,
        commit: String,
    }
    impl RepoFetcher for FakeFetcher {
        fn fetch(&self, _url: &str, dest: &Path) -> Result<Snapshot, SkillError> {
            for name in &self.names {
                let dir = dest.join(".localpilot").join("skills").join(name);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("SKILL.md"),
                    format!("---\nname: {name}\ndescription: does {name}\n---\nBody.\n"),
                )
                .unwrap();
            }
            Ok(Snapshot {
                commit: self.commit.clone(),
            })
        }
    }

    fn active_access() -> WebAccess {
        let mut access = WebAccess::new(true, vec!["github.com".to_string()], Vec::new());
        access.grant_session();
        access
    }

    fn cfg(root: &Path) -> WebDiscoveryConfig {
        WebDiscoveryConfig {
            access: active_access(),
            audit_log: root.join("audit.log"),
            staging_root: root.join("staging"),
            limit: 10,
        }
    }

    #[tokio::test]
    async fn a_configured_provider_is_used_and_its_repos_validate() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = FakeProvider {
            host: "github.com".to_string(),
            urls: vec!["https://github.com/owner/repo".to_string()],
        };
        let fetcher = FakeFetcher {
            names: vec!["threejs-webgl".to_string()],
            commit: "c0ffee".to_string(),
        };
        let found =
            discover_web_repositories("threejs", &provider, &fetcher, &cfg(tmp.path())).await;
        assert_eq!(found.repos.len(), 1);
        assert_eq!(found.repos[0].url, "https://github.com/owner/repo");
        assert!(found.repos[0]
            .packages
            .iter()
            .any(|p| p.name == "threejs-webgl"));
        assert!(!found.partial);
        assert!(found.requests_made >= 2, "one search + one validation");
    }

    #[tokio::test]
    async fn the_github_fallback_finds_and_validates_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let body = r#"{"incomplete_results":false,"items":[{"clone_url":"https://github.com/freshtechbro/claudedesignskills.git"}]}"#;
        let search = GitHubRepoSearch::new(Arc::new(FakeHttp {
            status: 200,
            body: body.to_string(),
        }));
        let fetcher = FakeFetcher {
            names: vec!["threejs-webgl".to_string()],
            commit: "abc".to_string(),
        };
        let found = discover_web_repositories(
            "threejs procedural materials",
            &search,
            &fetcher,
            &cfg(tmp.path()),
        )
        .await;
        assert_eq!(found.repos.len(), 1);
        assert!(found.repos[0]
            .packages
            .iter()
            .any(|p| p.name == "threejs-webgl"));
    }

    #[tokio::test]
    async fn disabled_web_makes_zero_requests() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = FakeProvider {
            host: "github.com".to_string(),
            urls: vec!["https://github.com/o/r".to_string()],
        };
        let fetcher = FakeFetcher {
            names: vec!["a".to_string()],
            commit: "x".to_string(),
        };
        // Web disabled by config: WebAccess never activates.
        let disabled = WebAccess::new(false, vec!["github.com".to_string()], Vec::new());
        let config = WebDiscoveryConfig {
            access: disabled,
            audit_log: tmp.path().join("audit.log"),
            staging_root: tmp.path().join("staging"),
            limit: 10,
        };
        let found = discover_web_repositories("q", &provider, &fetcher, &config).await;
        assert_eq!(
            found.requests_made, 0,
            "no request may go out when web is disabled"
        );
        assert!(found.repos.is_empty());
    }

    #[tokio::test]
    async fn a_rate_limited_search_is_partial_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let search = GitHubRepoSearch::new(Arc::new(FakeHttp {
            status: 403,
            body: String::new(),
        }));
        let fetcher = FakeFetcher {
            names: vec!["a".to_string()],
            commit: "x".to_string(),
        };
        let found = discover_web_repositories("q", &search, &fetcher, &cfg(tmp.path())).await;
        assert!(found.partial, "a rate limit must be reported as partial");
        assert!(found.repos.is_empty());
        assert!(found.notes.iter().any(|n| n.contains("rate-limited")));
    }

    fn validated_repo(url: &str, names: &[&str]) -> ValidatedRepo {
        ValidatedRepo {
            url: url.to_string(),
            commit: "c0ffee1234".to_string(),
            catalog_root: ".localpilot/skills".to_string(),
            packages: names
                .iter()
                .map(|n| CatalogPackage {
                    name: (*n).to_string(),
                    description: format!("does {n}"),
                    version: "0.1.0".to_string(),
                    dir: PathBuf::from("/tmp"),
                    source_path: format!(".localpilot/skills/{n}"),
                })
                .collect(),
        }
    }

    #[test]
    fn build_proposal_records_evidence_and_recommends_by_query() {
        let repo = validated_repo(
            "https://github.com/freshtechbro/claudedesignskills",
            &["threejs-webgl", "gardening"],
        );
        let proposal = build_proposal(&repo, "threejs materials", "project", "1000");
        assert_eq!(
            proposal.repo_url,
            "https://github.com/freshtechbro/claudedesignskills"
        );
        assert_eq!(proposal.commit, "c0ffee1234");
        assert_eq!(proposal.catalog_root, ".localpilot/skills");
        assert_eq!(
            proposal.available_skills,
            vec!["threejs-webgl", "gardening"]
        );
        assert_eq!(proposal.recommended_skill.as_deref(), Some("threejs-webgl"));
        assert_eq!(proposal.scope, "project");
        assert_eq!(proposal.state, ProposalState::Pending);
        assert_eq!(proposal.provenance, "github-search");
    }

    #[test]
    fn proposals_path_follows_scope() {
        let root = Path::new("/proj");
        let home = Path::new("/home");
        let project = proposals_path(root, Some(home), false).unwrap();
        assert!(project.ends_with("skill-proposals.toml"));
        assert!(project.starts_with("/proj"));
        let global = proposals_path(root, Some(home), true).unwrap();
        assert!(global.starts_with("/home"));
        // A global run with no home cannot place a proposal file.
        assert!(proposals_path(root, None, true).is_none());
    }

    #[test]
    fn render_discovery_shows_only_relevant_matches_and_the_recommendation() {
        let installed = DiscoveredSkill {
            name: "threejs-webgl".to_string(),
            description: "three.js procedural materials".to_string(),
            state: MatchState::Installed,
            repo_url: None,
            source_id: None,
            commit: None,
            catalog_root: None,
            source_path: None,
            discoverable: true,
        };
        let irrelevant = DiscoveredSkill {
            name: "gardening".to_string(),
            description: "water the plants".to_string(),
            state: MatchState::Available,
            repo_url: Some("https://github.com/o/r".to_string()),
            source_id: Some("o-r".to_string()),
            commit: Some("abc".to_string()),
            catalog_root: Some(".localpilot/skills".to_string()),
            source_path: Some(".localpilot/skills/gardening".to_string()),
            discoverable: false,
        };
        let ranked = rank("threejs materials", vec![installed, irrelevant], None);
        let mut buf = Vec::new();
        render_discovery("threejs materials", &ranked, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("[installed] threejs-webgl"), "{text}");
        assert!(
            !text.contains("gardening"),
            "irrelevant match hidden: {text}"
        );
        assert!(text.contains("Recommended: threejs-webgl"), "{text}");
    }

    #[tokio::test]
    async fn end_to_end_threejs_discovers_recommends_saves_and_mutates_nothing() {
        // The #41 flagship acceptance case, fully hermetic: a configured provider
        // surfaces `freshtechbro/claudedesignskills`, the fetcher validates a catalog
        // offering `threejs-webgl`, and discovery recommends it, writes the report,
        // and saves a review proposal — registering no source and installing nothing.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let provider = FakeProvider {
            host: "github.com".to_string(),
            urls: vec!["https://github.com/freshtechbro/claudedesignskills".to_string()],
        };
        let fetcher = FakeFetcher {
            names: vec!["threejs-webgl".to_string(), "other".to_string()],
            commit: "c0ffee1234".to_string(),
        };
        let deps = DiscoveryDeps {
            search: &provider,
            fetcher: &fetcher,
            home: Some(home),
            web_enabled: true,
            allowlist: vec!["github.com".to_string(), "api.github.com".to_string()],
            disallowlist: Vec::new(),
            audit_log: root
                .join(".localpilot")
                .join("research")
                .join("egress-audit.log"),
            autonomous_discovery: false,
        };
        let mut buf = Vec::new();
        let outcome = run_discovery(
            &root,
            "threejs procedural materials",
            false,
            true,
            &deps,
            &mut buf,
        )
        .await
        .unwrap();
        assert!(!outcome.had_failure);
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Relevant skills"), "{text}");
        assert!(text.contains("Recommended: threejs-webgl"), "{text}");

        // A review proposal was saved with the full evidence.
        let proposals = root.join(".localpilot").join("skill-proposals.toml");
        assert!(proposals.is_file(), "no review proposal was saved");
        let saved = std::fs::read_to_string(&proposals).unwrap();
        assert!(saved.contains("freshtechbro/claudedesignskills"), "{saved}");
        assert!(saved.contains("threejs-webgl"), "{saved}");
        assert!(saved.contains("pending"), "{saved}");

        // Nothing was mutated: no source registered, no skill installed, and the
        // transient validation snapshot was cleaned up.
        assert!(
            !root.join(".localpilot").join("skill-sources.toml").exists(),
            "discovery must not register a source"
        );
        assert!(
            !root.join(".localpilot").join("skills").exists(),
            "discovery must not install a skill"
        );
        let staging = root
            .join(".localpilot")
            .join("skill-discovery")
            .join(source_id(
                "https://github.com/freshtechbro/claudedesignskills",
            ));
        assert!(!staging.exists(), "validation left a snapshot behind");
    }

    #[test]
    fn autoload_gate_only_loads_an_installed_discoverable_skill_under_the_toggle() {
        let mk = |state: MatchState, discoverable: bool| DiscoveredSkill {
            name: "s".to_string(),
            description: "d".to_string(),
            state,
            repo_url: None,
            source_id: None,
            commit: None,
            catalog_root: None,
            source_path: None,
            discoverable,
        };
        // Toggle on: only an installed, discoverable skill auto-loads.
        assert!(should_autoload(&mk(MatchState::Installed, true), true));
        // A user-only installed skill stays report-only.
        assert!(!should_autoload(&mk(MatchState::Installed, false), true));
        // An available match never auto-loads (it is not installed).
        assert!(!should_autoload(&mk(MatchState::Available, true), true));
        // Toggle off: nothing auto-loads, even an installed discoverable skill.
        assert!(!should_autoload(&mk(MatchState::Installed, true), false));
    }

    #[tokio::test]
    async fn a_non_allowlisted_candidate_host_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        // The provider host is allowlisted, but it returns an off-allowlist repo.
        let provider = FakeProvider {
            host: "github.com".to_string(),
            urls: vec!["https://evil.example/o/r".to_string()],
        };
        let fetcher = FakeFetcher {
            names: vec!["a".to_string()],
            commit: "x".to_string(),
        };
        let found = discover_web_repositories("q", &provider, &fetcher, &cfg(tmp.path())).await;
        assert!(
            found.repos.is_empty(),
            "an off-allowlist candidate must not be fetched"
        );
        assert!(found.notes.iter().any(|n| n.contains("not allowlisted")));
    }
}
