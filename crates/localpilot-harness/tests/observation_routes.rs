//! Qualify the browser, desktop and game observation routes before any
//! visual or interaction claim.
//!
//! The browser route is exercised end to end with a controlled MCP browser
//! server: the tools are registered from its descriptors, authorized by the
//! permission engine, dispatched to the server, and their results delivered
//! back, including a screenshot. A screenshot counts as inspected only when
//! the provider accepts images and answered the request that held it; for a
//! text-only provider it stays captured and the visual claim stays
//! unsupported. Desktop interaction and engine runs have no qualified adapter:
//! their claims are reported as deferred, and work on such projects goes on.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use localpilot_core::{ContentBlock, Role};
use localpilot_harness::{
    register_task_guidance_context, EvidenceState, GuidanceCatalog, GuidanceHookMode, ImageState,
    RouteStatus, RuntimeEvent, SessionConfig, SessionRuntime,
};
use localpilot_llm::{FakeProvider, ModelRequest};
use localpilot_mcp::{McpError, McpTool, McpToolDescriptor, Transport};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Effect, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// A controlled browser server: answers each tool by name and records calls.
#[derive(Default)]
struct BrowserServer {
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl Transport for BrowserServer {
    async fn call(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let name = params["name"].as_str().unwrap_or_default().to_string();
        self.calls.lock().unwrap().push(format!("{method}:{name}"));
        Ok(match name.as_str() {
            // The shapes the real Playwright MCP server returns: a text block
            // with the page state, and for a screenshot a text block followed by
            // an image block.
            "browser_navigate" => json!({ "content": [{ "type": "text",
                "text": "### Ran Playwright code
### Page
- Page URL: http://127.0.0.1:5173/" }] }),
            "browser_click" if params["arguments"]["target"].is_string() => {
                json!({ "content": [{ "type": "text", "text": "### Ran Playwright code
await page.getByRole('button').click();" }] })
            }
            // What the real server answers when the required `target` is missing.
            "browser_click" => json!({ "isError": true, "content": [{ "type": "text",
                "text": "### Error
Invalid arguments for tool \"browser_click\"" }] }),
            "browser_take_screenshot" => json!({ "content": [
                { "type": "text", "text": "### Result
- [Screenshot of viewport](page.png)" },
                { "type": "image", "mimeType": "image/png", "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==" }
            ] }),
            _ => return Err(McpError::Protocol(format!("unknown tool {name}"))),
        })
    }
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn registry(server: &Arc<BrowserServer>) -> ToolRegistry {
    let mut registry = ToolRegistry::with_builtins();
    for name in [
        "browser_navigate",
        "browser_click",
        "browser_take_screenshot",
    ] {
        let descriptor = McpToolDescriptor {
            name: name.to_string(),
            description: format!("{name} on the controlled test page"),
            input_schema: json!({ "type": "object" }),
        };
        // The same effects the CLI gives every MCP tool.
        registry.register(Box::new(McpTool::new(
            &descriptor,
            vec![Effect::Network],
            Arc::clone(server) as Arc<dyn Transport>,
        )));
    }
    registry
}

fn session(
    dir: &Path,
    tools: ToolRegistry,
    provider: Arc<FakeProvider>,
    accepts_images: bool,
) -> SessionRuntime {
    let project = dir.join("project");
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut runtime = SessionRuntime::new(
        provider as Arc<dyn localpilot_llm::ModelProvider>,
        tools,
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(&dir.join("store")),
        Workspace::new(&project).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    );
    runtime.set_image_support_override(Some(accepts_images));
    let names = runtime.tool_names();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let capabilities = localpilot_harness::capabilities_from_tools(&names);
    let catalog = GuidanceCatalog {
        root: project,
        home: Some(home),
        trusted: runtime.trust_flag(),
    };
    register_task_guidance_context(
        Some(GuidanceHookMode::Assisted),
        catalog,
        capabilities,
        &mut runtime,
    );
    runtime
}

async fn turn(runtime: &mut SessionRuntime, prompt: &str) {
    let (events, _rx) = broadcast::channel::<RuntimeEvent>(256);
    runtime
        .run_turn(prompt, &events, &CancellationToken::new())
        .await;
}

fn browser_script() -> FakeProvider {
    FakeProvider::new()
        .tool_call(
            "nav-1",
            "browser_navigate",
            json!({"url": "http://127.0.0.1:5173/"}),
        )
        .tool_call(
            "click-1",
            "browser_click",
            json!({"element": "Start button", "target": "e3"}),
        )
        .tool_call("shot-1", "browser_take_screenshot", json!({}))
        .text("The score shows 1 after the click.")
}

fn three_js_game(dir: &Path) {
    write(
        &dir.join("project"),
        "package.json",
        r#"{"name": "g", "dependencies": {"three": "0.169.0"}, "devDependencies": {"vite": "^5.4.0"}}"#,
    );
}

fn tool_text(request: &ModelRequest, id: &str) -> Option<String> {
    request
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::ToolResult(result) if result.id.to_string() == id => {
                Some(result.output.clone())
            }
            _ => None,
        })
}

#[tokio::test]
async fn the_browser_route_is_registered_authorized_dispatched_and_delivered() {
    let dir = tempfile::tempdir().unwrap();
    three_js_game(dir.path());
    let server = Arc::new(BrowserServer::default());
    let provider = Arc::new(browser_script());
    let mut runtime = session(dir.path(), registry(&server), Arc::clone(&provider), true);

    turn(&mut runtime, "Add a score counter to the scene.").await;

    // Dispatched to the server, in order.
    assert_eq!(
        *server.calls.lock().unwrap(),
        [
            "tools/call:browser_navigate",
            "tools/call:browser_click",
            "tools/call:browser_take_screenshot"
        ]
    );
    // Delivered back to the model.
    let last = provider.requests().pop().unwrap();
    assert!(tool_text(&last, "click-1")
        .unwrap()
        .contains("getByRole('button').click()"));
    assert!(tool_text(&last, "shot-1")
        .unwrap()
        .contains("[image attached: image/png]"));

    let observations = runtime.task_observations();
    let interactions: Vec<_> = observations
        .iter()
        .filter(|o| o.capability == "browser.interact")
        .collect();
    assert_eq!(interactions.len(), 2);
    assert!(interactions
        .iter()
        .all(|o| o.state == EvidenceState::Passed));
    let shot = observations
        .iter()
        .find(|o| o.capability == "image.inspect")
        .unwrap();
    assert_eq!(shot.image, Some(ImageState::Inspected), "{shot:?}");

    let requested = runtime.requested_evidence();
    for capability in ["browser.interact", "image.inspect"] {
        let entry = requested
            .iter()
            .find(|r| r.capability == capability)
            .unwrap();
        assert_eq!(
            entry.state,
            EvidenceState::Passed,
            "{capability}: {entry:?}"
        );
    }
}

#[tokio::test]
async fn a_screenshot_for_a_text_only_provider_is_captured_and_supports_no_visual_claim() {
    let dir = tempfile::tempdir().unwrap();
    three_js_game(dir.path());
    let server = Arc::new(BrowserServer::default());
    let mut runtime = session(
        dir.path(),
        registry(&server),
        Arc::new(browser_script()),
        false,
    );

    turn(&mut runtime, "Add a score counter to the scene.").await;

    let shot = runtime
        .task_observations()
        .into_iter()
        .find(|o| o.capability == "image.inspect")
        .unwrap();
    assert_eq!(shot.image, Some(ImageState::Captured));
    let requested = runtime.requested_evidence();
    let image = requested
        .iter()
        .find(|r| r.capability == "image.inspect")
        .unwrap();
    assert_eq!(image.state, EvidenceState::Unavailable);
    // Interaction is still observed: the narrower claim stands.
    let browser = requested
        .iter()
        .find(|r| r.capability == "browser.interact")
        .unwrap();
    assert_eq!(browser.state, EvidenceState::Passed);
    let route = runtime
        .observation_routes()
        .into_iter()
        .find(|r| r.capability == "image.inspect")
        .unwrap();
    assert!(
        matches!(route.status, RouteStatus::Unavailable(ref why) if why.contains("does not accept images"))
    );
}

#[tokio::test]
async fn without_browser_tools_browser_claims_are_unavailable_and_work_continues() {
    let dir = tempfile::tempdir().unwrap();
    three_js_game(dir.path());
    let provider = Arc::new(FakeProvider::new().text("Added the counter."));
    let mut runtime = session(dir.path(), ToolRegistry::with_builtins(), provider, true);

    turn(&mut runtime, "Add a score counter to the scene.").await;

    let selection = runtime.guidance_selection().unwrap();
    assert!(selection.selected_names().contains(&"guide-web"));
    let requested = runtime.requested_evidence();
    assert_eq!(
        requested
            .iter()
            .find(|r| r.capability == "browser.interact")
            .unwrap()
            .state,
        EvidenceState::Unavailable
    );
}

#[tokio::test]
async fn desktop_and_engine_claims_are_deferred_and_say_why() {
    for (files, request, capability, reason) in [
        (
            vec![(
                "Notes.csproj",
                r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><UseWPF>true</UseWPF></PropertyGroup></Project>"#,
            )],
            "Fix the save button.",
            "desktop.interact",
            "headless tests are not window interaction",
        ),
        (
            vec![(
                "project.godot",
                "config_version=5\n[application]\nconfig/features=PackedStringArray(\"4.3\")\n",
            )],
            "Fix the double jump.",
            "engine.run",
            "headless engine command is process evidence only",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        for (path, contents) in files {
            write(&dir.path().join("project"), path, contents);
        }
        let provider = Arc::new(FakeProvider::new().text("Looked at it."));
        let mut runtime = session(dir.path(), ToolRegistry::with_builtins(), provider, true);

        turn(&mut runtime, request).await;

        let requested = runtime.requested_evidence();
        let entry = requested
            .iter()
            .find(|r| r.capability == capability)
            .unwrap();
        assert_eq!(entry.state, EvidenceState::Unavailable, "{capability}");
        let route = runtime
            .observation_routes()
            .into_iter()
            .find(|r| r.capability == capability)
            .unwrap();
        assert!(
            matches!(route.status, RouteStatus::Deferred(ref why) if why.contains(reason)),
            "{route:?}"
        );
        let report = runtime.guidance_report().render();
        assert!(report.contains(reason), "{report}");
        // The task itself went on: a procedure was selected and a turn ran.
        assert!(runtime.guidance_selection().unwrap().primary.is_some());
    }
}

#[tokio::test]
async fn a_browser_call_the_permission_engine_refuses_is_denied_evidence() {
    let dir = tempfile::tempdir().unwrap();
    three_js_game(dir.path());
    let server = Arc::new(BrowserServer::default());
    let project = dir.path().join("project");
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let provider = Arc::new(browser_script());
    let mut runtime = SessionRuntime::new(
        provider as Arc<dyn localpilot_llm::ModelProvider>,
        registry(&server),
        PermissionEngine::new(Profile::Default, Vec::new()),
        // Every approval request is answered no.
        Box::new(ScriptedApprover::new(Vec::new())),
        Store::open(&dir.path().join("store")),
        Workspace::new(&project).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    );

    turn(&mut runtime, "Add a score counter to the scene.").await;

    assert!(
        server.calls.lock().unwrap().is_empty(),
        "nothing reached the server"
    );
    let observations = runtime.task_observations();
    assert!(!observations.is_empty());
    assert!(
        observations
            .iter()
            .all(|o| o.state == EvidenceState::Denied),
        "{observations:?}"
    );
}
