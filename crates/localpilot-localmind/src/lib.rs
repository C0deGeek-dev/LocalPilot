//! LocalMind learning adapter for LocalPilot.
//!
//! This is the host edge between LocalPilot and the host-neutral LocalMind
//! learning engine. LocalPilot owns evidence capture, permissions, redaction,
//! and the UI; LocalMind owns the learning loop (session summaries, candidate
//! lessons, the review queue, accepted-memory promotion, audit, search, and
//! agent-ready context). This crate maps LocalPilot's session records into
//! LocalMind's contracts and drives the loop; LocalMind never depends back.
#![forbid(unsafe_code)]

mod codegraph;
mod error;
mod ops;

use std::fmt::Write as _;
use std::path::Path;

pub use codegraph::{
    codegraph_export, codegraph_inspect, codegraph_reindex, CodeGraphSummary, ExportFormat,
    SymbolReport,
};
pub use ops::{
    audit, context_for, memory_delete, memory_disable_injection, memory_injection_enabled,
    memory_list, promote, review_decide, review_list, review_show, search, skill_body, skill_show,
    skills_generate, skills_list, AuditEntry, MemorySummary, ReviewSummary, ReviewVerdict,
    SearchHit, SkillDraftInfo,
};

use localmind_core::{SessionId as LearningSessionId, SessionSource};
use localmind_store::{
    CloseoutProcessor, DeterministicExtractor, ProjectConfig, TranscriptImportFormat,
    TranscriptImporter,
};
use localpilot_core::{ContentBlock, Message, Role, SessionId};
use localpilot_store::Store;

pub use error::LearningError;

/// The project-local LocalMind config file name.
const CONFIG_FILE: &str = ".localmind.toml";

/// A minimal local-only learning config, written on first use.
const DEFAULT_CONFIG: &str = "[learning]\nenabled = true\nlocal_only = true\n";

/// Ensure the project has a LocalMind config, writing a local-only default when
/// absent. Returns whether a config was created.
///
/// # Errors
/// Returns [`LearningError::Config`] if the file cannot be written.
pub fn initialize(project_root: &Path) -> Result<bool, LearningError> {
    let path = project_root.join(CONFIG_FILE);
    if path.exists() {
        return Ok(false);
    }
    std::fs::write(&path, DEFAULT_CONFIG).map_err(|e| LearningError::Config(e.to_string()))?;
    Ok(true)
}

/// The result of closing out a session into LocalMind.
#[derive(Debug, Clone)]
pub struct CloseoutSummary {
    /// The LocalMind session id assigned to the imported transcript.
    pub session_id: String,
    /// Number of candidate lessons extracted.
    pub candidate_count: usize,
    /// Number of candidates enqueued for review.
    pub enqueued_count: usize,
}

/// Close out an LocalPilot session: read its redacted transcript, import it into
/// the project's LocalMind store, and run summary + candidate-lesson extraction,
/// enqueuing candidates for review.
///
/// # Errors
/// Returns [`LearningError`] if the transcript cannot be read or any LocalMind
/// import/close-out step fails.
pub fn closeout_session(
    project_root: &Path,
    store: &Store,
    session: SessionId,
) -> Result<CloseoutSummary, LearningError> {
    let messages = store
        .read_transcript(session)
        .map_err(|e| LearningError::Transcript(e.to_string()))?;
    let transcript = render_transcript(&messages);

    initialize(project_root)?;
    let config =
        ProjectConfig::discover(project_root).map_err(|e| LearningError::Config(e.to_string()))?;
    let import = TranscriptImporter::import_text(
        &config,
        &transcript,
        SessionSource::LocalPilot,
        TranscriptImportFormat::PlainText,
    )
    .map_err(|e| LearningError::Import(e.to_string()))?;

    let report = CloseoutProcessor::closeout_project_session(
        project_root,
        &import.session_id,
        &DeterministicExtractor,
    )
    .map_err(|e| LearningError::Closeout(e.to_string()))?;

    Ok(CloseoutSummary {
        session_id: report.session_id.to_string(),
        candidate_count: report.candidate_count,
        enqueued_count: report.enqueued_count,
    })
}

/// Render a session's messages as a plain-text transcript for import. The text
/// is redacted again by LocalMind on import, layered on LocalPilot's own
/// redaction at persistence time.
fn render_transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        let speaker = role_label(message.role);
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => {
                    let _ = writeln!(out, "{speaker}: {text}");
                }
                ContentBlock::Reasoning { text, .. } => {
                    let _ = writeln!(out, "{speaker} (reasoning): {text}");
                }
                ContentBlock::ToolUse(call) => {
                    let _ = writeln!(out, "{speaker} calls {}: {}", call.name, call.input);
                }
                ContentBlock::ToolResult(result) => {
                    let label = if result.is_error {
                        "tool error"
                    } else {
                        "tool result"
                    };
                    let _ = writeln!(out, "{label}: {}", result.output);
                }
                _ => {}
            }
        }
    }
    out
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::UserShell => "user shell",
    }
}

/// Re-exported so callers can name the learning session id without depending on
/// LocalMind directly.
pub type LocalMindSessionId = LearningSessionId;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn closeout_imports_and_extracts_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = Store::open(root);
        let session = SessionId::new();
        store
            .append_message(
                session,
                &Message::text(Role::User, "fix the failing parser test"),
            )
            .unwrap();
        store
            .append_message(
                session,
                &Message::text(
                    Role::Assistant,
                    "The off-by-one was in the tokenizer bounds check.",
                ),
            )
            .unwrap();

        let summary = closeout_session(root, &store, session).unwrap();

        // The config and session artifacts were created under the project.
        assert!(root.join(CONFIG_FILE).exists());
        assert!(!summary.session_id.is_empty());
        // A deterministic extraction never panics and reports a candidate count.
        assert!(summary.enqueued_count <= summary.candidate_count);
    }

    /// Boundary fixture: a realistic LocalPilot session bundle (user ask, tool
    /// failure, fix, explicit lesson, repeated commands) must map across the
    /// adapter into reviewable LocalMind candidates — this pins the
    /// host-to-engine contract end to end, not just "nothing crashed".
    #[test]
    fn realistic_session_bundle_maps_to_reviewable_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = Store::open(root);
        let session = SessionId::new();
        for (role, text) in [
            (Role::User, "the exporter test is failing again"),
            (
                Role::Assistant,
                "error: assertion failed at writer.rs:88, the batch flush ordering is wrong",
            ),
            (
                Role::Assistant,
                "Fixed: flushing before the clear; the suite is passing now.",
            ),
            (
                Role::User,
                "Lesson: exporter changes need the integration suite, not just unit tests.",
            ),
        ] {
            store
                .append_message(session, &Message::text(role, text))
                .unwrap();
        }

        let summary = closeout_session(root, &store, session).unwrap();

        assert!(
            summary.enqueued_count >= 2,
            "expected the lesson marker and the failure/fix pair to enqueue, got {summary:?}"
        );
        let items = review_list(root).unwrap();
        assert_eq!(items.len(), summary.enqueued_count);
    }

    #[test]
    fn initialize_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(initialize(dir.path()).unwrap());
        assert!(!initialize(dir.path()).unwrap());
    }

    #[test]
    fn context_lookup_does_not_initialize_a_fresh_project() {
        let dir = tempfile::tempdir().unwrap();

        let context = context_for(dir.path(), "parser").unwrap();

        assert!(context.is_none());
        assert!(!dir.path().join(CONFIG_FILE).exists());
        assert!(!dir.path().join(".localmind").exists());
    }

    #[test]
    fn review_and_search_surfaces_open_after_closeout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = Store::open(root);
        let session = SessionId::new();
        store
            .append_message(
                session,
                &Message::text(Role::User, "the build failed with a borrow error"),
            )
            .unwrap();
        store
            .append_message(
                session,
                &Message::text(
                    Role::Assistant,
                    "Fixed: clone the value before the await so no lock is held across it.",
                ),
            )
            .unwrap();
        closeout_session(root, &store, session).unwrap();

        // The review queue, memory search, and audit log all open without error;
        // their contents depend on the deterministic extractor's heuristics.
        let items = review_list(root).unwrap();
        let _ = search(root, "lock").unwrap();
        let _ = audit(root).unwrap();

        // If a candidate was enqueued, the accept -> promote path round-trips.
        if let Some(first) = items.first() {
            review_decide(root, &first.id, ReviewVerdict::Accept, "tester", None).unwrap();
            let memory_id = promote(root, &first.id).unwrap();
            assert!(!memory_id.is_empty());
        }
    }
}
