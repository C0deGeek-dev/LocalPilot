//! `localpilot swarm run <plan>`: construct a swarm host and run a task plan,
//! putting each worker on the model its node asked for.
//!
//! This is the surface that lifts the swarm substrate out of "library-only":
//! everything under `localpilot-server/src/swarm/*` was constructed only by
//! tests, and the model-callable `swarm` tool is messaging-only. A CLI verb was
//! chosen over a spawn-capable tool action because the CLI is the layer that
//! actually holds a provider and a model, because "run this plan" is a user
//! action rather than something a mid-turn agent does (letting a running session
//! spawn workers is autonomous-loop territory, out of scope here), and because a
//! command constructs the host directly and so is straightforward to drive with
//! fake providers offline.
//!
//! The pieces:
//!
//! - [`HostedWorkerFactory`] is the production [`WorkerFactory`]: it builds each
//!   worker as an ordinary headless session on the model its spawn asked for,
//!   reusing the shared `SessionSetup` recipe rather than a second builder.
//! - [`run`] resolves the setup, loads the plan file, adopts a coordinator, and
//!   runs the plan to completion.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use localpilot_core::SessionId;
use localpilot_harness::SessionRuntime;
use localpilot_llm::ModelProvider;
use localpilot_sandbox::Profile;
use localpilot_server::registry::SessionRegistry;
use localpilot_server::swarm::driver::{run_plan, DriverConfig, RunReport};
use localpilot_server::swarm::registry::{SwarmLimits, SwarmRegistry};
use localpilot_server::swarm::scope::{swarm_id_for_dir, SwarmId};
use localpilot_server::swarm::spawn::{SpawnRequest, SwarmHost, WorkerFactory};
use localpilot_taskgraph::ops::seed;
use localpilot_taskgraph::{ActorId, NodeSpec, PlanMode, TaskPlan};
use serde::Deserialize;

use crate::server_cmd::SessionSetup;

/// The idempotency key the entrypoint seeds a plan under: a fixed string, so a
/// retried run replays the same node ids rather than growing a second copy.
const SEED_KEY: &str = "swarm-run";

/// How many workers may run at once by default — and so, since a worker holds
/// one model, how many models may be resident at once. The real resource bound
/// (RAM/VRAM): with N agents each on their own model, N model loads exist, so
/// this caps the fan-out that would otherwise exhaust the machine. Overridable
/// per run with `--concurrency`.
const DEFAULT_CONCURRENCY: usize = 4;

/// The default lifetime member cap — a runaway guard (it counts departed members,
/// so a plan that keeps re-spawning replacements still stops), distinct from the
/// concurrency bound above. Overridable with `--max-agents`.
const DEFAULT_MAX_AGENTS: usize = 64;

/// A plan as a file expresses it: an objective, a run mode, and the batch of
/// nodes — each an ordinary [`NodeSpec`], so a node may name the model it wants
/// via the spec's `model` field and depend on earlier nodes by batch position.
#[derive(Debug, Deserialize)]
struct PlanFile {
    /// What the plan is for, carried into every worker's assignment.
    objective: String,
    /// How strictly to run it. Defaults to `light`.
    #[serde(default)]
    mode: PlanMode,
    /// The tasks to seed, in order.
    nodes: Vec<NodeSpec>,
}

impl PlanFile {
    /// Read and parse a plan file, failing with the path in the message so a
    /// typo is obvious.
    fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| anyhow::anyhow!("cannot read plan {}: {error}", path.display()))?;
        let plan: PlanFile = serde_json::from_str(&text)
            .map_err(|error| anyhow::anyhow!("cannot parse plan {}: {error}", path.display()))?;
        if plan.nodes.is_empty() {
            anyhow::bail!("plan {} has no nodes to run", path.display());
        }
        Ok(plan)
    }
}

/// The production [`WorkerFactory`]. Builds each worker as an ordinary headless
/// session on the model its spawn asked for, reusing the shared `SessionSetup`
/// recipe. The host owns this because it is the only layer that holds a provider
/// and a model — the server crate deliberately does not, which is why session
/// construction is left to a factory it is handed.
///
/// A `routes` table maps each model a configured provider advertises to that
/// provider, so a worker whose node names a model a *non-default* provider
/// serves is built on that provider. It also defines availability: a model no
/// configured provider advertises is refused *before* the build (fail-loud,
/// never a silent fallback to the default), which is the whole point of letting
/// a plan pin a model — a typo or an unconfigured model must not quietly run on
/// something else.
pub(crate) struct HostedWorkerFactory {
    setup: Arc<SessionSetup>,
    routes: HashMap<String, Arc<dyn ModelProvider>>,
}

impl HostedWorkerFactory {
    /// A factory over the setup's default provider. The default model is served
    /// by the default provider; [`with_routes`](Self::with_routes) adds more.
    pub(crate) fn new(setup: Arc<SessionSetup>) -> Self {
        let mut routes = HashMap::new();
        routes.insert(setup.model().to_string(), setup.provider().clone());
        Self { setup, routes }
    }

    /// Add model→provider routes so a worker whose node names a model a
    /// *non-default* provider advertises is built on that provider, and so that
    /// model counts as available.
    #[must_use]
    pub(crate) fn with_routes(mut self, routes: HashMap<String, Arc<dyn ModelProvider>>) -> Self {
        self.routes.extend(routes);
        self
    }

    /// The provider to build `model` on: the one that advertises it, or the
    /// default provider. The availability gate ensures a routed provider exists
    /// before this is reached for a *requested* model; the default covers a
    /// `None` model resolving to the session default.
    fn provider_for(&self, model: &str) -> Arc<dyn ModelProvider> {
        self.routes
            .get(model)
            .cloned()
            .unwrap_or_else(|| self.setup.provider().clone())
    }
}

impl WorkerFactory for HostedWorkerFactory {
    fn create(&self, request: &SpawnRequest) -> Result<SessionRuntime, String> {
        // Honour the requested model; `None` means the session's default model.
        // The factory only *attempts* the model — the spawn path verifies the
        // built session really runs on it and refuses a mismatch.
        let model = request
            .model
            .as_deref()
            .unwrap_or_else(|| self.setup.model());
        let provider = self.provider_for(model);
        self.setup
            .build_worker(&provider, model)
            .map_err(|error| error.to_string())
    }

    fn ensure_model_available(&self, model: &str) -> Result<(), String> {
        if self.routes.contains_key(model) {
            return Ok(());
        }
        let mut known: Vec<&str> = self.routes.keys().map(String::as_str).collect();
        known.sort_unstable();
        Err(format!(
            "no configured provider advertises it; the configured models are [{}]. Add a provider \
             entry with `model = {model:?}` to run a task on it.",
            known.join(", ")
        ))
    }
}

/// Run a task plan as a swarm, putting each worker on the model its node asked
/// for.
///
/// Resolves the workspace's provider/model/profile/MCP once (the shared
/// `SessionSetup` recipe), builds a [`SwarmHost`] over the production
/// [`HostedWorkerFactory`], adopts a coordinator, seeds the plan, and runs it to
/// completion — printing a short report of what ran.
///
/// # Errors
/// Returns an error if the plan cannot be read, the setup cannot be resolved, or
/// the coordinator cannot be adopted.
pub(crate) async fn run(
    plan_path: &Path,
    model: Option<&str>,
    provider_id: Option<&str>,
    profile: Profile,
    concurrency: Option<usize>,
    max_agents: Option<usize>,
) -> anyhow::Result<()> {
    let plan_file = PlanFile::load(plan_path)?;
    let setup = Arc::new(SessionSetup::resolve(model, provider_id, profile).await?);
    let swarm = swarm_id_for_dir(setup.cwd());
    // Route every model a configured provider advertises to that provider, so a
    // node may target a model a non-default provider serves — and so a model
    // none of them advertises is refused rather than silently defaulted.
    let factory =
        Arc::new(HostedWorkerFactory::new(setup.clone()).with_routes(setup.provider_routes()));

    // Bound the fan-out. `--concurrency` caps how many workers — and so how many
    // models — run at once (the RAM/VRAM bound); the swarm's active budget is one
    // more than that, because the coordinator is itself an active member holding a
    // slot. `--max-agents` is the lifetime runaway guard.
    let concurrency = concurrency.unwrap_or(DEFAULT_CONCURRENCY).max(1);
    let limits = SwarmLimits {
        max_active: concurrency + 1,
        max_members: max_agents
            .unwrap_or(DEFAULT_MAX_AGENTS)
            .max(concurrency + 1),
    };
    let driver = DriverConfig {
        concurrency,
        ..DriverConfig::default()
    };

    let sessions = SessionRegistry::new();
    let host = SwarmHost::new(
        sessions.clone(),
        SwarmRegistry::with_limits(limits),
        factory,
    );
    let coordinator = adopt_coordinator(&host, &sessions, &setup, &swarm).await?;

    let report = drive(&host, &swarm, coordinator, plan_file, driver).await?;
    print_report(&report);
    Ok(())
}

/// Build and register a coordinator session, then adopt it as the swarm's root
/// so worker reports have somewhere to land.
async fn adopt_coordinator(
    host: &SwarmHost,
    sessions: &SessionRegistry,
    setup: &SessionSetup,
    swarm: &SwarmId,
) -> anyhow::Result<SessionId> {
    let runtime = setup
        .build_worker(setup.provider(), setup.model())
        .map_err(|error| anyhow::anyhow!("could not build the coordinator session: {error}"))?;
    let coordinator = sessions.register(runtime).await?;
    host.adopt_root(swarm, coordinator, "coordinator").await?;
    Ok(coordinator)
}

/// Seed the plan onto the swarm and run it to completion.
///
/// Split from [`run`] so a test can drive it against a host built over fake
/// providers, with no config or network anywhere.
async fn drive(
    host: &SwarmHost,
    swarm: &SwarmId,
    coordinator: SessionId,
    plan_file: PlanFile,
    config: DriverConfig,
) -> anyhow::Result<RunReport> {
    let owner = ActorId::new(coordinator.to_string());
    let mut plan = TaskPlan::new(plan_file.objective, plan_file.mode, owner.clone());
    seed(&mut plan, &owner, SEED_KEY, &plan_file.nodes)?;
    host.swarms().set_plan(swarm, plan).await;
    Ok(run_plan(host, swarm, coordinator, config).await)
}

/// Print a short, plain-text summary of the run to stderr, so a piped stdout
/// stays clean.
fn print_report(report: &RunReport) {
    eprintln!(
        "swarm run finished: {} dispatched, {} completed, {} failed, {} abandoned (peak {} in \
         flight){}",
        report.dispatched,
        report.completed,
        report.failed,
        report.abandoned,
        report.peak_in_flight,
        if report.settled {
            ""
        } else {
            "; the plan did not settle"
        },
    );
    if let Some(hint) = &report.starvation {
        eprintln!("swarm run note: {hint}");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use futures::StreamExt as _;
    use localpilot_llm::{
        FakeProvider, ModelEvent, ModelEventStream, ModelProvider, ModelRequest,
        ProviderDeclaration, ProviderError,
    };

    /// A stateless stand-in provider: every request returns the same short text,
    /// however many workers call it concurrently. `FakeProvider`'s script queue
    /// is *consumed*, so a single one shared across two workers starves the
    /// second — the fixture-queue trap the swarm substrate warns of. A real
    /// provider is stateless across requests; this models that.
    struct AlwaysAnswers {
        declaration: ProviderDeclaration,
        text: String,
    }

    impl AlwaysAnswers {
        fn arc(text: &str) -> Arc<dyn ModelProvider> {
            Arc::new(Self {
                declaration: FakeProvider::new().declaration().clone(),
                text: text.to_string(),
            })
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for AlwaysAnswers {
        fn declaration(&self) -> &ProviderDeclaration {
            &self.declaration
        }

        async fn stream(&self, _request: ModelRequest) -> Result<ModelEventStream, ProviderError> {
            Ok(futures::stream::iter(vec![
                Ok(ModelEvent::TextDelta(self.text.clone())),
                Ok(ModelEvent::Done),
            ])
            .boxed())
        }
    }

    /// A setup over a stateless fake provider, plus the temp dir its store lives
    /// in — kept by the caller so the path stays valid for the session's life.
    fn setup_on(model: &str) -> (tempfile::TempDir, Arc<SessionSetup>) {
        let dir = tempfile::tempdir().unwrap();
        let setup = Arc::new(SessionSetup::for_test(
            AlwaysAnswers::arc("done"),
            model,
            dir.path().to_path_buf(),
        ));
        (dir, setup)
    }

    /// 01.1/01.2: the factory builds a worker on the requested model, and on the
    /// session default when the request names none.
    #[test]
    fn the_factory_builds_a_worker_on_the_requested_model() {
        let (_dir, setup) = setup_on("default-model");
        let factory = HostedWorkerFactory::new(setup);
        let swarm = SwarmId::new("t");

        let requested = factory
            .create(
                &SpawnRequest::new(swarm.clone(), SessionId::new(), "w", "do it")
                    .with_model("pinned-model"),
            )
            .unwrap();
        assert_eq!(
            requested.model(),
            "pinned-model",
            "the worker runs on the model the spawn asked for"
        );

        let defaulted = factory
            .create(&SpawnRequest::new(swarm, SessionId::new(), "w", "do it"))
            .unwrap();
        assert_eq!(
            defaulted.model(),
            "default-model",
            "no requested model means the session default"
        );
    }

    /// A `PlanFile` deserializes an objective, an optional mode, and nodes whose
    /// per-node model is carried through.
    #[test]
    fn a_plan_file_parses_nodes_and_their_models() {
        let json = r#"{
            "objective": "make it green",
            "mode": "light",
            "nodes": [
                { "title": "a", "prompt": "do a", "model": "fast" },
                { "title": "b", "prompt": "do b", "depends_on_batch": [0] }
            ]
        }"#;
        let plan: PlanFile = serde_json::from_str(json).unwrap();
        assert_eq!(plan.objective, "make it green");
        assert_eq!(plan.mode, PlanMode::Light);
        assert_eq!(plan.nodes.len(), 2);
        assert_eq!(plan.nodes[0].model.as_deref(), Some("fast"));
        assert_eq!(plan.nodes[1].model, None);
        assert_eq!(plan.nodes[1].depends_on_batch, vec![0]);
    }

    /// A plan with no nodes is refused rather than running an empty swarm.
    #[test]
    fn a_plan_with_no_nodes_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.json");
        std::fs::write(&path, r#"{ "objective": "nothing", "nodes": [] }"#).unwrap();
        assert!(PlanFile::load(&path).is_err());
    }

    /// Per spawn: (task name, requested model, built model).
    type SpawnRecord = Arc<std::sync::Mutex<Vec<(String, Option<String>, String)>>>;

    /// A factory that records what each spawn asked for and what it built, so a
    /// test can prove the per-node model travelled all the way into the worker.
    struct RecordingFactory {
        inner: HostedWorkerFactory,
        seen: SpawnRecord,
    }

    impl WorkerFactory for RecordingFactory {
        fn create(&self, request: &SpawnRequest) -> Result<SessionRuntime, String> {
            let runtime = self.inner.create(request)?;
            self.seen.lock().unwrap().push((
                request.name.clone(),
                request.model.clone(),
                runtime.model().to_string(),
            ));
            Ok(runtime)
        }

        // Delegate the availability gate, so an unserved model is refused before
        // `create` and never recorded — exactly what the real spawn path sees.
        fn ensure_model_available(&self, model: &str) -> Result<(), String> {
            self.inner.ensure_model_available(model)
        }
    }

    /// 03.4 (and the 04.4 runtime-flavour guard): the entrypoint runs a small
    /// plan to completion over fake providers, a worker spawns per task via the
    /// production factory, and each node's model reaches its spawn request *and*
    /// the session actually built. Deliberately on the **current-thread** runtime
    /// (`#[tokio::test]` default): if any part of the swarm path called
    /// `block_in_place`, this would panic instead of pass.
    #[tokio::test]
    async fn the_entrypoint_runs_a_plan_and_each_worker_gets_its_nodes_model() {
        let (_dir, setup) = setup_on("default-model");
        let swarm = swarm_id_for_dir(setup.cwd());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        // Advertise "careful-model" too, so the pinned node's model is available.
        let mut routes = HashMap::new();
        routes.insert("careful-model".to_string(), setup.provider().clone());
        let factory = Arc::new(RecordingFactory {
            inner: HostedWorkerFactory::new(setup.clone()).with_routes(routes),
            seen: seen.clone(),
        });

        let sessions = SessionRegistry::new();
        let host = SwarmHost::new(sessions.clone(), SwarmRegistry::new(), factory);
        let coordinator = adopt_coordinator(&host, &sessions, &setup, &swarm)
            .await
            .unwrap();

        let plan = PlanFile {
            objective: "make it green".to_string(),
            mode: PlanMode::Light,
            nodes: vec![
                NodeSpec::task("pinned", "run me on a specific model").on_model("careful-model"),
                NodeSpec::task("default", "run me on the session default"),
            ],
        };
        let report = drive(&host, &swarm, coordinator, plan, DriverConfig::default())
            .await
            .unwrap();

        assert!(report.settled, "the plan settled: {report:?}");
        assert_eq!(report.dispatched, 2, "one worker per task: {report:?}");
        assert_eq!(
            report.completed, 2,
            "both workers ran on the model their node asked for, so neither was refused for a \
             mismatch: {report:?}"
        );

        let seen = seen.lock().unwrap().clone();
        let pinned = seen
            .iter()
            .find(|(name, _, _)| name == "pinned")
            .expect("the pinned task spawned a worker");
        let default = seen
            .iter()
            .find(|(name, _, _)| name == "default")
            .expect("the default task spawned a worker");
        assert_eq!(
            pinned.1.as_deref(),
            Some("careful-model"),
            "the node's model reached its spawn request"
        );
        assert_eq!(
            pinned.2, "careful-model",
            "and the worker session was built on it"
        );
        assert_eq!(
            default.1, None,
            "the default node's spawn request is model-less"
        );
        assert_eq!(
            default.2, "default-model",
            "and its worker runs on the session default"
        );
    }

    /// 04.2: routing resolves each model to the provider that advertises it, and
    /// availability follows the same table — a served model passes, an unserved
    /// one is refused with a message that names the configured models.
    #[test]
    fn routes_resolve_the_right_provider_and_availability_follows_them() {
        let dir = tempfile::tempdir().unwrap();
        let default_provider = AlwaysAnswers::arc("d");
        let provider_a = AlwaysAnswers::arc("a");
        let setup = Arc::new(SessionSetup::for_test(
            default_provider.clone(),
            "default-model",
            dir.path().to_path_buf(),
        ));
        let mut routes = HashMap::new();
        routes.insert("model-a".to_string(), provider_a.clone());
        let factory = HostedWorkerFactory::new(setup).with_routes(routes);

        assert!(
            Arc::ptr_eq(&factory.provider_for("model-a"), &provider_a),
            "a routed model resolves to its own provider"
        );
        assert!(
            Arc::ptr_eq(&factory.provider_for("default-model"), &default_provider),
            "the default model resolves to the default provider"
        );

        assert!(factory.ensure_model_available("model-a").is_ok());
        assert!(factory.ensure_model_available("default-model").is_ok());
        let err = factory.ensure_model_available("no-such-model").unwrap_err();
        assert!(err.contains("model-a"), "{err}");
        assert!(err.contains("default-model"), "{err}");
    }

    /// 04.3 (with the 04.1 refusal folded in): two workers run on two models
    /// served by two different providers, each built on the model its node
    /// asked for; a third node names a model no provider serves and is refused
    /// before any build rather than silently running on the default.
    #[tokio::test]
    async fn two_providers_serve_two_models_and_an_unserved_model_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let setup = Arc::new(SessionSetup::for_test(
            AlwaysAnswers::arc("default answer"),
            "default-model",
            dir.path().to_path_buf(),
        ));
        let mut routes = HashMap::new();
        routes.insert("model-a".to_string(), AlwaysAnswers::arc("answer from A"));
        routes.insert("model-b".to_string(), AlwaysAnswers::arc("answer from B"));

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let factory = Arc::new(RecordingFactory {
            inner: HostedWorkerFactory::new(setup.clone()).with_routes(routes),
            seen: seen.clone(),
        });
        let swarm = swarm_id_for_dir(setup.cwd());
        let sessions = SessionRegistry::new();
        let host = SwarmHost::new(sessions.clone(), SwarmRegistry::new(), factory);
        let coordinator = adopt_coordinator(&host, &sessions, &setup, &swarm)
            .await
            .unwrap();

        let plan = PlanFile {
            objective: "two models, two providers".to_string(),
            mode: PlanMode::Light,
            nodes: vec![
                NodeSpec::task("a", "run on A").on_model("model-a"),
                NodeSpec::task("b", "run on B").on_model("model-b"),
                NodeSpec::task("ghost", "run on nothing").on_model("unserved-model"),
            ],
        };
        let report = drive(&host, &swarm, coordinator, plan, DriverConfig::default())
            .await
            .unwrap();

        assert!(report.settled, "{report:?}");
        assert_eq!(
            report.completed, 2,
            "the two served nodes completed on their own models: {report:?}"
        );
        assert!(
            report.failed >= 1,
            "the unserved-model node failed loudly rather than running on the default: {report:?}"
        );

        let built: HashMap<String, (Option<String>, String)> = seen
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .map(|(name, requested, model)| (name, (requested, model)))
            .collect();
        assert_eq!(built["a"].0.as_deref(), Some("model-a"));
        assert_eq!(built["a"].1, "model-a", "worker a built on model-a");
        assert_eq!(built["b"].0.as_deref(), Some("model-b"));
        assert_eq!(built["b"].1, "model-b", "worker b built on model-b");
        assert!(
            !built.contains_key("ghost"),
            "the unserved model was refused before any build was paid for"
        );
    }

    /// 05.1: the concurrency bound is wired through and honoured — a low bound
    /// serialises the fan-out (never more than one worker, and so one model, in
    /// flight at once) yet the plan still completes. The active budget is the
    /// concurrency plus one for the coordinator's own slot; `max_active: 2` here
    /// is that "1 worker + coordinator", and a worker would starve without it.
    #[tokio::test]
    async fn a_low_concurrency_bound_serialises_the_fan_out() {
        let (_dir, setup) = setup_on("default-model");
        let swarm = swarm_id_for_dir(setup.cwd());
        let factory = Arc::new(HostedWorkerFactory::new(setup.clone()));
        let sessions = SessionRegistry::new();
        let host = SwarmHost::new(
            sessions.clone(),
            SwarmRegistry::with_limits(SwarmLimits {
                max_active: 2,
                max_members: 32,
            }),
            factory,
        );
        let coordinator = adopt_coordinator(&host, &sessions, &setup, &swarm)
            .await
            .unwrap();

        let plan = PlanFile {
            objective: "wide but bounded".to_string(),
            mode: PlanMode::Light,
            nodes: (0..4)
                .map(|i| NodeSpec::task(format!("t{i}"), "independent work"))
                .collect(),
        };
        let report = drive(
            &host,
            &swarm,
            coordinator,
            plan,
            DriverConfig {
                concurrency: 1,
                ..DriverConfig::default()
            },
        )
        .await
        .unwrap();

        assert!(report.settled, "{report:?}");
        assert_eq!(
            report.completed, 4,
            "every task still completes under the bound: {report:?}"
        );
        assert_eq!(
            report.peak_in_flight, 1,
            "the concurrency bound held: never more than one worker at once"
        );
    }
}
