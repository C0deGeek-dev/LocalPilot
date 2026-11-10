//! Task guidance across a session's life: a component change within a turn,
//! repeated requests, compaction, resume, and packages that change, disappear
//! or stop being trusted.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use localpilot_core::{ContentBlock, Role};
use localpilot_harness::{
    register_task_guidance_context, GuidanceCatalog, GuidanceHookMode, RuntimeEvent, SessionConfig,
    SessionRuntime,
};
use localpilot_llm::{FakeProvider, ModelRequest};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn guided(root: &Path, name: &str, guidance: &str, body: &str) {
    write(
        root,
        &format!(".localpilot/skills/{name}/skill.toml"),
        &format!(
            "name = \"{name}\"\ndescription = \"{name} guidance\"\nversion = \"1.0.0\"\n\
             [guidance]\nschema_version = 1\n{guidance}\n"
        ),
    );
    write(root, &format!(".localpilot/skills/{name}/SKILL.md"), body);
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        guided(
            &home,
            "investigate",
            "role = \"procedure\"\nintents = [\"investigate\"]",
            "BODY-INVESTIGATE\n",
        );
        guided(
            &home,
            "implement",
            "role = \"procedure\"\nintents = [\"implement\"]",
            "BODY-IMPLEMENT\n",
        );
        guided(
            &home,
            "web",
            "role = \"supplement\"\nstacks = [\"web\"]",
            "BODY-WEB\n",
        );
        guided(
            &home,
            "dotnet",
            "role = \"supplement\"\nstacks = [\"dotnet\"]",
            "BODY-DOTNET\n",
        );
        guided(
            &home,
            "rust",
            "role = \"supplement\"\nstacks = [\"rust\"]",
            "BODY-RUST v1\n",
        );
        Self { dir }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn runtime(&self, provider: Arc<FakeProvider>, config: SessionConfig) -> SessionRuntime {
        let mut runtime = SessionRuntime::new(
            provider as Arc<dyn localpilot_llm::ModelProvider>,
            ToolRegistry::with_builtins(),
            PermissionEngine::new(Profile::Default, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::open(&self.dir.path().join("store")),
            Workspace::new(&self.project()).unwrap(),
            RecoveryEngine::new(RecoveryBudget::default()),
            config,
            Vec::new(),
        );
        let catalog = GuidanceCatalog {
            root: self.project(),
            home: Some(self.home()),
            trusted: runtime.trust_flag(),
        };
        register_task_guidance_context(
            Some(GuidanceHookMode::Assisted),
            catalog,
            BTreeSet::from(["process.capture".to_string()]),
            &mut runtime,
        );
        runtime
    }
}

async fn turn(runtime: &mut SessionRuntime, prompt: &str) {
    let (events, _rx) = broadcast::channel::<RuntimeEvent>(64);
    runtime
        .run_turn(prompt, &events, &CancellationToken::new())
        .await;
}

fn prose(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .filter(|m| matches!(m.role, Role::User | Role::System))
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn work_that_moves_to_another_component_updates_the_next_request() {
    let fixture = Fixture::new();
    let project = fixture.project();
    write(
        &project,
        "api/Api.csproj",
        r#"<Project Sdk="Microsoft.NET.Sdk.Web"></Project>"#,
    );
    write(
        &project,
        "web/package.json",
        r#"{"name": "web", "dependencies": {"react": "^18.3.1"}}"#,
    );
    write(&project, "web/src/App.tsx", "export const App = 1;\n");
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("read-1", "read_file", json!({"path": "web/src/App.tsx"}))
            .text("done"),
    );
    let mut runtime = fixture.runtime(Arc::clone(&provider), SessionConfig::default());

    turn(&mut runtime, "Add input validation.").await;

    let sent = provider.requests();
    assert_eq!(sent.len(), 2);
    // Before any path: the component is unclear, so no body is loaded.
    assert!(!prose(&sent[0]).contains("BODY-"), "{}", prose(&sent[0]));
    // After the work touched the web app: its guidance, once.
    let second = prose(&sent[1]);
    assert_eq!(second.matches("BODY-WEB").count(), 1, "{second}");
    assert_eq!(second.matches("BODY-IMPLEMENT").count(), 1, "{second}");
    assert!(!second.contains("BODY-DOTNET"));
    let selection = runtime.guidance_selection().unwrap();
    assert_eq!(selection.component.component, "web");
    // The record follows the same request.
    let record = runtime.task_guidance();
    assert!(record[1].entries.iter().any(|e| e.source.id == "web"));
    assert!(record[0].entries.iter().all(|e| e.source.id != "web"));
}

#[tokio::test]
async fn unchanged_guidance_appears_once_per_request_and_survives_compaction() {
    let fixture = Fixture::new();
    write(
        &fixture.project(),
        "Cargo.toml",
        "[package]\nname = \"c\"\nversion = \"0.1.0\"\n",
    );
    let provider = Arc::new(
        FakeProvider::new()
            .text("one")
            .text("two")
            .text("summary")
            .text("three"),
    );
    let mut runtime = fixture.runtime(Arc::clone(&provider), SessionConfig::default());

    turn(&mut runtime, "Fix the wrong exit code.").await;
    turn(&mut runtime, "Fix the wrong exit code.").await;
    runtime.compact_conversation_force().await;
    turn(&mut runtime, "Fix the wrong exit code.").await;

    for request in provider
        .requests()
        .iter()
        .filter(|r| prose(r).contains("Question:"))
    {
        let text = prose(request);
        assert_eq!(text.matches("BODY-INVESTIGATE").count(), 1, "{text}");
        assert_eq!(text.matches("BODY-RUST v1").count(), 1, "{text}");
    }
    let last = provider.requests().pop().unwrap();
    assert!(prose(&last).contains("BODY-INVESTIGATE"));
}

#[tokio::test]
async fn a_resumed_session_continues_the_task_in_progress() {
    let fixture = Fixture::new();
    write(
        &fixture.project(),
        "Cargo.toml",
        "[package]\nname = \"c\"\nversion = \"0.1.0\"\n",
    );
    let first = Arc::new(FakeProvider::new().text("looking"));
    let mut runtime = fixture.runtime(Arc::clone(&first), SessionConfig::default());
    turn(&mut runtime, "Fix the wrong exit code.").await;
    let session = runtime.session_id();
    drop(runtime);

    let second = Arc::new(FakeProvider::new().text("continuing"));
    let mut resumed = fixture.runtime(Arc::clone(&second), SessionConfig::default());
    resumed.load_session(session).unwrap();
    turn(&mut resumed, "ga door").await;

    let text = prose(&second.requests()[0]);
    assert!(text.contains("BODY-INVESTIGATE"), "{text}");
    let selection = resumed.guidance_selection().unwrap();
    assert!(
        selection
            .intent
            .reason
            .contains("continues the earlier request"),
        "{}",
        selection.intent.reason
    );

    // Without the earlier request, the same words choose nothing to load.
    let fresh = Arc::new(FakeProvider::new().text("?"));
    let mut new_session = fixture.runtime(Arc::clone(&fresh), SessionConfig::default());
    turn(&mut new_session, "ga door").await;
    assert!(!prose(&fresh.requests()[0]).contains("BODY-INVESTIGATE"));
}

#[tokio::test]
async fn changed_removed_and_untrusted_packages_are_not_reused() {
    let fixture = Fixture::new();
    let project = fixture.project();
    write(
        &project,
        "Cargo.toml",
        "[package]\nname = \"c\"\nversion = \"0.1.0\"\n",
    );
    guided(
        &project,
        "cargo-local",
        "role = \"supplement\"\nstacks = [\"rust\"]",
        "BODY-PROJECT-LOCAL\n",
    );
    let provider = Arc::new(FakeProvider::new().text("a").text("b").text("c").text("d"));
    let mut runtime = fixture.runtime(Arc::clone(&provider), SessionConfig::default());

    turn(&mut runtime, "Fix the wrong exit code.").await;
    assert!(prose(&provider.requests()[0]).contains("BODY-RUST v1"));
    assert!(prose(&provider.requests()[0]).contains("BODY-PROJECT-LOCAL"));

    // Changed content: the new text, never the old.
    write(
        &fixture.home(),
        ".localpilot/skills/rust/SKILL.md",
        "BODY-RUST v2\n",
    );
    turn(&mut runtime, "Fix the wrong exit code.").await;
    let second = prose(&provider.requests()[1]);
    assert!(
        second.contains("BODY-RUST v2") && !second.contains("BODY-RUST v1"),
        "{second}"
    );

    // Removed package: gone.
    std::fs::remove_dir_all(fixture.home().join(".localpilot/skills/rust")).unwrap();
    turn(&mut runtime, "Fix the wrong exit code.").await;
    assert!(!prose(&provider.requests()[2]).contains("BODY-RUST"));

    // No longer trusted: the project's own package is gone.
    runtime.set_trusted(false);
    turn(&mut runtime, "Fix the wrong exit code.").await;
    let fourth = prose(&provider.requests()[3]);
    assert!(!fourth.contains("BODY-PROJECT-LOCAL"), "{fourth}");
    assert!(fourth.contains("BODY-INVESTIGATE"));
}
