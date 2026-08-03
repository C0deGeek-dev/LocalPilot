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
use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::ProviderRegistry;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{
    Approver, Effect, Interactivity, PermissionEngine, PermissionRequest, Profile,
};
use localpilot_store::Store;
use localpilot_tools::{UserAnswer, UserPrompter, UserQuestion};
use localpilot_tui::ApprovalRequest;
use tokio::sync::{mpsc, oneshot};

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
#[allow(dead_code)]
pub(crate) enum PairPeer {
    A,
    B,
}

impl PairPeer {
    #[allow(dead_code)]
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
        }
    }
}

/// One explicit provider/model choice for a pair member.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct InteractivePeerSelection<'a> {
    pub(crate) provider_id: &'a str,
    pub(crate) model: &'a str,
}

/// One identified member of a freshly constructed interactive pair.
#[allow(dead_code)]
pub(crate) struct InteractivePeerBundle {
    pub(crate) peer: PairPeer,
    pub(crate) provider_id: String,
    pub(crate) model: String,
    pub(crate) session: InteractiveSessionBundle,
}

/// Two ordinary interactive sessions sharing one workspace and original task.
#[allow(dead_code)]
pub(crate) struct InteractivePairBundle {
    task: String,
    pub(crate) a: InteractivePeerBundle,
    pub(crate) b: InteractivePeerBundle,
}

impl InteractivePairBundle {
    /// The exact task installed into both prompts and later handed to the driver.
    #[allow(dead_code)]
    pub(crate) fn task(&self) -> &str {
        &self.task
    }
}

impl InteractiveSessionSetup {
    /// Resolve provider, MCP, and agent resources once for this workspace.
    pub(crate) async fn resolve(
        cwd: PathBuf,
        config: Config,
        profile: Profile,
    ) -> anyhow::Result<Self> {
        let providers = Arc::new(ProviderRegistry::from_config(&config)?);
        let mcp = crate::mcp::McpTools::load(&config).await;
        let agents = crate::agents_cmd::session_agents(&cwd);
        Ok(Self {
            config,
            cwd,
            profile,
            providers,
            mcp,
            agents,
        })
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
            Store::open(&self.cwd),
            crate::session_cmd::workspace_with_read_roots(&self.cwd, &self.config)?,
            RecoveryEngine::new(RecoveryBudget::default()),
            interactive_config(&self.config, self.profile, model, context_window),
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
    #[allow(dead_code)]
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

        let mut a_session = self.build(a.provider_id, a.model).await?;
        let mut b_session = self.build(b.provider_id, b.model).await?;
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
                provider_id: a.provider_id.to_string(),
                model: a.model.to_string(),
                session: a_session,
            },
            b: InteractivePeerBundle {
                peer: PairPeer::B,
                provider_id: b.provider_id.to_string(),
                model: b.model.to_string(),
                session: b_session,
            },
        })
    }

    #[allow(dead_code)]
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
    fn for_test(
        cwd: PathBuf,
        config: Config,
        profile: Profile,
        providers: ProviderRegistry,
    ) -> Self {
        Self {
            config,
            cwd,
            profile,
            providers: Arc::new(providers),
            mcp: crate::mcp::McpTools::default(),
            agents: None,
        }
    }
}

/// The exact interactive `SessionConfig` formerly constructed inline by chat.
fn interactive_config(
    config: &Config,
    profile: Profile,
    model: &str,
    context_window: Option<u64>,
) -> SessionConfig {
    let rails = config.harness.resolved_rails(true);
    SessionConfig {
        model: model.to_string(),
        interactivity: Interactivity::Interactive,
        trusted: matches!(profile, Profile::Bypass | Profile::Unrestricted),
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
    use localpilot_sandbox::{CommandClass, Decision};
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

        let built = interactive_config(&config, Profile::Unrestricted, "chosen", Some(20_000));
        assert_eq!(built.model, "chosen");
        assert_eq!(built.interactivity, Interactivity::Interactive);
        assert!(built.trusted);
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

        let untrusted = interactive_config(&config, Profile::Default, "chosen", None);
        assert!(!untrusted.trusted);
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

        assert_eq!(pair.task(), task);
        assert_eq!(pair.a.peer, PairPeer::A);
        assert_eq!(pair.b.peer, PairPeer::B);
        assert_eq!(pair.a.provider_id, "first");
        assert_eq!(pair.a.model, "model-a");
        assert_eq!(pair.b.provider_id, "second");
        assert_eq!(pair.b.model, "model-b");
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
