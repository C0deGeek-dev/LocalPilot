//! Shared construction for fully interactive CLI sessions.
//!
//! The terminal hosts own rendering and event-loop policy. This module owns the
//! one runtime recipe they consume: providers and MCP connections are resolved
//! once, while every build gets independent tools, channels, permissions, store,
//! workspace, and runtime state.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use localpilot_agents::AgentSet;
use localpilot_config::Config;
use localpilot_core::SessionId;
use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime};
use localpilot_llm::ProviderRegistry;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{
    Approver, Effect, Interactivity, PermissionEngine, PermissionRequest, Profile,
};
use localpilot_server::swarm::{AdoptedPair, SwarmHost};
use localpilot_server::{swarm_id_for_dir, SessionHost, SessionRegistry, SwarmId, SwarmRegistry};
use localpilot_store::Store;
use localpilot_tools::{UserAnswer, UserPrompter, UserQuestion};
use tokio::sync::{broadcast, mpsc, oneshot};

/// A pending tool-approval request, as surfaced to an interactive host.
#[derive(Debug, Clone)]
pub(crate) struct ApprovalRequest {
    pub(crate) tool: String,
    pub(crate) target: String,
    pub(crate) risk_class: String,
}

/// A pending approval handed from the runtime to an interactive host.
pub(crate) struct ApprovalCall {
    pub(crate) request: ApprovalRequest,
    pub(crate) reply: oneshot::Sender<bool>,
}

/// A pending set of questions handed from the runtime to an interactive host.
pub(crate) struct QuestionCall {
    pub(crate) questions: Vec<UserQuestion>,
    pub(crate) reply: oneshot::Sender<Vec<UserAnswer>>,
}

/// An approver that suspends the turn and sends the decision to its host.
pub(crate) struct TuiApprover {
    tx: mpsc::UnboundedSender<ApprovalCall>,
}

impl TuiApprover {
    pub(crate) fn new(tx: mpsc::UnboundedSender<ApprovalCall>) -> Self {
        Self { tx }
    }
}

impl Approver for TuiApprover {
    fn approve<'a>(
        &'a self,
        request: &'a PermissionRequest,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        let (reply, answer) = oneshot::channel();
        let sent = self.tx.send(ApprovalCall {
            request: describe(request),
            reply,
        });
        Box::pin(async move {
            // A closed channel (UI gone) is a denial, never a silent approval.
            if sent.is_err() {
                return false;
            }
            answer.await.unwrap_or(false)
        })
    }
}

/// A prompter that suspends the turn and sends its questions to its host.
struct TuiPrompter {
    tx: mpsc::UnboundedSender<QuestionCall>,
}

impl UserPrompter for TuiPrompter {
    fn ask<'a>(
        &'a self,
        questions: &'a [UserQuestion],
    ) -> Pin<Box<dyn Future<Output = Vec<UserAnswer>> + Send + 'a>> {
        let (reply, answer) = oneshot::channel();
        let sent = self.tx.send(QuestionCall {
            questions: questions.to_vec(),
            reply,
        });
        let count = questions.len();
        Box::pin(async move {
            // A closed channel (UI gone) is a dismissal, never an invented
            // answer — the same rule the approver follows for a denial.
            if sent.is_err() {
                return vec![UserAnswer::Dismissed; count];
            }
            answer
                .await
                .unwrap_or_else(|_| vec![UserAnswer::Dismissed; count])
        })
    }
}

/// Inputs shared by every fully interactive session in one CLI invocation.
pub(crate) struct InteractiveSessionSetup {
    config: Config,
    cwd: PathBuf,
    profile: Profile,
    providers: Arc<ProviderRegistry>,
    /// Kept alive so one set of MCP transports backs each fresh tool registry.
    mcp: crate::mcp::McpTools,
    agents: Option<Arc<AgentSet>>,
    /// The one canonical launch trust decision (`prompt_required(profile, cwd)`),
    /// computed once in [`InteractiveSessionSetup::resolve`]. Every built runtime
    /// (both pair peers) derives `SessionConfig.trusted = !trust_required` from
    /// it, and the selected host reads the same value for its trust gate — so the
    /// runtime and the gate can never diverge or re-read the store (no TOCTOU).
    trust_required: bool,
    /// The initial, trust-safe package-discovery hint, computed once at launch:
    /// installed skill packages exist but model discovery is off. Copied into
    /// every built runtime's `SessionConfig` — no per-build/per-peer scan.
    package_discovery_hint: bool,
    /// Incognito session: the built runtime gets an in-memory store and the
    /// incognito permission floor, and the host skips every persistence path
    /// (closeout, knowledge index, code-graph reindex, ingest observer).
    incognito: bool,
}

/// A fresh interactive runtime and the host-facing halves of its user channels.
///
/// The bundle is intentionally not cloneable: one host owns each receiver.
pub(crate) struct InteractiveSessionBundle {
    pub(crate) runtime: SessionRuntime,
    /// The existing CLI trust flow also sends approvals through this channel.
    pub(crate) approval_tx: mpsc::UnboundedSender<ApprovalCall>,
    pub(crate) approvals: mpsc::UnboundedReceiver<ApprovalCall>,
    pub(crate) questions: mpsc::UnboundedReceiver<QuestionCall>,
}

/// Host-assigned identity of one member of an interactive pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairPeer {
    A,
    B,
}

impl PairPeer {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
        }
    }
}

/// One explicit provider/model choice for a pair member.
#[derive(Debug, Clone, Copy)]
pub(crate) struct InteractivePeerSelection<'a> {
    pub(crate) provider_id: &'a str,
    pub(crate) model: &'a str,
}

/// One identified member of a freshly constructed interactive pair.
pub(crate) struct InteractivePeerBundle {
    pub(crate) peer: PairPeer,
    pub(crate) session: InteractiveSessionBundle,
}

/// Two ordinary interactive sessions sharing one workspace and original task.
pub(crate) struct InteractivePairBundle {
    task: String,
    pub(crate) a: InteractivePeerBundle,
    pub(crate) b: InteractivePeerBundle,
}

/// One hosted pair member and the UI-facing channels tied to its provenance.
pub(crate) struct InteractiveHostedPeer {
    pub(crate) host: Arc<SessionHost>,
    pub(crate) events: broadcast::Receiver<RuntimeEvent>,
    pub(crate) approvals: mpsc::UnboundedReceiver<ApprovalCall>,
    pub(crate) questions: mpsc::UnboundedReceiver<QuestionCall>,
}

/// Resources that keep an adopted interactive pair alive while it is driven.
pub(crate) struct InteractivePairOwner {
    pub(crate) cwd: PathBuf,
    pub(crate) task: String,
    pub(crate) sessions: [SessionId; 2],
    pub(crate) registry: SessionRegistry,
    pub(crate) swarm_host: SwarmHost,
    pub(crate) a: InteractiveHostedPeer,
    pub(crate) b: InteractiveHostedPeer,
}

impl InteractivePairOwner {
    /// Close both hosted runtimes after their driver has stopped.
    ///
    /// The caller must restore terminal modes before awaiting this cleanup and
    /// must no longer have a live driver. Explicit teardown is required because
    /// each runtime's peer binding points back through the host and registry.
    pub(crate) async fn close(self) {
        let Self {
            cwd,
            sessions,
            registry,
            swarm_host,
            a,
            b,
            ..
        } = self;

        a.host.cancel();
        b.host.cancel();
        close_registered(&registry, &swarm_host, &sessions).await;

        // Release every strong host/receiver/channel owner before closeout.
        drop(a);
        drop(b);
        drop(swarm_host);
        drop(registry);
        close_pair_contexts(&cwd, sessions);
    }
}

/// An adopted pair coupled to the resources that own its hosted sessions.
pub(crate) struct InteractivePairHost {
    adopted: AdoptedPair,
    owner: InteractivePairOwner,
}

impl InteractivePairHost {
    /// The two sessions in stable peer order.
    pub(crate) fn sessions(&self) -> [SessionId; 2] {
        self.owner.sessions
    }

    /// The exact original task shared by both sessions.
    pub(crate) fn task(&self) -> &str {
        &self.owner.task
    }

    /// Grant live workspace trust to BOTH pair peers on trust acceptance, before
    /// the driver spawns. All-or-error: resolve both registry handles FIRST and
    /// fail (mutating neither) if either is missing, then set each runtime trusted
    /// and — when `hint` — append the package-discovery cue once. `hint` is
    /// computed once by the host (trusted-side), so pair-run stays free of config
    /// and skill-discovery dependencies. Mirrors `close_registered`'s
    /// resolve-then-lock-then-mutate discipline over the shared registry.
    pub(crate) async fn grant_trust(&self, hint: bool) -> anyhow::Result<()> {
        let sessions = self.owner.sessions;
        // Resolve both handles before mutating either — a missing peer is an
        // internal error, never a silently half-trusted pair.
        let handle_a = self
            .owner
            .registry
            .get(sessions[0])
            .await
            .ok_or_else(|| anyhow::anyhow!("pair peer A session is not registered"))?;
        let handle_b = self
            .owner
            .registry
            .get(sessions[1])
            .await
            .ok_or_else(|| anyhow::anyhow!("pair peer B session is not registered"))?;
        for handle in [handle_a, handle_b] {
            let mut runtime = handle.lock().await;
            runtime.set_trusted(true);
            if hint {
                runtime.note_package_discovery_disabled_but_present();
            }
        }
        Ok(())
    }

    /// Build, register, adopt, and subscribe to two interactive sessions.
    pub(crate) async fn prepare(
        setup: &InteractiveSessionSetup,
        task: &str,
        a: InteractivePeerSelection<'_>,
        b: InteractivePeerSelection<'_>,
    ) -> anyhow::Result<Self> {
        let bundle = setup.build_pair(task, a, b).await?;
        let registry = SessionRegistry::new();
        let swarm_host = SwarmHost::for_adoption(registry.clone(), SwarmRegistry::new());
        let swarm = swarm_id_for_dir(setup.cwd());
        Self::from_bundle(
            setup.cwd().to_path_buf(),
            bundle,
            registry,
            swarm_host,
            swarm,
        )
        .await
    }

    /// Split driver ownership from the UI/session resources without cloning.
    pub(crate) fn into_parts(self) -> (AdoptedPair, InteractivePairOwner) {
        (self.adopted, self.owner)
    }

    /// Tear down a pair that was prepared but never handed to a driver.
    pub(crate) async fn close(self) {
        let Self { adopted, owner } = self;
        drop(adopted);
        owner.close().await;
    }

    async fn from_bundle(
        cwd: PathBuf,
        bundle: InteractivePairBundle,
        registry: SessionRegistry,
        swarm_host: SwarmHost,
        swarm: SwarmId,
    ) -> anyhow::Result<Self> {
        let InteractivePairBundle { task, a, b } = bundle;
        let InteractivePeerBundle {
            peer: a_peer,
            session: a_session,
        } = a;
        let InteractivePeerBundle {
            peer: b_peer,
            session: b_session,
        } = b;
        let a_id = a_session.runtime.session_id();
        let b_id = b_session.runtime.session_id();
        let sessions = [a_id, b_id];

        if a_id == b_id {
            close_unhosted_pair_session(&cwd, a_session);
            close_unhosted_pair_session(&cwd, b_session);
            return Err(anyhow::anyhow!(
                "interactive pair sessions must have distinct ids"
            ));
        }

        let InteractiveSessionBundle {
            runtime: a_runtime,
            approval_tx: _,
            approvals: a_approvals,
            questions: a_questions,
        } = a_session;
        let InteractiveSessionBundle {
            runtime: b_runtime,
            approval_tx: b_approval_tx,
            approvals: b_approvals,
            questions: b_questions,
        } = b_session;

        // Registration consumes its runtime on both outcomes, so a failure can
        // explicitly clean only a still-owned or already-registered peer.
        if let Err(error) = registry.register(a_runtime).await {
            let b_session = InteractiveSessionBundle {
                runtime: b_runtime,
                approval_tx: b_approval_tx,
                approvals: b_approvals,
                questions: b_questions,
            };
            close_unhosted_pair_session(&cwd, b_session);
            return Err(error.into());
        }
        if let Err(error) = registry.register(b_runtime).await {
            close_registered(&registry, &swarm_host, &[a_id]).await;
            crate::context_inject::close_out(&cwd, a_id);
            return Err(error.into());
        }

        let adopted = match swarm_host
            .adopt_pair(&swarm, (a_id, a_peer.label()), (b_id, b_peer.label()))
            .await
        {
            Ok(adopted) => adopted,
            Err(error) => {
                close_registered(&registry, &swarm_host, &sessions).await;
                close_pair_contexts(&cwd, sessions);
                return Err(error.into());
            }
        };
        let [a_host, b_host] = adopted.hosts();
        let a_events = a_host.subscribe();
        let b_events = b_host.subscribe();
        let owner = InteractivePairOwner {
            cwd,
            task,
            sessions,
            registry,
            swarm_host,
            a: InteractiveHostedPeer {
                host: a_host,
                events: a_events,
                approvals: a_approvals,
                questions: a_questions,
            },
            b: InteractiveHostedPeer {
                host: b_host,
                events: b_events,
                approvals: b_approvals,
                questions: b_questions,
            },
        };
        Ok(Self { adopted, owner })
    }
}

async fn close_registered(
    registry: &SessionRegistry,
    swarm_host: &SwarmHost,
    sessions: &[SessionId],
) {
    for &session in sessions {
        if let Some(host) = swarm_host.host(session).await {
            host.cancel();
        }
    }
    for &session in sessions {
        if let Some(handle) = registry.get(session).await {
            handle.lock().await.close();
        }
    }
    for &session in sessions {
        swarm_host.unhost(session).await;
    }
    for &session in sessions {
        registry.remove(session).await;
    }
}

fn close_unhosted_pair_session(cwd: &Path, mut session: InteractiveSessionBundle) {
    let id = session.runtime.session_id();
    session.runtime.close();
    drop(session);
    crate::context_inject::close_out(cwd, id);
}

fn close_pair_contexts(cwd: &Path, sessions: [SessionId; 2]) {
    for session in sessions {
        crate::context_inject::close_out(cwd, session);
    }
}

impl InteractiveSessionSetup {
    /// Resolve provider, MCP, and agent resources once for this workspace.
    pub(crate) async fn resolve(
        cwd: PathBuf,
        config: Config,
        profile: Profile,
    ) -> anyhow::Result<Self> {
        Self::resolve_with(cwd, config, profile, false).await
    }

    /// [`Self::resolve`] with the incognito switch. When `incognito`, every
    /// runtime this setup builds is non-persistent (in-memory store + incognito
    /// permission floor) and the host skips its persistence paths.
    pub(crate) async fn resolve_with(
        cwd: PathBuf,
        config: Config,
        profile: Profile,
        incognito: bool,
    ) -> anyhow::Result<Self> {
        let providers = Arc::new(ProviderRegistry::from_config(&config)?);
        let mcp = crate::mcp::McpTools::load(&config).await;
        let agents = crate::agents_cmd::session_agents(&cwd);
        // One canonical launch trust decision, shared by every built runtime and
        // the host gate; and one trust-safe initial package-discovery hint.
        let trust_required = crate::trust::prompt_required(profile, &cwd);
        let package_discovery_hint = initial_package_discovery_hint(&config, &cwd, !trust_required);
        Ok(Self {
            config,
            cwd,
            profile,
            providers,
            mcp,
            agents,
            trust_required,
            package_discovery_hint,
            incognito,
        })
    }

    /// The one canonical launch trust decision (`prompt_required(profile, cwd)`),
    /// so the host gate reads the same value the runtimes were built with.
    pub(crate) fn trust_required(&self) -> bool {
        self.trust_required
    }

    /// Build a fresh fully interactive session on an explicit provider/model.
    pub(crate) async fn build(
        &self,
        provider_id: &str,
        model: &str,
    ) -> anyhow::Result<InteractiveSessionBundle> {
        let provider = self
            .providers
            .get(provider_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("provider '{provider_id}' is not configured"))?;

        // The real context window: per-provider declaration first, then
        // best-effort discovery. Failure falls back to the configured budget.
        let mut context_window = provider.declaration().max_context_tokens;
        if context_window.is_none() {
            context_window = discovered_window(&self.config, provider_id, model).await;
        }

        let (approval_tx, approvals) = mpsc::unbounded_channel::<ApprovalCall>();
        let (question_tx, questions) = mpsc::unbounded_channel::<QuestionCall>();
        let mut tools = self.mcp.registry();
        let broker = crate::mcp::install_broker(&self.config.tools, &mut tools);
        let mut runtime = SessionRuntime::new(
            provider,
            tools,
            PermissionEngine::new(self.profile, Vec::new()),
            Box::new(TuiApprover::new(approval_tx.clone())),
            // Incognito keeps nothing on disk: the store is in-memory.
            if self.incognito {
                Store::ephemeral()
            } else {
                Store::open(&self.cwd)
            },
            crate::session_cmd::workspace_with_read_roots(&self.cwd, &self.config)?,
            RecoveryEngine::new(RecoveryBudget::default()),
            interactive_config(
                &self.config,
                model,
                context_window,
                !self.trust_required,
                self.package_discovery_hint,
                self.incognito,
            ),
            Vec::new(),
        );
        runtime.set_broker(broker);
        runtime.set_prompter(Arc::new(TuiPrompter { tx: question_tx }));
        if let Some(agents) = &self.agents {
            runtime.set_agents(Arc::clone(agents));
        }
        runtime.set_registry(Arc::clone(&self.providers));
        runtime.set_image_support_override(
            resolved_image_support(&self.config, Some(provider_id)).await,
        );
        localpilot_harness::register_project_analysis_context(
            &self.cwd,
            self.config.context.project_analysis,
            self.config.docs.lookup_policy,
            &mut runtime,
        );
        localpilot_harness::register_project_instructions_context(
            &self.cwd,
            self.config.context.inject_instructions,
            self.config.context.instruction_char_budget,
            &mut runtime,
        );
        localpilot_localmind::register_context_hook(&self.cwd, &mut runtime);

        Ok(InteractiveSessionBundle {
            runtime,
            approval_tx,
            approvals,
            questions,
        })
    }

    /// Build two distinct interactive sessions over this setup's one workspace.
    ///
    /// Both explicit selections are validated before either runtime is created.
    /// The same preserved task and parser-owned protocol contract are installed
    /// on both runtimes before either can be driven.
    pub(crate) async fn build_pair(
        &self,
        task: &str,
        a: InteractivePeerSelection<'_>,
        b: InteractivePeerSelection<'_>,
    ) -> anyhow::Result<InteractivePairBundle> {
        if task.trim().is_empty() {
            return Err(anyhow::anyhow!("pair task must not be empty"));
        }
        self.validate_pair_selection(PairPeer::A, a)?;
        self.validate_pair_selection(PairPeer::B, b)?;

        let a_session = self.build(a.provider_id, a.model).await?;
        let b_session = self.build(b.provider_id, b.model).await;
        finish_pair_build(&self.cwd, task, a_session, b_session)
    }

    fn validate_pair_selection(
        &self,
        peer: PairPeer,
        selection: InteractivePeerSelection<'_>,
    ) -> anyhow::Result<()> {
        for (kind, value) in [
            ("provider", selection.provider_id),
            ("model", selection.model),
        ] {
            if value.trim().is_empty() {
                return Err(anyhow::anyhow!(
                    "peer {} {kind} must not be empty",
                    peer.label()
                ));
            }
            if value.trim() != value {
                return Err(anyhow::anyhow!(
                    "peer {} {kind} must not have leading or trailing whitespace",
                    peer.label()
                ));
            }
        }
        if self.providers.get(selection.provider_id).is_none() {
            return Err(anyhow::anyhow!(
                "peer {} provider '{}' is not configured",
                peer.label(),
                selection.provider_id
            ));
        }
        Ok(())
    }

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        cwd: PathBuf,
        config: Config,
        profile: Profile,
        providers: ProviderRegistry,
    ) -> Self {
        let trust_required = crate::trust::prompt_required(profile, &cwd);
        let package_discovery_hint = initial_package_discovery_hint(&config, &cwd, !trust_required);
        Self {
            config,
            cwd,
            profile,
            providers: Arc::new(providers),
            mcp: crate::mcp::McpTools::default(),
            agents: None,
            trust_required,
            package_discovery_hint,
            incognito: false,
        }
    }
}

fn finish_pair_build(
    cwd: &Path,
    task: &str,
    mut a_session: InteractiveSessionBundle,
    b_session: anyhow::Result<InteractiveSessionBundle>,
) -> anyhow::Result<InteractivePairBundle> {
    let mut b_session = match b_session {
        Ok(session) => session,
        Err(error) => {
            close_unhosted_pair_session(cwd, a_session);
            return Err(error);
        }
    };
    a_session
        .runtime
        .append_system_prompt(localpilot_server::swarm::pair_session_directive(
            PairPeer::A.label(),
            PairPeer::B.label(),
            task,
        ));
    b_session
        .runtime
        .append_system_prompt(localpilot_server::swarm::pair_session_directive(
            PairPeer::B.label(),
            PairPeer::A.label(),
            task,
        ));

    Ok(InteractivePairBundle {
        task: task.to_string(),
        a: InteractivePeerBundle {
            peer: PairPeer::A,
            session: a_session,
        },
        b: InteractivePeerBundle {
            peer: PairPeer::B,
            session: b_session,
        },
    })
}

/// The exact interactive `SessionConfig` formerly constructed inline by chat.
///
/// `trusted` and `package_discovery_disabled_but_present` are the launch snapshot
/// computed once in [`InteractiveSessionSetup::resolve`] and passed in verbatim,
/// so every built runtime (both pair peers) shares one decision — no per-build
/// re-derivation of trust or re-scan of the skill catalog.
fn interactive_config(
    config: &Config,
    model: &str,
    context_window: Option<u64>,
    trusted: bool,
    package_discovery_disabled_but_present: bool,
    incognito: bool,
) -> SessionConfig {
    let rails = config.harness.resolved_rails(true);
    SessionConfig {
        model: model.to_string(),
        interactivity: Interactivity::Interactive,
        trusted,
        package_discovery_disabled_but_present,
        incognito,
        context_token_limit: localpilot_harness::effective_context_limit(
            context_window,
            config.harness.context_token_limit,
        ),
        compaction_mode: compaction_mode(config.compaction.mode),
        summarizer_tuning: localpilot_harness::SummarizerTuning::from_config(&config.compaction),
        tool_call_budget: rails.tool_call_budget,
        tool_call_budget_max: rails.tool_call_budget_max,
        tool_budget_explicit: rails.budget_explicit,
        rules: config.harness.rules.clone(),
        enforce_claim_gate: config.harness.claim_gate.is_enabled(),
        tool_marker_enabled: config.tools.marker,
        enforce_readable_errors: config.tools.readable_errors,
        repair_mode: config.tools.repair,
        elide_seen_reads: config.tools.elide_seen_reads,
        turn_timeout: rails.turn_timeout_secs.map(std::time::Duration::from_secs),
        verify_before_done: config.harness.verify_before_done,
        verify_command: config.harness.verify_command.clone(),
        ..SessionConfig::default()
    }
}

/// The initial, trust-safe package-discovery hint: `true` only when model-facing
/// skill discovery is off (`[skills] autonomous_discovery = false`) yet installed
/// discoverable skill packages are present. Best-effort — a discovery error never
/// blocks launch (treated as no hint). Trust-safe — the project overlay is read
/// only when the workspace is `trusted`; an untrusted workspace counts the
/// user-global baseline alone and never reads project manifests.
pub(crate) fn initial_package_discovery_hint(config: &Config, cwd: &Path, trusted: bool) -> bool {
    // Resolve the real per-user global home once; the injectable seam below does
    // the counting, so tests can pin the global baseline instead of the machine's.
    initial_package_discovery_hint_with(
        config,
        cwd,
        localpilot_skills::user_home().as_deref(),
        trusted,
    )
}

/// Grant live workspace trust to an interactive runtime on trust acceptance, and
/// refresh the package-discovery cue now that the project overlay is readable.
///
/// The full-screen accept branch uses this helper. Pair does NOT
/// route through it — it uses the separate all-or-error
/// [`InteractivePairHost::grant_trust`] with a host-precomputed hint (same
/// `set_trusted → config.trusted` policy, different call site because both peer
/// handles must be resolved before either is mutated). Trust is set in-memory
/// only (`runtime.set_trusted(true)`); persisting it across sessions stays the
/// caller's separate `trust::remember` step, so the launch gate is unweakened.
pub(crate) fn grant_live_trust(runtime: &mut SessionRuntime, config: &Config, cwd: &Path) {
    // Resolve the real per-user global home once; the injectable seam does the work.
    grant_live_trust_with(
        runtime,
        config,
        cwd,
        localpilot_skills::user_home().as_deref(),
    );
}

/// [`grant_live_trust`] with an explicit global-baseline `home` — the injectable
/// seam, so cue tests pin the global catalog instead of the machine's. Sets the
/// runtime trusted, then appends the "disabled, not empty" cue at most once (the
/// runtime helper is monotonic), only when discovery is off and a discoverable
/// package is now readable; no package content is injected.
pub(crate) fn grant_live_trust_with(
    runtime: &mut SessionRuntime,
    config: &Config,
    cwd: &Path,
    home: Option<&Path>,
) {
    runtime.set_trusted(true);
    if initial_package_discovery_hint_with(config, cwd, home, true) {
        runtime.note_package_discovery_disabled_but_present();
    }
}

/// [`initial_package_discovery_hint`] with an explicit global-baseline `home`
/// (the injectable seam). The project overlay is read only when `trusted`; the
/// global baseline is read from `home` (or omitted when `None`). Best-effort — a
/// discovery error yields no hint and never blocks launch.
pub(crate) fn initial_package_discovery_hint_with(
    config: &Config,
    cwd: &Path,
    home: Option<&Path>,
    trusted: bool,
) -> bool {
    if config.skills.autonomous_discovery {
        // Discovery is on: the package tools are registered, so the model has a
        // real way in and needs no "disabled, not empty" cue.
        return false;
    }
    localpilot_skills::discover(cwd, home, trusted)
        .map(|set| set.discoverable_count() > 0)
        .unwrap_or(false)
}

/// Resolve the active provider's image-input capability: explicit config wins,
/// otherwise a best-effort read-only local-server probe, otherwise false.
pub(crate) async fn resolved_image_support(
    config: &Config,
    provider_id: Option<&str>,
) -> Option<bool> {
    let id = provider_id.unwrap_or(&config.provider.default);
    let entry = config.providers.get(id)?;
    let declared = entry.supports_vision;
    let probed = if declared.is_none() && config.discovery.vision_probe {
        match crate::models_cmd::listing_base_url(entry) {
            Some(base_url) => {
                crate::models_cmd::probe_vision_for_provider(config, id, &base_url).await
            }
            None => None,
        }
    } else {
        None
    };
    Some(localpilot_llm::resolve_vision(declared, probed))
}

async fn discovered_window(config: &Config, provider_id: &str, model: &str) -> Option<u64> {
    let entry = config.providers.get(provider_id)?;
    if entry.kind == "anthropic" {
        return None;
    }
    let base_url = crate::models_cmd::listing_base_url(entry)?;
    let models = crate::models_cmd::discover_models_for_provider(config, provider_id, &base_url)
        .await
        .ok()?;
    models
        .into_iter()
        .find(|candidate| candidate.id == model)
        .and_then(|candidate| candidate.context_window)
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

fn describe(request: &PermissionRequest) -> ApprovalRequest {
    let target_kind = match request.effect {
        Effect::ReadPath { .. } | Effect::WritePath { .. } => "path",
        Effect::RunCommand(_) => "command",
        Effect::Network => "network",
    };
    let risk_class = request.effect.risk_label();
    let target = if request.detail.is_empty() {
        format!("({target_kind})")
    } else {
        request.detail.clone()
    };
    ApprovalRequest {
        tool: request.tool.to_string(),
        target,
        risk_class: risk_class.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use localpilot_config::{CompactionMode, ProviderConfig, RepairMode, RuleSeverity};
    use localpilot_harness::StopReason;
    use localpilot_llm::{FakeProvider, ModelProvider, ProviderRegistry};
    use localpilot_sandbox::{CommandClass, Decision, Workspace};
    use localpilot_server::swarm::{SpawnError, SwarmError};
    use localpilot_server::SwarmLimits;
    use localpilot_skills::SkillList;
    use localpilot_store::SessionEventKind;
    use localpilot_tools::{Tool, ToolContext};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn provider(id: &str, context_window: Option<u64>) -> Arc<FakeProvider> {
        let seed = FakeProvider::new();
        let mut declaration = seed.declaration().clone();
        declaration.id = id.to_string();
        declaration.display_name = id.to_string();
        declaration.max_context_tokens = context_window;
        Arc::new(FakeProvider::new().with_declaration(declaration))
    }

    /// Write a `SKILL.md` skill under `skills_dir/<name>/`. `user_only` marks it
    /// `disable-model-invocation: true` (excluded from discovery counts).
    fn write_skill_md_at(skills_dir: &Path, name: &str, description: &str, user_only: bool) {
        let dir = skills_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let flag = if user_only {
            "disable-model-invocation: true\n"
        } else {
            ""
        };
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: {description}\n{flag}---\n\nBody of {name}.\n"
            ),
        )
        .unwrap();
    }

    /// Write a project-overlay skill under `<cwd>/.localpilot/skills/<name>/`.
    fn write_project_skill(cwd: &Path, name: &str, description: &str, user_only: bool) {
        write_skill_md_at(
            &cwd.join(".localpilot").join("skills"),
            name,
            description,
            user_only,
        );
    }

    fn setup_with_two_providers(
        root: &Path,
    ) -> (
        InteractiveSessionSetup,
        Arc<FakeProvider>,
        Arc<FakeProvider>,
    ) {
        let first = provider("first", Some(32_000));
        let second = provider("second", Some(16_000));
        let first_dyn: Arc<dyn ModelProvider> = first.clone();
        let second_dyn: Arc<dyn ModelProvider> = second.clone();
        let providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::from([
            ("first".to_string(), first_dyn),
            ("second".to_string(), second_dyn),
        ]);
        let models = HashMap::from([
            ("first".to_string(), "model-a".to_string()),
            ("second".to_string(), "model-b".to_string()),
        ]);
        let registry = ProviderRegistry::from_providers(providers, models, "first");
        let mut config = Config::default();
        config.providers.insert(
            "first".to_string(),
            ProviderConfig {
                supports_vision: Some(true),
                ..ProviderConfig::default()
            },
        );
        config.providers.insert(
            "second".to_string(),
            ProviderConfig {
                supports_vision: Some(false),
                ..ProviderConfig::default()
            },
        );
        (
            InteractiveSessionSetup::for_test(
                root.to_path_buf(),
                config,
                Profile::Default,
                registry,
            ),
            first,
            second,
        )
    }

    fn setup_with_provider(
        root: &Path,
        provider: Arc<dyn ModelProvider>,
    ) -> InteractiveSessionSetup {
        let providers = HashMap::from([("first".to_string(), provider)]);
        let models = HashMap::from([("first".to_string(), "model-a".to_string())]);
        InteractiveSessionSetup::for_test(
            root.to_path_buf(),
            Config::default(),
            Profile::Default,
            ProviderRegistry::from_providers(providers, models, "first"),
        )
    }

    #[test]
    fn interactive_config_preserves_every_chat_runtime_setting() {
        let mut config = Config::default();
        config.harness.context_token_limit = 99_999;
        config.harness.tool_call_budget = Some(17);
        config.harness.tool_call_budget_max = Some(31);
        config.harness.turn_timeout_secs = Some(43);
        config.harness.claim_gate =
            serde_json::from_str("\"warn\"").expect("deserialize claim-gate fixture");
        config
            .harness
            .rules
            .insert("check_before_launch".to_string(), RuleSeverity::Block);
        config.harness.verify_before_done = true;
        config.harness.verify_command = Some("cargo test".to_string());
        config.compaction.mode = CompactionMode::SmartWithFallback;
        config.compaction.summary_token_limit = 321;
        config.compaction.summarizer_input_tokens = 654;
        config.compaction.summarizer_timeout_secs = 9;
        config.tools.marker = true;
        config.tools.readable_errors = false;
        config.tools.repair = RepairMode::Warn;
        config.tools.elide_seen_reads = true;

        // The trust decision and the package-discovery hint are the launch
        // snapshot passed in verbatim — no longer re-derived from the profile here.
        let built = interactive_config(&config, "chosen", Some(20_000), true, false, false);
        assert_eq!(built.model, "chosen");
        assert_eq!(built.interactivity, Interactivity::Interactive);
        assert!(built.trusted);
        assert!(!built.package_discovery_disabled_but_present);
        assert_eq!(
            built.context_token_limit,
            localpilot_harness::effective_context_limit(Some(20_000), 99_999)
        );
        assert_eq!(
            built.compaction_mode,
            localpilot_harness::CompactionMode::SmartWithFallback
        );
        assert_eq!(built.summarizer_tuning.output_token_limit, 321);
        assert_eq!(built.summarizer_tuning.input_char_budget, 654 * 4);
        assert_eq!(built.summarizer_tuning.timeout.as_secs(), 9);
        assert_eq!(built.tool_call_budget, Some(17));
        assert_eq!(built.tool_call_budget_max, Some(31));
        assert!(built.tool_budget_explicit);
        assert_eq!(
            built.rules.get("check_before_launch"),
            Some(&RuleSeverity::Block)
        );
        assert!(built.enforce_claim_gate);
        assert!(built.tool_marker_enabled);
        assert!(!built.enforce_readable_errors);
        assert_eq!(built.repair_mode, RepairMode::Warn);
        assert!(built.elide_seen_reads);
        assert_eq!(built.turn_timeout, Some(std::time::Duration::from_secs(43)));
        assert!(built.verify_before_done);
        assert_eq!(built.verify_command.as_deref(), Some("cargo test"));

        // A passed-in untrusted snapshot + a set hint flow straight through.
        let untrusted = interactive_config(&config, "chosen", None, false, true, false);
        assert!(!untrusted.trusted);
        assert!(untrusted.package_discovery_disabled_but_present);
    }

    #[test]
    fn initial_package_discovery_hint_is_false_when_autonomous_discovery_is_on() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        let mut config = Config::default();
        config.skills.autonomous_discovery = true;
        // Discovery on ⇒ the package tools are registered ⇒ no "disabled" cue,
        // regardless of what is on disk (no scan even happens).
        assert!(!initial_package_discovery_hint(
            &config,
            dir.path(),
            /* trusted */ true
        ));
    }

    /// A one-provider registry for a `for_test` setup.
    fn one_provider_registry() -> ProviderRegistry {
        let first: Arc<dyn ModelProvider> = provider("first", None);
        let providers: HashMap<String, Arc<dyn ModelProvider>> =
            HashMap::from([("first".to_string(), first)]);
        ProviderRegistry::from_providers(providers, HashMap::new(), "first")
    }

    #[test]
    fn the_setup_snapshots_one_trust_decision_read_by_the_host_gate() {
        let dir = tempfile::tempdir().expect("temporary workspace");

        // A fresh temp dir is never in the trust store, so under a prompting
        // profile the one launch snapshot says trust is required — the same value
        // the host gate reads and the runtime is built untrusted from.
        let prompting = InteractiveSessionSetup::for_test(
            dir.path().to_path_buf(),
            Config::default(),
            Profile::Default,
            one_provider_registry(),
        );
        assert!(prompting.trust_required());

        // An explicit bypass profile needs no prompt: one decision, false.
        let bypass = InteractiveSessionSetup::for_test(
            dir.path().to_path_buf(),
            Config::default(),
            Profile::Bypass,
            one_provider_registry(),
        );
        assert!(!bypass.trust_required());
    }

    #[test]
    fn initial_package_discovery_hint_only_counts_discoverable_packages() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        let home = tempfile::tempdir().expect("global home");
        let global = home.path().join(".localpilot").join("skills");

        // Flag off, untrusted, project-only, NO injected home ⇒ the project overlay
        // is not read and the global baseline is absent ⇒ no hint.
        write_project_skill(dir.path(), "proj-pkg", "a project helper", false);
        let mut config = Config::default();
        config.skills.autonomous_discovery = false;
        assert!(!initial_package_discovery_hint_with(
            &config,
            dir.path(),
            None,
            /* trusted */ false
        ));

        // Untrusted, but an injected global with a Discoverable package ⇒ hint.
        write_skill_md_at(&global, "glob-pkg", "a global helper", false);
        assert!(initial_package_discovery_hint_with(
            &config,
            dir.path(),
            Some(home.path()),
            /* trusted */ false
        ));

        // Trusted, project Discoverable, no injected home ⇒ hint from the project.
        assert!(initial_package_discovery_hint_with(
            &config,
            dir.path(),
            None,
            /* trusted */ true
        ));

        // A UserOnly-only baseline must NOT set the hint.
        let user_only_home = tempfile::tempdir().expect("user-only home");
        write_skill_md_at(
            &user_only_home.path().join(".localpilot").join("skills"),
            "hidden-pkg",
            "hidden",
            true,
        );
        assert!(!initial_package_discovery_hint_with(
            &config,
            dir.path(),
            Some(user_only_home.path()),
            /* trusted */ false
        ));

        // Flag ON ⇒ never hinted (the tools are registered instead).
        config.skills.autonomous_discovery = true;
        assert!(!initial_package_discovery_hint_with(
            &config,
            dir.path(),
            Some(home.path()),
            /* trusted */ true
        ));
    }

    #[tokio::test]
    async fn the_launch_hint_is_snapshotted_once_and_reused_by_both_pair_peers() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        // A discoverable project package, discovery off, and a trusting profile so
        // the launch hint is true when the setup is built.
        write_project_skill(dir.path(), "pair-pkg", "a project helper", false);
        let first = provider("first", Some(32_000));
        let second = provider("second", Some(16_000));
        let providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::from([
            ("first".to_string(), first as Arc<dyn ModelProvider>),
            ("second".to_string(), second as Arc<dyn ModelProvider>),
        ]);
        let models = HashMap::from([
            ("first".to_string(), "model-a".to_string()),
            ("second".to_string(), "model-b".to_string()),
        ]);
        let mut config = Config::default();
        config.skills.autonomous_discovery = false;
        let setup = InteractiveSessionSetup::for_test(
            dir.path().to_path_buf(),
            config,
            Profile::Bypass,
            ProviderRegistry::from_providers(providers, models, "first"),
        );
        // The hint was captured once at construction; bypass needs no prompt.
        assert!(
            setup.package_discovery_hint,
            "the launch hint was snapshotted"
        );
        assert!(!setup.trust_required());

        // Remove the package AFTER the snapshot: a per-peer rescan would now miss
        // it, so if both peers still carry the cue the hint must be the snapshot.
        std::fs::remove_dir_all(dir.path().join(".localpilot")).unwrap();

        let bundle = setup
            .build_pair(
                "pair task",
                InteractivePeerSelection {
                    provider_id: "first",
                    model: "model-a",
                },
                InteractivePeerSelection {
                    provider_id: "second",
                    model: "model-b",
                },
            )
            .await
            .expect("build pair");

        const MARKER: &str = "discovery is off, not that there are no skills";
        for peer in [&bundle.a, &bundle.b] {
            assert_eq!(
                peer.session
                    .runtime
                    .system_prompt_text()
                    .matches(MARKER)
                    .count(),
                1,
                "both peers must carry the one snapshotted disabled cue exactly once"
            );
            assert_eq!(
                peer.session.runtime.trusted(),
                !setup.trust_required(),
                "both peers derive trust from the one snapshot"
            );
        }
    }

    /// The package-discovery-disabled cue marker, shared by the grant-path tests.
    const CUE_MARKER: &str = "discovery is off, not that there are no skills";

    /// A minimal UNTRUSTED runtime with only builtin tools and NO seeded cue —
    /// built directly (not via `setup.build`) so the initial cue state is
    /// deterministic and does not depend on the machine's real global catalog.
    fn untrusted_runtime(dir: &Path) -> SessionRuntime {
        let (approval_tx, _rx) = mpsc::unbounded_channel::<ApprovalCall>();
        SessionRuntime::new(
            provider("first", None) as Arc<dyn ModelProvider>,
            crate::mcp::McpTools::default().registry(),
            PermissionEngine::new(Profile::Default, Vec::new()),
            Box::new(TuiApprover::new(approval_tx)),
            Store::open(dir),
            Workspace::new(dir).expect("workspace"),
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig {
                model: "m".to_string(),
                interactivity: Interactivity::NonInteractive,
                trusted: false,
                package_discovery_disabled_but_present: false,
                ..SessionConfig::default()
            },
            Vec::new(),
        )
    }

    /// A read-only `ToolContext` keyed on a given trust value — the shape
    /// `session.rs` builds from `runtime.config.trusted` for every tool call.
    fn skill_lookup_ctx(ws: &Workspace, trusted: bool) -> ToolContext<'_> {
        ToolContext {
            workspace: ws,
            interactivity: Interactivity::NonInteractive,
            trusted,
            retention: None,
            processes: None,
            agents: None,
            prompter: None,
            peers: None,
        }
    }

    #[tokio::test]
    async fn a_trust_grant_makes_the_project_overlay_visible_to_a_real_tool_lookup() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        // A discoverable project-overlay skill package.
        write_project_skill(dir.path(), "proj-only-pkg", "a project helper", false);
        let (setup, _a, _b) = setup_with_two_providers(dir.path());
        // Untrusted launch (Profile::Default, fresh temp dir) ⇒ runtime built untrusted.
        let mut bundle = setup.build("first", "model-a").await.expect("build");
        assert!(!bundle.runtime.trusted(), "untrusted launch");

        let ws = Workspace::new(dir.path()).expect("workspace");
        // Before the grant: a REAL skill_list lookup keyed on runtime.trusted()
        // does NOT see the project overlay.
        let before = SkillList::new()
            .invoke(json!({}), &skill_lookup_ctx(&ws, bundle.runtime.trusted()))
            .await
            .expect("list before");
        assert!(
            !before.text.contains("proj-only-pkg"),
            "the project overlay is hidden before the grant: {}",
            before.text
        );

        // The accept path grants live trust.
        grant_live_trust(&mut bundle.runtime, setup.config(), dir.path());
        assert!(bundle.runtime.trusted(), "granted");

        // After: the same lookup, keyed on the now-true runtime trust, sees it.
        let after = SkillList::new()
            .invoke(json!({}), &skill_lookup_ctx(&ws, bundle.runtime.trusted()))
            .await
            .expect("list after");
        assert!(
            after.text.contains("proj-only-pkg"),
            "the project overlay is visible after the grant: {}",
            after.text
        );
        bundle.runtime.close();
    }

    #[test]
    fn a_trust_grant_appends_the_disabled_cue_false_true_true_through_the_seam() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        // A project-only Discoverable package; discovery off (default config).
        write_project_skill(dir.path(), "proj-pkg", "a project helper", false);
        let mut runtime = untrusted_runtime(dir.path());

        // Untrusted construction seeds NO cue (deterministic — no global-home read).
        assert_eq!(
            runtime.system_prompt_text().matches(CUE_MARKER).count(),
            0,
            "no cue before the grant"
        );
        // Grant with NO injected global home: the now-trusted project overlay
        // yields the hint, so the cue is appended exactly once (false→true).
        grant_live_trust_with(&mut runtime, &Config::default(), dir.path(), None);
        assert_eq!(
            runtime.system_prompt_text().matches(CUE_MARKER).count(),
            1,
            "the grant appends the cue exactly once"
        );
        // A repeat grant is idempotent — the monotonic helper never double-appends.
        grant_live_trust_with(&mut runtime, &Config::default(), dir.path(), None);
        assert_eq!(
            runtime.system_prompt_text().matches(CUE_MARKER).count(),
            1,
            "true→true is a no-op"
        );
        runtime.close();
    }

    #[test]
    fn a_grant_flips_trust_but_adds_no_cue_for_a_user_only_only_project() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        // Only a UserOnly package — never discoverable, so no cue is warranted.
        write_project_skill(dir.path(), "hidden-only", "hidden", true);
        let mut runtime = untrusted_runtime(dir.path());

        grant_live_trust_with(&mut runtime, &Config::default(), dir.path(), None);
        assert!(runtime.trusted(), "trust still flips");
        assert_eq!(
            runtime.system_prompt_text().matches(CUE_MARKER).count(),
            0,
            "a user-only package is not discoverable, so no cue"
        );
        runtime.close();
    }

    #[test]
    fn a_grant_flips_trust_but_adds_no_cue_when_autonomous_discovery_is_on() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        write_project_skill(dir.path(), "proj-pkg", "a project helper", false);
        let mut runtime = untrusted_runtime(dir.path());
        let mut config = Config::default();
        config.skills.autonomous_discovery = true;

        grant_live_trust_with(&mut runtime, &config, dir.path(), None);
        assert!(runtime.trusted(), "trust still flips");
        assert_eq!(
            runtime.system_prompt_text().matches(CUE_MARKER).count(),
            0,
            "discovery on means the tools are registered — no disabled cue"
        );
        runtime.close();
    }

    #[tokio::test]
    async fn pair_grant_trust_flips_both_peers() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        let (setup, _a, _b) = setup_with_two_providers(dir.path());
        let pair = InteractivePairHost::prepare(
            &setup,
            "review the change",
            InteractivePeerSelection {
                provider_id: "first",
                model: "model-a",
            },
            InteractivePeerSelection {
                provider_id: "second",
                model: "model-b",
            },
        )
        .await
        .expect("hosted pair");

        let sa = pair.owner.sessions[0];
        let sb = pair.owner.sessions[1];
        // Before: both peers untrusted (Profile::Default launch).
        assert!(!peer_trusted(&pair, sa).await);
        assert!(!peer_trusted(&pair, sb).await);

        pair.grant_trust(false).await.expect("grant both peers");
        assert!(peer_trusted(&pair, sa).await, "peer A trusted after grant");
        assert!(peer_trusted(&pair, sb).await, "peer B trusted after grant");

        pair.close().await;
    }

    #[tokio::test]
    async fn pair_grant_trust_is_all_or_error_when_a_peer_is_missing() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        let (setup, _a, _b) = setup_with_two_providers(dir.path());
        let pair = InteractivePairHost::prepare(
            &setup,
            "review the change",
            InteractivePeerSelection {
                provider_id: "first",
                model: "model-a",
            },
            InteractivePeerSelection {
                provider_id: "second",
                model: "model-b",
            },
        )
        .await
        .expect("hosted pair");
        let sa = pair.owner.sessions[0];
        let sb = pair.owner.sessions[1];

        // Peer B goes missing: grant must fail and mutate NEITHER peer (both
        // handles are resolved before either is touched).
        pair.owner.registry.remove(sb).await;
        assert!(
            pair.grant_trust(false).await.is_err(),
            "a missing peer is an error"
        );
        assert!(
            !peer_trusted(&pair, sa).await,
            "peer A must not be mutated when peer B is missing"
        );

        pair.close().await;
    }

    /// Read a hosted peer's live runtime trust through the shared registry.
    async fn peer_trusted(pair: &InteractivePairHost, session: SessionId) -> bool {
        pair.owner
            .registry
            .get(session)
            .await
            .expect("registered peer")
            .lock()
            .await
            .trusted()
    }

    /// Count the disabled-cue marker in a hosted peer's live system prompt.
    async fn peer_cue_count(pair: &InteractivePairHost, session: SessionId) -> usize {
        pair.owner
            .registry
            .get(session)
            .await
            .expect("registered peer")
            .lock()
            .await
            .system_prompt_text()
            .matches(CUE_MARKER)
            .count()
    }

    #[tokio::test]
    async fn pair_grant_makes_each_peer_overlay_visible_and_cues_once() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        // A project-only Discoverable package; discovery off (default config).
        write_project_skill(dir.path(), "pair-proj-pkg", "a project helper", false);
        let (setup, _a, _b) = setup_with_two_providers(dir.path());
        let pair = InteractivePairHost::prepare(
            &setup,
            "review the change",
            InteractivePeerSelection {
                provider_id: "first",
                model: "model-a",
            },
            InteractivePeerSelection {
                provider_id: "second",
                model: "model-b",
            },
        )
        .await
        .expect("hosted pair");
        let sessions = [pair.owner.sessions[0], pair.owner.sessions[1]];
        let ws = Workspace::new(dir.path()).expect("workspace");

        // BEFORE the grant: each peer is untrusted, so a REAL skill_list lookup
        // keyed on that peer's live trust hides the project overlay.
        for session in sessions {
            let trusted = peer_trusted(&pair, session).await;
            assert!(!trusted, "peer untrusted before grant");
            let out = SkillList::new()
                .invoke(json!({}), &skill_lookup_ctx(&ws, trusted))
                .await
                .expect("list before");
            assert!(
                !out.text.contains("pair-proj-pkg"),
                "the project overlay is hidden before the grant: {}",
                out.text
            );
        }

        // Grant BOTH peers through the pair's own all-or-error path, true-hint branch.
        pair.grant_trust(true).await.expect("grant both peers");

        // AFTER: each peer's real lookup shows the overlay, and each carries the cue once.
        for session in sessions {
            let trusted = peer_trusted(&pair, session).await;
            assert!(trusted, "peer trusted after grant");
            let out = SkillList::new()
                .invoke(json!({}), &skill_lookup_ctx(&ws, trusted))
                .await
                .expect("list after");
            assert!(
                out.text.contains("pair-proj-pkg"),
                "the project overlay is visible after the grant: {}",
                out.text
            );
            assert_eq!(peer_cue_count(&pair, session).await, 1, "cue once per peer");
        }

        // A repeated grant is idempotent per peer.
        pair.grant_trust(true).await.expect("grant again");
        for session in sessions {
            assert_eq!(
                peer_cue_count(&pair, session).await,
                1,
                "a repeat grant must not double-append per peer"
            );
        }
        pair.close().await;
    }

    #[tokio::test]
    async fn repeated_builds_are_distinct_and_keep_the_shared_model_registry() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let (setup, _, _) = setup_with_two_providers(directory.path());

        let mut first = setup.build("first", "model-a").await.expect("first");
        let mut second = setup.build("second", "model-b").await.expect("second");
        assert_ne!(first.runtime.session_id(), second.runtime.session_id());
        assert_eq!(first.runtime.active_provider_id(), "first");
        assert_eq!(first.runtime.active_model(), "model-a");
        assert_eq!(second.runtime.active_provider_id(), "second");
        assert_eq!(second.runtime.active_model(), "model-b");
        assert!(first.runtime.active_accepts_images());
        assert!(!second.runtime.active_accepts_images());
        assert_ne!(
            first.runtime.permission_engine_handle().profile(),
            Profile::Bypass
        );

        let switched = first
            .runtime
            .set_active_provider("second")
            .expect("shared provider registry is installed");
        assert_eq!(switched.model, "model-b");
        assert!(first.approvals.try_recv().is_err());
        assert!(first.questions.try_recv().is_err());
        assert!(second.approvals.try_recv().is_err());
        assert!(second.questions.try_recv().is_err());

        let error = setup
            .build("missing", "model-x")
            .await
            .err()
            .expect("unknown provider refused");
        assert!(error
            .to_string()
            .contains("provider 'missing' is not configured"));
    }

    #[tokio::test]
    async fn pair_builds_two_explicit_sessions_with_shared_task_and_fixed_identities() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let (setup, first_provider, second_provider) = setup_with_two_providers(directory.path());
        let task = "  Preserve the exact original task.\r\nAnd its edges.  ";
        let mut pair = setup
            .build_pair(
                task,
                InteractivePeerSelection {
                    provider_id: "first",
                    model: "model-a",
                },
                InteractivePeerSelection {
                    provider_id: "second",
                    model: "model-b",
                },
            )
            .await
            .expect("interactive pair");

        assert_eq!(pair.task, task);
        assert_eq!(pair.a.peer, PairPeer::A);
        assert_eq!(pair.b.peer, PairPeer::B);
        assert_eq!(pair.a.session.runtime.active_provider_id(), "first");
        assert_eq!(pair.a.session.runtime.active_model(), "model-a");
        assert_eq!(pair.b.session.runtime.active_provider_id(), "second");
        assert_eq!(pair.b.session.runtime.active_model(), "model-b");
        assert_ne!(
            pair.a.session.runtime.session_id(),
            pair.b.session.runtime.session_id()
        );
        assert_eq!(
            pair.a.session.runtime.store().root(),
            pair.b.session.runtime.store().root()
        );
        assert!(pair.a.session.runtime.active_accepts_images());
        assert!(!pair.b.session.runtime.active_accepts_images());

        // Both peers run under the same selected profile, through independent engines:
        // changing one peer's profile never touches the other's.
        assert_eq!(
            pair.a.session.runtime.permission_engine_handle().profile(),
            Profile::Default
        );
        assert_eq!(
            pair.b.session.runtime.permission_engine_handle().profile(),
            Profile::Default
        );
        pair.a
            .session
            .runtime
            .set_permission_profile(Profile::Unrestricted, Vec::new());
        assert_eq!(
            pair.a.session.runtime.permission_engine_handle().profile(),
            Profile::Unrestricted
        );
        assert_eq!(
            pair.b.session.runtime.permission_engine_handle().profile(),
            Profile::Default,
            "B's permission engine is independent of A's"
        );

        let a_directive = localpilot_server::swarm::pair_session_directive("A", "B", task);
        let b_directive = localpilot_server::swarm::pair_session_directive("B", "A", task);
        let a_prompt = pair.a.session.runtime.system_prompt_text();
        let b_prompt = pair.b.session.runtime.system_prompt_text();
        assert!(a_prompt.ends_with(&a_directive));
        assert!(b_prompt.ends_with(&b_directive));
        assert!(!a_prompt.contains("You are coordinating several agents"));
        assert!(!b_prompt.contains("You are coordinating several agents"));
        assert!(first_provider.requests().is_empty());
        assert!(second_provider.requests().is_empty());

        let switched_a = pair
            .a
            .session
            .runtime
            .set_active_provider("second")
            .expect("A shares provider registry");
        let switched_b = pair
            .b
            .session
            .runtime
            .set_active_provider("first")
            .expect("B shares provider registry");
        assert_eq!(switched_a.model, "model-b");
        assert_eq!(switched_b.model, "model-a");

        let (reply, _) = oneshot::channel();
        pair.a
            .session
            .approval_tx
            .send(ApprovalCall {
                request: ApprovalRequest {
                    tool: "pair-channel-probe".to_string(),
                    target: "A".to_string(),
                    risk_class: "test".to_string(),
                },
                reply,
            })
            .expect("send only to A");
        assert_eq!(
            pair.a
                .session
                .approvals
                .recv()
                .await
                .expect("A receives its approval")
                .request
                .target,
            "A"
        );
        assert!(pair.b.session.approvals.try_recv().is_err());
        assert!(pair.a.session.questions.try_recv().is_err());
        assert!(pair.b.session.questions.try_recv().is_err());
    }

    #[tokio::test]
    async fn interactive_pair_host_retains_exact_hosts_identity_and_four_input_receivers() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let (setup, first_provider, second_provider) = setup_with_two_providers(directory.path());
        let mut pair = InteractivePairHost::prepare(
            &setup,
            "review the change",
            InteractivePeerSelection {
                provider_id: "first",
                model: "model-a",
            },
            InteractivePeerSelection {
                provider_id: "second",
                model: "model-b",
            },
        )
        .await
        .expect("hosted pair");

        assert_ne!(pair.owner.sessions[0], pair.owner.sessions[1]);
        assert_eq!(pair.owner.sessions, pair.adopted.sessions());
        assert_eq!(pair.owner.task, "review the change");
        let [adopted_a, adopted_b] = pair.adopted.hosts();
        assert!(Arc::ptr_eq(&pair.owner.a.host, &adopted_a));
        assert!(Arc::ptr_eq(&pair.owner.b.host, &adopted_b));
        assert_eq!(pair.owner.a.host.subscriber_count(), 2);
        assert_eq!(pair.owner.b.host.subscriber_count(), 2);
        assert!(pair.owner.a.events.try_recv().is_err());
        assert!(pair.owner.b.events.try_recv().is_err());
        assert!(pair.owner.a.approvals.try_recv().is_err());
        assert!(pair.owner.a.questions.try_recv().is_err());
        assert!(pair.owner.b.approvals.try_recv().is_err());
        assert!(pair.owner.b.questions.try_recv().is_err());
        assert_eq!(pair.owner.registry.len().await, 2);
        assert!(first_provider.requests().is_empty());
        assert!(second_provider.requests().is_empty());

        pair.close().await;
    }

    #[tokio::test]
    async fn interactive_pair_host_consuming_split_keeps_exact_hosts_and_subscriptions() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let (setup, first_provider, second_provider) = setup_with_two_providers(directory.path());
        let pair = InteractivePairHost::prepare(
            &setup,
            "split ownership",
            InteractivePeerSelection {
                provider_id: "first",
                model: "model-a",
            },
            InteractivePeerSelection {
                provider_id: "second",
                model: "model-b",
            },
        )
        .await
        .expect("hosted pair");

        let (adopted, owner) = pair.into_parts();
        assert_eq!(adopted.sessions(), owner.sessions);
        let [adopted_a, adopted_b] = adopted.hosts();
        assert!(Arc::ptr_eq(&owner.a.host, &adopted_a));
        assert!(Arc::ptr_eq(&owner.b.host, &adopted_b));
        assert_eq!(owner.a.host.subscriber_count(), 2);
        assert_eq!(owner.b.host.subscriber_count(), 2);
        assert!(first_provider.requests().is_empty());
        assert!(second_provider.requests().is_empty());

        drop(adopted);
        owner.close().await;
    }

    #[tokio::test]
    async fn interactive_pair_host_close_removes_sessions_hosts_and_closes_both_logs() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let (setup, first_provider, second_provider) = setup_with_two_providers(directory.path());
        let bundle = setup
            .build_pair(
                "tear down both",
                InteractivePeerSelection {
                    provider_id: "first",
                    model: "model-a",
                },
                InteractivePeerSelection {
                    provider_id: "second",
                    model: "model-b",
                },
            )
            .await
            .expect("pair bundle");
        let sessions = [
            bundle.a.session.runtime.session_id(),
            bundle.b.session.runtime.session_id(),
        ];
        let registry = SessionRegistry::new();
        let external_registry = registry.clone();
        let swarm_host = SwarmHost::for_adoption(registry.clone(), SwarmRegistry::new());
        let external_host = swarm_host.clone();
        let pair = InteractivePairHost::from_bundle(
            directory.path().to_path_buf(),
            bundle,
            registry,
            swarm_host,
            swarm_id_for_dir(directory.path()),
        )
        .await
        .expect("hosted pair");

        pair.close().await;

        assert!(external_registry.is_empty().await);
        for session in sessions {
            assert!(external_host.host(session).await.is_none());
            assert_session_closed(directory.path(), session);
        }
        assert!(first_provider.requests().is_empty());
        assert!(second_provider.requests().is_empty());
    }

    #[tokio::test]
    async fn interactive_pair_host_admission_failure_rolls_back_both_sessions() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let (setup, first_provider, second_provider) = setup_with_two_providers(directory.path());
        let bundle = setup
            .build_pair(
                "refuse atomically",
                InteractivePeerSelection {
                    provider_id: "first",
                    model: "model-a",
                },
                InteractivePeerSelection {
                    provider_id: "second",
                    model: "model-b",
                },
            )
            .await
            .expect("pair bundle");
        let sessions = [
            bundle.a.session.runtime.session_id(),
            bundle.b.session.runtime.session_id(),
        ];
        let registry = SessionRegistry::new();
        let external_registry = registry.clone();
        let swarms = SwarmRegistry::with_limits(SwarmLimits {
            max_members: 1,
            max_active: 4,
        });
        let external_swarms = swarms.clone();
        let swarm_host = SwarmHost::for_adoption(registry.clone(), swarms);
        let external_host = swarm_host.clone();
        let swarm = swarm_id_for_dir(directory.path());

        let error = InteractivePairHost::from_bundle(
            directory.path().to_path_buf(),
            bundle,
            registry,
            swarm_host,
            swarm.clone(),
        )
        .await
        .err()
        .expect("pair admission must fail");

        assert!(matches!(
            error.downcast_ref::<SpawnError>(),
            Some(SpawnError::Admission(SwarmError::MemberCapReached {
                cap: 1
            }))
        ));
        assert!(external_registry.is_empty().await);
        assert!(external_swarms.members(&swarm).await.is_empty());
        for session in sessions {
            assert!(external_host.host(session).await.is_none());
            assert_session_closed(directory.path(), session);
        }
        assert!(first_provider.requests().is_empty());
        assert!(second_provider.requests().is_empty());
    }

    #[tokio::test]
    async fn failed_second_pair_build_closes_the_already_built_first_session() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let (setup, first_provider, second_provider) = setup_with_two_providers(directory.path());
        let a = InteractivePeerSelection {
            provider_id: "first",
            model: "model-a",
        };
        let a_session = setup.build(a.provider_id, a.model).await.expect("peer A");
        let a_id = a_session.runtime.session_id();

        let error = finish_pair_build(
            directory.path(),
            "cleanup partial construction",
            a_session,
            Err(anyhow::anyhow!("peer B construction failed")),
        )
        .err()
        .expect("peer B error is preserved");

        assert_eq!(error.to_string(), "peer B construction failed");
        assert_session_closed(directory.path(), a_id);
        assert!(first_provider.requests().is_empty());
        assert!(second_provider.requests().is_empty());
    }

    fn assert_session_closed(root: &Path, session: SessionId) {
        let events = Store::open(root)
            .read_events(session)
            .expect("session event log");
        assert!(
            events
                .iter()
                .any(|event| event.kind == SessionEventKind::SessionClosed),
            "session {session} was not closed: {events:?}"
        );
    }

    #[tokio::test]
    async fn pair_validation_refuses_both_selections_before_any_build_or_fallback() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let (setup, first_provider, second_provider) = setup_with_two_providers(directory.path());
        let valid_a = InteractivePeerSelection {
            provider_id: "first",
            model: "model-a",
        };
        let valid_b = InteractivePeerSelection {
            provider_id: "second",
            model: "model-b",
        };
        let invalid = [
            (
                "empty task",
                "   ",
                valid_a,
                valid_b,
                "pair task must not be empty",
            ),
            (
                "empty A provider",
                "task",
                InteractivePeerSelection {
                    provider_id: "",
                    model: "model-a",
                },
                valid_b,
                "peer A provider must not be empty",
            ),
            (
                "empty B model",
                "task",
                valid_a,
                InteractivePeerSelection {
                    provider_id: "second",
                    model: "  ",
                },
                "peer B model must not be empty",
            ),
            (
                "unknown A provider",
                "task",
                InteractivePeerSelection {
                    provider_id: "missing",
                    model: "model-a",
                },
                valid_b,
                "peer A provider 'missing' is not configured",
            ),
            (
                "unknown B provider",
                "task",
                valid_a,
                InteractivePeerSelection {
                    provider_id: "missing",
                    model: "model-b",
                },
                "peer B provider 'missing' is not configured",
            ),
            (
                "provider whitespace is not trimmed",
                "task",
                InteractivePeerSelection {
                    provider_id: " first ",
                    model: "model-a",
                },
                valid_b,
                "peer A provider must not have leading or trailing whitespace",
            ),
            (
                "model whitespace is not trimmed",
                "task",
                valid_a,
                InteractivePeerSelection {
                    provider_id: "second",
                    model: " model-b ",
                },
                "peer B model must not have leading or trailing whitespace",
            ),
        ];

        for (case, task, a, b, expected) in invalid {
            let error = setup
                .build_pair(task, a, b)
                .await
                .err()
                .unwrap_or_else(|| panic!("{case} must fail"));
            assert_eq!(error.to_string(), expected, "{case}");
        }
        assert!(first_provider.requests().is_empty());
        assert!(second_provider.requests().is_empty());
    }

    #[tokio::test]
    async fn built_runtime_routes_real_questions_and_approvals_to_its_bundle() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let seed = FakeProvider::new();
        let mut declaration = seed.declaration().clone();
        declaration.id = "first".to_string();
        declaration.display_name = "first".to_string();
        let scripted: Arc<dyn ModelProvider> = Arc::new(
            FakeProvider::new()
                .with_declaration(declaration)
                .tool_call(
                    "question",
                    "ask_user",
                    json!({
                        "questions": [{
                            "header": "Database",
                            "question": "Which database?",
                            "options": [
                                {"label": "Postgres"},
                                {"label": "SQLite"}
                            ]
                        }]
                    }),
                )
                .tool_call(
                    "write",
                    "write_file",
                    json!({"path": "answer.txt", "content": "Postgres"}),
                )
                .text("done"),
        );
        let setup = setup_with_provider(directory.path(), scripted);
        let InteractiveSessionBundle {
            mut runtime,
            mut approvals,
            mut questions,
            ..
        } = setup.build("first", "model-a").await.expect("bundle");
        let (events, _) = tokio::sync::broadcast::channel(32);
        let cancel = CancellationToken::new();
        let turn = runtime.run_turn("choose and record it", &events, &cancel);
        tokio::pin!(turn);

        let question = tokio::select! {
            call = questions.recv() => call.expect("question reaches its host"),
            reason = &mut turn => panic!("turn stopped before question was answered: {reason:?}"),
        };
        assert_eq!(question.questions[0].question, "Which database?");
        assert!(approvals.try_recv().is_err());
        question
            .reply
            .send(vec![UserAnswer::Selected(vec!["Postgres".to_string()])])
            .expect("answer question");

        let approval = tokio::select! {
            call = approvals.recv() => call.expect("approval reaches its host"),
            reason = &mut turn => panic!("turn stopped before approval was answered: {reason:?}"),
        };
        assert_eq!(approval.request.tool, "write_file");
        assert_eq!(approval.request.target, "answer.txt");
        assert!(questions.try_recv().is_err());
        approval.reply.send(true).expect("approve write");

        assert_eq!(turn.await, StopReason::Done);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("answer.txt")).expect("written file"),
            "Postgres"
        );
    }

    #[tokio::test]
    async fn approval_and_question_routes_are_fail_closed_and_independent() {
        let (approval_tx, mut approvals) = mpsc::unbounded_channel();
        let approver = TuiApprover::new(approval_tx);
        let request = PermissionRequest {
            tool: "run_shell".to_string(),
            effect: Effect::RunCommand(CommandClass::Unknown),
            interactivity: Interactivity::Interactive,
            trusted: true,
            detail: "cargo test".to_string(),
        };
        let approval = approver.approve(&request);
        tokio::pin!(approval);
        let call = tokio::select! {
            call = approvals.recv() => call.expect("approval routed"),
            answer = &mut approval => panic!("approval completed before host answer: {answer}"),
        };
        assert_eq!(call.request.tool, "run_shell");
        call.reply.send(true).expect("answer approval");
        assert!(approval.await);

        let question = UserQuestion {
            header: Some("Database".to_string()),
            question: "Which one?".to_string(),
            options: vec![
                localpilot_tools::QuestionOption {
                    label: "Postgres".to_string(),
                    description: None,
                },
                localpilot_tools::QuestionOption {
                    label: "SQLite".to_string(),
                    description: None,
                },
            ],
            multi_select: false,
        };
        let (question_tx, mut questions) = mpsc::unbounded_channel();
        let prompter = TuiPrompter { tx: question_tx };
        let answer = prompter.ask(std::slice::from_ref(&question));
        tokio::pin!(answer);
        let call = tokio::select! {
            call = questions.recv() => call.expect("question routed"),
            answer = &mut answer => panic!("question completed before host answer: {answer:?}"),
        };
        assert_eq!(call.questions, vec![question.clone()]);
        call.reply
            .send(vec![UserAnswer::Selected(vec!["Postgres".to_string()])])
            .expect("answer question");
        assert_eq!(
            answer.await,
            vec![UserAnswer::Selected(vec!["Postgres".to_string()])]
        );

        let (closed_tx, closed_rx) = mpsc::unbounded_channel();
        drop(closed_rx);
        assert!(!TuiApprover::new(closed_tx).approve(&request).await);

        let (closed_tx, closed_rx) = mpsc::unbounded_channel();
        drop(closed_rx);
        let answers = TuiPrompter { tx: closed_tx }
            .ask(std::slice::from_ref(&call.questions[0]))
            .await;
        assert_eq!(answers, vec![UserAnswer::Dismissed]);
        assert_eq!(
            PermissionEngine::new(Profile::Default, Vec::new()).decide(&request),
            Decision::Ask
        );
    }
}
