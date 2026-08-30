//! Deterministic project analysis for per-turn stack context.
//!
//! This is deliberately local and generic: it reads common manifests and marker
//! files, then renders a compact nudge that the model should reuse what exists
//! before inventing parallel scripts, packages, or entrypoints.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use localpilot_config::LookupPolicy;

use crate::{ContextHook, SessionRuntime};

const MANIFESTS: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "composer.json",
    "Gemfile",
];

const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "Cargo.lock",
    "poetry.lock",
    "uv.lock",
    "go.sum",
    "composer.lock",
    "Gemfile.lock",
];

const ENTRY_PREFIXES: &[&str] = &["main", "index", "server", "serve", "app"];
const ENTRY_EXTS: &[&str] = &["js", "jsx", "ts", "tsx", "mjs", "cjs", "rs", "py", "go"];
const MAX_ITEMS: usize = 16;

/// Compact, deterministic facts about the project shape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectAnalysis {
    manifests: BTreeSet<String>,
    lockfiles: BTreeSet<String>,
    scripts: BTreeSet<String>,
    packages: BTreeSet<String>,
    entrypoints: BTreeSet<String>,
}

impl ProjectAnalysis {
    /// Render a system-context block for the given lookup policy.
    #[must_use]
    pub fn render_context(&self, lookup_policy: LookupPolicy) -> Option<String> {
        if self.is_empty() {
            return None;
        }

        let mut lines = vec!["Project facts:".to_string()];
        push_line(&mut lines, "manifests", &self.manifests);
        push_line(&mut lines, "lockfiles", &self.lockfiles);
        push_line(&mut lines, "scripts", &self.scripts);
        push_line(&mut lines, "packages", &self.packages);
        push_line(&mut lines, "entrypoints", &self.entrypoints);
        lines.push(
            "guidance: prefer existing scripts, entrypoints, and dependencies before adding \
             alternatives."
                .to_string(),
        );
        lines.push(format!(
            "lookup policy: {}",
            lookup_policy_label(lookup_policy)
        ));
        lines.push(lookup_guidance(lookup_policy).to_string());
        Some(lines.join("\n"))
    }

    fn is_empty(&self) -> bool {
        self.manifests.is_empty()
            && self.lockfiles.is_empty()
            && self.scripts.is_empty()
            && self.packages.is_empty()
            && self.entrypoints.is_empty()
    }
}

/// Analyze the workspace root using only local, read-only filesystem facts.
///
/// # Errors
/// Returns an IO error if the root directory cannot be listed.
pub fn analyze_project(root: &Path) -> io::Result<ProjectAnalysis> {
    let mut analysis = ProjectAnalysis::default();
    for name in MANIFESTS {
        if root.join(name).is_file() {
            analysis.manifests.insert((*name).to_string());
        }
    }
    for name in LOCKFILES {
        if root.join(name).is_file() {
            analysis.lockfiles.insert((*name).to_string());
        }
    }
    read_package_json(root, &mut analysis);
    read_cargo_toml(root, &mut analysis);
    collect_entrypoints(root, &mut analysis)?;
    Ok(analysis)
}

/// A context hook that contributes project-analysis facts before each turn.
pub struct ProjectAnalysisContext {
    root: PathBuf,
    lookup_policy: LookupPolicy,
}

impl ProjectAnalysisContext {
    /// Create a project-analysis context hook.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, lookup_policy: LookupPolicy) -> Self {
        Self {
            root: root.into(),
            lookup_policy,
        }
    }
}

impl ContextHook for ProjectAnalysisContext {
    fn name(&self) -> &str {
        "project-analysis"
    }

    fn context_for(&self, _prompt: &str) -> Option<String> {
        analyze_project(&self.root)
            .ok()
            .and_then(|analysis| analysis.render_context(self.lookup_policy))
    }
}

/// Register project-analysis context on a session runtime when enabled.
pub fn register_project_analysis_context(
    root: &Path,
    enabled: bool,
    lookup_policy: LookupPolicy,
    runtime: &mut SessionRuntime,
) {
    if enabled {
        runtime
            .hooks_mut()
            .register_context_hook(Arc::new(ProjectAnalysisContext::new(root, lookup_policy)));
    }
}

fn read_package_json(root: &Path, analysis: &mut ProjectAnalysis) {
    let Ok(text) = std::fs::read_to_string(root.join("package.json")) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    if let Some(scripts) = value.get("scripts").and_then(serde_json::Value::as_object) {
        analysis.scripts.extend(scripts.keys().cloned());
    }
    for key in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(deps) = value.get(key).and_then(serde_json::Value::as_object) {
            analysis.packages.extend(deps.keys().cloned());
        }
    }
}

fn read_cargo_toml(root: &Path, analysis: &mut ProjectAnalysis) {
    let Ok(text) = std::fs::read_to_string(root.join("Cargo.toml")) else {
        return;
    };
    let mut in_deps = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_deps = matches!(
                trimmed,
                "[dependencies]" | "[dev-dependencies]" | "[build-dependencies]"
            );
            continue;
        }
        if !in_deps || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = trimmed.split_once('=') {
            let name = name.trim().trim_matches('"');
            if !name.is_empty() {
                analysis.packages.insert(name.to_string());
            }
        }
    }
}

fn collect_entrypoints(root: &Path, analysis: &mut ProjectAnalysis) -> io::Result<()> {
    collect_entrypoints_in(root, root, analysis)?;
    let src = root.join("src");
    if src.is_dir() {
        collect_entrypoints_in(root, &src, analysis)?;
    }
    Ok(())
}

fn collect_entrypoints_in(
    root: &Path,
    dir: &Path,
    analysis: &mut ProjectAnalysis,
) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || !is_entrypoint_path(&path) {
            continue;
        }
        if let Ok(relative) = path.strip_prefix(root) {
            analysis
                .entrypoints
                .insert(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

fn is_entrypoint_path(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
        return false;
    };
    let stem = stem.to_ascii_lowercase();
    let ext = ext.to_ascii_lowercase();
    ENTRY_PREFIXES.contains(&stem.as_str()) && ENTRY_EXTS.contains(&ext.as_str())
}

fn push_line(lines: &mut Vec<String>, label: &str, items: &BTreeSet<String>) {
    if items.is_empty() {
        return;
    }
    let rendered = render_items(items);
    lines.push(format!("{label}: {rendered}"));
}

fn render_items(items: &BTreeSet<String>) -> String {
    let mut rendered: Vec<String> = items.iter().take(MAX_ITEMS).cloned().collect();
    if items.len() > MAX_ITEMS {
        rendered.push(format!("+{} more", items.len() - MAX_ITEMS));
    }
    rendered.join(", ")
}

fn lookup_policy_label(policy: LookupPolicy) -> &'static str {
    match policy {
        LookupPolicy::LocalOnly => "local_only",
        LookupPolicy::Evidence => "evidence",
        LookupPolicy::Proactive => "proactive",
    }
}

fn lookup_guidance(policy: LookupPolicy) -> &'static str {
    match policy {
        LookupPolicy::LocalOnly => {
            "lookup guidance: stay within local project context unless the user asks for external information."
        }
        LookupPolicy::Evidence => {
            "lookup guidance: when local facts are insufficient or uncertain, use available project knowledge, docs, MCP, or tool-discovery before guessing."
        }
        LookupPolicy::Proactive => {
            "lookup guidance: for package or framework work, proactively use available project knowledge, docs, MCP, or tool-discovery before choosing an API."
        }
    }
}
