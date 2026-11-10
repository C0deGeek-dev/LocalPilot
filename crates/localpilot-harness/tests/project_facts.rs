//! Component-scoped project facts against synthetic projects.
//!
//! Each test writes a small project into a temporary directory and reads its
//! facts back. Tool presence is decided by a stand-in, never by the host's
//! `PATH`, and nothing here runs a project command or touches the network.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use localpilot_harness::{ComponentFacts, FactConfidence, ProjectFacts};

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Facts for `root` where every tool in `present` is on `PATH`, and the
/// programs that were looked up.
fn facts_with(root: &Path, present: &[&'static str]) -> (ProjectFacts, Arc<Mutex<Vec<String>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&asked);
    let present: Vec<&'static str> = present.to_vec();
    let facts = ProjectFacts::with_tool_lookup(
        root,
        Box::new(move |program| {
            seen.lock().unwrap().push(program.to_string());
            present.contains(&program)
        }),
    );
    (facts, asked)
}

fn facts(root: &Path) -> ProjectFacts {
    facts_with(root, &[]).0
}

/// `(id, value, confidence)` of `id`, for compact assertions.
fn fact(facts: &ComponentFacts, id: &str) -> (Option<String>, FactConfidence) {
    let found = facts
        .get(id)
        .unwrap_or_else(|| panic!("no fact {id} in {}: {:?}", facts.component, facts.facts));
    (found.value.clone(), found.confidence)
}

fn confirmed(value: &str) -> (Option<String>, FactConfidence) {
    (Some(value.to_string()), FactConfidence::Confirmed)
}

/// Every file and directory under `root`, relative.
fn tree(root: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            out.insert(path.strip_prefix(root).unwrap().to_path_buf());
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    out
}

fn rust_workspace(root: &Path) {
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/cli\", \"crates/math\"]\nresolver = \"2\"\n",
    );
    write(
        root,
        "rust-toolchain.toml",
        "[toolchain]\nchannel = \"1.82.0\"\n",
    );
    write(
        root,
        "crates/cli/Cargo.toml",
        "[package]\nname = \"counter\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(root, "crates/cli/src/main.rs", "fn main() {}\n");
    write(
        root,
        "crates/math/Cargo.toml",
        "[package]\nname = \"math\"\nversion = \"0.1.0\"\nedition = \"2021\"\nrust-version = \"1.80\"\n",
    );
    write(root, "crates/math/src/lib.rs", "pub fn one() -> u8 { 1 }\n");
    write(root, "crates/math/tests/one.rs", "#[test] fn one() {}\n");
}

#[test]
fn a_workspace_member_is_its_own_component_with_the_workspace_pins() {
    let dir = tempfile::tempdir().unwrap();
    rust_workspace(dir.path());
    let (mut facts, _) = facts_with(dir.path(), &["cargo"]);

    assert_eq!(
        facts
            .component_of(Path::new("crates/cli/src/main.rs"))
            .as_deref(),
        Some("crates/cli")
    );
    let cli = facts.facts_for("crates/cli");
    assert_eq!(fact(&cli, "cargo.package"), confirmed("counter"));
    assert_eq!(fact(&cli, "cargo.bin"), (None, FactConfidence::Confirmed));
    assert_eq!(fact(&cli, "product.cli"), (None, FactConfidence::Tentative));
    assert_eq!(fact(&cli, "cargo.workspace_member"), confirmed("."));
    assert_eq!(fact(&cli, "rust.toolchain"), confirmed("1.82.0"));
    assert_eq!(
        cli.get("rust.toolchain").unwrap().source.as_deref(),
        Some("rust-toolchain.toml")
    );
    assert_eq!(fact(&cli, "tool.cargo"), (None, FactConfidence::Confirmed));
    assert!(!cli.has("cargo.lib"), "{:?}", cli.facts);
    assert!(!cli.has("test.target"));

    let math = facts
        .facts_for_path(Path::new("crates/math/src/lib.rs"))
        .unwrap();
    assert_eq!(math.component, "crates/math");
    assert_eq!(fact(&math, "cargo.lib"), (None, FactConfidence::Confirmed));
    assert_eq!(fact(&math, "rust.version_required"), confirmed("1.80"));
    assert_eq!(fact(&math, "test.target"), confirmed("tests"));
    assert!(!math.has("cargo.bin"));

    let root = facts.facts_for(".");
    assert_eq!(
        fact(&root, "cargo.workspace"),
        confirmed("crates/cli, crates/math")
    );
    assert!(!root.has("cargo.package"));
}

#[test]
fn a_mixed_repository_keeps_each_component_to_its_own_stack() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "Shop.sln", "Microsoft Visual Studio Solution File\n");
    write(root, "global.json", r#"{"sdk": {"version": "8.0.404"}}"#);
    write(
        root,
        "api/Api.csproj",
        r#"<Project Sdk="Microsoft.NET.Sdk.Web"><PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup></Project>"#,
    );
    write(
        root,
        "api/Program.cs",
        "var app = WebApplication.Create();\n",
    );
    write(
        root,
        "web/package.json",
        r#"{"name": "shop-web", "dependencies": {"react": "^18.3.1"}, "devDependencies": {"vite": "^5.4.0"}, "scripts": {"test": "vitest run", "deploy": "ignored"}}"#,
    );
    write(root, "web/index.html", "<!doctype html>\n");
    let mut facts = facts(root);

    assert_eq!(facts.components(), [".", "api", "web"]);

    let api = facts.facts_for_path(Path::new("api/Program.cs")).unwrap();
    assert_eq!(api.component, "api");
    assert_eq!(fact(&api, "aspnet.project"), confirmed("Api.csproj"));
    assert_eq!(
        fact(&api, "product.service"),
        (None, FactConfidence::Tentative)
    );
    assert_eq!(fact(&api, "dotnet.target_framework"), confirmed("net8.0"));
    // The SDK pin is found at the repository root above the component.
    assert_eq!(fact(&api, "dotnet.sdk"), confirmed("8.0.404"));
    assert!(api.facts.iter().all(|f| !f.id.starts_with("dependency.")));
    assert!(!api.has("product.browser"));
    // The tool is reported unknown, not as a failure.
    assert!(api.unknown("tool.dotnet").is_some());

    let web = facts.facts_for_path(Path::new("web/src/App.tsx")).unwrap();
    assert_eq!(web.component, "web");
    assert_eq!(fact(&web, "dependency.react"), confirmed("^18.3.1"));
    assert_eq!(fact(&web, "script.test"), confirmed("vitest run"));
    assert_eq!(fact(&web, "test.target"), confirmed("script.test"));
    assert!(!web.has("script.deploy"));
    assert_eq!(
        fact(&web, "product.browser"),
        (None, FactConfidence::Tentative)
    );
    assert!(web
        .facts
        .iter()
        .all(|f| !f.id.starts_with("dotnet.") && !f.id.starts_with("aspnet.")));

    // The root holds the solution, not the stacks of the components below it.
    let top = facts.facts_for(".");
    assert_eq!(fact(&top, "dotnet.solution"), confirmed("Shop.sln"));
    assert!(!top.has("aspnet.project"));
    assert!(!top.has("dependency.react"));
}

#[test]
fn an_engine_project_marks_its_project_file_as_generated() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "game/project.godot",
        "config_version=5\n\n[application]\nconfig/name=\"Hop\"\nrun/main_scene=\"res://main.tscn\"\nconfig/features=PackedStringArray(\"4.3\", \"C#\", \"Forward Plus\")\n",
    );
    write(
        root,
        "game/Hop.csproj",
        r#"<Project Sdk="Godot.NET.Sdk/4.3.0"><PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup></Project>"#,
    );
    write(
        root,
        "game/scripts/Player.cs",
        "public partial class Player {}\n",
    );
    write(
        root,
        "unity/ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 2022.3.42f1\nm_EditorVersionWithRevision: 2022.3.42f1 (abc)\n",
    );
    write(
        root,
        "unity/Assembly-CSharp.csproj",
        r#"<?xml version="1.0"?><Project ToolsVersion="4.0" DefaultTargets="Build"></Project>"#,
    );
    write(root, "unity/Assets/Scripts/Jump.cs", "class Jump {}\n");
    let mut facts = facts(root);

    let godot = facts
        .facts_for_path(Path::new("game/scripts/Player.cs"))
        .unwrap();
    assert_eq!(godot.component, "game");
    assert_eq!(
        fact(&godot, "godot.project"),
        (None, FactConfidence::Confirmed)
    );
    assert_eq!(fact(&godot, "godot.version"), confirmed("4.3"));
    assert_eq!(
        fact(&godot, "godot.main_scene"),
        confirmed("res://main.tscn")
    );
    assert_eq!(
        fact(&godot, "product.game"),
        (None, FactConfidence::Confirmed)
    );
    assert_eq!(fact(&godot, "generated.csproj"), confirmed("Hop.csproj"));
    assert_eq!(
        fact(&godot, "godot.csharp"),
        confirmed("Godot.NET.Sdk/4.3.0")
    );

    let unity = facts
        .facts_for_path(Path::new("unity/Assets/Scripts/Jump.cs"))
        .unwrap();
    assert_eq!(unity.component, "unity");
    assert_eq!(fact(&unity, "unity.version"), confirmed("2022.3.42f1"));
    assert_eq!(
        fact(&unity, "generated.csproj"),
        confirmed("Assembly-CSharp.csproj")
    );
    assert_eq!(
        fact(&unity, "product.game"),
        (None, FactConfidence::Confirmed)
    );
    // The generated project is not an SDK-style project, and says so.
    assert!(unity.unknown("dotnet.sdk_project").is_some());
}

#[test]
fn a_desktop_project_is_recognized_with_its_framework() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Notes.csproj",
        r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <OutputType>WinExe</OutputType>
    <TargetFramework>net8.0-windows</TargetFramework>
    <UseWPF>true</UseWPF>
  </PropertyGroup>
</Project>"#,
    );
    write(
        root,
        "tests/Notes.Tests.csproj",
        r#"<Project Sdk="Microsoft.NET.Sdk"><ItemGroup><PackageReference Include="Microsoft.NET.Test.Sdk" Version="17.11.1" /></ItemGroup></Project>"#,
    );
    let mut facts = facts(root);

    let app = facts.facts_for(".");
    assert_eq!(fact(&app, "wpf.project"), confirmed("Notes.csproj"));
    assert_eq!(
        fact(&app, "product.desktop"),
        (None, FactConfidence::Confirmed)
    );
    assert_eq!(
        fact(&app, "dotnet.target_framework"),
        confirmed("net8.0-windows")
    );
    assert!(!app.has("dotnet.test_project"));

    let tests = facts.facts_for("tests");
    assert_eq!(
        fact(&tests, "dotnet.test_project"),
        confirmed("Notes.Tests.csproj")
    );
    assert!(!tests.has("wpf.project"));
}

#[test]
fn the_installed_version_comes_from_the_lockfile_beside_the_declared_range() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"name": "site", "devDependencies": {"tailwindcss": "^3.4.0", "three": "0.169.0"}}"#,
    );
    write(
        root,
        "package-lock.json",
        r#"{"lockfileVersion": 3, "packages": {"": {}, "node_modules/tailwindcss": {"version": "3.4.17"}}}"#,
    );
    let mut facts = facts(root);

    let site = facts.facts_for(".");
    assert_eq!(fact(&site, "dependency.tailwindcss"), confirmed("^3.4.0"));
    assert_eq!(
        fact(&site, "dependency.tailwindcss.locked"),
        confirmed("3.4.17")
    );
    assert_eq!(
        site.get("dependency.tailwindcss.locked")
            .unwrap()
            .source
            .as_deref(),
        Some("package-lock.json")
    );
    // Declared, but not in the lockfile: only the range is known.
    assert_eq!(fact(&site, "dependency.three"), confirmed("0.169.0"));
    assert!(!site.has("dependency.three.locked"));
}

#[test]
fn an_empty_project_takes_declared_intent_without_inventing_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let mut facts = facts(dir.path());

    let empty = facts.facts_for(".");
    assert_eq!(
        fact(&empty, "project.empty"),
        (None, FactConfidence::Confirmed)
    );
    assert!(empty.facts.iter().all(|f| !f.id.starts_with("cargo.")));

    facts.declare(".", "declared.rust", None);
    facts.declare(".", "declared.cli", None);
    let declared = facts.facts_for(".");
    assert_eq!(
        fact(&declared, "declared.rust"),
        (None, FactConfidence::Declared)
    );
    assert_eq!(
        declared.with_confidence(FactConfidence::Declared).count(),
        2
    );
    assert!(declared.get("declared.rust").unwrap().source.is_none());
    // The user's word is not evidence that a manifest or a tool exists.
    assert!(!declared.has("cargo.package"));
    assert!(!declared.has("tool.cargo"));

    // A refresh drops detections, not what the user said.
    facts.refresh();
    assert!(facts.facts_for(".").has("declared.rust"));
}

#[test]
fn a_declared_fact_sits_beside_contradicting_evidence_and_does_not_replace_it() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Cargo.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
    );
    write(dir.path(), "src/main.rs", "fn main() {}\n");
    let mut facts = facts(dir.path());
    facts.declare(".", "product.library", None);

    let both = facts.facts_for(".");
    let confidences: Vec<FactConfidence> = both
        .facts
        .iter()
        .filter(|f| f.id.starts_with("product."))
        .map(|f| f.confidence)
        .collect();
    assert!(confidences.contains(&FactConfidence::Declared));
    assert!(confidences.contains(&FactConfidence::Tentative));
    assert_eq!(fact(&both, "cargo.bin"), (None, FactConfidence::Confirmed));
}

#[test]
fn what_cannot_be_read_is_unknown_and_the_rest_is_still_found() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "Cargo.toml", "[package\nname = ");
    write(root, "rust-toolchain", "stable\n");
    write(
        root,
        "tool/Legacy.csproj",
        r#"<Project ToolsVersion="15.0"></Project>"#,
    );
    write(root, "big/pyproject.toml", &"# padding\n".repeat(30_000));
    let mut facts = facts(root);

    let rust = facts.facts_for(".");
    assert!(rust
        .unknown("cargo.manifest")
        .unwrap()
        .reason
        .contains("not valid TOML"));
    assert_eq!(fact(&rust, "rust.toolchain"), confirmed("stable"));

    let legacy = facts.facts_for("tool");
    assert!(legacy
        .unknown("dotnet.sdk_project")
        .unwrap()
        .reason
        .contains("not supported"));

    let big = facts.facts_for("big");
    assert!(big
        .unknown("python.project")
        .unwrap()
        .reason
        .contains("read limit"));

    // A directory with files but no marker is unknown, not empty.
    write(root, "notes/todo.txt", "later\n");
    let notes = facts.facts_for("notes");
    assert!(notes.unknown("stack").is_some());
    assert!(!notes.has("project.empty"));
}

#[test]
fn detection_reads_and_runs_nothing_and_changes_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"name": "x", "scripts": {"test": "node -e \"require('fs').writeFileSync('ran-test', '')\"", "build": "node build.js"}}"#,
    );
    write(
        root,
        "build.js",
        "require('fs').writeFileSync('ran-build', '')\n",
    );
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\nbuild = \"build.rs\"\n",
    );
    write(
        root,
        "build.rs",
        "fn main() { std::fs::write(\"ran-build-rs\", \"\").unwrap(); }\n",
    );
    let before = tree(root);
    let (mut facts, asked) = facts_with(root, &["node"]);

    let all = facts.facts_for(".");

    assert_eq!(
        tree(root),
        before,
        "detection must not create or remove files"
    );
    assert!(all.has("script.test"));
    assert_eq!(fact(&all, "tool.node"), (None, FactConfidence::Confirmed));
    assert!(all.unknown("tool.cargo").is_some());
    // Tools are only looked up by name, once per stack.
    let mut asked = asked.lock().unwrap().clone();
    asked.sort();
    assert_eq!(asked, ["cargo", "node"]);
}

#[test]
fn facts_are_cached_until_a_file_they_came_from_changes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    write(root, "app/src/lib.rs", "\n");
    write(root, "other/package.json", r#"{"name": "other"}"#);
    let mut facts = facts(root);

    let first = facts.facts_for("app");
    assert!(!first.has("cargo.bin"));
    assert_eq!(facts.detections(), 1);

    // Asked again with nothing changed: answered from the cache.
    assert_eq!(facts.facts_for("app"), first);
    assert_eq!(facts.detections(), 1);

    // A change in another component leaves this one cached.
    write(
        root,
        "other/package.json",
        r#"{"name": "other", "version": "2.0.0"}"#,
    );
    facts.facts_for("app");
    assert_eq!(facts.detections(), 1);

    // A new binary target in this component is seen on the next request.
    write(root, "app/src/main.rs", "fn main() {}\n");
    let second = facts.facts_for("app");
    assert!(second.has("cargo.bin"));
    assert_eq!(facts.detections(), 2);

    // An edited manifest is seen too.
    write(
        root,
        "app/Cargo.toml",
        "[package]\nname = \"renamed\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    let third = facts.facts_for("app");
    assert_eq!(fact(&third, "cargo.package"), confirmed("renamed"));
    assert_eq!(fact(&third, "rust.edition"), confirmed("2024"));

    // An explicit refresh detects again even when nothing changed.
    let count = facts.detections();
    facts.refresh();
    facts.facts_for("app");
    assert_eq!(facts.detections(), count + 1);
    assert!(third
        .depends_on()
        .iter()
        .any(|path| path == "app/Cargo.toml"));
}

#[test]
fn dependency_and_build_directories_are_never_components() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name": "site"}"#);
    write(
        root,
        "node_modules/left-pad/package.json",
        r#"{"name": "left-pad"}"#,
    );
    write(root, "node_modules/left-pad/index.js", "\n");
    write(
        root,
        "target/debug/build/x/Cargo.toml",
        "[package]\nname = \"x\"\n",
    );
    write(root, ".cache/tool/package.json", "{}");
    let facts = facts(root);

    assert_eq!(facts.components(), ["."]);
    assert_eq!(
        facts
            .component_of(Path::new("node_modules/left-pad/index.js"))
            .as_deref(),
        Some(".")
    );
    assert_eq!(
        facts
            .component_of(Path::new("target/debug/build/x/Cargo.toml"))
            .as_deref(),
        Some(".")
    );
}

#[test]
fn a_path_outside_the_workspace_has_no_component() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "inside/Cargo.toml",
        "[package]\nname = \"i\"\nversion = \"0.1.0\"\n",
    );
    let facts = facts(&dir.path().join("inside"));

    assert_eq!(facts.component_of(Path::new("../elsewhere/file.rs")), None);
    assert_eq!(facts.component_of(&dir.path().join("elsewhere")), None);
    assert_eq!(
        facts.component_of(Path::new("src/main.rs")).as_deref(),
        Some(".")
    );
}
