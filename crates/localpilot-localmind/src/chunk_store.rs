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
/// v3 adds the additive nullable `language` column (language-tagged chunks +
/// language-filtered search). `NULL` = unknown/general = always eligible.
/// v4 adds the additive `ingest_chunk_vectors` table (best-effort chunk
/// embeddings), mirroring the accepted-memory `vector_index` shape.
const SCHEMA_VERSION: i32 = 4;
/// Cap on candidate rows pulled from the FTS index for one query, ordered by
/// relevance. Bounds query memory on a large corpus; far above any realistic
/// matched-set for a context pack, so it never changes small-fixture results.
const SEARCH_CANDIDATE_LIMIT: i64 = 512;
/// bm25 column weight for the `path` column — above the body so a query term that
/// names the file boosts that chunk (the principled replacement for the old
/// substring path-name bonus). Columns are (chunk_id UNINDEXED, path, text).
const BM25_PATH_WEIGHT: f64 = 5.0;
/// bm25 column weight for the chunk `text` column (the baseline).
const BM25_TEXT_WEIGHT: f64 = 1.0;

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
        if current < 3 {
            // Additive: the chunk's programming language. Existing rows default to
            // NULL (unknown ⇒ always eligible in the language filter); they are
            // re-tagged when re-ingested.
            self.connection
                .execute_batch("ALTER TABLE ingest_chunks ADD COLUMN language TEXT;")
                .map_err(|source| self.sqlite_err(source))?;
            current = 3;
        }
        if current < 4 {
            // Additive: a rebuildable chunk vector index, mirroring the
            // accepted-memory `vector_index` shape (LE-f32 BLOB, exact cosine in
            // Rust). Keyed by chunk id; `source_fingerprint` dedups re-embedding
            // of an unchanged chunk. Best-effort and disposable — absent rows just
            // mean those chunks were never embedded (keyword retrieval is intact).
            self.connection
                .execute_batch(
                    r#"
                    CREATE TABLE IF NOT EXISTS ingest_chunk_vectors (
                        chunk_id TEXT PRIMARY KEY,
                        source_fingerprint TEXT NOT NULL,
                        model TEXT NOT NULL,
                        dimensions INTEGER NOT NULL,
                        vector_blob BLOB NOT NULL,
                        updated_at TEXT NOT NULL
                    );
                    "#,
                )
                .map_err(|source| self.sqlite_err(source))?;
            current = 4;
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

    /// Drop every row — a full rebuild's clean slate. Chunk vectors are dropped
    /// with their chunks so no orphan vectors survive a rebuild.
    pub(crate) fn clear(&self) -> Result<(), IngestError> {
        self.connection
            .execute_batch(
                "DELETE FROM ingest_chunks; DELETE FROM ingest_chunks_fts; \
                 DELETE FROM ingest_chunk_vectors;",
            )
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
                     redaction_status, original_bytes, preview_bytes, superseded_by, language)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
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
                    superseded_by = excluded.superseded_by,
                    language = excluded.language
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
                    chunk.language,
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
    /// [`SEARCH_CANDIDATE_LIMIT`], each paired with its **bm25 relevance** (higher
    /// is better — the negated SQLite bm25, which is IDF-weighted so a common token
    /// like `and` contributes far less than a rare one). The `path` column is
    /// weighted above `text` ([`BM25_PATH_WEIGHT`]), so a term that names the
    /// file boosts that chunk without a separate substring pass. Returns the
    /// matching rows only — never the whole store.
    ///
    /// When `language` is `Some`, a chunk tagged with a *different* language is
    /// excluded while a `NULL`-tagged (unknown / general) chunk always remains
    /// eligible — the exact clause accepted-memory `search_lang` uses. The clause
    /// is appended **only** when filtering, so the unfiltered query shape is
    /// stable.
    pub(crate) fn search(
        &self,
        terms: &[String],
        language: Option<&str>,
    ) -> Result<Vec<(ChunkRecord, f64)>, IngestError> {
        let Some(match_expression) = fts_match_expression(terms) else {
            return Ok(Vec::new());
        };
        let language_clause = if language.is_some() {
            " AND (c.language = ?3 OR c.language IS NULL)"
        } else {
            ""
        };
        // Column order is (chunk_id UNINDEXED, path, text); bm25 weights are
        // positional, so the path column is weighted above the text column. bm25
        // is negative (more negative = better), so it is negated into a positive
        // "higher is better" relevance.
        let statement_sql = format!(
            r#"
                SELECT c.id, c.path, c.chunk_index, c.start_line, c.end_line, c.start_byte,
                       c.end_byte, c.content_hash, c.text, c.token_estimate, c.stale,
                       c.context_prefix, c.summary, c.redaction_status, c.original_bytes,
                       c.preview_bytes, c.superseded_by, c.language,
                       -bm25(ingest_chunks_fts, 0.0, {BM25_PATH_WEIGHT}, {BM25_TEXT_WEIGHT}) AS relevance
                FROM ingest_chunks_fts f
                JOIN ingest_chunks c ON c.id = f.chunk_id
                WHERE ingest_chunks_fts MATCH ?1{language_clause}
                ORDER BY relevance DESC, c.path, c.id
                LIMIT ?2
                "#,
        );
        let mut statement = self
            .connection
            .prepare(&statement_sql)
            .map_err(|source| self.sqlite_err(source))?;
        let map_row = |row: &rusqlite::Row<'_>| Ok((row_to_chunk(row)?, row.get::<_, f64>(18)?));
        let rows = if let Some(language) = language {
            statement.query_map(
                params![match_expression, SEARCH_CANDIDATE_LIMIT, language],
                map_row,
            )
        } else {
            statement.query_map(params![match_expression, SEARCH_CANDIDATE_LIMIT], map_row)
        }
        .map_err(|source| self.sqlite_err(source))?;
        let mut scored = Vec::new();
        for row in rows {
            scored.push(row.map_err(|source| self.sqlite_err(source))?);
        }
        Ok(scored)
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
        // Drop the vectors of the just-removed chunks so none are orphaned (the
        // chunk-vector table has no DB-level foreign key).
        tx.execute(
            "DELETE FROM ingest_chunk_vectors \
             WHERE chunk_id NOT IN (SELECT id FROM ingest_chunks)",
            [],
        )
        .map_err(|source| self.sqlite_err(source))?;
        tx.commit().map_err(|source| self.sqlite_err(source))?;
        Ok(removed)
    }

    /// The stored embedding fingerprint for a chunk, or `None` when the chunk has
    /// no vector yet — the re-embed signal. A chunk whose `source_fingerprint`
    /// already matches the text about to be embedded is skipped (no re-embed of
    /// unchanged content).
    pub(crate) fn vector_fingerprint(&self, chunk_id: &str) -> Result<Option<String>, IngestError> {
        self.connection
            .query_row(
                "SELECT source_fingerprint FROM ingest_chunk_vectors WHERE chunk_id = ?1",
                params![chunk_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|source| self.sqlite_err(source))
    }

    /// Insert or replace one chunk's embedding vector (LE-f32 BLOB, exact-cosine
    /// at search time), keyed by chunk id and stamped with the content fingerprint
    /// it was embedded from, the model, and the dimension count.
    pub(crate) fn upsert_chunk_vector(
        &self,
        chunk_id: &str,
        source_fingerprint: &str,
        model: &str,
        vector: &[f32],
        updated_at: &str,
    ) -> Result<(), IngestError> {
        let blob = encode_vector(vector);
        let dimensions = i64::try_from(vector.len()).unwrap_or(i64::MAX);
        self.connection
            .execute(
                "INSERT INTO ingest_chunk_vectors \
                    (chunk_id, source_fingerprint, model, dimensions, vector_blob, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(chunk_id) DO UPDATE SET \
                    source_fingerprint = excluded.source_fingerprint, \
                    model = excluded.model, \
                    dimensions = excluded.dimensions, \
                    vector_blob = excluded.vector_blob, \
                    updated_at = excluded.updated_at",
                params![
                    chunk_id,
                    source_fingerprint,
                    model,
                    dimensions,
                    blob,
                    updated_at
                ],
            )
            .map_err(|source| self.sqlite_err(source))?;
        Ok(())
    }

    /// Cosine-nearest fresh chunks to `query`, as `(chunk_id, score)` ordered by
    /// descending cosine and bounded by `limit`. Only **fresh** (non-stale) chunks
    /// with a stored vector of matching dimension are scored; tombstones never
    /// resurface as semantic hits. When `language` is `Some`, the same
    /// `(language = ? OR language IS NULL)` filter the keyword path uses applies,
    /// so the keyword and vector views agree on language eligibility.
    pub(crate) fn vector_search(
        &self,
        query: &[f32],
        limit: usize,
        language: Option<&str>,
    ) -> Result<Vec<(String, f32)>, IngestError> {
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let language_clause = if language.is_some() {
            " AND (c.language = ?1 OR c.language IS NULL)"
        } else {
            ""
        };
        let statement_sql = format!(
            "SELECT v.chunk_id, v.vector_blob \
             FROM ingest_chunk_vectors v \
             JOIN ingest_chunks c ON c.id = v.chunk_id \
             WHERE c.stale = 0{language_clause}"
        );
        let mut statement = self
            .connection
            .prepare(&statement_sql)
            .map_err(|source| self.sqlite_err(source))?;
        let map_row =
            |row: &rusqlite::Row<'_>| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?));
        let rows = if let Some(language) = language {
            statement.query_map(params![language], map_row)
        } else {
            statement.query_map([], map_row)
        }
        .map_err(|source| self.sqlite_err(source))?;
        let mut scored: Vec<(String, f32)> = Vec::new();
        for row in rows {
            let (chunk_id, blob) = row.map_err(|source| self.sqlite_err(source))?;
            let vector = decode_vector(&blob);
            if vector.len() != query.len() {
                continue;
            }
            scored.push((chunk_id, cosine_similarity(query, &vector)));
        }
        // Descending cosine, with chunk id as a stable tiebreak.
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(limit);
        Ok(scored)
    }

    /// Count of stored chunk vectors — reported after a run (embedded-vs-keyword)
    /// and used in tests.
    pub(crate) fn vector_count(&self) -> Result<usize, IngestError> {
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM ingest_chunk_vectors", [], |row| {
                row.get(0)
            })
            .map_err(|source| self.sqlite_err(source))?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Whether any chunk vectors exist — the cheap gate the search path uses to
    /// skip the query embed + vector pass entirely when nothing was embedded
    /// (embeddings off, never built, or the endpoint was down at ingest), keeping
    /// retrieval byte-identical to the keyword-only contract.
    pub(crate) fn has_vectors(&self) -> Result<bool, IngestError> {
        let found: Option<i64> = self
            .connection
            .query_row("SELECT 1 FROM ingest_chunk_vectors LIMIT 1", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|source| self.sqlite_err(source))?;
        Ok(found.is_some())
    }

    /// Fetch full chunk rows for an explicit set of ids — the layer-3 "fetch"
    /// step. Returns only rows whose id is in `ids` (never the whole store), so a
    /// batch fetch costs only what the caller asked for. Order is stable
    /// (path, chunk_index) for deterministic packing.
    pub(crate) fn fetch_by_ids(&self, ids: &[String]) -> Result<Vec<ChunkRecord>, IngestError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            r#"
            SELECT id, path, chunk_index, start_line, end_line, start_byte, end_byte,
                   content_hash, text, token_estimate, stale, context_prefix, summary,
                   redaction_status, original_bytes, preview_bytes, superseded_by, language
            FROM ingest_chunks WHERE id IN ({placeholders})
            ORDER BY path, chunk_index
            "#
        );
        let mut statement = self
            .connection
            .prepare(&sql)
            .map_err(|source| self.sqlite_err(source))?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(ids), row_to_chunk)
            .map_err(|source| self.sqlite_err(source))?;
        let mut chunks = Vec::new();
        for row in rows {
            chunks.push(row.map_err(|source| self.sqlite_err(source))?);
        }
        Ok(chunks)
    }

    /// The other chunk ids of the same file as `id`, in document order — the
    /// layer-2 "expand" neighbours. Empty when the id is unknown or the file has
    /// only one chunk. Cheap: ids only, no bodies loaded.
    pub(crate) fn sibling_ids(&self, id: &str) -> Result<Vec<String>, IngestError> {
        let path: Option<String> = self
            .connection
            .query_row(
                "SELECT path FROM ingest_chunks WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| self.sqlite_err(source))?;
        let Some(path) = path else {
            return Ok(Vec::new());
        };
        let mut statement = self
            .connection
            .prepare(
                "SELECT id FROM ingest_chunks WHERE path = ?1 AND id != ?2 ORDER BY chunk_index",
            )
            .map_err(|source| self.sqlite_err(source))?;
        let rows = statement
            .query_map(params![path, id], |row| row.get::<_, String>(0))
            .map_err(|source| self.sqlite_err(source))?;
        let mut siblings = Vec::new();
        for row in rows {
            siblings.push(row.map_err(|source| self.sqlite_err(source))?);
        }
        Ok(siblings)
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
                       redaction_status, original_bytes, preview_bytes, superseded_by, language
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
        language: row.get(17)?,
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

/// A query term must be at least this many characters to match as an FTS5
/// **prefix** (`"term"*`); shorter terms match a whole token **exactly**. A
/// 2-char term like `an` otherwise prefix-matches the token `and` (and `do`
/// matches `docker`, `documentation`, …), floating irrelevant chunks. Short terms
/// are not discriminating enough to wildcard.
const MIN_PREFIX_LEN: usize = 3;

/// Turns the (already lowercased) query terms into an FTS5 MATCH expression,
/// OR-ed together. A term of [`MIN_PREFIX_LEN`] or more characters becomes a
/// quoted prefix phrase (`"term"*`, so `pars` matches `parser`); a shorter term
/// becomes a quoted **exact** token (`"term"`, so `an` matches only the token
/// `an`, never `and`). Quoting neutralizes FTS5 query operators in user input;
/// embedded double quotes are doubled per FTS5 string rules. Returns `None` when
/// no non-empty terms remain.
fn fts_match_expression(terms: &[String]) -> Option<String> {
    let clauses: Vec<String> = terms
        .iter()
        .filter(|term| !term.is_empty())
        .map(|term| {
            let quoted = term.replace('"', "\"\"");
            if term.chars().count() >= MIN_PREFIX_LEN {
                format!("\"{quoted}\"*")
            } else {
                format!("\"{quoted}\"")
            }
        })
        .collect();
    if clauses.is_empty() {
        return None;
    }
    Some(clauses.join(" OR "))
}

/// Encode an embedding as a little-endian f32 BLOB — the same on-disk layout the
/// accepted-memory `vector_index` uses, so cosine is computed identically in Rust.
fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        blob.extend_from_slice(&value.to_le_bytes());
    }
    blob
}

/// Decode a little-endian f32 BLOB back into an embedding. A blob whose length is
/// not a multiple of 4 is treated as empty (it can never match a query's
/// dimension, so it is simply skipped at search time) rather than erroring — the
/// vector index is best-effort and rebuildable.
fn decode_vector(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// Exact cosine similarity, identical to the accepted-memory implementation, so
/// the keyword and vector views score the same way. Zero for a length mismatch or
/// a zero-norm vector.
fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut left_norm = 0.0_f32;
    let mut right_norm = 0.0_f32;
    for (l, r) in left.iter().zip(right.iter()) {
        dot += l * r;
        left_norm += l * l;
        right_norm += r * r;
    }
    if left_norm == 0.0 || right_norm == 0.0 {
        return 0.0;
    }
    dot / (left_norm.sqrt() * right_norm.sqrt())
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
            language: None,
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
        let hits = store.search(&["parser".to_string()], None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0.id, "c1");
        assert!(hits[0].1 > 0.0, "a match carries a positive bm25 relevance");

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
        let by_prefix = store.search(&["authentication".to_string()], None).unwrap();
        assert_eq!(by_prefix.len(), 1, "prefix terms must be searchable");
        assert_eq!(by_prefix[0].0.id, "c1");
        // The stored body is still the raw chunk, prefix kept in its own column.
        assert_eq!(by_prefix[0].0.text, "the token is refreshed on expiry");
        assert_eq!(
            by_prefix[0].0.context_prefix,
            "File docs/auth.md: Authentication Flow."
        );
        // The body itself still matches its own terms.
        assert_eq!(store.search(&["token".to_string()], None).unwrap().len(), 1);
    }

    #[test]
    fn language_filter_excludes_off_language_but_keeps_null_tagged() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::open(dir.path()).unwrap();
        let mut rust = chunk("rs", "a.rs", "h1", "parser mapping");
        rust.language = Some("rust".to_string());
        let mut python = chunk("py", "b.py", "h2", "parser mapping");
        python.language = Some("python".to_string());
        // A general (e.g. docs) chunk with no language tag stays eligible always.
        let general = chunk("gen", "c.md", "h3", "parser mapping");
        store.upsert_chunks(&[rust, python, general]).unwrap();

        // No filter: every matching chunk is returned (byte-identical to before).
        let unfiltered = store.search(&["parser".to_string()], None).unwrap();
        assert_eq!(unfiltered.len(), 3);

        // Filtered to rust: the python chunk is excluded; the rust chunk and the
        // NULL-tagged general chunk both remain.
        let rust_only = store.search(&["parser".to_string()], Some("rust")).unwrap();
        let ids: BTreeSet<String> = rust_only.into_iter().map(|(c, _)| c.id).collect();
        assert!(ids.contains("rs"), "the rust chunk must be kept");
        assert!(
            ids.contains("gen"),
            "a NULL-tagged chunk is always eligible"
        );
        assert!(
            !ids.contains("py"),
            "an off-language chunk must be excluded"
        );
    }

    #[test]
    fn a_short_term_matches_a_whole_token_not_a_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::open(dir.path()).unwrap();
        // A's only candidate token for "an" would be the prefix of "and"; B has the
        // standalone token "an".
        store
            .upsert_chunks(&[
                chunk("a", "a.md", "h1", "the pipeline builds and ships"),
                chunk("b", "b.md", "h2", "an apple keeps bugs away"),
            ])
            .unwrap();

        let ids: BTreeSet<String> = store
            .search(&["an".to_string()], None)
            .unwrap()
            .into_iter()
            .map(|(c, _)| c.id)
            .collect();
        assert!(ids.contains("b"), "the standalone token 'an' matches");
        assert!(
            !ids.contains("a"),
            "the 2-char term 'an' must not prefix-match the token 'and'"
        );
    }

    #[test]
    fn a_path_match_outranks_a_body_only_match() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::open(dir.path()).unwrap();
        // body-only match for "parser"…
        let body = chunk(
            "body",
            "notes.md",
            "h1",
            "the parser handles the input stream",
        );
        // …versus a path-only match (the term names the file, not the body).
        let named = chunk("named", "parser.md", "h2", "an overview document goes here");
        store.upsert_chunks(&[body, named]).unwrap();

        let hits = store.search(&["parser".to_string()], None).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].0.id, "named",
            "a chunk whose path names the term outranks a body-only match (bm25 path weight)"
        );
    }

    #[test]
    fn migrates_a_v2_database_by_adding_the_language_column() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(CHUNKS_DB);
        // Hand-build a v2 database: prefix column present, no language column.
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
                        superseded_by TEXT,
                        context_prefix TEXT NOT NULL DEFAULT ''
                    );
                    CREATE VIRTUAL TABLE ingest_chunks_fts
                        USING fts5(chunk_id UNINDEXED, path, text);
                    INSERT INTO ingest_chunks
                        (id, path, chunk_index, start_line, end_line, start_byte, end_byte,
                         content_hash, text, token_estimate)
                    VALUES ('old', 'a.md', 0, 1, 1, 0, 5, 'h1', 'legacy body', 1);
                    INSERT INTO ingest_chunks_fts(chunk_id, path, text)
                        VALUES ('old', 'a.md', 'legacy body');
                    PRAGMA user_version = 2;
                    "#,
                )
                .unwrap();
        }

        // Opening with the current build migrates v2 → v3.
        let store = ChunkStore::open(dir.path()).unwrap();
        let all = store.all_chunks().unwrap();
        assert_eq!(all.len(), 1, "the legacy row survives migration");
        assert_eq!(
            all[0].language, None,
            "migrated rows default to NULL language"
        );
        // The added column is usable and the NULL row stays eligible under a filter.
        let hits = store.search(&["legacy".to_string()], Some("rust")).unwrap();
        assert!(hits.iter().any(|(c, _)| c.id == "old"));
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
        let hits = store.search(&["new".to_string()], None).unwrap();
        assert!(hits.iter().any(|(c, _)| c.id == "new"));
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
