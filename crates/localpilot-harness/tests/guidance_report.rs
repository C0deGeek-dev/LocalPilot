//! The guidance report: active, stale, suggested, excluded and unavailable
//! guidance, read from a selection and the last provider request.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use localpilot_core::{GuidanceForm, GuidanceKind, GuidanceSource};
use localpilot_harness::{
    select_guidance, ContextContribution, ContextHook, ContextPlacement, GuidanceSegment,
    GuidanceStatus, ProjectFacts, RuntimeEvent, SelectionInput, SessionConfig, SessionRuntime,
};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_skills::{discovery_roots, SkillSet};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn guided(home: &Path, name: &str, guidance: &str) {
    write(
        home,
        &format!(".localpilot/skills/{name}/skill.toml"),
        &format!(
            "name = \"{name}\"\ndescription = \"{name} guidance for exit codes\"\nversion = \"2.0.0\"\n\
             [guidance]\nschema_version = 1\n{guidance}\n"
        ),
    );
    write(
        home,
        &format!(".localpilot/skills/{name}/SKILL.md"),
        "Body.\n",
    );
}

/// Contributes the investigate procedure's body, as assisted loading will.
struct ProcedureBody;

impl ContextHook for ProcedureBody {
    fn name(&self) -> &str {
        "procedure-body"
    }
    fn placement(&self) -> ContextPlacement {
        ContextPlacement::System
    }
    fn context_for(&self, _prompt: &str) -> Option<String> {
        Some("Investigate: reproduce the input first.".to_string())
    }
    fn contribute(&self, prompt: &str) -> ContextContribution {
        ContextContribution {
            text: self.context_for(prompt),
            memories: Vec::new(),
            guidance: vec![GuidanceSegment {
                source: GuidanceSource::new(
                    GuidanceKind::PackageSkill,
                    GuidanceForm::Body,
                    "investigate",
                    "global (.localpilot) v2.0.0",
                    "selected for the intent investigate",
                ),
                text: "Investigate: reproduce the input first.".to_string(),
            }],
        }
    }
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        write(
            &project,
            "Cargo.toml",
            "[package]\nname = \"c\"\nversion = \"0.1.0\"\n",
        );
        write(&project, "src/main.rs", "fn main() {}\n");
        write(&project, "AGENTS.md", "Keep exit codes documented.\n");
        guided(
            &home,
            "investigate",
            "role = \"procedure\"\nintents = [\"investigate\"]",
        );
        guided(
            &home,
            "implement",
            "role = \"procedure\"\nintents = [\"implement\"]",
        );
        guided(&home, "rust", "role = \"supplement\"\nstacks = [\"rust\"]");
        guided(
            &home,
            "cli",
            "role = \"supplement\"\nproducts = [\"cli\"]\ncapabilities_required_for_claim = [\"process.capture\"]",
        );
        guided(&home, "web", "role = \"supplement\"\nstacks = [\"web\"]");
        Self { dir }
    }

    fn runtime(&self, provider: Arc<FakeProvider>) -> SessionRuntime {
        let project = self.dir.path().join("project");
        let mut runtime = SessionRuntime::new(
            provider as Arc<dyn localpilot_llm::ModelProvider>,
            ToolRegistry::with_builtins(),
            PermissionEngine::new(Profile::Default, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::open(&self.dir.path().join("store")),
            Workspace::new(&project).unwrap(),
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig::default(),
            Vec::new(),
        );
        let context = localpilot_config::ContextDiscovery::new(&project)
            .with_global_dir(None)
            .discover();
        let paths = runtime.paths_in_play();
        runtime.hooks_mut().register_context_hook(Arc::new(
            localpilot_harness::ProjectInstructionsContext::with_context(
                &project, context, 8_000, paths,
            ),
        ));
        runtime
    }

    fn selection(&self, request: &str) -> localpilot_harness::GuidanceSelection {
        let project = self.dir.path().join("project");
        let home = self.dir.path().join("home");
        let set = SkillSet::resolve(&discovery_roots(&project, Some(&home), true)).unwrap();
        let mut facts = ProjectFacts::with_tool_lookup(&project, Box::new(|_| false));
        select_guidance(
            &SelectionInput {
                request,
                explicit: None,
                paths: &[],
                capabilities: &BTreeSet::new(),
                earlier: &[],
            },
            &mut facts,
            &set,
        )
    }
}

async fn turn(runtime: &mut SessionRuntime, prompt: &str) {
    let (events, _rx) = broadcast::channel::<RuntimeEvent>(64);
    runtime
        .run_turn(prompt, &events, &CancellationToken::new())
        .await;
}

#[tokio::test]
async fn the_report_says_what_reached_the_request_and_why_the_rest_did_not() {
    let fixture = Fixture::new();
    let provider = Arc::new(FakeProvider::new().text("ok"));
    let mut runtime = fixture.runtime(Arc::clone(&provider));
    runtime
        .hooks_mut()
        .register_context_hook(Arc::new(ProcedureBody));
    let request = "Fix the wrong exit code.";
    runtime.set_guidance_selection(Some(fixture.selection(request)));
    turn(&mut runtime, request).await;

    let report = runtime.guidance_report();
    let text = report.render();

    // Selected and in the request.
    let active = report.row("investigate", GuidanceStatus::Active).unwrap();
    assert!(
        active
            .detail
            .contains("procedure for the intent investigate"),
        "{text}"
    );
    assert!(active.detail.contains("v2.0.0"), "{text}");
    assert!(active.detail.contains("in the system text"), "{text}");
    // Selected, but nothing put it in the request.
    assert!(report
        .row("rust", GuidanceStatus::Stale)
        .unwrap()
        .detail
        .contains("not in the last request"));
    // A claim the CLI guidance supports needs a capability this session lacks.
    assert!(report
        .row("cli", GuidanceStatus::Unavailable)
        .unwrap()
        .detail
        .contains("process.capture"));
    // Left out, with the reason.
    assert!(report
        .row("web", GuidanceStatus::Excluded)
        .unwrap()
        .detail
        .contains("shows no stack web"));
    assert!(report
        .row("implement", GuidanceStatus::Excluded)
        .unwrap()
        .detail
        .contains("other intents"));
    // Guidance that arrived by another route is listed from the request record.
    assert!(report
        .row("AGENTS.md", GuidanceStatus::Active)
        .unwrap()
        .detail
        .contains("applies to every turn"));
    assert!(text.starts_with("guidance: 2 active"), "{text}");
    assert!(
        text.contains("intent: investigate → implement (confirmed"),
        "{text}"
    );
    assert!(text.contains("component: . (confirmed"), "{text}");
    // The provider's request does hold what the report calls active.
    let sent = provider.requests();
    let system: String = sent[0]
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            localpilot_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(system.contains("Investigate: reproduce the input first."));
    assert!(!system.contains("rust guidance"));
}

#[tokio::test]
async fn without_a_selection_the_report_lists_what_the_request_held() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime(Arc::new(FakeProvider::new().text("ok")));
    turn(&mut runtime, "hello").await;

    let report = runtime.guidance_report();

    assert!(report.summary.contains("no task selection ran"));
    assert!(report.row("AGENTS.md", GuidanceStatus::Active).is_some());
    assert!(report.header.is_empty());
}

#[tokio::test]
async fn reading_the_report_changes_nothing() {
    let fixture = Fixture::new();
    let provider = Arc::new(FakeProvider::new().text("ok").text("ok"));
    let mut runtime = fixture.runtime(Arc::clone(&provider));
    runtime.set_guidance_selection(Some(fixture.selection("Fix the exit code.")));
    turn(&mut runtime, "Fix the exit code.").await;

    let before = runtime.task_guidance();
    let first = runtime.guidance_report();
    let second = runtime.guidance_report();
    assert_eq!(first, second);
    assert_eq!(runtime.task_guidance(), before);
    assert_eq!(provider.requests().len(), 1);

    // The next request is assembled exactly as it would have been.
    turn(&mut runtime, "Fix the exit code.").await;
    let sent = provider.requests();
    assert_eq!(sent[0].messages.len(), 2);
    assert_eq!(sent[0].tools.len(), sent[1].tools.len());
}
