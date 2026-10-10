//! Session close-out into LocalMind.
//!
//! The pre-turn context hook now lives in `localpilot-localmind`
//! (`register_context_hook`); this module keeps the host-side session close-out
//! that runs on exit.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The same frames the TUI spinner uses, so exit-time progress looks like the
/// session that just ended.
const SPINNER: [char; 4] = ['◐', '◓', '◑', '◒'];

/// Run `work` with a live progress line on stderr, so a slow close-out stage
/// (model-backed lesson extraction can take a while against a busy local
/// server) is announced instead of looking like a hang. On a TTY the label
/// animates in place and is cleared when the stage finishes — the stage's own
/// summary line then prints on a clean row. On a non-TTY stderr the label
/// prints once, keeping captured logs clean.
fn with_progress<T>(label: &str, work: impl FnOnce() -> T) -> T {
    if !std::io::stderr().is_terminal() {
        eprintln!("{label} …");
        return work();
    }
    let running = Arc::new(AtomicBool::new(true));
    let spinner = {
        let running = Arc::clone(&running);
        let label = label.to_string();
        std::thread::spawn(move || {
            let mut frame = 0usize;
            let mut err = std::io::stderr();
            while running.load(Ordering::Relaxed) {
                let _ = write!(err, "\r{} {label} …", SPINNER[frame % SPINNER.len()]);
                let _ = err.flush();
                frame = frame.wrapping_add(1);
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
            // Clear the animated line so whatever prints next starts clean.
            let _ = write!(err, "\r{}\r", " ".repeat(label.chars().count() + 4));
            let _ = err.flush();
        })
    };
    let value = work();
    running.store(false, Ordering::Relaxed);
    let _ = spinner.join();
    value
}

/// A close-out stage, separated from its host-specific presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseOutStage {
    Lessons,
    Graph,
    Primer,
}

impl CloseOutStage {
    fn progress_label(self) -> &'static str {
        match self {
            Self::Lessons => "learning: checking the session for lessons",
            Self::Graph => "learning: updating the code graph",
            Self::Primer => "learning: refreshing the repo primer",
        }
    }

    fn skipped_label(self) -> &'static str {
        match self {
            Self::Lessons => "closeout",
            Self::Graph => "code graph reindex",
            Self::Primer => "primer distillation",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloseOutEvent {
    Session {
        candidates: usize,
        enqueued: usize,
        accepted: usize,
    },
    Graph {
        reindexed: usize,
        pruned: usize,
        remaining: usize,
    },
    PrimerEnqueued,
    Skipped {
        stage: CloseOutStage,
        reason: String,
    },
}

impl CloseOutEvent {
    pub(crate) fn is_warning(&self) -> bool {
        matches!(self, Self::Skipped { .. })
    }

    pub(crate) fn stderr_line(&self) -> String {
        match self {
            Self::Session { candidates, enqueued, accepted } => format!(
                "learning: closed out session — {candidates} candidate(s), {enqueued} enqueued, {accepted} auto-accepted"
            ),
            Self::Graph { reindexed, pruned, remaining } => format!(
                "learning: code graph updated — {reindexed} file(s) reindexed, {pruned} pruned{}",
                if *remaining > 0 { ", more queued for next session" } else { "" }
            ),
            Self::PrimerEnqueued => "learning: repo primer enqueued for review".to_string(),
            Self::Skipped { stage, reason } => format!("learning: {} skipped ({reason})", stage.skipped_label()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloseOutUpdate {
    Stage(CloseOutStage),
    Event(CloseOutEvent),
    Finished,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CloseOutReport {
    pub(crate) events: Vec<CloseOutEvent>,
}

/// The plain CLI presentation retains its existing stderr summaries and spinner.
pub(crate) fn close_out_stderr(cwd: &Path, session: localpilot_core::SessionId) -> CloseOutReport {
    close_out_impl(cwd, session, true, &mut |update| {
        if let CloseOutUpdate::Event(event) = update {
            if !matches!(
                event,
                CloseOutEvent::Graph {
                    reindexed: 0,
                    pruned: 0,
                    ..
                }
            ) {
                eprintln!("{}", event.stderr_line());
            }
        }
    })
}

/// Report stage progress and results through the owning host, never stderr.
pub(crate) fn close_out(
    cwd: &Path,
    session: localpilot_core::SessionId,
    mut update: impl FnMut(CloseOutUpdate),
) -> CloseOutReport {
    close_out_impl(cwd, session, false, &mut update)
}

fn close_out_stage<T>(
    stage: CloseOutStage,
    stderr: bool,
    update: &mut impl FnMut(CloseOutUpdate),
    work: impl FnOnce() -> T,
) -> T {
    update(CloseOutUpdate::Stage(stage));
    // Primer never had a spinner in the plain host; retain that behavior.
    if stderr && stage != CloseOutStage::Primer {
        with_progress(stage.progress_label(), work)
    } else {
        work()
    }
}

fn close_out_impl(
    cwd: &Path,
    session: localpilot_core::SessionId,
    stderr: bool,
    update: &mut impl FnMut(CloseOutUpdate),
) -> CloseOutReport {
    let store = localpilot_store::Store::open(cwd);
    if store
        .read_transcript(session)
        .map(|m| m.is_empty())
        .unwrap_or(true)
    {
        return CloseOutReport::default();
    }
    let mut report = CloseOutReport::default();
    let mut record = |event: CloseOutEvent, update: &mut dyn FnMut(CloseOutUpdate)| {
        report.events.push(event.clone());
        update(CloseOutUpdate::Event(event));
    };
    let closeout = close_out_stage(CloseOutStage::Lessons, stderr, update, || {
        localpilot_localmind::closeout_session(cwd, &store, session)
    });
    match closeout {
        Ok(summary) => {
            let _ = localpilot_localmind::record_active_session(cwd, &summary.session_id);
            record(
                CloseOutEvent::Session {
                    candidates: summary.candidate_count,
                    enqueued: summary.enqueued_count,
                    accepted: summary.accepted_count,
                },
                update,
            );
        }
        Err(error) => record(
            CloseOutEvent::Skipped {
                stage: CloseOutStage::Lessons,
                reason: error.to_string(),
            },
            update,
        ),
    }
    let reindex = close_out_stage(CloseOutStage::Graph, stderr, update, || {
        localpilot_localmind::codegraph_reindex(cwd, CODEGRAPH_BATCH_LIMIT)
    });
    let graph_current = match reindex {
        Ok(summary) => {
            // Return every count even when the graph was already current.
            record(
                CloseOutEvent::Graph {
                    reindexed: summary.reindexed,
                    pruned: summary.pruned,
                    remaining: summary.remaining,
                },
                update,
            );
            summary.remaining == 0
        }
        Err(error) => {
            record(
                CloseOutEvent::Skipped {
                    stage: CloseOutStage::Graph,
                    reason: error.to_string(),
                },
                update,
            );
            false
        }
    };
    if graph_current {
        let primer = close_out_stage(CloseOutStage::Primer, stderr, update, || {
            localpilot_localmind::distill_primer_into_review(cwd)
        });
        match primer {
            Ok(Some(_)) => record(CloseOutEvent::PrimerEnqueued, update),
            Ok(None) => {}
            Err(error) => record(
                CloseOutEvent::Skipped {
                    stage: CloseOutStage::Primer,
                    reason: error.to_string(),
                },
                update,
            ),
        }
    }
    update(CloseOutUpdate::Finished);
    report
}

/// How many files one session-close reindex pass may touch.
const CODEGRAPH_BATCH_LIMIT: usize = 64;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use localpilot_core::{Message, Role, SessionId};
    use localpilot_store::Store;

    #[test]
    fn close_out_of_a_real_session_enqueues_review_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        let session = SessionId::new();
        store
            .append_message(
                session,
                &Message::text(Role::User, "Lesson: redact secrets before persisting."),
            )
            .unwrap();

        // The shared helper that every non-REPL session-end path calls (headless
        // harness steps, the RPC serve loop) must learn from a real session, not
        // just the interactive REPL.
        let mut updates = Vec::new();
        let report = close_out(dir.path(), session, |update| updates.push(update));
        assert_eq!(
            updates.first(),
            Some(&CloseOutUpdate::Stage(CloseOutStage::Lessons))
        );
        assert_eq!(updates.last(), Some(&CloseOutUpdate::Finished));
        let streamed: Vec<_> = updates
            .into_iter()
            .filter_map(|update| match update {
                CloseOutUpdate::Event(event) => Some(event),
                _ => None,
            })
            .collect();
        assert_eq!(report.events, streamed);
        assert!(
            matches!(report.events.first(), Some(CloseOutEvent::Session { enqueued, .. }) if *enqueued > 0)
        );
        assert!(report
            .events
            .iter()
            .any(|event| matches!(event, CloseOutEvent::Graph { .. })));

        let items = localpilot_localmind::review_list(dir.path()).unwrap();
        assert!(
            !items.is_empty(),
            "closeout of a real session must enqueue at least one review candidate"
        );
    }

    #[test]
    fn disabled_learning_returns_warning_reasons_without_creating_a_store() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".localmind.toml"),
            "[learning]\nenabled = false\n",
        )
        .unwrap();
        let session = SessionId::new();
        Store::open(dir.path())
            .append_message(session, &Message::text(Role::User, "a session"))
            .unwrap();
        let report = close_out(dir.path(), session, |_| {});
        assert!(!report.events.is_empty());
        assert!(report.events.iter().all(CloseOutEvent::is_warning));
        assert!(report.events.iter().all(
            |event| matches!(event, CloseOutEvent::Skipped { reason, .. } if !reason.is_empty())
        ));
        assert!(!dir.path().join(".localmind").exists());
    }

    #[test]
    fn close_out_of_an_empty_session_creates_no_localmind_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new();

        // Opening and closing a bare session must leave no learning state, so a
        // plain prompt never creates project files.
        let mut updates = Vec::new();
        let report = close_out(dir.path(), session, |update| updates.push(update));
        assert!(report.events.is_empty());
        assert!(updates.is_empty());

        assert!(!dir.path().join(".localmind").exists());
        assert!(!dir.path().join(".localmind.toml").exists());
    }
}
