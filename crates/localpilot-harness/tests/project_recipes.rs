//! Reviewed project run recipes: chosen by component from trusted project
//! packages, shown with their validation state, and never more than advice.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use localpilot_core::{ContentBlock, GuidanceKind, Role};
use localpilot_harness::{
    register_task_guidance_context, GuidanceCatalog, GuidanceDelivery, GuidanceHookMode,
    RuntimeEvent, SessionConfig, SessionRuntime,
};
use localpilot_llm::{FakeProvider, ModelRequest};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn recipe_package(root: &Path, name: &str, recipe: &str) {
    write(
        root,
        &format!(".localpilot/skills/{name}/skill.toml"),
        &format!("name = \"{name}\"\ndescription = \"How this project runs\"\nversion = \"1.0.0\"\n\n{recipe}"),
    );
    write(
        root,
        &format!(".localpilot/skills/{name}/SKILL.md"),
        "Run notes.\n",
    );
}

const API_RECIPE: &str = r#"[[recipe]]
name = "api-dev"
component = "api"
working_directory = "api"
program = "dotnet"
args = ["run", "--urls", "http://127.0.0.1:5199"]
credentials = ["ORDERS_DB_PASSWORD"]
readiness = { kind = "http_get", value = "http://127.0.0.1:5199/health" }
scenario = ["POST an order", "GET it back"]
cleanup = "stop_started_process"
"#;

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        write(
            &project,
            "api/Api.csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk.Web"></Project>"#,
        );
        write(&project, "api/Program.cs", "var app = 1;\n");
        write(&project, "web/package.json", r#"{"name": "web"}"#);
        recipe_package(&project, "shop-run", API_RECIPE);
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        Self { dir }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    async fn run(&self, prompt: &str, trusted: bool) -> (SessionRuntime, ModelRequest) {
        let provider = Arc::new(FakeProvider::new().text("ok"));
        let mut runtime = SessionRuntime::new(
            Arc::clone(&provider) as Arc<dyn localpilot_llm::ModelProvider>,
            ToolRegistry::with_builtins(),
            PermissionEngine::new(Profile::Default, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::open(&self.dir.path().join("store")),
            Workspace::new(&self.project()).unwrap(),
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig {
                trusted,
                ..SessionConfig::default()
            },
            Vec::new(),
        );
        let catalog = GuidanceCatalog {
            root: self.project(),
            home: Some(self.dir.path().join("home")),
            trusted: runtime.trust_flag(),
        };
        register_task_guidance_context(
            Some(GuidanceHookMode::Suggest),
            catalog,
            BTreeSet::from([
                "process.capture".to_string(),
                "process.background".to_string(),
            ]),
            &mut runtime,
        );
        let (events, _rx) = broadcast::channel::<RuntimeEvent>(64);
        runtime
            .run_turn(prompt, &events, &CancellationToken::new())
            .await;
        let request = provider.requests().pop().unwrap();
        (runtime, request)
    }
}

fn user_text(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_recipe_for_the_component_reaches_the_request_with_its_source() {
    let fixture = Fixture::new();
    let (runtime, request) = fixture
        .run("Add a validation rule in api/Program.cs", true)
        .await;

    let text = user_text(&request);
    assert!(
        text.contains("Project run recipes for this component"),
        "{text}"
    );
    assert!(
        text.contains("- shop-run/api-dev: run `dotnet run --urls http://127.0.0.1:5199` in api"),
        "{text}"
    );
    assert!(text.contains("needs ORDERS_DB_PASSWORD from the user"));
    assert!(text.contains("(not validated)"));
    let selection = runtime.guidance_selection().unwrap();
    assert_eq!(selection.recipes.len(), 1);
    assert!(selection.recipes[0]
        .reason
        .contains("component api from shop-run"));
    let entry = runtime.task_guidance()[0]
        .entries
        .iter()
        .find(|e| e.source.kind == GuidanceKind::ProjectRecipe)
        .cloned()
        .unwrap();
    assert_eq!(entry.source.id, "shop-run/api-dev");
    assert_eq!(entry.delivery, GuidanceDelivery::Intact);
}

#[tokio::test]
async fn another_component_an_explanation_and_an_untrusted_workspace_get_no_recipe() {
    let fixture = Fixture::new();

    let (_, other) = fixture.run("Add a page in web/src/App.tsx", true).await;
    assert!(!user_text(&other).contains("api-dev"));

    let (explained, request) = fixture.run("Explain api/Program.cs", true).await;
    assert!(!user_text(&request).contains("api-dev"));
    let selection = explained.guidance_selection().unwrap();
    assert!(selection
        .excluded
        .iter()
        .any(|e| e.name == "shop-run/api-dev" && e.reason.contains("does not run anything")));

    let (_, untrusted) = fixture
        .run("Add a validation rule in api/Program.cs", false)
        .await;
    assert!(!user_text(&untrusted).contains("api-dev"));
}

#[tokio::test]
async fn a_global_recipe_names_no_project_and_is_left_out() {
    let fixture = Fixture::new();
    recipe_package(
        &fixture.dir.path().join("home"),
        "global-run",
        &API_RECIPE.replace("api-dev", "global-dev"),
    );

    let (runtime, request) = fixture
        .run("Add a validation rule in api/Program.cs", true)
        .await;

    assert!(!user_text(&request).contains("global-dev"));
    assert!(runtime
        .guidance_selection()
        .unwrap()
        .excluded
        .iter()
        .any(|e| e.name == "global-run/global-dev" && e.reason.contains("names no project")));
}

#[tokio::test]
async fn a_stale_recipe_is_shown_as_not_validated_for_the_changed_file() {
    let fixture = Fixture::new();
    let project = fixture.project();
    write(&project, "api/packages.lock.json", "{\"v\": 1}");
    let hash = format!("{:x}", Sha256::digest(b"{\"v\": 1}"));
    recipe_package(
        &project,
        "shop-run",
        &format!("{API_RECIPE}validated_against = {{ \"api/packages.lock.json\" = \"{hash}\" }}\n"),
    );

    let (_, current) = fixture
        .run("Add a validation rule in api/Program.cs", true)
        .await;
    assert!(user_text(&current).contains("(validated for the current files)"));

    write(&project, "api/packages.lock.json", "{\"v\": 2}");
    let (_, stale) = fixture
        .run("Add a validation rule in api/Program.cs", true)
        .await;
    let text = user_text(&stale);
    assert!(
        text.contains("not validated since api/packages.lock.json changed"),
        "{text}"
    );
}

#[tokio::test]
async fn a_recipe_changes_no_tool_and_runs_nothing() {
    let fixture = Fixture::new();
    let (with_recipe, request) = fixture
        .run("Add a validation rule in api/Program.cs", true)
        .await;

    // The turn made one request and no tool call: the host ran nothing.
    assert!(request.messages.iter().all(|m| m
        .content
        .iter()
        .all(|b| !matches!(b, ContentBlock::ToolResult(_)))));
    let plain = SessionRuntime::new(
        Arc::new(FakeProvider::new()) as Arc<dyn localpilot_llm::ModelProvider>,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(&fixture.dir.path().join("store2")),
        Workspace::new(&fixture.project()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    );
    assert_eq!(with_recipe.tool_names(), plain.tool_names());
    assert!(!fixture.project().join("api/bin").exists());
}
