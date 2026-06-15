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
mod context_hook;
mod error;
mod ingest;
mod knowledge_tool;
mod ops;
mod pack;
mod remember_tool;
mod skill_drafts_tool;

use std::fmt::Write as _;
use std::path::Path;

pub use codegraph::{
    codegraph_export, codegraph_inspect, codegraph_reindex, CodeGraphSummary, ExportFormat,
    SymbolReport,
};
pub use context_hook::{register_context_hook, LocalMindContext};
pub use ingest::{
    build_pack, cancel as ingest_cancel, compute_pack, context_for_prompt as ingest_context_for,
    exclude_path as ingest_exclude, forget as ingest_forget, include_path as ingest_include,
    normalize_project_path, pause as ingest_pause, preview as ingest_preview,
    promote_for_review as ingest_promote, rebuild as ingest_rebuild, resume as ingest_resume,
    review_items as ingest_review_items, run as ingest_run, search as knowledge_search,
    should_build_index, skipped as ingest_skipped, status as ingest_status, BudgetEstimate,
    CandidateStatus, ChunkRecord, ContextPack, IngestError, IngestJob, IngestReviewItem, JobStatus,
    KnowledgeHit, ManifestEntry, PreviewManifest, RunMode, RunSummary,
};
pub use knowledge_tool::KnowledgeSearch;
pub use ops::{
    audit, context_for, memory_delete, memory_disable_injection, memory_injection_enabled,
    memory_list, promote, review_decide, review_list, review_show, search, skill_body, skill_show,
    skills_active, skills_generate, skills_list, ActiveSkillInfo, AuditEntry, MemorySummary,
    ReviewSummary, ReviewVerdict, SearchHit, SkillDraftInfo,
};
pub use pack::{PackEntry, PackSource};
pub use remember_tool::Remember;
pub use skill_drafts_tool::SkillDrafts;

use localmind_core::{SessionId as LearningSessionId, SessionSource};
use localmind_store::{
    CloseoutProcessor, DeterministicExtractor, ProjectConfig, TranscriptImportFormat,
    TranscriptImporter,
};
use localpilot_core::{ContentBlock, Message, Role, SessionId};
use localpilot_store::{SessionEventKind, Store};

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
    let mut transcript = render_transcript(&messages);
    // Enrich the transcript with structured signals from the execution log
    // (failed tools, recovery events, committed steps) so extraction keys on the
    // fact LocalPilot already recorded, not just re-parsed prose. Best-effort:
    // the deterministic text path stays the baseline if the event log is absent.
    if let Ok(events) = store.read_events(session) {
        transcript.push_str(&render_session_signals(
            events.iter().map(|event| &event.kind),
        ));
    }

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

    // Select the extractor from the project's inference config. When an
    // `[inference]` endpoint is configured with extraction enabled, use the
    // model-backed extractor — which itself falls back to the deterministic
    // extractor when the endpoint is unreachable or returns malformed output —
    // otherwise run the deterministic path directly. The default experience may
    // depend on a local model for learning, but always degrades gracefully to
    // the deterministic baseline.
    let report = if uses_model_extraction(&config) {
        CloseoutProcessor::closeout_project_session_with_configured_inference(
            project_root,
            &import.session_id,
        )
    } else {
        CloseoutProcessor::closeout_project_session(
            project_root,
            &import.session_id,
            &DeterministicExtractor,
        )
    }
    .map_err(|e| LearningError::Closeout(e.to_string()))?;

    Ok(CloseoutSummary {
        session_id: report.session_id.to_string(),
        candidate_count: report.candidate_count,
        enqueued_count: report.enqueued_count,
    })
}

/// Whether session closeout should use the model-backed extractor for this
/// project. True when an `[inference]` endpoint is configured and its
/// `features.extraction` flag is on. The model-backed path falls back to the
/// deterministic extractor on its own when the endpoint is unreachable, so a
/// configured-but-down endpoint still produces (deterministic) candidates.
fn uses_model_extraction(config: &ProjectConfig) -> bool {
    config
        .config
        .inference
        .as_ref()
        .is_some_and(|inference| inference.features.extraction)
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

/// Render compact, redaction-safe structured signals from the session event log,
/// appended to the imported transcript so the extractor sees explicit
/// failure/recovery/outcome facts. Names, statuses, and short commit hashes only
/// — no raw payloads. Returns empty when there is nothing notable to report.
fn render_session_signals<'a>(kinds: impl Iterator<Item = &'a SessionEventKind>) -> String {
    use std::collections::BTreeMap;
    let mut failed_tools: BTreeMap<String, usize> = BTreeMap::new();
    let mut recoveries: Vec<String> = Vec::new();
    let mut commits: Vec<String> = Vec::new();
    for kind in kinds {
        match kind {
            SessionEventKind::ToolFinished {
                name,
                is_error: true,
                ..
            } => {
                *failed_tools.entry(name.clone()).or_default() += 1;
            }
            SessionEventKind::RecoveryDiagnostic { kind, health } => {
                recoveries.push(format!("{kind} (health: {health})"));
            }
            SessionEventKind::StepCompleted {
                number,
                commit: Some(hash),
                attempts,
            } => {
                commits.push(format!(
                    "step {number} committed {hash} after {attempts} attempt(s)"
                ));
            }
            _ => {}
        }
    }
    if failed_tools.is_empty() && recoveries.is_empty() && commits.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nSession signals (from the execution log):\n");
    for (tool, count) in &failed_tools {
        let _ = writeln!(out, "- tool {tool} failed {count} time(s)");
    }
    for recovery in &recoveries {
        let _ = writeln!(out, "- recovery: {recovery}");
    }
    for commit in &commits {
        let _ = writeln!(out, "- {commit}");
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
    fn session_signals_summarize_failures_recovery_and_commits() {
        let kinds = vec![
            SessionEventKind::ToolFinished {
                id: "1".into(),
                name: "run_shell".into(),
                is_error: true,
            },
            SessionEventKind::ToolFinished {
                id: "2".into(),
                name: "run_shell".into(),
                is_error: true,
            },
            SessionEventKind::ToolFinished {
                id: "3".into(),
                name: "read_file".into(),
                is_error: false,
            },
            SessionEventKind::RecoveryDiagnostic {
                kind: "degenerate_output".into(),
                health: "degraded".into(),
            },
            SessionEventKind::StepCompleted {
                number: 2,
                commit: Some("abc1234".into()),
                attempts: 1,
            },
        ];
        let out = render_session_signals(kinds.iter());
        assert!(
            out.contains("tool run_shell failed 2 time(s)"),
            "got: {out}"
        );
        // Successful tools are not noise.
        assert!(!out.contains("read_file"), "got: {out}");
        assert!(out.contains("recovery: degenerate_output"), "got: {out}");
        assert!(out.contains("step 2 committed abc1234"), "got: {out}");

        // Nothing notable → empty, so the deterministic text path is unchanged.
        assert!(render_session_signals(std::iter::empty()).is_empty());
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

    use localmind_store::ProjectConfig;

    fn project_config_with(toml: &str) -> (tempfile::TempDir, ProjectConfig) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), toml).unwrap();
        let config = ProjectConfig::discover(dir.path()).unwrap();
        (dir, config)
    }

    /// Path selection: the extractor follows the project's inference config.
    /// No `[inference]` → deterministic; configured + extraction on → model;
    /// configured but extraction off → deterministic.
    #[test]
    fn extractor_selection_follows_inference_config() {
        let (_d1, no_inference) = project_config_with("[learning]\nenabled = true\n");
        assert!(!uses_model_extraction(&no_inference));

        let (_d2, configured) = project_config_with(
            "[learning]\nenabled = true\n\n[inference]\nchat_base_url = \"http://127.0.0.1:1\"\nchat_model = \"m\"\n",
        );
        assert!(uses_model_extraction(&configured));

        let (_d3, feature_off) = project_config_with(
            "[learning]\nenabled = true\n\n[inference]\nchat_base_url = \"http://127.0.0.1:1\"\nchat_model = \"m\"\n\n[inference.features]\nextraction = false\n",
        );
        assert!(!uses_model_extraction(&feature_off));
    }

    /// Endpoint unavailable: a configured-but-unreachable endpoint must not break
    /// closeout — the model path falls back to the (hardened) deterministic
    /// extractor, which still surfaces the explicit lesson.
    #[test]
    fn closeout_falls_back_to_deterministic_when_endpoint_unavailable() {
        // A bound-then-dropped port is guaranteed closed → connection refused.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join(CONFIG_FILE),
            format!(
                "[learning]\nenabled = true\n\n[inference]\nchat_base_url = \"http://{dead_addr}\"\nchat_model = \"m\"\ntimeout_secs = 2\n"
            ),
        )
        .unwrap();

        let store = Store::open(root);
        let session = SessionId::new();
        store
            .append_message(
                session,
                &Message::text(Role::User, "Lesson: prefer guard clauses over deeply nested ifs"),
            )
            .unwrap();

        let summary = closeout_session(root, &store, session).unwrap();
        assert!(
            summary.enqueued_count >= 1,
            "deterministic fallback should still enqueue the explicit lesson, got {summary:?}"
        );
        let items = review_list(root).unwrap();
        assert!(
            items.iter().any(|item| item.summary.contains("guard clauses")),
            "fallback lesson missing: {items:?}"
        );
    }

    /// Model path used: a reachable endpoint's structured output drives the
    /// candidates, not the deterministic extractor.
    #[test]
    fn closeout_uses_model_output_when_endpoint_reachable() {
        let content = "{\"summary_title\":\"T\",\"summary_body\":\"B\",\"candidates\":[\
            {\"summary\":\"Model-extracted lesson: pin the exporter schema in a test\",\
            \"category\":\"process\",\"confidence\":0.9,\"action\":\"promote_to_memory\"}]}";
        let chat_body = serde_json::json!({
            "choices": [{ "message": { "content": content } }]
        })
        .to_string();
        let base_url = mock_chat_server(chat_body);

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join(CONFIG_FILE),
            format!(
                "[learning]\nenabled = true\n\n[inference]\nchat_base_url = \"{base_url}\"\nchat_model = \"m\"\ntimeout_secs = 5\n"
            ),
        )
        .unwrap();

        let store = Store::open(root);
        let session = SessionId::new();
        store
            .append_message(session, &Message::text(Role::User, "the parser test was failing"))
            .unwrap();

        closeout_session(root, &store, session).unwrap();
        let items = review_list(root).unwrap();
        assert!(
            items
                .iter()
                .any(|item| item.summary.contains("Model-extracted lesson")),
            "model output was not used: {items:?}"
        );
    }

    /// A one-shot OpenAI-compatible chat endpoint that returns `body` to the
    /// first request. Returns its base URL.
    fn mock_chat_server(body: String) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0_u8; 2048];
                // Drain the request headers/body enough to let the client finish.
                let _ = stream.read(&mut buffer);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{address}")
    }
}
