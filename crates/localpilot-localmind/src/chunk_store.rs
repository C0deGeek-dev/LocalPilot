//! Indexed, low-RAM storage for derived ingest chunks.
//!
//! Replaces the monolithic `chunks.json` (fully deserialized into memory and
//! linearly scanned on every search and refresh) with an embedded SQLite store
//! and an FTS5 index, so search narrows to the matching rows through the index
//! and refresh updates only the paths that changed. The store lives at
//! `.localmind/ingest/chunks.sqlite`; it is derived and disposable — `ingest
//! rebuild` recreates it from source. The schema mirrors the proven
//! accepted-memory store pattern (a `PRAGMA user_version` stepper plus an FTS5
//! virtual table kept in sync with the base table).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::ingest::{ChunkRecord, IngestError};

/// On-disk store file under `.localmind/ingest/`.
const CHUNKS_DB: &str = "chunks.sqlite";
/// Legacy JSON index migrated in on first open of an existing project.
const LEGACY_CHUNKS_FILE: &str = "chunks.json";
/// Highest schema version this build understands.
/// v2 adds the additive `context_prefix` column (contextual chunk prefixing).
const SCHEMA_VERSION: i32 = 2;
/// Cap on candidate rows pulled from the FTS index for one query, ordered by
/// relevance. Bounds query memory on a large corpus; far above any realistic
/// matched-set for a context pack, so it never changes small-fixture results.
const SEARCH_CANDIDATE_LIMIT: i64 = 512;

/// The indexed chunk store for one project's ingest directory.
pub(crate) struct ChunkStore {
    connection: Connection,
    db_path: PathBuf,
}

impl ChunkStore {
    /// Open (creating and migrating as needed) the chunk store under
    /// `ingest_dir`. When the store is new and a legacy `chunks.json` exists, its
    /// rows are migrated in once and the JSON file is removed so the database is
    /// the single source of truth.
    ///
    /// # Errors
    /// Returns [`IngestError`] when the database cannot be opened, migrated, or
    /// seeded from a legacy index.
    pub(crate) fn open(ingest_dir: &Path) -> Result<Self, IngestError> {
        let db_path = ingest_dir.join(CHUNKS_DB);
        let connection = Connection::open(&db_path).map_err(|source| IngestError::Sqlite {
            path: db_path.clone(),
            source,
        })?;
        let store = Self {
            connection,
            db_path,
        };
        store.migrate()?;
        store.migrate_legacy_json(ingest_dir)?;
        Ok(store)
    }

    fn sqlite_err(&self, source: rusqlite::Error) -> IngestError {
        IngestError::Sqlite {
            path: self.db_path.clone(),
            source,
        }
    }

    /// Step the schema forward one version at a time, so a fresh database and a
    /// pre-existing one converge on the same shape. Each step is additive; a
    /// database newer than this build is refused upstream in [`Self::open`]'s
    /// caller via the shared `user_version` discipline.
    fn migrate(&self) -> Result<(), IngestError> {
        let mut current: i32 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|source| self.sqlite_err(source))?;
        if current >= SCHEMA_VERSION {
            return Ok(());
        }
        if current < 1 {
            self.connection
                .execute_batch(
                    r#"
                    CREATE TABLE IF NOT EXISTS ingest_chunks (
                        id TEXT PRIMARY KEY,
                        path TEXT NOT NULL,
                        chunk_index INTEGER NOT NULL,
                        start_line INTEGER NOT NULL,
                        end_line INTEGER NOT NULL,
                        start_byte INTEGER NOT NULL,
                        end_byte INTEGER NOT NULL,
                        content_hash TEXT NOT NULL,
                        text TEXT NOT NULL,
                        token_estimate INTEGER NOT NULL,
                        stale INTEGER NOT NULL DEFAULT 0,
                        summary TEXT NOT NULL DEFAULT '',
                        redaction_status TEXT NOT NULL DEFAULT '',
                        original_bytes INTEGER NOT NULL DEFAULT 0,
                        preview_bytes INTEGER NOT NULL DEFAULT 0,
                        superseded_by TEXT
                    );
                    CREATE INDEX IF NOT EXISTS idx_ingest_chunks_path
                        ON ingest_chunks(path);
                    CREATE VIRTUAL TABLE IF NOT EXISTS ingest_chunks_fts
                        USING fts5(chunk_id UNINDEXED, path, text);
                    "#,
                )
                .map_err(|source| self.sqlite_err(source))?;
            current = 1;
        }
        if current < 2 {
            // Additive: the offline contextual prefix. Existing rows default to
            // empty; their FTS text is unchanged until they are re-ingested.
            self.connection
                .execute_batch(
                    "ALTER TABLE ingest_chunks \
                     ADD COLUMN context_prefix TEXT NOT NULL DEFAULT '';",
                )
                .map_err(|source| self.sqlite_err(source))?;
            current = 2;
        }
        let _ = current;
        self.connection
            .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .map_err(|source| self.sqlite_err(source))?;
        Ok(())
    }

    /// One-time migration from the legacy `chunks.json` index. Seeds the rows
    /// (preserving their `stale`/`superseded_by` flags), then removes the JSON so
    /// the database is authoritative.
    fn migrate_legacy_json(&self, ingest_dir: &Path) -> Result<(), IngestError> {
        if self.count()? != 0 {
            return Ok(());
        }
        let legacy = ingest_dir.join(LEGACY_CHUNKS_FILE);
        if !legacy.exists() {
            return Ok(());
        }
        let text = std::fs::read_to_string(&legacy).map_err(|source| IngestError::Io {
            path: legacy.clone(),
            source,
        })?;
        let chunks: Vec<ChunkRecord> =
            serde_json::from_str(&text).map_err(|source| IngestError::Json {
                path: legacy.clone(),
                source: Box::new(source),
            })?;
        self.upsert_chunks(&chunks)?;
        std::fs::remove_file(&legacy).map_err(|source| IngestError::Io {
            path: legacy,
            source,
        })?;
        Ok(())
    }

    /// Drop every row — a full rebuild's clean slate.
    pub(crate) fn clear(&self) -> Result<(), IngestError> {
        self.connection
            .execute_batch("DELETE FROM ingest_chunks; DELETE FROM ingest_chunks_fts;")
            .map_err(|source| self.sqlite_err(source))?;
        Ok(())
    }

    /// Insert or replace chunks by id, keeping the FTS index in sync. Each row is
    /// written with the chunk's own `stale`/`superseded_by` flags. One
    /// transaction so the base and FTS rows never diverge.
    pub(crate) fn upsert_chunks(&self, chunks: &[ChunkRecord]) -> Result<(), IngestError> {
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(|source| self.sqlite_err(source))?;
        for chunk in chunks {
            tx.execute(
                "DELETE FROM ingest_chunks_fts WHERE chunk_id = ?1",
                params![chunk.id],
            )
            .map_err(|source| self.sqlite_err(source))?;
            tx.execute(
                r#"
                INSERT INTO ingest_chunks
                    (id, path, chunk_index, start_line, end_line, start_byte, end_byte,
                     content_hash, text, token_estimate, stale, context_prefix, summary,
                     redaction_status, original_bytes, preview_bytes, superseded_by)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
                ON CONFLICT(id) DO UPDATE SET
                    path = excluded.path,
                    chunk_index = excluded.chunk_index,
                    start_line = excluded.start_line,
                    end_line = excluded.end_line,
                    start_byte = excluded.start_byte,
                    end_byte = excluded.end_byte,
                    content_hash = excluded.content_hash,
                    text = excluded.text,
                    token_estimate = excluded.token_estimate,
                    stale = excluded.stale,
                    context_prefix = excluded.context_prefix,
                    summary = excluded.summary,
                    redaction_status = excluded.redaction_status,
                    original_bytes = excluded.original_bytes,
                    preview_bytes = excluded.preview_bytes,
                    superseded_by = excluded.superseded_by
                "#,
                params![
                    chunk.id,
                    chunk.path,
                    chunk.chunk_index,
                    to_i64(chunk.start_line),
                    to_i64(chunk.end_line),
                    to_i64(chunk.start_byte),
                    to_i64(chunk.end_byte),
                    chunk.content_hash,
                    chunk.text,
                    to_i64(chunk.token_estimate),
                    i64::from(chunk.stale),
                    chunk.context_prefix,
                    chunk.summary,
                    chunk.redaction_status,
                    to_i64(chunk.original_bytes),
                    to_i64(chunk.preview_bytes),
                    chunk.superseded_by,
                ],
            )
            .map_err(|source| self.sqlite_err(source))?;
            // The prefixed text — context prefix then the chunk body — is what
            // the FTS index sees, so a chunk split mid-thought still matches its
            // document's subject. The stored `text` stays the raw chunk.
            tx.execute(
                "INSERT INTO ingest_chunks_fts(chunk_id, path, text) VALUES (?1, ?2, ?3)",
                params![chunk.id, chunk.path, prefixed_text(chunk)],
            )
            .map_err(|source| self.sqlite_err(source))?;
        }
        tx.commit().map_err(|source| self.sqlite_err(source))?;
        Ok(())
    }

    /// Whether a fresh (non-stale) chunk for this exact `path:content_hash`
    /// already exists — the reuse signal for an unchanged file on refresh.
    pub(crate) fn has_fresh(&self, path: &str, content_hash: &str) -> Result<bool, IngestError> {
        let found: Option<i64> = self
            .connection
            .query_row(
                "SELECT 1 FROM ingest_chunks WHERE path = ?1 AND content_hash = ?2 AND stale = 0 LIMIT 1",
                params![path, content_hash],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| self.sqlite_err(source))?;
        Ok(found.is_some())
    }

    /// A changed file: tombstone the path's prior fresh rows (whose hash differs
    /// from the new one), pointing them at the new content hash, before the new
    /// rows are upserted. Stale rows are kept, not deleted, so search can still
    /// surface them tagged.
    pub(crate) fn mark_path_changed(&self, path: &str, new_hash: &str) -> Result<(), IngestError> {
        self.connection
            .execute(
                "UPDATE ingest_chunks SET stale = 1, superseded_by = ?2 \
                 WHERE path = ?1 AND content_hash != ?2 AND stale = 0",
                params![path, new_hash],
            )
            .map_err(|source| self.sqlite_err(source))?;
        Ok(())
    }

    /// A removed file (no longer a candidate): its fresh rows become stale with
    /// no successor.
    pub(crate) fn mark_path_removed(&self, path: &str) -> Result<(), IngestError> {
        self.connection
            .execute(
                "UPDATE ingest_chunks SET stale = 1, superseded_by = NULL \
                 WHERE path = ?1 AND stale = 0",
                params![path],
            )
            .map_err(|source| self.sqlite_err(source))?;
        Ok(())
    }

    /// Mark every fresh row whose path is no longer among `present_paths` stale.
    /// Distinct paths are cheap to list (no chunk bodies loaded), so this stays
    /// low-RAM on a large index.
    pub(crate) fn stale_removed_paths(
        &self,
        present_paths: &BTreeSet<String>,
    ) -> Result<(), IngestError> {
        for path in self.distinct_paths()? {
            if !present_paths.contains(&path) {
                self.mark_path_removed(&path)?;
            }
        }
        Ok(())
    }

    fn distinct_paths(&self) -> Result<Vec<String>, IngestError> {
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT path FROM ingest_chunks WHERE stale = 0")
            .map_err(|source| self.sqlite_err(source))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|source| self.sqlite_err(source))?;
        let mut paths = Vec::new();
        for row in rows {
            paths.push(row.map_err(|source| self.sqlite_err(source))?);
        }
        Ok(paths)
    }

    /// Candidate rows for `terms`, narrowed through the FTS index and bounded by
    /// [`SEARCH_CANDIDATE_LIMIT`], ordered by relevance. Returns the matching
    /// rows only — never the whole store — so the caller scores a small set.
    pub(crate) fn search(&self, terms: &[String]) -> Result<Vec<ChunkRecord>, IngestError> {
        let Some(match_expression) = fts_match_expression(terms) else {
            return Ok(Vec::new());
        };
        let mut statement = self
            .connection
            .prepare(
                r#"
                SELECT c.id, c.path, c.chunk_index, c.start_line, c.end_line, c.start_byte,
                       c.end_byte, c.content_hash, c.text, c.token_estimate, c.stale,
                       c.context_prefix, c.summary, c.redaction_status, c.original_bytes,
                       c.preview_bytes, c.superseded_by
                FROM ingest_chunks_fts f
                JOIN ingest_chunks c ON c.id = f.chunk_id
                WHERE ingest_chunks_fts MATCH ?1
                ORDER BY bm25(ingest_chunks_fts), c.path, c.id
                LIMIT ?2
                "#,
            )
            .map_err(|source| self.sqlite_err(source))?;
        let rows = statement
            .query_map(params![match_expression, SEARCH_CANDIDATE_LIMIT], |row| {
                row_to_chunk(row)
            })
            .map_err(|source| self.sqlite_err(source))?;
        let mut chunks = Vec::new();
        for row in rows {
            chunks.push(row.map_err(|source| self.sqlite_err(source))?);
        }
        Ok(chunks)
    }

    /// Delete every chunk for a path or a single chunk id, keeping FTS in sync.
    /// Returns the number of rows removed.
    pub(crate) fn forget(&self, target: &str) -> Result<usize, IngestError> {
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(|source| self.sqlite_err(source))?;
        tx.execute(
            "DELETE FROM ingest_chunks_fts WHERE chunk_id IN \
             (SELECT id FROM ingest_chunks WHERE path = ?1 OR id = ?1)",
            params![target],
        )
        .map_err(|source| self.sqlite_err(source))?;
        let removed = tx
            .execute(
                "DELETE FROM ingest_chunks WHERE path = ?1 OR id = ?1",
                params![target],
            )
            .map_err(|source| self.sqlite_err(source))?;
        tx.commit().map_err(|source| self.sqlite_err(source))?;
        Ok(removed)
    }

    /// Total rows, including stale tombstones — the index size reported as
    /// `chunks_written`.
    pub(crate) fn count(&self) -> Result<usize, IngestError> {
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM ingest_chunks", [], |row| row.get(0))
            .map_err(|source| self.sqlite_err(source))?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Every chunk, for tests and verification (not used on the runtime search
    /// path).
    #[cfg(test)]
    pub(crate) fn all_chunks(&self) -> Result<Vec<ChunkRecord>, IngestError> {
        let mut statement = self
            .connection
            .prepare(
                r#"
                SELECT id, path, chunk_index, start_line, end_line, start_byte, end_byte,
                       content_hash, text, token_estimate, stale, context_prefix, summary,
                       redaction_status, original_bytes, preview_bytes, superseded_by
                FROM ingest_chunks ORDER BY path, chunk_index
                "#,
            )
            .map_err(|source| self.sqlite_err(source))?;
        let rows = statement
            .query_map([], row_to_chunk)
            .map_err(|source| self.sqlite_err(source))?;
        let mut chunks = Vec::new();
        for row in rows {
            chunks.push(row.map_err(|source| self.sqlite_err(source))?);
        }
        Ok(chunks)
    }
}

/// Whether a chunk store already exists for this ingest directory.
pub(crate) fn exists(ingest_dir: &Path) -> bool {
    ingest_dir.join(CHUNKS_DB).exists()
}

fn row_to_chunk(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkRecord> {
    Ok(ChunkRecord {
        id: row.get(0)?,
        path: row.get(1)?,
        chunk_index: row.get(2)?,
        start_line: from_i64(row.get(3)?),
        end_line: from_i64(row.get(4)?),
        start_byte: from_i64(row.get(5)?),
        end_byte: from_i64(row.get(6)?),
        content_hash: row.get(7)?,
        text: row.get(8)?,
        token_estimate: from_i64(row.get(9)?),
        stale: row.get::<_, i64>(10)? != 0,
        context_prefix: row.get(11)?,
        summary: row.get(12)?,
        redaction_status: row.get(13)?,
        original_bytes: from_i64(row.get(14)?),
        preview_bytes: from_i64(row.get(15)?),
        superseded_by: row.get(16)?,
    })
}

/// The text indexed for full-text search: the context prefix (when present)
/// followed by the chunk body. An empty prefix yields the raw body, so legacy
/// rows index exactly as before.
fn prefixed_text(chunk: &ChunkRecord) -> String {
    if chunk.context_prefix.is_empty() {
        chunk.text.clone()
    } else {
        format!("{}\n{}", chunk.context_prefix, chunk.text)
    }
}

/// Turns the (already lowercased, non-empty) query terms into an FTS5 MATCH
/// expression: each term becomes a quoted prefix phrase (`"term"*`), OR-ed
/// together. Quoting neutralizes FTS5 query operators in user input; embedded
/// double quotes are doubled per FTS5 string rules. Returns `None` for no terms.
fn fts_match_expression(terms: &[String]) -> Option<String> {
    if terms.is_empty() {
        return None;
    }
    let expression = terms
        .iter()
        .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ");
    Some(expression)
}

/// `u64` span/count → `i64` for SQLite. Values this large never occur for line
/// or byte spans, so a saturating clamp is a safe, panic-free guard.
fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn from_i64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn chunk(id: &str, path: &str, hash: &str, text: &str) -> ChunkRecord {
        ChunkRecord {
            id: id.to_string(),
            path: path.to_string(),
            chunk_index: 0,
            start_line: 1,
            end_line: 1,
            start_byte: 0,
            end_byte: text.len() as u64,
            content_hash: hash.to_string(),
            text: text.to_string(),
            token_estimate: 1,
            stale: false,
            context_prefix: String::new(),
            summary: String::new(),
            redaction_status: "redacted".to_string(),
            original_bytes: text.len() as u64,
            preview_bytes: text.len() as u64,
            superseded_by: None,
        }
    }

    #[test]
    fn schema_migrates_and_round_trips_a_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::open(dir.path()).unwrap();
        store
            .upsert_chunks(&[chunk("c1", "a.md", "h1", "alpha parser guide")])
            .unwrap();
        assert_eq!(store.count().unwrap(), 1);
        let hits = store.search(&["parser".to_string()]).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "c1");

        // Re-open: rows persist and migration is idempotent.
        drop(store);
        let reopened = ChunkStore::open(dir.path()).unwrap();
        assert_eq!(reopened.count().unwrap(), 1);
    }

    #[test]
    fn the_context_prefix_is_indexed_so_a_chunk_matches_its_documents_subject() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::open(dir.path()).unwrap();
        let mut prefixed = chunk(
            "c1",
            "docs/auth.md",
            "h1",
            "the token is refreshed on expiry",
        );
        prefixed.context_prefix = "File docs/auth.md: Authentication Flow.".to_string();
        store.upsert_chunks(&[prefixed]).unwrap();

        // The body never says "authentication"; only the indexed prefix does.
        let by_prefix = store.search(&["authentication".to_string()]).unwrap();
        assert_eq!(by_prefix.len(), 1, "prefix terms must be searchable");
        assert_eq!(by_prefix[0].id, "c1");
        // The stored body is still the raw chunk, prefix kept in its own column.
        assert_eq!(by_prefix[0].text, "the token is refreshed on expiry");
        assert_eq!(
            by_prefix[0].context_prefix,
            "File docs/auth.md: Authentication Flow."
        );
        // The body itself still matches its own terms.
        assert_eq!(store.search(&["token".to_string()]).unwrap().len(), 1);
    }

    #[test]
    fn migrates_a_preexisting_v1_database_by_adding_the_prefix_column() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(CHUNKS_DB);
        // Hand-build a v1 database: the old schema with no context_prefix column.
        {
            let connection = Connection::open(&db_path).unwrap();
            connection
                .execute_batch(
                    r#"
                    CREATE TABLE ingest_chunks (
                        id TEXT PRIMARY KEY,
                        path TEXT NOT NULL,
                        chunk_index INTEGER NOT NULL,
                        start_line INTEGER NOT NULL,
                        end_line INTEGER NOT NULL,
                        start_byte INTEGER NOT NULL,
                        end_byte INTEGER NOT NULL,
                        content_hash TEXT NOT NULL,
                        text TEXT NOT NULL,
                        token_estimate INTEGER NOT NULL,
                        stale INTEGER NOT NULL DEFAULT 0,
                        summary TEXT NOT NULL DEFAULT '',
                        redaction_status TEXT NOT NULL DEFAULT '',
                        original_bytes INTEGER NOT NULL DEFAULT 0,
                        preview_bytes INTEGER NOT NULL DEFAULT 0,
                        superseded_by TEXT
                    );
                    CREATE VIRTUAL TABLE ingest_chunks_fts
                        USING fts5(chunk_id UNINDEXED, path, text);
                    INSERT INTO ingest_chunks
                        (id, path, chunk_index, start_line, end_line, start_byte, end_byte,
                         content_hash, text, token_estimate)
                    VALUES ('old', 'a.md', 0, 1, 1, 0, 5, 'h1', 'legacy body', 1);
                    INSERT INTO ingest_chunks_fts(chunk_id, path, text)
                        VALUES ('old', 'a.md', 'legacy body');
                    PRAGMA user_version = 1;
                    "#,
                )
                .unwrap();
        }

        // Opening with the current build migrates the v1 database to v2.
        let store = ChunkStore::open(dir.path()).unwrap();
        assert_eq!(
            store.count().unwrap(),
            1,
            "the legacy row survives migration"
        );
        let all = store.all_chunks().unwrap();
        assert_eq!(all[0].context_prefix, "", "migrated rows default to empty");
        // The added column is usable: a new prefixed row round-trips.
        let mut fresh = chunk("new", "b.md", "h2", "fresh body");
        fresh.context_prefix = "File b.md: New.".to_string();
        store.upsert_chunks(&[fresh]).unwrap();
        let hits = store.search(&["new".to_string()]).unwrap();
        assert!(hits.iter().any(|hit| hit.id == "new"));
    }

    #[test]
    fn migrates_legacy_chunks_json_then_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join(LEGACY_CHUNKS_FILE);
        let mut stale_chunk = chunk("c1", "a.md", "h1", "legacy text");
        stale_chunk.stale = true;
        stale_chunk.superseded_by = Some("h2".to_string());
        std::fs::write(&legacy, serde_json::to_string(&[stale_chunk]).unwrap()).unwrap();

        let store = ChunkStore::open(dir.path()).unwrap();

        let all = store.all_chunks().unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].stale, "migration preserves the stale flag");
        assert_eq!(all[0].superseded_by.as_deref(), Some("h2"));
        assert!(
            !legacy.exists(),
            "the legacy json is removed once migrated into the db"
        );
        assert!(exists(dir.path()), "the sqlite store is now present");
    }
}
