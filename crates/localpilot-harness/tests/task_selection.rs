//! Guidance selection from intent and project evidence, against synthetic
//! projects and a synthetic skill catalog in a temporary directory.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::Path;

use localpilot_harness::{
    select_guidance, GuidanceSelection, LoadDecision, ProjectFacts, SelectionConfidence,
    SelectionInput, TaskIntent,
};
use localpilot_skills::{discovery_roots, SkillSet};

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// A package with a `[guidance]` section in the catalog at `home`.
fn guided(home: &Path, name: &str, description: &str, guidance: &str) {
    let dir = format!(".localpilot/skills/{name}");
    write(
        home,
        &format!("{dir}/skill.toml"),
        &format!(
            "name = \"{name}\"\ndescription = \"{description}\"\nversion = \"1.0.0\"\n\
             [guidance]\nschema_version = 1\n{guidance}\n"
        ),
    );
    write(home, &format!("{dir}/SKILL.md"), &format!("{name} body.\n"));
}

/// The catalog the tests select from.
fn catalog(home: &Path) {
    for intent in [
        "explain",
        "investigate",
        "implement",
        "refactor",
        "review",
        "measure",
    ] {
        guided(
            home,
            intent,
            &format!("How to {intent} code"),
            &format!("role = \"procedure\"\nintents = [\"{intent}\"]"),
        );
    }
    let supplement = "role = \"supplement\"\n";
    guided(
        home,
        "rust",
        "Rust and Cargo specifics",
        &format!("{supplement}stacks = [\"rust\"]"),
    );
    guided(
        home,
        "cli",
        "Command line behaviour",
        &format!("{supplement}products = [\"cli\"]\ncapabilities_required_for_claim = [\"process.capture\"]"),
    );
    guided(
        home,
        "service",
        "Service request and response behaviour",
        &format!("{supplement}products = [\"service\"]\ncapabilities_required_for_claim = [\"http.observe\"]"),
    );
    guided(
        home,
        "dotnet",
        ".NET specifics",
        &format!("{supplement}stacks = [\"dotnet\"]"),
    );
    guided(
        home,
        "web",
        "Browser app specifics",
        &format!("{supplement}stacks = [\"web\"]"),
    );
    guided(
        home,
        "godot4",
        "Godot 4 specifics",
        &format!(
            "{supplement}stacks = [\"godot4\"]\ncapabilities_required_for_claim = [\"engine.run\"]"
        ),
    );
    // A user-only package and one without metadata.
    write(
        home,
        ".localpilot/skills/manual-release/SKILL.md",
        "---\nname: manual-release\ndescription: Publish a release of the package\ndisable-model-invocation: true\n---\nRelease steps.\n",
    );
    write(
        home,
        ".localpilot/skills/release-notes/SKILL.md",
        "---\nname: release-notes\ndescription: Write release notes for the package\n---\nNotes.\n",
    );
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project")).unwrap();
        catalog(&dir.path().join("home"));
        Self { dir }
    }

    fn project(&self) -> std::path::PathBuf {
        self.dir.path().join("project")
    }

    fn file(&self, relative: &str, contents: &str) -> &Self {
        write(&self.project(), relative, contents);
        self
    }

    fn rust_cli(&self) -> &Self {
        self.file(
            "Cargo.toml",
            "[package]\nname = \"counter\"\nversion = \"0.1.0\"\n",
        )
        .file("src/main.rs", "fn main() {}\n")
    }

    fn set(&self) -> SkillSet {
        let home = self.dir.path().join("home");
        SkillSet::resolve(&discovery_roots(&self.project(), Some(&home), true)).unwrap()
    }

    fn facts(&self) -> ProjectFacts {
        ProjectFacts::with_tool_lookup(self.project(), Box::new(|_| false))
    }

    fn select(&self, request: &str, paths: &[&str], capabilities: &[&str]) -> GuidanceSelection {
        self.select_with(&mut self.facts(), request, None, paths, capabilities)
    }

    fn select_with(
        &self,
        facts: &mut ProjectFacts,
        request: &str,
        explicit: Option<&str>,
        paths: &[&str],
        capabilities: &[&str],
    ) -> GuidanceSelection {
        let paths: Vec<String> = paths.iter().map(|p| (*p).to_string()).collect();
        let capabilities: BTreeSet<String> =
            capabilities.iter().map(|c| (*c).to_string()).collect();
        select_guidance(
            &SelectionInput {
                request,
                explicit,
                paths: &paths,
                capabilities: &capabilities,
                earlier: &[],
            },
            facts,
            &self.set(),
        )
    }
}

fn excluded_reason<'a>(selection: &'a GuidanceSelection, name: &str) -> &'a str {
    &selection
        .excluded
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("{name} not excluded: {:?}", selection.excluded))
        .reason
}

#[test]
fn a_cli_defect_fix_selects_investigation_with_rust_and_cli_and_may_load_bodies() {
    let fixture = Fixture::new();
    fixture.rust_cli();

    let selection = fixture.select(
        "Los de verkeerde exitcode op voor --count -1.",
        &[],
        &["process.capture"],
    );

    assert_eq!(
        selection.intent.phases,
        [TaskIntent::Investigate, TaskIntent::Implement]
    );
    assert_eq!(selection.selected_names(), ["investigate", "cli", "rust"]);
    assert_eq!(
        selection.loading,
        LoadDecision::Body,
        "{}",
        selection.loading_reason
    );
    assert!(selection
        .supplements
        .iter()
        .all(|s| s.unsupported_claims.is_empty()));
    assert_eq!(selection.component.component, ".");
    assert_eq!(
        selection.component.confidence,
        SelectionConfidence::Confirmed
    );
}

#[test]
fn an_explanation_takes_the_stack_but_not_the_runtime_supplement() {
    let fixture = Fixture::new();
    fixture.rust_cli();

    for request in [
        "Explain how this Rust argument parser works.",
        "Leg uit hoe deze argument parser werkt.",
    ] {
        let selection = fixture.select(request, &[], &["process.capture"]);
        assert_eq!(selection.selected_names(), ["explain", "rust"], "{request}");
        assert!(excluded_reason(&selection, "cli").contains("does not run or change"));
        assert!(
            !selection.intent.phases.iter().any(|p| p.acts()),
            "{request}"
        );
    }
}

#[test]
fn every_intent_selects_its_procedure_in_english_and_dutch() {
    let fixture = Fixture::new();
    fixture.rust_cli();
    let cases = [
        ("explain", "Explain what main does.", "Wat doet main?"),
        (
            "investigate",
            "Investigate why the count is off.",
            "Zoek uit waarom de telling afwijkt.",
        ),
        (
            "implement",
            "Add a --json flag.",
            "Voeg een --json vlag toe.",
        ),
        (
            "refactor",
            "Refactor the argument handling.",
            "Herstructureer de argumentverwerking.",
        ),
        (
            "review",
            "Review this change.",
            "Controleer deze wijziging.",
        ),
        (
            "measure",
            "Measure the startup time.",
            "Meet de opstarttijd.",
        ),
    ];
    for (procedure, english, dutch) in cases {
        for request in [english, dutch] {
            let selection = fixture.select(request, &[], &["process.capture"]);
            assert_eq!(
                selection.primary.as_ref().map(|p| p.name.as_str()),
                Some(procedure),
                "{request}: {:?}",
                selection.intent
            );
            assert!(selection.supplements.iter().any(|s| s.name == "rust"));
            let runtime = selection.supplements.iter().any(|s| s.name == "cli");
            assert_eq!(
                runtime,
                !matches!(procedure, "explain" | "review"),
                "{request}"
            );
        }
    }
}

#[test]
fn a_missing_capability_narrows_the_claim_and_keeps_the_guidance() {
    let fixture = Fixture::new();
    fixture.rust_cli();

    let selection = fixture.select("Fix the wrong exit code.", &[], &[]);

    let cli = selection
        .supplements
        .iter()
        .find(|s| s.name == "cli")
        .unwrap();
    assert_eq!(cli.unsupported_claims, ["process.capture"]);
    assert_eq!(selection.loading, LoadDecision::Body);
    assert_eq!(selection.primary.as_ref().unwrap().name, "investigate");
}

#[test]
fn a_mixed_repository_scopes_to_the_named_component() {
    let fixture = Fixture::new();
    fixture
        .file(
            "api/Api.csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk.Web"></Project>"#,
        )
        .file("api/Program.cs", "var app = 1;\n")
        .file(
            "web/package.json",
            r#"{"name": "web", "dependencies": {"react": "^18.3.1"}}"#,
        )
        .file("web/index.html", "<!doctype html>\n");

    let selection = fixture.select(
        "Add a validation rule in api/Program.cs",
        &[],
        &["process.capture", "http.observe"],
    );

    assert_eq!(selection.component.component, "api");
    assert_eq!(
        selection.component.confidence,
        SelectionConfidence::Confirmed
    );
    assert_eq!(
        selection.selected_names(),
        ["implement", "dotnet", "service"]
    );
    assert!(excluded_reason(&selection, "web").contains("shows no stack web"));
    assert_eq!(selection.loading, LoadDecision::Body);
}

#[test]
fn work_across_two_components_stays_tentative_and_offers_metadata_only() {
    let fixture = Fixture::new();
    fixture
        .file(
            "api/Api.csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk.Web"></Project>"#,
        )
        .file(
            "web/package.json",
            r#"{"name": "web", "dependencies": {"react": "^18.3.1"}}"#,
        );

    let selection = fixture.select(
        "Add input validation.",
        &["api/Program.cs", "web/src/Form.tsx"],
        &[],
    );

    assert_eq!(
        selection.component.confidence,
        SelectionConfidence::Tentative
    );
    assert_eq!(selection.component.component, "web");
    assert_eq!(selection.component.alternatives, ["api"]);
    assert_eq!(selection.loading, LoadDecision::Metadata);
    assert!(selection.loading_reason.contains("component is unclear"));
}

#[test]
fn engine_evidence_outranks_the_generated_project_file() {
    let fixture = Fixture::new();
    fixture
        .file(
            "project.godot",
            "config_version=5\n[application]\nconfig/features=PackedStringArray(\"4.3\", \"C#\")\n",
        )
        .file(
            "Hop.csproj",
            r#"<Project Sdk="Godot.NET.Sdk/4.3.0"></Project>"#,
        )
        .file("scripts/Player.cs", "class Player {}\n");

    let selection = fixture.select("Fix the double jump in scripts/Player.cs", &[], &[]);

    assert_eq!(selection.selected_names(), ["investigate", "godot4"]);
    assert!(excluded_reason(&selection, "dotnet").contains("shows no stack dotnet"));
    let godot = &selection.supplements[0];
    assert_eq!(godot.unsupported_claims, ["engine.run"]);
    assert!(selection.stacks.iter().all(|(stack, _)| stack != "dotnet"));
}

#[test]
fn a_user_only_package_is_reached_only_by_name() {
    let fixture = Fixture::new();
    fixture.rust_cli();

    let automatic = fixture.select("Publish a release of the package.", &[], &[]);
    let everything: Vec<&str> = automatic
        .selected_names()
        .into_iter()
        .chain(automatic.suggestions.iter().map(|s| s.name.as_str()))
        .chain(automatic.excluded.iter().map(|e| e.name.as_str()))
        .collect();
    assert!(!everything.contains(&"manual-release"), "{everything:?}");
    // The package without metadata is still offered by its description.
    assert!(automatic
        .suggestions
        .iter()
        .any(|s| s.name == "release-notes"));

    let explicit = fixture.select_with(
        &mut fixture.facts(),
        "Publish a release of the package.",
        Some("manual-release"),
        &[],
        &[],
    );
    assert_eq!(explicit.primary.as_ref().unwrap().name, "manual-release");
    assert_eq!(
        explicit.primary.as_ref().unwrap().reason,
        "named explicitly by the user"
    );
    assert_eq!(explicit.loading, LoadDecision::Body);
}

#[test]
fn an_unclear_request_falls_back_to_suggestions() {
    let fixture = Fixture::new();
    fixture.rust_cli();

    let selection = fixture.select("release notes for the package", &[], &[]);

    assert!(selection.primary.is_none());
    assert_eq!(selection.loading, LoadDecision::Metadata);
    assert!(selection.loading_reason.contains("intent is unclear"));
    assert!(selection
        .suggestions
        .iter()
        .any(|s| s.name == "release-notes"));
}

#[test]
fn a_declared_stack_selects_guidance_marked_as_declared() {
    let fixture = Fixture::new();
    let mut facts = fixture.facts();
    facts.declare(".", "declared.rust", None);
    facts.declare(".", "declared.cli", None);

    let selection = fixture.select_with(
        &mut facts,
        "Maak een nieuwe CLI die regels telt.",
        None,
        &[],
        &["process.capture"],
    );

    assert_eq!(selection.selected_names(), ["implement", "cli", "rust"]);
    assert_eq!(
        selection.stacks,
        [("rust".to_string(), SelectionConfidence::Declared)]
    );
}

#[test]
fn equal_procedures_do_not_justify_loading_one() {
    let fixture = Fixture::new();
    fixture.rust_cli();
    guided(
        &fixture.dir.path().join("home"),
        "implement-alt",
        "How to implement code",
        "role = \"procedure\"\nintents = [\"implement\"]",
    );

    let selection = fixture.select("Add a --json flag.", &[], &[]);

    assert_eq!(selection.loading, LoadDecision::Metadata);
    assert!(selection.loading_reason.contains("equally"));
}

#[test]
fn an_unusable_section_is_excluded_with_its_reason() {
    let fixture = Fixture::new();
    fixture.rust_cli();
    write(
        &fixture.dir.path().join("home"),
        ".localpilot/skills/broken/skill.toml",
        "name = \"broken\"\ndescription = \"Rust exit codes\"\nversion = \"1.0.0\"\n[guidance]\nschema_version = 7\n",
    );
    write(
        &fixture.dir.path().join("home"),
        ".localpilot/skills/broken/SKILL.md",
        "Body.\n",
    );

    let selection = fixture.select("Fix the exit code.", &[], &[]);

    assert!(excluded_reason(&selection, "broken").contains("cannot be used"));
}
