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
}
