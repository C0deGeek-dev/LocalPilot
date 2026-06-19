//! Project-analysis context: a deterministic, read-only summary of the local
//! stack that helps the model reuse existing packages, scripts, and entrypoints
//! before inventing alternatives.
#![allow(clippy::unwrap_used)]

use localpilot_config::LookupPolicy;
use localpilot_harness::{analyze_project, ContextHook, ProjectAnalysisContext};

#[test]
fn package_manifest_scripts_and_dependencies_become_compact_project_facts() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{
  "scripts": {
    "dev": "runtime serve.ts",
    "build": "runtime build",
    "test": "runtime test"
  },
  "dependencies": {
    "@example/router": "1.0.0",
    "view-lib": "2.0.0"
  },
  "devDependencies": {
    "type-checker": "3.0.0"
  }
}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
    std::fs::write(dir.path().join("serve.ts"), "export default {}\n").unwrap();

    let analysis = analyze_project(dir.path()).unwrap();
    let rendered = analysis
        .render_context(LookupPolicy::Evidence)
        .expect("a manifest-backed project should render facts");

    assert!(rendered.contains("Project facts:"));
    assert!(rendered.contains("manifests: package.json"));
    assert!(rendered.contains("lockfiles: package-lock.json"));
    assert!(rendered.contains("scripts: build, dev, test"));
    assert!(rendered.contains("packages: @example/router, type-checker, view-lib"));
    assert!(rendered.contains("entrypoints: serve.ts"));
    assert!(rendered.contains("prefer existing scripts, entrypoints, and dependencies"));
    assert!(rendered.contains("lookup policy: evidence"));
    assert!(rendered.contains("available project knowledge, docs, MCP, or tool-discovery"));
}

#[test]
fn local_only_policy_keeps_the_expansion_guidance_local() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{ "dependencies": { "widget-kit": "1.0.0" } }"#,
    )
    .unwrap();

    let analysis = analyze_project(dir.path()).unwrap();
    let rendered = analysis
        .render_context(LookupPolicy::LocalOnly)
        .expect("a package manifest should render facts");

    assert!(rendered.contains("lookup policy: local_only"));
    assert!(rendered.contains("stay within local project context unless the user asks"));
    assert!(!rendered.contains("docs, MCP"));
}

#[test]
fn context_hook_contributes_nothing_for_an_unmarked_project() {
    let dir = tempfile::tempdir().unwrap();
    let hook = ProjectAnalysisContext::new(dir.path(), LookupPolicy::Evidence);
    assert_eq!(hook.context_for("anything"), None);
}
