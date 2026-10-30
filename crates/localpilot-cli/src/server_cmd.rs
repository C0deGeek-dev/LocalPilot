//! `localpilot serve` / `localpilot connect`: the opt-in local-IPC server.
//!
//! This module wires the already-built `localpilot-server` primitives
//! (transport, daemon lifecycle, session registry, per-session host, attach
//! dispatch) to a user-facing `serve` daemon and a thin `connect` client. It is
//! strictly opt-in: nothing here runs unless the user invokes `serve`/`connect`
//! (or the hidden `__server-serve` the daemon spawns). The default in-process
//! `chat`/`ask`/`print`/`harness` path never touches any of it (D003).
//!
//! # The shared session recipe
//!
//! [`SessionSetup`] resolves a workspace's config, provider, and MCP servers
//! once, then [`SessionSetup::build`] mints a fresh, independent
//! [`SessionRuntime`] on demand from that setup. It is the single construction
//! recipe: the stdio `localpilot rpc` command (see [`crate::rpc_cmd`]) and the
//! server's per-connection [`ServerFactory`] both build sessions through it, so
//! the two paths can never drift.
//!
//! # The per-connection loop
//!
//! Each accepted connection reads a first `attach` record, binds to a session
//! (reusing a live [`SessionHost`] when several connections name the same id, so
//! fanout is the host's own broadcast), confirms with `attached`, then routes
//! `prompt`/`cancel`/`steer`/`status`/`permission_reply` records while streaming
//! every session event back. EOF or `shutdown` detaches *this* connection only —
//! the session lives on for any other attached client (lifecycle/reaping is a
//! later subject).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use localpilot_config::{CliOverrides, ConfigPaths};
use localpilot_core::SessionId;
use localpilot_harness::{
    register_project_analysis_context, register_project_instructions_context, SessionConfig,
    SessionRuntime, SummarizerTuning,
};
use localpilot_llm::{ModelProvider, ProviderRegistry};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_rpc::{
    AskRegistry, AttachTarget, ClientCommand, ClientRecord, InputDisposition, JsonRecordReader,
    PendingAsk, Principal, RejectCode, ReplyRefusal, RpcApprover, ServerEvent, ServerRecord,
    CAPABILITY_ACKS, CAPABILITY_RESUME, RPC_PROTOCOL_VERSION, SERVER_VERSION,
};
use localpilot_sandbox::{Approver, Interactivity, PermissionEngine, Profile, ScriptedApprover};
use localpilot_server::host::Admit;
use localpilot_server::{
    acquire, attach, ensure_running, Acquired, AttachError, Conn, Endpoint, Listener,
    RegistryError, SessionFactory, SessionHost, SessionRegistry, TransportError,
};
use localpilot_store::Store;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::server_devices::{
    presented_device_id, DeviceRegistry, FailureDelays, GatewayLeases, PairingRefusal, Pairings,
    Refusal, PAIRING_TTL,
};
use crate::server_feed::{stored_history, Admission, SessionFeed};
use crate::server_requests::{canonical_payload, Admission as RequestAdmission, RequestStore};

// --- The shared session-construction recipe ---------------------------------

/// A workspace's resolved inputs for building sessions: config, the chosen
/// provider, the model, the permission profile, and the connected MCP servers.
///
/// Resolved once (config load, provider resolution, MCP server spawn) so each
/// [`build`](Self::build) is a cheap synchronous call. Captured by the server
/// [`ServerFactory`] at `serve` startup and used once by the stdio `rpc`
/// command.
pub(crate) struct SessionSetup {
    config: localpilot_config::Config,
    cwd: std::path::PathBuf,
    provider: Arc<dyn ModelProvider>,
    model: String,
    profile: Profile,
    /// Held so the spawned MCP server processes stay alive for the life of the
    /// setup; each [`build`](Self::build) projects a fresh registry from it.
    mcp: crate::mcp::McpTools,
    agents: Option<Arc<localpilot_agents::AgentSet>>,
    /// Resolved at asynchronous setup, consumed by the synchronous factories.
    context_windows: HashMap<(String, String), crate::context_window::Window>,
}

/// A freshly built session plus the wire approver's serve-loop halves. The
/// runtime bakes the [`RpcApprover`]; the halves let the driving connection
/// surface that session's permission asks and answer them.
pub(crate) struct BuiltSession {
    pub(crate) runtime: SessionRuntime,
    pub(crate) ask_rx: mpsc::UnboundedReceiver<PendingAsk>,
    pub(crate) asks: AskRegistry,
}

impl SessionSetup {
    /// Resolve the workspace config, model, provider, MCP servers, and agents.
    ///
    /// # Errors
    /// Returns an error if configuration, the model, or the provider cannot be
    /// resolved.
    pub(crate) async fn resolve(
        model: Option<&str>,
        provider_id: Option<&str>,
        profile: Profile,
    ) -> anyhow::Result<Self> {
        let cwd = std::env::current_dir()?;
        let config =
            localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())?;
        let model = model
            .map(str::to_string)
            .or_else(|| config.resolve_model(provider_id))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no model: pass --model, or set a default in .localpilot.toml \
                     ([providers.<id>] model = \"...\")"
                )
            })?;
        let registry = ProviderRegistry::from_config(&config)?;
        let provider = match provider_id {
            Some(id) => registry.get(id),
            None => registry.default_provider(),
        }
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no provider is configured"))?;
        let mut selections = HashMap::new();
        for id in registry.ids() {
            if let (Some(model), Some(provider)) = (registry.default_model(&id), registry.get(&id))
            {
                selections.insert(
                    (id, model.to_owned()),
                    provider.declaration().max_context_tokens,
                );
            }
        }
        selections.insert(
            (provider.declaration().id.clone(), model.clone()),
            provider.declaration().max_context_tokens,
        );
        let resolutions =
            futures::future::join_all(selections.into_iter().map(|((id, model), declared)| {
                let config = &config;
                async move {
                    let resolution =
                        crate::context_window::resolve(config, &id, &model, declared).await;
                    if let Some(warning) = resolution.warning_once() {
                        eprintln!("{warning}");
                    }
                    ((id, model), resolution.window)
                }
            }))
            .await;
        let context_windows = resolutions.into_iter().collect();
        let mcp = crate::mcp::McpTools::load(&config).await;
        let agents = crate::agents_cmd::session_agents(&cwd);
        Ok(Self {
            config,
            cwd,
            provider,
            model,
            profile,
            mcp,
            agents,
            context_windows,
        })
    }

    /// A setup over an already-built provider and model, for offline tests that
    /// need the real session-construction recipe without config, credentials, or
    /// MCP. Bypass profile so a headless worker's writes are permitted.
    #[cfg(test)]
    pub(crate) fn for_test(
        provider: Arc<dyn ModelProvider>,
        model: impl Into<String>,
        cwd: std::path::PathBuf,
    ) -> Self {
        Self {
            config: localpilot_config::Config::default(),
            cwd,
            provider,
            model: model.into(),
            profile: Profile::Bypass,
            mcp: crate::mcp::McpTools::default(),
            agents: None,
            context_windows: HashMap::new(),
        }
    }

    /// The resolved model name.
    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    /// The resolved default provider.
    pub(crate) fn provider(&self) -> &Arc<dyn ModelProvider> {
        &self.provider
    }

    /// Map each model a configured provider advertises to that provider, so a
    /// swarm worker whose node names a non-default model is built on the provider
    /// that serves it. Rebuilds the provider registry from the resolved config;
    /// that is pure (no network, no MCP), so it is cheap to do once at startup.
    /// Returns an empty map if the registry cannot be built — the caller still
    /// has the default provider for the session's own model.
    pub(crate) fn provider_routes(&self) -> HashMap<String, Arc<dyn ModelProvider>> {
        let mut routes = HashMap::new();
        let Ok(registry) = ProviderRegistry::from_config(&self.config) else {
            return routes;
        };
        for id in registry.ids() {
            if let (Some(model), Some(provider)) = (registry.default_model(&id), registry.get(&id))
            {
                routes.insert(model.to_string(), provider.clone());
            }
        }
        routes
    }

    /// The workspace root.
    pub(crate) fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    /// Build one fresh, independent [`SessionRuntime`] from this setup, plus the
    /// wire approver halves for the connection that will drive it.
    ///
    /// This is the single recipe the stdio `rpc` command and the server factory
    /// share — provider, tools (builtins + MCP), broker, permission engine,
    /// wire approver, store, workspace read-roots, recovery, and the interactive
    /// [`SessionConfig`], then the project-context hooks.
    ///
    /// # Errors
    /// Returns an error if the workspace read-roots cannot be resolved.
    pub(crate) fn build(&self) -> anyhow::Result<BuiltSession> {
        let (approver, ask_rx, asks) = RpcApprover::new();
        // The wire client answers asks; the engine treats the session as
        // interactive so ask-class effects reach the client instead of being
        // denied outright.
        let runtime = self.build_session(
            &self.provider,
            &self.model,
            Box::new(approver),
            Interactivity::Interactive,
        )?;
        Ok(BuiltSession {
            runtime,
            ask_rx,
            asks,
        })
    }

    /// Build one headless worker session on an explicit provider and model,
    /// sharing this setup's resolved config, tools, and MCP servers.
    ///
    /// The swarm's per-worker recipe: the *same* session-construction path as
    /// [`build`](Self::build), but non-interactive with a deny-by-default approver
    /// — a worker has no wire client to answer a permission ask — and free to
    /// target any configured provider so different workers can run on different
    /// models. Building the session is deliberately all this does; the spawn path
    /// verifies afterwards that the worker really runs on the model asked for.
    ///
    /// # Errors
    /// Returns an error if the workspace read-roots cannot be resolved.
    pub(crate) fn build_worker(
        &self,
        provider: &Arc<dyn ModelProvider>,
        model: &str,
    ) -> anyhow::Result<SessionRuntime> {
        self.build_session(
            provider,
            model,
            Box::new(ScriptedApprover::new(Vec::new())),
            Interactivity::NonInteractive,
        )
    }

    /// The shared session-construction recipe, parameterised on which provider
    /// and model to build on and how the session answers permission asks. Both
    /// the wire [`build`](Self::build) and the headless
    /// [`build_worker`](Self::build_worker) go through here so the two can never
    /// drift.
    ///
    /// # Errors
    /// Returns an error if the workspace read-roots cannot be resolved.
    fn build_session(
        &self,
        provider: &Arc<dyn ModelProvider>,
        model: &str,
        approver: Box<dyn Approver>,
        interactivity: Interactivity,
    ) -> anyhow::Result<SessionRuntime> {
        let window = self
            .context_windows
            .get(&(provider.declaration().id.clone(), model.to_owned()))
            .copied()
            .unwrap_or_else(|| {
                crate::context_window::Window::from_signals(
                    None,
                    provider.declaration().max_context_tokens,
                    self.config.harness.context_token_limit,
                )
            });
        if let Some(warning) = crate::context_window::capacity_warning_once(
            &provider.declaration().id,
            model,
            window,
            self.config.harness.context_token_limit,
            provider.declaration().max_output_tokens,
        ) {
            eprintln!("{warning}");
        }
        let context_token_limit = window.budget(
            self.config.harness.context_token_limit,
            provider.declaration().max_output_tokens,
        );
        let mut registry = self.mcp.registry();
        let broker = crate::mcp::install_broker(&self.config.tools, &mut registry);
        // Apply the built-in safety rails: interactive for the client-driven serve
        // loop, headless for a worker with no client. Explicit `[harness]` values
        // still win inside `resolved_rails`.
        let interactive = matches!(interactivity, Interactivity::Interactive);
        let rails = self.config.harness.resolved_rails(interactive);
        let mut runtime = SessionRuntime::new(
            provider.clone(),
            registry,
            PermissionEngine::new(self.profile, Vec::new())
                .with_allowed_commands(crate::session_cmd::allowed_commands(&self.config)),
            approver,
            Store::open(&self.cwd),
            crate::session_cmd::workspace_with_read_roots(&self.cwd, &self.config)?,
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig {
                model: model.to_string(),
                interactivity,
                trusted: matches!(self.profile, Profile::Bypass | Profile::Unrestricted),
                context_token_limit,
                compaction_mode: compaction_mode(self.config.compaction.mode),
                summarizer_tuning: SummarizerTuning::from_config(&self.config.compaction),
                tool_call_budget: rails.tool_call_budget,
                tool_call_budget_max: rails.tool_call_budget_max,
                tool_budget_explicit: rails.budget_explicit,
                rules: self.config.harness.rules.clone(),
                enforce_claim_gate: self.config.harness.claim_gate.is_enabled(),
                tool_marker_enabled: self.config.tools.marker,
                tool_prompt_reveal: self.config.tools.prompt_reveal,
                enforce_readable_errors: self.config.tools.readable_errors,
                repair_mode: self.config.tools.repair,
                elide_seen_reads: self.config.tools.elide_seen_reads,
                turn_timeout: rails.turn_timeout_secs.map(std::time::Duration::from_secs),
                granularity: Some(self.config.harness.granularity.clone()),
                verify_before_done: self.config.harness.verify_before_done,
                verify_command: self.config.harness.verify_command.clone(),
                ..SessionConfig::default()
            },
            Vec::new(),
        );
        runtime.set_broker(broker);
        if let Some(agents) = &self.agents {
            runtime.set_agents(agents.clone());
        }
        register_project_analysis_context(
            &self.cwd,
            self.config.context.project_analysis,
            self.config.docs.lookup_policy,
            &mut runtime,
        );
        register_project_instructions_context(
            &self.cwd,
            self.config.context.inject_instructions,
            self.config.context.instruction_char_budget,
            &mut runtime,
        );
        runtime.set_retrieved_beside_question(self.config.context.retrieved_beside_question);
        Ok(runtime)
    }
}

/// Display label for a permission profile (shared with the stdio `rpc` serve
/// context so both surfaces name profiles identically).
pub(crate) fn profile_label(profile: Profile) -> &'static str {
    match profile {
        Profile::Default => "default",
        Profile::Relaxed => "relaxed",
        Profile::ReadOnly => "readonly",
        Profile::Bypass => "bypass",
        Profile::Unrestricted => "unrestricted",
    }
}

fn compaction_mode(mode: localpilot_config::CompactionMode) -> localpilot_harness::CompactionMode {
    match mode {
        localpilot_config::CompactionMode::Deterministic => {
            localpilot_harness::CompactionMode::Deterministic
        }
        localpilot_config::CompactionMode::SmartWithFallback => {
            localpilot_harness::CompactionMode::SmartWithFallback
        }
    }
}

// --- The server session factory ---------------------------------------------

/// The serve-loop halves of one session's wire approver.
struct ApproverHalves {
    ask_rx: mpsc::UnboundedReceiver<PendingAsk>,
    asks: AskRegistry,
}

/// A registry [`SessionFactory`] that also hands the connection triggering a
/// build the wire approver halves for the session it created, and reports the
/// model for `hello`/`status`.
trait ConnectionFactory: SessionFactory {
    /// Take the approver halves of the most recently built session, if any.
    fn take_asks(&self) -> Option<ApproverHalves>;
    /// The model these sessions run.
    fn model(&self) -> &str;
}

/// The server's session factory: builds a fresh, independent [`SessionRuntime`]
/// per new session from the workspace's resolved [`SessionSetup`], and stashes
/// the wire approver halves so the connection that triggered the build services
/// that session's permission asks.
struct ServerFactory {
    setup: SessionSetup,
    /// The approver halves of the most recently built session. Single-slot:
    /// attach binding is serialized by the host-map mutex, so exactly one
    /// build's halves sit here between a `create()` and the serve loop taking
    /// them.
    pending: StdMutex<Option<ApproverHalves>>,
}

impl ServerFactory {
    fn new(setup: SessionSetup) -> Self {
        Self {
            setup,
            pending: StdMutex::new(None),
        }
    }
}

impl SessionFactory for ServerFactory {
    fn create(&self) -> Result<SessionRuntime, RegistryError> {
        let built = self
            .setup
            .build()
            .map_err(|error| RegistryError::Factory(error.to_string()))?;
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = Some(ApproverHalves {
            ask_rx: built.ask_rx,
            asks: built.asks,
        });
        Ok(built.runtime)
    }
}

impl ConnectionFactory for ServerFactory {
    fn take_asks(&self) -> Option<ApproverHalves> {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn model(&self) -> &str {
        self.setup.model()
    }
}

// --- The per-session host map (multi-client sharing) -------------------------

/// The set of live per-session hosts, keyed by [`SessionId`], kept beside the
/// registry. Several connections that attach to the same id share one
/// [`SessionHost`] — created on first attach — so fanout is the host's own
/// broadcast.
#[derive(Clone, Default)]
struct HostMap {
    inner: Arc<Mutex<HashMap<SessionId, Arc<SessionHost>>>>,
    /// Each hosted session's permission asks and sequenced feed. The asks
    /// belong to the session, not to the connection that happened to create
    /// it: every attached client sees them and any of them may answer.
    served: Arc<StdMutex<HashMap<SessionId, Served>>>,
    /// Retry keys of mutating commands, per session. Kept beside the host map
    /// (not in a host) so a session closed after inactivity and resumed still
    /// answers a retry with its original reply.
    requests: Arc<RequestStore>,
    /// This workspace's paired devices (in memory unless opened on a store).
    devices: Arc<DeviceRegistry>,
    /// The gateway's live capabilities.
    gateways: Arc<GatewayLeases>,
    /// Outstanding pairing codes.
    pairings: Arc<Pairings>,
    /// Growing delays for failed device credentials.
    delays: Arc<FailureDelays>,
    /// Device handshakes (attach, pairing exchange) verified at once.
    handshakes: Arc<Handshakes>,
}

/// Bounds the device handshakes the server verifies at once. Under a flood a
/// valid device waits for a slot like everyone else; it is never refused for
/// others' failures.
struct Handshakes(tokio::sync::Semaphore);

impl Default for Handshakes {
    fn default() -> Self {
        Self(tokio::sync::Semaphore::new(16))
    }
}

/// What the server keeps for one hosted session besides its host.
#[derive(Clone)]
struct Served {
    asks: Option<AskRegistry>,
    feed: Arc<SessionFeed>,
}

impl HostMap {
    /// Bind a connection to its session: reuse the live host when the target id
    /// is already hosted, otherwise create+register the session via the registry
    /// and wrap its handle in a fresh host. Returns the bound id, the shared
    /// host, and the session's ask registry (absent only when the factory built
    /// no wire approver).
    ///
    /// Binding is serialized by the map mutex, which also fences the factory's
    /// single-slot approver hand-off.
    async fn bind<F: ConnectionFactory>(
        &self,
        target: AttachTarget,
        registry: &SessionRegistry,
        factory: &F,
        store: &Store,
        authorized: &(dyn Fn() -> Result<(), String> + Sync),
    ) -> Result<(SessionId, Arc<SessionHost>, Served), BindError> {
        let mut map = self.inner.lock().await;
        // The authorization point: checked while holding the host map, before
        // any session is opened, resumed or joined. A device whose gateway
        // closed or whose pairing was revoked while this waited gets nothing.
        authorized().map_err(BindError::Unauthorized)?;
        // A resume target whose session is already hosted short-circuits to the
        // live host — never a second `attach`, so the store known-id guard (for
        // an on-disk resume) is correctly skipped for a live session, and a
        // freshly opened session with no store entry yet is still joinable.
        if let Some(id) = existing_id(&target, store)? {
            if let Some(host) = map.get(&id) {
                let served = self.served_for(id, host, None);
                return Ok((id, host.clone(), served));
            }
        }
        let id = attach(target, registry, factory, store).await?;
        let handle = registry
            .get(id)
            .await
            .ok_or(AttachError::Registry(RegistryError::NotFound))?;
        let host = Arc::new(SessionHost::new(handle).await);
        {
            // Queued work from a device starts only while that device is still
            // paired and granted this workspace.
            let devices = self.devices.clone();
            host.set_origin_gate(Arc::new(move |origin: &str| {
                origin.strip_prefix("device:").is_none_or(|id| {
                    devices
                        .standing(id)
                        .is_ok_and(|standing| standing.scope >= localpilot_rpc::Scope::Control)
                })
            }));
        }
        map.insert(id, host.clone());
        let asks = factory.take_asks().map(|halves| {
            // The feed reads asks from the registry's broadcast; this only
            // keeps the approver's channel open (a closed one denies every ask)
            // and ends with the session's approver.
            let mut ask_rx = halves.ask_rx;
            tokio::spawn(async move { while ask_rx.recv().await.is_some() {} });
            halves.asks
        });
        let served = self.served_for(id, &host, Some(asks));
        Ok((id, host, served))
    }

    /// The served-session record for `id`, created (with its feed) on first
    /// use. `fresh_asks` is the ask registry of a session just built.
    fn served_for(
        &self,
        id: SessionId,
        host: &SessionHost,
        fresh_asks: Option<Option<AskRegistry>>,
    ) -> Served {
        let mut served = self.served.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(asks) = fresh_asks {
            // A fresh build replaces whatever an earlier host of this id left.
            if let Some(old) = served.remove(&id) {
                old.feed.close("replaced");
            }
            let feed = SessionFeed::start(host.subscribe(), asks.clone());
            served.insert(id, Served { asks, feed });
        }
        served
            .entry(id)
            .or_insert_with(|| Served {
                asks: None,
                feed: SessionFeed::start(host.subscribe(), None),
            })
            .clone()
    }

    /// The served-session record for `id`, if any.
    fn served(&self, id: SessionId) -> Option<Served> {
        self.served
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    /// Close the session's feed (attached clients are told why) and forget it.
    fn close_served(&self, id: SessionId, reason: &str) {
        let removed = self
            .served
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        if let Some(served) = removed {
            served.feed.close(reason);
        }
    }
}

/// How often a write that has not finished re-checks a device's access.
const ACCESS_POLL: Duration = Duration::from_millis(250);

/// Run `work` (a write to the client) while the connection's principal keeps
/// its access: `None` as soon as the gateway lease ends or `authorized` fails,
/// checked before `work` starts and every [`ACCESS_POLL`] while it is blocked.
/// For the local machine `authorized` always holds and the lease never ends.
async fn while_authorized<T>(
    lease: &CancellationToken,
    authorized: &(dyn Fn() -> Result<(), String> + Sync),
    work: impl std::future::Future<Output = T>,
) -> Option<T> {
    // Checked here, synchronously: an interval's first tick still waits for
    // the timer driver, which a write that is ready at once would beat.
    if lease.is_cancelled() || authorized().is_err() {
        return None;
    }
    let lost = async {
        let mut poll =
            tokio::time::interval_at(tokio::time::Instant::now() + ACCESS_POLL, ACCESS_POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            poll.tick().await;
            if authorized().is_err() {
                return;
            }
        }
    };
    tokio::select! {
        biased;
        () = lease.cancelled() => None,
        () = lost => None,
        result = work => Some(result),
    }
}

/// Why a connection could not be bound to a session.
#[derive(Debug)]
enum BindError {
    Attach(AttachError),
    /// The principal was no longer authorized when binding.
    Unauthorized(String),
}

impl From<AttachError> for BindError {
    fn from(error: AttachError) -> Self {
        Self::Attach(error)
    }
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Attach(error) => error.fmt(f),
            Self::Unauthorized(reason) => f.write_str(reason),
        }
    }
}

/// The concrete id a resume target already names, so a live host can be reused
/// without re-attaching. `open_new` never names an existing id.
fn existing_id(target: &AttachTarget, store: &Store) -> Result<Option<SessionId>, AttachError> {
    match target {
        AttachTarget::OpenNew => Ok(None),
        AttachTarget::ResumeId { session_id } => Ok(Some(*session_id)),
        AttachTarget::ResumeName { name } => Ok(store
            .find_session_by_name(name)
            .map_err(|error| AttachError::Registry(RegistryError::Store(error)))?
            .map(|entry| entry.id)),
        // A newer attach mode this build does not know: let `attach` report it.
        _ => Ok(None),
    }
}

// --- Session reaping --------------------------------------------------------

/// After the last client of a session detaches, the reaper waits this long
/// before removing the session, so a client that reconnects promptly rejoins the
/// same live session rather than a rebuilt one.
const REAP_GRACE_SECS: u64 = 60;
/// A session with no activity (no turn started) for at least this long is reaped
/// even while a client is still nominally attached — a stalled or abandoned
/// client must not pin a session's RAM forever.
const REAP_IDLE_SECS: u64 = 30 * 60;
/// How often the reaper wakes to scan the live sessions.
const REAP_TICK_SECS: u64 = 30;

/// Tuning for the [`Reaper`]. Production uses [`ReaperConfig::default`]; tests
/// inject tiny (or huge) durations to drive each branch deterministically.
#[derive(Clone, Copy, Debug)]
struct ReaperConfig {
    /// Grace after the last client detaches before the session is reaped.
    grace: Duration,
    /// Idle time (no turn started) before the session is reaped regardless of
    /// attached clients.
    idle: Duration,
    /// How often [`Reaper::run`] scans.
    tick: Duration,
}

impl Default for ReaperConfig {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(REAP_GRACE_SECS),
            idle: Duration::from_secs(REAP_IDLE_SECS),
            tick: Duration::from_secs(REAP_TICK_SECS),
        }
    }
}

/// Removes sessions no client needs any more, persisting each before it goes.
///
/// A session is reaped when either its last client detached at least `grace` ago,
/// or it has been idle for at least `idle` — but **never** while a turn is in
/// flight. Busy-safety is the [`SessionHandle`]'s own async mutex: a running turn
/// holds it for the whole turn, so the reaper only closes a session it can
/// [`try_lock`](tokio::sync::Mutex::try_lock). It persists the event log
/// ([`SessionRuntime::close`](localpilot_harness::SessionRuntime::close)) *before*
/// removing the session from the registry and host map.
struct Reaper {
    registry: SessionRegistry,
    hosts: HostMap,
    config: ReaperConfig,
    /// When each currently client-less session was first observed empty, so the
    /// grace window is measured from the disconnect rather than from start-up.
    first_empty: HashMap<SessionId, Instant>,
}

impl Reaper {
    fn new(registry: SessionRegistry, hosts: HostMap, config: ReaperConfig) -> Self {
        Self {
            registry,
            hosts,
            config,
            first_empty: HashMap::new(),
        }
    }

    /// Run one reap scan as of `now`, returning the ids reaped. The instant is a
    /// parameter, so a test drives every timing branch without real timers.
    ///
    /// The whole scan holds the host-map lock — the same lock
    /// [`HostMap::bind`] takes to attach — so no client can bind a session
    /// between the decision to reap it and its removal (no attach/reap race).
    async fn reap_once(&mut self, now: Instant) -> Vec<SessionId> {
        // Clone the host-map handle so the guard borrows this local `Arc`, not
        // `self` — leaving `self.first_empty` and `self.registry` free to mutate
        // during the scan. Same underlying mutex, so the scan still serialises
        // against `HostMap::bind` (no attach/reap race).
        let hosts = self.hosts.inner.clone();
        let mut map = hosts.lock().await;

        // Phase 1: decide, reading only the lock-free host signals.
        let mut candidates = Vec::new();
        for (id, host) in map.iter() {
            if host.is_busy() {
                // A turn is in flight: never a candidate, and do not accrue grace.
                self.first_empty.remove(id);
                continue;
            }
            let idle_expired =
                now.saturating_duration_since(host.last_active()) >= self.config.idle;
            let attached = self
                .hosts
                .served(*id)
                .map_or_else(|| host.subscriber_count(), |served| served.feed.clients());
            let reap = if attached == 0 {
                let since = *self.first_empty.entry(*id).or_insert(now);
                now.saturating_duration_since(since) >= self.config.grace || idle_expired
            } else {
                // A client is attached: the grace window does not apply, but a
                // long-idle (zombie) session is still reaped.
                self.first_empty.remove(id);
                idle_expired
            };
            if reap {
                candidates.push(*id);
            }
        }
        // Forget bookkeeping for ids no longer hosted.
        self.first_empty.retain(|id, _| map.contains_key(id));

        // Phase 2: persist-then-remove, each guarded by a non-blocking try_lock so
        // a turn that started after the busy check above is still never reaped.
        let mut reaped = Vec::new();
        for id in candidates {
            let Some(handle) = self.registry.get(id).await else {
                // Already gone from the registry: drop the stale host entry.
                map.remove(&id);
                self.first_empty.remove(&id);
                continue;
            };
            // A turn that started after the busy check above still holds this
            // lock, so a failed `try_lock` leaves the session for the next scan
            // rather than racing the turn.
            let Ok(mut runtime) = handle.try_lock() else {
                continue;
            };
            // Input admitted after the busy check above (it may not hold the
            // runtime yet) keeps the session; otherwise stop admitting, so
            // nothing can start on a session that is going away.
            if let Some(host) = map.get(&id) {
                if !host.close_admission() {
                    continue;
                }
                // Persist the event log (and any interventions still waiting
                // for the runtime) before the session disappears.
                host.flush_interventions(&mut runtime);
            }
            runtime.close();
            drop(runtime);
            self.registry.remove(id).await;
            map.remove(&id);
            self.hosts.close_served(id, "closed after inactivity");
            self.first_empty.remove(&id);
            reaped.push(id);
        }
        reaped
    }

    /// Scan on a fixed interval until `shutdown` fires.
    async fn run(mut self, shutdown: CancellationToken) {
        let mut ticker = tokio::time::interval(self.config.tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // `interval`'s first tick is immediate; consume it so a just-started
        // server does not scan before any session can exist.
        ticker.tick().await;
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                _ = ticker.tick() => {
                    let reaped = self.reap_once(Instant::now()).await;
                    if !reaped.is_empty() {
                        tracing::debug!(
                            "localpilot server reaped {} idle/detached session(s)",
                            reaped.len()
                        );
                    }
                }
            }
        }
    }
}

/// Persist and drop every remaining session at shutdown. The accept loop has
/// already stopped, so no new turn can start; this waits out any in-flight turn
/// (locking each runtime), records its `SessionClosed`, and clears the registry
/// and host map so the endpoint is released with nothing left in memory.
async fn close_all_sessions(registry: &SessionRegistry, hosts: &HostMap) {
    for id in registry.list().await {
        hosts.close_served(id, "the server is shutting down");
        if let Some(handle) = registry.get(id).await {
            handle.lock().await.close();
        }
        registry.remove(id).await;
    }
    hosts.inner.lock().await.clear();
}

// --- The per-connection serve loop ------------------------------------------

async fn serve_connection<R, W, F>(
    read: R,
    mut write: W,
    registry: &SessionRegistry,
    hosts: &HostMap,
    factory: &F,
    store: &Store,
    profile: Profile,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: ConnectionFactory,
{
    let mut reader = JsonRecordReader::new(read);
    let model = factory.model().to_string();
    let profile_label = profile_label(profile).to_string();

    // The first record must be an `attach`.
    let Some(first) = reader.next().await? else {
        return Ok(()); // client closed before attaching
    };
    let record: ClientRecord = match serde_json::from_value(first) {
        Ok(record) => record,
        Err(error) => {
            emit(
                &mut write,
                None,
                ServerEvent::Error {
                    message: malformed(&error),
                },
            )
            .await?;
            return Ok(());
        }
    };
    let reply_id = record.id;
    if record.v > RPC_PROTOCOL_VERSION {
        emit(
            &mut write,
            reply_id,
            ServerEvent::Error {
                message: unsupported_version(record.v),
            },
        )
        .await?;
        return Ok(());
    }
    // The first record decides what this connection is, once: the local
    // machine, the gateway's control channel, or a paired device. A device
    // connection that fails any check is refused and closed; it never becomes
    // local.
    let (principal, lease, target, capabilities, since) = match record.command {
        ClientCommand::Attach {
            target,
            capabilities,
            since,
        } => (Principal::Local, None, target, capabilities, since),
        ClientCommand::GatewayRegister => {
            return serve_gateway_control(&mut reader, &mut write, hosts, reply_id).await;
        }
        ClientCommand::DeviceAttach {
            capability,
            credential,
            target,
            capabilities,
            since,
        } => match admit_device(hosts, capability.expose(), credential.expose(), &target).await {
            Ok((principal, lease)) => (principal, Some(lease), target, capabilities, since),
            Err(message) => {
                emit(&mut write, reply_id, ServerEvent::Error { message }).await?;
                return Ok(());
            }
        },
        ClientCommand::PairBegin {
            name,
            scope,
            this_workspace_only,
        } => {
            // A local connection (same OS user over the owner-only pipe).
            let event = match hosts.pairings.begin(&name, scope, this_workspace_only) {
                Ok(code) => ServerEvent::PairingCode {
                    code: localpilot_rpc::WireSecret::new(code.expose()),
                    expires_in_secs: PAIRING_TTL.as_secs(),
                },
                Err(error) => ServerEvent::Error {
                    message: error.to_string(),
                },
            };
            emit(&mut write, reply_id, event).await?;
            return Ok(());
        }
        ClientCommand::PairExchange {
            capability,
            pairing,
        } => {
            let event = match exchange_pairing(hosts, capability.expose(), pairing.expose()).await {
                Ok(event) => event,
                Err(message) => ServerEvent::Error { message },
            };
            emit(&mut write, reply_id, event).await?;
            return Ok(());
        }
        _ => {
            emit(
                &mut write,
                reply_id,
                ServerEvent::Error {
                    message: "the first command on a server connection must be `attach`"
                        .to_string(),
                },
            )
            .await?;
            return Ok(());
        }
    };
    let lease = lease.unwrap_or_default();
    if lease.is_cancelled() {
        emit(
            &mut write,
            reply_id.clone(),
            ServerEvent::Error {
                message: "the gateway disconnected".to_string(),
            },
        )
        .await?;
        return Ok(());
    }
    // A device's pairing can be revoked from any workspace's server or the CLI;
    // its open connection re-checks on this tick as well as on every record.
    let mut standing_tick = tokio::time::interval(Duration::from_secs(1));
    standing_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let acks = capabilities.iter().any(|name| name == CAPABILITY_ACKS);
    let resume = capabilities.iter().any(|name| name == CAPABILITY_RESUME);
    // What the principal must still hold at each point it gains access: for a
    // device, a live gateway lease and a current pairing.
    let authorized = || -> Result<(), String> {
        let Principal::Device { id } = &principal else {
            return Ok(());
        };
        if lease.is_cancelled() {
            return Err("the gateway disconnected".to_string());
        }
        hosts
            .devices
            .standing(id)
            .map(|_| ())
            .map_err(|_| "this device is no longer allowed".to_string())
    };
    // A write to the client under `while_authorized`: access lost drops the
    // connection without a closing record.
    macro_rules! guarded {
        ($work:expr) => {
            match while_authorized(&lease, &authorized, $work).await {
                Some(result) => result?,
                None => return Ok(()),
            }
        };
    }
    let bound = hosts
        .bind(target, registry, factory, store, &authorized)
        .await;
    let (session_id, host, served) = match bound {
        Ok(bound) => bound,
        Err(error) => {
            emit(
                &mut write,
                reply_id,
                ServerEvent::Error {
                    message: error.to_string(),
                },
            )
            .await?;
            return Ok(());
        }
    };

    // Attach to the session's feed before confirming, so a client that has
    // observed `attached` is guaranteed every later record — including a turn
    // another client drives. The stored history is read first, never under
    // the feed lock.
    let history = read_history(store, session_id, resume);
    let attachment = served
        .feed
        .attach(since.as_ref(), resume, &history, &|| admission_of(&host));
    let mut live = attachment.live;
    let _guard = attachment.guard;
    // Checked again right before anything of the session is sent.
    if let Err(message) = authorized() {
        emit(&mut write, reply_id, ServerEvent::Error { message }).await?;
        return Ok(());
    }
    let mut view = ConnectionView {
        acks,
        resume,
        floor: attachment.floor,
        shown: HashSet::new(),
    };
    // Every write until the live loop takes over is guarded: if a device's
    // access ends while a write is blocked or between writes, the connection
    // is dropped at once (no closing record is attempted on a transport that
    // is not draining). Bytes already handed to the transport cannot be taken
    // back.
    let attached = emit(
        &mut write,
        reply_id,
        ServerEvent::Attached {
            capabilities: server_capabilities(),
            instance: hosts.requests.instance().to_string(),
            session_id,
            server_version: SERVER_VERSION.to_string(),
            epoch: if resume {
                attachment.epoch
            } else {
                String::new()
            },
        },
    );
    guarded!(attached);
    let server_ctx = ServerContext { hosts, store };
    let feed_ctx = FeedContext {
        feed: &served.feed,
        store,
        session_id,
        host: &host,
    };
    // The replay, snapshot or closure goes through the same delivery as live
    // records, so a replayed resync or closure is handled the same way, each
    // under the same guard.
    for (seq, event) in attachment.initial {
        if guarded!(deliver(&mut write, &mut view, &feed_ctx, seq, event)) == Flow::Stop {
            return Ok(());
        }
    }

    loop {
        tokio::select! {
            // Closing and revocation win over anything else that is ready.
            biased;
            // A revoked (or no longer granted) device stops receiving at once.
            _ = standing_tick.tick(), if matches!(principal, Principal::Device { .. }) => {
                if let Principal::Device { id } = &principal {
                    if let Err(refusal) = hosts.devices.standing(id) {
                        let reason = match refusal {
                            Refusal::Revoked => "this device was revoked",
                            Refusal::NotGranted => "this device is no longer allowed in this workspace",
                            Refusal::Unavailable => "paired devices are disabled on this server",
                            Refusal::BadCredential => "this device is no longer paired",
                        };
                        farewell(&mut write, &view, reason.to_string()).await;
                        break;
                    }
                }
            }
            // A device connection ends when its gateway's control channel does.
            () = lease.cancelled() => {
                farewell(&mut write, &view, "the gateway disconnected".to_string()).await;
                break;
            }
            // The session's numbered feed: every record, whoever drove the turn
            // and whoever answered an ask.
            record = live.recv() => match record {
                Ok((seq, event)) => {
                    if seq <= view.floor {
                        continue; // already covered by the replay or snapshot
                    }
                    // Every live write keeps watching the device's access, so
                    // a client that stops reading cannot be handed a protected
                    // record after its access ended.
                    if guarded!(deliver(&mut write, &mut view, &feed_ctx, Some(seq), event))
                        == Flow::Stop
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // This connection fell behind: never a silent skip.
                    if view.resume {
                        let (_, position) = served.feed.pending_asks();
                        if position.closed.is_none() {
                            guarded!(emit(
                                &mut write,
                                None,
                                ServerEvent::ResyncRequired {
                                    epoch: position.epoch,
                                    seq: position.head,
                                },
                            ));
                        }
                    }
                    if guarded!(resync(&mut write, &mut view, &feed_ctx)) == Flow::Stop {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            // Client commands.
            record = reader.next() => {
                let Some(value) = record? else { break }; // EOF: detach this connection
                // A device is checked against its current standing before
                // anything runs or is replayed: revoked ends the connection,
                // a command beyond its scope is refused.
                match authorize(hosts, &principal, &value) {
                    Authorized::Yes => {}
                    Authorized::Refused(id, message) => {
                        guarded!(reject(&mut write, id, acks, RejectCode::Forbidden, message));
                        continue;
                    }
                    Authorized::Gone(reason) => {
                        farewell(&mut write, &view, reason).await;
                        break;
                    }
                }
                // A mutating command with a retry key runs at most once; a
                // retry gets the original reply. Other records run as before.
                if let Some((id, key, payload)) = retry_key(&value) {
                    // A remembered reply is replayed to whichever connection
                    // retries, so keyed commands are only taken from clients
                    // that read acknowledgements. Refused before anything is
                    // reserved or run.
                    if !acks {
                        guarded!(reject(
                            &mut write,
                            id,
                            acks,
                            RejectCode::Malformed,
                            "a request key needs the `acks` capability; nothing ran".to_string(),
                        ));
                        continue;
                    }
                    let Ok(payload) = payload else {
                        guarded!(reject(
                            &mut write,
                            id,
                            acks,
                            RejectCode::Malformed,
                            "the command could not be encoded for its request key; nothing ran"
                                .to_string(),
                        ));
                        continue;
                    };
                    let stream = principal.label();
                    // The dedupe lock is taken and released inside `admit` and
                    // `complete`; it is never held across the command.
                    match hosts.requests.admit(session_id, &stream, &key, &payload) {
                        RequestAdmission::Run => {
                            let mut reply = Vec::new();
                            let outcome = handle_command(
                                value,
                                &host,
                                &session_id,
                                &model,
                                &profile_label,
                                served.asks.as_ref(),
                                acks,
                                &server_ctx,
                                &principal,
                                &mut reply,
                            )
                            .await;
                            // Record the outcome before trying to deliver it,
                            // so a lost connection never leaves it pending.
                            hosts.requests.complete(
                                session_id,
                                &stream,
                                &key,
                                reply_events(&reply),
                            );
                            guarded!(send_raw(&mut write, &reply));
                            if outcome? {
                                break;
                            }
                        }
                        RequestAdmission::Replay(reply) => {
                            // The original outcome, under this retry's id.
                            for event in reply {
                                guarded!(emit(&mut write, id.clone(), event));
                            }
                        }
                        RequestAdmission::Refuse(code, message) => {
                            guarded!(reject(&mut write, id, acks, code, message));
                        }
                    }
                    continue;
                }
                // The command runs to completion whatever happens to the
                // transport; only delivering its reply is guarded.
                let mut reply = Vec::new();
                let stop = handle_command(
                    value,
                    &host,
                    &session_id,
                    &model,
                    &profile_label,
                    served.asks.as_ref(),
                    acks,
                    &server_ctx,
                    &principal,
                    &mut reply,
                )
                .await;
                guarded!(send_raw(&mut write, &reply));
                if stop? {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Which feed records one connection is sent, and what it has been shown.
struct ConnectionView {
    acks: bool,
    resume: bool,
    /// Live records at or below this feed position are already covered.
    floor: u64,
    /// Asks this connection shows as pending.
    shown: HashSet<String>,
}

impl ConnectionView {
    /// Whether a record type is one this connection declared it can read.
    fn visible(&self, event: &ServerEvent) -> bool {
        match event {
            ServerEvent::TurnStarted
            | ServerEvent::Snapshot { .. }
            | ServerEvent::ResyncRequired { .. }
            | ServerEvent::SessionClosed { .. } => self.resume,
            ServerEvent::PermissionResolved { .. } | ServerEvent::AsksResync { .. } => {
                self.acks || self.resume
            }
            _ => true,
        }
    }

    fn note(&mut self, event: &ServerEvent) {
        match event {
            ServerEvent::PermissionAsk { ask_id, .. } => {
                self.shown.insert(ask_id.clone());
            }
            ServerEvent::PermissionResolved { ask_id, .. } => {
                self.shown.remove(ask_id);
            }
            ServerEvent::Snapshot { asks, .. } | ServerEvent::AsksResync { asks } => {
                self.shown = asks.iter().map(|ask| ask.ask_id.clone()).collect();
            }
            _ => {}
        }
    }
}

/// The retry key of a mutating command, with its correlation id and the
/// canonical bytes of the command it binds. `None` for records without a key, for commands that
/// change nothing, and for records that do not parse or use a newer protocol
/// version (those are refused as usual).
/// A correlation id, a request key, and the canonical bytes of the command it
/// binds (or the encoding error, which is refused).
type RetryKey = (
    Option<String>,
    localpilot_rpc::RequestKey,
    serde_json::Result<Vec<u8>>,
);

fn retry_key(value: &serde_json::Value) -> Option<RetryKey> {
    let record: ClientRecord = serde_json::from_value(value.clone()).ok()?;
    let key = record.request?;
    let mutating = matches!(
        record.command,
        ClientCommand::Prompt { .. }
            | ClientCommand::Cancel
            | ClientCommand::PermissionReply { .. }
            | ClientCommand::RunCommand { .. }
    );
    if !mutating || record.v > RPC_PROTOCOL_VERSION {
        return None;
    }
    Some((record.id, key, canonical_payload(&record.command)))
}

/// The events of the reply records a command wrote, for replaying them later
/// under another correlation id.
fn reply_events(reply: &[u8]) -> Vec<ServerEvent> {
    String::from_utf8_lossy(reply)
        .lines()
        .filter_map(|line| serde_json::from_str::<ServerRecord>(line).ok())
        .map(|record| record.event)
        .collect()
}

/// Serve the gateway's control channel: issue a capability valid while this
/// connection stays open, accept nothing else, and revoke the capability (which
/// closes every device connection bound to it) when the channel ends.
async fn serve_gateway_control<R, W>(
    reader: &mut JsonRecordReader<R>,
    write: &mut W,
    hosts: &HostMap,
    reply_id: Option<String>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let lease = match hosts.gateways.register() {
        Ok(lease) => lease,
        Err(error) => {
            emit(
                write,
                reply_id,
                ServerEvent::Error {
                    message: error.to_string(),
                },
            )
            .await?;
            return Ok(());
        }
    };
    hosts.devices.audit("gateway_registered", None, "");
    emit(
        write,
        reply_id,
        ServerEvent::GatewayRegistered {
            capability: localpilot_rpc::WireSecret::new(lease.capability.expose()),
        },
    )
    .await?;
    while let Some(value) = reader.next().await? {
        let record: Option<ClientRecord> = serde_json::from_value(value).ok();
        let id = record.as_ref().and_then(|record| record.id.clone());
        if matches!(
            record.map(|record| record.command),
            Some(ClientCommand::Shutdown)
        ) {
            emit(write, id, ServerEvent::Closed).await?;
            break;
        }
        emit(
            write,
            id,
            ServerEvent::Rejected {
                code: RejectCode::Forbidden,
                message: "this is a gateway control channel; it carries no session commands"
                    .to_string(),
            },
        )
        .await?;
    }
    drop(lease);
    hosts.devices.audit("gateway_closed", None, "");
    Ok(())
}

/// Check a device attach: a live gateway capability, a valid credential, and
/// enough scope for the attach target. Returns the device principal and the
/// gateway lease the connection ends with.
async fn admit_device(
    hosts: &HostMap,
    capability: &str,
    credential: &str,
    target: &AttachTarget,
) -> Result<(Principal, CancellationToken), String> {
    let lease = hosts
        .gateways
        .lease(capability)
        .ok_or_else(|| "device connections must come through a registered gateway".to_string())?;
    let _slot = wait_for_slot(hosts, &lease).await?;
    let key = presented_device_id(credential).unwrap_or_default();
    // Verified only while the gateway's capability is still live.
    let verified = hosts
        .gateways
        .while_live(capability, |_| hosts.devices.verify(credential))
        .ok_or_else(|| "the gateway disconnected".to_string())?;
    let standing = match verified {
        Ok(standing) => {
            hosts.delays.succeeded(&key);
            hosts.devices.audit("attached", Some(&standing.id), "");
            standing
        }
        Err(refusal) => {
            hosts.devices.audit(
                "refused",
                (!key.is_empty()).then_some(key.as_str()),
                &format!("{refusal:?}"),
            );
            // A wrong credential waits longer each time; a valid one never
            // does.
            tokio::time::sleep(hosts.delays.failed(&key)).await;
            return Err(match refusal {
                Refusal::Unavailable => "paired devices are disabled on this server".to_string(),
                Refusal::BadCredential | Refusal::Revoked | Refusal::NotGranted => {
                    "this device is not paired with this workspace".to_string()
                }
            });
        }
    };
    if standing.scope < target.requirement() {
        return Err("this device may watch existing sessions but not open new ones".to_string());
    }
    Ok((Principal::Device { id: standing.id }, lease))
}

/// Wait for a handshake slot, giving up if the gateway's control channel
/// closes meanwhile.
async fn wait_for_slot<'a>(
    hosts: &'a HostMap,
    lease: &CancellationToken,
) -> Result<tokio::sync::SemaphorePermit<'a>, String> {
    tokio::select! {
        biased;
        () = lease.cancelled() => Err("the gateway disconnected".to_string()),
        slot = hosts.handshakes.0.acquire() => {
            slot.map_err(|_| "the server is shutting down".to_string())
        }
    }
}

/// Exchange a pairing code, arriving through a registered gateway, for a
/// device credential.
async fn exchange_pairing(
    hosts: &HostMap,
    capability: &str,
    pairing: &str,
) -> Result<ServerEvent, String> {
    let lease = hosts
        .gateways
        .lease(capability)
        .ok_or_else(|| "pairing must come through a registered gateway".to_string())?;
    let _slot = wait_for_slot(hosts, &lease).await?;
    // The device is registered only while the gateway's capability is live: a
    // control channel that closed while this waited creates nothing.
    let exchanged = hosts
        .gateways
        .while_live(capability, |_| {
            hosts.pairings.exchange(pairing, &hosts.devices)
        })
        .ok_or_else(|| "the gateway disconnected".to_string())?;
    match exchanged {
        Ok((standing, credential)) => {
            hosts.devices.audit(
                "paired",
                Some(&standing.id),
                &format!("{:?}", standing.scope),
            );
            Ok(ServerEvent::DevicePaired {
                device_id: standing.id,
                credential: localpilot_rpc::WireSecret::new(credential.expose()),
                scope: standing.scope,
            })
        }
        Err(PairingRefusal::Invalid) => {
            hosts
                .devices
                .audit("pairing_refused", None, "invalid or expired code");
            let key = format!("pairing:{}", pairing.split('.').next().unwrap_or_default());
            tokio::time::sleep(hosts.delays.failed(&key)).await;
            Err("this pairing code is not valid (it may have expired or been used)".to_string())
        }
        Err(PairingRefusal::Registry(reason)) => Err(reason),
    }
}

/// Start pairing a device from this computer: ask the workspace's server for
/// a single-use code and show it, as a QR code of `<url>#pair=<code>` when the
/// address the phone will use is known.
pub async fn pair(
    name: &str,
    scope: localpilot_rpc::Scope,
    this_workspace_only: bool,
    url: Option<&str>,
) -> anyhow::Result<()> {
    // Checked before anything is asked of the server: the QR code must point
    // only at a trusted HTTPS origin, carrying nothing but the pairing code.
    let origin = url.map(pairing_origin).transpose()?;
    // Say what the pairing will grant before creating the code.
    let reach = if this_workspace_only {
        format!(
            "only this workspace ({})",
            crate::server_devices::workspace_id(&std::env::current_dir()?)
        )
    } else {
        "every workspace you serve as this user, now and in the future \
         (pass --this-workspace-only to limit it to this one)"
            .to_string()
    };
    let can = match scope {
        localpilot_rpc::Scope::Observe => "watch sessions",
        localpilot_rpc::Scope::Control => {
            "watch sessions, prompt, answer permission asks, cancel and run remote commands"
        }
    };
    eprintln!("Pairing \"{name}\": it will be able to {can} in {reach}.");
    let cwd = std::env::current_dir()?;
    let endpoint = Endpoint::resolve(&cwd)?;
    let conn = match localpilot_server::connect(&endpoint).await {
        Ok(conn) => conn,
        Err(TransportError::NotRunning) => anyhow::bail!(
            "no localpilot server is running for this workspace ({}). Start one with \
             `localpilot serve` first.",
            endpoint.display()
        ),
        Err(error) => return Err(error.into()),
    };
    let (read, mut write) = tokio::io::split(conn);
    let mut reader = JsonRecordReader::new(read);
    send_command(
        &mut write,
        ClientCommand::PairBegin {
            name: name.to_string(),
            scope,
            this_workspace_only,
        },
    )
    .await?;
    let Some(value) = reader.next().await? else {
        anyhow::bail!("the server closed the connection without a pairing code");
    };
    let record: ServerRecord = serde_json::from_value(value)?;
    let (code, expires_in_secs) = match record.event {
        ServerEvent::PairingCode {
            code,
            expires_in_secs,
        } => (code.expose().to_string(), expires_in_secs),
        ServerEvent::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected reply from the server: {other:?}"),
    };
    let mut out = std::io::stdout().lock();
    use std::io::Write as _;
    match origin {
        Some(origin) => {
            let link = format!("{origin}/#pair={code}");
            match qrcode::QrCode::new(link.as_bytes()) {
                Ok(qr) => writeln!(
                    out,
                    "{}",
                    qr.render::<qrcode::render::unicode::Dense1x2>()
                        .quiet_zone(true)
                        .build()
                )?,
                Err(error) => writeln!(out, "(could not draw a QR code: {error})")?,
            }
            writeln!(out, "Scan this with the phone, or open: {link}")?;
        }
        None => {
            writeln!(out, "Pairing code: {code}")?;
            writeln!(
                out,
                "Pairing only authorizes the device; it does not open any network access. \
                 The device also needs the HTTPS address of your LocalPilot gateway; pass \
                 --url <that address> to show both as a QR code."
            )?;
        }
    }
    writeln!(
        out,
        "The code works once and expires in {} minutes.",
        expires_in_secs / 60
    )?;
    Ok(())
}

/// The origin a pairing QR code may point at: `https`, a host, an optional
/// port, and nothing else (no user info, path, query or fragment that could
/// carry other data or send the device somewhere unexpected). Nothing is
/// requested from it.
fn pairing_origin(url: &str) -> anyhow::Result<String> {
    let parsed = reqwest::Url::parse(url.trim())
        .map_err(|error| anyhow::anyhow!("--url is not a valid address: {error}"))?;
    if parsed.scheme() != "https" {
        anyhow::bail!(
            "--url must be an https:// address (the gateway is reached over trusted HTTPS)"
        );
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        anyhow::bail!("--url must not contain a user name or password");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        anyhow::bail!("--url must not contain a query or fragment");
    }
    if !matches!(parsed.path(), "" | "/") {
        anyhow::bail!("--url must be the gateway's origin, without a path");
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("--url must name a host"))?;
    Ok(match parsed.port() {
        Some(port) => format!("https://{host}:{port}"),
        None => format!("https://{host}"),
    })
}

/// `localpilot devices list`: every paired device, newest first.
pub fn devices_list() -> anyhow::Result<()> {
    let registry = cli_registry()?;
    let devices = registry.list()?;
    if devices.is_empty() {
        println!("No paired devices.");
    }
    for device in devices {
        let reach = match &device.workspaces {
            None => "all workspaces".to_string(),
            Some(roots) => roots.join(", "),
        };
        let state = match device.revoked_unix {
            Some(_) => "revoked",
            None => "active",
        };
        println!(
            "{}  {:<20} {:<8} {:<8} {}",
            device.id,
            device.name,
            format!("{:?}", device.scope).to_lowercase(),
            state,
            reach
        );
    }
    Ok(())
}

/// `localpilot devices revoke <id>`: revoke a device everywhere. Every
/// workspace's server sees it at the device's next record (open connections
/// within a second).
pub fn devices_revoke(id: &str) -> anyhow::Result<()> {
    let registry = cli_registry()?;
    if registry.revoke(id)? {
        registry.audit("revoked", Some(id), "from the command line");
        println!("Revoked {id}.");
    } else {
        println!("No active device {id}.");
    }
    Ok(())
}

fn cli_registry() -> anyhow::Result<DeviceRegistry> {
    let cwd = std::env::current_dir()?;
    let registry = DeviceRegistry::for_user(&cwd);
    if let Some(reason) = registry.unavailable() {
        anyhow::bail!("the device registry cannot be used: {reason}");
    }
    Ok(registry)
}

/// Whether a principal may send a record now.
enum Authorized {
    Yes,
    /// Refused, with the record's correlation id and why.
    Refused(Option<String>, String),
    /// The device is no longer valid: end the connection.
    Gone(String),
}

/// Check one record against the principal's current standing. The local
/// machine may send anything; a device is re-read from the registry each time
/// and needs the scope the command requires.
fn authorize(hosts: &HostMap, principal: &Principal, value: &serde_json::Value) -> Authorized {
    let Principal::Device { id: device } = principal else {
        return Authorized::Yes;
    };
    let standing = match hosts.devices.standing(device) {
        Ok(standing) => standing,
        Err(Refusal::Revoked) => return Authorized::Gone("this device was revoked".to_string()),
        Err(Refusal::Unavailable) => {
            return Authorized::Gone("paired devices are disabled on this server".to_string())
        }
        Err(Refusal::BadCredential) => {
            return Authorized::Gone("this device is no longer paired".to_string())
        }
        Err(Refusal::NotGranted) => {
            return Authorized::Gone(
                "this device is no longer allowed in this workspace".to_string(),
            )
        }
    };
    // A record that does not parse is refused by the dispatcher as malformed;
    // it can run nothing.
    let Ok(record) = serde_json::from_value::<ClientRecord>(value.clone()) else {
        return Authorized::Yes;
    };
    match record.command.requirement() {
        localpilot_rpc::Requirement::AtLeast(scope) if standing.scope >= scope => Authorized::Yes,
        localpilot_rpc::Requirement::AtLeast(_) => Authorized::Refused(
            record.id,
            "this device may watch this session but not act on it".to_string(),
        ),
        localpilot_rpc::Requirement::LocalOnly => Authorized::Refused(
            record.id,
            "only this computer may send that command".to_string(),
        ),
    }
}

/// Describe a record that failed to parse without quoting any of it: parse
/// errors can echo the values they rejected, and a malformed record may carry
/// a credential or pairing code.
fn malformed(error: &serde_json::Error) -> String {
    let what = match error.classify() {
        serde_json::error::Category::Data => "a field is missing or has the wrong type",
        serde_json::error::Category::Syntax | serde_json::error::Category::Eof => "not valid JSON",
        serde_json::error::Category::Io => "it could not be read",
    };
    format!("malformed record: {what}")
}

/// Whether a connection keeps reading after a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// What a connection needs to reach its session's feed state.
struct FeedContext<'a> {
    feed: &'a SessionFeed,
    store: &'a Store,
    session_id: SessionId,
    host: &'a SessionHost,
}

/// The host's current admission state, for a snapshot.
fn admission_of(host: &SessionHost) -> Admission {
    Admission {
        busy: host.is_busy(),
        queued: host.queued_follow_ups(),
    }
}

/// Deliver one feed record to a connection: forward it when the connection
/// reads its type, resynchronise on `resync_required`, and stop after the
/// session closes.
async fn deliver<W: AsyncWrite + Unpin>(
    write: &mut W,
    view: &mut ConnectionView,
    ctx: &FeedContext<'_>,
    seq: Option<u64>,
    event: ServerEvent,
) -> std::io::Result<Flow> {
    match event {
        ServerEvent::SessionClosed { reason } => {
            emit_closure(write, view, seq, reason).await?;
            Ok(Flow::Stop)
        }
        ServerEvent::ResyncRequired { .. } => {
            // The server lost records; everyone reconciles.
            if view.resume {
                emit_at(write, None, seq, event).await?;
            }
            resync(write, view, ctx).await
        }
        event => {
            if view.visible(&event) {
                view.note(&event);
                emit_at(write, None, seq, event).await?;
            }
            Ok(Flow::Continue)
        }
    }
}

/// Write already-encoded records to the client.
async fn send_raw<W: AsyncWrite + Unpin>(write: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    write.write_all(bytes).await?;
    write.flush().await
}

/// Best-effort closing record for a connection whose access just ended: the
/// connection is dropped either way, and never waits on a client that is not
/// reading.
async fn farewell<W: AsyncWrite + Unpin>(write: &mut W, view: &ConnectionView, reason: String) {
    let _ = tokio::time::timeout(ACCESS_POLL, emit_closure(write, view, None, reason)).await;
}

/// Tell a connection its session closed, in the record type it reads.
async fn emit_closure<W: AsyncWrite + Unpin>(
    write: &mut W,
    view: &ConnectionView,
    seq: Option<u64>,
    reason: String,
) -> std::io::Result<()> {
    let closing = if view.resume {
        ServerEvent::SessionClosed { reason }
    } else {
        ServerEvent::Closed
    };
    emit_at(write, None, seq, closing).await
}

/// Bring a connection that missed records back to the feed's state: a fresh
/// snapshot for `resume` clients, the pending set for `acks` clients, and any
/// not-yet-shown pending ask for everyone else. A closed feed ends the
/// connection instead.
async fn resync<W: AsyncWrite + Unpin>(
    write: &mut W,
    view: &mut ConnectionView,
    ctx: &FeedContext<'_>,
) -> std::io::Result<Flow> {
    if view.resume {
        let history = read_history(ctx.store, ctx.session_id, true);
        let (snapshot, head) = ctx.feed.snapshot(&history, &|| admission_of(ctx.host));
        if let ServerEvent::SessionClosed { reason } = snapshot {
            emit_closure(write, view, None, reason).await?;
            return Ok(Flow::Stop);
        }
        view.floor = view.floor.max(head);
        view.note(&snapshot);
        emit(write, None, snapshot).await?;
        return Ok(Flow::Continue);
    }
    let (asks, position) = ctx.feed.pending_asks();
    if let Some(reason) = position.closed {
        emit_closure(write, view, None, reason).await?;
        return Ok(Flow::Stop);
    }
    view.floor = view.floor.max(position.head);
    if view.acks {
        let resync = ServerEvent::AsksResync { asks };
        view.note(&resync);
        emit(write, None, resync).await?;
        return Ok(Flow::Continue);
    }
    let pending: HashSet<String> = asks.iter().map(|ask| ask.ask_id.clone()).collect();
    view.shown.retain(|id| pending.contains(id));
    for ask in asks {
        if view.shown.insert(ask.ask_id.clone()) {
            emit(
                write,
                None,
                ServerEvent::PermissionAsk {
                    ask_id: ask.ask_id,
                    tool: ask.tool,
                    detail: ask.detail,
                    risk: ask.risk,
                    expires_at_ms: ask.expires_at_ms,
                },
            )
            .await?;
        }
    }
    Ok(Flow::Continue)
}

/// The stored transcript tail a snapshot carries (empty unless the client
/// reads snapshots). A store error leaves it empty and marked truncated.
fn read_history(
    store: &Store,
    session_id: SessionId,
    wanted: bool,
) -> (Vec<localpilot_rpc::HistoryMessage>, bool) {
    if !wanted {
        return (Vec::new(), false);
    }
    match store.read_transcript(session_id) {
        Ok(messages) => stored_history(&messages),
        Err(_) => (Vec::new(), true),
    }
}

fn unsupported_version(v: u32) -> String {
    format!(
        "protocol version {v} is newer than this server supports (up to {RPC_PROTOCOL_VERSION})"
    )
}

/// Route one decoded client command. Returns `true` when the connection should
/// detach (a `shutdown`).
///
/// A connection that declared the `acks` capability gets an explicit
/// `accepted` / `queued` / `rejected` reply to every mutating command; an older
/// client gets the records it always got (`error` on refusal, `queued` for a
/// steer or follow-up, nothing on success).
#[allow(clippy::too_many_arguments)] // the loop genuinely threads these
async fn handle_command<W: AsyncWrite + Unpin>(
    value: serde_json::Value,
    host: &Arc<SessionHost>,
    session_id: &SessionId,
    model: &str,
    profile_label: &str,
    asks: Option<&AskRegistry>,
    acks: bool,
    server: &ServerContext<'_>,
    principal: &Principal,
    write: &mut W,
) -> anyhow::Result<bool> {
    // Keep the correlation id of a record that fails to parse, when it has a
    // usable one, so the client can match the rejection to its command.
    let raw_id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let record: ClientRecord = match serde_json::from_value(value) {
        Ok(record) => record,
        Err(error) => {
            reject(
                write,
                raw_id,
                acks,
                RejectCode::Malformed,
                malformed(&error),
            )
            .await?;
            return Ok(false);
        }
    };
    let id = record.id;
    if record.v > RPC_PROTOCOL_VERSION {
        reject(
            write,
            id,
            acks,
            RejectCode::UnsupportedVersion,
            unsupported_version(record.v),
        )
        .await?;
        return Ok(false);
    }
    match record.command {
        ClientCommand::Hello => {
            emit(
                write,
                id,
                ServerEvent::Hello {
                    protocol_version: RPC_PROTOCOL_VERSION,
                    session_id: session_id.to_string(),
                    model: model.to_string(),
                    capabilities: server_capabilities(),
                },
            )
            .await?;
        }
        ClientCommand::Status => {
            let outstanding = asks
                .map(AskRegistry::outstanding_detail)
                .unwrap_or_default();
            emit(
                write,
                id,
                ServerEvent::Status {
                    session_id: session_id.to_string(),
                    model: model.to_string(),
                    profile: profile_label.to_string(),
                    busy: host.is_busy(),
                    pending_asks: outstanding.iter().map(|ask| ask.ask_id.clone()).collect(),
                    next_step: None,
                    asks: outstanding.iter().map(PendingAsk::detail).collect(),
                },
            )
            .await?;
        }
        ClientCommand::Prompt { text, disposition } => match disposition {
            InputDisposition::Immediate | InputDisposition::FollowUp => {
                let follow_up = disposition == InputDisposition::FollowUp;
                // Drive on its own task; events (including the terminal
                // `stopped`) reach this and every other attached connection
                // via the host broadcast. A turn keeps running for other
                // clients if this connection detaches.
                let origin =
                    matches!(principal, Principal::Device { .. }).then(|| principal.label());
                match host.submit_from(text, follow_up, origin) {
                    Admit::Started => accept(write, id, acks, None).await?,
                    Admit::Queued => {
                        emit(
                            write,
                            id,
                            ServerEvent::Queued {
                                disposition: InputDisposition::FollowUp,
                            },
                        )
                        .await?;
                    }
                    Admit::Busy => {
                        reject(
                            write,
                            id,
                            acks,
                            RejectCode::Busy,
                            "a turn is already running on this session; cancel it first, \
                             send a steer disposition to inject into it, or send a follow-up"
                                .to_string(),
                        )
                        .await?;
                    }
                    Admit::QueueFull => {
                        reject(
                            write,
                            id,
                            acks,
                            RejectCode::QueueFull,
                            "too many follow-ups are already waiting for this session".to_string(),
                        )
                        .await?;
                    }
                    Admit::Closed => {
                        reject(
                            write,
                            id,
                            acks,
                            RejectCode::SessionClosed,
                            "this session was closed after inactivity; attach again to resume it"
                                .to_string(),
                        )
                        .await?;
                    }
                }
            }
            InputDisposition::Steer => {
                host.record_intervention("steer", text.clone(), &principal.label());
                host.steer(text);
                emit(
                    write,
                    id,
                    ServerEvent::Queued {
                        disposition: InputDisposition::Steer,
                    },
                )
                .await?;
            }
        },
        ClientCommand::Cancel => {
            let cancelled = host.cancel_all();
            if cancelled.turn || cancelled.dropped_follow_ups > 0 {
                host.record_intervention(
                    "cancel",
                    format!(
                        "dropped {} queued follow-up(s)",
                        cancelled.dropped_follow_ups
                    ),
                    &principal.label(),
                );
                accept(write, id, acks, Some(cancelled.dropped_follow_ups)).await?;
            } else {
                reject(
                    write,
                    id,
                    acks,
                    RejectCode::NoTurnRunning,
                    "no turn is running".to_string(),
                )
                .await?;
            }
        }
        ClientCommand::PermissionReply { ask_id, allow } => {
            let Some(asks) = asks else {
                reject(
                    write,
                    id,
                    acks,
                    RejectCode::UnknownAsk,
                    format!("unknown ask id {ask_id}"),
                )
                .await?;
                return Ok(false);
            };
            let context = asks
                .outstanding_detail()
                .into_iter()
                .find(|ask| ask.ask_id == ask_id)
                .map(|ask| format!("{}: {}", ask.tool, ask.detail));
            match asks.reply(&ask_id, allow, principal.clone()) {
                Ok(_) => {
                    host.record_intervention(
                        if allow { "allow" } else { "deny" },
                        context.unwrap_or_else(|| ask_id.clone()),
                        &principal.label(),
                    );
                    accept(write, id, acks, None).await?;
                }
                Err(refusal) => {
                    let (code, message) = match refusal {
                        ReplyRefusal::AlreadyResolved(outcome) => (
                            RejectCode::AlreadyResolved,
                            format!("ask {ask_id} was already resolved ({outcome:?})"),
                        ),
                        ReplyRefusal::Expired => (
                            RejectCode::Expired,
                            format!("ask {ask_id} expired before the reply arrived"),
                        ),
                        ReplyRefusal::Unknown => {
                            (RejectCode::UnknownAsk, format!("unknown ask id {ask_id}"))
                        }
                    };
                    reject(write, id, acks, code, message).await?;
                }
            }
        }
        ClientCommand::Shutdown => {
            emit(write, id, ServerEvent::Closed).await?;
            return Ok(true);
        }
        ClientCommand::RevokeSelf => {
            let Principal::Device { id: device } = principal else {
                reject(
                    write,
                    id,
                    acks,
                    RejectCode::Forbidden,
                    "only a paired device can revoke itself".to_string(),
                )
                .await?;
                return Ok(false);
            };
            return match server.hosts.devices.revoke(device) {
                Ok(_) => {
                    server
                        .hosts
                        .devices
                        .audit("revoked", Some(device), "by the device itself");
                    accept(write, id, acks, None).await?;
                    Ok(true)
                }
                Err(error) => {
                    emit(
                        write,
                        id,
                        ServerEvent::Error {
                            message: error.to_string(),
                        },
                    )
                    .await?;
                    Ok(false)
                }
            };
        }
        ClientCommand::Commands => {
            emit(
                write,
                id,
                ServerEvent::Commands {
                    items: remote_commands(),
                },
            )
            .await?;
        }
        ClientCommand::Sessions => {
            let items = list_sessions(server).await;
            emit(write, id, ServerEvent::Sessions { items }).await?;
        }
        ClientCommand::RunCommand { name, args } => {
            run_command(
                write, id, host, session_id, server, principal, acks, &name, &args,
            )
            .await?;
        }
        ClientCommand::Attach { .. } => {
            reject(
                write,
                id,
                acks,
                RejectCode::AlreadyAttached,
                "already attached; a connection binds its session once".to_string(),
            )
            .await?;
        }
        // `ClientCommand` is `#[non_exhaustive]`: an unknown command is a typed
        // error, never a panic.
        _ => {
            reject(
                write,
                id,
                acks,
                RejectCode::UnsupportedCommand,
                "this server build does not understand that command".to_string(),
            )
            .await?;
        }
    }
    Ok(false)
}

/// What `handle_command` needs from the server beyond the session itself.
struct ServerContext<'a> {
    hosts: &'a HostMap,
    store: &'a Store,
}

/// Sessions listed by `sessions`, at most.
const SESSIONS_LISTED: usize = 50;

/// The opt-ins a served session implements, for `attached` and `hello`.
fn server_capabilities() -> Vec<String> {
    localpilot_rpc::SERVER_CAPABILITIES
        .iter()
        .map(|name| (*name).to_string())
        .collect()
}

/// The slash commands a remote client may run, with their argument shape and
/// the device scope they need.
fn remote_commands() -> Vec<localpilot_rpc::CommandInfo> {
    localpilot_slash::specs_for(localpilot_slash::Host::Remote)
        .into_iter()
        .map(|(name, description)| {
            let args = match localpilot_slash::lookup(name).map(|spelling| spelling.args) {
                Some(localpilot_slash::ArgSpec::Required) => "required",
                Some(localpilot_slash::ArgSpec::Optional) => "optional",
                _ => "none",
            };
            localpilot_rpc::CommandInfo {
                name: name.to_string(),
                description: description.to_string(),
                args: args.to_string(),
                scope: "control".to_string(),
            }
        })
        .collect()
}

/// The workspace's sessions: every live one first (never pushed out by
/// history), then stored ones by most recent update, each id once, at most
/// [`SESSIONS_LISTED`] in all (live ones are always included).
async fn list_sessions(server: &ServerContext<'_>) -> Vec<localpilot_rpc::SessionInfo> {
    let live: Vec<SessionId> = server.hosts.inner.lock().await.keys().copied().collect();
    let mut stored = server.store.list_sessions().unwrap_or_default();
    stored.sort_by(|a, b| b.updated_unix.cmp(&a.updated_unix));
    let mut items: Vec<localpilot_rpc::SessionInfo> = live
        .iter()
        .map(|id| {
            let entry = stored.iter().find(|entry| entry.id == *id);
            localpilot_rpc::SessionInfo {
                id: *id,
                name: entry.and_then(|entry| entry.name.clone()),
                live: true,
                message_count: entry.map_or(0, |entry| entry.message_count),
                updated_unix: entry.map_or(0, |entry| entry.updated_unix),
            }
        })
        .collect();
    for entry in stored {
        if items.len() >= SESSIONS_LISTED.max(live.len()) {
            break;
        }
        if live.contains(&entry.id) {
            continue;
        }
        items.push(localpilot_rpc::SessionInfo {
            id: entry.id,
            name: entry.name,
            live: false,
            message_count: entry.message_count,
            updated_unix: entry.updated_unix,
        });
    }
    items
}

/// Run one advertised slash command against the session. It needs the
/// session to itself, so it runs only when no turn is running or admitted;
/// an unknown command or bad arguments are refused and never become a prompt.
#[allow(clippy::too_many_arguments)] // one command's whole context
async fn run_command<W: AsyncWrite + Unpin>(
    write: &mut W,
    id: Option<String>,
    host: &Arc<SessionHost>,
    session_id: &SessionId,
    server: &ServerContext<'_>,
    principal: &Principal,
    acks: bool,
    name: &str,
    args: &str,
) -> std::io::Result<()> {
    // The name must be exactly one advertised command: arguments travel only in
    // `args`, so a name like "compact force" is refused rather than parsed.
    let advertised = localpilot_slash::specs_for(localpilot_slash::Host::Remote)
        .iter()
        .any(|(spelling, _)| *spelling == name);
    if !advertised {
        return reject(
            write,
            id,
            acks,
            RejectCode::UnknownCommand,
            format!(
                "`{name}` is not a command a remote client can run; send `commands` to list them"
            ),
        )
        .await;
    }
    let line = if args.trim().is_empty() {
        format!("/{name}")
    } else {
        format!("/{name} {}", args.trim())
    };
    let action = localpilot_slash::parse_slash_for(localpilot_slash::Host::Remote, &line);
    let action = match action {
        Some(localpilot_slash::SlashAction::Unknown(_)) | None => {
            return reject(
                write,
                id,
                acks,
                RejectCode::UnknownCommand,
                format!("`{name}` is not a command a remote client can run; send `commands` to list them"),
            )
            .await;
        }
        Some(localpilot_slash::SlashAction::Invalid { reason, .. }) => {
            return reject(write, id, acks, RejectCode::InvalidArguments, reason).await;
        }
        Some(action) => action,
    };
    let Some(reservation) = host.reserve_command() else {
        return reject(
            write,
            id,
            acks,
            RejectCode::Busy,
            "the session is busy; run the command when no turn is running".to_string(),
        )
        .await;
    };
    let outcome = {
        let mut runtime = reservation.runtime().await;
        let outcome = match &action {
            localpilot_slash::SlashAction::Compact { force } => {
                let result = if *force {
                    runtime.compact_conversation_force().await
                } else {
                    runtime.compact_conversation().await
                };
                Ok(if result.compacted {
                    format!(
                        "compacted the context to {} of {} tokens",
                        result.context_used, result.context_limit
                    )
                } else {
                    format!(
                        "nothing to compact: the context uses {} of {} tokens",
                        result.context_used, result.context_limit
                    )
                })
            }
            localpilot_slash::SlashAction::Clear => {
                runtime.clear_conversation();
                Ok(
                    "the model starts on an empty context; the stored transcript is kept"
                        .to_string(),
                )
            }
            localpilot_slash::SlashAction::NameSession(new_name) => server
                .store
                .set_session_name(*session_id, new_name)
                .map(|()| format!("named this session \"{}\"", new_name.trim()))
                .map_err(|error| (RejectCode::InvalidArguments, error.to_string())),
            localpilot_slash::SlashAction::SetEffort(level) => {
                match localpilot_llm::ReasoningEffort::parse(level.trim()) {
                    Some(effort) => {
                        runtime.set_reasoning_effort(Some(effort));
                        Ok(format!("reasoning effort set to {}", level.trim()))
                    }
                    None => Err((
                        RejectCode::InvalidArguments,
                        "reasoning effort is one of minimal, low, medium, high".to_string(),
                    )),
                }
            }
            _ => Err((
                RejectCode::UnknownCommand,
                format!("`{name}` cannot run on a served session"),
            )),
        };
        if outcome.is_ok()
            && matches!(
                action,
                localpilot_slash::SlashAction::Compact { .. }
                    | localpilot_slash::SlashAction::Clear
            )
        {
            // New context usage reaches every client through the session's
            // own ordered stream.
            let (used, limit) = runtime.context_usage();
            host.publish(localpilot_harness::RuntimeEvent::ContextUsage { used, limit });
        }
        outcome
    };
    drop(reservation);
    match outcome {
        Ok(message) => {
            host.record_intervention("command", line.clone(), &principal.label());
            if acks {
                emit(
                    write,
                    id,
                    ServerEvent::CommandResult {
                        name: name.to_string(),
                        message,
                    },
                )
                .await?;
            }
            Ok(())
        }
        Err((code, message)) => reject(write, id, acks, code, message).await,
    }
}

/// Acknowledge a command that is being acted on now (`acks` clients only).
async fn accept<W: AsyncWrite + Unpin>(
    write: &mut W,
    id: Option<String>,
    acks: bool,
    dropped_follow_ups: Option<usize>,
) -> std::io::Result<()> {
    if acks {
        emit(write, id, ServerEvent::Accepted { dropped_follow_ups }).await?;
    }
    Ok(())
}

/// Refuse a command: a typed `rejected` for `acks` clients, the legacy `error`
/// record for everyone else.
async fn reject<W: AsyncWrite + Unpin>(
    write: &mut W,
    id: Option<String>,
    acks: bool,
    code: RejectCode,
    message: String,
) -> std::io::Result<()> {
    let event = if acks {
        ServerEvent::Rejected { code, message }
    } else {
        ServerEvent::Error { message }
    };
    emit(write, id, event).await
}

async fn emit<W: AsyncWrite + Unpin>(
    write: &mut W,
    id: Option<String>,
    event: ServerEvent,
) -> std::io::Result<()> {
    emit_at(write, id, None, event).await
}

/// Emit a record carrying its feed position.
async fn emit_at<W: AsyncWrite + Unpin>(
    write: &mut W,
    id: Option<String>,
    seq: Option<u64>,
    event: ServerEvent,
) -> std::io::Result<()> {
    let record = ServerRecord {
        seq,
        v: RPC_PROTOCOL_VERSION,
        id,
        event,
    };
    let mut line = serde_json::to_vec(&record).unwrap_or_else(|_| b"{}".to_vec());
    line.push(b'\n');
    write.write_all(&line).await?;
    write.flush().await
}

// --- `localpilot serve` -----------------------------------------------------

/// Serve this workspace's sessions on the local-IPC endpoint until Ctrl-C.
///
/// Acquires the single-owner singleton first: if a live daemon already owns the
/// endpoint, this prints that and exits cleanly rather than double-serving.
///
/// # Errors
/// Returns an error if the endpoint cannot be acquired or the session setup
/// (config/provider/MCP) fails.
pub async fn serve(
    model: Option<&str>,
    provider_id: Option<&str>,
    profile: Profile,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let endpoint = Endpoint::resolve(&cwd)?;
    let (listener, singleton) = match acquire(&endpoint).await? {
        Acquired::AlreadyRunning => {
            println!(
                "a localpilot server is already running for this workspace at {}",
                endpoint.display()
            );
            return Ok(());
        }
        Acquired::Owned {
            listener,
            singleton,
        } => (listener, singleton),
    };

    let setup = SessionSetup::resolve(model, provider_id, profile).await?;
    let store = Store::open(&cwd);
    let registry = SessionRegistry::new();
    let hosts = HostMap {
        // One registry per user, shared by every workspace's server.
        devices: Arc::new(DeviceRegistry::for_user(&cwd)),
        ..HostMap::default()
    };
    if let Some(reason) = hosts.devices.unavailable() {
        eprintln!("localpilot: paired devices are disabled until this is fixed: {reason}");
    }
    let factory = Arc::new(ServerFactory::new(setup));

    // The session reaper runs beside the accept loop, sharing its registry and
    // host map: it removes sessions whose clients have all detached (after a
    // grace period) or that have gone idle, persisting each first and never
    // touching a session with an in-flight turn.
    let reaper_shutdown = CancellationToken::new();
    let reaper = Reaper::new(registry.clone(), hosts.clone(), ReaperConfig::default());
    let reaper_task = tokio::spawn(reaper.run(reaper_shutdown.clone()));

    eprintln!(
        "localpilot server listening at {} — Ctrl-C to stop",
        endpoint.display()
    );
    accept_loop(&listener, &registry, &hosts, &factory, &store, profile).await;

    // Clean shutdown: stop the reaper, then persist and drop every remaining
    // session, then drop the listener (Unix unlinks the socket) and the singleton
    // (Unix removes the lock file) to release the endpoint.
    reaper_shutdown.cancel();
    let _ = reaper_task.await;
    close_all_sessions(&registry, &hosts).await;
    drop(listener);
    drop(singleton);
    eprintln!("localpilot server stopped");
    Ok(())
}

async fn accept_loop<F: ConnectionFactory + 'static>(
    listener: &Listener,
    registry: &SessionRegistry,
    hosts: &HostMap,
    factory: &Arc<F>,
    store: &Store,
    profile: Profile,
) {
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!();
                break;
            }
            accepted = listener.accept() => match accepted {
                Ok(conn) => {
                    let registry = registry.clone();
                    let hosts = hosts.clone();
                    let factory = factory.clone();
                    let store = store.clone();
                    tokio::spawn(async move {
                        let (read, write) = tokio::io::split(conn);
                        if let Err(error) = serve_connection(
                            read,
                            write,
                            &registry,
                            &hosts,
                            factory.as_ref(),
                            &store,
                            profile,
                        )
                        .await
                        {
                            tracing::debug!("localpilot server connection ended: {error}");
                        }
                    });
                }
                Err(error) => tracing::warn!("localpilot server accept failed: {error}"),
            },
        }
    }
}

// --- `localpilot connect` ---------------------------------------------------

/// Connect to this workspace's server and relay a session over stdin/stdout.
///
/// With `auto_spawn` (the `--server` flag) a missing daemon is started first;
/// otherwise a missing daemon is a clear error suggesting `serve`.
///
/// # Errors
/// Returns an error if no server can be reached (or started) or the transport
/// fails.
pub async fn connect(resume: Option<&str>, auto_spawn: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let endpoint = Endpoint::resolve(&cwd)?;
    let conn = if auto_spawn {
        ensure_running(&endpoint).await.map_err(|error| {
            anyhow::anyhow!(
                "could not reach or start a localpilot server for this workspace: {error}"
            )
        })?
    } else {
        match localpilot_server::connect(&endpoint).await {
            Ok(conn) => conn,
            Err(TransportError::NotRunning) => anyhow::bail!(
                "no localpilot server is running for this workspace ({}). Start one with \
                 `localpilot serve` in this workspace, or pass `--server` to start one \
                 automatically.",
                endpoint.display()
            ),
            Err(error) => return Err(error.into()),
        }
    };
    let target = match resume {
        None => AttachTarget::OpenNew,
        Some(spec) => match spec.parse::<SessionId>() {
            Ok(session_id) => AttachTarget::ResumeId { session_id },
            Err(_) => AttachTarget::ResumeName {
                name: spec.to_string(),
            },
        },
    };
    run_client(conn, target).await
}

/// The thin plain-text client: attach, then relay stdin lines as prompts and
/// render incoming events to stdout. `Ctrl-C` cancels the running turn; a
/// pending permission ask is answered with `/allow` or `/deny`.
async fn run_client(conn: Conn, target: AttachTarget) -> anyhow::Result<()> {
    let (read, mut write) = tokio::io::split(conn);
    let mut reader = JsonRecordReader::new(read);
    send_command(
        &mut write,
        ClientCommand::Attach {
            since: None,
            target,
            capabilities: vec![CAPABILITY_ACKS.to_string()],
        },
    )
    .await?;

    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut last_ask: Option<String> = None;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                send_command(&mut write, ClientCommand::Cancel).await?;
            }
            line = stdin.next_line() => match line? {
                Some(line) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if trimmed == "/allow" || trimmed == "/deny" {
                        match last_ask.take() {
                            Some(ask_id) => {
                                send_command(
                                    &mut write,
                                    ClientCommand::PermissionReply { ask_id, allow: trimmed == "/allow" },
                                )
                                .await?;
                            }
                            None => eprintln!("localpilot: no pending permission ask to answer"),
                        }
                    } else {
                        send_command(
                            &mut write,
                            ClientCommand::Prompt { text: line, disposition: InputDisposition::Immediate },
                        )
                        .await?;
                    }
                }
                None => {
                    // stdin closed: ask the server to detach this connection.
                    let _ = send_command(&mut write, ClientCommand::Shutdown).await;
                    break;
                }
            },
            record = reader.next() => match record? {
                Some(value) => match serde_json::from_value::<ServerRecord>(value) {
                    Ok(record) => {
                        if render_event(record.event, &mut last_ask)? {
                            break;
                        }
                    }
                    Err(error) => {
                        eprintln!("localpilot: server sent a record this client could not read: {error}");
                    }
                },
                None => {
                    eprintln!("localpilot: server closed the connection");
                    break;
                }
            },
        }
    }
    Ok(())
}

async fn send_command<W: AsyncWrite + Unpin>(
    write: &mut W,
    command: ClientCommand,
) -> std::io::Result<()> {
    let record = ClientRecord {
        request: None,
        v: RPC_PROTOCOL_VERSION,
        id: None,
        command,
    };
    let mut line = serde_json::to_vec(&record).unwrap_or_else(|_| b"{}".to_vec());
    line.push(b'\n');
    write.write_all(&line).await?;
    write.flush().await
}

fn outcome_label(outcome: localpilot_rpc::AskOutcome) -> &'static str {
    match outcome {
        localpilot_rpc::AskOutcome::Allowed => "allowed",
        localpilot_rpc::AskOutcome::Denied => "denied",
        localpilot_rpc::AskOutcome::Expired => "expired unanswered",
        localpilot_rpc::AskOutcome::Cancelled => "cancelled with its turn",
    }
}

/// Render one server event as plain text. Session text goes to stdout; status,
/// tool, and lifecycle notes go to stderr so a piped stdout stays the answer.
/// Returns `true` when the server has closed the session.
fn render_event(event: ServerEvent, last_ask: &mut Option<String>) -> anyhow::Result<bool> {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    match event {
        ServerEvent::Attached {
            session_id,
            server_version,
            ..
        } => {
            let version = if server_version.is_empty() {
                "unknown".to_string()
            } else {
                server_version
            };
            eprintln!(
                "localpilot: attached to session {session_id} (server {version}). \
                 Type a prompt and press Enter; Ctrl-C cancels a turn."
            );
        }
        ServerEvent::Hello {
            session_id, model, ..
        } => eprintln!("localpilot: session {session_id} on {model}"),
        ServerEvent::TextDelta { text } => {
            write!(out, "{text}")?;
            out.flush()?;
        }
        ServerEvent::ReasoningDelta { .. } => {}
        ServerEvent::ToolStarted { name, .. } => eprintln!("  [tool] {name}"),
        ServerEvent::ToolFinished { name, is_error, .. } => {
            eprintln!("  [tool] {name} {}", if is_error { "failed" } else { "ok" })
        }
        ServerEvent::ToolStuck { name, count } => {
            eprintln!("  [tool] {name} stuck after {count} attempts; switching strategy");
        }
        ServerEvent::Warning { message } => eprintln!("localpilot: warning: {message}"),
        ServerEvent::Plan { steps } => {
            for step in steps {
                eprintln!("  [plan] {} — {}", step.status, step.title);
            }
        }
        ServerEvent::QuotaPaused { reset } => {
            eprintln!("localpilot: paused on provider quota until {reset}");
        }
        ServerEvent::Stopped { reason } => {
            writeln!(out)?;
            out.flush()?;
            eprintln!("localpilot: turn stopped ({reason})");
        }
        ServerEvent::Status {
            session_id,
            model,
            profile,
            busy,
            pending_asks,
            next_step,
            ..
        } => eprintln!(
            "localpilot: session {session_id} · model {model} · profile {profile} · {}{}{}",
            if busy { "busy" } else { "idle" },
            if pending_asks.is_empty() {
                String::new()
            } else {
                format!(" · {} pending ask(s)", pending_asks.len())
            },
            next_step
                .map(|step| format!(" · next: {step}"))
                .unwrap_or_default(),
        ),
        ServerEvent::PermissionAsk {
            ask_id,
            tool,
            detail,
            risk,
            ..
        } => {
            eprintln!("localpilot: permission needed — {tool} ({risk}): {detail}");
            eprintln!("           type /allow or /deny to answer.");
            *last_ask = Some(ask_id);
        }
        ServerEvent::AsksResync { asks } => {
            // The authoritative pending set after this client fell behind: the
            // ask to answer is one that is still pending, never a stale one.
            if !asks
                .iter()
                .any(|ask| last_ask.as_deref() == Some(ask.ask_id.as_str()))
            {
                *last_ask = None;
            }
            match asks.last() {
                Some(ask) => {
                    if last_ask.is_none() {
                        eprintln!(
                            "localpilot: permission needed — {} ({}): {}",
                            ask.tool, ask.risk, ask.detail
                        );
                        eprintln!("           type /allow or /deny to answer.");
                        *last_ask = Some(ask.ask_id.clone());
                    }
                }
                None => eprintln!("localpilot: no permission asks pending"),
            }
        }
        ServerEvent::PermissionResolved {
            ask_id,
            outcome,
            by,
        } => {
            if last_ask.as_deref() == Some(ask_id.as_str()) {
                *last_ask = None;
            }
            let who = by
                .map(|by| format!(" by {}", by.label()))
                .unwrap_or_default();
            eprintln!("localpilot: permission {}{who}", outcome_label(outcome));
        }
        ServerEvent::Rejected { message, .. } | ServerEvent::Error { message } => {
            eprintln!("localpilot: error: {message}");
        }
        ServerEvent::Closed => {
            eprintln!("localpilot: session closed by the server");
            return Ok(true);
        }
        // Usage / context-usage / queued / recovery carry no plain-text output,
        // and `ServerEvent` is `#[non_exhaustive]`.
        _ => {}
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use localpilot_harness::RuntimeEvent;
    use localpilot_llm::FakeProvider;
    use localpilot_sandbox::{ScriptedApprover, Workspace};
    use localpilot_store::SessionEventKind;
    use localpilot_tools::ToolRegistry;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    /// A turn and its round trips must settle well within this; exceeding it is a
    /// hang the test turns into a failure.
    const DEADLINE: Duration = Duration::from_secs(15);

    #[tokio::test]
    async fn context_server_and_worker_factories_use_the_pre_resolved_window() {
        let mut declaration = FakeProvider::new().declaration().clone();
        declaration.id = "context-test".to_owned();
        declaration.max_context_tokens = Some(131_072);
        declaration.max_output_tokens = Some(4096);
        let provider: Arc<dyn ModelProvider> =
            Arc::new(FakeProvider::new().with_declaration(declaration));
        let cwd = tempfile::tempdir().unwrap();
        let mut setup = SessionSetup::for_test(provider.clone(), "model", cwd.path().to_owned());
        setup.config = crate::context_window::tests::config_for(
            "http://context-server-test.invalid/v1",
            Some(131_072),
        );
        let resolution = crate::context_window::resolve_with_probe(
            &setup.config,
            "context-test",
            "model",
            Some(131_072),
            async {
                Some(localpilot_llm::ServerContextWindow {
                    tokens: 32768,
                    source: localpilot_llm::ContextWindowSource::ServerProps,
                })
            },
        )
        .await;
        setup.context_windows.insert(
            ("context-test".to_owned(), "model".to_owned()),
            resolution.window,
        );
        assert_eq!(
            setup.build().unwrap().runtime.context_usage().1,
            32768 - 4096
        );
        assert_eq!(
            setup
                .build_worker(&provider, "model")
                .unwrap()
                .context_usage()
                .1,
            32768 - 4096
        );
    }

    /// Builds `SessionRuntime`s over one shared temp-dir store and provider —
    /// mirrors the server crate's own test factory — with no wire approver (a
    /// bypass session never asks), so `take_asks` is `None`.
    struct FakeFactory {
        dir: Arc<tempfile::TempDir>,
        provider: Arc<FakeProvider>,
        /// Build sessions that ask the wire client (default profile, interactive,
        /// wire approver) instead of bypass sessions that never ask.
        asking: bool,
        pending: StdMutex<Option<ApproverHalves>>,
    }

    impl FakeFactory {
        fn new(provider: FakeProvider) -> Self {
            Self {
                dir: Arc::new(tempfile::tempdir().unwrap()),
                provider: Arc::new(provider),
                asking: false,
                pending: StdMutex::new(None),
            }
        }

        fn asking(provider: FakeProvider) -> Self {
            Self {
                asking: true,
                ..Self::new(provider)
            }
        }

        fn store(&self) -> Store {
            Store::open(self.dir.path())
        }
    }

    impl SessionFactory for FakeFactory {
        fn create(&self) -> Result<SessionRuntime, RegistryError> {
            let root = self.dir.path();
            let workspace =
                Workspace::new(root).map_err(|error| RegistryError::Factory(error.to_string()))?;
            let (profile, approver, interactivity): (Profile, Box<dyn Approver>, Interactivity) =
                if self.asking {
                    let (approver, ask_rx, asks) = RpcApprover::new();
                    *self.pending.lock().unwrap() = Some(ApproverHalves { ask_rx, asks });
                    (
                        Profile::Default,
                        Box::new(approver),
                        Interactivity::Interactive,
                    )
                } else {
                    (
                        Profile::Bypass,
                        Box::new(ScriptedApprover::always()),
                        Interactivity::NonInteractive,
                    )
                };
            Ok(SessionRuntime::new(
                self.provider.clone(),
                ToolRegistry::with_builtins(),
                PermissionEngine::new(profile, Vec::new()),
                approver,
                Store::open(root),
                workspace,
                RecoveryEngine::new(RecoveryBudget::default()),
                SessionConfig {
                    interactivity,
                    trusted: true,
                    ..SessionConfig::default()
                },
                Vec::new(),
            ))
        }
    }

    impl ConnectionFactory for FakeFactory {
        fn take_asks(&self) -> Option<ApproverHalves> {
            self.pending.lock().unwrap().take()
        }
        fn model(&self) -> &str {
            "fake-model"
        }
    }

    fn unique_endpoint() -> Endpoint {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        #[cfg(windows)]
        {
            Endpoint::from_addr(&format!(r"\\.\pipe\localpilot-servecmd-{pid}-{n}"))
        }
        #[cfg(unix)]
        {
            Endpoint::from_addr(&format!(
                "{}/lp-sc-{pid}-{n}.sock",
                std::env::temp_dir().display()
            ))
        }
    }

    /// A server that accepts exactly `count` connections against one shared
    /// registry/host-map/factory (so several connections can share a session),
    /// and completes only once every client has disconnected — the clean-stop
    /// signal the tests await.
    fn spawn_server(factory: Arc<FakeFactory>, count: usize) -> (Endpoint, JoinHandle<()>) {
        spawn_server_on(factory, count, HostMap::default())
    }

    /// [`spawn_server`] over a host map the test keeps, to act on hosted
    /// sessions (close them) while clients are attached.
    fn spawn_server_on(
        factory: Arc<FakeFactory>,
        count: usize,
        hosts: HostMap,
    ) -> (Endpoint, JoinHandle<()>) {
        spawn_server_with(factory, count, hosts, SessionRegistry::new())
    }

    /// [`spawn_server_on`] over a registry the test keeps too, so it can run
    /// the reaper against the served sessions.
    fn spawn_server_with(
        factory: Arc<FakeFactory>,
        count: usize,
        hosts: HostMap,
        registry: SessionRegistry,
    ) -> (Endpoint, JoinHandle<()>) {
        let endpoint = unique_endpoint();
        let listener = Listener::bind(&endpoint).unwrap();
        let store = factory.store();
        let handle = tokio::spawn(async move {
            let mut connections = Vec::new();
            for _ in 0..count {
                let conn = listener.accept().await.unwrap();
                let registry = registry.clone();
                let hosts = hosts.clone();
                let factory = factory.clone();
                let store = store.clone();
                connections.push(tokio::spawn(async move {
                    let (read, write) = tokio::io::split(conn);
                    serve_connection(
                        read,
                        write,
                        &registry,
                        &hosts,
                        factory.as_ref(),
                        &store,
                        Profile::Bypass,
                    )
                    .await
                    .unwrap();
                }));
            }
            for connection in connections {
                connection.await.unwrap();
            }
        });
        (endpoint, handle)
    }

    async fn dial(endpoint: &Endpoint) -> Conn {
        timeout(DEADLINE, async {
            loop {
                match localpilot_server::connect(endpoint).await {
                    Ok(conn) => return conn,
                    Err(TransportError::NotRunning | TransportError::Busy) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => panic!("dial failed: {error:?}"),
                }
            }
        })
        .await
        .expect("dial timed out")
    }

    async fn read_event(reader: &mut JsonRecordReader<impl AsyncRead + Unpin>) -> ServerEvent {
        let value = timeout(DEADLINE, reader.next())
            .await
            .expect("read timed out")
            .unwrap()
            .expect("connection closed unexpectedly");
        serde_json::from_value::<ServerRecord>(value)
            .expect("decode server record")
            .event
    }

    async fn attach_client<W: AsyncWrite + Unpin>(
        reader: &mut JsonRecordReader<impl AsyncRead + Unpin>,
        write: &mut W,
        target: AttachTarget,
    ) -> SessionId {
        attach_client_with(reader, write, target, Vec::new()).await
    }

    /// Attach declaring `capabilities` (empty = a legacy client).
    async fn attach_client_with<W: AsyncWrite + Unpin>(
        reader: &mut JsonRecordReader<impl AsyncRead + Unpin>,
        write: &mut W,
        target: AttachTarget,
        capabilities: Vec<String>,
    ) -> SessionId {
        send_command(
            write,
            ClientCommand::Attach {
                since: None,
                target,
                capabilities,
            },
        )
        .await
        .unwrap();
        match read_event(reader).await {
            ServerEvent::Attached { session_id, .. } => session_id,
            other => panic!("expected `attached`, got {other:?}"),
        }
    }

    /// Read events until the turn stops.
    async fn collect_turn(
        reader: &mut JsonRecordReader<impl AsyncRead + Unpin>,
    ) -> Vec<ServerEvent> {
        let mut events = Vec::new();
        loop {
            let event = read_event(reader).await;
            let stop = matches!(event, ServerEvent::Stopped { .. });
            events.push(event);
            if stop {
                break;
            }
        }
        events
    }

    // --- serve loop: a turn streams back and the server stops cleanly --------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_loop_streams_a_turn_and_stops_cleanly() {
        let factory = Arc::new(FakeFactory::new(
            FakeProvider::new().text("hello from serve"),
        ));
        let (endpoint, server) = spawn_server(factory, 1);

        let conn = dial(&endpoint).await;
        let (read, mut write) = tokio::io::split(conn);
        let mut reader = JsonRecordReader::new(read);

        let id = attach_client(&mut reader, &mut write, AttachTarget::OpenNew).await;
        assert!(!id.to_string().is_empty());

        send_command(
            &mut write,
            ClientCommand::Prompt {
                text: "hi".to_string(),
                disposition: InputDisposition::Immediate,
            },
        )
        .await
        .unwrap();

        let observed = collect_turn(&mut reader).await;
        assert!(
            observed
                .iter()
                .any(|event| matches!(event, ServerEvent::TextDelta { text } if text.contains("hello from serve"))),
            "the turn's text streamed back: {observed:?}"
        );
        assert!(
            observed
                .iter()
                .any(|event| matches!(event, ServerEvent::Stopped { reason } if reason == "done")),
            "the turn stopped done: {observed:?}"
        );

        // The client disconnects; the server connection ends cleanly (the task's
        // `serve_connection(...).unwrap()` proves it returned `Ok`).
        drop(write);
        drop(reader);
        timeout(DEADLINE, server)
            .await
            .expect("server task did not stop")
            .unwrap();
    }

    // --- a connect-style client drives one full turn -------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn connect_client_drives_one_full_turn() {
        let factory = Arc::new(FakeFactory::new(
            FakeProvider::new().text("client observed this"),
        ));
        let (endpoint, server) = spawn_server(factory, 1);

        // Drive the client wire exactly as `connect` does: attach, prompt, read.
        let conn = dial(&endpoint).await;
        let (read, mut write) = tokio::io::split(conn);
        let mut reader = JsonRecordReader::new(read);

        let observed_attach = {
            send_command(
                &mut write,
                ClientCommand::Attach {
                    since: None,
                    target: AttachTarget::OpenNew,
                    capabilities: Vec::new(),
                },
            )
            .await
            .unwrap();
            read_event(&mut reader).await
        };
        assert!(
            matches!(observed_attach, ServerEvent::Attached { .. }),
            "the client observed the attach: {observed_attach:?}"
        );

        send_command(
            &mut write,
            ClientCommand::Prompt {
                text: "go".to_string(),
                disposition: InputDisposition::Immediate,
            },
        )
        .await
        .unwrap();

        let observed = collect_turn(&mut reader).await;
        assert!(
            observed
                .iter()
                .any(|event| matches!(event, ServerEvent::TextDelta { text } if text.contains("client observed this"))),
            "the client observed the turn text: {observed:?}"
        );
        assert!(
            observed
                .iter()
                .any(|event| matches!(event, ServerEvent::Stopped { .. })),
            "the client observed the turn stop: {observed:?}"
        );

        drop(write);
        drop(reader);
        timeout(DEADLINE, server)
            .await
            .expect("server task did not stop")
            .unwrap();
    }

    // --- two clients on one session, both observe the stream -----------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_clients_share_one_session_and_both_observe_the_turn() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("shared answer")));
        let (endpoint, server) = spawn_server(factory, 2);

        // Client 1 opens a fresh session.
        let conn1 = dial(&endpoint).await;
        let (read1, mut write1) = tokio::io::split(conn1);
        let mut reader1 = JsonRecordReader::new(read1);
        let id = attach_client(&mut reader1, &mut write1, AttachTarget::OpenNew).await;

        // Client 2 resumes the *same live* session by id — reusing the shared
        // host, not a second registry entry.
        let conn2 = dial(&endpoint).await;
        let (read2, mut write2) = tokio::io::split(conn2);
        let mut reader2 = JsonRecordReader::new(read2);
        let id2 = attach_client(
            &mut reader2,
            &mut write2,
            AttachTarget::ResumeId { session_id: id },
        )
        .await;
        assert_eq!(id, id2, "both connections bind the one session");

        // Client 1 drives; both connections observe the same stream (fanout).
        send_command(
            &mut write1,
            ClientCommand::Prompt {
                text: "build it".to_string(),
                disposition: InputDisposition::Immediate,
            },
        )
        .await
        .unwrap();

        // Observed-vs-expected: each client independently sees the text and stop.
        let observed1 = collect_turn(&mut reader1).await;
        let observed2 = collect_turn(&mut reader2).await;
        for (label, observed) in [("1", &observed1), ("2", &observed2)] {
            assert!(
                observed
                    .iter()
                    .any(|event| matches!(event, ServerEvent::TextDelta { text } if text.contains("shared answer"))),
                "client {label} saw the text: {observed:?}"
            );
            assert!(
                observed.iter().any(
                    |event| matches!(event, ServerEvent::Stopped { reason } if reason == "done")
                ),
                "client {label} saw the stop: {observed:?}"
            );
        }

        drop(write1);
        drop(reader1);
        drop(write2);
        drop(reader2);
        timeout(DEADLINE, server)
            .await
            .expect("server task did not stop")
            .unwrap();
    }

    // --- session-owned permission asks and command acknowledgements -----------

    /// A provider whose first turn runs a command the default profile asks
    /// about, then answers; later turns answer with `then`.
    fn asking_provider(then: &[&str]) -> FakeProvider {
        let mut provider = FakeProvider::new()
            .tool_call(
                "c1",
                "run_shell",
                serde_json::json!({ "program": "rm", "args": ["-rf", "no-such-dir"] }),
            )
            .text("first answer");
        for text in then {
            provider = provider.text(text);
        }
        provider
    }

    fn acks() -> Vec<String> {
        vec![CAPABILITY_ACKS.to_string()]
    }

    /// Read events until `pred` matches one, returning everything read.
    async fn read_until(
        reader: &mut JsonRecordReader<impl AsyncRead + Unpin>,
        pred: impl Fn(&ServerEvent) -> bool,
    ) -> Vec<ServerEvent> {
        let mut seen = Vec::new();
        loop {
            let event = read_event(reader).await;
            let done = pred(&event);
            seen.push(event);
            if done {
                return seen;
            }
        }
    }

    async fn send_with_id<W: AsyncWrite + Unpin>(write: &mut W, id: &str, command: ClientCommand) {
        let record = ClientRecord {
            request: None,
            v: RPC_PROTOCOL_VERSION,
            id: Some(id.to_string()),
            command,
        };
        let mut line = serde_json::to_vec(&record).unwrap();
        line.push(b'\n');
        write.write_all(&line).await.unwrap();
        write.flush().await.unwrap();
    }

    fn ask_id_of(events: &[ServerEvent]) -> String {
        events
            .iter()
            .find_map(|event| match event {
                ServerEvent::PermissionAsk { ask_id, .. } => Some(ask_id.clone()),
                _ => None,
            })
            .expect("a permission ask")
    }

    fn is_ask(event: &ServerEvent) -> bool {
        matches!(event, ServerEvent::PermissionAsk { .. })
    }

    fn is_stop(event: &ServerEvent) -> bool {
        matches!(event, ServerEvent::Stopped { .. })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn any_attached_client_may_answer_and_the_first_answer_wins() {
        let factory = Arc::new(FakeFactory::asking(asking_provider(&[])));
        let (endpoint, server) = spawn_server(factory, 2);

        let (read1, mut write1) = tokio::io::split(dial(&endpoint).await);
        let mut reader1 = JsonRecordReader::new(read1);
        let id = attach_client_with(&mut reader1, &mut write1, AttachTarget::OpenNew, acks()).await;
        let (read2, mut write2) = tokio::io::split(dial(&endpoint).await);
        let mut reader2 = JsonRecordReader::new(read2);
        attach_client_with(
            &mut reader2,
            &mut write2,
            AttachTarget::ResumeId { session_id: id },
            acks(),
        )
        .await;

        send_with_id(
            &mut write1,
            "p1",
            ClientCommand::Prompt {
                text: "clean up".to_string(),
                disposition: InputDisposition::Immediate,
            },
        )
        .await;
        let seen1 = read_until(&mut reader1, is_ask).await;
        assert!(
            seen1
                .iter()
                .any(|event| matches!(event, ServerEvent::Accepted { .. })),
            "the prompt was acknowledged: {seen1:?}"
        );
        let ask = ask_id_of(&seen1);
        let seen2 = read_until(&mut reader2, is_ask).await;
        assert_eq!(
            ask_id_of(&seen2),
            ask,
            "the second client sees the same ask"
        );

        // The client that did not start the turn answers first and wins.
        send_with_id(
            &mut write2,
            "r2",
            ClientCommand::PermissionReply {
                ask_id: ask.clone(),
                allow: true,
            },
        )
        .await;
        // Its acknowledgement arrives before the other client answers, so the
        // order of the two answers is fixed.
        let mut early2 = read_until(&mut reader2, |event| {
            matches!(event, ServerEvent::Accepted { .. })
        })
        .await;
        send_with_id(
            &mut write1,
            "r1",
            ClientCommand::PermissionReply {
                ask_id: ask.clone(),
                allow: false,
            },
        )
        .await;

        let mut rest1 = read_until(&mut reader1, is_stop).await;
        // The late answer's refusal may arrive after the turn has stopped.
        if !rest1
            .iter()
            .any(|event| matches!(event, ServerEvent::Rejected { .. }))
        {
            rest1.extend(
                read_until(&mut reader1, |event| {
                    matches!(event, ServerEvent::Rejected { .. })
                })
                .await,
            );
        }
        early2.extend(read_until(&mut reader2, is_stop).await);
        let rest2 = early2;
        assert!(
            rest1.iter().any(|event| matches!(
                event,
                ServerEvent::Rejected {
                    code: RejectCode::AlreadyResolved,
                    ..
                }
            )),
            "the late answer is refused: {rest1:?}"
        );
        assert!(
            rest2
                .iter()
                .any(|event| matches!(event, ServerEvent::Accepted { .. })),
            "the winning answer is acknowledged: {rest2:?}"
        );
        for (label, rest) in [("1", &rest1), ("2", &rest2)] {
            let resolved: Vec<_> = rest
                .iter()
                .filter(|event| matches!(event, ServerEvent::PermissionResolved { .. }))
                .collect();
            assert_eq!(
                resolved.len(),
                1,
                "client {label} hears exactly one outcome: {rest:?}"
            );
            assert!(matches!(
                resolved[0],
                ServerEvent::PermissionResolved {
                    outcome: localpilot_rpc::AskOutcome::Allowed,
                    by: Some(Principal::Local),
                    ..
                }
            ));
        }

        drop((write1, reader1, write2, reader2));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_ask_outlives_the_client_whose_prompt_raised_it() {
        let factory = Arc::new(FakeFactory::asking(asking_provider(&[])));
        let (endpoint, server) = spawn_server(factory, 2);

        // A legacy client starts the turn, sees the ask, and goes away.
        let (read1, mut write1) = tokio::io::split(dial(&endpoint).await);
        let mut reader1 = JsonRecordReader::new(read1);
        let id = attach_client(&mut reader1, &mut write1, AttachTarget::OpenNew).await;
        send_command(
            &mut write1,
            ClientCommand::Prompt {
                text: "clean up".to_string(),
                disposition: InputDisposition::Immediate,
            },
        )
        .await
        .unwrap();
        let ask = ask_id_of(&read_until(&mut reader1, is_ask).await);
        drop((write1, reader1));

        // A client that attaches afterwards is shown the outstanding ask and
        // may answer it.
        let (read2, mut write2) = tokio::io::split(dial(&endpoint).await);
        let mut reader2 = JsonRecordReader::new(read2);
        attach_client_with(
            &mut reader2,
            &mut write2,
            AttachTarget::ResumeId { session_id: id },
            acks(),
        )
        .await;
        let shown = read_until(&mut reader2, is_ask).await;
        assert!(matches!(
            shown.last(),
            Some(ServerEvent::PermissionAsk { ask_id, expires_at_ms, .. })
                if *ask_id == ask && *expires_at_ms > 0
        ));
        send_with_id(
            &mut write2,
            "deny",
            ClientCommand::PermissionReply {
                ask_id: ask.clone(),
                allow: false,
            },
        )
        .await;
        let rest = read_until(&mut reader2, is_stop).await;
        assert!(rest
            .iter()
            .any(|event| matches!(event, ServerEvent::Accepted { .. })));
        assert!(rest.iter().any(|event| matches!(
            event,
            ServerEvent::PermissionResolved {
                outcome: localpilot_rpc::AskOutcome::Denied,
                ..
            }
        )));

        drop((write2, reader2));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_legacy_client_receives_no_new_record_types() {
        let factory = Arc::new(FakeFactory::asking(asking_provider(&[])));
        let (endpoint, server) = spawn_server(factory, 1);

        let (read, mut write) = tokio::io::split(dial(&endpoint).await);
        let mut reader = JsonRecordReader::new(read);
        attach_client(&mut reader, &mut write, AttachTarget::OpenNew).await;
        send_command(
            &mut write,
            ClientCommand::Prompt {
                text: "clean up".to_string(),
                disposition: InputDisposition::Immediate,
            },
        )
        .await
        .unwrap();
        let mut seen = read_until(&mut reader, is_ask).await;
        let ask = ask_id_of(&seen);
        for _ in 0..2 {
            send_command(
                &mut write,
                ClientCommand::PermissionReply {
                    ask_id: ask.clone(),
                    allow: false,
                },
            )
            .await
            .unwrap();
        }
        seen.extend(read_until(&mut reader, is_stop).await);
        // The second reply's refusal may arrive before or after the stop.
        if !seen
            .iter()
            .any(|event| matches!(event, ServerEvent::Error { .. }))
        {
            seen.extend(
                read_until(&mut reader, |event| {
                    matches!(event, ServerEvent::Error { .. })
                })
                .await,
            );
        }
        assert!(
            !seen.iter().any(|event| matches!(
                event,
                ServerEvent::Accepted { .. }
                    | ServerEvent::Rejected { .. }
                    | ServerEvent::PermissionResolved { .. }
                    | ServerEvent::TurnStarted
                    | ServerEvent::Snapshot { .. }
                    | ServerEvent::ResyncRequired { .. }
                    | ServerEvent::SessionClosed { .. }
            )),
            "no record type the legacy client cannot read: {seen:?}"
        );

        drop((write, reader));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_follow_up_sent_while_busy_is_queued_and_runs_next() {
        let factory = Arc::new(FakeFactory::asking(asking_provider(&["second answer"])));
        let (endpoint, server) = spawn_server(factory, 1);

        let (read, mut write) = tokio::io::split(dial(&endpoint).await);
        let mut reader = JsonRecordReader::new(read);
        attach_client_with(&mut reader, &mut write, AttachTarget::OpenNew, acks()).await;
        send_with_id(
            &mut write,
            "p1",
            ClientCommand::Prompt {
                text: "clean up".to_string(),
                disposition: InputDisposition::Immediate,
            },
        )
        .await;
        // The turn is now parked on the ask: it is running.
        let ask = ask_id_of(&read_until(&mut reader, is_ask).await);

        send_with_id(
            &mut write,
            "f1",
            ClientCommand::Prompt {
                text: "and then this".to_string(),
                disposition: InputDisposition::FollowUp,
            },
        )
        .await;
        send_with_id(
            &mut write,
            "i1",
            ClientCommand::Prompt {
                text: "right now".to_string(),
                disposition: InputDisposition::Immediate,
            },
        )
        .await;
        send_with_id(
            &mut write,
            "r1",
            ClientCommand::PermissionReply {
                ask_id: ask,
                allow: false,
            },
        )
        .await;

        let mut seen = read_until(&mut reader, is_stop).await;
        seen.extend(read_until(&mut reader, is_stop).await);
        assert!(seen.iter().any(|event| matches!(
            event,
            ServerEvent::Queued {
                disposition: InputDisposition::FollowUp
            }
        )));
        assert!(seen.iter().any(|event| matches!(
            event,
            ServerEvent::Rejected {
                code: RejectCode::Busy,
                ..
            }
        )));
        assert!(
            seen.iter().any(
                |event| matches!(event, ServerEvent::TextDelta { text } if text.contains("second answer"))
            ),
            "the follow-up ran as its own turn: {seen:?}"
        );

        drop((write, reader));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    // --- sequenced feed: replay, snapshot, closure, protocol version ---------

    fn resume_caps() -> Vec<String> {
        vec![CAPABILITY_ACKS.to_string(), CAPABILITY_RESUME.to_string()]
    }

    /// Attach with capabilities and an optional cursor; returns the session id
    /// and the feed epoch from `attached`.
    async fn attach_resume<W: AsyncWrite + Unpin>(
        reader: &mut JsonRecordReader<impl AsyncRead + Unpin>,
        write: &mut W,
        target: AttachTarget,
        since: Option<localpilot_rpc::FeedCursor>,
    ) -> (SessionId, String) {
        send_command(
            write,
            ClientCommand::Attach {
                target,
                capabilities: resume_caps(),
                since,
            },
        )
        .await
        .unwrap();
        match read_event(reader).await {
            ServerEvent::Attached {
                session_id, epoch, ..
            } => (session_id, epoch),
            other => panic!("expected `attached`, got {other:?}"),
        }
    }

    /// Read whole records (with their feed position) until `pred` matches.
    async fn read_records_until(
        reader: &mut JsonRecordReader<impl AsyncRead + Unpin>,
        pred: impl Fn(&ServerEvent) -> bool,
    ) -> Vec<ServerRecord> {
        let mut seen = Vec::new();
        loop {
            let value = timeout(DEADLINE, reader.next())
                .await
                .expect("read timed out")
                .unwrap()
                .expect("server closed early");
            let record: ServerRecord = serde_json::from_value(value).unwrap();
            let done = pred(&record.event);
            seen.push(record);
            if done {
                return seen;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_reconnecting_client_replays_exactly_what_it_missed() {
        let factory = Arc::new(FakeFactory::new(
            FakeProvider::new()
                .text("first answer")
                .text("second answer"),
        ));
        let (endpoint, server) = spawn_server(factory, 3);

        // A resume client follows one turn, noting its last position.
        let (read1, mut write1) = tokio::io::split(dial(&endpoint).await);
        let mut reader1 = JsonRecordReader::new(read1);
        let (id, epoch) =
            attach_resume(&mut reader1, &mut write1, AttachTarget::OpenNew, None).await;
        assert!(!epoch.is_empty(), "resume clients learn the feed epoch");
        let first = read_records_until(&mut reader1, |event| {
            matches!(event, ServerEvent::Snapshot { .. })
        })
        .await;
        assert!(matches!(first[0].event, ServerEvent::Snapshot { .. }));
        send_command(
            &mut write1,
            ClientCommand::Prompt {
                text: "one".to_string(),
                disposition: InputDisposition::Immediate,
            },
        )
        .await
        .unwrap();
        let turn = read_records_until(&mut reader1, is_stop).await;
        assert!(turn
            .iter()
            .any(|record| record.event == ServerEvent::TurnStarted));
        let last = turn.iter().filter_map(|record| record.seq).max().unwrap();
        drop((write1, reader1));

        // Meanwhile another client drives a second turn.
        let (read2, mut write2) = tokio::io::split(dial(&endpoint).await);
        let mut reader2 = JsonRecordReader::new(read2);
        attach_client(
            &mut reader2,
            &mut write2,
            AttachTarget::ResumeId { session_id: id },
        )
        .await;
        send_command(
            &mut write2,
            ClientCommand::Prompt {
                text: "two".to_string(),
                disposition: InputDisposition::Immediate,
            },
        )
        .await
        .unwrap();
        collect_turn(&mut reader2).await;

        // The first client comes back with its cursor and gets the second turn
        // replayed: no snapshot, no gap, nothing it already had.
        let (read3, mut write3) = tokio::io::split(dial(&endpoint).await);
        let mut reader3 = JsonRecordReader::new(read3);
        attach_resume(
            &mut reader3,
            &mut write3,
            AttachTarget::ResumeId { session_id: id },
            Some(localpilot_rpc::FeedCursor { epoch, seq: last }),
        )
        .await;
        let replay = read_records_until(&mut reader3, is_stop).await;
        assert!(!replay
            .iter()
            .any(|record| matches!(record.event, ServerEvent::Snapshot { .. })));
        let seqs: Vec<u64> = replay.iter().map(|record| record.seq.unwrap()).collect();
        assert_eq!(seqs[0], last + 1, "starts right after the cursor");
        assert!(
            seqs.windows(2).all(|pair| pair[1] > pair[0]),
            "in order: {seqs:?}"
        );
        assert!(replay.iter().any(|record| matches!(
            &record.event,
            ServerEvent::TextDelta { text } if text.contains("second answer")
        )));
        assert!(!replay.iter().any(|record| matches!(
            &record.event,
            ServerEvent::TextDelta { text } if text.contains("first answer")
        )));

        drop((write2, reader2, write3, reader3));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_snapshot_during_a_running_turn_does_not_wait_for_it() {
        let factory = Arc::new(FakeFactory::asking(asking_provider(&[])));
        let (endpoint, server) = spawn_server(factory, 2);

        let (read1, mut write1) = tokio::io::split(dial(&endpoint).await);
        let mut reader1 = JsonRecordReader::new(read1);
        let id = attach_client_with(&mut reader1, &mut write1, AttachTarget::OpenNew, acks()).await;
        send_command(
            &mut write1,
            ClientCommand::Prompt {
                text: "clean up".to_string(),
                disposition: InputDisposition::Immediate,
            },
        )
        .await
        .unwrap();
        // The turn is now parked on its ask, holding the session runtime.
        let ask = ask_id_of(&read_until(&mut reader1, is_ask).await);

        // A resume client attaching now gets the state at once.
        let (read2, mut write2) = tokio::io::split(dial(&endpoint).await);
        let mut reader2 = JsonRecordReader::new(read2);
        attach_resume(
            &mut reader2,
            &mut write2,
            AttachTarget::ResumeId { session_id: id },
            None,
        )
        .await;
        match read_event(&mut reader2).await {
            ServerEvent::Snapshot { turn, asks, .. } => {
                assert_eq!(turn, localpilot_rpc::TurnState::Running);
                assert_eq!(asks.len(), 1);
                assert_eq!(asks[0].ask_id, ask);
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }

        send_command(
            &mut write1,
            ClientCommand::PermissionReply {
                ask_id: ask,
                allow: false,
            },
        )
        .await
        .unwrap();
        // Both clients read the turn to its end before going away.
        read_until(&mut reader1, is_stop).await;
        read_until(&mut reader2, is_stop).await;
        drop((write1, reader1, write2, reader2));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn closing_a_session_tells_every_attached_client() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let hosts = HostMap::default();
        let (endpoint, server) = spawn_server_on(factory, 2, hosts.clone());

        let (read1, mut write1) = tokio::io::split(dial(&endpoint).await);
        let mut reader1 = JsonRecordReader::new(read1);
        let (id, _) = attach_resume(&mut reader1, &mut write1, AttachTarget::OpenNew, None).await;
        read_event(&mut reader1).await; // the initial snapshot
        let (read2, mut write2) = tokio::io::split(dial(&endpoint).await);
        let mut reader2 = JsonRecordReader::new(read2);
        attach_client(
            &mut reader2,
            &mut write2,
            AttachTarget::ResumeId { session_id: id },
        )
        .await;
        assert_eq!(hosts.served(id).unwrap().feed.clients(), 2);

        hosts.close_served(id, "closed after inactivity");
        assert!(matches!(
            read_event(&mut reader1).await,
            ServerEvent::SessionClosed { reason } if reason == "closed after inactivity"
        ));
        assert_eq!(
            read_event(&mut reader2).await,
            ServerEvent::Closed,
            "a legacy client gets the closing record it knows"
        );
        drop((write1, reader1, write2, reader2));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_newer_protocol_version_is_refused() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let (endpoint, server) = spawn_server(factory, 2);

        // On an attached connection: rejected, nothing runs.
        let (read, mut write) = tokio::io::split(dial(&endpoint).await);
        let mut reader = JsonRecordReader::new(read);
        attach_client_with(&mut reader, &mut write, AttachTarget::OpenNew, acks()).await;
        write
            .write_all(
                b"{\"v\":99,\"id\":\"n1\",\"command\":{\"type\":\"prompt\",\"text\":\"hi\"}}\n",
            )
            .await
            .unwrap();
        write.flush().await.unwrap();
        let value = timeout(DEADLINE, reader.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let record: ServerRecord = serde_json::from_value(value).unwrap();
        assert_eq!(record.id.as_deref(), Some("n1"));
        assert!(matches!(
            record.event,
            ServerEvent::Rejected {
                code: RejectCode::UnsupportedVersion,
                ..
            }
        ));

        // As the first record: an error, and the connection ends.
        let (read2, mut write2) = tokio::io::split(dial(&endpoint).await);
        let mut reader2 = JsonRecordReader::new(read2);
        write2
            .write_all(b"{\"v\":99,\"command\":{\"type\":\"attach\",\"target\":{\"mode\":\"open_new\"}}}\n")
            .await
            .unwrap();
        write2.flush().await.unwrap();
        assert!(matches!(
            read_event(&mut reader2).await,
            ServerEvent::Error { .. }
        ));
        assert!(timeout(DEADLINE, reader2.next())
            .await
            .unwrap()
            .unwrap()
            .is_none());

        drop((write, reader, write2, reader2));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    fn view(acks: bool, resume: bool) -> ConnectionView {
        ConnectionView {
            acks,
            resume,
            floor: 0,
            shown: HashSet::new(),
        }
    }

    fn written(buffer: &[u8]) -> Vec<ServerEvent> {
        String::from_utf8_lossy(buffer)
            .lines()
            .map(|line| serde_json::from_str::<ServerRecord>(line).unwrap().event)
            .collect()
    }

    /// A hosted session with its feed, as `bind` would set it up.
    async fn served_session(
        factory: &Arc<FakeFactory>,
    ) -> (
        SessionRegistry,
        HostMap,
        SessionId,
        Arc<SessionHost>,
        Served,
    ) {
        let registry = SessionRegistry::new();
        let hosts = HostMap::default();
        let (id, host) = open_hosted_session(factory, &registry, &hosts).await;
        let served = hosts.served_for(id, &host, Some(None));
        (registry, hosts, id, host, served)
    }

    #[tokio::test]
    async fn a_resync_after_the_session_closed_ends_the_connection() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let store = factory.store();
        let (_registry, hosts, id, host, served) = served_session(&factory).await;
        hosts.close_served(id, "closed after inactivity");
        let ctx = FeedContext {
            feed: &served.feed,
            store: &store,
            session_id: id,
            host: &host,
        };

        // A resume client that lagged across the close gets the closure, not a
        // snapshot, and stops; so does a legacy one, in the record it knows.
        for (resume, expected) in [
            (
                true,
                ServerEvent::SessionClosed {
                    reason: "closed after inactivity".to_string(),
                },
            ),
            (false, ServerEvent::Closed),
        ] {
            let mut out = Vec::new();
            let mut connection = view(true, resume);
            assert_eq!(
                resync(&mut out, &mut connection, &ctx).await.unwrap(),
                Flow::Stop
            );
            assert_eq!(written(&out), vec![expected.clone()]);

            // A replayed or live `resync_required` reaches the same end.
            let mut out = Vec::new();
            let flow = deliver(
                &mut out,
                &mut connection,
                &ctx,
                Some(7),
                ServerEvent::ResyncRequired {
                    epoch: "e".to_string(),
                    seq: 7,
                },
            )
            .await
            .unwrap();
            assert_eq!(flow, Flow::Stop);
            assert_eq!(written(&out).last(), Some(&expected));
        }

        // An attach that races the close sees only the closure.
        let attached = served
            .feed
            .attach(None, true, &(Vec::new(), false), &|| admission_of(&host));
        assert!(matches!(
            attached.initial.as_slice(),
            [(None, ServerEvent::SessionClosed { .. })]
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_snapshot_shows_accepted_work_as_busy_before_its_turn_starts() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let (registry, _hosts, id, host, served) = served_session(&factory).await;

        // Hold the session so the accepted turn cannot start.
        let handle = registry.get(id).await.unwrap();
        let guard = handle.lock().await;
        assert_eq!(host.submit("go".to_string(), false), Admit::Started);
        match served
            .feed
            .snapshot(&(Vec::new(), false), &|| admission_of(&host))
            .0
        {
            ServerEvent::Snapshot { turn, busy, .. } => {
                assert_eq!(turn, localpilot_rpc::TurnState::Idle, "no turn started yet");
                assert!(busy, "accepted work is reported busy");
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
        drop(guard);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_snapshot_between_follow_ups_is_busy() {
        // The follow-up turn parks on a permission ask, so the session stays
        // busy after the first turn stops.
        let provider = FakeProvider::new()
            .text("first answer")
            .tool_call(
                "c1",
                "run_shell",
                serde_json::json!({ "program": "rm", "args": ["-rf", "no-such-dir"] }),
            )
            .text("second answer");
        let factory = Arc::new(FakeFactory::asking(provider));
        let (_registry, hosts, id, host, served) = served_session(&factory).await;
        let mut live = served
            .feed
            .attach(None, true, &(Vec::new(), false), &|| admission_of(&host))
            .live;

        assert_eq!(host.submit("one".to_string(), false), Admit::Started);
        assert_eq!(host.submit("two".to_string(), true), Admit::Queued);
        timeout(DEADLINE, async {
            loop {
                if let Ok((_, ServerEvent::Stopped { .. })) = live.recv().await {
                    break;
                }
            }
        })
        .await
        .expect("the first turn stopped");

        match served
            .feed
            .snapshot(&(Vec::new(), false), &|| admission_of(&host))
            .0
        {
            ServerEvent::Snapshot { busy, .. } => {
                assert!(busy, "the follow-up is admitted, so the session is busy");
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
        host.cancel_all();
        hosts.close_served(id, "test over");
    }

    // --- idempotent retries -----------------------------------------------------

    /// Attach declaring `acks`; returns the session id and the server instance.
    async fn attach_for_retries<W: AsyncWrite + Unpin>(
        reader: &mut JsonRecordReader<impl AsyncRead + Unpin>,
        write: &mut W,
        target: AttachTarget,
    ) -> (SessionId, String) {
        send_command(
            write,
            ClientCommand::Attach {
                target,
                capabilities: acks(),
                since: None,
            },
        )
        .await
        .unwrap();
        match read_event(reader).await {
            ServerEvent::Attached {
                session_id,
                instance,
                ..
            } => (session_id, instance),
            other => panic!("expected `attached`, got {other:?}"),
        }
    }

    async fn send_keyed<W: AsyncWrite + Unpin>(
        write: &mut W,
        id: &str,
        instance: &str,
        n: u64,
        command: ClientCommand,
    ) {
        let record = ClientRecord {
            v: RPC_PROTOCOL_VERSION,
            id: Some(id.to_string()),
            request: Some(localpilot_rpc::RequestKey {
                instance: instance.to_string(),
                client: "phone".to_string(),
                n,
            }),
            command,
        };
        let mut line = serde_json::to_vec(&record).unwrap();
        line.push(b'\n');
        write.write_all(&line).await.unwrap();
        write.flush().await.unwrap();
    }

    fn prompt(text: &str) -> ClientCommand {
        ClientCommand::Prompt {
            text: text.to_string(),
            disposition: InputDisposition::Immediate,
        }
    }

    /// Read the next record; `None` if nothing arrives within `wait`.
    async fn next_record_within(
        reader: &mut JsonRecordReader<impl AsyncRead + Unpin>,
        wait: Duration,
    ) -> Option<ServerRecord> {
        match timeout(wait, reader.next()).await {
            Ok(Ok(Some(value))) => Some(serde_json::from_value(value).unwrap()),
            _ => None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_retried_request_gets_its_original_reply_and_runs_once() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("only answer")));
        let (endpoint, server) = spawn_server(factory, 1);
        let (read, mut write) = tokio::io::split(dial(&endpoint).await);
        let mut reader = JsonRecordReader::new(read);
        let (_, instance) =
            attach_for_retries(&mut reader, &mut write, AttachTarget::OpenNew).await;
        assert!(!instance.is_empty(), "`attached` names the server instance");

        send_keyed(&mut write, "p1", &instance, 1, prompt("go")).await;
        let first = read_records_until(&mut reader, is_stop).await;
        assert!(first.iter().any(|record| record.id.as_deref() == Some("p1")
            && matches!(record.event, ServerEvent::Accepted { .. })));

        // The reply was lost; the client retries the same request under a new
        // correlation id, and the original outcome comes back under that id.
        send_keyed(&mut write, "p1-retry", &instance, 1, prompt("go")).await;
        let replay = next_record_within(&mut reader, DEADLINE).await.unwrap();
        assert_eq!(replay.id.as_deref(), Some("p1-retry"));
        assert!(matches!(replay.event, ServerEvent::Accepted { .. }));
        assert!(
            next_record_within(&mut reader, Duration::from_millis(400))
                .await
                .is_none(),
            "the retry started no second turn"
        );

        // The same number for another command is refused.
        send_keyed(&mut write, "p2", &instance, 1, prompt("something else")).await;
        let refused = next_record_within(&mut reader, DEADLINE).await.unwrap();
        assert!(matches!(
            refused.event,
            ServerEvent::Rejected {
                code: RejectCode::IdReused,
                ..
            }
        ));
        // A request issued to an earlier server instance has an unknown outcome.
        send_keyed(&mut write, "p3", "an-earlier-server", 2, prompt("go")).await;
        let refused = next_record_within(&mut reader, DEADLINE).await.unwrap();
        assert!(matches!(
            refused.event,
            ServerEvent::Rejected {
                code: RejectCode::OutcomeUnknown,
                ..
            }
        ));

        drop((write, reader));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_request_retried_after_the_session_was_reaped_is_not_run_again() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("only answer")));
        let hosts = HostMap::default();
        let registry = SessionRegistry::new();
        let (endpoint, server) = spawn_server_with(factory, 2, hosts.clone(), registry.clone());

        let (read, mut write) = tokio::io::split(dial(&endpoint).await);
        let mut reader = JsonRecordReader::new(read);
        let (id, instance) =
            attach_for_retries(&mut reader, &mut write, AttachTarget::OpenNew).await;
        send_keyed(&mut write, "p1", &instance, 1, prompt("go")).await;
        read_until(&mut reader, is_stop).await;

        // The session sits idle and is reaped while the client is attached.
        let mut reaper = Reaper::new(
            registry.clone(),
            hosts.clone(),
            ReaperConfig {
                grace: Duration::from_secs(3600),
                idle: Duration::ZERO,
                tick: Duration::from_secs(1),
            },
        );
        assert_eq!(reaper.reap_once(Instant::now()).await, vec![id]);
        assert_eq!(read_event(&mut reader).await, ServerEvent::Closed);
        drop((write, reader));

        // The client resumes the session and retries the request it never saw
        // answered: the original reply, and no second turn.
        let (read, mut write) = tokio::io::split(dial(&endpoint).await);
        let mut reader = JsonRecordReader::new(read);
        let (resumed, same_instance) = attach_for_retries(
            &mut reader,
            &mut write,
            AttachTarget::ResumeId { session_id: id },
        )
        .await;
        assert_eq!(resumed, id);
        assert_eq!(same_instance, instance);
        send_keyed(&mut write, "p1", &instance, 1, prompt("go")).await;
        let replay = next_record_within(&mut reader, DEADLINE).await.unwrap();
        assert_eq!(replay.id.as_deref(), Some("p1"));
        assert!(matches!(replay.event, ServerEvent::Accepted { .. }));
        assert!(
            next_record_within(&mut reader, Duration::from_millis(400))
                .await
                .is_none(),
            "nothing ran again"
        );

        drop((write, reader));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_request_whose_reply_was_never_read_replays_on_another_connection() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("only answer")));
        let (endpoint, server) = spawn_server(factory, 2);

        // The first connection sends and never reads its reply.
        let (read1, mut write1) = tokio::io::split(dial(&endpoint).await);
        let mut reader1 = JsonRecordReader::new(read1);
        let (id, instance) =
            attach_for_retries(&mut reader1, &mut write1, AttachTarget::OpenNew).await;
        send_keyed(&mut write1, "p1", &instance, 1, prompt("go")).await;

        // The client comes back on a new connection and retries.
        let (read2, mut write2) = tokio::io::split(dial(&endpoint).await);
        let mut reader2 = JsonRecordReader::new(read2);
        attach_for_retries(
            &mut reader2,
            &mut write2,
            AttachTarget::ResumeId { session_id: id },
        )
        .await;
        // Let the first copy finish, then retry: never run twice.
        tokio::time::sleep(Duration::from_millis(300)).await;
        send_keyed(&mut write2, "p1-again", &instance, 1, prompt("go")).await;
        let mut replies = Vec::new();
        while let Some(record) = next_record_within(&mut reader2, Duration::from_millis(400)).await
        {
            replies.push(record);
        }
        let to_retry: Vec<_> = replies
            .iter()
            .filter(|record| record.id.as_deref() == Some("p1-again"))
            .collect();
        assert_eq!(to_retry.len(), 1, "one reply to the retry: {replies:?}");
        assert!(matches!(to_retry[0].event, ServerEvent::Accepted { .. }));
        let stops = replies
            .iter()
            .filter(|record| matches!(record.event, ServerEvent::Stopped { .. }))
            .count();
        assert!(stops <= 1, "at most the original turn's stop: {replies:?}");

        drop((write1, reader1, write2, reader2));
        let _ = timeout(DEADLINE, server).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_copies_of_a_request_sent_together_run_once() {
        let factory = Arc::new(FakeFactory::new(
            FakeProvider::new()
                .text("only answer")
                .text("a second turn"),
        ));
        let (endpoint, server) = spawn_server(factory, 3);

        // A resume watcher counts the turns that start.
        let (read0, mut write0) = tokio::io::split(dial(&endpoint).await);
        let mut watcher = JsonRecordReader::new(read0);
        let (id, _) = attach_resume(&mut watcher, &mut write0, AttachTarget::OpenNew, None).await;
        read_event(&mut watcher).await; // initial snapshot

        let mut clients = Vec::new();
        for _ in 0..2 {
            let (read, mut write) = tokio::io::split(dial(&endpoint).await);
            let mut reader = JsonRecordReader::new(read);
            let (_, instance) = attach_for_retries(
                &mut reader,
                &mut write,
                AttachTarget::ResumeId { session_id: id },
            )
            .await;
            clients.push((reader, write, instance));
        }
        let (_, _, instance) = &clients[0];
        let instance = instance.clone();
        let (first, second) = clients.split_at_mut(1);
        tokio::join!(
            send_keyed(&mut first[0].1, "a", &instance, 1, prompt("go")),
            send_keyed(&mut second[0].1, "b", &instance, 1, prompt("go")),
        );

        let mut started = 0;
        while let Some(record) = next_record_within(&mut watcher, Duration::from_millis(600)).await
        {
            if record.event == ServerEvent::TurnStarted {
                started += 1;
            }
        }
        assert_eq!(started, 1, "the duplicate did not start a second turn");
        for (reader, _, _) in clients.iter_mut() {
            let reply = loop {
                let record = next_record_within(reader, DEADLINE).await.unwrap();
                if record.id.is_some() {
                    break record;
                }
            };
            assert!(
                matches!(
                    reply.event,
                    ServerEvent::Accepted { .. }
                        | ServerEvent::Rejected {
                            code: RejectCode::InProgress,
                            ..
                        }
                ),
                "each copy is accepted, replayed, or told it is in progress: {reply:?}"
            );
        }

        drop((write0, watcher, clients));
        let _ = timeout(DEADLINE, server).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_request_key_needs_the_acks_capability() {
        let factory = Arc::new(FakeFactory::new(
            FakeProvider::new().text("only answer").text("second"),
        ));
        let (endpoint, server) = spawn_server(factory, 2);

        // A client without `acks` sends a keyed prompt: refused, nothing runs,
        // nothing is remembered.
        let (read1, mut write1) = tokio::io::split(dial(&endpoint).await);
        let mut reader1 = JsonRecordReader::new(read1);
        send_command(
            &mut write1,
            ClientCommand::Attach {
                target: AttachTarget::OpenNew,
                capabilities: Vec::new(),
                since: None,
            },
        )
        .await
        .unwrap();
        let (id, instance) = match read_event(&mut reader1).await {
            ServerEvent::Attached {
                session_id,
                instance,
                ..
            } => (session_id, instance),
            other => panic!("expected `attached`, got {other:?}"),
        };
        send_keyed(&mut write1, "p1", &instance, 1, prompt("go")).await;
        let refused = next_record_within(&mut reader1, DEADLINE).await.unwrap();
        assert_eq!(refused.id.as_deref(), Some("p1"));
        assert!(
            matches!(refused.event, ServerEvent::Error { .. }),
            "a legacy client gets the record type it knows"
        );
        assert!(next_record_within(&mut reader1, Duration::from_millis(300))
            .await
            .is_none());

        // The same key from an `acks` client then runs normally: the refused
        // copy reserved nothing.
        let (read2, mut write2) = tokio::io::split(dial(&endpoint).await);
        let mut reader2 = JsonRecordReader::new(read2);
        attach_for_retries(
            &mut reader2,
            &mut write2,
            AttachTarget::ResumeId { session_id: id },
        )
        .await;
        send_keyed(&mut write2, "p1", &instance, 1, prompt("go")).await;
        let records = read_records_until(&mut reader2, is_stop).await;
        assert!(records
            .iter()
            .any(|record| record.id.as_deref() == Some("p1")
                && matches!(record.event, ServerEvent::Accepted { .. })));

        drop((write1, reader1, write2, reader2));
        let _ = timeout(DEADLINE, server).await;
    }

    // --- discovery and typed slash commands -----------------------------------

    fn run(name: &str, args: &str) -> ClientCommand {
        ClientCommand::RunCommand {
            name: name.to_string(),
            args: args.to_string(),
        }
    }

    /// The first record that answers correlation id `id`.
    async fn reply_to(
        reader: &mut JsonRecordReader<impl AsyncRead + Unpin>,
        id: &str,
    ) -> ServerEvent {
        loop {
            let record = next_record_within(reader, DEADLINE)
                .await
                .expect("a reply arrived");
            if record.id.as_deref() == Some(id) {
                return record.event;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_catalogue_lists_only_remote_commands_with_their_scope() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let (endpoint, server) = spawn_server(factory, 1);
        let (read, mut write) = tokio::io::split(dial(&endpoint).await);
        let mut reader = JsonRecordReader::new(read);
        send_command(
            &mut write,
            ClientCommand::Attach {
                target: AttachTarget::OpenNew,
                capabilities: acks(),
                since: None,
            },
        )
        .await
        .unwrap();
        match read_event(&mut reader).await {
            ServerEvent::Attached { capabilities, .. } => {
                assert_eq!(capabilities, ["acks", "resume", "retry", "commands"]);
            }
            other => panic!("expected `attached`, got {other:?}"),
        }

        send_with_id(&mut write, "c1", ClientCommand::Commands).await;
        match reply_to(&mut reader, "c1").await {
            ServerEvent::Commands { items } => {
                let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
                assert_eq!(names, ["effort", "name", "rename", "clear", "compact"]);
                assert!(items.iter().all(|item| item.scope == "control"));
                let shape = |name: &str| {
                    items
                        .iter()
                        .find(|item| item.name == name)
                        .map(|item| item.args.clone())
                        .unwrap()
                };
                assert_eq!(shape("clear"), "none");
                assert_eq!(shape("compact"), "optional");
                assert_eq!(shape("name"), "required");
                assert!(!names.contains(&"bypass") && !names.contains(&"model"));
            }
            other => panic!("expected `commands`, got {other:?}"),
        }

        drop((write, reader));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_remote_command_changes_session_state_and_unknown_ones_never_reach_the_model() {
        // The provider answers one turn at most: any command that leaked into
        // a prompt would start a turn the watcher sees.
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let (endpoint, server) = spawn_server(factory, 2);

        let (read0, mut write0) = tokio::io::split(dial(&endpoint).await);
        let mut watcher = JsonRecordReader::new(read0);
        let (id, _) = attach_resume(&mut watcher, &mut write0, AttachTarget::OpenNew, None).await;
        read_event(&mut watcher).await; // initial snapshot

        let (read, mut write) = tokio::io::split(dial(&endpoint).await);
        let mut reader = JsonRecordReader::new(read);
        attach_client_with(
            &mut reader,
            &mut write,
            AttachTarget::ResumeId { session_id: id },
            acks(),
        )
        .await;

        // Naming the session changes what `sessions` reports.
        send_with_id(&mut write, "n1", run("name", "nightly")).await;
        assert!(matches!(
            reply_to(&mut reader, "n1").await,
            ServerEvent::CommandResult { name, .. } if name == "name"
        ));
        send_with_id(&mut write, "s1", ClientCommand::Sessions).await;
        match reply_to(&mut reader, "s1").await {
            ServerEvent::Sessions { items } => {
                let this = items.iter().find(|item| item.id == id).unwrap();
                assert!(this.live);
                assert_eq!(this.name.as_deref(), Some("nightly"));
                assert_eq!(items[0].id, id, "live sessions come first");
            }
            other => panic!("expected `sessions`, got {other:?}"),
        }

        // Clearing reports new context usage to every client.
        send_with_id(&mut write, "c1", run("clear", "")).await;
        assert!(matches!(
            reply_to(&mut reader, "c1").await,
            ServerEvent::CommandResult { .. }
        ));
        let usage = read_records_until(&mut watcher, |event| {
            matches!(event, ServerEvent::ContextUsage { .. })
        })
        .await;
        assert!(
            usage.last().unwrap().seq.is_some(),
            "through the numbered feed"
        );

        // Unknown, local-only and malformed commands are refused, and the name
        // field cannot smuggle arguments.
        for (rid, command, code) in [
            ("u4", run("compact force", ""), RejectCode::UnknownCommand),
            ("u5", run("name nightly", ""), RejectCode::UnknownCommand),
            ("u6", run("/clear", ""), RejectCode::UnknownCommand),
            ("u1", run("bypass", ""), RejectCode::UnknownCommand),
            ("u2", run("nope", "x"), RejectCode::UnknownCommand),
            ("u3", run("effort", "extreme"), RejectCode::InvalidArguments),
        ] {
            send_with_id(&mut write, rid, command).await;
            match reply_to(&mut reader, rid).await {
                ServerEvent::Rejected { code: got, .. } => assert_eq!(got, code, "{rid}"),
                other => panic!("{rid}: expected a rejection, got {other:?}"),
            }
        }
        // None of it started a turn.
        let mut started = false;
        while let Some(record) = next_record_within(&mut watcher, Duration::from_millis(300)).await
        {
            started |= record.event == ServerEvent::TurnStarted;
        }
        assert!(!started, "no command reached the model as a prompt");

        drop((write0, watcher, write, reader));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_remote_command_waits_for_no_turn_and_is_refused_while_busy() {
        let factory = Arc::new(FakeFactory::asking(asking_provider(&[])));
        let (endpoint, server) = spawn_server(factory, 1);
        let (read, mut write) = tokio::io::split(dial(&endpoint).await);
        let mut reader = JsonRecordReader::new(read);
        attach_client_with(&mut reader, &mut write, AttachTarget::OpenNew, acks()).await;
        send_with_id(&mut write, "p1", prompt("clean up")).await;
        // The turn parks on its ask, holding the session.
        let ask = ask_id_of(&read_until(&mut reader, is_ask).await);

        send_with_id(&mut write, "k1", run("compact", "")).await;
        assert!(matches!(
            reply_to(&mut reader, "k1").await,
            ServerEvent::Rejected {
                code: RejectCode::Busy,
                ..
            }
        ));

        send_with_id(
            &mut write,
            "r1",
            ClientCommand::PermissionReply {
                ask_id: ask,
                allow: false,
            },
        )
        .await;
        read_until(&mut reader, is_stop).await;
        drop((write, reader));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_legacy_client_running_a_command_gets_only_known_records() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let (endpoint, server) = spawn_server(factory, 1);
        let (read, mut write) = tokio::io::split(dial(&endpoint).await);
        let mut reader = JsonRecordReader::new(read);
        attach_client(&mut reader, &mut write, AttachTarget::OpenNew).await;

        send_with_id(&mut write, "c1", run("clear", "")).await;
        send_with_id(&mut write, "u1", run("bypass", "")).await;
        // Read until both the refusal and the context usage `clear` publishes
        // have arrived, so nothing is written after the client goes away.
        let (mut refused, mut usage) = (false, false);
        while !(refused && usage) {
            let record = next_record_within(&mut reader, DEADLINE).await.unwrap();
            match (record.id.as_deref(), &record.event) {
                (Some("u1"), ServerEvent::Error { .. }) => refused = true,
                (Some("u1"), other) => panic!("a legacy refusal is an `error`, got {other:?}"),
                (_, ServerEvent::ContextUsage { .. }) => usage = true,
                (_, ServerEvent::CommandResult { .. }) => {
                    panic!("a legacy client gets no `command_result`")
                }
                _ => {}
            }
        }
        drop((write, reader));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_retried_remote_command_runs_once() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let (endpoint, server) = spawn_server(factory, 1);
        let (read, mut write) = tokio::io::split(dial(&endpoint).await);
        let mut reader = JsonRecordReader::new(read);
        let (_, instance) =
            attach_for_retries(&mut reader, &mut write, AttachTarget::OpenNew).await;

        send_keyed(&mut write, "n1", &instance, 1, run("name", "first")).await;
        assert!(matches!(
            reply_to(&mut reader, "n1").await,
            ServerEvent::CommandResult { .. }
        ));
        // The same request under a new id replays; a new number with another
        // payload runs.
        send_keyed(&mut write, "n1-again", &instance, 1, run("name", "first")).await;
        assert!(matches!(
            reply_to(&mut reader, "n1-again").await,
            ServerEvent::CommandResult { .. }
        ));
        send_keyed(&mut write, "n1-other", &instance, 1, run("name", "second")).await;
        assert!(matches!(
            reply_to(&mut reader, "n1-other").await,
            ServerEvent::Rejected {
                code: RejectCode::IdReused,
                ..
            }
        ));
        drop((write, reader));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    #[test]
    fn a_pairing_url_must_be_a_bare_https_origin() {
        assert_eq!(
            pairing_origin("https://pc.example.ts.net").unwrap(),
            "https://pc.example.ts.net"
        );
        assert_eq!(
            pairing_origin("https://pc.example.ts.net:8443/").unwrap(),
            "https://pc.example.ts.net:8443"
        );
        for bad in [
            "http://pc.example.ts.net",
            "https://user:pw@pc.example.ts.net",
            "https://pc.example.ts.net/?next=evil",
            "https://pc.example.ts.net/#x",
            "https://pc.example.ts.net/some/path",
            "ftp://pc.example.ts.net",
            "not a url",
        ] {
            assert!(pairing_origin(bad).is_err(), "{bad} must be refused");
        }
    }

    // --- paired devices ---------------------------------------------------------

    /// Open a connection and send `command` as its first record.
    async fn first_record(
        endpoint: &Endpoint,
        command: ClientCommand,
    ) -> (
        JsonRecordReader<tokio::io::ReadHalf<Conn>>,
        tokio::io::WriteHalf<Conn>,
        ServerEvent,
    ) {
        let (read, mut write) = tokio::io::split(dial(endpoint).await);
        let mut reader = JsonRecordReader::new(read);
        send_command(&mut write, command).await.unwrap();
        let reply = read_event(&mut reader).await;
        (reader, write, reply)
    }

    /// Register a gateway; returns its control channel (keep it open) and the
    /// capability.
    async fn register_gateway(
        endpoint: &Endpoint,
    ) -> (
        (
            JsonRecordReader<tokio::io::ReadHalf<Conn>>,
            tokio::io::WriteHalf<Conn>,
        ),
        String,
    ) {
        let (reader, write, reply) = first_record(endpoint, ClientCommand::GatewayRegister).await;
        match reply {
            ServerEvent::GatewayRegistered { capability } => {
                ((reader, write), capability.expose().to_string())
            }
            other => panic!("expected `gateway_registered`, got {other:?}"),
        }
    }

    /// Pair a device through the gateway; returns its id and credential.
    async fn pair_device(
        endpoint: &Endpoint,
        hosts: &HostMap,
        capability: &str,
        scope: localpilot_rpc::Scope,
    ) -> (String, String) {
        let code = hosts.pairings.begin("phone", scope, false).unwrap();
        let (_, _, reply) = first_record(
            endpoint,
            ClientCommand::PairExchange {
                capability: localpilot_rpc::WireSecret::new(capability),
                pairing: localpilot_rpc::WireSecret::new(code.expose()),
            },
        )
        .await;
        match reply {
            ServerEvent::DevicePaired {
                device_id,
                credential,
                ..
            } => (device_id, credential.expose().to_string()),
            other => panic!("expected `device_paired`, got {other:?}"),
        }
    }

    fn device_attach(capability: &str, credential: &str, target: AttachTarget) -> ClientCommand {
        ClientCommand::DeviceAttach {
            capability: localpilot_rpc::WireSecret::new(capability),
            credential: localpilot_rpc::WireSecret::new(credential),
            target,
            capabilities: resume_caps(),
            since: None,
        }
    }

    /// A local session to attach devices to.
    async fn local_session(
        endpoint: &Endpoint,
    ) -> (
        JsonRecordReader<tokio::io::ReadHalf<Conn>>,
        tokio::io::WriteHalf<Conn>,
        SessionId,
    ) {
        let (read, mut write) = tokio::io::split(dial(endpoint).await);
        let mut reader = JsonRecordReader::new(read);
        let id = attach_client_with(&mut reader, &mut write, AttachTarget::OpenNew, acks()).await;
        (reader, write, id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_observing_device_may_watch_but_not_act() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let hosts = HostMap::default();
        let (endpoint, server) = spawn_server_on(factory, 5, hosts.clone());
        let (local_reader, local_write, session) = local_session(&endpoint).await;
        let (gateway, capability) = register_gateway(&endpoint).await;
        let (_, credential) = pair_device(
            &endpoint,
            &hosts,
            &capability,
            localpilot_rpc::Scope::Observe,
        )
        .await;

        // Watching an existing session: allowed.
        let (mut reader, mut write, reply) = first_record(
            &endpoint,
            device_attach(
                &capability,
                &credential,
                AttachTarget::ResumeId {
                    session_id: session,
                },
            ),
        )
        .await;
        assert!(matches!(reply, ServerEvent::Attached { .. }), "{reply:?}");
        read_event(&mut reader).await; // snapshot
        send_with_id(&mut write, "s1", ClientCommand::Status).await;
        assert!(matches!(
            reply_to(&mut reader, "s1").await,
            ServerEvent::Status { .. }
        ));
        // Acting: refused, every kind.
        for (rid, command) in [
            ("p1", prompt("go")),
            ("c1", ClientCommand::Cancel),
            ("r1", run("name", "x")),
            (
                "a1",
                ClientCommand::PermissionReply {
                    ask_id: "ask-1".to_string(),
                    allow: true,
                },
            ),
            ("g1", ClientCommand::GatewayRegister),
        ] {
            send_with_id(&mut write, rid, command).await;
            assert!(
                matches!(
                    reply_to(&mut reader, rid).await,
                    ServerEvent::Rejected {
                        code: RejectCode::Forbidden,
                        ..
                    }
                ),
                "{rid}"
            );
        }
        drop((reader, write));

        // Opening a new session needs `control`: refused, and never local.
        let (_, _, reply) = first_record(
            &endpoint,
            device_attach(&capability, &credential, AttachTarget::OpenNew),
        )
        .await;
        assert!(matches!(reply, ServerEvent::Error { .. }), "{reply:?}");

        drop((gateway, local_reader, local_write));
        let _ = timeout(DEADLINE, server).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_controlling_device_acts_under_its_own_identity() {
        let factory = Arc::new(FakeFactory::asking(asking_provider(&[])));
        let hosts = HostMap::default();
        let (endpoint, server) = spawn_server_on(factory, 3, hosts.clone());
        let (gateway, capability) = register_gateway(&endpoint).await;
        let (device_id, credential) = pair_device(
            &endpoint,
            &hosts,
            &capability,
            localpilot_rpc::Scope::Control,
        )
        .await;
        let (mut reader, mut write, reply) = first_record(
            &endpoint,
            device_attach(&capability, &credential, AttachTarget::OpenNew),
        )
        .await;
        assert!(matches!(reply, ServerEvent::Attached { .. }), "{reply:?}");
        read_event(&mut reader).await; // snapshot

        send_with_id(&mut write, "p1", prompt("clean up")).await;
        let ask = ask_id_of(&read_until(&mut reader, is_ask).await);
        send_with_id(
            &mut write,
            "r1",
            ClientCommand::PermissionReply {
                ask_id: ask,
                allow: false,
            },
        )
        .await;
        let rest = read_until(&mut reader, is_stop).await;
        assert!(
            rest.iter().any(|event| matches!(
                event,
                ServerEvent::PermissionResolved {
                    by: Some(Principal::Device { id }),
                    ..
                } if *id == device_id
            )),
            "the answer is attributed to the device, not to this computer: {rest:?}"
        );
        assert!(hosts
            .devices
            .audit_lines()
            .iter()
            .any(|line| line.contains("\"attached\"") && line.contains(&device_id)));

        drop((reader, write, gateway));
        let _ = timeout(DEADLINE, server).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_device_connection_never_falls_back_to_local() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let hosts = HostMap::default();
        let (endpoint, server) = spawn_server_on(factory, 6, hosts.clone());
        let (gateway, capability) = register_gateway(&endpoint).await;
        let (device_id, credential) = pair_device(
            &endpoint,
            &hosts,
            &capability,
            localpilot_rpc::Scope::Control,
        )
        .await;
        let forged = format!("lpd1.{device_id}.{}", "0".repeat(64));

        for (label, command) in [
            (
                "no capability",
                device_attach("lpg1.forged", &credential, AttachTarget::OpenNew),
            ),
            (
                "bad credential",
                device_attach(&capability, &forged, AttachTarget::OpenNew),
            ),
            (
                "garbage credential",
                device_attach(&capability, "nope", AttachTarget::OpenNew),
            ),
        ] {
            let (mut reader, _write, reply) = first_record(&endpoint, command).await;
            assert!(
                matches!(reply, ServerEvent::Error { .. }),
                "{label}: {reply:?}"
            );
            assert!(
                timeout(DEADLINE, reader.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .is_none(),
                "{label}: the connection ends"
            );
        }

        // The gateway's control channel carries no session commands.
        let ((mut control_reader, mut control_write), _) = (gateway, ());
        send_with_id(&mut control_write, "x1", prompt("go")).await;
        assert!(matches!(
            reply_to(&mut control_reader, "x1").await,
            ServerEvent::Rejected {
                code: RejectCode::Forbidden,
                ..
            }
        ));

        // Its capability dies with it, and so do the device connections on it.
        let (mut device, _device_write, reply) = first_record(
            &endpoint,
            device_attach(&capability, &credential, AttachTarget::OpenNew),
        )
        .await;
        assert!(matches!(reply, ServerEvent::Attached { .. }));
        read_event(&mut device).await; // snapshot
        drop((control_reader, control_write));
        let closed = read_records_until(&mut device, |event| {
            matches!(event, ServerEvent::SessionClosed { .. })
        })
        .await;
        assert!(closed.iter().any(|record| matches!(
            &record.event,
            ServerEvent::SessionClosed { reason } if reason.contains("gateway")
        )));
        let _ = timeout(DEADLINE, server).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_revoked_device_stops_receiving_and_cannot_retry() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let hosts = HostMap::default();
        let (endpoint, server) = spawn_server_on(factory, 4, hosts.clone());
        let (gateway, capability) = register_gateway(&endpoint).await;
        let (device_id, credential) = pair_device(
            &endpoint,
            &hosts,
            &capability,
            localpilot_rpc::Scope::Control,
        )
        .await;
        let (mut reader, _write, reply) = first_record(
            &endpoint,
            device_attach(&capability, &credential, AttachTarget::OpenNew),
        )
        .await;
        assert!(matches!(reply, ServerEvent::Attached { .. }));
        read_event(&mut reader).await; // snapshot

        // Revoked elsewhere (the CLI, another workspace): the open stream ends
        // within the standing tick, without the device sending anything.
        assert!(hosts.devices.revoke(&device_id).unwrap());
        let closed = timeout(Duration::from_secs(5), async {
            loop {
                if let ServerEvent::SessionClosed { reason } = read_event(&mut reader).await {
                    break reason;
                }
            }
        })
        .await
        .expect("closed promptly");
        assert!(closed.contains("revoked"));

        // A new attach with the same credential is refused.
        let (_, _, reply) = first_record(
            &endpoint,
            device_attach(&capability, &credential, AttachTarget::OpenNew),
        )
        .await;
        assert!(matches!(reply, ServerEvent::Error { .. }));
        drop(gateway);
        let _ = timeout(DEADLINE, server).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn no_secret_reaches_the_audit_log_or_an_error() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let hosts = HostMap::default();
        let (endpoint, server) = spawn_server_on(factory, 4, hosts.clone());
        let (gateway, capability) = register_gateway(&endpoint).await;
        let code = hosts
            .pairings
            .begin("phone", localpilot_rpc::Scope::Control, false)
            .unwrap();
        let wrong = format!(
            "{}.{}",
            code.expose().split('.').next().unwrap(),
            "f".repeat(64)
        );
        let (_, _, refused) = first_record(
            &endpoint,
            ClientCommand::PairExchange {
                capability: localpilot_rpc::WireSecret::new(capability.clone()),
                pairing: localpilot_rpc::WireSecret::new(wrong.clone()),
            },
        )
        .await;
        let (_, _, paired) = first_record(
            &endpoint,
            ClientCommand::PairExchange {
                capability: localpilot_rpc::WireSecret::new(capability.clone()),
                pairing: localpilot_rpc::WireSecret::new(code.expose()),
            },
        )
        .await;
        let ServerEvent::DevicePaired { credential, .. } = paired else {
            panic!("expected `device_paired`, got {paired:?}");
        };
        let credential = credential.expose().to_string();
        let (_, _, bad) = first_record(
            &endpoint,
            device_attach(
                &capability,
                &format!("{credential}x"),
                AttachTarget::OpenNew,
            ),
        )
        .await;

        let mut seen = hosts.devices.audit_lines().join("\n");
        seen.push_str(&format!("{refused:?} {bad:?}"));
        let code_secret = code.expose().split('.').nth(1).unwrap();
        let credential_secret = credential.rsplit('.').next().unwrap();
        for secret in [code_secret, credential_secret, &capability, &wrong] {
            assert!(!seen.contains(secret), "a secret leaked: {seen}");
        }
        drop(gateway);
        let _ = timeout(DEADLINE, server).await;
    }

    #[test]
    fn a_malformed_record_never_echoes_what_it_carried() {
        let sentinel = "SENTINEL-credential-text";
        for raw in [
            format!(
                r#"{{"v":1,"command":{{"type":"device_attach","capability":"{sentinel}","credential":7}}}}"#
            ),
            format!(
                r#"{{"v":1,"command":{{"type":"pair_begin","name":"x","scope":"{sentinel}"}}}}"#
            ),
            format!(r#"{{"v":1,"command":{{"type":"{sentinel}"}}}}"#),
        ] {
            let error = serde_json::from_str::<ClientRecord>(&raw).unwrap_err();
            let message = malformed(&error);
            assert!(!message.contains(sentinel), "{message}");
            assert!(message.starts_with("malformed record"));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_closed_gateway_creates_no_device_and_attaches_nothing_after_its_wait() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let hosts = HostMap::default();
        let (endpoint, server) = spawn_server_on(factory, 4, hosts.clone());
        let (gateway, capability) = register_gateway(&endpoint).await;
        let (_, credential) = pair_device(
            &endpoint,
            &hosts,
            &capability,
            localpilot_rpc::Scope::Control,
        )
        .await;
        let code = hosts
            .pairings
            .begin("tablet", localpilot_rpc::Scope::Control, false)
            .unwrap();

        // Every handshake slot is taken: both requests below wait.
        let held = hosts.handshakes.0.acquire_many(16).await.unwrap();
        let pairing = tokio::spawn({
            let endpoint = endpoint.clone();
            let capability = capability.clone();
            let code = code.expose().to_string();
            async move {
                first_record(
                    &endpoint,
                    ClientCommand::PairExchange {
                        capability: localpilot_rpc::WireSecret::new(capability),
                        pairing: localpilot_rpc::WireSecret::new(code),
                    },
                )
                .await
                .2
            }
        });
        let attach = tokio::spawn({
            let endpoint = endpoint.clone();
            let capability = capability.clone();
            async move {
                first_record(
                    &endpoint,
                    device_attach(&capability, &credential, AttachTarget::OpenNew),
                )
                .await
                .2
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        // The gateway's control channel closes while they wait.
        drop(gateway);
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(held);

        let pairing = timeout(DEADLINE, pairing).await.unwrap().unwrap();
        let attach = timeout(DEADLINE, attach).await.unwrap().unwrap();
        assert!(matches!(pairing, ServerEvent::Error { .. }), "{pairing:?}");
        assert!(matches!(attach, ServerEvent::Error { .. }), "{attach:?}");
        assert_eq!(
            hosts.devices.list().unwrap().len(),
            1,
            "no device was created under the closed gateway"
        );
        assert!(hosts.inner.lock().await.is_empty(), "no session was opened");
        let _ = timeout(DEADLINE, server).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn access_lost_while_waiting_to_bind_opens_and_sends_nothing() {
        for revoke in [false, true] {
            let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
            let hosts = HostMap::default();
            let (endpoint, server) = spawn_server_on(factory, 3, hosts.clone());
            let (gateway, capability) = register_gateway(&endpoint).await;
            let (device_id, credential) = pair_device(
                &endpoint,
                &hosts,
                &capability,
                localpilot_rpc::Scope::Control,
            )
            .await;

            // Hold the host map: the device's handshake succeeds, then waits
            // to bind.
            let map = hosts.inner.lock().await;
            let attach = tokio::spawn({
                let endpoint = endpoint.clone();
                let capability = capability.clone();
                async move {
                    first_record(
                        &endpoint,
                        device_attach(&capability, &credential, AttachTarget::OpenNew),
                    )
                    .await
                    .2
                }
            });
            tokio::time::sleep(Duration::from_millis(300)).await;
            // Access ends while it waits.
            let _gateway = if revoke {
                assert!(hosts.devices.revoke(&device_id).unwrap());
                Some(gateway)
            } else {
                drop(gateway);
                None
            };
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(map);

            let reply = timeout(DEADLINE, attach).await.unwrap().unwrap();
            assert!(
                matches!(reply, ServerEvent::Error { .. }),
                "revoke={revoke}: refused, got {reply:?}"
            );
            assert!(
                hosts.inner.lock().await.is_empty(),
                "revoke={revoke}: no session was opened"
            );
            drop(_gateway);
            let _ = timeout(DEADLINE, server).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_device_revoked_while_its_first_write_is_blocked_gets_nothing_more() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let hosts = HostMap::default();
        let registry = SessionRegistry::new();
        let store = factory.store();
        let lease = hosts.gateways.register().unwrap();
        let capability = lease.capability.expose().to_string();
        let (standing, credential) = hosts
            .devices
            .add("phone", localpilot_rpc::Scope::Control, false)
            .unwrap();

        // Client to server: roomy. Server to client: 32 bytes, so the
        // `attached` record blocks part-way while nobody reads.
        let (server_read, mut client_write) = tokio::io::duplex(64 * 1024);
        let (mut client_read, server_write) = tokio::io::duplex(32);
        let connection = tokio::spawn({
            let hosts = hosts.clone();
            let factory = factory.clone();
            async move {
                serve_connection(
                    server_read,
                    server_write,
                    &registry,
                    &hosts,
                    factory.as_ref(),
                    &store,
                    Profile::Bypass,
                )
                .await
            }
        });
        send_command(
            &mut client_write,
            ClientCommand::DeviceAttach {
                capability: localpilot_rpc::WireSecret::new(capability),
                credential: localpilot_rpc::WireSecret::new(credential.expose()),
                target: AttachTarget::OpenNew,
                capabilities: resume_caps(),
                since: None,
            },
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Revoked from elsewhere while the write is blocked; wait past the
        // poll bound, then drain what reached the client.
        assert!(hosts.devices.revoke(&standing.id).unwrap());
        tokio::time::sleep(ACCESS_POLL * 3).await;
        let mut received = Vec::new();
        timeout(
            DEADLINE,
            tokio::io::AsyncReadExt::read_to_end(&mut client_read, &mut received),
        )
        .await
        .expect("the connection was dropped")
        .unwrap();
        timeout(DEADLINE, connection)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            received.len() <= 32,
            "nothing beyond what was already buffered: {} bytes",
            received.len()
        );
        let text = String::from_utf8_lossy(&received);
        assert!(!text.contains("snapshot"), "no snapshot was sent: {text}");
        drop(lease);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_device_revoked_while_a_live_write_is_blocked_gets_nothing_more() {
        let answer = "y".repeat(16 * 1024);
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text(&answer)));
        let hosts = HostMap::default();
        let registry = SessionRegistry::new();
        let store = factory.store();
        let lease = hosts.gateways.register().unwrap();
        let capability = lease.capability.expose().to_string();
        let (standing, credential) = hosts
            .devices
            .add("phone", localpilot_rpc::Scope::Control, false)
            .unwrap();

        // Server to client: 4 KiB, enough for the attachment but not for the
        // 16 KiB answer, which blocks once the client stops reading.
        let (server_read, mut client_write) = tokio::io::duplex(64 * 1024);
        let (client_read, server_write) = tokio::io::duplex(4 * 1024);
        let connection = tokio::spawn({
            let hosts = hosts.clone();
            let factory = factory.clone();
            async move {
                serve_connection(
                    server_read,
                    server_write,
                    &registry,
                    &hosts,
                    factory.as_ref(),
                    &store,
                    Profile::Bypass,
                )
                .await
            }
        });
        let mut reader = JsonRecordReader::new(client_read);
        send_command(
            &mut client_write,
            ClientCommand::DeviceAttach {
                capability: localpilot_rpc::WireSecret::new(capability),
                credential: localpilot_rpc::WireSecret::new(credential.expose()),
                target: AttachTarget::OpenNew,
                capabilities: resume_caps(),
                since: None,
            },
        )
        .await
        .unwrap();
        read_until(&mut reader, |event| {
            matches!(event, ServerEvent::Snapshot { .. })
        })
        .await;

        // Attached and caught up; now a turn's answer is written while
        // nobody reads.
        send_command(
            &mut client_write,
            ClientCommand::Prompt {
                text: "go".to_string(),
                disposition: InputDisposition::Immediate,
            },
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Revoked from elsewhere while the live write is blocked; wait past
        // the poll bound, then drain what reached the client.
        assert!(hosts.devices.revoke(&standing.id).unwrap());
        tokio::time::sleep(ACCESS_POLL * 3).await;
        let mut after = Vec::new();
        // Until EOF, or the record cut off when the connection was dropped.
        while let Ok(Some(value)) = timeout(DEADLINE, reader.next())
            .await
            .expect("the connection was dropped")
        {
            after.push(value);
        }
        timeout(DEADLINE, connection)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let delivered: usize = after
            .iter()
            .filter_map(|value| serde_json::from_value::<ServerRecord>(value.clone()).ok())
            .map(|record| match record.event {
                ServerEvent::TextDelta { text } => text.len(),
                _ => 0,
            })
            .sum();
        assert!(
            delivered < answer.len(),
            "the blocked answer was not delivered after revocation"
        );
        drop(lease);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_device_revoked_while_its_command_runs_never_gets_the_reply() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new()));
        let hosts = HostMap::default();
        let registry = SessionRegistry::new();
        let store = factory.store();
        let lease = hosts.gateways.register().unwrap();
        let capability = lease.capability.expose().to_string();
        let (standing, credential) = hosts
            .devices
            .add("phone", localpilot_rpc::Scope::Control, false)
            .unwrap();

        let (server_read, mut client_write) = tokio::io::duplex(64 * 1024);
        let (client_read, server_write) = tokio::io::duplex(64 * 1024);
        let connection = tokio::spawn({
            let hosts = hosts.clone();
            let factory = factory.clone();
            async move {
                serve_connection(
                    server_read,
                    server_write,
                    &registry,
                    &hosts,
                    factory.as_ref(),
                    &store,
                    Profile::Bypass,
                )
                .await
            }
        });
        let mut reader = JsonRecordReader::new(client_read);
        send_command(
            &mut client_write,
            ClientCommand::DeviceAttach {
                capability: localpilot_rpc::WireSecret::new(capability),
                credential: localpilot_rpc::WireSecret::new(credential.expose()),
                target: AttachTarget::OpenNew,
                capabilities: resume_caps(),
                since: None,
            },
        )
        .await
        .unwrap();
        let attachment = read_until(&mut reader, |event| {
            matches!(event, ServerEvent::Snapshot { .. })
        })
        .await;
        let instance = attachment
            .iter()
            .find_map(|event| match event {
                ServerEvent::Attached { instance, .. } => Some(instance.clone()),
                _ => None,
            })
            .unwrap();

        // Hold the session list so the command is still running when the
        // device is revoked; the transport stays writable throughout.
        let held = hosts.inner.lock().await;
        send_keyed(
            &mut client_write,
            "s1",
            &instance,
            1,
            ClientCommand::Sessions,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(hosts.devices.revoke(&standing.id).unwrap());
        drop(held);

        let mut after = Vec::new();
        // Until EOF, or a record cut off when the connection was dropped.
        while let Ok(Some(value)) = timeout(DEADLINE, reader.next())
            .await
            .expect("the connection was dropped")
        {
            after.push(value);
        }
        timeout(DEADLINE, connection)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let replied = after
            .iter()
            .filter_map(|value| serde_json::from_value::<ServerRecord>(value.clone()).ok())
            .any(|record| matches!(record.event, ServerEvent::Sessions { .. }));
        assert!(
            !replied,
            "the reply was delivered after revocation: {after:?}"
        );
        drop(lease);
    }

    // --- the plain-text client renderer --------------------------------------

    fn detail(id: &str) -> localpilot_rpc::AskDetail {
        localpilot_rpc::AskDetail {
            ask_id: id.to_string(),
            tool: "run_shell".to_string(),
            detail: "cargo test".to_string(),
            risk: "run a command".to_string(),
            expires_at_ms: 0,
        }
    }

    #[test]
    fn an_empty_resync_clears_a_stale_ask_without_a_resolution() {
        // The shown ask expired and its outcome aged out of the server's memory,
        // so no `permission_resolved` precedes the resync.
        let mut last_ask = Some("ask-old".to_string());
        render_event(ServerEvent::AsksResync { asks: Vec::new() }, &mut last_ask).unwrap();
        assert_eq!(last_ask, None, "/allow no longer targets the gone ask");
    }

    #[test]
    fn a_resync_exposes_an_ask_only_it_announced() {
        let mut last_ask = Some("ask-old".to_string());
        render_event(
            ServerEvent::AsksResync {
                asks: vec![detail("ask-new")],
            },
            &mut last_ask,
        )
        .unwrap();
        assert_eq!(last_ask.as_deref(), Some("ask-new"));
    }

    #[test]
    fn a_resync_keeps_a_still_pending_ask_selected() {
        let mut last_ask = Some("ask-a".to_string());
        render_event(
            ServerEvent::AsksResync {
                asks: vec![detail("ask-a"), detail("ask-b")],
            },
            &mut last_ask,
        )
        .unwrap();
        assert_eq!(last_ask.as_deref(), Some("ask-a"));
    }

    #[test]
    fn render_event_tracks_a_permission_ask_and_signals_close() {
        let mut last_ask = None;
        assert!(
            !render_event(ServerEvent::TextDelta { text: "hi".into() }, &mut last_ask).unwrap()
        );
        render_event(
            ServerEvent::PermissionAsk {
                ask_id: "ask-9".into(),
                tool: "run_shell".into(),
                detail: "ls".into(),
                risk: "run a command".into(),
                expires_at_ms: 0,
            },
            &mut last_ask,
        )
        .unwrap();
        assert_eq!(
            last_ask.as_deref(),
            Some("ask-9"),
            "the ask id is tracked for a later /allow or /deny"
        );
        assert!(
            render_event(ServerEvent::Closed, &mut last_ask).unwrap(),
            "a `closed` event signals the client to stop"
        );
    }

    // --- shared resource pool -----------------------------------------------

    /// Register a session and wrap it in a host, mirroring what `HostMap::bind`
    /// does on a fresh attach, but directly — so a reaper test controls the
    /// registry and host map without a wire round-trip.
    async fn open_hosted_session(
        factory: &Arc<FakeFactory>,
        registry: &SessionRegistry,
        hosts: &HostMap,
    ) -> (SessionId, Arc<SessionHost>) {
        let id = registry.open_new(factory.as_ref()).await.unwrap();
        let handle = registry.get(id).await.unwrap();
        let host = Arc::new(SessionHost::new(handle).await);
        hosts.inner.lock().await.insert(id, host.clone());
        (id, host)
    }

    /// One `SessionSetup` builds many sessions that share its single provider
    /// stack (not a fresh provider each), while their mutable per-session state
    /// stays isolated: a turn on A does not appear in B.
    ///
    /// A real-MCP variant would additionally assert both sessions' tool calls
    /// reach the *one* connected MCP transport; that pool-identity property is
    /// covered offline by `one_mcp_connection_pool_is_shared_across_registries`
    /// in `crate::mcp` (no MCP subprocess needed).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_setup_shares_the_provider_pool_across_isolated_sessions() {
        use localpilot_config::Config;

        let dir = tempfile::tempdir().unwrap();
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new().text("alpha only"));
        let setup = SessionSetup {
            config: Config::default(),
            cwd: dir.path().to_path_buf(),
            provider: provider.clone(),
            model: "fake-model".to_string(),
            profile: Profile::Bypass,
            mcp: crate::mcp::McpTools::default(),
            agents: None,
            context_windows: HashMap::new(),
        };

        // The one provider stack the setup captured, before any session is built.
        let shared = Arc::strong_count(&provider);

        let mut a = setup.build().unwrap();
        let b = setup.build().unwrap();

        // Both sessions cloned the *one* provider Arc — the stack was shared, not
        // rebuilt. Two builds add exactly two strong refs on the same allocation.
        assert_eq!(
            Arc::strong_count(&provider),
            shared + 2,
            "each built session shares the one provider pool, not a fresh provider"
        );

        // Session isolation still holds: a turn on A must not appear in B.
        let (events, _rx) = broadcast::channel::<RuntimeEvent>(16);
        let cancel = CancellationToken::new();
        a.runtime.run_turn("hi", &events, &cancel).await;

        assert_eq!(
            a.runtime.last_assistant_text().as_deref(),
            Some("alpha only"),
            "A ran its turn against the shared provider"
        );
        assert_eq!(
            b.runtime.last_assistant_text(),
            None,
            "a turn on A must not bleed into B"
        );
    }

    // --- session reaping ----------------------------------------------------

    /// A session whose client detached and whose grace has elapsed is reaped:
    /// gone from the registry and host map, event log persisted first.
    #[tokio::test]
    async fn a_detached_session_past_grace_is_reaped_and_persisted() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("bye")));
        let store = factory.store();
        let registry = SessionRegistry::new();
        let hosts = HostMap::default();
        let (id, _host) = open_hosted_session(&factory, &registry, &hosts).await;

        // No client ever attached (subscriber_count == 0); with zero grace the
        // first scan reaps it.
        let mut reaper = Reaper::new(
            registry.clone(),
            hosts.clone(),
            ReaperConfig {
                grace: Duration::ZERO,
                idle: Duration::from_secs(3600),
                tick: Duration::from_secs(1),
            },
        );
        let reaped = reaper.reap_once(Instant::now()).await;

        assert_eq!(
            reaped,
            vec![id],
            "the detached, past-grace session was reaped"
        );
        assert!(registry.get(id).await.is_none(), "gone from the registry");
        assert!(
            hosts.inner.lock().await.get(&id).is_none(),
            "gone from the host map"
        );
        let events = store.read_events(id).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.kind == SessionEventKind::SessionClosed),
            "the reaped session persisted its close first: {events:?}"
        );
    }

    /// A session with an in-flight turn (its mutex held, exactly as `drive`
    /// holds it for the whole turn) is never reaped, even past every deadline.
    #[tokio::test]
    async fn a_busy_session_is_not_reaped_even_past_the_deadline() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("busy")));
        let registry = SessionRegistry::new();
        let hosts = HostMap::default();
        let (id, _host) = open_hosted_session(&factory, &registry, &hosts).await;

        // Hold the session mutex, the way a running turn does for its duration.
        let handle = registry.get(id).await.unwrap();
        let _turn_guard = handle.lock().await;

        // Zero grace and zero idle would reap any *free* session at once; the
        // held lock must still protect this one (the reaper only closes what it
        // can `try_lock`).
        let mut reaper = Reaper::new(
            registry.clone(),
            hosts.clone(),
            ReaperConfig {
                grace: Duration::ZERO,
                idle: Duration::ZERO,
                tick: Duration::from_secs(1),
            },
        );
        let reaped = reaper.reap_once(Instant::now()).await;

        assert!(
            reaped.is_empty(),
            "a session with a held turn-lock must never be reaped"
        );
        assert!(
            registry.get(id).await.is_some(),
            "the busy session survives"
        );
        assert!(hosts.inner.lock().await.get(&id).is_some());
    }

    /// A session with a still-attached client is not reaped while it is active,
    /// even with a zero grace window.
    #[tokio::test]
    async fn a_session_with_an_attached_client_is_not_reaped() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("attached")));
        let registry = SessionRegistry::new();
        let hosts = HostMap::default();
        let (id, host) = open_hosted_session(&factory, &registry, &hosts).await;

        // A client attaches: its live receiver keeps subscriber_count at 1.
        let _client = host.subscribe();
        assert_eq!(host.subscriber_count(), 1);

        // Zero grace (would reap a detached session at once) but a long idle
        // window, so this recently-active, still-attached session stays.
        let mut reaper = Reaper::new(
            registry.clone(),
            hosts.clone(),
            ReaperConfig {
                grace: Duration::ZERO,
                idle: Duration::from_secs(3600),
                tick: Duration::from_secs(1),
            },
        );
        let reaped = reaper.reap_once(Instant::now()).await;

        assert!(
            reaped.is_empty(),
            "a session with a live client must not be reaped"
        );
        assert!(registry.get(id).await.is_some());
    }

    /// The idle rule reaps independently of the disconnect grace: a session idle
    /// past the timeout is reaped even with a huge grace window still open.
    #[tokio::test]
    async fn an_idle_session_past_the_idle_timeout_is_reaped() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("idle")));
        let registry = SessionRegistry::new();
        let hosts = HostMap::default();
        let (id, _host) = open_hosted_session(&factory, &registry, &hosts).await;

        let mut reaper = Reaper::new(
            registry.clone(),
            hosts.clone(),
            ReaperConfig {
                grace: Duration::from_secs(3600),
                idle: Duration::ZERO,
                tick: Duration::from_secs(1),
            },
        );
        let reaped = reaper.reap_once(Instant::now()).await;

        assert_eq!(
            reaped,
            vec![id],
            "the idle rule reaps independently of grace"
        );
        assert!(registry.get(id).await.is_none());
    }

    #[tokio::test]
    async fn a_reaped_session_admits_no_more_input() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("idle")));
        let registry = SessionRegistry::new();
        let hosts = HostMap::default();
        let (id, host) = open_hosted_session(&factory, &registry, &hosts).await;

        let mut reaper = Reaper::new(
            registry.clone(),
            hosts.clone(),
            ReaperConfig {
                grace: Duration::from_secs(3600),
                idle: Duration::ZERO,
                tick: Duration::from_secs(1),
            },
        );
        assert_eq!(reaper.reap_once(Instant::now()).await, vec![id]);
        // A connection still holding the host cannot start work on the closed
        // session.
        assert_eq!(host.submit("late".to_string(), false), Admit::Closed);
        assert_eq!(host.submit("late".to_string(), true), Admit::Closed);
    }

    #[tokio::test]
    async fn a_session_with_admitted_work_is_not_reaped() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("busy")));
        let registry = SessionRegistry::new();
        let hosts = HostMap::default();
        let (id, host) = open_hosted_session(&factory, &registry, &hosts).await;

        // Hold the runtime so the admitted turn cannot start: it is accepted
        // but has not published its token.
        let handle = registry.get(id).await.unwrap();
        let guard = handle.lock().await;
        assert_eq!(host.submit("go".to_string(), false), Admit::Started);
        assert!(
            host.is_busy(),
            "accepted work counts as busy before it starts"
        );

        let mut reaper = Reaper::new(
            registry.clone(),
            hosts.clone(),
            ReaperConfig {
                grace: Duration::ZERO,
                idle: Duration::ZERO,
                tick: Duration::from_secs(1),
            },
        );
        assert!(reaper.reap_once(Instant::now()).await.is_empty());
        drop(guard);
        assert!(registry.get(id).await.is_some());
    }

    #[tokio::test]
    async fn a_malformed_record_is_rejected_with_its_correlation_id() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("x")));
        let (endpoint, server) = spawn_server(factory, 1);
        let (read, mut write) = tokio::io::split(dial(&endpoint).await);
        let mut reader = JsonRecordReader::new(read);
        attach_client_with(&mut reader, &mut write, AttachTarget::OpenNew, acks()).await;

        write
            .write_all(
                b"{\"v\":1,\"id\":\"m1\",\"command\":{\"type\":\"prompt\"}}
",
            )
            .await
            .unwrap();
        write.flush().await.unwrap();
        let record = timeout(DEADLINE, reader.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let record: ServerRecord = serde_json::from_value(record).unwrap();
        assert_eq!(record.id.as_deref(), Some("m1"));
        assert!(matches!(
            record.event,
            ServerEvent::Rejected {
                code: RejectCode::Malformed,
                ..
            }
        ));

        drop((write, reader));
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }

    /// A multi-provider config resolves a model→provider route for each
    /// provider that advertises a model, so different swarm workers can be built
    /// on models served by different providers.
    #[test]
    fn provider_routes_map_each_configured_models_provider() {
        use localpilot_config::{Config, ProviderConfig};
        use localpilot_llm::ProviderRegistry;

        let mut config = Config::default();
        config.providers.insert(
            "fast".to_string(),
            ProviderConfig {
                kind: "openai-compatible".to_string(),
                base_url: Some("http://localhost:1/v1".to_string()),
                model: Some("fast-model".to_string()),
                ..ProviderConfig::default()
            },
        );
        config.providers.insert(
            "careful".to_string(),
            ProviderConfig {
                kind: "openai-compatible".to_string(),
                base_url: Some("http://localhost:2/v1".to_string()),
                model: Some("careful-model".to_string()),
                ..ProviderConfig::default()
            },
        );
        config.provider.default = "fast".to_string();

        let registry = ProviderRegistry::from_config(&config).unwrap();
        let setup = SessionSetup {
            config,
            cwd: std::env::temp_dir(),
            provider: registry.default_provider().unwrap().clone(),
            model: "fast-model".to_string(),
            profile: Profile::Bypass,
            mcp: crate::mcp::McpTools::default(),
            agents: None,
            context_windows: HashMap::new(),
        };

        let routes = setup.provider_routes();
        assert_eq!(routes.len(), 2, "one route per configured advertised model");
        assert!(routes.contains_key("fast-model"));
        assert!(routes.contains_key("careful-model"));
    }

    /// Clean shutdown (the subject-05 deferral): every remaining session is
    /// persisted (`SessionClosed`) and both the registry and host map are cleared.
    #[tokio::test]
    async fn clean_shutdown_persists_and_clears_all_sessions() {
        let factory = Arc::new(FakeFactory::new(FakeProvider::new().text("shutting down")));
        let store = factory.store();
        let registry = SessionRegistry::new();
        let hosts = HostMap::default();
        let (id_a, _a) = open_hosted_session(&factory, &registry, &hosts).await;
        let (id_b, _b) = open_hosted_session(&factory, &registry, &hosts).await;

        close_all_sessions(&registry, &hosts).await;

        assert!(registry.is_empty().await, "registry cleared at shutdown");
        assert!(
            hosts.inner.lock().await.is_empty(),
            "host map cleared at shutdown"
        );
        for id in [id_a, id_b] {
            let events = store.read_events(id).unwrap();
            assert!(
                events
                    .iter()
                    .any(|event| event.kind == SessionEventKind::SessionClosed),
                "session {id} persisted its close at shutdown: {events:?}"
            );
        }
    }
}
