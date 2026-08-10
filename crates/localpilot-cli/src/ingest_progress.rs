//! Ingest progress, preflight, and result helpers for the full-screen host's
//! long-running ingest paths. The generic operation pump never learns
//! `IngestProgress`; it drains through a UI-agnostic closure that calls
//! [`drain_ingest_progress_with`].

use std::path::Path;

use localpilot_localmind::{IngestError, IngestProgress, JobStatus, RunMode, RunSummary};
use tokio::sync::mpsc;
use tokio::task::JoinError;

/// The loaded config, resolved run mode, and start notice for a proceeding ingest.
pub(crate) struct PreparedIngest {
    pub config: localpilot_config::IngestConfig,
    pub mode: RunMode,
    pub start_notice: String,
}

/// Whether an ingest request proceeds to the walk or exits early with a single
/// notice (config error, nothing to resume, already complete, unreadable status).
pub(crate) enum IngestPreflight {
    Proceed(PreparedIngest),
    EarlyExit(String),
}

/// Resolve the config, run mode, and start notice for an ingest request. Runs
/// before any Busy transition, so an `EarlyExit` never enters Busy or starts a
/// walk. Kept host-neutral so the operation pump owns the decision.
pub(crate) fn ingest_preflight(
    cwd: &Path,
    requested_mode: RunMode,
    resume: bool,
) -> IngestPreflight {
    let config = match crate::ingest_cmd::load_ingest_config(cwd) {
        Ok(config) => config,
        Err(error) => return IngestPreflight::EarlyExit(format!("ingest config error: {error}")),
    };
    // `resume` resolves the same decision the session-open trigger uses: resume an
    // interrupted job, rebuild, or report nothing-to-do.
    let mode = if resume {
        match localpilot_localmind::ingest_status(cwd) {
            Ok(Some(job)) => {
                let has_index = localpilot_localmind::has_chunk_index(cwd);
                match localpilot_localmind::planned_run_mode(Some(&job), has_index) {
                    Some(mode) => mode,
                    None => {
                        return IngestPreflight::EarlyExit(
                            "ingest job already completed; run /ingest refresh to update"
                                .to_string(),
                        );
                    }
                }
            }
            Ok(None) => return IngestPreflight::EarlyExit("no ingest job to resume".to_string()),
            Err(error) => {
                return IngestPreflight::EarlyExit(format!("ingest status unreadable: {error}"));
            }
        }
    } else {
        requested_mode
    };
    let mode_label = match mode {
        RunMode::Full => "full",
        RunMode::Refresh => "refresh",
    };
    IngestPreflight::Proceed(PreparedIngest {
        config,
        mode,
        start_notice: format!("ingesting project knowledge ({mode_label})…"),
    })
}

/// Drain queued ingestion progress, emitting one string per surfaced milestone.
/// Milestone stages emit once; per-file `Parsing` ticks are throttled to quarter
/// marks so a large walk does not flood the transcript. `total`/`bucket` carry the
/// throttle state across calls. `emit` is the host's notice sink
/// (full-screen `RuntimeUpdate::Notice`).
pub(crate) fn drain_ingest_progress_with(
    rx: &mut mpsc::UnboundedReceiver<IngestProgress>,
    total: &mut u64,
    bucket: &mut u64,
    mut emit: impl FnMut(String),
) {
    while let Ok(stage) = rx.try_recv() {
        match stage {
            IngestProgress::Discovering => emit("ingest: discovering files…".to_string()),
            IngestProgress::Discovered {
                candidates,
                skipped,
            } => {
                *total = candidates;
                emit(format!(
                    "ingest: {candidates} file(s) to parse ({skipped} skipped)"
                ));
            }
            IngestProgress::Parsing {
                completed,
                total: count,
            } => {
                *total = count;
                if count > 0 && completed > 0 {
                    let quarter = completed.saturating_mul(4) / count;
                    if quarter > *bucket {
                        *bucket = quarter;
                        emit(format!("ingest: parsed {completed}/{count} file(s)"));
                    }
                }
            }
            IngestProgress::Indexing => emit("ingest: indexing project context…".to_string()),
            IngestProgress::Writing => emit("ingest: writing index…".to_string()),
            // The caller posts the final summary line from the run result.
            IngestProgress::Completed { .. } => {}
        }
    }
}

/// Format the final ingest summary/error line from the joined walk result, shared
/// so every ingest caller prints it identically.
pub(crate) fn ingest_result_notice(
    result: Result<Result<RunSummary, IngestError>, JoinError>,
) -> String {
    match result {
        Ok(Ok(summary)) => {
            let interrupted =
                matches!(summary.job.status, JobStatus::Paused | JobStatus::Cancelled);
            let status = match summary.job.status {
                JobStatus::Completed => "completed",
                JobStatus::Paused => "paused",
                JobStatus::Cancelled => "cancelled",
                JobStatus::Failed => "failed",
                JobStatus::Running => "running",
                JobStatus::Queued => "queued",
            };
            let suffix = if interrupted {
                " — resume with /ingest resume"
            } else {
                ""
            };
            format!(
                "ingestion {status}: {} file(s), {} chunk(s){suffix}",
                summary.job.completed_files, summary.chunks_written
            )
        }
        Ok(Err(error)) => format!("ingestion failed: {error}"),
        Err(error) => format!("ingestion task error: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarter_mark_throttle_emits_only_on_bucket_crossings() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        for completed in 1..=8u64 {
            tx.send(IngestProgress::Parsing {
                completed,
                total: 8,
            })
            .unwrap();
        }
        drop(tx);
        let mut total = 0;
        let mut bucket = 0;
        let mut emitted = Vec::new();
        drain_ingest_progress_with(&mut rx, &mut total, &mut bucket, |m| emitted.push(m));
        assert_eq!(
            emitted,
            vec![
                "ingest: parsed 2/8 file(s)".to_string(),
                "ingest: parsed 4/8 file(s)".to_string(),
                "ingest: parsed 6/8 file(s)".to_string(),
                "ingest: parsed 8/8 file(s)".to_string(),
            ],
            "one notice per 25% crossing, no duplicates"
        );
        assert_eq!(total, 8);
    }

    #[test]
    fn preflight_early_exits_a_resume_with_no_job_without_entering_a_walk() {
        let dir = tempfile::tempdir().expect("temp dir");
        match ingest_preflight(dir.path(), RunMode::Full, true) {
            IngestPreflight::EarlyExit(notice) => assert_eq!(notice, "no ingest job to resume"),
            IngestPreflight::Proceed(_) => panic!("resume with no job must early-exit"),
        }
    }

    #[test]
    fn preflight_proceeds_for_a_fresh_run_with_the_shared_start_notice() {
        let dir = tempfile::tempdir().expect("temp dir");
        match ingest_preflight(dir.path(), RunMode::Full, false) {
            IngestPreflight::Proceed(prepared) => {
                assert!(matches!(prepared.mode, RunMode::Full));
                assert_eq!(prepared.start_notice, "ingesting project knowledge (full)…");
            }
            IngestPreflight::EarlyExit(notice) => panic!("fresh run must proceed, got: {notice}"),
        }
    }

    fn summary(status: JobStatus, files: u64, chunks: usize) -> RunSummary {
        RunSummary {
            job: localpilot_localmind::IngestJob {
                schema_version: 1,
                run_id: "test".to_string(),
                status,
                mode: "full".to_string(),
                queued_files: 0,
                completed_files: files,
                failed_files: 0,
                skipped_files: 0,
                started_unix: 0,
                updated_unix: 0,
                message: None,
            },
            manifest: localpilot_localmind::PreviewManifest {
                schema_version: 1,
                generated_unix: 0,
                project_root: String::new(),
                entries: Vec::new(),
                estimates: localpilot_localmind::BudgetEstimate::default(),
            },
            chunks_written: chunks,
            embedded_chunks: 0,
            doc_files_indexed: 0,
        }
    }

    #[test]
    fn result_notice_formats_status_and_the_paused_resume_suffix() {
        assert_eq!(
            ingest_result_notice(Ok(Ok(summary(JobStatus::Completed, 3, 12)))),
            "ingestion completed: 3 file(s), 12 chunk(s)"
        );
        assert_eq!(
            ingest_result_notice(Ok(Ok(summary(JobStatus::Paused, 1, 4)))),
            "ingestion paused: 1 file(s), 4 chunk(s) — resume with /ingest resume"
        );
        assert_eq!(
            ingest_result_notice(Ok(Ok(summary(JobStatus::Cancelled, 0, 0)))),
            "ingestion cancelled: 0 file(s), 0 chunk(s) — resume with /ingest resume"
        );
    }

    #[tokio::test]
    async fn shared_ingest_path_walks_a_tiny_fixture_end_to_end() {
        // A real ingest walk against a one-file temp corpus, driven through the exact
        // shared preflight → spawn_blocking(walk) → drain → result path `drive_ingest`
        // uses (minus the terminal pump), so real `IngestProgress` milestones and a real
        // `RunSummary` are surfaced through the shared notice helpers end-to-end.
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join(".localpilot.toml"),
            "[ingest]\nenabled = true\nmax_files = 100\nmax_run_bytes = 100000\nmax_tokens = 100000\n",
        )
        .expect("write config");
        std::fs::write(dir.path().join("README.md"), "parser guide\n").expect("write readme");

        let config = match ingest_preflight(dir.path(), RunMode::Full, false) {
            IngestPreflight::Proceed(prepared) => prepared.config,
            IngestPreflight::EarlyExit(notice) => panic!("fresh run must proceed: {notice}"),
        };
        let root = dir.path().to_path_buf();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = tokio::task::spawn_blocking(move || {
            localpilot_localmind::ingest_run_with_progress(
                &root,
                &config,
                RunMode::Full,
                &|| false,
                &mut |stage| {
                    let _ = tx.send(stage);
                },
            )
        });
        let result = handle.await;

        let mut total = 0_u64;
        let mut bucket = 0_u64;
        let mut notices = Vec::new();
        drain_ingest_progress_with(&mut rx, &mut total, &mut bucket, |m| notices.push(m));

        assert!(
            notices.iter().any(|n| n.starts_with("ingest: ")),
            "expected at least one progress milestone, got {notices:?}"
        );
        let summary_line = ingest_result_notice(result);
        assert!(
            summary_line.starts_with("ingestion completed: "),
            "expected a completed summary, got {summary_line:?}"
        );
        assert!(
            summary_line.contains("file(s), ") && summary_line.contains("chunk(s)"),
            "summary should carry real file + chunk counts, got {summary_line:?}"
        );
    }
}
