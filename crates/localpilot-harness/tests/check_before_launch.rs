//! Wiring test for the look-before-launch discipline: when the task prompt names
//! a local serveable target that has not been probed, an attempt to scaffold a
//! competing entry page surfaces the rule's verdict; probing the target first
//! clears it; a task that names no such target is never flagged.
//!
//! The "create your own" action under test is a `write_file` of `index.html` —
//! harmless and deterministic, so the test never stands up (or hangs on) a real
//! server. A real loopback listener provides a genuinely successful probe.
#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;

use indexmap::IndexMap;
use localpilot_config::RuleSeverity;
use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Run one scripted turn and return every warning the runtime emitted.
fn run_turn_collecting_warnings(
    root: &Path,
    prompt: &str,
    provider: FakeProvider,
    rules: IndexMap<String, RuleSeverity>,
) -> Vec<String> {
    let mut runtime = SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(root),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            rules,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let (events, mut rx) = broadcast::channel(256);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async { runtime.run_turn(prompt, &events, &cancel).await });
    drop(events);

    let mut warnings = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let RuntimeEvent::Warning(message) = event {
            warnings.push(message);
        }
    }
    warnings
}

/// A one-shot loopback HTTP responder on a free port, so a `fetch` against it
/// records a genuinely successful probe. Returns the bound port.
fn spawn_oneshot_http_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
    });
    port
}

fn flagged(warnings: &[String]) -> bool {
    warnings.iter().any(|w| w.contains("probe it first"))
}

#[test]
fn an_unprobed_launch_against_a_named_target_is_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // The prompt names a local target; the model scaffolds its own page without
    // probing it first.
    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "write_file",
            json!({ "path": "index.html", "content": "<html>mine</html>" }),
        )
        .text("served my own page");
    let warnings = run_turn_collecting_warnings(
        root,
        "The site is already running at http://localhost:8080 — show me its home page",
        provider,
        IndexMap::new(),
    );

    assert!(
        flagged(&warnings),
        "an unprobed scaffold against a named target must surface the nudge; got {warnings:?}"
    );
    // Default severity is Warn: the call still ran, so the page was written.
    assert!(root.join("index.html").exists());
}

#[test]
fn probing_the_named_target_first_clears_the_nudge() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let port = spawn_oneshot_http_server();

    // The model probes the named target (a real successful fetch) before writing
    // its own page, demonstrating the discipline — so no nudge fires.
    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "fetch",
            json!({ "url": format!("http://127.0.0.1:{port}/") }),
        )
        .tool_call(
            "c2",
            "write_file",
            json!({ "path": "index.html", "content": "<html>mine</html>" }),
        )
        .text("checked it, then built my own");
    let warnings = run_turn_collecting_warnings(
        root,
        &format!("Check the service at http://127.0.0.1:{port}/ then build a landing page"),
        provider,
        IndexMap::new(),
    );

    assert!(
        !flagged(&warnings),
        "a probed target must not trigger the nudge; got {warnings:?}"
    );
}

#[test]
fn a_launch_with_no_named_target_is_not_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "write_file",
            json!({ "path": "index.html", "content": "<html>mine</html>" }),
        )
        .text("scaffolded a new site");
    let warnings = run_turn_collecting_warnings(
        root,
        "Build me a brand-new landing page from scratch",
        provider,
        IndexMap::new(),
    );

    assert!(
        !flagged(&warnings),
        "a task that names no local target must never be flagged; got {warnings:?}"
    );
}

#[test]
fn a_blocking_severity_refuses_the_unprobed_launch() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let mut rules = IndexMap::new();
    rules.insert("check_before_launch".to_string(), RuleSeverity::Block);

    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "write_file",
            json!({ "path": "index.html", "content": "<html>mine</html>" }),
        )
        .text("tried to serve my own page");
    let warnings = run_turn_collecting_warnings(
        root,
        "The app is up at http://localhost:8080 — open it",
        provider,
        rules,
    );

    assert!(
        flagged(&warnings),
        "a Block severity must still surface the reason"
    );
    // Block refuses before the call runs: the competing page is never written.
    assert!(
        !root.join("index.html").exists(),
        "a blocked scaffold must not write the file"
    );
}
