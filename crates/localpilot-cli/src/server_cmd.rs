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

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};

use localpilot_config::{CliOverrides, ConfigPaths};
use localpilot_core::SessionId;
use localpilot_harness::{
    effective_context_limit, register_project_analysis_context,
    register_project_instructions_context, SessionConfig, SessionRuntime, SummarizerTuning,
};
use localpilot_llm::{ModelProvider, ProviderRegistry};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_rpc::{
    map_event, AskRegistry, AttachTarget, ClientCommand, ClientRecord, InputDisposition,
    JsonRecordReader, PendingAsk, RpcApprover, ServerEvent, ServerRecord, RPC_PROTOCOL_VERSION,
    SERVER_VERSION,
};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile};
use localpilot_server::{
    acquire, attach, ensure_running, Acquired, AttachError, Conn, Endpoint, Listener,
    RegistryError, SessionFactory, SessionHost, SessionRegistry, TransportError,
};
use localpilot_store::Store;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, Mutex};

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
        })
    }

    /// The resolved model name.
    pub(crate) fn model(&self) -> &str {
        &self.model
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
        let context_token_limit = effective_context_limit(
            self.provider.declaration().max_context_tokens,
            self.config.harness.context_token_limit,
        );
        let mut registry = self.mcp.registry();
        let broker = crate::mcp::install_broker(&self.config.tools, &mut registry);
        // The serve loop is driven by a client (interactive): apply the built-in
        // safety rails with the interactive profile, exactly as the stdio `rpc`
        // path does. Explicit `[harness]` values still win in `resolved_rails`.
        let rails = self.config.harness.resolved_rails(true);
        let mut runtime = SessionRuntime::new(
            self.provider.clone(),
            registry,
            PermissionEngine::new(self.profile, Vec::new()),
            Box::new(approver),
            Store::open(&self.cwd),
            crate::session_cmd::workspace_with_read_roots(&self.cwd, &self.config)?,
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig {
                model: self.model.clone(),
                // The wire client answers asks; the engine treats the session as
                // interactive so ask-class effects reach the client instead of
                // being denied outright.
                interactivity: Interactivity::Interactive,
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
                enforce_readable_errors: self.config.tools.readable_errors,
                repair_mode: self.config.tools.repair,
                elide_seen_reads: self.config.tools.elide_seen_reads,
                turn_timeout: rails.turn_timeout_secs.map(std::time::Duration::from_secs),
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
        Ok(BuiltSession {
            runtime,
            ask_rx,
            asks,
        })
    }
}

/// Display label for a permission profile (shared with the stdio `rpc` serve
/// context so both surfaces name profiles identically).
pub(crate) fn profile_label(profile: Profile) -> &'static str {
    match profile {
        Profile::Default => "default",
        Profile::Relaxed => "relaxed",
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
}

impl HostMap {
    /// Bind a connection to its session: reuse the live host when the target id
    /// is already hosted, otherwise create+register the session via the registry
    /// and wrap its handle in a fresh host. Returns the bound id, the shared
    /// host, and — only on a fresh create — the wire approver halves.
    ///
    /// Binding is serialized by the map mutex, which also fences the factory's
    /// single-slot approver hand-off.
    async fn bind<F: ConnectionFactory>(
        &self,
        target: AttachTarget,
        registry: &SessionRegistry,
        factory: &F,
        store: &Store,
    ) -> Result<(SessionId, Arc<SessionHost>, Option<ApproverHalves>), AttachError> {
        let mut map = self.inner.lock().await;
        // A resume target whose session is already hosted short-circuits to the
        // live host — never a second `attach`, so the store known-id guard (for
        // an on-disk resume) is correctly skipped for a live session, and a
        // freshly opened session with no store entry yet is still joinable.
        if let Some(id) = existing_id(&target, store)? {
            if let Some(host) = map.get(&id) {
                return Ok((id, host.clone(), None));
            }
        }
        let id = attach(target, registry, factory, store).await?;
        let handle = registry
            .get(id)
            .await
            .ok_or(AttachError::Registry(RegistryError::NotFound))?;
        let host = Arc::new(SessionHost::new(handle).await);
        map.insert(id, host.clone());
        Ok((id, host, factory.take_asks()))
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
                    message: format!("malformed record: {error}"),
                },
            )
            .await?;
            return Ok(());
        }
    };
    let reply_id = record.id;
    let target = match record.command {
        ClientCommand::Attach { target } => target,
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

    let (session_id, host, approver) = match hosts.bind(target, registry, factory, store).await {
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

    // Subscribe before confirming the attach, so a client that has observed
    // `attached` is guaranteed to receive every subsequent event — including a
    // turn another client drives.
    let mut events = host.subscribe();
    let (mut ask_rx, asks) = match approver {
        Some(halves) => (Some(halves.ask_rx), Some(halves.asks)),
        None => (None, None),
    };
    emit(
        &mut write,
        reply_id,
        ServerEvent::Attached {
            session_id,
            server_version: SERVER_VERSION.to_string(),
        },
    )
    .await?;

    loop {
        tokio::select! {
            // Fanout: every session event, whoever drove the turn, to this writer.
            event = events.recv() => match event {
                Ok(event) => emit(&mut write, None, map_event(event)).await?,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            // Permission asks for this session (only if this connection owns them).
            ask = next_ask(&mut ask_rx) => match ask {
                Some(ask) => emit_ask(&mut write, &ask).await?,
                None => ask_rx = None, // the approver's sender is gone; stop polling
            },
            // Client commands.
            record = reader.next() => {
                let Some(value) = record? else { break }; // EOF: detach this connection
                let stop = handle_command(
                    value, &host, &session_id, &model, &profile_label, asks.as_ref(), &mut write,
                )
                .await?;
                if stop {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Route one decoded client command. Returns `true` when the connection should
/// detach (a `shutdown`).
#[allow(clippy::too_many_arguments)] // the loop genuinely threads these
async fn handle_command<W: AsyncWrite + Unpin>(
    value: serde_json::Value,
    host: &Arc<SessionHost>,
    session_id: &SessionId,
    model: &str,
    profile_label: &str,
    asks: Option<&AskRegistry>,
    write: &mut W,
) -> anyhow::Result<bool> {
    let record: ClientRecord = match serde_json::from_value(value) {
        Ok(record) => record,
        Err(error) => {
            emit(
                write,
                None,
                ServerEvent::Error {
                    message: format!("malformed record: {error}"),
                },
            )
            .await?;
            return Ok(false);
        }
    };
    let id = record.id;
    match record.command {
        ClientCommand::Hello => {
            emit(
                write,
                id,
                ServerEvent::Hello {
                    protocol_version: RPC_PROTOCOL_VERSION,
                    session_id: session_id.to_string(),
                    model: model.to_string(),
                },
            )
            .await?;
        }
        ClientCommand::Status => {
            emit(
                write,
                id,
                ServerEvent::Status {
                    session_id: session_id.to_string(),
                    model: model.to_string(),
                    profile: profile_label.to_string(),
                    busy: host.is_busy(),
                    pending_asks: asks.map(AskRegistry::outstanding).unwrap_or_default(),
                    next_step: None,
                },
            )
            .await?;
        }
        ClientCommand::Prompt { text, disposition } => match disposition {
            // Immediate (and follow-up) start a turn when the session is idle.
            // A running turn rejects immediate input; steer injects into it.
            InputDisposition::Immediate | InputDisposition::FollowUp => {
                if host.is_busy() {
                    emit(
                        write,
                        id,
                        ServerEvent::Error {
                            message: "a turn is already running on this session; cancel it \
                                      first, or send a steer disposition to inject into it"
                                .to_string(),
                        },
                    )
                    .await?;
                } else {
                    let host = host.clone();
                    // Drive on its own task; events (including the terminal
                    // `stopped`) reach this and every other attached connection
                    // via the host broadcast. Detached on purpose: a turn keeps
                    // running for other clients if this connection detaches.
                    tokio::spawn(async move {
                        host.drive(&text).await;
                    });
                }
            }
            InputDisposition::Steer => {
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
            if !host.cancel() {
                emit(
                    write,
                    id,
                    ServerEvent::Error {
                        message: "no turn is running".to_string(),
                    },
                )
                .await?;
            }
        }
        ClientCommand::PermissionReply { ask_id, allow } => match asks {
            Some(asks) if asks.resolve(&ask_id, allow) => {}
            Some(_) => {
                emit(
                    write,
                    id,
                    ServerEvent::Error {
                        message: format!("unknown ask id {ask_id}"),
                    },
                )
                .await?;
            }
            None => {
                emit(
                    write,
                    id,
                    ServerEvent::Error {
                        message: "this connection does not own this session's permission asks \
                                  (another client attached first)"
                            .to_string(),
                    },
                )
                .await?;
            }
        },
        ClientCommand::Shutdown => {
            emit(write, id, ServerEvent::Closed).await?;
            return Ok(true);
        }
        ClientCommand::Attach { .. } => {
            emit(
                write,
                id,
                ServerEvent::Error {
                    message: "already attached; a connection binds its session once".to_string(),
                },
            )
            .await?;
        }
        // `ClientCommand` is `#[non_exhaustive]`: an unknown command is a typed
        // error, never a panic.
        _ => {
            emit(
                write,
                id,
                ServerEvent::Error {
                    message: "this server build does not understand that command".to_string(),
                },
            )
            .await?;
        }
    }
    Ok(false)
}

/// Await the next permission ask, or park forever when this connection owns no
/// approver — so the `select!` arm is inert rather than busy-looping.
async fn next_ask(rx: &mut Option<mpsc::UnboundedReceiver<PendingAsk>>) -> Option<PendingAsk> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn emit<W: AsyncWrite + Unpin>(
    write: &mut W,
    id: Option<String>,
    event: ServerEvent,
) -> std::io::Result<()> {
    let record = ServerRecord {
        v: RPC_PROTOCOL_VERSION,
        id,
        event,
    };
    let mut line = serde_json::to_vec(&record).unwrap_or_else(|_| b"{}".to_vec());
    line.push(b'\n');
    write.write_all(&line).await?;
    write.flush().await
}

async fn emit_ask<W: AsyncWrite + Unpin>(write: &mut W, ask: &PendingAsk) -> std::io::Result<()> {
    emit(
        write,
        None,
        ServerEvent::PermissionAsk {
            ask_id: ask.ask_id.clone(),
            tool: ask.tool.clone(),
            detail: ask.detail.clone(),
            risk: ask.risk.clone(),
        },
    )
    .await
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
    let hosts = HostMap::default();
    let factory = Arc::new(ServerFactory::new(setup));

    eprintln!(
        "localpilot server listening at {} — Ctrl-C to stop",
        endpoint.display()
    );
    accept_loop(&listener, &registry, &hosts, &factory, &store, profile).await;

    // Clean shutdown: dropping the listener (Unix unlinks the socket) and the
    // singleton (Unix removes the lock file) releases the endpoint.
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
    send_command(&mut write, ClientCommand::Attach { target }).await?;

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
        v: RPC_PROTOCOL_VERSION,
        id: None,
        command,
    };
    let mut line = serde_json::to_vec(&record).unwrap_or_else(|_| b"{}".to_vec());
    line.push(b'\n');
    write.write_all(&line).await?;
    write.flush().await
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
        } => {
            eprintln!("localpilot: permission needed — {tool} ({risk}): {detail}");
            eprintln!("           type /allow or /deny to answer.");
            *last_ask = Some(ask_id);
        }
        ServerEvent::Error { message } => eprintln!("localpilot: error: {message}"),
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

    use localpilot_llm::FakeProvider;
    use localpilot_sandbox::{ScriptedApprover, Workspace};
    use localpilot_tools::ToolRegistry;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    /// A turn and its round trips must settle well within this; exceeding it is a
    /// hang the test turns into a failure.
    const DEADLINE: Duration = Duration::from_secs(15);

    /// Builds `SessionRuntime`s over one shared temp-dir store and provider —
    /// mirrors the server crate's own test factory — with no wire approver (a
    /// bypass session never asks), so `take_asks` is `None`.
    struct FakeFactory {
        dir: Arc<tempfile::TempDir>,
        provider: Arc<FakeProvider>,
    }

    impl FakeFactory {
        fn new(provider: FakeProvider) -> Self {
            Self {
                dir: Arc::new(tempfile::tempdir().unwrap()),
                provider: Arc::new(provider),
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
            Ok(SessionRuntime::new(
                self.provider.clone(),
                ToolRegistry::with_builtins(),
                PermissionEngine::new(Profile::Bypass, Vec::new()),
                Box::new(ScriptedApprover::always()),
                Store::open(root),
                workspace,
                RecoveryEngine::new(RecoveryBudget::default()),
                SessionConfig {
                    interactivity: Interactivity::NonInteractive,
                    trusted: true,
                    ..SessionConfig::default()
                },
                Vec::new(),
            ))
        }
    }

    impl ConnectionFactory for FakeFactory {
        fn take_asks(&self) -> Option<ApproverHalves> {
            None
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
        let endpoint = unique_endpoint();
        let listener = Listener::bind(&endpoint).unwrap();
        let store = factory.store();
        let registry = SessionRegistry::new();
        let hosts = HostMap::default();
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
        send_command(write, ClientCommand::Attach { target })
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

    // --- 05.1 serve loop: a turn streams back and the server stops cleanly ----

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

    // --- 05.2 a connect-style client drives one full turn ---------------------

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
                    target: AttachTarget::OpenNew,
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

    // --- 05.4 two clients on one session, both observe the stream -------------

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

    // --- the plain-text client renderer --------------------------------------

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
}
