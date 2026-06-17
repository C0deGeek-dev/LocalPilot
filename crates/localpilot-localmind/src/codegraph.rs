//! Code-graph maintenance through the host boundary.
//!
//! The host owns workspace access: candidate files are enumerated here with
//! the same ignore discipline as the rest of capture (gitignore-aware walk,
//! no hidden files), and the engine's own boundary applies the project's
//! `excluded_paths` on top. The engine never walks the filesystem itself.

use crate::LearningError;
use localmind_codegraph::{IngestBoundary, Reindexer};
use localmind_store::{GraphStore, ProjectConfig};
use std::path::{Path, PathBuf};

/// Outcome of one bounded reindex pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CodeGraphSummary {
    pub reindexed: usize,
    pub pruned: usize,
    pub unchanged: usize,
    pub rejected: usize,
    /// Plan entries left for a later pass when the batch budget ran out.
    pub remaining: usize,
}

/// Runs one bounded, incremental code-graph reindex of the project. Change
/// detection is content-based, so calling this at any lifecycle point is
/// safe: an up-to-date graph is a fast no-op. `batch_limit` caps how many
/// files one pass may touch; leftover work is picked up by the next pass.
pub fn codegraph_reindex(
    project_root: &Path,
    batch_limit: usize,
) -> Result<CodeGraphSummary, LearningError> {
    let config = ProjectConfig::discover(project_root)
        .map_err(|error| LearningError::Config(error.to_string()))?;
    let excluded = config.config.learning.excluded_paths.clone();

    let candidates = source_candidates(project_root);
    let boundary = IngestBoundary::new(project_root, excluded)
        .map_err(|error| LearningError::Graph(error.to_string()))?;
    let store = GraphStore::open_project(project_root)
        .map_err(|error| LearningError::Graph(error.to_string()))?;

    let mut reindexer =
        Reindexer::new().map_err(|error| LearningError::Graph(error.to_string()))?;
    let mut plan = reindexer
        .plan(&boundary, &candidates, &store)
        .map_err(|error| LearningError::Graph(error.to_string()))?;
    let report = reindexer
        .run(&boundary, &store, &mut plan, batch_limit)
        .map_err(|error| LearningError::Graph(error.to_string()))?;

    Ok(CodeGraphSummary {
        reindexed: report.reindexed,
        pruned: report.pruned,
        unchanged: plan.unchanged,
        rejected: plan.rejected.len(),
        remaining: plan.remaining(),
    })
}

/// Source and documentation files under the project root, walked with the
/// host's capture discipline: gitignore-aware and skipping hidden entries. A
/// file is a candidate when the engine recognizes its language (any supported
/// grammar) or it is Markdown (kept for the doc-mention graph); the engine then
/// routes each file to the right extractor.
fn source_candidates(project_root: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for entry in ignore::WalkBuilder::new(project_root).build().flatten() {
        let path = entry.into_path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let indexable =
            localmind_codegraph::Language::from_path(name).is_some() || name.ends_with(".md");
        if indexable {
            candidates.push(path);
        }
    }
    candidates.sort();
    candidates
}

/// A symbol's place in the graph, flattened for display: the symbol itself,
/// what surrounds it, what tests it, and what was learned about it.
#[derive(Clone, Debug, Default)]
pub struct SymbolReport {
    pub kind: String,
    pub qualified_name: String,
    pub path: Option<String>,
    pub skeleton: Option<String>,
    /// `kind  qualified_name` lines for each neighbor.
    pub neighbors: Vec<String>,
    pub tests: Vec<String>,
    /// `(memory id, anchor confidence, body snippet)` per anchored entry.
    pub knowledge: Vec<(String, f32, String)>,
}

/// Inspects one symbol through the same tool contracts an MCP host uses.
/// Plain names work when unique; qualified names disambiguate.
pub fn codegraph_inspect(project_root: &Path, symbol: &str) -> Result<SymbolReport, LearningError> {
    let store = GraphStore::open_project(project_root)
        .map_err(|error| LearningError::Graph(error.to_string()))?;

    let neighborhood = localmind_mcp::handle(
        &store,
        &localmind_mcp::GraphToolRequest::MemorySymbolNeighborhood {
            symbol: symbol.to_string(),
            depth: 1,
        },
    )
    .map_err(|error| LearningError::Graph(error.to_string()))?;
    let localmind_mcp::GraphToolResponse::Neighborhood {
        symbol: summary,
        neighbors,
    } = neighborhood
    else {
        return Err(LearningError::Graph("unexpected tool response".to_string()));
    };

    let mut report = SymbolReport {
        kind: summary.kind,
        qualified_name: summary.qualified_name,
        path: summary.path,
        skeleton: summary.skeleton,
        neighbors: neighbors
            .iter()
            .map(|neighbor| format!("{}  {}", neighbor.kind, neighbor.qualified_name))
            .collect(),
        ..SymbolReport::default()
    };

    if let Ok(localmind_mcp::GraphToolResponse::Coverage { tests, .. }) = localmind_mcp::handle(
        &store,
        &localmind_mcp::GraphToolRequest::MemorySymbolCoverage {
            symbol: symbol.to_string(),
        },
    ) {
        report.tests = tests
            .iter()
            .map(|test| test.qualified_name.clone())
            .collect();
    }

    if let Ok(localmind_mcp::GraphToolResponse::Knowledge { knowledge, .. }) = localmind_mcp::handle(
        &store,
        &localmind_mcp::GraphToolRequest::MemorySymbolKnowledge {
            symbol: symbol.to_string(),
        },
    ) {
        let bodies: Vec<(String, String)> = crate::memory_list(project_root)
            .unwrap_or_default()
            .into_iter()
            .map(|entry| (entry.id, entry.body))
            .collect();
        report.knowledge = knowledge
            .iter()
            .map(|anchor| {
                let snippet = bodies
                    .iter()
                    .find(|(id, _)| id == &anchor.memory_id)
                    .map(|(_, body)| body.chars().take(120).collect())
                    .unwrap_or_default();
                (anchor.memory_id.clone(), anchor.confidence, snippet)
            })
            .collect();
    }
    Ok(report)
}

/// Export format for the local graph artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportFormat {
    Json,
    Html,
}

/// Writes a redacted snapshot of the active graph to a local file. The
/// serialized graph passes through the host redaction stack before it touches
/// disk; nothing leaves the machine.
pub fn codegraph_export(
    project_root: &Path,
    destination: &Path,
    format: ExportFormat,
) -> Result<(), LearningError> {
    let store = GraphStore::open_project(project_root)
        .map_err(|error| LearningError::Graph(error.to_string()))?;
    let nodes = store
        .active_nodes()
        .map_err(|error| LearningError::Graph(error.to_string()))?;
    let edges = store
        .active_edges()
        .map_err(|error| LearningError::Graph(error.to_string()))?;

    let graph = serde_json::json!({
        "nodes": nodes,
        "edges": edges,
    });
    let serialized = serde_json::to_string_pretty(&graph)
        .map_err(|error| LearningError::Graph(error.to_string()))?;
    let redacted = localpilot_config::redact::redact(&serialized);

    let artifact = match format {
        ExportFormat::Json => redacted,
        ExportFormat::Html => {
            let escaped = redacted
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            format!(
                "<!doctype html>\n<html><head><meta charset=\"utf-8\">\
                 <title>Workspace code graph</title></head>\
                 <body><h1>Workspace code graph</h1><pre>{escaped}</pre></body></html>\n"
            )
        }
    };
    std::fs::write(destination, artifact)
        .map_err(|error| LearningError::Graph(error.to_string()))?;
    Ok(())
}

/// Reports the change impact of a unified diff against the indexed graph. The
/// host already has git access, so it passes the diff text (`git diff
/// --unified=0`); this adapter parses it into changed spans and asks the engine
/// for the bounded, risk-tiered impact. Read-only: it opens the existing graph
/// and adds no persistent state.
pub fn codegraph_impact(
    project_root: &Path,
    unified_diff: &str,
) -> Result<localmind_codegraph::ChangeImpact, LearningError> {
    let spans = parse_unified_diff(unified_diff);
    let store = GraphStore::open_project(project_root)
        .map_err(|error| LearningError::Graph(error.to_string()))?;
    localmind_codegraph::compute_impact(
        &store,
        &spans,
        localmind_codegraph::ImpactOptions::default(),
    )
    .map_err(|error| LearningError::Graph(error.to_string()))
}

/// Parses a `git diff --unified=0` into the new-side changed spans, keyed by
/// repo-relative forward-slash path. Pure (no git invocation), so the caller
/// controls how the diff is produced. New files and renames flow through the
/// `+++ b/<path>` header; pure deletions contribute the line they removed at.
fn parse_unified_diff(unified_diff: &str) -> Vec<localmind_codegraph::ChangedSpan> {
    let mut spans = Vec::new();
    let mut current_path: Option<String> = None;
    for line in unified_diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            current_path = new_side_path(rest);
        } else if line.starts_with("@@ ") {
            let (Some(path), Some((start, count))) = (current_path.as_ref(), hunk_new_range(line))
            else {
                continue;
            };
            spans.push(localmind_codegraph::ChangedSpan {
                path: path.clone(),
                line_start: start,
                line_end: start + count.saturating_sub(1),
            });
        }
    }
    spans
}

/// The repo-relative path from a `+++ b/path` header, or `None` for `/dev/null`.
fn new_side_path(header: &str) -> Option<String> {
    let path = header.split('\t').next().unwrap_or(header).trim();
    if path == "/dev/null" {
        return None;
    }
    let path = path.strip_prefix("b/").unwrap_or(path);
    Some(path.replace('\\', "/"))
}

/// The new-side `+start,count` of a `@@ -a,b +start,count @@` hunk header.
/// `count` defaults to 1 when omitted.
fn hunk_new_range(header: &str) -> Option<(u64, u64)> {
    let plus = header.split('+').nth(1)?;
    let range = plus.split([' ', '@']).next()?;
    let mut parts = range.split(',');
    let start: u64 = parts.next()?.parse().ok()?;
    let count: u64 = match parts.next() {
        Some(value) => value.parse().ok()?,
        None => 1,
    };
    Some((start, count))
}

#[cfg(test)]
mod tests {
    use super::{codegraph_export, codegraph_inspect, codegraph_reindex, ExportFormat};
    use std::fs;

    #[test]
    fn reindex_is_incremental_and_honours_exclusions() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let root = temp_dir.path();
        fs::write(
            root.join(".localmind.toml"),
            "[learning]\nenabled = true\nexcluded_paths = [\"private\"]\n",
        )?;
        fs::create_dir_all(root.join("src"))?;
        fs::create_dir_all(root.join("private"))?;
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u8 { 42 }\n")?;
        fs::write(root.join("private/secret.rs"), "pub fn hidden() {}\n")?;

        let first = codegraph_reindex(root, usize::MAX)?;
        assert_eq!(first.reindexed, 1);
        assert_eq!(first.rejected, 1);
        assert_eq!(first.remaining, 0);

        // Nothing changed: the second pass is a no-op.
        let second = codegraph_reindex(root, usize::MAX)?;
        assert_eq!(second.reindexed, 0);
        assert_eq!(second.unchanged, 1);

        // An edit is picked up; the budget bounds the pass.
        fs::write(
            root.join("src/lib.rs"),
            "pub fn answer() -> u8 { 41 + 1 }\n",
        )?;
        let third = codegraph_reindex(root, usize::MAX)?;
        assert_eq!(third.reindexed, 1);
        Ok(())
    }

    #[test]
    fn inspect_reports_neighbors_tests_and_knowledge() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let root = temp_dir.path();
        fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n")?;
        fs::create_dir_all(root.join("src"))?;
        fs::write(
            root.join("src/lib.rs"),
            r#"
pub fn answer() -> u8 { 42 }

#[cfg(test)]
mod tests {
    #[test]
    fn answer_is_right() {
        let value = super::answer();
        assert_eq!(value, 42);
    }
}
"#,
        )?;
        codegraph_reindex(root, usize::MAX)?;

        let report = codegraph_inspect(root, "answer")?;
        assert_eq!(report.qualified_name, "src/lib.rs::answer");
        assert_eq!(report.kind, "function");
        assert!(!report.neighbors.is_empty());
        assert_eq!(report.tests, vec!["src/lib.rs::tests::answer_is_right"]);
        Ok(())
    }

    #[test]
    fn export_is_local_and_redacted() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let root = temp_dir.path();
        fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n")?;
        fs::create_dir_all(root.join("src"))?;
        // A secret-shaped literal that ends up in a stored skeleton must not
        // survive the export gate.
        let secret = "sk-proj-abcdefghijklmnopqrstuvwxyz123456";
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u8 { 42 }\n")?;
        codegraph_reindex(root, usize::MAX)?;
        {
            use localmind_core::{
                content_fingerprint, Confidence, EvidenceKind, EvidenceRef, GraphNode, NodeKind,
            };
            let store = localmind_store::GraphStore::open_project(root)
                .map_err(|error| format!("open store: {error}"))?;
            let mut node = GraphNode::new(
                NodeKind::Function,
                "connect",
                "src/lib.rs::connect",
                content_fingerprint("connect"),
                EvidenceRef::new(EvidenceKind::CodeParse, "span"),
                Confidence::new(1.0)?,
            );
            node.skeleton = Some(format!("pub fn connect(key: &str /* {secret} */)"));
            store
                .upsert_node(&node)
                .map_err(|error| format!("upsert: {error}"))?;
        }

        let json_path = root.join("graph.json");
        codegraph_export(root, &json_path, ExportFormat::Json)?;
        let exported = fs::read_to_string(&json_path)?;
        assert!(!exported.contains(secret), "secret leaked into the export");
        assert!(exported.contains("src/lib.rs::answer"));

        let html_path = root.join("graph.html");
        codegraph_export(root, &html_path, ExportFormat::Html)?;
        let html = fs::read_to_string(&html_path)?;
        assert!(!html.contains(secret));
        assert!(html.starts_with("<!doctype html>"));
        Ok(())
    }

    #[test]
    fn parse_unified_diff_extracts_new_side_spans() {
        let diff = "\
diff --git a/src/core.rs b/src/core.rs
--- a/src/core.rs
+++ b/src/core.rs
@@ -1,0 +2,3 @@
@@ -10 +12 @@
";
        let spans = super::parse_unified_diff(diff);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].path, "src/core.rs");
        assert_eq!((spans[0].line_start, spans[0].line_end), (2, 4));
        assert_eq!((spans[1].line_start, spans[1].line_end), (12, 12));
    }

    #[test]
    fn impact_adapter_reports_callers_for_a_diff() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let root = temp_dir.path();
        fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n")?;
        fs::create_dir_all(root.join("src"))?;
        // `hub` on line 1; `a` calls it on line 2.
        fs::write(
            root.join("src/core.rs"),
            "pub fn hub() -> u8 { 1 }\nfn a() { hub(); }\n",
        )?;
        codegraph_reindex(root, usize::MAX)?;

        // A diff that touches line 1 (hub) must surface `a` as an impacted caller.
        let diff = "+++ b/src/core.rs\n@@ -1 +1 @@\n";
        let impact = super::codegraph_impact(root, diff)?;
        assert!(
            impact
                .changed
                .iter()
                .any(|symbol| symbol.qualified_name.ends_with("::hub")),
            "hub must be a changed symbol; got {:?}",
            impact.changed
        );
        assert!(
            impact
                .impacted
                .iter()
                .any(|symbol| symbol.qualified_name.ends_with("::a")),
            "a must be an impacted caller; got {:?}",
            impact.impacted
        );
        Ok(())
    }
}
