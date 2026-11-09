//! What task guidance a session offered, and what the provider was handed.
//!
//! Each test builds a synthetic project and a synthetic home in one temporary
//! directory, drives a real session with a scripted provider and the real
//! skill tools, and compares two things that are produced independently: the
//! session's guidance record, and the requests the provider received. A record
//! that says a body was delivered is only believed when the body is in the
//! request.
//!
//! Nothing here reads the host's home directory, a user store, a credential or
//! the network: the global skill baseline and the instruction discovery are
//! given the fixture's own directories, and the provider is in memory.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use localpilot_config::ContextDiscovery;
use localpilot_core::{ContentBlock, GuidanceForm, GuidanceKind, Role};
use localpilot_harness::{
    ContextHook, GuidanceDelivery, GuidanceEntry, GuidancePlacement, PathsInPlay,
    ProjectInstructionsContext, RequestGuidance, RuntimeEvent, SessionConfig, SessionRuntime,
    StopReason,
};
use localpilot_llm::{FakeProvider, ModelRequest};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_skills::{SkillList, SkillLoad, SkillSearch};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// The character budget the instruction hook gets unless a test narrows it.
const INSTRUCTION_BUDGET: usize = 8_000;

/// A synthetic project and home, side by side in one temporary directory.
struct Fixture {
    dir: tempfile::TempDir,
    /// Register the skill tools, as `skills.autonomous_discovery` does.
    discovery: bool,
    trusted: bool,
    instruction_budget: usize,
    discovery_cue: bool,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        Self {
            dir,
            discovery: true,
            trusted: true,
            instruction_budget: INSTRUCTION_BUDGET,
            discovery_cue: false,
        }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn file(&self, relative: &str, contents: &str) -> &Self {
        write(&self.project().join(relative), contents);
        self
    }

    fn project_skill(&self, name: &str, description: &str, body: &str) -> &Self {
        write_skill(&self.project(), name, description, body, false);
        self
    }

    fn global_skill(&self, name: &str, description: &str, body: &str) -> &Self {
        write_skill(&self.home(), name, description, body, false);
        self
    }

    fn user_only_skill(&self, name: &str, description: &str, body: &str) -> &Self {
        write_skill(&self.project(), name, description, body, true);
        self
    }

    /// The instruction hook over this project, with no global tier.
    fn instructions(&self, paths_in_play: PathsInPlay) -> ProjectInstructionsContext {
        let root = self.project();
        let context = ContextDiscovery::new(&root)
            .with_global_dir(None)
            .discover();
        ProjectInstructionsContext::with_context(
            &root,
            context,
            self.instruction_budget,
            paths_in_play,
        )
    }

    /// Run one turn per prompt and return what was recorded and what was sent.
    async fn run(&self, provider: FakeProvider, prompts: &[&str]) -> Outcome {
        let provider = Arc::new(provider);
        let mut tools = ToolRegistry::with_builtins();
        if self.discovery {
            tools.register(Box::new(SkillList::with_home(Some(self.home()))));
            tools.register(Box::new(SkillSearch::with_home(Some(self.home()))));
            tools.register(Box::new(SkillLoad::with_home(Some(self.home()))));
        }
        let config = SessionConfig {
            trusted: self.trusted,
            package_discovery_disabled_but_present: self.discovery_cue,
            ..SessionConfig::default()
        };
        let mut runtime = SessionRuntime::new(
            Arc::clone(&provider) as Arc<dyn localpilot_llm::ModelProvider>,
            tools,
            PermissionEngine::new(Profile::Default, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::open(&self.dir.path().join("store")),
            Workspace::new(&self.project()).unwrap(),
            RecoveryEngine::new(RecoveryBudget::default()),
            config,
            Vec::new(),
        );
        let hook = self.instructions(runtime.paths_in_play());
        if hook.has_instructions() {
            runtime.hooks_mut().register_context_hook(Arc::new(hook));
        }
        let (events, _rx) = broadcast::channel::<RuntimeEvent>(256);
        let cancel = CancellationToken::new();
        for prompt in prompts {
            let stop = runtime.run_turn(prompt, &events, &cancel).await;
            assert_eq!(stop, StopReason::Done, "turn for {prompt:?}");
        }
        let outcome = Outcome {
            recorded: runtime.task_guidance(),
            sent: provider.requests(),
        };
        assert_eq!(
            outcome.recorded.len(),
            outcome.sent.len(),
            "one record per request the provider received"
        );
        outcome
    }
}

/// The session's record beside the requests the provider received.
struct Outcome {
    recorded: Vec<RequestGuidance>,
    sent: Vec<ModelRequest>,
}

impl Outcome {
    /// The record's entry for source `id` in request `index`.
    fn entry(&self, index: usize, id: &str) -> &GuidanceEntry {
        self.find(index, id)
            .unwrap_or_else(|| panic!("no entry {id:?} in {:?}", self.recorded[index]))
    }

    fn find(&self, index: usize, id: &str) -> Option<&GuidanceEntry> {
        self.recorded[index]
            .entries
            .iter()
            .find(|entry| entry.source.id == id)
    }

    /// The system and user text of request `index`.
    fn prose(&self, index: usize) -> String {
        text_of(&self.sent[index], |role| {
            matches!(role, Role::System | Role::User)
        })
    }

    /// The tool-result text of request `index`.
    fn tool_results(&self, index: usize) -> String {
        self.sent[index]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result.output.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn text_of(request: &ModelRequest, wanted: impl Fn(Role) -> bool) -> String {
    request
        .messages
        .iter()
        .filter(|message| wanted(message.role))
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn write_skill(root: &Path, name: &str, description: &str, body: &str, user_only: bool) {
    let flag = if user_only {
        "disable-model-invocation: true\n"
    } else {
        ""
    };
    write(
        &root
            .join(".localpilot")
            .join("skills")
            .join(name)
            .join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: {description}\n{flag}---\n\n{body}\n"),
    );
}

const SCOPED_RULE: &str = ".github/instructions/web.instructions.md";

#[tokio::test]
async fn an_instruction_file_is_recorded_with_its_reason_and_is_in_the_request() {
    let fixture = Fixture::new();
    fixture
        .file("AGENTS.md", "Indent with four spaces in this repository.")
        .file(
            SCOPED_RULE,
            "---\napplyTo: \"**/*.ts\"\n---\nPrefer named exports in TypeScript.",
        )
        .file("src/app.ts", "export const a = 1;\n");

    let outcome = fixture
        .run(
            FakeProvider::new().text("ok"),
            &["tighten the types in src/app.ts"],
        )
        .await;

    let always = outcome.entry(0, "AGENTS.md");
    assert_eq!(always.source.kind, GuidanceKind::ProjectInstructions);
    assert_eq!(always.source.form, GuidanceForm::Body);
    assert_eq!(always.source.origin, "repo-root");
    assert_eq!(always.source.reason, "applies to every turn");
    assert_eq!(always.placement, GuidancePlacement::System);
    assert_eq!(always.delivery, GuidanceDelivery::Intact);

    let scoped = outcome.entry(0, SCOPED_RULE);
    assert_eq!(scoped.source.reason, "applyTo **/*.ts matched src/app.ts");
    assert_eq!(scoped.delivery, GuidanceDelivery::Intact);

    // The request, read on its own, holds both bodies as system text.
    let system = text_of(&outcome.sent[0], |role| role == Role::System);
    assert!(system.contains("Indent with four spaces in this repository."));
    assert!(system.contains("Prefer named exports in TypeScript."));
}

#[tokio::test]
async fn a_scoped_rule_that_matches_nothing_is_neither_recorded_nor_sent() {
    let fixture = Fixture::new();
    fixture
        .file("AGENTS.md", "Indent with four spaces in this repository.")
        .file(
            SCOPED_RULE,
            "---\napplyTo: \"**/*.ts\"\n---\nPrefer named exports in TypeScript.",
        );

    let outcome = fixture
        .run(FakeProvider::new().text("ok"), &["what does this repo do?"])
        .await;

    assert!(outcome.find(0, SCOPED_RULE).is_none());
    assert!(!outcome.prose(0).contains("Prefer named exports"));
    assert_eq!(
        outcome.entry(0, "AGENTS.md").delivery,
        GuidanceDelivery::Intact
    );
}

#[tokio::test]
async fn an_instruction_file_cut_by_the_budget_is_recorded_as_cut() {
    let mut fixture = Fixture::new();
    let long_rule = format!(
        "Keep modules small. {}",
        "Explain every public item. ".repeat(40)
    );
    fixture
        .file("AGENTS.md", "Indent with four spaces in this repository.")
        .file(".github/copilot-instructions.md", &long_rule);
    // Room for the first file and the opening of the second, not its end.
    fixture.instruction_budget = 400;

    let outcome = fixture
        .run(FakeProvider::new().text("ok"), &["hello"])
        .await;

    assert_eq!(
        outcome.entry(0, "AGENTS.md").delivery,
        GuidanceDelivery::Intact
    );
    let cut = outcome.entry(0, ".github/copilot-instructions.md");
    assert_eq!(cut.delivery, GuidanceDelivery::Altered);
    // Independently: the request has the start of that file and not all of it.
    let prose = outcome.prose(0);
    assert!(prose.contains("Keep modules small."));
    assert!(!prose.contains(long_rule.trim_end()));
    assert!(cut.content_chars > 400);
}

#[tokio::test]
async fn a_search_is_recorded_as_metadata_and_a_load_as_the_body() {
    let fixture = Fixture::new();
    fixture
        .project_skill(
            "release-notes",
            "Write release notes from merged changes",
            "Group the entries under Added, Changed and Fixed. MARKER-RELEASE-BODY",
        )
        .global_skill(
            "release-audit",
            "Audit a release branch before tagging",
            "Compare the tag with the changelog. MARKER-AUDIT-BODY",
        );

    let provider = FakeProvider::new()
        .tool_call(
            "call-search",
            "skill_search",
            json!({"query": "release notes"}),
        )
        .tool_call("call-load", "skill_load", json!({"name": "release-notes"}))
        .text("done");
    let outcome = fixture.run(provider, &["write the release notes"]).await;

    // Request 0: nothing has been asked for yet.
    assert!(outcome.recorded[0].entries.is_empty());

    // Request 1: the search result. Names and summaries, no body.
    let found = outcome.entry(1, "release-notes");
    assert_eq!(found.source.kind, GuidanceKind::PackageSkill);
    assert_eq!(found.source.form, GuidanceForm::Metadata);
    assert_eq!(found.source.origin, "project (.localpilot) v0.0.0");
    assert!(
        found
            .source
            .reason
            .starts_with("matched the model's search \"release notes\" with score "),
        "{}",
        found.source.reason
    );
    assert_eq!(found.placement, GuidancePlacement::ToolResult);
    assert_eq!(found.delivery, GuidanceDelivery::Intact);
    // The fixture home is the global baseline: its skill is found, as global.
    assert_eq!(
        outcome.entry(1, "release-audit").source.origin,
        "global (.localpilot) v0.0.0"
    );
    let results = outcome.tool_results(1);
    assert!(results.contains("Write release notes from merged changes"));
    assert!(!results.contains("MARKER-RELEASE-BODY"));

    // Request 2: the load. The body is recorded and is in the request; the
    // earlier search result is still there, and still only metadata.
    let loaded: Vec<_> = outcome.recorded[2]
        .entries
        .iter()
        .filter(|entry| entry.source.id == "release-notes")
        .collect();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].source.form, GuidanceForm::Metadata);
    assert_eq!(loaded[1].source.form, GuidanceForm::Body);
    assert_eq!(loaded[1].source.reason, "the model loaded it by name");
    assert_eq!(loaded[1].delivery, GuidanceDelivery::Intact);
    let results = outcome.tool_results(2);
    assert!(results.contains("MARKER-RELEASE-BODY"));
    // The other skill was only ever a search hit.
    assert!(!results.contains("MARKER-AUDIT-BODY"));
    assert!(outcome.recorded[2]
        .entries
        .iter()
        .all(|entry| entry.source.id != "release-audit"
            || entry.source.form == GuidanceForm::Metadata));
}

#[tokio::test]
async fn a_listing_records_each_listed_package_and_no_user_only_one() {
    let fixture = Fixture::new();
    fixture
        .project_skill("lint-fix", "Fix lint findings", "Run the linter first.")
        .user_only_skill("ship-it", "Publish a release", "MARKER-USER-ONLY-BODY");

    let provider = FakeProvider::new()
        .tool_call("call-list", "skill_list", json!({}))
        .tool_call(
            "call-search",
            "skill_search",
            json!({"query": "publish a release"}),
        )
        .text("done");
    let outcome = fixture.run(provider, &["what skills are there?"]).await;

    let listed = outcome.entry(1, "lint-fix");
    assert_eq!(listed.source.form, GuidanceForm::Metadata);
    assert_eq!(listed.source.reason, "the model listed the catalog");
    for request in &outcome.recorded {
        assert!(
            request
                .entries
                .iter()
                .all(|entry| entry.source.id != "ship-it"),
            "a user-only package was recorded: {request:?}"
        );
    }
    for index in 0..outcome.sent.len() {
        assert!(!outcome
            .tool_results(index)
            .contains("MARKER-USER-ONLY-BODY"));
        assert!(!outcome.tool_results(index).contains("Publish a release"));
    }
}

#[tokio::test]
async fn an_untrusted_workspace_records_no_project_skill() {
    let mut fixture = Fixture::new();
    fixture.project_skill("lint-fix", "Fix lint findings", "Run the linter first.");
    fixture.global_skill("format-all", "Format every file", "Run the formatter.");
    fixture.trusted = false;

    let provider = FakeProvider::new()
        .tool_call("call-list", "skill_list", json!({}))
        .text("done");
    let outcome = fixture.run(provider, &["what skills are there?"]).await;

    assert!(outcome.find(1, "lint-fix").is_none());
    assert!(!outcome.tool_results(1).contains("Fix lint findings"));
    assert_eq!(
        outcome.entry(1, "format-all").source.origin,
        "global (.localpilot) v0.0.0"
    );
}

#[tokio::test]
async fn with_discovery_off_no_skill_reaches_the_request_or_the_record() {
    let mut fixture = Fixture::new();
    fixture.project_skill("lint-fix", "Fix lint findings", "MARKER-LINT-BODY");
    fixture.discovery = false;
    fixture.discovery_cue = true;

    let outcome = fixture
        .run(FakeProvider::new().text("ok"), &["fix the lint findings"])
        .await;

    // The request advertises no skill tool and carries no skill text.
    let request = &outcome.sent[0];
    assert!(request
        .tools
        .iter()
        .all(|tool| !tool.name.starts_with("skill_")));
    assert!(!outcome.prose(0).contains("MARKER-LINT-BODY"));
    assert!(!outcome.prose(0).contains("Fix lint findings"));
    // What it does carry is the host's sentence that packages exist, and the
    // record names exactly that.
    let kinds: Vec<_> = outcome.recorded[0]
        .entries
        .iter()
        .map(|entry| (entry.source.kind, entry.delivery))
        .collect();
    assert_eq!(
        kinds,
        [(GuidanceKind::HostNotice, GuidanceDelivery::Intact)]
    );
}

#[tokio::test]
async fn a_project_without_guidance_records_an_empty_request() {
    let fixture = Fixture::new();
    fixture.file("src/lib.rs", "pub fn one() -> u8 { 1 }\n");

    let outcome = fixture
        .run(FakeProvider::new().text("ok"), &["explain src/lib.rs"])
        .await;

    assert_eq!(outcome.recorded.len(), 1);
    assert!(outcome.recorded[0].entries.is_empty());
}

#[tokio::test]
async fn each_turn_records_the_rules_that_apply_to_it() {
    let fixture = Fixture::new();
    fixture
        .file(
            SCOPED_RULE,
            "---\napplyTo: \"**/*.ts\"\n---\nPrefer named exports in TypeScript.",
        )
        .file("src/app.ts", "export const a = 1;\n")
        .file("AGENTS.md", "Indent with four spaces in this repository.");

    let outcome = fixture
        .run(
            FakeProvider::new().text("ok").text("ok"),
            &["what does this repo do?", "now look at src/app.ts"],
        )
        .await;

    assert!(outcome.find(0, SCOPED_RULE).is_none());
    assert_eq!(
        outcome.entry(1, SCOPED_RULE).delivery,
        GuidanceDelivery::Intact
    );
    assert_eq!(outcome.recorded[0].request, 0);
    assert_eq!(outcome.recorded[1].request, 1);
}

/// Recording a hook's sources must not change the text it injects: the text is
/// the same whether it is asked for alone or together with its sources.
#[test]
fn reporting_sources_leaves_the_injected_text_unchanged() {
    let fixture = Fixture::new();
    fixture
        .file("AGENTS.md", "Indent with four spaces in this repository.")
        .file(
            SCOPED_RULE,
            "---\napplyTo: \"**/*.ts\"\n---\nPrefer named exports in TypeScript.",
        )
        .file("src/app.ts", "export const a = 1;\n");
    let hook = fixture.instructions(PathsInPlay::new());

    for prompt in ["hello", "tighten the types in src/app.ts"] {
        let alone = hook.context_for(prompt);
        let contribution = hook.contribute(prompt);
        assert_eq!(contribution.text, alone, "prompt {prompt:?}");
        let text = alone.unwrap();
        for segment in &contribution.guidance {
            assert!(
                text.contains(segment.text.trim()),
                "{} is not a part of the injected text",
                segment.source.id
            );
        }
    }
}
