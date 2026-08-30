//! Ingest LocalPilot's own written research reports into LocalMind's
//! documentation index so they are semantically searchable and visible in the
//! LocalMind UI.
//!
//! This is a thin binding over `localmind_store::ingest_docs`: it ensures the
//! project store exists, then chunks and ingests every Markdown report under the
//! research output directory into the project's `doc_chunk` index. It is
//! opt-in at the call site (the `[research] ingest_report` config) — nothing
//! here runs unless the host asks for it.

use std::path::Path;

use localmind_store::ingest_docs;

use crate::error::LearningError;

pub use localmind_store::DocIngestSummary;

/// One ingested documentation file, flattened for host presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocFileSummary {
    pub path: String,
    pub chunks: i64,
}

/// Read-only documentation-index orientation for a host UI.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DocIndexSummary {
    pub chunks: i64,
    pub vectors: i64,
    pub files: Vec<DocFileSummary>,
}

/// Chunk and ingest every Markdown file under `docs_dir` into `project_root`'s
/// LocalMind documentation index. Idempotent: unchanged report text is a no-op,
/// edited text re-embeds in place. Returns what was touched.
pub fn ingest_research_docs(
    project_root: &Path,
    docs_dir: &Path,
) -> Result<DocIngestSummary, LearningError> {
    // Make sure the project store/config exists before opening it (mirrors the
    // review-queue bridge), so a first-ever research run ingests cleanly.
    crate::initialize(project_root).map_err(|e| LearningError::Review(e.to_string()))?;
    ingest_docs(docs_dir, project_root).map_err(|e| LearningError::Review(e.to_string()))
}

/// The documentation-index counts a host diagnostic needs to tell "report
/// ingestion never ran" apart from "indexed without embeddings": how many doc
/// passages the project store holds and how many of them carry a vector.
/// Best-effort — `None` when no usable store exists at `project_root`.
#[must_use]
pub fn doc_index_counts(project_root: &Path) -> Option<(i64, i64)> {
    let persistence = localmind_store::MemoryPersistence::open_project(project_root).ok()?;
    let chunks = persistence.doc_chunk_count().ok()?;
    let vectors = persistence.doc_vector_count().ok()?;
    Some((chunks, vectors))
}

/// Browse the existing documentation index without creating a database.
///
/// # Errors
/// Returns [`LearningError::Memory`] when an existing database cannot be read.
pub fn doc_index_summary(project_root: &Path) -> Result<DocIndexSummary, LearningError> {
    if !crate::store_database_exists(project_root) {
        return Ok(DocIndexSummary::default());
    }
    let persistence = localmind_store::MemoryPersistence::open_project(project_root)
        .map_err(|error| LearningError::Memory(error.to_string()))?;
    let chunks = persistence
        .doc_chunk_count()
        .map_err(|error| LearningError::Memory(error.to_string()))?;
    let vectors = persistence
        .doc_vector_count()
        .map_err(|error| LearningError::Memory(error.to_string()))?;
    let files = persistence
        .doc_files()
        .map_err(|error| LearningError::Memory(error.to_string()))?
        .into_iter()
        .map(|(path, chunks)| DocFileSummary { path, chunks })
        .collect();
    Ok(DocIndexSummary {
        chunks,
        vectors,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_report_lands_in_the_project_doc_chunk_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let docs = root.join(".localpilot").join("research");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(
            docs.join("caching.md"),
            "# Caching\n\nCaches speed up repeated reads.\n",
        )
        .unwrap();

        let summary = ingest_research_docs(root, &docs).unwrap();
        assert_eq!(summary.files, 1, "the one report file is walked");
        assert!(summary.chunks >= 1, "its heading section becomes a chunk");
        assert!(
            summary.total_in_index >= 1,
            "the chunk is in the project index"
        );
    }

    #[test]
    fn an_empty_research_dir_ingests_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let docs = root.join(".localpilot").join("research");
        std::fs::create_dir_all(&docs).unwrap();

        let summary = ingest_research_docs(root, &docs).unwrap();
        assert_eq!(summary.files, 0);
        assert_eq!(summary.chunks, 0);
        assert_eq!(summary.total_in_index, 0);
    }

    #[test]
    fn reading_docs_from_a_bare_project_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            doc_index_summary(dir.path()).unwrap(),
            DocIndexSummary::default()
        );
        assert!(!dir.path().join(".localmind").exists());
        assert!(!dir.path().join(crate::CONFIG_FILE).exists());
    }
}
