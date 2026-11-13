//! The browser observation route, against a real browser.
//!
//! Nothing here is scripted on the tool side. A real Playwright MCP server is
//! started from an `[mcp.servers.browser]` configuration through the product's
//! own path (`McpTools::load`, `install_broker`), so the tools are registered,
//! advertised and dispatched exactly as in a session. The model side is the
//! real OpenAI-compatible adapter talking to a local recording HTTP server, so
//! what a provider is handed is read from the request bodies on the wire, not
//! from the in-memory request. A separate web server written for this test
//! records the page's own requests, so that the click and the failed asset are
//! known without trusting what the browser server reports.
//!
//! No real model is involved: "inspected" here means a provider that accepts
//! images answered the request holding the image, not that a model looked at it.
//!
//! The test needs Node, the `@playwright/mcp` package and its browser. When one
//! is missing it says so and skips; set `LOCALPILOT_REQUIRE_REAL_TESTS=1` to turn
//! a skip into a failure, which a gate that must prove this route should do.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use localpilot_harness::{
    register_task_guidance_context, EvidenceState, GuidanceCatalog, GuidanceHookMode, ImageState,
    RouteStatus, RuntimeEvent, SessionConfig, SessionRuntime,
};
use localpilot_llm::{OpenAiProvider, SourceType};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::mcp::{install_broker, McpTools};

const REQUIRE_VAR: &str = "LOCALPILOT_REQUIRE_REAL_TESTS";

/// Skip loudly, or fail when real tests are required.
fn unavailable(reason: &str) -> Option<PathBuf> {
    assert!(
        std::env::var_os(REQUIRE_VAR).is_none(),
        "{REQUIRE_VAR} is set but the real browser test cannot run: {reason}"
    );
    eprintln!("SKIPPED (real browser route): {reason}");
    None
}

/// The `@playwright/mcp` entry point beside the Node that is on `PATH`.
fn playwright_mcp() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("LOCALPILOT_PLAYWRIGHT_MCP") {
        let explicit = PathBuf::from(explicit);
        return explicit
            .is_file()
            .then_some(explicit)
            .or_else(|| unavailable("LOCALPILOT_PLAYWRIGHT_MCP does not name a file"));
    }
    let Some(node) = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .find(|dir| dir.join("node.exe").is_file() || dir.join("node").is_file())
    else {
        return unavailable("node is not on PATH");
    };
    let candidates = [
        node.join("node_modules/@playwright/mcp/cli.js"),
        node.join("../lib/node_modules/@playwright/mcp/cli.js"),
    ];
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .or_else(|| unavailable("the @playwright/mcp package is not installed globally"))
}

/// The web server the browser visits. It records every request it receives.
struct Oracle {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

const PAGE: &str = "<!doctype html><html><head><title>Oracle game</title></head>\
<body style=\"margin:0\"><h1>Score board</h1>\
<button id=\"start\" onclick=\"n++;document.getElementById('score').textContent='score '+n;\
document.body.style.background='#2d6cdf';fetch('/clicked',{method:'POST'})\">Start</button>\
<p id=\"score\">score 0</p>\
<script>var n=0;console.error('asset missing: models/ship.glb');fetch('/missing.png');</script>\
</body></html>";

impl Oracle {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let log = Arc::clone(&log);
                std::thread::spawn(move || serve(stream, &log));
            }
        });
        Self { port, seen }
    }

    fn requests(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

fn serve(mut stream: TcpStream, log: &Mutex<Vec<String>>) {
    let mut buffer = [0u8; 4096];
    let Ok(read) = stream.read(&mut buffer) else {
        return;
    };
    let head = String::from_utf8_lossy(&buffer[..read]);
    let mut first = head.lines().next().unwrap_or_default().split_whitespace();
    let (verb, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    if target == "/favicon.ico" {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    log.lock().unwrap().push(format!("{verb} {target}"));
    let (status, body) = match (verb, target) {
        ("GET", "/") => ("200 OK", PAGE),
        ("POST", "/clicked") => ("204 No Content", ""),
        _ => ("404 Not Found", "no"),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// An OpenAI-style streaming response that calls one tool.
fn tool_call(id: &str, name: &str, arguments: &Value) -> ResponseTemplate {
    let call = json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
        "index": 0, "id": id, "type": "function",
        "function": {"name": name, "arguments": arguments.to_string()}
    }]}, "finish_reason": null}]});
    let done = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]});
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(format!("data: {call}\n\ndata: {done}\n\ndata: [DONE]\n\n"))
}

fn text(content: &str) -> ResponseTemplate {
    let said =
        json!({"choices": [{"index": 0, "delta": {"content": content}, "finish_reason": null}]});
    let done = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(format!("data: {said}\n\ndata: {done}\n\ndata: [DONE]\n\n"))
}

/// Decode standard base64 (no external crate needed for a test).
fn base64(input: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::new();
    let (mut bits, mut have) = (0u32, 0);
    for c in input.bytes().filter_map(value) {
        bits = (bits << 6) | u32::from(c);
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push((bits >> have) as u8);
            bits &= (1 << have) - 1;
        }
    }
    out
}

/// The `data:image/png;base64,...` payloads in a request body, in order.
fn images_in(body: &str) -> Vec<Vec<u8>> {
    body.split("data:image/png;base64,")
        .skip(1)
        .map(|rest| {
            let end = rest.find(['"', '\\']).unwrap_or(rest.len());
            base64(&rest[..end])
        })
        .collect()
}

/// A PNG's width and height from its header, when it is one.
fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || bytes[..8] != [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] {
        return None;
    }
    let read = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    Some((read(16), read(20)))
}

/// Playwright browser processes currently running (Windows; `None` elsewhere).
fn browser_processes() -> Option<usize> {
    if !cfg!(windows) {
        return None;
    }
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -like '*ms-playwright*' }).Count",
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()
        .or(Some(0))
}

#[tokio::test]
async fn a_real_browser_is_driven_through_the_product_path_and_observed_on_the_wire() {
    let Some(server_entry) = playwright_mcp() else {
        return;
    };
    let before_processes = browser_processes();
    let oracle = Oracle::start();
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name": "g", "dependencies": {"three": "0.169.0"}, "devDependencies": {"vite": "^5.4.0"}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let output_dir = dir.path().join("browser-output");

    // The product path: an [mcp.servers.browser] entry, McpTools::load, the broker.
    let mut config = localpilot_config::Config::default();
    config.mcp.servers.insert(
        "browser".to_string(),
        localpilot_config::McpServerConfig {
            command: "node".to_string(),
            args: vec![
                server_entry.to_string_lossy().to_string(),
                "--headless".to_string(),
                "--isolated".to_string(),
                "--output-dir".to_string(),
                output_dir.to_string_lossy().to_string(),
            ],
            env: indexmap::IndexMap::new(),
        },
    );
    let mcp = McpTools::load(&config).await;
    let mut registry = mcp.registry();
    let registered: BTreeSet<String> = registry.names().iter().map(|n| (*n).to_string()).collect();
    assert!(
        registered.contains("browser_click") && registered.contains("browser_take_screenshot"),
        "the real server's tools were not registered: {registered:?}"
    );
    let broker = install_broker(&config.tools, &mut registry);

    // The model: the real OpenAI-compatible adapter against a recording server.
    let url = format!("http://127.0.0.1:{}/", oracle.port);
    let wire = MockServer::start().await;
    let script: Vec<ResponseTemplate> = vec![
        // What a model has to do first: the broker advertises only the browser
        // tools the request matched, so the rest are found and loaded.
        tool_call(
            "s0",
            "tool_search",
            &json!({"need": "browser navigate to a url"}),
        ),
        tool_call("l1", "tool_load", &json!({"name": "browser_navigate"})),
        tool_call("l2", "tool_load", &json!({"name": "browser_snapshot"})),
        tool_call(
            "l3",
            "tool_load",
            &json!({"name": "browser_console_messages"}),
        ),
        tool_call("l4", "tool_load", &json!({"name": "browser_close"})),
        tool_call("c1", "browser_navigate", &json!({"url": url})),
        tool_call("c2", "browser_snapshot", &json!({})),
        tool_call("c3", "browser_take_screenshot", &json!({})),
        // The call a model would make from habit: `ref` instead of `target`.
        tool_call(
            "c4",
            "browser_click",
            &json!({"element": "Start button", "ref": "e3"}),
        ),
        tool_call(
            "c5",
            "browser_click",
            &json!({"element": "Start button", "target": "e3"}),
        ),
        tool_call("c6", "browser_console_messages", &json!({})),
        tool_call("c7", "browser_take_screenshot", &json!({})),
        tool_call("c8", "browser_close", &json!({})),
        text("The score reads 1 after the click; the console shows the missing asset."),
    ];
    for response in script {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(response)
            .up_to_n_times(1)
            .mount(&wire)
            .await;
    }
    let provider = Arc::new(
        OpenAiProvider::new("local", "Local", SourceType::LocalServer, wire.uri(), None)
            .with_declared_vision(Some(true)),
    );

    let mut runtime = SessionRuntime::new(
        provider as Arc<dyn localpilot_llm::ModelProvider>,
        registry,
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(&dir.path().join("store")),
        Workspace::new(&project).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    );
    runtime.set_broker(broker);
    // Before anything is loaded: registered, but the model cannot call it yet.
    let before = runtime
        .observation_routes()
        .into_iter()
        .find(|r| r.capability == "browser.interact")
        .unwrap();
    eprintln!("REAL RUN browser route before loading: {before:?}");
    assert!(
        matches!(&before.status, RouteStatus::Hidden(hidden) if hidden.iter().any(|t| t == "browser_navigate")),
        "{before:?}"
    );
    let names = runtime.tool_names();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    register_task_guidance_context(
        Some(GuidanceHookMode::Assisted),
        GuidanceCatalog {
            root: project.clone(),
            home: Some(dir.path().join("home")),
            trusted: runtime.trust_flag(),
        },
        localpilot_harness::capabilities_from_tools(&names),
        &mut runtime,
    );

    let (events, _rx) = broadcast::channel::<RuntimeEvent>(512);
    let prompt = format!(
        "Add a score counter to the scene. Open {url} in the browser, click Start and take a screenshot."
    );
    runtime
        .run_turn(&prompt, &events, &CancellationToken::new())
        .await;

    // ---- what the model was offered, read from the wire
    let requests = wire.received_requests().await.unwrap();
    let bodies: Vec<String> = requests
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .collect();
    assert!(
        bodies.len() >= 14,
        "the script did not run to the end: {} requests",
        bodies.len()
    );
    let advertised: BTreeSet<String> = serde_json::from_str::<Value>(&bodies[0]).unwrap()["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    eprintln!("REAL RUN advertised in the first request: {advertised:?}");
    assert!(
        !advertised.contains("browser_navigate"),
        "the first request already advertised navigation: {advertised:?}"
    );
    // After the model loaded them, the last request advertises what it used.
    let last_advertised: BTreeSet<String> = serde_json::from_str::<Value>(bodies.last().unwrap())
        .unwrap()["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    for tool in [
        "browser_navigate",
        "browser_click",
        "browser_take_screenshot",
        "browser_snapshot",
        "browser_console_messages",
    ] {
        assert!(
            last_advertised.contains(tool),
            "{tool} not advertised after loading: {last_advertised:?}"
        );
    }

    // ---- the page, known from the test's own web server
    let seen = oracle.requests();
    eprintln!("REAL RUN oracle saw: {seen:?}");
    assert!(seen.contains(&"GET /".to_string()), "{seen:?}");
    assert!(seen.contains(&"GET /missing.png".to_string()), "{seen:?}");
    assert_eq!(
        seen.iter().filter(|r| *r == "POST /clicked").count(),
        1,
        "exactly one click reached the page: {seen:?}"
    );

    // ---- images on the wire
    let shots: Vec<Vec<u8>> = bodies.iter().flat_map(|body| images_in(body)).collect();
    assert!(
        !shots.is_empty(),
        "no image reached the provider on the wire"
    );
    let first = &shots[0];
    eprintln!(
        "REAL RUN screenshots on the wire: {:?}",
        shots
            .iter()
            .map(|shot| (png_size(shot), shot.len()))
            .collect::<Vec<_>>()
    );
    let (width, height) = png_size(first).expect("a real PNG reached the provider");
    assert!(
        width > 0 && height > 0 && first.len() > 2_000,
        "{width}x{height}, {} bytes",
        first.len()
    );
    let distinct: BTreeSet<&Vec<u8>> = shots.iter().collect();
    assert!(
        distinct.len() >= 2,
        "the screenshot after the click is the same image as the one before it"
    );
    // The console error reached the provider as text.
    assert!(
        bodies
            .iter()
            .any(|b| b.contains("asset missing: models/ship.glb")),
        "the console error never reached the provider"
    );

    // ---- what the session recorded
    let observations = runtime.task_observations();
    let browser: Vec<_> = observations
        .iter()
        .filter(|o| o.capability == "browser.interact")
        .collect();
    eprintln!(
        "REAL RUN browser observations: {:?}",
        browser
            .iter()
            .map(|o| (o.tool.as_str(), o.state, o.result.as_str()))
            .collect::<Vec<_>>()
    );
    assert!(browser
        .iter()
        .any(|o| o.tool == "browser_snapshot" && o.state == EvidenceState::Passed));
    assert!(browser
        .iter()
        .any(|o| o.tool == "browser_console_messages" && o.state == EvidenceState::Passed));
    let clicks: Vec<_> = browser
        .iter()
        .filter(|o| o.tool == "browser_click")
        .collect();
    assert_eq!(
        clicks.last().map(|o| o.state),
        Some(EvidenceState::Passed),
        "the click that worked is the last one: {clicks:?}"
    );
    let images: Vec<_> = observations
        .iter()
        .filter(|o| o.capability == "image.inspect")
        .collect();
    assert_eq!(images.len(), 2, "{images:?}");
    assert!(
        images
            .iter()
            .all(|o| o.image == Some(ImageState::Inspected)),
        "{images:?}"
    );
    let requested = runtime.requested_evidence();
    for capability in ["browser.interact", "image.inspect"] {
        let entry = requested.iter().find(|r| r.capability == capability);
        assert_eq!(
            entry.map(|e| e.state),
            Some(EvidenceState::Passed),
            "{capability}: {requested:?}"
        );
    }

    // ---- the route claims only what the run showed
    let route = runtime
        .observation_routes()
        .into_iter()
        .find(|r| r.capability == "browser.interact")
        .unwrap();
    assert!(
        matches!(route.status, RouteStatus::Available(_)),
        "{route:?} although the run loaded and used the browser tools"
    );

    // ---- hygiene: nothing left behind
    drop(runtime);
    drop(mcp);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(
        !Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".playwright-mcp")
            .exists(),
        "the browser server wrote into the working directory"
    );
    if let (Some(before), Some(after)) = (before_processes, browser_processes()) {
        assert!(
            after <= before,
            "browser processes left behind: {before} before, {after} after"
        );
    }
}
