//! The built-in task-guidance packages, selected for synthetic projects of
//! each initial kind, plus one outside the initial set.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::Path;

use localpilot_harness::{select_guidance, GuidanceSelection, ProjectFacts, SelectionInput};
use localpilot_skills::{discovery_roots, SkillScope, SkillSet};

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn select(root: &Path, request: &str, capabilities: &[&str]) -> (GuidanceSelection, SkillSet) {
    let set = SkillSet::resolve(&discovery_roots(root, None, true))
        .unwrap()
        .with_builtin_guidance();
    let mut facts = ProjectFacts::with_tool_lookup(root, Box::new(|_| false));
    let capabilities: BTreeSet<String> = capabilities.iter().map(|c| (*c).to_string()).collect();
    let selection = select_guidance(
        &SelectionInput {
            request,
            explicit: None,
            paths: &[],
            capabilities: &capabilities,
            earlier: &[],
        },
        &mut facts,
        &set,
    );
    (selection, set)
}

/// A label, the project's files, the request and the expected selection.
type Case = (
    &'static str,
    &'static [(&'static str, &'static str)],
    &'static str,
    &'static [&'static str],
);

fn names(selection: &GuidanceSelection) -> Vec<&str> {
    selection.selected_names()
}

#[test]
fn each_initial_project_kind_gets_its_procedure_and_supplements() {
    let cases: &[Case] = &[
        (
            "rust cli defect",
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"c\"\nversion = \"0.1.0\"\n",
                ),
                ("src/main.rs", "fn main() {}\n"),
            ],
            "Fix the wrong exit code for --count -1.",
            &["guide-investigate", "guide-cli", "guide-rust"],
        ),
        (
            "rust explanation",
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"c\"\nversion = \"0.1.0\"\n",
                ),
                ("src/main.rs", "fn main() {}\n"),
            ],
            "Explain how the argument parser works.",
            &["guide-explain", "guide-rust"],
        ),
        (
            "aspnet service",
            &[(
                "Api.csproj",
                r#"<Project Sdk="Microsoft.NET.Sdk.Web"></Project>"#,
            )],
            "Add an endpoint that lists orders.",
            &["guide-implement", "guide-dotnet", "guide-service"],
        ),
        (
            "wpf desktop",
            &[(
                "Notes.csproj",
                r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><UseWPF>true</UseWPF></PropertyGroup></Project>"#,
            )],
            "Add a save button to the main window.",
            &["guide-implement", "guide-desktop", "guide-dotnet"],
        ),
        (
            "godot game",
            &[(
                "project.godot",
                "config_version=5\n[application]\nconfig/features=PackedStringArray(\"4.3\")\n",
            )],
            "Fix the double jump.",
            &["guide-investigate", "guide-game", "guide-godot4"],
        ),
        (
            "three.js browser game",
            &[(
                "package.json",
                r#"{"name": "g", "dependencies": {"three": "0.169.0"}, "devDependencies": {"vite": "^5.4.0"}}"#,
            )],
            "Add a score counter to the scene.",
            &["guide-implement", "guide-threejs", "guide-web"],
        ),
    ];
    for (label, files, request, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        for (path, contents) in *files {
            write(dir.path(), path, contents);
        }
        let (selection, _) = select(dir.path(), request, &["process.capture"]);
        assert_eq!(
            names(&selection),
            *expected,
            "{label}: {:?}",
            selection.excluded
        );
    }
}

#[test]
fn a_project_outside_the_initial_set_still_gets_the_procedure() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "pyproject.toml", "[project]\nname = \"tool\"\n");

    let (selection, _) = select(dir.path(), "Add a --verbose option.", &[]);

    assert_eq!(names(&selection), ["guide-implement"]);
    assert!(selection.supplements.is_empty());
}

#[test]
fn a_missing_capability_is_reported_on_the_runtime_supplement() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Api.csproj",
        r#"<Project Sdk="Microsoft.NET.Sdk.Web"></Project>"#,
    );

    let (selection, _) = select(dir.path(), "Add an endpoint that lists orders.", &[]);

    let service = selection
        .supplements
        .iter()
        .find(|s| s.name == "guide-service")
        .unwrap();
    assert_eq!(service.unsupported_claims, ["http.observe"]);
}

#[test]
fn a_project_package_with_the_same_name_replaces_the_built_in_one() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Cargo.toml",
        "[package]\nname = \"c\"\nversion = \"0.1.0\"\n",
    );
    write(
        dir.path(),
        ".localpilot/skills/guide-rust/skill.toml",
        "name = \"guide-rust\"\ndescription = \"Our Rust rules\"\nversion = \"9.0.0\"\n\
         [guidance]\nschema_version = 1\nrole = \"supplement\"\nstacks = [\"rust\"]\n",
    );
    write(
        dir.path(),
        ".localpilot/skills/guide-rust/SKILL.md",
        "Our own Rust rules.\n",
    );

    let (selection, set) = select(dir.path(), "Fix the exit code.", &[]);

    assert!(names(&selection).contains(&"guide-rust"));
    let skill = set.by_name("guide-rust").unwrap();
    assert_eq!(skill.scope, SkillScope::ProjectLocalPilot);
    assert_eq!(skill.instructions, "Our own Rust rules.\n");
    assert!(set
        .shadowed()
        .iter()
        .any(|s| s.name == "guide-rust" && s.loser_scope == SkillScope::Builtin));
}

#[test]
fn without_guidance_the_catalog_has_no_built_in_packages() {
    let dir = tempfile::tempdir().unwrap();
    let plain = SkillSet::resolve(&discovery_roots(dir.path(), None, true)).unwrap();
    assert!(plain.names().iter().all(|name| !name.starts_with("guide-")));
    let with = plain.with_builtin_guidance();
    assert_eq!(
        with.names()
            .iter()
            .filter(|name| name.starts_with("guide-"))
            .count(),
        localpilot_skills::builtin_guidance().len()
    );
}

#[test]
fn an_installed_procedure_outranks_the_built_in_one_on_an_equal_match() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Cargo.toml",
        "[package]\nname = \"c\"\nversion = \"0.1.0\"\n",
    );
    write(
        dir.path(),
        ".localpilot/skills/our-investigation/skill.toml",
        "name = \"our-investigation\"\ndescription = \"Find out why something behaves wrongly, before or instead of fixing it\"\nversion = \"1.0.0\"\n\
         [guidance]\nschema_version = 1\nrole = \"procedure\"\nintents = [\"investigate\"]\n",
    );
    write(
        dir.path(),
        ".localpilot/skills/our-investigation/SKILL.md",
        "Ours.\n",
    );

    let (selection, _) = select(dir.path(), "Fix the exit code.", &[]);

    assert_eq!(
        selection.primary.as_ref().unwrap().name,
        "our-investigation"
    );
    assert_eq!(selection.loading, localpilot_harness::LoadDecision::Body);
    assert!(selection
        .excluded
        .iter()
        .any(|e| e.name == "guide-investigate" && e.reason.contains("ranked higher")));
}
