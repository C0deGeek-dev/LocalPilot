//! Pre-turn context hook: contribute relevant LocalMind knowledge to a turn.
//!
//! Only accepted, review-gated project memory is contributed as always-on
//! context (lean, and injected into the request rather than stored, so it never
//! accumulates). Ingested folder knowledge is reached on demand through the
//! `knowledge_search` tool instead of being seeded every turn — unless the
//! project opts back into the legacy push behavior via `[ingest] mode = "push"`.
//!
//! This lives in the engine crate (not the host binary) so the pull/push gate is
//! unit-testable; the host just registers it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use localpilot_config::{CliOverrides, ConfigPaths, IngestConfig, IngestMode};
use localpilot_harness::{ContextContribution, ContextHook, SessionRuntime};
use localpilot_store::MemoryUsed;

/// Cap on the accepted-memory block, so the always-on context stays lean
/// regardless of how large the memory store grows.
const ACCEPTED_MEMORY_CHAR_CAP: usize = 1_200;

/// Cap on the always-on repo-primer block, so session-start orientation stays
/// a small, bounded token cost.
const PRIMER_CHAR_CAP: usize = 1_000;

/// Cap on the always-on rule-cue block, so promoted curated lessons stay terse.
const RULE_CUE_CHAR_CAP: usize = 1_000;

/// The audit id for the always-on repository primer block (it is one block, not
/// a searchable memory row).
const PRIMER_ID: &str = "<repository-primer>";

/// LocalMind retrieval as a pre-turn context hook. Best-effort — a miss or error
/// contributes nothing and never fails the turn.
pub struct LocalMindContext {
    root: PathBuf,
    /// The workspace's dominant language, detected once per session (a bounded
    /// scan) and cached, so language-relevance filtering costs nothing per turn.
    language: OnceLock<Option<&'static str>>,
}

impl LocalMindContext {
    /// A hook rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            language: OnceLock::new(),
        }
    }

    /// The workspace's dominant programming language, computed once and cached.
    /// Detection lives in `localmind-store` so the workspace signal and the stored
    /// lesson tag share one source of truth.
    fn workspace_language(&self) -> Option<&'static str> {
        *self
            .language
            .get_or_init(|| localmind_store::detect_workspace_language(&self.root))
    }

    fn ingest_config(&self) -> Option<IngestConfig> {
        localpilot_config::load(&ConfigPaths::standard(&self.root), &CliOverrides::default())
            .ok()
            .map(|config| config.ingest)
    }

    /// Resolve the accepted-memory injection policy for this project. The
    /// keyword-path defaults preserve the prior fixed behaviour (a 1200-char
    /// budget, no category dedup); the semantic relevance gate
    /// (`injection_min_cosine`) ships default-on but is best-effort (inert without
    /// an embedding endpoint). A `[memory]` section opts into further tuning. When
    /// context-aware budgeting is on, the budget is scaled toward the default
    /// provider's declared context window so a small model injects less.
    fn injection_policy(&self) -> InjectionPolicy {
        let Ok(config) =
            localpilot_config::load(&ConfigPaths::standard(&self.root), &CliOverrides::default())
        else {
            return InjectionPolicy::fixed_default();
        };
        let memory = &config.memory;
        let char_budget = if memory.injection_context_aware {
            let context_tokens = config
                .providers
                .get(&config.provider.default)
                .and_then(|provider| provider.context_window);
            context_aware_budget(context_tokens, memory.injection_char_budget)
        } else {
            memory.injection_char_budget
        };
        InjectionPolicy {
            min_score: memory.injection_min_score,
            char_budget,
            skip_categories: memory.injection_skip_categories.clone(),
            language_filter: memory.injection_language_filter,
            min_cosine: memory.injection_min_cosine,
        }
    }
}

/// The resolved accepted-memory injection policy for one turn.
struct InjectionPolicy {
    /// Minimum retrieval score a hit must clear to be injected.
    min_score: i64,
    /// Char budget for the injected accepted-memory block.
    char_budget: usize,
    /// Lesson categories skipped because a rule already enforces equivalent guidance.
    skip_categories: Vec<String>,
    /// Skip a lesson clearly about a different language than the workspace's.
    language_filter: bool,
    /// Minimum normalized cosine a hit with an embedding must clear to be injected
    /// (the semantic relevance gate). `0.0` disables. A hit without a cosine
    /// (no embedding endpoint / unembedded lesson) always passes — best-effort.
    min_cosine: f32,
}

impl InjectionPolicy {
    /// The prior fixed behaviour, used when config cannot be read. The cosine gate
    /// is off here so a config read failure never tightens injection.
    fn fixed_default() -> Self {
        Self {
            min_score: 0,
            char_budget: ACCEPTED_MEMORY_CHAR_CAP,
            skip_categories: Vec::new(),
            language_filter: true,
            min_cosine: 0.0,
        }
    }

    /// Whether `hit` should be skipped: a weak bm25 match, an enforced category,
    /// or — when a cosine is present — semantically too far from the prompt. A hit
    /// with no cosine (no embedding endpoint, or an unembedded lesson) is never
    /// gated on relevance, so the no-embed keyword path is unchanged.
    fn skips(&self, score: i64, category: &str, cosine: Option<f32>) -> bool {
        score < self.min_score
            || self
                .skip_categories
                .iter()
                .any(|skip| skip.eq_ignore_ascii_case(category))
            || (self.min_cosine > 0.0 && cosine.is_some_and(|c| c < self.min_cosine))
    }
}

/// Scale the injected char budget toward the model's context window — a small
/// model gets a smaller budget — never exceeding `ceiling` and never below a
/// floor that keeps at least one useful memory line. With no declared context
/// window the ceiling (the fixed budget) is used unchanged.
fn context_aware_budget(context_tokens: Option<u64>, ceiling: usize) -> usize {
    /// Share of the context window allotted to accepted-memory injection.
    const INJECTION_CONTEXT_FRACTION: f64 = 0.04;
    /// Rough chars-per-token for converting a token budget to the char cap.
    const CHARS_PER_TOKEN: f64 = 4.0;
    /// Floor so a tiny context still injects at least one useful line.
    const MIN_BUDGET: usize = 400;
    let Some(tokens) = context_tokens else {
        return ceiling;
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // bounded by clamp
    let scaled = (tokens as f64 * INJECTION_CONTEXT_FRACTION * CHARS_PER_TOKEN) as usize;
    scaled.clamp(MIN_BUDGET.min(ceiling), ceiling)
}

impl ContextHook for LocalMindContext {
    fn name(&self) -> &str {
        "localmind-context"
    }

    fn context_for(&self, prompt: &str) -> Option<String> {
        self.contribute(prompt).text
    }

    fn memories_used(&self, prompt: &str) -> Vec<MemoryUsed> {
        self.contribute(prompt).memories
    }

    /// The injected context and the exact memories it represents, from a single
    /// retrieval, so the "memories used" record matches the injection block for
    /// block. Each injected block contributes its records under its own layer
    /// (`primer`, `memory`, `ingest`); a memory whose snippet does not fit the
    /// char budget is neither injected nor recorded.
    fn contribute(&self, prompt: &str) -> ContextContribution {
        let mut blocks: Vec<String> = Vec::new();
        let mut memories: Vec<MemoryUsed> = Vec::new();

        // The accepted cold-start primer: always-on orientation (not prompt
        // relevance), token-bounded. An unaccepted or stale primer is not active.
        if let Some(text) = crate::primer::accepted_primer(&self.root).ok().flatten() {
            blocks.push(format!(
                "Repository primer:\n{}",
                bound(&text, PRIMER_CHAR_CAP)
            ));
            memories.push(MemoryUsed {
                id: PRIMER_ID.to_string(),
                score: 0,
                layer: "primer".to_string(),
            });
        }

        // Always-on rule cues: curated lessons a human promoted to terse,
        // always-present rules (independent of prompt relevance). A weak model
        // acts on a short always-on rule better than on a retrieved paragraph.
        // Recorded under the `rule-cue` layer, and excluded from the relevance
        // block below so a cue is never injected twice.
        let cue_ids = crate::rule_cue::rule_cue_ids(&self.root);
        if !cue_ids.is_empty() {
            if let Ok(records) = crate::ops::memory_list(&self.root) {
                let mut block = String::from("Project rule cues (always apply):\n");
                let mut wrote = false;
                for record in records {
                    if !cue_ids.contains(&record.id) {
                        continue;
                    }
                    let line = format!("- {}\n", record.body.trim());
                    if block.chars().count() + line.chars().count() > RULE_CUE_CHAR_CAP {
                        break;
                    }
                    block.push_str(&line);
                    wrote = true;
                    memories.push(MemoryUsed {
                        id: record.id,
                        score: 0,
                        layer: "rule-cue".to_string(),
                    });
                }
                if wrote {
                    blocks.push(block.trim_end().to_string());
                }
            }
        }

        // Accepted memory: one ranked, capped retrieval feeds both the injected
        // block and the recorded set, line by line under the char budget. Still
        // best-effort for the turn, but a broken store (corrupt/misconfigured) no
        // longer vanishes silently: it is logged so the failure is diagnosable
        // instead of looking identical to "no memory".
        let policy = self.injection_policy();
        // The workspace language (cached), used only when the filter is on. It is
        // pushed into the query so retrieval excludes off-language lessons inside
        // the FTS search — a Python idiom injected into a Rust task degrades the
        // solution — and returns rows that are already language-relevant rather
        // than spending the budget on rows that would be dropped here. A lesson
        // that names no single language stays eligible for every task.
        let task_language = policy
            .language_filter
            .then(|| self.workspace_language())
            .flatten();
        match crate::ops::context_hits(&self.root, prompt, task_language) {
            Ok(hits) => {
                let mut block = String::from("Relevant accepted project memory:\n");
                let mut wrote = false;
                for hit in hits {
                    // Relevance gate + dedup-vs-enforced: a weak bm25 match, a
                    // semantically off-topic lesson (cosine below the threshold), or
                    // a category a rule already enforces must not consume the budget.
                    if policy.skips(hit.score, &hit.category, hit.cosine) {
                        continue;
                    }
                    // A lesson already injected as an always-on rule cue is not
                    // injected again here.
                    if cue_ids.contains(&hit.memory_id) {
                        continue;
                    }
                    let line = format!("- {}\n", hit.snippet.trim());
                    if block.chars().count() + line.chars().count() > policy.char_budget {
                        break;
                    }
                    block.push_str(&line);
                    wrote = true;
                    memories.push(MemoryUsed {
                        id: hit.memory_id,
                        score: hit.score,
                        layer: "memory".to_string(),
                    });
                }
                if wrote {
                    blocks.push(block.trim_end().to_string());
                }
            }
            Err(error) => {
                tracing::warn!(
                    target: "localpilot::localmind",
                    %error,
                    "accepted-memory retrieval failed; this turn ran without memory context (the store may be corrupt or misconfigured)"
                );
            }
        }

        // Ingested knowledge only in legacy push mode; record the exact chunks.
        if let Some(config) = self.ingest_config() {
            if config.enabled && config.mode == IngestMode::Push {
                if let Ok(Some((text, ids))) =
                    crate::ingest::context_for_prompt_with_ids(&self.root, prompt)
                {
                    blocks.push(text.trim_end().to_string());
                    for id in ids {
                        memories.push(MemoryUsed {
                            id,
                            score: 0,
                            layer: "ingest".to_string(),
                        });
                    }
                }
            }
        }

        ContextContribution {
            text: (!blocks.is_empty()).then(|| blocks.join("\n")),
            memories,
        }
    }

    /// Record this turn's injected memories against the store, bumping each one's
    /// usage count. Best-effort and post-turn (the harness calls this once at the
    /// turn-exit, never on the retrieval read path), so a failed bump never fails
    /// the turn. Synthetic primer/ingest ids match no memory row and fall through.
    fn record_usage(&self, memories: &[MemoryUsed]) {
        crate::ops::record_memory_usage(&self.root, memories);
    }
}

/// Truncate `text` to at most `cap` characters, adding a marker when it was cut.
fn bound(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let truncated: String = text.chars().take(cap).collect();
    format!("{truncated}\n… (memory truncated)")
}

/// Register the LocalMind context hook on a session runtime.
pub fn register_context_hook(cwd: &Path, runtime: &mut SessionRuntime) {
    runtime
        .hooks_mut()
        .register_context_hook(Arc::new(LocalMindContext::new(cwd)));
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::ingest::{run as ingest_run, RunMode};
    use localpilot_config::IngestConfig;

    fn seed_ingest(root: &Path) {
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn distinctive_marker_symbol() -> u32 { 7 }\n",
        )
        .unwrap();
        ingest_run(root, &IngestConfig::default(), RunMode::Full).unwrap();
    }

    #[test]
    fn pull_mode_does_not_inject_ingested_knowledge() {
        let dir = tempfile::tempdir().unwrap();
        seed_ingest(dir.path());
        // No .localpilot.toml → default mode is pull, and there is no accepted
        // memory store, so the hook contributes nothing even though the ingest
        // index would match the prompt.
        let hook = LocalMindContext::new(dir.path());
        assert_eq!(hook.context_for("distinctive_marker_symbol"), None);
    }

    #[test]
    fn push_mode_injects_ingested_knowledge() {
        let dir = tempfile::tempdir().unwrap();
        seed_ingest(dir.path());
        std::fs::write(
            dir.path().join(".localpilot.toml"),
            "[ingest]\nenabled = true\nmode = \"push\"\n",
        )
        .unwrap();
        let hook = LocalMindContext::new(dir.path());
        let context = hook
            .context_for("distinctive_marker_symbol")
            .expect("push mode must inject the matching ingest chunk");
        assert!(
            context.contains("src/lib.rs"),
            "expected the ingested file in the pushed context, got: {context}"
        );
    }

    #[test]
    fn an_accepted_primer_is_injected_into_session_context() {
        use localmind_core::{ReviewAction, ReviewDecision, ReviewItemId};
        use localmind_store::{MemoryPersistence, ReviewQueue};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn hub() -> u8 { 1 }\nfn caller() { hub(); }\n",
        )
        .unwrap();
        crate::codegraph_reindex(root, usize::MAX).unwrap();
        let id = crate::distill_primer_into_review(root).unwrap().unwrap();

        // Before acceptance the hook injects nothing.
        let hook = LocalMindContext::new(root);
        assert_eq!(hook.context_for("anything"), None);

        // Accept + promote the primer, then the hook includes it always-on.
        let queue = ReviewQueue::open_project(root).unwrap();
        let item = ReviewItemId::new(&id);
        queue
            .decide(ReviewDecision {
                item_id: item.clone(),
                action: ReviewAction::Accept,
                reviewer: "tester".to_string(),
                decided_at: None,
                note: None,
                replacement_summary: None,
                evidence: Vec::new(),
            })
            .unwrap();
        MemoryPersistence::open_project(root)
            .unwrap()
            .promote_review_item(&item)
            .unwrap();

        let context = hook
            .context_for("a prompt unrelated to the primer text")
            .expect("the accepted primer is always-on context");
        assert!(context.contains("Repository primer:"));

        // The injected primer block is recorded in the audit under its own layer,
        // so the inspector reflects what actually rode in the turn's context.
        let used = hook.memories_used("a prompt unrelated to the primer text");
        assert!(
            used.iter().any(|m| m.layer == "primer"),
            "the injected primer must be recorded with the primer layer: {used:?}"
        );
    }

    #[test]
    fn memories_used_reports_a_relevant_accepted_memory() {
        use localmind_core::{
            Confidence, EvidenceKind, EvidenceRef, LessonCategory, MemoryEntry, MemoryEntryId,
            MemoryScope, MemoryStatus,
        };
        use localmind_store::MemoryPersistence;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        let entry = MemoryEntry {
            id: MemoryEntryId::new("mem-redact"),
            scope: MemoryScope::Project,
            body: "always redact secrets before persisting a transcript".to_string(),
            category: LessonCategory::SecurityWarning,
            confidence: Confidence::new(0.9).unwrap(),
            source_session: None,
            evidence: vec![EvidenceRef::new(EvidenceKind::ManualNote, "seeded")],
            tags: Vec::new(),
            related_files: Vec::new(),
            related_entities: Vec::new(),
            created_at: None,
            updated_at: None,
            supersedes: Vec::new(),
            contradicts: Vec::new(),
            status: MemoryStatus::Active,
        };
        MemoryPersistence::open_project(root)
            .unwrap()
            .persist_memory_entry(&entry)
            .unwrap();

        let hook = LocalMindContext::new(root);
        let used = hook.memories_used("how should I redact secrets");
        assert!(
            used.iter()
                .any(|m| m.id == "mem-redact" && m.layer == "memory"),
            "the relevant accepted memory must be reported as used: {used:?}"
        );

        // An unrelated prompt surfaces nothing.
        assert!(hook.memories_used("audio playback latency").is_empty());
    }

    #[test]
    fn a_broken_memory_store_is_handled_best_effort_at_the_hook_seam() {
        // context_hits now propagates a corrupt-store error (proven in ops). At
        // this seam the hook must stay best-effort — log and contribute nothing,
        // never panic or fail the turn.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        let state = root.join(".localmind");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(
            state.join("localmind.sqlite"),
            b"this is not a sqlite database",
        )
        .unwrap();

        let hook = LocalMindContext::new(root);
        assert_eq!(hook.context_for("anything"), None);
        assert!(hook
            .memories_used("anything")
            .iter()
            .all(|m| m.layer != "memory"));
    }

    #[test]
    fn a_promoted_rule_cue_is_injected_always_on() {
        // A cue is injected regardless of prompt relevance and recorded under the
        // rule-cue layer.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        seed_memory(
            root,
            "cue-1",
            "always run lark verify before declaring green",
        );
        crate::rule_cue::register_rule_cues(root, &["cue-1".to_string()]).unwrap();

        let hook = LocalMindContext::new(root);
        let context = hook
            .context_for("an unrelated prompt about audio latency")
            .expect("a promoted cue is always-on context");
        assert!(context.contains("Project rule cues"));
        let used = hook.memories_used("an unrelated prompt about audio latency");
        assert!(
            used.iter()
                .any(|m| m.id == "cue-1" && m.layer == "rule-cue"),
            "the cue must be recorded under the rule-cue layer: {used:?}"
        );
    }

    #[test]
    fn a_cue_is_not_also_injected_as_a_relevance_hit() {
        // When the prompt matches a cue-promoted memory, it is injected once (as a
        // cue), never twice (cue + relevance).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        seed_memory(
            root,
            "cue-dup",
            "always redact secrets before persisting a transcript",
        );
        crate::rule_cue::register_rule_cues(root, &["cue-dup".to_string()]).unwrap();

        let hook = LocalMindContext::new(root);
        let used = hook.memories_used("how should I redact secrets");
        let cue = used.iter().filter(|m| m.id == "cue-dup").count();
        assert_eq!(cue, 1, "a cue must be injected exactly once: {used:?}");
        assert!(
            used.iter()
                .any(|m| m.id == "cue-dup" && m.layer == "rule-cue"),
            "the single injection is the rule cue, not a relevance hit"
        );
    }

    #[test]
    fn context_aware_budget_shrinks_for_a_small_model() {
        // A small context window yields a smaller budget than the ceiling; a large
        // one is capped at the ceiling; an unknown window leaves the ceiling.
        let small = context_aware_budget(Some(4_096), 1_200);
        let large = context_aware_budget(Some(256_000), 1_200);
        assert!(small < 1_200, "a small model must inject less: {small}");
        assert!(small >= 400, "but never below the one-line floor: {small}");
        assert_eq!(large, 1_200, "a large model is capped at the fixed ceiling");
        assert_eq!(
            context_aware_budget(None, 1_200),
            1_200,
            "no declared context window leaves the ceiling unchanged"
        );
    }

    #[test]
    fn the_default_policy_still_injects_a_relevant_memory() {
        // Guard the floor: with no [memory] section, a relevant accepted memory is
        // injected exactly as before.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        seed_memory(root, "mem-keep", "always redact secrets before persisting");
        let hook = LocalMindContext::new(root);
        let used = hook.memories_used("how should I redact secrets");
        assert!(used.iter().any(|m| m.layer == "memory"));
    }

    #[test]
    fn the_cosine_gate_skips_an_off_topic_hit_but_passes_an_on_topic_one() {
        // A fixture-cosine test at the gate seam (no network embedder needed): an
        // off-topic same-language lesson (cosine below the threshold) is skipped,
        // an on-topic one passes, and a hit with no cosine always passes (the
        // best-effort no-embed path).
        let gate = InjectionPolicy {
            min_score: 0,
            char_budget: 1_200,
            skip_categories: Vec::new(),
            language_filter: true,
            min_cosine: 0.6,
        };
        assert!(
            !gate.skips(10, "CodePattern", Some(0.81)),
            "an on-topic lesson (cosine ≥ threshold) is injected"
        );
        assert!(
            gate.skips(10, "CodePattern", Some(0.40)),
            "an off-topic same-language lesson (cosine < threshold) is gated out"
        );
        assert!(
            !gate.skips(10, "CodePattern", None),
            "a hit with no cosine (no embedding) always passes — best-effort"
        );
        assert!(
            !gate.skips(10, "CodePattern", Some(0.60)),
            "exactly at the threshold passes (only strictly-below is gated)"
        );

        // A disabled gate (0.0) never skips on relevance, even a near-zero cosine.
        let disabled = InjectionPolicy {
            min_cosine: 0.0,
            min_score: 0,
            char_budget: 1_200,
            skip_categories: Vec::new(),
            language_filter: true,
        };
        assert!(!disabled.skips(10, "CodePattern", Some(0.01)));
    }

    #[test]
    fn the_default_cosine_gate_is_inert_without_an_embedding_endpoint() {
        // The cosine gate ships default-on (0.6), but it is best-effort: with no
        // embedding endpoint configured, every hit carries no cosine, so a relevant
        // memory injects exactly as on the keyword-only path — byte-identical.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        seed_memory(
            root,
            "mem-noembed",
            "always redact secrets before persisting",
        );
        let hook = LocalMindContext::new(root);
        let used = hook.memories_used("how should I redact secrets");
        assert!(
            used.iter().any(|m| m.id == "mem-noembed" && m.layer == "memory"),
            "with no embed endpoint the default cosine gate must not drop a relevant memory: {used:?}"
        );
    }

    #[test]
    fn a_low_relevance_memory_is_gated_out() {
        // With a min-score gate above any achievable bm25 score, the matching
        // memory is not injected — a weak match no longer fills the budget.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        std::fs::write(
            root.join(".localpilot.toml"),
            "[memory]\ninjection_min_score = 1000000\n",
        )
        .unwrap();
        seed_memory(root, "mem-weak", "always redact secrets before persisting");
        let hook = LocalMindContext::new(root);
        let used = hook.memories_used("how should I redact secrets");
        assert!(
            !used.iter().any(|m| m.layer == "memory"),
            "a sub-threshold match must be gated out: {used:?}"
        );
    }

    #[test]
    fn an_enforced_category_memory_is_not_injected() {
        // A category the rule engine already enforces is skipped at injection, so
        // injection adds signal rather than restating an enforced rule.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        std::fs::write(
            root.join(".localpilot.toml"),
            "[memory]\ninjection_skip_categories = [\"SecurityWarning\"]\n",
        )
        .unwrap();
        // seed_memory persists with category SecurityWarning.
        seed_memory(root, "mem-sec", "always redact secrets before persisting");
        let hook = LocalMindContext::new(root);
        let used = hook.memories_used("how should I redact secrets");
        assert!(
            !used.iter().any(|m| m.layer == "memory"),
            "an enforced-category memory must be skipped: {used:?}"
        );
    }

    #[test]
    fn bound_truncates_with_a_marker() {
        let long = "x".repeat(2_000);
        let bounded = bound(&long, 1_200);
        // Capped near the limit: 1200 kept chars plus a short truncation marker,
        // well under the un-truncated 2000.
        assert!(bounded.chars().count() < 1_300);
        assert!(bounded.starts_with(&"x".repeat(1_200)));
        assert!(bounded.contains("memory truncated"));
        // Short input is returned unchanged.
        assert_eq!(bound("short", 1_200), "short");
    }

    fn seed_memory(root: &Path, id: &str, body: &str) {
        use localmind_core::{
            Confidence, EvidenceKind, EvidenceRef, LessonCategory, MemoryEntry, MemoryEntryId,
            MemoryScope, MemoryStatus,
        };
        use localmind_store::MemoryPersistence;
        let entry = MemoryEntry {
            id: MemoryEntryId::new(id),
            scope: MemoryScope::Project,
            body: body.to_string(),
            category: LessonCategory::SecurityWarning,
            confidence: Confidence::new(0.9).unwrap(),
            source_session: None,
            evidence: vec![EvidenceRef::new(EvidenceKind::ManualNote, "seeded")],
            tags: Vec::new(),
            related_files: Vec::new(),
            related_entities: Vec::new(),
            created_at: None,
            updated_at: None,
            supersedes: Vec::new(),
            contradicts: Vec::new(),
            status: MemoryStatus::Active,
        };
        MemoryPersistence::open_project(root)
            .unwrap()
            .persist_memory_entry(&entry)
            .unwrap();
    }

    #[test]
    fn record_usage_bumps_the_injected_memory_hit_count() {
        use localmind_store::MemoryPersistence;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        seed_memory(root, "mem-hit", "always redact secrets before persisting");

        let hook = LocalMindContext::new(root);
        // A turn injects the memory; the harness then delivers the used set
        // post-turn via record_usage (the seam under test).
        let used = hook.memories_used("how should I redact secrets");
        assert!(
            used.iter().any(|m| m.id == "mem-hit"),
            "the memory must be injected first: {used:?}"
        );
        hook.record_usage(&used);

        let record = MemoryPersistence::open_project(root)
            .unwrap()
            .list_memory()
            .unwrap()
            .into_iter()
            .find(|r| r.memory_id.as_str() == "mem-hit")
            .expect("the memory is present");
        assert_eq!(
            record.hit_count, 1,
            "the injected memory's usage was bumped"
        );
        assert!(record.last_used_at.is_some(), "and last_used_at is stamped");

        // Best-effort: an empty set is a harmless no-op (never panics/fails).
        hook.record_usage(&[]);
    }

    #[test]
    fn memories_used_is_capped_to_the_injected_set() {
        // More matches than are injected: the audit records at most the injected
        // cap, never the full result set (the over-report this fix closes).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        for i in 0..8 {
            seed_memory(
                root,
                &format!("mem-{i}"),
                &format!("widget pipeline note number {i}"),
            );
        }
        let hook = LocalMindContext::new(root);
        let used = hook.memories_used("widget pipeline note");
        let memory_layer = used.iter().filter(|m| m.layer == "memory").count();
        assert!(memory_layer > 0, "expected matches to be recorded");
        assert!(
            memory_layer <= crate::ops::CONTEXT_MEMORY_LIMIT,
            "audit must not exceed the injected cap: {memory_layer}"
        );
    }

    #[test]
    fn the_audit_records_exactly_the_injected_memory_lines() {
        // The audit and the injection come from one retrieval under one budget,
        // so the recorded memory-layer entries equal the injected `- ` lines
        // exactly — never a memory that was not injected, nor one omitted.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        for i in 0..4 {
            seed_memory(
                root,
                &format!("note-{i}"),
                &format!("widget pipeline note number {i}"),
            );
        }
        let hook = LocalMindContext::new(root);
        let contribution = hook.contribute("widget pipeline note");
        let recorded = contribution
            .memories
            .iter()
            .filter(|m| m.layer == "memory")
            .count();
        let text = contribution.text.unwrap_or_default();
        let injected_lines = text.lines().filter(|line| line.starts_with("- ")).count();
        assert!(recorded >= 1, "expected matches");
        assert_eq!(
            recorded, injected_lines,
            "every recorded memory is an injected line and vice versa"
        );
    }
}
