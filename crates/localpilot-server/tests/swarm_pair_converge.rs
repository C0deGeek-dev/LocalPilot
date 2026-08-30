//! The convergence driver over a real adopted pair: [`AdoptedPair`] as
//! [`PairEndpoints`], driven end to end by [`PairDriver`], plus the endpoint
//! adapter's behaviour on the boundaries the deterministic fakes exist to pin —
//! exact current-turn envelope (never stale), token cost, cancellation, and the
//! error mapping.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use localpilot_core::{ContentBlock, SessionId, TokenUsage};
use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::{
    FakeProvider, ModelEvent, ModelEventStream, ModelProvider, ModelRequest, ProviderDeclaration,
    ProviderError,
};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_server::registry::{RegistryError, SessionFactory, SessionRegistry};
use localpilot_server::swarm::converge::{
    EndpointError, NotifyReply, PairBounds, PairDriver, PairEndpoints, PairOutcome, TurnReply,
};
use localpilot_server::swarm::registry::SwarmRegistry;
use localpilot_server::swarm::scope::SwarmId;
use localpilot_server::swarm::spawn::{AdoptedPair, SpawnRequest, SwarmHost, WorkerFactory};
use localpilot_store::Store;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

fn swarm() -> SwarmId {
    SwarmId::new("pair-swarm")
}

/// The lower-case hex SHA-256 of an artifact's canonical bytes — the digest the
/// driver holds for a candidate, recomputed here so a scripted `agree` can name
/// it. Mirrors the converge module's own canonicalisation (line endings only).
fn digest(artifact: &str) -> String {
    use std::fmt::Write as _;
    let canonical = artifact.replace("\r\n", "\n").replace('\r', "\n");
    let mut hex = String::new();
    for byte in Sha256::digest(canonical.as_bytes()) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Whether a request carried a text block containing `needle`.
fn request_contains(request: &ModelRequest, needle: &str) -> bool {
    request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if text.contains(needle)))
    })
}

/// Whether any request a provider received contained `needle` — used to prove a
/// delivery reached a peer before its turn ran.
fn any_request_contains(provider: &FakeProvider, needle: &str) -> bool {
    provider
        .requests()
        .iter()
        .any(|r| request_contains(r, needle))
}

/// Whether the provider's `n`-th request contained `needle`.
fn nth_request_contains(provider: &FakeProvider, n: usize, needle: &str) -> bool {
    provider
        .requests()
        .get(n)
        .is_some_and(|r| request_contains(r, needle))
}

/// The distinct models a provider was driven with across all its requests.
fn models_of(provider: &FakeProvider) -> Vec<String> {
    let mut models: Vec<String> = provider.requests().into_iter().map(|r| r.model).collect();
    models.sort();
    models.dedup();
    models
}

const DEFAULT_MODEL: &str = "test-model";

/// A shared workspace and a queue of `(provider, model)` build inputs. Scripted
/// [`FakeProvider`]s are also retained concretely so a test can read the requests
/// they received, including the exact model each was driven with.
struct Sessions {
    dir: Arc<TempDir>,
    queued: Mutex<VecDeque<(Arc<dyn ModelProvider>, String)>>,
    fakes: Mutex<Vec<Arc<FakeProvider>>>,
}

impl Sessions {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            dir: Arc::new(tempfile::tempdir().unwrap()),
            queued: Mutex::new(VecDeque::new()),
            fakes: Mutex::new(Vec::new()),
        })
    }

    fn queue(&self, provider: FakeProvider) {
        self.queue_as(provider, DEFAULT_MODEL);
    }

    /// Queue a scripted fake built with an explicit model, so a test can prove a
    /// peer was driven with its own configured model.
    fn queue_as(&self, provider: FakeProvider, model: &str) {
        let provider = Arc::new(provider);
        self.fakes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::clone(&provider));
        self.queued
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back((provider, model.to_string()));
    }

    fn queue_dyn(&self, provider: Arc<dyn ModelProvider>) {
        self.queued
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back((provider, DEFAULT_MODEL.to_string()));
    }

    /// The `index`-th scripted fake, in the order fakes were queued.
    fn fake(&self, index: usize) -> Arc<FakeProvider> {
        Arc::clone(&self.fakes.lock().unwrap_or_else(|e| e.into_inner())[index])
    }

    fn build(&self) -> Result<SessionRuntime, String> {
        let (provider, model) = self
            .queued
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or_else(|| {
                (
                    Arc::new(FakeProvider::new()) as Arc<dyn ModelProvider>,
                    DEFAULT_MODEL.to_string(),
                )
            });
        let root = self.dir.path();
        let workspace = Workspace::new(root).map_err(|err| err.to_string())?;
        Ok(SessionRuntime::new(
            provider,
            localpilot_tools::ToolRegistry::with_builtins(),
            PermissionEngine::new(Profile::Bypass, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::open(root),
            workspace,
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig {
                model,
                interactivity: Interactivity::NonInteractive,
                trusted: true,
                ..SessionConfig::default()
            },
            Vec::new(),
        ))
    }
}

impl WorkerFactory for Sessions {
    fn create(&self, _request: &SpawnRequest) -> Result<SessionRuntime, String> {
        self.build()
    }
}

impl SessionFactory for Sessions {
    fn create(&self) -> Result<SessionRuntime, RegistryError> {
        self.build().map_err(RegistryError::Factory)
    }
}

/// Register two ordinary sessions and adopt them as a symmetric pair — the exact
/// substrate the endpoint adapter runs over, with no coordinator or spawn.
async fn adopt(
    sessions: &Arc<Sessions>,
    alice: FakeProvider,
    bob: FakeProvider,
) -> (SwarmHost, AdoptedPair) {
    let registry = SessionRegistry::new();
    let factory = Arc::clone(sessions) as Arc<dyn SessionFactory>;
    sessions.queue(alice);
    let a = registry.open_new(&*factory).await.unwrap();
    sessions.queue(bob);
    let b = registry.open_new(&*factory).await.unwrap();
    let host = SwarmHost::new(
        registry,
        SwarmRegistry::new(),
        Arc::clone(sessions) as Arc<dyn WorkerFactory>,
    );
    let pair = host
        .adopt_pair(&swarm(), (a, "alice"), (b, "bob"))
        .await
        .unwrap();
    (host, pair)
}

fn propose_of(artifact: &str) -> String {
    format!(r#"{{"v":1,"action":"propose","artifact":"{artifact}"}}"#)
}

fn agree_of(artifact: &str) -> String {
    format!(
        r#"{{"v":1,"action":"agree","revision":1,"digest":"{}"}}"#,
        digest(artifact)
    )
}

#[tokio::test]
async fn a_real_adopted_pair_converges_through_the_driver() {
    let sessions = Sessions::new();
    let artifact = "final report: the answer is 42";
    let task = "produce the final report";
    let propose = propose_of(artifact);
    let agree = agree_of(artifact);

    // Two ordinary sessions with DISTINCT configured models, adopted as a pair.
    // A proposes then agrees (two turns); B agrees (one turn): A propose ->
    // deliver+drive B agree -> deliver+drive A agree -> converged.
    let registry = SessionRegistry::new();
    let factory = Arc::clone(&sessions) as Arc<dyn SessionFactory>;
    sessions.queue_as(
        FakeProvider::new().text(&propose).text(&agree),
        "alpha-model",
    );
    let a = registry.open_new(&*factory).await.unwrap();
    sessions.queue_as(FakeProvider::new().text(&agree), "beta-model");
    let b = registry.open_new(&*factory).await.unwrap();
    let host = SwarmHost::new(
        registry,
        SwarmRegistry::new(),
        Arc::clone(&sessions) as Arc<dyn WorkerFactory>,
    );
    let pair = host
        .adopt_pair(&swarm(), (a, "alice"), (b, "bob"))
        .await
        .unwrap();
    assert_eq!(pair.sessions(), [a, b]);

    let bounds = PairBounds {
        max_rounds: 4,
        slot_timeout: Duration::from_secs(5),
        // Each slot drives exactly one scripted turn, which reports usage total
        // = 2; the budget equals that so the cost mapping is exercised, not
        // bypassed.
        slot_token_budget: 2,
    };
    let driver = PairDriver::new(a, b, task, bounds).unwrap();
    let mut endpoints = pair;
    let report = driver.run(&mut endpoints).await;

    assert_eq!(report.reason(), &PairOutcome::Converged { revision: 1 });
    assert_eq!(sessions.fake(0).requests().len(), 2, "A is driven twice");
    assert_eq!(sessions.fake(1).requests().len(), 1, "B is driven once");

    // Distinct configured models drove distinct sessions.
    assert_eq!(
        models_of(&sessions.fake(0)),
        vec!["alpha-model".to_string()]
    );
    assert_eq!(models_of(&sessions.fake(1)), vec!["beta-model".to_string()]);

    // The whole three-turn sequence over the real transport: A's first turn
    // carried the exact original task (the first-proposer handoff); B's turn saw
    // A's proposal delivered as alice; A's second turn saw B's agreement
    // delivered as bob (both bound sender identities, both notify-before-drive
    // transitions).
    assert!(
        nth_request_contains(&sessions.fake(0), 0, task),
        "A's first turn carries the original task"
    );
    // The `[system]` prefix is required, not just the sender name: it pins the
    // delivery to `SoftInterruptSource::System` end to end, excluding a
    // regression that injected a peer message as ordinary user steering.
    assert!(
        any_request_contains(&sessions.fake(1), "[system] Message from alice"),
        "B saw A's proposal delivered as a system peer message before its turn"
    );
    assert!(
        any_request_contains(&sessions.fake(0), "[system] Message from bob"),
        "A saw B's agreement delivered as a system peer message before its second turn"
    );

    // Retained terminal state: each peer's latest raw is its agree; the candidate
    // is A's proposal at revision 1.
    assert_eq!(report.raw_for(a), Some(agree.as_str()));
    assert_eq!(report.raw_for(b), Some(agree.as_str()));
    let candidate = report.candidate().unwrap();
    assert_eq!(candidate.revision(), 1);
    assert_eq!(candidate.artifact(), artifact);
    assert_eq!(candidate.digest(), digest(artifact));
}

#[tokio::test]
async fn an_envelope_is_the_final_message_its_cost_accumulates_and_a_textless_turn_resets_it() {
    let sessions = Sessions::new();
    let (host, mut pair) = adopt(
        &sessions,
        FakeProvider::new()
            // Turn 1: one iteration, a first answer (usage total 2).
            .text("an earlier answer")
            // Turn 2, iteration 1: intermediate text plus a tool call (usage 2)
            // — the loop continues after the tool result.
            .script(vec![
                Ok(ModelEvent::TextDelta("intermediate note ".to_string())),
                Ok(ModelEvent::ToolCall {
                    id: "t1".to_string(),
                    name: "read_file".to_string(),
                    input_json: serde_json::json!({ "path": "does-not-exist.txt" }),
                    provider_metadata: None,
                }),
                Ok(ModelEvent::Usage(TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..TokenUsage::default()
                })),
                Ok(ModelEvent::Done),
            ])
            // Turn 2, iteration 2: the final answer (usage 2).
            .text("a fresh answer")
            // Turn 3: no assistant text at all.
            .text(""),
        FakeProvider::new(),
    )
    .await;
    let [a, _b] = pair.sessions();
    let token = CancellationToken::new();

    let first = PairEndpoints::drive(&mut pair, a, "one", &token)
        .await
        .unwrap();
    assert_eq!(
        first,
        TurnReply::Produced {
            envelope: "an earlier answer".to_string(),
            cost: 2,
        }
    );

    // A two-iteration turn: the envelope is the FINAL assistant message only —
    // not "intermediate note a fresh answer" concatenated across iterations,
    // which is exactly why the event stream was rejected as the source — and the
    // cost is the SUM of both iterations' usage (2 + 2), proving per-turn
    // accumulation.
    let second = PairEndpoints::drive(&mut pair, a, "two", &token)
        .await
        .unwrap();
    assert_eq!(
        second,
        TurnReply::Produced {
            envelope: "a fresh answer".to_string(),
            cost: 4,
        }
    );

    // A turn that produces no assistant text is a failure, never the prior turn's
    // text re-served as an envelope.
    let third = PairEndpoints::drive(&mut pair, a, "three", &token)
        .await
        .unwrap_err();
    assert!(
        matches!(third, EndpointError::ProviderError(_)),
        "{third:?}"
    );

    // And directly: the textless turn reset the per-turn capture to `None`, while
    // history still holds the prior turn's answer — proving the reset is
    // turn-scoped, not a read of the most recent message anywhere in history.
    let handle = host.sessions().get(a).await.unwrap();
    let runtime = handle.lock().await;
    assert_eq!(
        runtime.current_turn_assistant_text(),
        None,
        "the textless turn reset the per-turn assistant-text capture"
    );
    assert_eq!(
        runtime.last_assistant_text().as_deref(),
        Some("a fresh answer"),
        "history still holds the prior turn's answer"
    );
}

#[tokio::test]
async fn a_precancelled_drive_runs_no_turn() {
    let sessions = Sessions::new();
    let (_host, mut pair) = adopt(
        &sessions,
        FakeProvider::new().text("should never run"),
        FakeProvider::new(),
    )
    .await;
    let [a, _b] = pair.sessions();

    let token = CancellationToken::new();
    token.cancel();
    let reply = PairEndpoints::drive(&mut pair, a, "go", &token)
        .await
        .unwrap();
    assert_eq!(reply, TurnReply::Cancelled);
    assert_eq!(
        sessions.fake(0).requests().len(),
        0,
        "a pre-cancelled drive starts no provider turn"
    );
}

#[tokio::test]
async fn a_precancelled_notify_delivers_nothing() {
    let sessions = Sessions::new();
    let (host, mut pair) = adopt(&sessions, FakeProvider::new(), FakeProvider::new()).await;
    let [a, b] = pair.sessions();

    let token = CancellationToken::new();
    token.cancel();
    let reply = PairEndpoints::notify(&mut pair, a, b, "hello", &token)
        .await
        .unwrap();
    assert_eq!(reply, NotifyReply::Cancelled);

    let queued = host.host(b).await.unwrap();
    assert!(
        host.sessions()
            .get(b)
            .await
            .unwrap()
            .lock()
            .await
            .steer_queue()
            .is_empty(),
        "a pre-cancelled notify enqueues nothing"
    );
    drop(queued);
}

#[tokio::test]
async fn unknown_peers_and_recipients_map_to_peer_failed() {
    let sessions = Sessions::new();
    let (_host, mut pair) = adopt(&sessions, FakeProvider::new(), FakeProvider::new()).await;
    let [a, _b] = pair.sessions();
    let stranger = SessionId::new();
    let token = CancellationToken::new();

    let drive_err = PairEndpoints::drive(&mut pair, stranger, "go", &token)
        .await
        .unwrap_err();
    assert!(matches!(drive_err, EndpointError::PeerFailed(_)));

    let notify_err = PairEndpoints::notify(&mut pair, a, stranger, "hi", &token)
        .await
        .unwrap_err();
    assert!(matches!(notify_err, EndpointError::PeerFailed(_)));
}

/// A provider whose turn never completes on its own, so a drive stays in flight
/// until the driver's cancellation tears it down.
struct PendingProvider {
    declaration: ProviderDeclaration,
}

impl PendingProvider {
    fn new() -> Self {
        Self {
            declaration: FakeProvider::new().declaration().clone(),
        }
    }
}

#[async_trait]
impl ModelProvider for PendingProvider {
    fn declaration(&self) -> &ProviderDeclaration {
        &self.declaration
    }

    async fn stream(&self, _request: ModelRequest) -> Result<ModelEventStream, ProviderError> {
        Ok(Box::pin(futures::stream::pending::<
            Result<ModelEvent, ProviderError>,
        >()))
    }
}

#[tokio::test]
async fn an_in_flight_drive_is_cancelled_and_the_host_is_freed() {
    let sessions = Sessions::new();
    let registry = SessionRegistry::new();
    let factory = Arc::clone(&sessions) as Arc<dyn SessionFactory>;
    // A's turn blocks forever; B is never driven.
    sessions.queue_dyn(Arc::new(PendingProvider::new()));
    let a = registry.open_new(&*factory).await.unwrap();
    sessions.queue(FakeProvider::new());
    let b = registry.open_new(&*factory).await.unwrap();
    let host = SwarmHost::new(
        registry,
        SwarmRegistry::new(),
        Arc::clone(&sessions) as Arc<dyn WorkerFactory>,
    );
    let mut pair = host
        .adopt_pair(&swarm(), (a, "alice"), (b, "bob"))
        .await
        .unwrap();

    let host_a = pair.host(a).unwrap();
    let token = CancellationToken::new();
    let drive = PairEndpoints::drive(&mut pair, a, "go", &token);
    tokio::pin!(drive);
    // Let the turn reach flight, then cancel it — deterministically, off the
    // host's own busy flag rather than a timer.
    let reply = loop {
        tokio::select! {
            result = &mut drive => break result,
            () = async {
                while !host_a.is_busy() {
                    tokio::task::yield_now().await;
                }
                token.cancel();
                std::future::pending::<()>().await
            } => {}
        }
    };

    assert_eq!(
        reply.unwrap(),
        TurnReply::Cancelled,
        "a cancelled in-flight drive reports a clean cancellation"
    );
    assert!(
        !host_a.is_busy(),
        "the cancel was delivered to the running turn and awaited, freeing the host"
    );
}

#[tokio::test]
async fn aborting_a_run_over_a_blocked_proposer_ends_aborted_and_frees_the_host() {
    let sessions = Sessions::new();
    let registry = SessionRegistry::new();
    let factory = Arc::clone(&sessions) as Arc<dyn SessionFactory>;
    // A's first turn (the proposal) blocks forever; B is never reached.
    sessions.queue_dyn(Arc::new(PendingProvider::new()));
    let a = registry.open_new(&*factory).await.unwrap();
    sessions.queue(FakeProvider::new());
    let b = registry.open_new(&*factory).await.unwrap();
    let host = SwarmHost::new(
        registry,
        SwarmRegistry::new(),
        Arc::clone(&sessions) as Arc<dyn WorkerFactory>,
    );
    let pair = host
        .adopt_pair(&swarm(), (a, "alice"), (b, "bob"))
        .await
        .unwrap();
    let host_a = pair.host(a).unwrap();

    let bounds = PairBounds {
        max_rounds: 4,
        // Generous, so the abort — not the slot deadline — is what stops the run.
        slot_timeout: Duration::from_secs(30),
        slot_token_budget: 0,
    };
    let driver = PairDriver::new(a, b, "produce the report", bounds).unwrap();
    let abort = driver.abort_handle();
    let mut endpoints = pair;
    let run = driver.run(&mut endpoints);
    tokio::pin!(run);
    let report = loop {
        tokio::select! {
            result = &mut run => break result,
            () = async {
                while !host_a.is_busy() {
                    tokio::task::yield_now().await;
                }
                abort.abort();
                std::future::pending::<()>().await
            } => {}
        }
    };

    assert_eq!(
        report.reason(),
        &PairOutcome::Aborted,
        "an abort during the in-flight proposal ends the run aborted"
    );
    assert!(
        !host_a.is_busy(),
        "the abort reached the real turn through the endpoint and freed the host"
    );
}
