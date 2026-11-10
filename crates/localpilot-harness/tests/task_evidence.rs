//! Evidence guidance asks for, against what tools actually observed, in real
//! sessions with the built-in guidance.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use localpilot_harness::{
    register_task_guidance_context, EvidenceState, GuidanceCatalog, GuidanceHookMode, RuntimeEvent,
    SessionConfig, SessionRuntime,
};
use localpilot_llm::FakeProvider;
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

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn rust_cli() -> Self {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("project"),
            "Cargo.toml",
            "[package]\nname = \"c\"\nversion = \"0.1.0\"\n",
        );
        write(&dir.path().join("project"), "src/main.rs", "fn main() {}\n");
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        Self { dir }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn runtime(
        &self,
        provider: Arc<FakeProvider>,
        profile: Profile,
        mode: Option<GuidanceHookMode>,
        store: &str,
    ) -> SessionRuntime {
        let mut runtime = SessionRuntime::new(
            provider as Arc<dyn localpilot_llm::ModelProvider>,
            ToolRegistry::with_builtins(),
            PermissionEngine::new(profile, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::open(&self.dir.path().join(store)),
            Workspace::new(&self.project()).unwrap(),
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig::default(),
            Vec::new(),
        );
        let names = runtime.tool_names();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let capabilities: BTreeSet<String> = localpilot_harness::capabilities_from_tools(&names);
        let catalog = GuidanceCatalog {
            root: self.project(),
            home: Some(self.dir.path().join("home")),
            trusted: runtime.trust_flag(),
        };
        register_task_guidance_context(mode, catalog, capabilities, &mut runtime);
        runtime
    }
}

async fn turn(runtime: &mut SessionRuntime, prompt: &str) {
    let (events, _rx) = broadcast::channel::<RuntimeEvent>(256);
    runtime
        .run_turn(prompt, &events, &CancellationToken::new())
        .await;
}

fn shell(id: &str, command: &str) -> (String, serde_json::Value) {
    (id.to_string(), json!({"command": command}))
}

#[tokio::test]
async fn the_cli_guidance_asks_for_process_evidence_and_the_exit_code_decides_it() {
    let fixture = Fixture::rust_cli();
    let (first_id, first) = shell("run-1", "exit 3");
    let (second_id, second) = shell("run-2", "exit 0");
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(&first_id, "run_shell", first)
            .tool_call(&second_id, "run_shell", second)
            .text("done"),
    );
    let mut runtime = fixture.runtime(
        provider,
        Profile::Default,
        Some(GuidanceHookMode::Assisted),
        "store",
    );

    turn(&mut runtime, "Fix the wrong exit code.").await;

    let observations = runtime.task_observations();
    let shell: Vec<_> = observations
        .iter()
        .filter(|o| o.capability == "process.capture")
        .collect();
    assert_eq!(shell.len(), 2, "{observations:?}");
    assert_eq!(
        (shell[0].state, shell[0].result.as_str()),
        (EvidenceState::Failed, "exit 3")
    );
    assert_eq!(
        (shell[1].state, shell[1].result.as_str()),
        (EvidenceState::Passed, "exit 0")
    );
    assert_eq!(shell[0].component.as_deref(), Some("."));
    assert_eq!(shell[0].intent.as_deref(), Some("investigate"));
    assert_eq!(shell[0].call, "run-1");

    let requested = runtime.requested_evidence();
    let process = requested
        .iter()
        .find(|r| r.capability == "process.capture")
        .unwrap();
    assert_eq!(process.requested_by, ["guide-cli"]);
    assert_eq!(process.state, EvidenceState::Passed);
    assert_eq!(process.calls, ["run-1", "run-2"]);
    let report = runtime.guidance_report().render();
    assert!(
        report.contains("evidence process.capture for guide-cli: observed passing (run-1, run-2)"),
        "{report}"
    );
}

#[tokio::test]
async fn loaded_guidance_alone_is_not_evidence() {
    let fixture = Fixture::rust_cli();
    let provider = Arc::new(FakeProvider::new().text("I would run the binary."));
    let mut runtime = fixture.runtime(
        provider,
        Profile::Default,
        Some(GuidanceHookMode::Assisted),
        "store",
    );

    turn(&mut runtime, "Fix the wrong exit code.").await;

    assert!(runtime.task_observations().is_empty());
    let requested = runtime.requested_evidence();
    let process = requested
        .iter()
        .find(|r| r.capability == "process.capture")
        .unwrap();
    assert_eq!(process.state, EvidenceState::Planned);
    assert!(process.calls.is_empty());
}

#[tokio::test]
async fn a_refused_command_is_denied_evidence() {
    let fixture = Fixture::rust_cli();
    let (id, input) = shell("run-1", "exit 0");
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(&id, "run_shell", input)
            .text("blocked"),
    );
    let mut runtime = fixture.runtime(
        provider,
        Profile::ReadOnly,
        Some(GuidanceHookMode::Assisted),
        "store",
    );

    turn(&mut runtime, "Fix the wrong exit code.").await;

    let observation = runtime
        .task_observations()
        .into_iter()
        .find(|o| o.capability == "process.capture")
        .unwrap();
    assert_eq!(observation.state, EvidenceState::Denied);
    let requested = runtime.requested_evidence();
    assert_eq!(
        requested
            .iter()
            .find(|r| r.capability == "process.capture")
            .unwrap()
            .state,
        EvidenceState::Denied
    );
}

#[tokio::test]
async fn a_capability_the_session_lacks_stays_unavailable_whatever_ran() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("project"),
        "project.godot",
        "config_version=5\n[application]\nconfig/features=PackedStringArray(\"4.3\")\n",
    );
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let fixture = Fixture { dir };
    let (id, input) = shell("run-1", "exit 0");
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(&id, "run_shell", input)
            .text("done"),
    );
    let mut runtime = fixture.runtime(
        provider,
        Profile::Default,
        Some(GuidanceHookMode::Assisted),
        "store",
    );

    turn(&mut runtime, "Fix the double jump.").await;

    let requested = runtime.requested_evidence();
    let engine = requested
        .iter()
        .find(|r| r.capability == "engine.run")
        .unwrap();
    assert_eq!(engine.state, EvidenceState::Unavailable);
    assert!(engine.requested_by.contains(&"guide-godot4".to_string()));
    // The shell command did run and is recorded, but it is not engine evidence.
    assert!(runtime
        .task_observations()
        .iter()
        .any(|o| o.capability == "process.capture"));
}

#[tokio::test]
async fn guidance_and_evidence_change_no_recorded_event_or_check() {
    let fixture = Fixture::rust_cli();
    let script = || {
        let (id, input) = shell("run-1", "exit 0");
        Arc::new(
            FakeProvider::new()
                .tool_call(&id, "run_shell", input)
                .text("done"),
        )
    };
    let mut off = fixture.runtime(script(), Profile::Default, None, "store-off");
    let mut on = fixture.runtime(
        script(),
        Profile::Default,
        Some(GuidanceHookMode::Assisted),
        "store-on",
    );
    turn(&mut off, "Fix the wrong exit code.").await;
    turn(&mut on, "Fix the wrong exit code.").await;

    let kinds = |runtime: &SessionRuntime, store: &str| -> Vec<String> {
        Store::open(&fixture.dir.path().join(store))
            .read_events(runtime.session_id())
            .unwrap()
            .iter()
            .map(|event| {
                serde_json::to_value(&event.kind).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    };
    assert_eq!(kinds(&off, "store-off"), kinds(&on, "store-on"));
    assert_eq!(
        off.current_turn_verification(),
        on.current_turn_verification()
    );
}
