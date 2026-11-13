//! The task-guidance routing corpus, run end to end.
//!
//! `fixtures/routing-cases.json` is the frozen research corpus (LocalHub,
//! 2026-11-02): Dutch and English requests with synthetic facts and the
//! expected intent, component, loading and supplements. Each case is turned
//! into a real project in a temporary directory, a real session runs with the
//! guidance hook and a scripted provider, and the assertions read the final
//! provider request, the guidance record and the selection — never only a
//! label. Lifecycle cases drive their events: a tool call that moves the work,
//! a compaction and a resume, an exhausted body budget, an injected tool
//! result.
//!
//! The corpus's guidance ids map onto the built-in packages
//! (`rust` -> `guide-rust`, both desktop ids -> `guide-desktop`, ...). Host
//! facts (`host.windows`, `host.linux`) and a pre-existing failing test are
//! recorded by the corpus but change nothing in selection; they are noted, not
//! simulated. Nothing here reads the real home, a user store or the network.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use localpilot_core::{ContentBlock, GuidanceForm, GuidanceKind, Role};
use localpilot_harness::{
    register_task_guidance_context, select_guidance, GuidanceCatalog, GuidanceDelivery,
    GuidanceHookMode, GuidanceSelection, ProjectFacts, RequestGuidance, RuntimeEvent,
    SelectionInput, SessionConfig, SessionRuntime,
};
use localpilot_llm::{FakeProvider, ModelRequest};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_skills::{discovery_roots, SkillSet};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

const CORPUS: &str = include_str!("fixtures/routing-cases.json");
/// The digest the research recorded for the frozen corpus.
const CORPUS_SHA256: &str = "e1a5e61e073d9d6e38ab080c32cd3e4c6c9ac3c81ee8faa76d2682eafe163d62";

fn package_for(id: &str) -> &'static str {
    match id {
        "rust" => "guide-rust",
        "cli" => "guide-cli",
        "library" => "guide-library",
        "service" => "guide-service",
        "dotnet" => "guide-dotnet",
        "web" => "guide-web",
        "threejs" => "guide-threejs",
        "tailwind" => "guide-tailwind",
        "desktop.wpf" | "desktop.avalonia" => "guide-desktop",
        "game" => "guide-game",
        "godot4" => "guide-godot4",
        "manual-release" => "manual-release",
        other => panic!("unmapped corpus guidance id {other}"),
    }
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn under(component: &str, path: &str) -> String {
    if component == "." {
        path.to_string()
    } else {
        format!("{component}/{path}")
    }
}

/// A case's project, home and settings, built from its facts.
struct Built {
    dir: tempfile::TempDir,
    trusted: bool,
    /// A file inside the case's component, recorded as in play, when the
    /// component is not the root.
    in_play: Option<String>,
}

impl Built {
    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }
}

fn build(case: &Value) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let input = &case["input"];
    let component = input["component"].as_str().unwrap();
    let facts: BTreeSet<&str> = input["facts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    let has = |fact: &str| facts.contains(fact);
    let mut trusted = true;

    if has("cargo.workspace") {
        let members = if component == "." {
            String::new()
        } else {
            format!("\"{component}\"")
        };
        write(
            &project,
            "Cargo.toml",
            &format!("[workspace]\nmembers = [{members}]\n"),
        );
    }
    if has("cargo.package")
        || (has("component.library") && !has("cargo.workspace"))
        || (has("cargo.workspace") && component != ".")
    {
        write(
            &project,
            &under(component, "Cargo.toml"),
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n",
        );
    }
    if has("cargo.bin") {
        write(&project, &under(component, "src/main.rs"), "fn main() {}\n");
    }
    if has("component.library") {
        write(&project, &under(component, "src/lib.rs"), "pub fn f() {}\n");
    }
    if has("dotnet.sdk_project")
        && !has("aspnet.project")
        && !has("wpf.project")
        && !has("avalonia.project")
    {
        write(
            &project,
            &under(component, "App.csproj"),
            r#"<Project Sdk="Microsoft.NET.Sdk"></Project>"#,
        );
    }
    if has("aspnet.project") {
        write(
            &project,
            &under(component, "Api.csproj"),
            r#"<Project Sdk="Microsoft.NET.Sdk.Web"></Project>"#,
        );
        write(
            &project,
            &under(component, "Controllers/OrdersController.cs"),
            "class OrdersController {}\n",
        );
    }
    if has("wpf.project") {
        write(
            &project,
            &under(component, "Editor.csproj"),
            r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><UseWPF>true</UseWPF></PropertyGroup></Project>"#,
        );
    }
    if has("avalonia.project") {
        write(
            &project,
            &under(component, "Ui.csproj"),
            r#"<Project Sdk="Microsoft.NET.Sdk"><ItemGroup><PackageReference Include="Avalonia" Version="11.1.0" /></ItemGroup></Project>"#,
        );
    }
    if has("web.project") {
        let mut deps = vec![r#""vite": "^5.4.0""#.to_string()];
        if has("dependency.three") {
            deps.push(r#""three": "0.169.0""#.to_string());
        }
        if has("dependency.tailwind") {
            deps.push(r#""tailwindcss": "^3.4.0""#.to_string());
        }
        write(
            &project,
            &under(component, "package.json"),
            &format!(
                "{{\"name\": \"app\", \"dependencies\": {{{}}}}}",
                deps.join(", ")
            ),
        );
        if has("version.from_lockfile") {
            write(
                &project,
                &under(component, "package-lock.json"),
                r#"{"lockfileVersion": 3, "packages": {"node_modules/tailwindcss": {"version": "3.4.17"}}}"#,
            );
        }
    }
    if has("sibling.web") {
        write(
            &project,
            "web/package.json",
            r#"{"name": "web", "dependencies": {"react": "^18.3.1", "vite": "^5.4.0"}}"#,
        );
        write(
            &project,
            "web/src/Settings.tsx",
            "export const Settings = 1;\n",
        );
    }
    if has("sibling.aspnet") {
        write(
            &project,
            "server/Server.csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk.Web"></Project>"#,
        );
    }
    if has("godot.project") {
        let features = if has("godot.version.4") {
            "PackedStringArray(\"4.3\")"
        } else {
            "PackedStringArray()"
        };
        write(
            &project,
            &under(component, "project.godot"),
            &format!("config_version=5\n[application]\nconfig/features={features}\n"),
        );
    }
    if has("gdscript.scene") {
        write(
            &project,
            &under(component, "main.tscn"),
            "[gd_scene format=3]\n",
        );
        write(
            &project,
            &under(component, "player.gd"),
            "extends CharacterBody2D\n",
        );
    }
    if has("unity.project") {
        write(
            &project,
            &under(component, "ProjectSettings/ProjectVersion.txt"),
            "m_EditorVersion: 2022.3.42f1\n",
        );
    }
    if has("generated.csproj") {
        write(
            &project,
            &under(component, "Assembly-CSharp.csproj"),
            r#"<Project ToolsVersion="4.0"></Project>"#,
        );
    }
    if has("unreal.project") {
        write(
            &project,
            &under(component, "Game.uproject"),
            r#"{"EngineAssociation": "5.4"}"#,
        );
    }
    if has("test.preexisting_failure") {
        write(
            &project,
            &under(component, "tests/failing.rs"),
            "#[test] fn fails() { panic!() }\n",
        );
    }
    if has("catalog.global.rust") {
        write(
            &home,
            ".localpilot/skills/guide-rust/skill.toml",
            "name = \"guide-rust\"\ndescription = \"Global Rust rules\"\nversion = \"2.0.0\"\n\
             [guidance]\nschema_version = 1\nrole = \"supplement\"\nstacks = [\"rust\"]\nbody_sections = [\"#Rules\"]\n",
        );
        write(
            &home,
            ".localpilot/skills/guide-rust/SKILL.md",
            "## Rules\nBODY-GLOBAL-RUST\n",
        );
    }
    if has("catalog.user_only.manual-release") {
        write(
            &home,
            ".localpilot/skills/manual-release/SKILL.md",
            "---\nname: manual-release\ndescription: Publish a release by hand\ndisable-model-invocation: true\n---\nBODY-MANUAL-RELEASE\n",
        );
    }
    if has("project.overlay.untrusted") {
        trusted = false;
        write(
            &project,
            ".localpilot/skills/overlay-rust/skill.toml",
            "name = \"overlay-rust\"\ndescription = \"Overlay Rust rules\"\nversion = \"1.0.0\"\n\
             [guidance]\nschema_version = 1\nrole = \"supplement\"\nstacks = [\"rust\"]\n",
        );
        write(
            &project,
            ".localpilot/skills/overlay-rust/SKILL.md",
            "BODY-UNTRUSTED-OVERLAY\n",
        );
    }
    let in_play = (component != ".").then(|| {
        let candidates = ["Cargo.toml", "Api.csproj", "package.json", "project.godot"];
        candidates
            .iter()
            .map(|file| under(component, file))
            .find(|path| project.join(path).is_file())
            .unwrap_or_else(|| {
                let path = under(component, "README.md");
                write(&project, &path, "component\n");
                path
            })
    });
    Built {
        dir,
        trusted,
        in_play,
    }
}

fn mode_of(case: &Value) -> Option<GuidanceHookMode> {
    match case["input"]["mode"].as_str().unwrap() {
        "assisted" => Some(GuidanceHookMode::Assisted),
        "suggest" => Some(GuidanceHookMode::Suggest),
        "off" => None,
        other => panic!("unknown mode {other}"),
    }
}

fn capabilities_of(case: &Value) -> BTreeSet<String> {
    case["input"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect()
}

fn runtime(
    built: &Built,
    case: &Value,
    provider: Arc<FakeProvider>,
    store: &str,
) -> SessionRuntime {
    let mut runtime = SessionRuntime::new(
        provider as Arc<dyn localpilot_llm::ModelProvider>,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(&built.dir.path().join(store)),
        Workspace::new(&built.project()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            trusted: built.trusted,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    if let Some(path) = &built.in_play {
        let absolute = built.project().join(path);
        runtime.paths_in_play().record(&built.project(), &absolute);
    }
    let catalog = GuidanceCatalog {
        root: built.project(),
        home: Some(built.home()),
        trusted: runtime.trust_flag(),
    };
    register_task_guidance_context(mode_of(case), catalog, capabilities_of(case), &mut runtime);
    runtime
}

async fn turn(runtime: &mut SessionRuntime, prompt: &str) {
    let (events, _rx) = broadcast::channel::<RuntimeEvent>(256);
    runtime
        .run_turn(prompt, &events, &CancellationToken::new())
        .await;
}

/// What a case's run produced.
struct Run {
    selection: Option<GuidanceSelection>,
    record: RequestGuidance,
    request: ModelRequest,
    tools: Vec<String>,
    /// Requested evidence whose state claims more than its route allows.
    unsupported_passes: Vec<String>,
    /// Whether a completion check ran in the last turn.
    check_ran: bool,
}

/// Requested evidence that is not reported unavailable although the session
/// has no route for it.
fn unsupported_passes(runtime: &SessionRuntime) -> Vec<String> {
    let routes = runtime.observation_routes();
    runtime
        .requested_evidence()
        .into_iter()
        .filter(|requested| {
            let available = routes.iter().any(|route| {
                route.capability == requested.capability
                    && matches!(
                        route.status,
                        localpilot_harness::RouteStatus::Available(_)
                            | localpilot_harness::RouteStatus::Hidden(_)
                    )
            });
            !available && requested.state != localpilot_harness::EvidenceState::Unavailable
        })
        .map(|requested| requested.capability)
        .collect()
}

/// Run `case` through a real session, driving its lifecycle events.
async fn run(case: &Value, built: &Built) -> Run {
    let request = case["request"].as_str().unwrap();
    let events: Vec<&str> = case["events"]
        .as_array()
        .map(|events| events.iter().map(|e| e["type"].as_str().unwrap()).collect())
        .unwrap_or_default();
    let mut provider = FakeProvider::new();
    if events.contains(&"component_changed") {
        let to = case["events"][0]["to"].as_str().unwrap();
        provider = provider.tool_call(
            "move-1",
            "read_file",
            json!({"path": format!("{to}/package.json")}),
        );
    }
    if events.contains(&"tool_result") {
        let text = case["events"][0]["text"].as_str().unwrap();
        let component = case["input"]["component"].as_str().unwrap();
        write(&built.project(), &under(component, "notes.txt"), text);
        provider = provider.tool_call(
            "inject-1",
            "read_file",
            json!({"path": under(component, "notes.txt")}),
        );
    }
    if events.contains(&"guidance_body_budget_exhausted") {
        for name in ["guide-investigate", "guide-rust", "guide-cli"] {
            let (role, applies) = match name {
                "guide-investigate" => ("procedure", "intents = [\"investigate\"]"),
                "guide-rust" => ("supplement", "stacks = [\"rust\"]"),
                _ => ("supplement", "products = [\"cli\"]"),
            };
            write(
                &built.home(),
                &format!(".localpilot/skills/{name}/skill.toml"),
                &format!(
                    "name = \"{name}\"\ndescription = \"{name} long\"\nversion = \"1.0.0\"\n\
                     [guidance]\nschema_version = 1\nrole = \"{role}\"\n{applies}\n"
                ),
            );
            write(
                &built.home(),
                &format!(".localpilot/skills/{name}/SKILL.md"),
                &"word ".repeat(2_000),
            );
        }
    }
    let provider = Arc::new(provider.text("ok").text("ok").text("summary").text("ok"));
    let mut session = runtime(built, case, Arc::clone(&provider), "store");
    turn(&mut session, request).await;
    if events.contains(&"compact") {
        session.compact_conversation_force().await;
    }
    if events.contains(&"resume") {
        let id = session.session_id();
        drop(session);
        let resumed_provider = Arc::new(FakeProvider::new().text("ok"));
        let mut resumed = runtime(built, case, Arc::clone(&resumed_provider), "store");
        resumed.load_session(id).unwrap();
        turn(&mut resumed, request).await;
        let record = resumed.task_guidance().last().cloned().unwrap();
        return Run {
            selection: resumed.guidance_selection(),
            record,
            request: resumed_provider.requests().pop().unwrap(),
            tools: resumed.tool_names(),
            unsupported_passes: unsupported_passes(&resumed),
            check_ran: resumed.current_turn_verification().is_some(),
        };
    }
    let record = session.task_guidance().last().cloned().unwrap();
    Run {
        selection: session.guidance_selection(),
        record,
        request: provider.requests().pop().unwrap(),
        tools: session.tool_names(),
        unsupported_passes: unsupported_passes(&session),
        check_ran: session.current_turn_verification().is_some(),
    }
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

fn is_procedure(name: &str) -> bool {
    matches!(
        name,
        "guide-explain"
            | "guide-investigate"
            | "guide-implement"
            | "guide-refactor"
            | "guide-review"
            | "guide-measure"
    )
}

fn package_entries(record: &RequestGuidance, form: GuidanceForm) -> BTreeSet<String> {
    record
        .entries
        .iter()
        .filter(|e| e.source.kind == GuidanceKind::PackageSkill && e.source.form == form)
        .filter(|e| e.delivery == GuidanceDelivery::Intact)
        .map(|e| e.source.id.clone())
        .collect()
}

fn mapped(case: &Value, key: &str) -> BTreeSet<String> {
    case["expected"][key]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| package_for(id.as_str().unwrap()).to_string())
        .collect()
}

/// Check one case; returns the problems found.
async fn check(case: &Value) -> Vec<String> {
    let id = case["id"].as_str().unwrap();
    let expected = &case["expected"];
    let built = build(case);
    let outcome = run(case, &built).await;
    let mut problems = Vec::new();
    let mut fail = |problem: String| problems.push(format!("{id}: {problem}"));

    // Hard invariants for every case.
    let baseline = SessionRuntime::new(
        Arc::new(FakeProvider::new()) as Arc<dyn localpilot_llm::ModelProvider>,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(&built.dir.path().join("baseline")),
        Workspace::new(&built.project()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    );
    if outcome.tools != baseline.tool_names() {
        fail("authority changed: the tool set differs from a session without guidance".into());
    }
    let text = prose(&outcome.request);
    for marker in ["BODY-MANUAL-RELEASE", "BODY-UNTRUSTED-OVERLAY"] {
        if text.contains(marker) {
            fail(format!("{marker} reached the request"));
        }
    }
    let bodies = package_entries(&outcome.record, GuidanceForm::Body);
    let metadata = package_entries(&outcome.record, GuidanceForm::Metadata);
    // No duplicate guidance: each loaded body is in the request exactly once.
    for name in &bodies {
        let copies = text.matches(&format!("### {name}\n")).count();
        if copies != 1 {
            fail(format!("{name} is in the request {copies} times"));
        }
    }
    // No unsupported pass claim, and guidance adds no check.
    if !outcome.unsupported_passes.is_empty() {
        fail(format!(
            "evidence without a route is not reported unavailable: {:?}",
            outcome.unsupported_passes
        ));
    }
    if outcome.check_ran {
        fail("a completion check ran although nothing was changed".into());
    }

    let intent = expected["intent"].as_str();
    match intent {
        // Guidance off: nothing new in the request or the record.
        None => {
            if !outcome.record.entries.is_empty()
                || outcome.selection.is_some()
                || text.contains("Task guidance")
            {
                fail("guidance off still produced guidance".into());
            }
            return problems;
        }
        Some("answer") => {
            let selection = outcome.selection.as_ref().unwrap();
            if selection.primary.is_some() || !bodies.is_empty() {
                fail(format!(
                    "a development procedure was chosen for a non-development question: {:?}",
                    selection.primary
                ));
            }
            return problems;
        }
        Some("explicit") => {
            // Automatic selection never reaches the user-only package...
            if bodies.contains("manual-release") || metadata.contains("manual-release") {
                fail("the user-only package was chosen automatically".into());
            }
            // ...and the explicit path does.
            let set = SkillSet::resolve(&discovery_roots(
                &built.project(),
                Some(&built.home()),
                built.trusted,
            ))
            .unwrap()
            .with_builtin_guidance();
            let mut facts = ProjectFacts::with_tool_lookup(built.project(), Box::new(|_| false));
            let selection = select_guidance(
                &SelectionInput {
                    request: case["request"].as_str().unwrap(),
                    explicit: Some("manual-release"),
                    paths: &[],
                    capabilities: &capabilities_of(case),
                    earlier: &[],
                },
                &mut facts,
                &set,
            );
            if selection.primary.as_ref().map(|p| p.name.as_str()) != Some("manual-release") {
                fail(format!(
                    "explicit selection did not reach manual-release: {:?}",
                    selection.primary
                ));
            }
            return problems;
        }
        Some(_) => {}
    }

    let Some(selection) = outcome.selection else {
        fail("no selection was recorded".into());
        return problems;
    };
    let got_intent = selection.intent.primary().map(|i| i.as_str());
    if got_intent != intent {
        fail(format!(
            "intent {got_intent:?}, expected {intent:?} ({})",
            selection.intent.reason
        ));
    }
    let component = expected["component"].as_str().unwrap();
    if selection.component.component != component {
        fail(format!(
            "component {}, expected {component}",
            selection.component.component
        ));
    }
    let required = mapped(case, "required_supplements");
    // Two corpus ids share `guide-desktop`; an id forbidden here while its
    // package is required stands for the other framework, not the package.
    let forbidden: BTreeSet<String> = mapped(case, "forbidden_supplements")
        .difference(&required)
        .cloned()
        .collect();
    // A supplement whose body did not fit the budget is still recommended,
    // by name.
    let supplements: BTreeSet<String> = selection
        .supplements
        .iter()
        .chain(
            selection
                .suggestions
                .iter()
                .filter(|s| s.reason.contains("does not fit the guidance budget")),
        )
        .filter(|s| !is_procedure(&s.name))
        .map(|s| s.name.clone())
        .collect();
    if supplements != required {
        fail(format!(
            "supplements {supplements:?}, expected {required:?}; excluded {:?}",
            selection.excluded
        ));
    }
    for name in &forbidden {
        if supplements.contains(name) || bodies.contains(name) {
            fail(format!("forbidden guidance {name} was selected or loaded"));
        }
    }
    match expected["loading"].as_str().unwrap() {
        "body" => {
            let primary = selection.primary.as_ref().map(|p| p.name.clone());
            let mut wanted = required.clone();
            wanted.extend(primary);
            if bodies != wanted {
                fail(format!(
                    "bodies in the request {bodies:?}, expected {wanted:?} ({})",
                    selection.loading_reason
                ));
            }
        }
        "metadata" => {
            if !bodies.is_empty() {
                fail(format!("bodies loaded in a metadata case: {bodies:?}"));
            }
            for name in &required {
                if !metadata.contains(name) {
                    fail(format!("{name} was not recommended by name"));
                }
            }
        }
        other => fail(format!("unexpected loading {other}")),
    }
    problems
}

#[test]
fn the_corpus_is_the_frozen_research_corpus() {
    let normalized = CORPUS.replace("\r\n", "\n");
    let digest = format!("{:x}", Sha256::digest(normalized.as_bytes()));
    assert_eq!(digest, CORPUS_SHA256);
    let corpus: Value = serde_json::from_str(CORPUS).unwrap();
    assert_eq!(corpus["cases"].as_array().unwrap().len(), 30);
}

#[tokio::test]
async fn every_routing_and_lifecycle_case_holds_end_to_end() {
    let corpus: Value = serde_json::from_str(CORPUS).unwrap();
    let mut problems = Vec::new();
    for case in corpus["cases"].as_array().unwrap() {
        problems.extend(check(case).await);
    }
    assert!(
        problems.is_empty(),
        "{} problem(s):\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// Held-out paraphrases, written separately from the corpus and not used to
/// tune selection. Misses are reported, not hidden; the hard invariants fail
/// the test.
#[tokio::test]
async fn held_out_paraphrases_report_their_misses() {
    let cases = [
        (
            "Waarom geeft het programma exit 0 bij een negatieve telling?",
            &["cargo.package", "cargo.bin"][..],
            "investigate",
            &["rust", "cli"][..],
        ),
        (
            "Can you walk me through the argument parsing?",
            &["cargo.package", "cargo.bin"][..],
            "explain",
            &["rust"][..],
        ),
        (
            "Restructure the math crate's internals, keep its API.",
            &["cargo.package", "component.library"][..],
            "refactor",
            &["rust", "library"][..],
        ),
        (
            "Implementeer een endpoint voor annuleren.",
            &["aspnet.project"][..],
            "implement",
            &["dotnet", "service"][..],
        ),
        (
            "Beschrijf wat de controller doet.",
            &["aspnet.project"][..],
            "explain",
            &["dotnet"][..],
        ),
        (
            "Debug why the binding stays empty.",
            &["dotnet.sdk_project", "avalonia.project"][..],
            "investigate",
            &["dotnet", "desktop.avalonia"][..],
        ),
        (
            "Repareer de botsing van de speler.",
            &["godot.project", "godot.version.4"][..],
            "investigate",
            &["game", "godot4"][..],
        ),
        (
            "Benchmark the parser against the old one.",
            &["cargo.package", "component.library"][..],
            "measure",
            &["rust", "library"][..],
        ),
        (
            "Kijk deze pull request na.",
            &["cargo.package"][..],
            "review",
            &["rust"][..],
        ),
        (
            "Make the orbit controls smoother.",
            &["web.project", "dependency.three"][..],
            "implement",
            &["web", "threejs"][..],
        ),
    ];
    let mut misses = Vec::new();
    for (request, facts, intent, supplements) in cases {
        let case = json!({
            "id": "H",
            "request": request,
            "input": {"mode": "assisted", "facts": facts, "component": ".", "capabilities": ["process.capture"]},
            "expected": {"intent": intent, "component": ".", "loading": "body", "required_supplements": supplements, "forbidden_supplements": []},
        });
        let problems = check(&case).await;
        for problem in &problems {
            assert!(
                !problem.contains("authority changed") && !problem.contains("reached the request"),
                "hard invariant: {problem}"
            );
        }
        if !problems.is_empty() {
            misses.push(format!("{request}: {}", problems.join("; ")));
        }
    }
    eprintln!(
        "held-out paraphrases: {} of {} matched",
        10 - misses.len(),
        10
    );
    for miss in &misses {
        eprintln!("  miss: {miss}");
    }
}
