//! Binding layer for the `/research` mode and `localpilot research` subcommand.
//!
//! The host-neutral loop lives in `localpilot-research`; this module supplies
//! the concrete local [`Source`]s over LocalPilot's retrieval primitives and
//! the run orchestrator that renders a report artefact and enqueues
//! review-gated memory candidates. Web research is added separately and stays
//! off by default (`policies/remote-egress.md`).

use std::io::Write;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use localpilot_config::{CliOverrides, ConfigPaths};
use localpilot_research::{
    candidates_from, render_markdown, run_research, Bounds, Evidence, HeuristicSynthesizer,
    Provenance, ResearchReport, Source, SourceError, SourceSet,
};

/// Confidence attached to research-derived memory candidates: low, because they
/// are machine-derived and unreviewed — they route to review, never accepted.
const RESEARCH_CANDIDATE_CONFIDENCE: f32 = 0.3;

/// Evidence snippets to take from each source per sub-question.
const PER_SOURCE_EVIDENCE: usize = 5;

/// Resolved options for a research run.
pub struct ResearchOptions {
    /// Maximum sub-questions the run may pursue.
    pub max_questions: usize,
    /// Directory the report artefact is written to.
    pub output_dir: PathBuf,
    /// Whether to write the report artefact.
    pub write_report: bool,
    /// Whether to enqueue review-gated memory candidates.
    pub enqueue_memory: bool,
}

/// Build run options from the `[research]` config. Returns `None` when the
/// research surface is disabled (`[research].enabled = false`).
pub fn options_from_config(
    root: &Path,
    write_report: bool,
    enqueue_memory: bool,
) -> anyhow::Result<Option<ResearchOptions>> {
    let config = localpilot_config::load(&ConfigPaths::standard(root), &CliOverrides::default())?;
    if !config.research.enabled {
        return Ok(None);
    }
    let output_dir = config.research.output_dir.clone().map_or_else(
        || root.join(".localpilot").join("research"),
        |dir| root.join(dir),
    );
    Ok(Some(ResearchOptions {
        max_questions: config.research.max_questions.max(1),
        output_dir,
        write_report,
        enqueue_memory,
    }))
}

/// Run a local research pass for `topic`: gather across local sources,
/// synthesise, then (per options) write a report artefact and enqueue
/// review-gated memory candidates. A short human summary is written to `out`.
pub async fn run_local_research(
    root: &Path,
    topic: &str,
    options: &ResearchOptions,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let sources = build_local_sources(root);
    let bounds = Bounds {
        max_questions: options.max_questions,
        per_source_evidence: PER_SOURCE_EVIDENCE,
    };
    let outcome = run_research(topic, &sources, &HeuristicSynthesizer, bounds).await?;

    for error in &outcome.source_errors {
        writeln!(out, "note: {error}")?;
    }
    if options.write_report {
        let path = write_report(&options.output_dir, topic, &outcome.report)?;
        writeln!(out, "report: {}", path.display())?;
    }
    if options.enqueue_memory {
        let enqueued = enqueue_candidates(root, &outcome.report)?;
        writeln!(out, "memory candidates enqueued for review: {enqueued}")?;
    }
    writeln!(
        out,
        "findings: {}  open questions: {}",
        outcome.report.findings.len(),
        outcome.report.open_questions.len()
    )?;
    Ok(())
}

/// Assemble the local source set: ingested knowledge + accepted memory.
fn build_local_sources(root: &Path) -> SourceSet {
    SourceSet::new()
        .with(Box::new(KnowledgeSource {
            root: root.to_path_buf(),
        }))
        .with(Box::new(MemorySource {
            root: root.to_path_buf(),
        }))
}

struct KnowledgeSource {
    root: PathBuf,
}

#[async_trait]
impl Source for KnowledgeSource {
    fn label(&self) -> &str {
        "knowledge"
    }
    async fn gather(&self, question: &str, limit: usize) -> Result<Vec<Evidence>, SourceError> {
        let hits = localpilot_localmind::knowledge_search(&self.root, question)
            .map_err(|error| SourceError::new("knowledge", error.to_string()))?;
        Ok(hits
            .into_iter()
            .take(limit)
            .map(|hit| map_knowledge_hit(question, &hit))
            .collect())
    }
}

fn map_knowledge_hit(question: &str, hit: &localpilot_localmind::KnowledgeHit) -> Evidence {
    Evidence {
        question: question.to_string(),
        snippet: hit.snippet.clone(),
        provenance: Provenance::new(
            "knowledge",
            Some(format!("{}:{}-{}", hit.path, hit.start_line, hit.end_line)),
        ),
    }
}

struct MemorySource {
    root: PathBuf,
}

#[async_trait]
impl Source for MemorySource {
    fn label(&self) -> &str {
        "memory"
    }
    async fn gather(&self, question: &str, limit: usize) -> Result<Vec<Evidence>, SourceError> {
        let hits = localpilot_localmind::search_readonly(&self.root, question)
            .map_err(|error| SourceError::new("memory", error.to_string()))?;
        Ok(hits
            .into_iter()
            .take(limit)
            .map(|hit| map_memory_hit(question, &hit))
            .collect())
    }
}

fn map_memory_hit(question: &str, hit: &localpilot_localmind::SearchHit) -> Evidence {
    Evidence {
        question: question.to_string(),
        snippet: hit.snippet.clone(),
        provenance: Provenance::new("memory", Some(hit.memory_id.clone())),
    }
}

/// Render the report and write it (redacted) to `dir`, returning the path.
fn write_report(dir: &Path, topic: &str, report: &ResearchReport) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.md", slugify(topic)));
    let body = localpilot_config::redact::redact(&render_markdown(report));
    std::fs::write(&path, body)?;
    Ok(path)
}

/// Map supported, backed findings to review-queue candidates and enqueue them
/// through the existing review-gated path. Returns the number enqueued. Never
/// writes accepted memory directly.
fn enqueue_candidates(root: &Path, report: &ResearchReport) -> anyhow::Result<usize> {
    let mut enqueued = 0;
    for spec in candidates_from(report, RESEARCH_CANDIDATE_CONFIDENCE) {
        let body = format!(
            "{}\n\n(research finding; sources: {})",
            spec.body,
            provenance_summary(&spec.provenance)
        );
        let lesson = localpilot_localmind::RetrospectiveLesson::new(
            localpilot_config::redact::redact(&body),
        );
        if localpilot_localmind::write_retrospective_lesson(root, &lesson)?.is_some() {
            enqueued += 1;
        }
    }
    Ok(enqueued)
}

fn provenance_summary(provenance: &[Provenance]) -> String {
    provenance
        .iter()
        .map(|p| match &p.locator {
            Some(locator) => format!("{}:{locator}", p.source),
            None => p.source.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Turn a topic into a filesystem-safe slug. Falls back to `research` when the
/// topic has no alphanumeric characters.
fn slugify(topic: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in topic.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(60).collect();
    if slug.is_empty() {
        "research".to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_research::{ClaimStatus, Finding};

    fn knowledge_hit() -> localpilot_localmind::KnowledgeHit {
        localpilot_localmind::KnowledgeHit {
            chunk_id: "c1".to_string(),
            path: "src/lib.rs".to_string(),
            score: 10,
            start_line: 4,
            end_line: 9,
            content_hash: "h".to_string(),
            stale: false,
            snippet: "fn foo() {}".to_string(),
            token_estimate: 5,
            inclusion_reason: "match".to_string(),
            skip_reason: None,
        }
    }

    fn memory_hit() -> localpilot_localmind::SearchHit {
        localpilot_localmind::SearchHit {
            memory_id: "mem_7".to_string(),
            score: 3,
            path: "memory/7.md".to_string(),
            snippet: "prefer X over Y".to_string(),
            category: "guidance".to_string(),
            cosine: None,
        }
    }

    #[test]
    fn knowledge_hit_maps_to_path_line_provenance() {
        let evidence = map_knowledge_hit("how", &knowledge_hit());
        assert_eq!(evidence.snippet, "fn foo() {}");
        assert_eq!(evidence.provenance.source, "knowledge");
        assert_eq!(
            evidence.provenance.locator.as_deref(),
            Some("src/lib.rs:4-9")
        );
    }

    #[test]
    fn memory_hit_maps_to_id_provenance() {
        let evidence = map_memory_hit("how", &memory_hit());
        assert_eq!(evidence.provenance.source, "memory");
        assert_eq!(evidence.provenance.locator.as_deref(), Some("mem_7"));
    }

    #[test]
    fn slugify_is_filesystem_safe() {
        assert_eq!(slugify("Tokio select! macro"), "tokio-select-macro");
        assert_eq!(slugify("  spaced  "), "spaced");
        assert_eq!(slugify("***"), "research");
    }

    #[test]
    fn write_report_writes_rendered_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let mut report = ResearchReport::new("caching");
        report.findings = vec![Finding {
            statement: "caches speed reads".to_string(),
            status: ClaimStatus::Supported,
            supporting: vec![Provenance::new("memory", Some("mem_1".to_string()))],
        }];
        let path = write_report(dir.path(), "caching", &report).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# Research: caching"));
        assert!(body.contains("caches speed reads"));
        assert!(path.ends_with("caching.md"));
    }
}
