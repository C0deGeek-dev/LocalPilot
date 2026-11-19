//! Wiring test for the session's web gateway: the session hands its gateway to
//! web tools with its own approver and its current privacy state, and a session
//! change (a new session, entering or leaving incognito) clears the consent the
//! gateway holds.
#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::{
    ToolRegistry, WebContext, WebFetchRequest, WebGateway, WebPage, WebRefusal,
};
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Observing {
    incognito_seen: Mutex<Vec<bool>>,
    resets: AtomicUsize,
}

#[async_trait]
impl WebGateway for Observing {
    async fn fetch(
        &self,
        _request: WebFetchRequest,
        context: WebContext<'_>,
    ) -> Result<WebPage, WebRefusal> {
        self.incognito_seen.lock().unwrap().push(context.incognito);
        Err(WebRefusal::Declined("not in this test".to_string()))
    }
    async fn search(
        &self,
        _request: localpilot_tools::WebSearchRequest,
        _context: WebContext<'_>,
    ) -> Result<localpilot_tools::WebSearchResults, WebRefusal> {
        Err(WebRefusal::NoSearchBackend("not in this test".to_string()))
    }
    fn search_ready(&self, _incognito: bool) -> bool {
        false
    }
    fn reset_consent(&self) {
        self.resets.fetch_add(1, Ordering::SeqCst);
    }
}

fn provider() -> FakeProvider {
    FakeProvider::new()
        .tool_call("c1", "fetch", json!({ "url": "https://example.test/" }))
        .text("done")
        .tool_call("c2", "fetch", json!({ "url": "https://example.test/" }))
        .text("done")
}

#[test]
fn the_gateway_sees_live_incognito_and_session_changes_clear_consent() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Arc::new(Observing::default());
    let mut runtime = SessionRuntime::new(
        Arc::new(provider()),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            interactivity: Interactivity::Interactive,
            trusted: true,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    runtime.set_web_gateway(gateway.clone());
    let (events, _rx) = broadcast::channel(256);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();

    rt.block_on(async { runtime.run_turn("look it up", &events, &cancel).await });
    assert!(runtime.enter_incognito());
    assert_eq!(gateway.resets.load(Ordering::SeqCst), 1);
    rt.block_on(async { runtime.run_turn("again", &events, &cancel).await });
    assert!(runtime.exit_incognito());
    assert_eq!(gateway.resets.load(Ordering::SeqCst), 2);

    assert_eq!(*gateway.incognito_seen.lock().unwrap(), vec![false, true]);
}
