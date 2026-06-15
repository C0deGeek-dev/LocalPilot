//! LocalMind learning adapter for LocalPilot.
//!
//! This is the host edge between LocalPilot and the host-neutral LocalMind
//! learning engine. LocalPilot owns evidence capture, permissions, redaction,
//! and the UI; LocalMind owns the learning loop (session summaries, candidate
//! lessons, the review queue, accepted-memory promotion, audit, search, and
//! agent-ready context). This crate maps LocalPilot's session records into
//! LocalMind's contracts and drives the loop; LocalMind never depends back.
#![forbid(unsafe_code)]

mod active_skills_tool;
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

pub use active_skills_tool::ActiveSkills;
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
    memory_list, promote, review_decide, review_list, review_show, search, skill_activate,
    skill_body, skill_show, skills_active, skills_generate, skills_list, ActiveSkillInfo,
    AuditEntry, MemorySummary, ReviewSummary, ReviewVerdict, SearchHit, SkillDraftInfo,
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

/// The local-only learning header, always written.
const LEARNING_CONFIG: &str = "[learning]\nenabled = true\nlocal_only = true\n";

/// A local inference endpoint derived from the host's own provider config.
struct LocalInferenceEndpoint {
    base_url: String,
    model: String,
}

/// Ensure the project has a LocalMind config, writing a local-only default when
/// absent. When the project's default LocalPilot provider points at a loopback
/// endpoint, the written config also enables model-backed learning against that
/// same local endpoint, so "local models do the learning jobs" needs no manual
/// plumbing. The model path degrades to the deterministic extractor when the
/// endpoint is unreachable, and a remote provider is never wired automatically
/// (that stays an explicit opt-in per the ecosystem remote-egress policy).
/// Returns whether a config was created.
///
/// # Errors
/// Returns [`LearningError::Config`] if the file cannot be written.
pub fn initialize(project_root: &Path) -> Result<bool, LearningError> {
    let path = project_root.join(CONFIG_FILE);
    if path.exists() {
        return Ok(false);
    }
    let endpoint = detect_local_inference_endpoint(project_root);
    std::fs::write(&path, render_default_config(endpoint.as_ref()))
        .map_err(|e| LearningError::Config(e.to_string()))?;
    Ok(true)
}

/// Build the default `.localmind.toml` body: always the local-only learning
/// header, plus an `[inference]` block pointing at `endpoint` when one was
/// detected. LocalMind stays host-neutral — it only reads a generic local
/// endpoint; the host decides whether to populate it.
fn render_default_config(endpoint: Option<&LocalInferenceEndpoint>) -> String {
    let mut config = String::from(LEARNING_CONFIG);
    if let Some(endpoint) = endpoint {
        let _ = write!(
            config,
            "\n[inference]\nchat_base_url = \"{base}\"\nchat_model = \"{model}\"\nembedding_base_url = \"{base}\"\n",
            base = endpoint.base_url,
            model = endpoint.model,
        );
    }
    config
}

/// Detect a local inference endpoint from the project's LocalPilot provider
/// config: the default provider's `base_url`, when it is a loopback address.
/// The `/v1` suffix LocalPilot carries is stripped because LocalMind appends the
/// OpenAI path itself. Returns `None` when the default provider is remote,
/// unconfigured, or unreadable — in which case learning stays deterministic.
fn detect_local_inference_endpoint(project_root: &Path) -> Option<LocalInferenceEndpoint> {
    // Project-scoped only: never let the machine's user config decide whether a
    // project wires model-backed learning (that would make behaviour depend on
    // the host machine, not the project).
    let paths = localpilot_config::ConfigPaths {
        user: None,
        project: Some(localpilot_config::project_config_path(project_root)),
    };
    let config =
        localpilot_config::load(&paths, &localpilot_config::CliOverrides::default()).ok()?;
    let provider = config.providers.get(&config.provider.default)?;
    let base_url = provider.base_url.as_deref()?;
    if !is_loopback_endpoint(base_url) {
        return None;
    }
    let root = base_url
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .trim_end_matches('/')
        .to_string();
    if root.is_empty() {
        return None;
    }
    let model = provider
        .model
        .clone()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "local".to_string());
    Some(LocalInferenceEndpoint {
        base_url: root,
        model,
    })
}

/// Whether `url`'s host is a loopback address (the only endpoint auto-wired for
/// learning; LAN/remote endpoints require explicit configuration).
fn is_loopback_endpoint(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("//127.0.0.1") || lower.contains("//localhost") || lower.contains("//[::1]")
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
                &Message::text(
                    Role::User,
                    "Lesson: prefer guard clauses over deeply nested ifs",
                ),
            )
            .unwrap();

        let summary = closeout_session(root, &store, session).unwrap();
        assert!(
            summary.enqueued_count >= 1,
            "deterministic fallback should still enqueue the explicit lesson, got {summary:?}"
        );
        let items = review_list(root).unwrap();
        assert!(
            items
                .iter()
                .any(|item| item.summary.contains("guard clauses")),
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
            .append_message(
                session,
                &Message::text(Role::User, "the parser test was failing"),
            )
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

    #[test]
    fn default_config_is_learning_only_without_a_local_provider() {
        let config = render_default_config(None);
        assert!(config.contains("[learning]"));
        assert!(!config.contains("[inference]"));
    }

    #[test]
    fn default_config_wires_inference_to_a_local_endpoint() {
        let endpoint = LocalInferenceEndpoint {
            base_url: "http://127.0.0.1:11435".to_string(),
            model: "qcoder".to_string(),
        };
        let config = render_default_config(Some(&endpoint));
        assert!(config.contains("[inference]"));
        assert!(config.contains("chat_base_url = \"http://127.0.0.1:11435\""));
        assert!(config.contains("chat_model = \"qcoder\""));
        // The `/v1` path is LocalMind's to append; it must not be in the base.
        assert!(!config.contains("11435/v1"));
    }

    #[test]
    fn detects_a_loopback_provider_and_strips_the_v1_suffix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".localpilot.toml"),
            "[provider]\ndefault = \"local\"\n\n[providers.local]\nkind = \"anthropic\"\nbase_url = \"http://127.0.0.1:11435/v1\"\nmodel = \"qcoder\"\n",
        )
        .unwrap();
        let endpoint = detect_local_inference_endpoint(dir.path()).expect("a local endpoint");
        assert_eq!(endpoint.base_url, "http://127.0.0.1:11435");
        assert_eq!(endpoint.model, "qcoder");
    }

    #[test]
    fn ignores_a_remote_provider() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".localpilot.toml"),
            "[provider]\ndefault = \"remote\"\n\n[providers.remote]\nkind = \"anthropic\"\nbase_url = \"https://api.example.com/v1\"\n",
        )
        .unwrap();
        assert!(detect_local_inference_endpoint(dir.path()).is_none());
    }

    #[test]
    fn initialize_wires_model_extraction_for_a_local_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".localpilot.toml"),
            "[provider]\ndefault = \"local\"\n\n[providers.local]\nkind = \"anthropic\"\nbase_url = \"http://127.0.0.1:11435/v1\"\nmodel = \"qcoder\"\n",
        )
        .unwrap();
        assert!(initialize(dir.path()).unwrap());
        let config = localmind_store::ProjectConfig::discover(dir.path()).unwrap();
        assert!(
            uses_model_extraction(&config),
            "a local-provider project should default to model-backed extraction"
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
                // Drain the full request before responding, so the client never
                // sees a reset mid-send (which would make this flaky under load).
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            request.extend_from_slice(&buffer[..read]);
                            if request_is_complete(&request) {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
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

    /// Whether `request` holds a complete HTTP request (headers plus a body of
    /// the declared Content-Length).
    fn request_is_complete(request: &[u8]) -> bool {
        let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
            return false;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let mut content_length = 0_usize;
        for line in headers.lines() {
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        request.len() >= header_end + 4 + content_length
    }
}
