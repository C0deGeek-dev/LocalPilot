//! The searchable index over session transcript spans.
//!
//! # Where it lives, and why not somewhere else
//!
//! `.localmind/sessions/spans.sqlite` — beside the transcripts it derives from,
//! inside the project, which is the privacy posture's requirement that the index
//! never escape the project root.
//!
//! It is a **separate file** from the ingest chunk store, while following that
//! store's pattern exactly (a `PRAGMA user_version` stepper, an FTS5 virtual
//! table kept in step with a base table). Sharing the *pattern* is reuse;
//! sharing the *file* would couple two things whose lifecycles differ — spans
//! die when their session is deleted, chunks when their file changes — so a span
//! rebuild would put the document index at risk for no benefit. Both are derived
//! and disposable: deleting either loses nothing that cannot be rebuilt.
//!
//! # Contentless, and what that costs
//!
//! The FTS5 table stores **no text**. On a contentless table `snippet()`,
//! `highlight()` and selecting the body all return `NULL`; only `rowid` and
//! `bm25()` work. That is the point rather than a limitation to work around: a
//! content-carrying index can render a hit from its own second copy, which hides
//! a broken fetch path until someone follows a link. Here a broken fetch is an
//! immediate empty result. It is also four times smaller.
//!
//! `contentless_delete=1` restores ordinary `DELETE FROM`, which deletion
//! propagation needs — without it a delete must re-supply the text the index by
//! definition does not have. It has been available since SQLite 3.43 and the
//! bundled build here is 3.46, so this is safe; `contentless_unindexed` is
//! **not** available at 3.46 and is deliberately unused.
//!
//! # Locators
//!
//! A span is addressed by `session_id`, the chunking version that produced it,
//! and its ordinal. The version is part of the address on purpose: re-chunking
//! renumbers ordinals, so a locator issued under an older contract must resolve
//! to *nothing* rather than to whatever now occupies that slot. Returning the
//! wrong span silently is worse than returning none.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::ingest::fnv_hash_hex;
use crate::transcript::{read_transcript, SpanKind, TranscriptSchema, SPAN_CHUNKING_VERSION};

/// On-disk store file under `.localmind/sessions/`.
const SPANS_DB: &str = "spans.sqlite";
/// Highest schema version this build understands.
const SCHEMA_VERSION: i32 = 1;
/// Cap on candidate rows pulled from the FTS index for one query. Bounds query
/// memory on a corpus of hundreds of thousands of spans while sitting far above
/// any realistic result set.
const SEARCH_CANDIDATE_LIMIT: i64 = 512;

/// A failure from the span index.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SpanStoreError {
    /// The database could not be opened, migrated, or queried.
    #[error("span index {path}: {source}")]
    Sqlite {
        /// The database file involved.
        path: PathBuf,
        /// The underlying SQLite failure.
        source: rusqlite::Error,
    },
    /// A transcript could not be read from disk.
    #[error("read transcript {path}: {source}")]
    Read {
        /// The transcript involved.
        path: PathBuf,
        /// The underlying I/O failure.
        source: std::io::Error,
    },
    /// The index was asked to live outside the project it belongs to.
    ///
    /// Enforced where the path is used rather than where it is configured: a
    /// check that runs once can be true when it runs and false when it matters.
    #[error(
        "refusing a span index at {path}: it must live inside the project at {project_root}. \
         The index derives from session transcripts and may not be written outside the \
         project that produced them"
    )]
    OutsideProject {
        /// The rejected location.
        path: PathBuf,
        /// The project the index must stay within.
        project_root: PathBuf,
    },
    /// The store was built by a newer version of this software.
    #[error(
        "span index {path} was written by a newer build (schema v{found}, this build \
         understands v{supported}). Refusing to open it rather than risk corrupting it; \
         the index is derived, so deleting it and re-indexing is safe"
    )]
    SchemaTooNew {
        /// The database file involved.
        path: PathBuf,
        /// The version found on disk.
        found: i32,
        /// The highest version this build understands.
        supported: i32,
    },
}

/// Whether a project has a span index on disk.
///
/// Cheap and side-effect free: opening the store would create it, and "is there
/// anything to search" must not be a question that writes.
#[must_use]
pub fn has_span_index(project_root: &Path) -> bool {
    project_root
        .join(".localmind")
        .join("sessions")
        .join(SPANS_DB)
        .is_file()
}

/// The durable address of a span: `span:<session>:<version>:<ordinal>`.
///
/// The chunking version is part of the address rather than metadata beside it.
/// Re-chunking renumbers ordinals, so a locator issued under an older contract
/// must resolve to nothing rather than to whatever now occupies that slot.
#[must_use]
pub fn span_locator(hit: &SpanHit) -> String {
    format!(
        "span:{}:{}:{}",
        hit.session_id, hit.chunking_version, hit.ordinal
    )
}

/// Read a locator back into its parts. Returns `None` for anything that is not
/// a span locator, including a well-formed id belonging to another source.
#[must_use]
pub fn parse_span_locator(id: &str) -> Option<(String, u32, usize)> {
    let rest = id.strip_prefix("span:")?;
    let mut parts = rest.rsplitn(3, ':');
    let ordinal = parts.next()?.parse().ok()?;
    let version = parts.next()?.parse().ok()?;
    let session = parts.next()?;
    if session.is_empty() {
        return None;
    }
    Some((session.to_string(), version, ordinal))
}

/// Why a locator could not be resolved to text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpanMiss {
    /// The id is not a span locator at all.
    NotASpan,
    /// The locator was issued under a different chunking contract. Its ordinal
    /// no longer means what it meant, so resolving it would return the wrong
    /// span rather than none.
    StaleContract,
    /// The session is gone, or its transcript no longer holds that ordinal.
    Gone,
    /// The transcript has changed since indexing, so the span at that ordinal is
    /// no longer the span that was indexed.
    Changed,
}

/// A span resolved back to its text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchedSpan {
    /// The locator that produced it.
    pub locator: String,
    /// The session it came from.
    pub session_id: String,
    /// 1-based start line in the transcript.
    pub start_line: usize,
    /// 1-based end line, inclusive.
    pub end_line: usize,
    /// What the span contains.
    pub kind: SpanKind,
    /// The span text, as indexed.
    pub text: String,
}

/// One hit from the index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpanHit {
    /// The session the span came from.
    pub session_id: String,
    /// The span's position in that session, under `chunking_version`.
    pub ordinal: usize,
    /// The contract the span was indexed under.
    pub chunking_version: u32,
    /// What the span contains.
    pub kind: SpanKind,
    /// 1-based line range in the source transcript.
    pub start_line: usize,
    /// 1-based end line, inclusive.
    pub end_line: usize,
}

/// What one indexing pass did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IndexReport {
    /// Sessions examined.
    pub sessions_seen: usize,
    /// Sessions re-indexed because they were new or had changed.
    pub sessions_indexed: usize,
    /// Sessions skipped because nothing had changed.
    pub sessions_unchanged: usize,
    /// Sessions whose spans were removed because their transcript is gone.
    pub sessions_removed: usize,
    /// Spans written.
    pub spans_written: usize,
    /// Transcript lines that should have been records and were not.
    pub unparseable_lines: usize,
    /// Records understood structurally but of an unknown type.
    pub unrecognised_records: usize,
}

/// The span index for one project.
#[derive(Debug)]
pub struct SpanStore {
    connection: Connection,
    db_path: PathBuf,
}

impl SpanStore {
    /// Open (creating and migrating as needed) the span index for a project.
    ///
    /// # Errors
    /// Returns [`SpanStoreError`] when the index would fall outside the project,
    /// when it was written by a newer build, or when SQLite refuses.
    pub fn open(project_root: &Path) -> Result<Self, SpanStoreError> {
        let sessions_dir = project_root.join(".localmind").join("sessions");
        Self::open_at(project_root, &sessions_dir)
    }

    /// Open the index in an explicit directory, refusing one outside the project.
    ///
    /// # Errors
    /// As [`SpanStore::open`].
    pub fn open_at(project_root: &Path, directory: &Path) -> Result<Self, SpanStoreError> {
        let db_path = directory.join(SPANS_DB);
        if !is_inside(project_root, directory) {
            return Err(SpanStoreError::OutsideProject {
                path: db_path,
                project_root: project_root.to_path_buf(),
            });
        }
        if let Err(source) = std::fs::create_dir_all(directory) {
            return Err(SpanStoreError::Read {
                path: directory.to_path_buf(),
                source,
            });
        }
        let connection = Connection::open(&db_path).map_err(|source| SpanStoreError::Sqlite {
            path: db_path.clone(),
            source,
        })?;
        // A derived, rebuildable index does not need to survive a power cut: if
        // it is lost or torn, re-indexing recreates it from the transcripts. WAL
        // plus `synchronous = NORMAL` matches that, and together with a
        // transaction per session it is the difference between an fsync per span
        // and none.
        //
        // `busy_timeout` is the writer discipline: SQLite serialises writers, so
        // a second indexer waits its turn rather than failing. Without it a
        // concurrent run returns `SQLITE_BUSY` immediately, which reads like
        // corruption. Indexing one session takes milliseconds.
        let _ = connection.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;              PRAGMA busy_timeout = 5000;",
        );
        let store = Self {
            connection,
            db_path,
        };
        store.migrate()?;
        Ok(store)
    }

    fn sqlite_err(&self, source: rusqlite::Error) -> SpanStoreError {
        SpanStoreError::Sqlite {
            path: self.db_path.clone(),
            source,
        }
    }

    /// Step the schema forward, refusing a store from a newer build.
    fn migrate(&self) -> Result<(), SpanStoreError> {
        let mut current: i32 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|source| self.sqlite_err(source))?;
        if current > SCHEMA_VERSION {
            return Err(SpanStoreError::SchemaTooNew {
                path: self.db_path.clone(),
                found: current,
                supported: SCHEMA_VERSION,
            });
        }
        if current >= SCHEMA_VERSION {
            return Ok(());
        }
        if current < 1 {
            self.connection
                .execute_batch(
                    r"
                    CREATE TABLE IF NOT EXISTS session_spans (
                        span_row INTEGER PRIMARY KEY,
                        session_id TEXT NOT NULL,
                        ordinal INTEGER NOT NULL,
                        part INTEGER NOT NULL,
                        kind TEXT NOT NULL,
                        start_line INTEGER NOT NULL,
                        end_line INTEGER NOT NULL,
                        chunking_version INTEGER NOT NULL,
                        text_hash TEXT NOT NULL,
                        byte_len INTEGER NOT NULL,
                        UNIQUE(session_id, chunking_version, ordinal)
                    );
                    CREATE INDEX IF NOT EXISTS idx_session_spans_session
                        ON session_spans(session_id);
                    CREATE INDEX IF NOT EXISTS idx_session_spans_kind
                        ON session_spans(kind);
                    CREATE VIRTUAL TABLE IF NOT EXISTS session_spans_fts
                        USING fts5(body, tokenize='unicode61', content='',
                                   contentless_delete=1);
                    CREATE TABLE IF NOT EXISTS session_index_state (
                        session_id TEXT PRIMARY KEY,
                        source_hash TEXT NOT NULL,
                        source_bytes INTEGER NOT NULL,
                        chunking_version INTEGER NOT NULL,
                        schema TEXT NOT NULL,
                        spans INTEGER NOT NULL,
                        unparseable_lines INTEGER NOT NULL,
                        unrecognised_records INTEGER NOT NULL,
                        control_records INTEGER NOT NULL
                    );
                    ",
                )
                .map_err(|source| self.sqlite_err(source))?;
            current = 1;
        }
        debug_assert_eq!(current, SCHEMA_VERSION);
        self.connection
            .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .map_err(|source| self.sqlite_err(source))
    }

    /// Index every session under `sessions_dir`, skipping those that have not
    /// changed and dropping those whose transcript has gone.
    ///
    /// Sessions are visited in sorted order so a report is reproducible.
    ///
    /// # Errors
    /// Returns [`SpanStoreError`] when SQLite refuses. A transcript that cannot
    /// be read is counted and skipped, never fatal — one unreadable session must
    /// not cost the other eighty-six.
    pub fn index_sessions(&self, sessions_dir: &Path) -> Result<IndexReport, SpanStoreError> {
        let mut report = IndexReport::default();
        let mut present: Vec<String> = Vec::new();

        let mut entries: Vec<PathBuf> = std::fs::read_dir(sessions_dir)
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();

        for directory in entries {
            let Some(session_id) = directory.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let transcript = directory.join("transcript.redacted.txt");
            if !transcript.is_file() {
                continue;
            }
            present.push(session_id.to_string());
            report.sessions_seen += 1;
            let Ok(text) = std::fs::read_to_string(&transcript) else {
                continue;
            };
            let source_hash = fnv_hash_hex(text.as_bytes());
            if self.is_current(session_id, &source_hash)? {
                report.sessions_unchanged += 1;
                continue;
            }
            let read = read_transcript(&text);
            self.replace_session(session_id, &read.spans)?;
            self.record_state(session_id, &source_hash, text.len(), read.schema, &read)?;
            report.sessions_indexed += 1;
            report.spans_written += read.spans.len();
            report.unparseable_lines += read.recovery.unparseable_lines;
            report.unrecognised_records += read.recovery.unrecognised_records;
        }

        report.sessions_removed = self.forget_absent(&present)?;
        Ok(report)
    }

    /// Whether a session is already indexed from exactly this content, under the
    /// current chunking contract.
    ///
    /// The contract version is part of the comparison: after a chunker change,
    /// unchanged content still needs re-indexing, because its spans no longer
    /// mean what the stored ordinals say they mean.
    fn is_current(&self, session_id: &str, source_hash: &str) -> Result<bool, SpanStoreError> {
        let stored: Option<(String, i64)> = self
            .connection
            .query_row(
                "SELECT source_hash, chunking_version FROM session_index_state \
                 WHERE session_id = ?1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|source| self.sqlite_err(source))?;
        Ok(stored.is_some_and(|(hash, version)| {
            hash == source_hash && version == i64::from(SPAN_CHUNKING_VERSION)
        }))
    }

    /// Replace one session's spans wholesale.
    ///
    /// Delete-then-insert rather than a diff: a transcript is append-mostly but
    /// not guaranteed to be, and a wrong incremental update is a silently stale
    /// index. Re-chunking one session costs milliseconds.
    fn replace_session(
        &self,
        session_id: &str,
        spans: &[crate::transcript::Span],
    ) -> Result<(), SpanStoreError> {
        // One transaction for the whole session. Without it SQLite commits — and
        // fsyncs — once per span, which on a real corpus is tens of thousands of
        // round trips and turns a two-second job into one measured in minutes.
        self.connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|source| self.sqlite_err(source))?;
        let result = self.replace_session_inner(session_id, spans);
        // A session is replaced atomically: a failure part-way leaves the old
        // spans in place rather than half the new ones, so the index is never
        // silently partial.
        let finish = if result.is_ok() { "COMMIT" } else { "ROLLBACK" };
        self.connection
            .execute_batch(finish)
            .map_err(|source| self.sqlite_err(source))?;
        result
    }

    fn replace_session_inner(
        &self,
        session_id: &str,
        spans: &[crate::transcript::Span],
    ) -> Result<(), SpanStoreError> {
        self.forget_session(session_id)?;
        for span in spans {
            self.connection
                .execute(
                    "INSERT INTO session_spans (session_id, ordinal, part, kind, start_line, \
                     end_line, chunking_version, text_hash, byte_len) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        session_id,
                        i64::try_from(span.ordinal).unwrap_or(i64::MAX),
                        i64::try_from(span.part).unwrap_or(i64::MAX),
                        kind_name(span.kind),
                        i64::try_from(span.start_line).unwrap_or(i64::MAX),
                        i64::try_from(span.end_line).unwrap_or(i64::MAX),
                        i64::from(span.chunking_version),
                        fnv_hash_hex(span.text.as_bytes()),
                        i64::try_from(span.text.len()).unwrap_or(i64::MAX),
                    ],
                )
                .map_err(|source| self.sqlite_err(source))?;
            let span_row = self.connection.last_insert_rowid();
            self.connection
                .execute(
                    "INSERT INTO session_spans_fts (rowid, body) VALUES (?1, ?2)",
                    params![span_row, span.text],
                )
                .map_err(|source| self.sqlite_err(source))?;
        }
        Ok(())
    }

    /// Drop every span for one session, from both tables.
    fn forget_session(&self, session_id: &str) -> Result<usize, SpanStoreError> {
        self.connection
            .execute(
                "DELETE FROM session_spans_fts WHERE rowid IN \
                 (SELECT span_row FROM session_spans WHERE session_id = ?1)",
                params![session_id],
            )
            .map_err(|source| self.sqlite_err(source))?;
        let removed = self
            .connection
            .execute(
                "DELETE FROM session_spans WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(|source| self.sqlite_err(source))?;
        Ok(removed)
    }

    /// Remove every session the store knows about that is no longer on disk.
    ///
    /// The index never outlives its source: deleting a session must take its
    /// spans with it, or the index becomes a copy of deleted material.
    fn forget_absent(&self, present: &[String]) -> Result<usize, SpanStoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT session_id FROM session_index_state")
            .map_err(|source| self.sqlite_err(source))?;
        let known: Vec<String> = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|source| self.sqlite_err(source))?
            .filter_map(Result::ok)
            .collect();
        drop(statement);

        let mut removed = 0;
        for session_id in known {
            if present.iter().any(|value| value == &session_id) {
                continue;
            }
            self.forget_session(&session_id)?;
            self.connection
                .execute(
                    "DELETE FROM session_index_state WHERE session_id = ?1",
                    params![session_id],
                )
                .map_err(|source| self.sqlite_err(source))?;
            removed += 1;
        }
        Ok(removed)
    }

    /// Record what was indexed, including what could not be, so a session's
    /// recovery telemetry survives past the run that produced it.
    fn record_state(
        &self,
        session_id: &str,
        source_hash: &str,
        source_bytes: usize,
        schema: TranscriptSchema,
        read: &crate::transcript::TranscriptRead,
    ) -> Result<(), SpanStoreError> {
        self.connection
            .execute(
                "INSERT OR REPLACE INTO session_index_state (session_id, source_hash, \
                 source_bytes, chunking_version, schema, spans, unparseable_lines, \
                 unrecognised_records, control_records) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    session_id,
                    source_hash,
                    i64::try_from(source_bytes).unwrap_or(i64::MAX),
                    i64::from(SPAN_CHUNKING_VERSION),
                    schema_name(schema),
                    i64::try_from(read.spans.len()).unwrap_or(i64::MAX),
                    i64::try_from(read.recovery.unparseable_lines).unwrap_or(i64::MAX),
                    i64::try_from(read.recovery.unrecognised_records).unwrap_or(i64::MAX),
                    i64::try_from(read.recovery.control_records).unwrap_or(i64::MAX),
                ],
            )
            .map_err(|source| self.sqlite_err(source))?;
        Ok(())
    }

    /// Search the index, best match first.
    ///
    /// Returns locators, never text: the index holds none. A caller resolves a
    /// hit against its transcript.
    ///
    /// # Errors
    /// Returns [`SpanStoreError`] when SQLite refuses. A query FTS5 cannot parse
    /// yields no hits rather than an error — a malformed query from a model is
    /// an empty result, not a failure to report upward.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SpanHit>, SpanStoreError> {
        let cleaned = fts_query(query);
        if cleaned.is_empty() {
            return Ok(Vec::new());
        }
        let mut statement = self
            .connection
            .prepare(
                "SELECT s.session_id, s.ordinal, s.chunking_version, s.kind, s.start_line, \
                 s.end_line \
                 FROM session_spans_fts f \
                 JOIN session_spans s ON s.span_row = f.rowid \
                 WHERE session_spans_fts MATCH ?1 \
                 ORDER BY bm25(session_spans_fts) \
                 LIMIT ?2",
            )
            .map_err(|source| self.sqlite_err(source))?;
        let capped = i64::try_from(limit)
            .unwrap_or(SEARCH_CANDIDATE_LIMIT)
            .min(SEARCH_CANDIDATE_LIMIT);
        let rows = statement.query_map(params![cleaned, capped], |row| {
            Ok(SpanHit {
                session_id: row.get(0)?,
                ordinal: usize::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                chunking_version: u32::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                kind: kind_from_name(&row.get::<_, String>(3)?),
                start_line: usize::try_from(row.get::<_, i64>(4)?).unwrap_or(0),
                end_line: usize::try_from(row.get::<_, i64>(5)?).unwrap_or(0),
            })
        });
        match rows {
            Ok(rows) => Ok(rows.filter_map(Result::ok).collect()),
            // FTS5 rejects some inputs at query time (an unmatched quote, a bare
            // operator). That is an empty result, not an error to propagate.
            Err(_) => Ok(Vec::new()),
        }
    }

    /// Resolve many hits to their text in one pass, re-chunking each transcript
    /// at most once.
    ///
    /// The index is contentless, so text always comes from the transcript. Doing
    /// that per hit would re-read and re-chunk a 34 MiB file for every result
    /// from the same session; grouping by session makes the cost per *session*
    /// rather than per hit.
    ///
    /// A hit that cannot be resolved is `None` rather than an error: one moved
    /// or edited transcript must not cost the whole result set.
    #[must_use]
    pub fn span_texts(&self, hits: &[SpanHit]) -> Vec<Option<String>> {
        let mut out = vec![None; hits.len()];
        let mut sessions: Vec<&str> = hits.iter().map(|hit| hit.session_id.as_str()).collect();
        sessions.sort_unstable();
        sessions.dedup();
        for session_id in sessions {
            let Some(spans) = self.rechunk(session_id) else {
                continue;
            };
            for (index, hit) in hits.iter().enumerate() {
                if hit.session_id != session_id {
                    continue;
                }
                if hit.chunking_version != SPAN_CHUNKING_VERSION {
                    continue;
                }
                if let Some(span) = spans.get(hit.ordinal) {
                    out[index] = Some(span.text.clone());
                }
            }
        }
        out
    }

    /// Resolve one locator to its text, or say why it could not be.
    ///
    /// # Errors
    /// Returns [`SpanStoreError`] when SQLite refuses.
    pub fn fetch_span(
        &self,
        locator: &str,
    ) -> Result<Result<FetchedSpan, SpanMiss>, SpanStoreError> {
        let Some((session_id, version, ordinal)) = parse_span_locator(locator) else {
            return Ok(Err(SpanMiss::NotASpan));
        };
        if version != SPAN_CHUNKING_VERSION {
            return Ok(Err(SpanMiss::StaleContract));
        }
        let stored: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT kind, text_hash FROM session_spans                  WHERE session_id = ?1 AND chunking_version = ?2 AND ordinal = ?3",
                params![session_id, i64::from(version), i64::try_from(ordinal).unwrap_or(i64::MAX)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|source| self.sqlite_err(source))?;
        let Some((kind, text_hash)) = stored else {
            return Ok(Err(SpanMiss::Gone));
        };
        let Some(spans) = self.rechunk(&session_id) else {
            return Ok(Err(SpanMiss::Gone));
        };
        let Some(span) = spans.get(ordinal) else {
            return Ok(Err(SpanMiss::Gone));
        };
        // The index holds no text, so this is the only place the answer can be
        // checked against what was indexed. A transcript edited since indexing
        // yields a *different* span at the same ordinal, and returning it would
        // be a wrong answer wearing a correct locator.
        if fnv_hash_hex(span.text.as_bytes()) != text_hash {
            return Ok(Err(SpanMiss::Changed));
        }
        Ok(Ok(FetchedSpan {
            locator: locator.to_string(),
            session_id,
            start_line: span.start_line,
            end_line: span.end_line,
            kind: kind_from_name(&kind),
            text: span.text.clone(),
        }))
    }

    /// Re-derive one session's spans from its transcript.
    fn rechunk(&self, session_id: &str) -> Option<Vec<crate::transcript::Span>> {
        let sessions_dir = self.db_path.parent()?;
        let transcript = sessions_dir
            .join(session_id)
            .join("transcript.redacted.txt");
        let text = std::fs::read_to_string(transcript).ok()?;
        Some(read_transcript(&text).spans)
    }

    /// Forget what is known about a session's indexed state, so the next pass
    /// re-indexes it even though its content has not changed.
    ///
    /// The spans themselves are left alone: the next pass replaces them. This is
    /// the "index this again" operation — useful after a chunker change that did
    /// not bump the contract version, and it is also exactly the state a crash
    /// between writing spans and recording state leaves behind, which is why
    /// that ordering is the safe one.
    ///
    /// # Errors
    /// Returns [`SpanStoreError`] when SQLite refuses.
    pub fn forget_index_state(&self, session_id: &str) -> Result<(), SpanStoreError> {
        self.connection
            .execute(
                "DELETE FROM session_index_state WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(|source| self.sqlite_err(source))?;
        Ok(())
    }

    /// How many spans the index holds.
    ///
    /// # Errors
    /// Returns [`SpanStoreError`] when SQLite refuses.
    pub fn span_count(&self) -> Result<usize, SpanStoreError> {
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM session_spans", [], |row| row.get(0))
            .map_err(|source| self.sqlite_err(source))?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// How many sessions the index holds spans for.
    ///
    /// # Errors
    /// Returns [`SpanStoreError`] when SQLite refuses.
    pub fn session_count(&self) -> Result<usize, SpanStoreError> {
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM session_index_state", [], |row| {
                row.get(0)
            })
            .map_err(|source| self.sqlite_err(source))?;
        Ok(usize::try_from(count).unwrap_or(0))
    }
}

/// Whether `candidate` is inside `root`.
///
/// Compares normalised absolute paths, so `project/../elsewhere` does not pass
/// by looking like a child. Neither path needs to exist: the check runs before
/// the directory is created.
fn is_inside(root: &Path, candidate: &Path) -> bool {
    let root = normalise(root);
    let candidate = normalise(candidate);
    candidate.starts_with(&root)
}

/// Resolve `.` and `..` lexically, without touching the filesystem.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Words too common to discriminate between spans.
///
/// A term that appears in most spans admits nearly the whole corpus into the
/// candidate set. bm25 then ranks it near-zero, so it contributes nothing to
/// *ordering* — but it has already filled the result window, and the spans it
/// let in displace ones that matched on meaning.
///
/// This list exists because the retrieval-quality measurement found it: the
/// query `"the"` alone matched 8 of 13 spans, and a paraphrase query scored
/// perfect recall while matching only on `one`, `the` and `of`. The number said
/// the index handled paraphrase; it handled nothing.
///
/// Deliberately short. A long stopword list starts discarding terms that
/// discriminate in a technical corpus — `use`, `type`, `match` and `test` are
/// all ordinary English and all meaningful here.
const STOP_TERMS: &[&str] = &[
    "the", "and", "but", "for", "not", "with", "that", "this", "from", "into", "are", "was",
    "were", "have", "has", "had", "you", "your", "our", "its", "their", "there", "here", "what",
    "when", "where", "how", "why", "who", "all", "any", "can", "will", "would", "should", "could",
    "does", "did", "done", "get", "got", "let", "out", "one", "two", "some", "such", "than",
    "then", "them", "they", "about", "also", "just", "only", "very", "more", "most", "other",
    "others", "each", "every", "been", "being", "which", "while", "after", "before", "over",
    "under", "again", "still", "even", "make", "made", "may", "might", "must", "need", "want",
];

/// Reduce a caller's query to discriminating terms joined by `OR`.
///
/// A model writes prose, not FTS5 syntax, and prose contains characters FTS5
/// treats as operators. Stripping them means a malformed query returns poor
/// results rather than an error.
///
/// Terms in [`STOP_TERMS`] are dropped. A query made only of them reduces to
/// nothing and returns no results, which is the honest answer: it asked for
/// everything, and everything is not a result set.
fn fts_query(query: &str) -> String {
    let terms: Vec<String> = query
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|term| term.len() > 1)
        .map(str::to_lowercase)
        .filter(|term| !STOP_TERMS.contains(&term.as_str()))
        .map(|term| format!("\"{term}\""))
        .collect();
    terms.join(" OR ")
}

fn kind_name(kind: SpanKind) -> &'static str {
    match kind {
        SpanKind::UserMessage => "user",
        SpanKind::AssistantMessage => "assistant",
        SpanKind::Reasoning => "reasoning",
        SpanKind::ToolCall => "tool_call",
        SpanKind::ToolOutput => "tool_output",
        SpanKind::System => "system",
    }
}

fn kind_from_name(name: &str) -> SpanKind {
    match name {
        "user" => SpanKind::UserMessage,
        "reasoning" => SpanKind::Reasoning,
        "tool_call" => SpanKind::ToolCall,
        "tool_output" => SpanKind::ToolOutput,
        "system" => SpanKind::System,
        _ => SpanKind::AssistantMessage,
    }
}

fn schema_name(schema: TranscriptSchema) -> &'static str {
    match schema {
        TranscriptSchema::ClaudeJsonl => "claude_jsonl",
        TranscriptSchema::CodexJsonl => "codex_jsonl",
        TranscriptSchema::PlainText => "plain_text",
    }
}
