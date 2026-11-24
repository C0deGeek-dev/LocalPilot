//! Search backends as both search call sites see them: chat `web_search` and
//! `/research`.
//!
//! A backend is either a search tool designated by hand (`[research.mcp]
//! tools` naming an `[mcp.servers]` entry) or a managed one (`[search.managed]`).
//! A managed backend is search-only: it is never part of the session's MCP
//! tools, it starts only at its first real search, its launch must still match
//! the identity recorded at setup, and its one designated tool must take a
//! single `query` string.

use std::sync::Arc;
use std::time::Duration;

use localpilot_config::{Config, CredentialStore, ManagedSearchBackend, McpServerConfig};
use localpilot_mcp::{McpClient, McpToolDescriptor, Transport};

/// One search tool LocalPilot may call.
#[derive(Debug, Clone)]
pub(crate) struct SearchSpec {
    /// The `[mcp.servers]` key, or the `[search.managed]` id.
    pub(crate) server: String,
    /// The exact tool name called.
    pub(crate) tool: String,
    /// What is run to start the server.
    pub(crate) launch: McpServerConfig,
    /// Set for a managed backend.
    pub(crate) managed: Option<ManagedSearchBackend>,
}

impl SearchSpec {
    /// `server.tool` for a hand-designated tool and `managed.id.tool` for a
    /// managed one, for disclosure and audit lines, so the two never read alike
    /// when they share a name.
    pub(crate) fn label(&self) -> String {
        if self.managed.is_some() {
            format!("managed.{}.{}", self.server, self.tool)
        } else {
            format!("{}.{}", self.server, self.tool)
        }
    }

    /// The key a consent grant and a running server are kept under. A
    /// `[search.managed]` id may equal an `[mcp.servers]` key, and the two are
    /// different programs: neither may use the other's grant or connection.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub(crate) fn key(&self) -> String {
        if self.managed.is_some() {
            format!("managed:{}", self.server)
        } else {
            format!("mcp:{}", self.server)
        }
    }

    /// The audit detail written before a managed launch: id, tier and launch
    /// identity, never a query.
    pub(crate) fn launch_detail(&self) -> String {
        self.managed.as_ref().map_or_else(String::new, |managed| {
            format!(
                "id={} tier={} identity={}",
                self.server,
                managed.tier.as_str(),
                managed.launch_identity
            )
        })
    }
}

/// Every search tool, in the order they are tried: hand-designated tools
/// whose server is configured, then managed backends. Also the `server.tool`
/// names designated by hand whose server is not configured.
pub(crate) fn search_specs(config: &Config) -> (Vec<SearchSpec>, Vec<String>) {
    let mut found = Vec::new();
    let mut unresolved = Vec::new();
    for pair in &config.research.mcp.tools {
        match config.mcp.servers.get(&pair.server) {
            Some(server) => found.push(SearchSpec {
                server: pair.server.clone(),
                tool: pair.tool.clone(),
                launch: server.clone(),
                managed: None,
            }),
            None => unresolved.push(format!("{}.{}", pair.server, pair.tool)),
        }
    }
    for (id, managed) in &config.search.managed {
        found.push(SearchSpec {
            server: id.clone(),
            tool: managed.tool.clone(),
            launch: managed.server(),
            managed: Some(managed.clone()),
        });
    }
    (found, unresolved)
}

/// How a search server is started. Production spawns the configured command
/// and completes the MCP handshake within the given bound; tests substitute a
/// scripted transport.
pub(crate) type Launcher = Arc<
    dyn Fn(
            McpServerConfig,
            Duration,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Arc<dyn Transport>, String>> + Send>,
        > + Send
        + Sync,
>;

/// The production launcher.
pub(crate) fn spawn_launcher() -> Launcher {
    Arc::new(|config: McpServerConfig, handshake: Duration| {
        Box::pin(async move {
            let (transport, _environment) =
                crate::mcp_env::spawn_server(&config, &CredentialStore::user())
                    .map_err(|error| error.to_string())?;
            let client = McpClient::new(Arc::clone(&transport));
            match tokio::time::timeout(handshake, client.initialize()).await {
                Ok(Ok(_)) => Ok(transport),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => Err("the server did not finish starting in time".to_string()),
            }
        })
    })
}

/// Why a managed backend was not started or not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ManagedStartError {
    /// The launch definition no longer matches the recorded identity.
    IdentityChanged,
    /// The pre-launch audit record could not be written.
    AuditFailed,
    /// The server did not start or finish its handshake.
    StartFailed,
    /// The designated tool is missing or does not take one `query` string.
    Incompatible(String),
    /// The engine it searches (a local SearXNG) is not LocalPilot's, did not
    /// start or does not answer.
    EngineNotReady(String),
}

impl ManagedStartError {
    /// The audit decision for this outcome.
    pub(crate) const fn decision(&self) -> &'static str {
        match self {
            Self::IdentityChanged => "search-launch-refused-identity",
            Self::AuditFailed => "search-launch-refused-audit",
            Self::StartFailed => "search-launch-failed",
            Self::Incompatible(_) => "search-launch-refused-schema",
            Self::EngineNotReady(_) => "search-launch-refused-engine",
        }
    }

    /// A short, value-free explanation for the person.
    pub(crate) fn describe(&self, server: &str) -> String {
        match self {
            Self::IdentityChanged => format!(
                "the search backend {server} was not started: its launch settings changed \
                 since setup (run the search setup again to review them)"
            ),
            Self::AuditFailed => format!(
                "the search backend {server} was not started: the egress audit record could \
                 not be written"
            ),
            Self::StartFailed => format!("the search backend {server} could not be started"),
            Self::Incompatible(reason) => {
                format!("the search backend {server} cannot be used: {reason}")
            }
            Self::EngineNotReady(reason) => {
                format!("the search backend {server} was not started: {reason}")
            }
        }
    }
}

/// Start a managed backend: identity check, then the pre-launch audit record
/// (`audit` returns whether it was written), then the launch, then the tool
/// schema check. Nothing is started, and no credential resolved, unless the
/// identity matches and the record was written.
pub(crate) async fn start_managed(
    spec: &SearchSpec,
    managed: &ManagedSearchBackend,
    launcher: &Launcher,
    ensure_engine: bool,
    audit: impl FnOnce(&str, &str) -> bool,
) -> Result<Arc<dyn Transport>, ManagedStartError> {
    if !managed.identity_matches() {
        return Err(ManagedStartError::IdentityChanged);
    }
    if !audit("search-launch", &spec.launch_detail()) {
        return Err(ManagedStartError::AuditFailed);
    }
    let keep = if ensure_engine && managed.tier == localpilot_config::SearchTier::SelfHosted {
        ensure_engine_running(managed)
            .await
            .map_err(ManagedStartError::EngineNotReady)?
    } else {
        crate::searxng::Keep::default()
    };
    let handshake = Duration::from_secs(managed.handshake_timeout_secs);
    let transport = tokio::time::timeout(
        handshake + Duration::from_secs(1),
        launcher(spec.launch.clone(), handshake),
    )
    .await
    .map_err(|_| ManagedStartError::StartFailed)?
    .map_err(|_| ManagedStartError::StartFailed)?;
    let client = McpClient::new(Arc::clone(&transport));
    let advertised = tokio::time::timeout(
        Duration::from_secs(managed.call_timeout_secs),
        client.list_tools(),
    )
    .await
    .map_err(|_| ManagedStartError::Incompatible("it did not list its tools in time".into()))?
    .map_err(|_| ManagedStartError::Incompatible("it did not list its tools".into()))?;
    check_search_schema(&advertised, &spec.tool).map_err(ManagedStartError::Incompatible)?;
    if keep.holds() {
        return Ok(Arc::new(Kept {
            inner: transport,
            _keep: keep,
        }));
    }
    Ok(transport)
}

/// A bridge whose SearXNG runs in Docker inside WSL: the distribution is kept
/// up for exactly as long as the bridge lives.
struct Kept {
    inner: Arc<dyn Transport>,
    _keep: crate::searxng::Keep,
}

#[async_trait::async_trait]
impl Transport for Kept {
    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, localpilot_mcp::McpError> {
        self.inner.call(method, params).await
    }

    async fn notify(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), localpilot_mcp::McpError> {
        self.inner.notify(method, params).await
    }
}

/// Before a self-hosted backend's bridge starts: its SearXNG container is
/// verified as LocalPilot's and started if it is stopped.
async fn ensure_engine_running(
    managed: &ManagedSearchBackend,
) -> Result<crate::searxng::Keep, String> {
    let port = managed
        .env
        .get("SEARXNG_URL")
        .and_then(|item| match item {
            localpilot_config::McpEnvEntry::Plain(url) => {
                url.rsplit(':').next()?.parse::<u16>().ok()
            }
            localpilot_config::McpEnvEntry::Object(_) => None,
        })
        .ok_or_else(|| "its SearXNG address is not a local port".to_string())?;
    let state_dir = crate::search_setup::state_dir()
        .ok_or_else(|| "there is no home folder for SearXNG's record".to_string())?;
    tokio::task::spawn_blocking(move || {
        crate::searxng::ensure_running(
            &crate::searxng::LocalHost,
            &state_dir,
            port,
            &|event, detail| crate::search_setup::audit_searxng(&state_dir, event, detail),
        )
    })
    .await
    .map_err(|_| "starting SearXNG was interrupted".to_string())?
}

/// What a consent question adds for a backend LocalPilot also starts an
/// engine for.
pub(crate) const fn start_note(tier: localpilot_config::SearchTier) -> &'static str {
    match tier {
        localpilot_config::SearchTier::SelfHosted => {
            " If its SearXNG container is stopped, LocalPilot starts it first; it uses about 105 MB of memory while it runs (localpilot search stop frees it)."
        }
        _ => "",
    }
}

/// Whether `tool` is advertised and takes exactly what LocalPilot sends: a
/// `query` string, with no other required parameter.
pub(crate) fn check_search_schema(
    advertised: &[McpToolDescriptor],
    tool: &str,
) -> Result<(), String> {
    let Some(descriptor) = advertised.iter().find(|candidate| candidate.name == tool) else {
        return Err(format!("it does not offer the tool {tool}"));
    };
    let schema = &descriptor.input_schema;
    let query_type = schema
        .get("properties")
        .and_then(|properties| properties.get("query"))
        .and_then(|query| query.get("type"))
        .and_then(serde_json::Value::as_str);
    if query_type != Some("string") {
        return Err(format!("its tool {tool} does not take a query string"));
    }
    let extra_required = schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|required| required.iter().any(|name| name.as_str() != Some("query")));
    if extra_required {
        return Err(format!(
            "its tool {tool} requires parameters other than the query"
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod tests {
    use super::*;
    use localpilot_config::SearchTier;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Echo;

    #[async_trait::async_trait]
    impl Transport for Echo {
        async fn call(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, localpilot_mcp::McpError> {
            Ok(json!({ "method": method, "params": params }))
        }
    }

    #[tokio::test]
    async fn a_kept_bridge_answers_as_the_bridge_and_ends_the_keep_with_it() {
        let held = Arc::new(());
        let keep = crate::searxng::Keep::holding(Arc::clone(&held));
        let kept = Kept {
            inner: Arc::new(Echo),
            _keep: keep,
        };
        assert_eq!(
            kept.call("tools/list", json!({"a": 1})).await.unwrap(),
            json!({ "method": "tools/list", "params": {"a": 1} })
        );
        kept.notify("notifications/initialized", json!({}))
            .await
            .unwrap();
        assert_eq!(Arc::strong_count(&held), 2);
        drop(kept);
        assert_eq!(Arc::strong_count(&held), 1, "the keep ends with the bridge");
    }

    /// A managed backend whose recorded identity matches its launch.
    pub(crate) fn managed_backend(tool: &str) -> ManagedSearchBackend {
        let mut managed = ManagedSearchBackend {
            tier: SearchTier::Keyless,
            command: "search-server".to_string(),
            args: vec!["--stdio".to_string()],
            env: indexmap::IndexMap::new(),
            tool: tool.to_string(),
            launch_identity: String::new(),
            call_timeout_secs: 5,
            handshake_timeout_secs: 5,
            max_query_chars: 400,
            max_query_words: None,
            unattended: false,
        };
        managed.launch_identity = localpilot_config::search::launch_identity(&managed.server());
        managed
    }

    pub(crate) fn search_tool_list(tool: &str) -> serde_json::Value {
        json!({ "tools": [{
            "name": tool,
            "inputSchema": {
                "type": "object",
                "properties": { "query": { "type": "string" }, "max_results": { "type": "integer" } },
                "required": ["query"]
            }
        }]})
    }

    /// A launcher handing out `transport`, counting starts.
    pub(crate) fn counting_launcher(
        transport: Arc<localpilot_mcp::ScriptedTransport>,
    ) -> (Launcher, Arc<AtomicUsize>) {
        let starts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&starts);
        let launcher: Launcher = Arc::new(move |_config, _handshake| {
            counter.fetch_add(1, Ordering::SeqCst);
            let transport: Arc<dyn Transport> = transport.clone();
            Box::pin(async move { Ok(transport) })
        });
        (launcher, starts)
    }

    fn descriptor(schema: serde_json::Value) -> Vec<McpToolDescriptor> {
        vec![serde_json::from_value(json!({ "name": "search", "inputSchema": schema })).unwrap()]
    }

    #[test]
    fn only_a_single_query_string_schema_is_compatible() {
        let ok = descriptor(json!({
            "type": "object",
            "properties": { "query": { "type": "string" }, "count": { "type": "integer" } },
            "required": ["query"]
        }));
        assert!(check_search_schema(&ok, "search").is_ok());
        assert!(check_search_schema(&ok, "other").is_err());
        let not_string = descriptor(json!({ "properties": { "query": { "type": "integer" } } }));
        assert!(check_search_schema(&not_string, "search").is_err());
        let extra = descriptor(json!({
            "properties": { "query": { "type": "string" }, "key": { "type": "string" } },
            "required": ["query", "key"]
        }));
        assert!(check_search_schema(&extra, "search").is_err());
        let no_query = descriptor(json!({ "properties": { "q": { "type": "string" } } }));
        assert!(check_search_schema(&no_query, "search").is_err());
    }

    #[test]
    fn managed_backends_follow_hand_designated_tools() {
        let mut config = Config::default();
        config
            .mcp
            .servers
            .insert("hand".to_string(), McpServerConfig::default());
        config
            .research
            .mcp
            .tools
            .push(localpilot_config::ResearchMcpTool {
                server: "hand".to_string(),
                tool: "find".to_string(),
            });
        config
            .research
            .mcp
            .tools
            .push(localpilot_config::ResearchMcpTool {
                server: "gone".to_string(),
                tool: "find".to_string(),
            });
        config
            .search
            .managed
            .insert("ddg".to_string(), managed_backend("search"));
        let (specs, unresolved) = search_specs(&config);
        let labels: Vec<_> = specs.iter().map(SearchSpec::label).collect();
        assert_eq!(labels, ["hand.find", "managed.ddg.search"]);
        assert!(specs[0].managed.is_none());
        assert!(specs[1].managed.is_some());
        assert_eq!(unresolved, ["gone.find"]);
    }

    #[tokio::test]
    async fn a_changed_launch_or_a_failed_record_starts_nothing() {
        let transport = Arc::new(
            localpilot_mcp::ScriptedTransport::new().with("tools/list", search_tool_list("search")),
        );
        let (launcher, starts) = counting_launcher(transport);
        let managed = managed_backend("search");
        let spec = SearchSpec {
            server: "ddg".to_string(),
            tool: "search".to_string(),
            launch: managed.server(),
            managed: Some(managed.clone()),
        };
        let mut tampered = managed.clone();
        tampered.args.push("--exfiltrate".to_string());
        let mut tampered_spec = spec.clone();
        tampered_spec.launch = tampered.server();
        let mut audited = Vec::new();
        assert_eq!(
            start_managed(&tampered_spec, &tampered, &launcher, false, |d, _| {
                audited.push(d.to_string());
                true
            })
            .await
            .err(),
            Some(ManagedStartError::IdentityChanged)
        );
        assert!(audited.is_empty(), "nothing recorded as a launch");
        assert_eq!(
            start_managed(&spec, &managed, &launcher, false, |_, _| false)
                .await
                .err(),
            Some(ManagedStartError::AuditFailed)
        );
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        let mut details = Vec::new();
        assert!(
            start_managed(&spec, &managed, &launcher, false, |d, detail| {
                details.push((d.to_string(), detail.to_string()));
                true
            })
            .await
            .is_ok()
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(details[0].0, "search-launch");
        assert!(details[0]
            .1
            .contains("id=ddg tier=keyless identity=sha256:"));
    }

    #[tokio::test]
    async fn an_incompatible_tool_is_refused_after_start() {
        let transport = Arc::new(localpilot_mcp::ScriptedTransport::new().with(
            "tools/list",
            json!({ "tools": [{ "name": "search", "inputSchema": { "properties": { "q": { "type": "string" } } } }] }),
        ));
        let (launcher, _starts) = counting_launcher(transport);
        let managed = managed_backend("search");
        let spec = SearchSpec {
            server: "ddg".to_string(),
            tool: "search".to_string(),
            launch: managed.server(),
            managed: Some(managed.clone()),
        };
        let error = start_managed(&spec, &managed, &launcher, false, |_, _| true)
            .await
            .err()
            .unwrap();
        assert!(matches!(error, ManagedStartError::Incompatible(_)));
        assert_eq!(error.decision(), "search-launch-refused-schema");
    }
}
