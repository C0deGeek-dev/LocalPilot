//! Expand and fetch layers of token-budgeted retrieval.
//!
//! Retrieval is staged so a caller (or the model) spends a few tokens to
//! *locate* the right memory before paying for any body. The cheap **index**
//! layer is `knowledge_search` (`ingest::search`); these two functions add the
//! next two stages on top of it:
//!
//! - **Expand** (cheap): the document neighbours around chosen ids.
//! - **Fetch** (the only expensive layer): full bodies for explicit ids.
//!
//! Every layer reports its token cost, so the budget being spent is visible.
//! The budgeted cross-source pack lives in `ingest::compute_pack` (the live
//! `knowledge_search` path); there is deliberately no second pack path here.

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::ingest::{self, IngestError};

/// Approximate token estimate: ~4 chars per token, floored at 1 for any
/// non-empty text, 0 for empty.
fn estimate_tokens(text: &str) -> u64 {
    let chars = text.chars().count() as u64;
    if chars == 0 {
        0
    } else {
        (chars / 4).max(1)
    }
}

/// Layer 2: the document neighbours around a chosen id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expansion {
    pub id: String,
    pub neighbor_ids: Vec<String>,
    pub token_cost: u64,
}

/// Layer 3: a full chunk body for an explicit id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchedBody {
    pub id: String,
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    pub body: String,
    pub token_cost: u64,
}

/// Layer 2: document neighbours around each id. Cheap (ids only). Read-only.
///
/// # Errors
/// Returns [`IngestError`] when the derived index cannot be read.
pub fn expand_layer(project_root: &Path, ids: &[String]) -> Result<Vec<Expansion>, IngestError> {
    let mut expansions = Vec::with_capacity(ids.len());
    for id in ids {
        let neighbor_ids = ingest::sibling_chunk_ids(project_root, id)?;
        let token_cost = estimate_tokens(&neighbor_ids.join(" "));
        expansions.push(Expansion {
            id: id.clone(),
            neighbor_ids,
            token_cost,
        });
    }
    Ok(expansions)
}

/// Layer 3: full bodies for an explicit set of ids — and only those ids.
/// Read-only.
///
/// # Errors
/// Returns [`IngestError`] when the derived index cannot be read.
pub fn fetch_layer(project_root: &Path, ids: &[String]) -> Result<Vec<FetchedBody>, IngestError> {
    let chunks = ingest::fetch_chunks(project_root, ids)?;
    Ok(chunks
        .into_iter()
        .map(|chunk| FetchedBody {
            token_cost: chunk.token_estimate.max(estimate_tokens(&chunk.text)),
            id: chunk.id,
            path: chunk.path,
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            body: chunk.text,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use localpilot_config::IngestConfig;

    /// Ingest a fixture project with two files, one of them large, and return its
    /// root.
    fn ingested_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        // A large file so its full body dwarfs a one-line index summary.
        let big = format!(
            "// widget module\n{}",
            "fn widget() {} // widget\n".repeat(400)
        );
        std::fs::write(dir.path().join("src/widget.rs"), big).unwrap();
        std::fs::write(
            dir.path().join("src/small.rs"),
            "// gadget helper\nfn gadget() -> u32 { 1 } // gadget\n",
        )
        .unwrap();
        ingest::run(dir.path(), &IngestConfig::default(), ingest::RunMode::Full).unwrap();
        dir
    }

    /// Ids come from the index layer (`knowledge_search`/`ingest::search`); the
    /// fetch layer must return bodies for exactly those ids and no others.
    #[test]
    fn fetch_returns_only_the_requested_ids() {
        let dir = ingested_project();
        let hits = ingest::search(dir.path(), "widget gadget").unwrap();
        assert!(hits.len() >= 2, "expected hits across both files");
        let wanted = vec![hits[0].chunk_id.clone()];

        let bodies = fetch_layer(dir.path(), &wanted).unwrap();
        assert_eq!(bodies.len(), 1, "fetch must return exactly the asked ids");
        assert_eq!(bodies[0].id, wanted[0]);
    }

    /// Expand echoes each requested id with its (cheap) neighbour list.
    #[test]
    fn expand_reports_each_requested_id() {
        let dir = ingested_project();
        let hits = ingest::search(dir.path(), "widget").unwrap();
        assert!(!hits.is_empty(), "expected an index hit for widget");
        let wanted = vec![hits[0].chunk_id.clone()];

        let expansions = expand_layer(dir.path(), &wanted).unwrap();
        assert_eq!(expansions.len(), 1);
        assert_eq!(expansions[0].id, wanted[0]);
    }
}
