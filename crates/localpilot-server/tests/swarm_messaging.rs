//! Agent-to-agent messaging over real sessions: who a message reaches, who is
//! allowed to reach them, and what each delivery mode actually does.
//!
//! The tool's own tests cover parsing. These cover routing, which is where the
//! interesting mistakes live — a broadcast that escapes its subtree, or a
//! message reported as delivered that never reached a queue.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use localpilot_core::SessionId;
use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_server::registry::{RegistryError, SessionFactory, SessionRegistry};
use localpilot_server::swarm::messaging::SessionPeers;
use localpilot_server::swarm::registry::{MemberStatus, SwarmLimits, SwarmRegistry};
use localpilot_server::swarm::scope::SwarmId;
use localpilot_server::swarm::spawn::{SpawnRequest, Spawned, SwarmHost, WorkerFactory};
use localpilot_store::Store;
use localpilot_tools::{Audience, Delivery, PeerMessage, SwarmPeers};
use tempfile::TempDir;

struct Sessions {
    dir: Arc<TempDir>,
}

impl Sessions {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            dir: Arc::new(tempfile::tempdir().unwrap()),
        })
    }

    fn build(&self) -> Result<SessionRuntime, String> {
        Ok(self.build_with(FakeProvider::new().text("acknowledged")))
    }

    /// A session whose model follows `provider`'s script, for the one test that
    /// needs the model to actually call the tool.
    fn build_with(&self, provider: FakeProvider) -> SessionRuntime {
        let root = self.dir.path();
        let workspace = Workspace::new(root).expect("the temp dir is a valid workspace");
        SessionRuntime::new(
            Arc::new(provider),
            localpilot_tools::ToolRegistry::with_builtins(),
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
        )
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

fn swarm() -> SwarmId {
    SwarmId::new("messaging-swarm")
}

/// A coordinator with `lead` → {`alpha` → `alpha-child`, `beta`}.
async fn tree() -> (SwarmHost, SessionId, SessionId, SessionId, SessionId) {
    let sessions = Sessions::new();
    let registry = SessionRegistry::new();
    let lead = registry
        .open_new(&*(Arc::clone(&sessions) as Arc<dyn SessionFactory>))
        .await
        .unwrap();
    let host = SwarmHost::new(
        registry,
        SwarmRegistry::with_limits(SwarmLimits {
            max_members: 16,
            max_active: 16,
        }),
        Arc::clone(&sessions) as Arc<dyn WorkerFactory>,
    );
    host.adopt_root(&swarm(), lead, "lead").await.unwrap();

    let alpha = spawn(&host, lead, "alpha").await;
    let beta = spawn(&host, lead, "beta").await;
    let alpha_child = spawn(&host, alpha, "alpha-child").await;
    (host, lead, alpha, beta, alpha_child)
}

async fn spawn(host: &SwarmHost, parent: SessionId, name: &str) -> SessionId {
    let request = SpawnRequest::new(swarm(), parent, name, "work");
    match host.spawn(&request).await.unwrap() {
        Spawned::Started { session } => session,
        other => panic!("expected a fresh spawn, got {other:?}"),
    }
}

fn peers(host: &SwarmHost, me: SessionId) -> SessionPeers {
    SessionPeers::new(host.clone(), swarm(), me)
}

fn note(audience: Audience, body: &str) -> PeerMessage {
    PeerMessage {
        audience,
        tldr: None,
        body: body.to_string(),
        delivery: Delivery::Notify,
    }
}

/// Whether a session has anything waiting on its soft-interrupt queue.
async fn has_pending(host: &SwarmHost, session: SessionId) -> bool {
    !host
        .sessions()
        .get(session)
        .await
        .unwrap()
        .lock()
        .await
        .steer_queue()
        .is_empty()
}

#[tokio::test]
async fn a_message_to_a_name_reaches_that_peer_and_nobody_else() {
    let (host, lead, alpha, beta, _child) = tree().await;

    let delivered = peers(&host, lead)
        .send(&note(Audience::One("alpha".into()), "start with parse.rs"))
        .await
        .unwrap();

    assert_eq!(delivered.reached, 1);
    assert_eq!(delivered.recipients, vec!["alpha".to_string()]);
    assert!(has_pending(&host, alpha).await);
    assert!(!has_pending(&host, beta).await);
}

#[tokio::test]
async fn a_message_to_a_session_id_reaches_the_same_peer() {
    let (host, lead, alpha, _beta, _child) = tree().await;

    let delivered = peers(&host, lead)
        .send(&note(Audience::One(alpha.to_string()), "by id"))
        .await
        .unwrap();

    assert_eq!(delivered.recipients, vec!["alpha".to_string()]);
    assert!(has_pending(&host, alpha).await);
}

#[tokio::test]
async fn an_unknown_or_ambiguous_name_is_refused_rather_than_guessed() {
    let (host, lead, _alpha, _beta, _child) = tree().await;

    let unknown = peers(&host, lead)
        .send(&note(Audience::One("nobody".into()), "hello"))
        .await
        .unwrap_err();
    assert!(unknown.contains("nobody"), "{unknown}");

    // A second `alpha` makes the name useless, and sending to an arbitrary one
    // would be worse than not sending.
    spawn(&host, lead, "alpha").await;
    let ambiguous = peers(&host, lead)
        .send(&note(Audience::One("alpha".into()), "hello"))
        .await
        .unwrap_err();
    assert!(ambiguous.contains("session id"), "{ambiguous}");
}

#[tokio::test]
async fn messaging_yourself_is_refused() {
    let (host, lead, _alpha, _beta, _child) = tree().await;
    let error = peers(&host, lead)
        .send(&note(Audience::One("lead".into()), "note to self"))
        .await
        .unwrap_err();
    assert!(error.contains("that is you"), "{error}");
}

#[tokio::test]
async fn a_broadcast_stops_at_the_edge_of_the_senders_own_subtree() {
    let (host, lead, alpha, beta, child) = tree().await;

    let delivered = peers(&host, alpha)
        .send(&note(Audience::Subtree, "I am editing parse.rs"))
        .await
        .unwrap();

    assert_eq!(delivered.recipients, vec!["alpha-child".to_string()]);
    assert!(has_pending(&host, child).await);
    assert!(!has_pending(&host, beta).await, "beta is not alpha's");
    assert!(!has_pending(&host, lead).await, "nor is the coordinator");
    assert!(
        !has_pending(&host, alpha).await,
        "and not the sender itself"
    );
}

#[tokio::test]
async fn only_the_coordinator_may_address_the_whole_swarm() {
    let (host, lead, alpha, beta, child) = tree().await;

    let refused = peers(&host, alpha)
        .send(&note(Audience::Swarm, "everybody stop"))
        .await
        .unwrap_err();
    assert!(refused.contains("only the coordinator"), "{refused}");
    assert!(
        !has_pending(&host, beta).await,
        "the refusal reached nobody"
    );

    let delivered = peers(&host, lead)
        .send(&note(Audience::Swarm, "everybody stop"))
        .await
        .unwrap();
    let mut names = delivered.recipients.clone();
    names.sort();
    assert_eq!(names, vec!["alpha", "alpha-child", "beta"]);
    for member in [alpha, beta, child] {
        assert!(has_pending(&host, member).await);
    }
    assert!(!has_pending(&host, lead).await, "the sender is excluded");
}

#[tokio::test]
async fn a_member_that_has_finished_is_not_counted_as_reached() {
    let (host, lead, alpha, beta, _child) = tree().await;
    host.swarms()
        .set_status(&swarm(), beta, MemberStatus::Finished)
        .await
        .unwrap();

    let delivered = peers(&host, lead)
        .send(&note(Audience::Swarm, "still going?"))
        .await
        .unwrap();

    assert!(
        !delivered.recipients.contains(&"beta".to_string()),
        "a finished member has no turn to reach, so reporting it as reached would be a lie: {:?}",
        delivered.recipients
    );
    assert!(has_pending(&host, alpha).await);
    assert!(!has_pending(&host, beta).await);
}

#[tokio::test]
async fn waking_an_idle_peer_starts_a_turn_rather_than_queueing_forever() {
    let (host, lead, alpha, _beta, _child) = tree().await;
    let mut message = note(Audience::One("alpha".into()), "you are needed now");
    message.delivery = Delivery::Wake;

    peers(&host, lead).send(&message).await.unwrap();

    // The turn is started on a spawned task, so the sender is not blocked for
    // as long as the recipient takes. Wait for it to have run.
    let alpha_host = host.host(alpha).await.unwrap();
    for _ in 0..200 {
        if !alpha_host.is_busy() {
            let ran = host
                .sessions()
                .get(alpha)
                .await
                .unwrap()
                .lock()
                .await
                .last_assistant_text()
                .is_some();
            if ran {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("waking an idle peer should have driven a turn");
}

#[tokio::test]
async fn a_swarm_member_can_actually_call_the_messaging_tool() {
    // The whole point of binding a peer view to each session: without it the
    // tool is registered, callable, and permanently useless. This drives a real
    // turn in which the model calls `swarm` and reads the answer.
    let sessions = Sessions::new();
    let registry = SessionRegistry::new();
    let lead = registry
        .open_new(&*(Arc::clone(&sessions) as Arc<dyn SessionFactory>))
        .await
        .unwrap();
    let host = SwarmHost::new(
        registry,
        SwarmRegistry::new(),
        Arc::clone(&sessions) as Arc<dyn WorkerFactory>,
    );
    let lead_host = host.adopt_root(&swarm(), lead, "lead").await.unwrap();
    let alpha = spawn(&host, lead, "alpha").await;

    // A session whose scripted model calls the tool and then answers.
    let runtime = sessions.build_with(
        FakeProvider::new()
            .tool_call("call-1", "swarm", serde_json::json!({ "action": "roster" }))
            .text("I know who is here now"),
    );
    let caller = runtime.session_id();
    host.sessions().register(runtime).await.unwrap();
    host.adopt_root(&swarm(), caller, "caller").await.unwrap();
    let caller_host = host.host(caller).await.unwrap();

    caller_host.drive("who else is working on this?").await;

    let transcript = host
        .sessions()
        .get(caller)
        .await
        .unwrap()
        .lock()
        .await
        .last_assistant_text()
        .unwrap_or_default();
    assert_eq!(transcript, "I know who is here now");
    let _ = (lead_host, alpha);
}

#[tokio::test]
async fn the_roster_reports_the_tree_from_the_askers_point_of_view() {
    let (host, lead, alpha, _beta, child) = tree().await;

    let from_alpha = peers(&host, alpha).roster().await;
    let mine: Vec<&str> = from_alpha
        .iter()
        .filter(|p| p.in_my_subtree)
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(mine, vec!["alpha-child"]);
    assert!(
        from_alpha.iter().all(|p| p.session != alpha.to_string()),
        "the asker is not in its own roster"
    );

    let identity = peers(&host, lead).identity().await;
    assert!(identity.is_coordinator);
    assert_eq!(identity.name, "lead");
    assert!(!peers(&host, child).identity().await.is_coordinator);
}
