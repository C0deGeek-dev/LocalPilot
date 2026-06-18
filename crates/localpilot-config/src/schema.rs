//! Configuration schema.
//!
//! These types mirror `.localpilot.toml`. They are deliberately permissive about
//! unknown provider options (preserved under [`ProviderConfig::options`]) so a
//! provider can carry namespaced settings the core does not yet model.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// The full resolved configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub provider: ProviderSelection,
    pub providers: IndexMap<String, ProviderConfig>,
    pub harness: HarnessConfig,
    pub permissions: PermissionsConfig,
    pub quota: QuotaConfig,
    pub mcp: McpConfig,
    pub ingest: IngestConfig,
    pub compaction: CompactionConfig,
    pub storage: StorageConfig,
    pub skills: SkillsConfig,
    pub tools: ToolsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: ProviderSelection::default(),
            providers: IndexMap::new(),
            harness: HarnessConfig::default(),
            permissions: PermissionsConfig::default(),
            quota: QuotaConfig::default(),
            mcp: McpConfig::default(),
            ingest: IngestConfig::default(),
            compaction: CompactionConfig::default(),
            storage: StorageConfig::default(),
            skills: SkillsConfig::default(),
            tools: ToolsConfig::default(),
        }
    }
}

/// Pull-discovery broker configuration (ADR-0031). The broker narrows each turn's
/// advertised tool *schemas* to a small working set and resolves a need to the
/// right tool on demand, revealing its schema. Every field defaults so an absent
/// `[tools]` block reproduces today's behaviour exactly: `broker = false`
/// advertises the full registry (the rollback path), and the marker/learning
/// triggers are off. The numeric defaults mirror `localpilot-tools`' own
/// `BrokerConfig` defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// Enable the broker (narrow advertised schemas + resolve/reveal on miss).
    /// Default `false` — the full tool set is advertised, as before.
    pub broker: bool,
    /// The core working set always advertised when the broker is on, in addition
    /// to the broker's own `tool_search`/`tool_load`. Empty uses the built-in
    /// default (a lean read/edit/search/shell set).
    pub core: Vec<String>,
    /// Maximum revealed tools retained before LRU eviction.
    pub working_set_cap: usize,
    /// Minimum resolution score to reveal; below it, a miss is a clean "no match".
    pub score_floor: u32,
    /// Enable the loose `NEED: <capability>` marker trigger. Default `false` — the
    /// always-on failure-driven trigger does not need it.
    pub marker: bool,
    /// Enable broker learning: re-rank by past success, graduate hot tools into the
    /// always-advertised set, and record redacted resolution telemetry. Default
    /// `false` — the broker still works (mechanical freshness) without it.
    pub learning: bool,
    /// Reveals of one tool before it graduates into the always-advertised set.
    pub graduation_threshold: usize,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            broker: false,
            core: Vec::new(),
            working_set_cap: 24,
            score_floor: 1,
            marker: false,
            learning: false,
            graduation_threshold: 3,
        }
    }
}

/// Project-local skill surface configuration.
///
/// Skills are advisory prompt modules discovered on demand. The deterministic,
/// user-typed load (`localpilot skills show`) is always available; this flag only
/// governs **autonomous** model discovery — whether the agent may reach for
/// `skill_search`/`skill_load` on its own. It is **off by default** so a small
/// local model never auto-injects a skill unless the project opts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SkillsConfig {
    /// Register `skill_search`/`skill_load` so the model can discover and read
    /// project skills on its own. Default `false`.
    pub autonomous_discovery: bool,
}

/// Retention for the project-local `.localpilot/` state. A conservative cap is on
/// by default so session history and tool-output snapshots cannot grow without
/// bound; `0` on either limit disables that axis, and `auto_prune = false` turns
/// off the best-effort cleanup at chat startup (the `session prune` command still
/// works).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// Prune on a best-effort basis when the interactive chat starts.
    pub auto_prune: bool,
    /// Keep at most this many of the most-recently-updated sessions (`0` = no
    /// limit).
    pub max_sessions: u64,
    /// Drop sessions not updated within this many days (`0` = no limit).
    pub max_age_days: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            auto_prune: true,
            max_sessions: 100,
            max_age_days: 90,
        }
    }
}

/// Runtime context compaction configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    /// Default is deterministic. `smart_with_fallback` is accepted as an opt-in
    /// contract and falls back deterministically when no summarizer backend is
    /// configured.
    pub mode: CompactionMode,
    /// Maximum summary size target for smart/deterministic digest rendering.
    pub summary_token_limit: u64,
    /// Maximum input budget for model-backed summarization when enabled.
    pub summarizer_input_tokens: u64,
    /// Timeout for a future model-backed summarizer call.
    pub summarizer_timeout_secs: u64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            mode: CompactionMode::default(),
            summary_token_limit: 1_024,
            summarizer_input_tokens: 8_192,
            summarizer_timeout_secs: 20,
        }
    }
}

/// Requested runtime compaction mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMode {
    #[default]
    Deterministic,
    SmartWithFallback,
}

/// How ingested project knowledge reaches the model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestMode {
    /// Reachable on demand through the `knowledge_search` tool; never seeded into
    /// the turn context. The lean default.
    #[default]
    Pull,
    /// Legacy behavior: relevant ingest chunks are also auto-seeded into each
    /// turn's context. Kept as an escape hatch.
    Push,
}

/// Project-local folder ingestion configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IngestConfig {
    /// Whether ingest commands are allowed to persist derived project knowledge.
    pub enabled: bool,
    /// How ingested knowledge reaches the model (pull via tool, or legacy push
    /// into context).
    pub mode: IngestMode,
    /// Paths or glob-like fragments explicitly included for ingestion.
    pub include: Vec<String>,
    /// Paths or glob-like fragments explicitly excluded from ingestion.
    pub exclude: Vec<String>,
    /// Heavy/generated directory names skipped before file classification.
    pub default_skip_dirs: Vec<String>,
    /// Maximum bytes read from one file before it becomes metadata-only.
    pub max_file_bytes: u64,
    /// Maximum total candidate bytes processed in one run.
    pub max_run_bytes: u64,
    /// Maximum candidate files processed in one run.
    pub max_files: u64,
    /// Approximate token budget for persisted chunks.
    pub max_tokens: u64,
    /// Maximum elapsed time budget for a run.
    pub max_elapsed_secs: u64,
    /// Maximum model-backed calls for enrichment. The deterministic v1 path
    /// leaves this at zero unless the user opts in later.
    pub max_model_calls: u64,
    /// Opt in to model-written contextual chunk prefixes. Off by default: chunk
    /// prefixes are synthesized locally from front matter and leading lines. When
    /// on, a wired enricher may send file content off-machine to write richer
    /// prefixes, and each use is audited. Without an enricher this stays
    /// synthetic even when set.
    pub contextual_prefix_enrichment: bool,
    /// Minimum seconds between session-open auto-refreshes of a completed index.
    /// Once the index is built, a later session re-runs a refresh only when
    /// source files have changed and at least this long has passed since the last
    /// run — a debounce so quick successive sessions do not re-walk repeatedly.
    pub refresh_min_interval_secs: u64,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: IngestMode::default(),
            include: Vec::new(),
            exclude: Vec::new(),
            default_skip_dirs: [
                ".git",
                ".localmind",
                ".localpilot",
                "target",
                "node_modules",
                "bin",
                "obj",
                "dist",
                "build",
                ".venv",
                ".next",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            max_file_bytes: 1_048_576,
            max_run_bytes: 25_000_000,
            max_files: 5_000,
            max_tokens: 1_000_000,
            max_elapsed_secs: 600,
            max_model_calls: 0,
            contextual_prefix_enrichment: false,
            refresh_min_interval_secs: 600,
        }
    }
}

/// Model Context Protocol servers to connect to. Each server's tools are exposed
/// through the same permission engine and redaction as builtin tools.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    pub servers: IndexMap<String, McpServerConfig>,
}

/// One MCP server launched as a local subprocess speaking JSON-RPC over stdio.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// The command to launch the server.
    pub command: String,
    /// Arguments passed to the command.
    #[serde(default)]
    pub args: Vec<String>,
}

/// Which provider is active by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderSelection {
    pub default: String,
}

impl Default for ProviderSelection {
    fn default() -> Self {
        Self {
            default: "local".to_string(),
        }
    }
}

/// One provider entry. The credential itself is never stored here; only the name
/// of the environment variable that carries it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Default model for this provider, used when a command does not name one
    /// (for example launching the interactive REPL with no `--model`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// HTTP request timeout in seconds. Defaults are applied by provider
    /// adapters; this override is useful for slow local inference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_secs: Option<u64>,
    /// The model's context window in tokens. When set, the session budget is
    /// derived from it (window minus a response reserve) and takes precedence
    /// over the global `[harness] context_token_limit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Ask adapters to avoid optional thinking/reasoning output where the
    /// provider exposes a documented request shape for that behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppress_thinking: Option<bool>,
    /// Namespaced provider options the core does not model are preserved here.
    #[serde(flatten)]
    pub options: IndexMap<String, serde_json::Value>,
}

/// Operating mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Agent,
    Harness,
}

/// The no-unsupported-claim gate over a final reply (the verification gate that
/// flags a completed-action claim no verified tool call supports). Default `off`
/// while its false-positive rate is being measured; `warn` appends a visible,
/// non-destructive correction to an unsupported claim (it never drops content).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimGate {
    #[default]
    Off,
    Warn,
}

impl ClaimGate {
    /// Whether the gate reviews the final reply.
    #[must_use]
    pub fn is_enabled(self) -> bool {
        matches!(self, ClaimGate::Warn)
    }
}

/// Harness behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessConfig {
    pub mode: Mode,
    pub attempts_per_step: u32,
    pub auto_commit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_command: Option<String>,
    /// Discovered, ratified quality-gate checks (ADR-0009). Empty by default;
    /// when empty and `test_command` is set, [`HarnessConfig::resolved_checks`]
    /// synthesizes a single phase `test` check for back-compat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<CheckConfig>,
    pub rules: IndexMap<String, RuleSeverity>,
    /// Token budget the session keeps the conversation within (compaction trims
    /// older turns to stay under it). Set it to the model's usable context.
    pub context_token_limit: usize,
    /// Soft start for the per-turn tool-call ceiling. A turn that keeps making
    /// progress runs past this up to `tool_call_budget_max`; a turn detected as
    /// making no forward progress stops here. This is the count an ordinary task
    /// stays well under.
    pub tool_call_budget: usize,
    /// Hard cost-contract ceiling: the per-turn tool-call count that always stops
    /// the loop, regardless of progress, so a turn can never run unbounded. With
    /// `tool_call_budget_max == tool_call_budget` the ceiling is the flat fixed
    /// budget; raise it above the soft start to let a productive turn extend.
    pub tool_call_budget_max: usize,
    /// The no-unsupported-claim gate over the final reply. `off` (default) skips
    /// it; `warn` flags a completed-action claim no verified tool call supports.
    pub claim_gate: ClaimGate,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            mode: Mode::default(),
            attempts_per_step: 3,
            auto_commit: true,
            test_command: None,
            checks: Vec::new(),
            rules: IndexMap::new(),
            context_token_limit: 24_000,
            tool_call_budget: 50,
            tool_call_budget_max: 50,
            claim_gate: ClaimGate::default(),
        }
    }
}

/// Severity of a harness rule verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSeverity {
    Off,
    Warn,
    Block,
}

/// One quality-gate check (ADR-0009). Stored as a program plus an argument list
/// (no shell interpretation), matching how the runtime executes commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckConfig {
    /// Stable, unique check name (also the `[harness.rules]`-style override key).
    pub name: String,
    /// The program to run.
    pub program: String,
    /// Arguments passed as a list, not a shell string.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Optional fixer program run when the check fails and `auto_fix` allows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_program: Option<String>,
    /// Arguments for `fix_program`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fix_args: Vec<String>,
    /// When the check runs.
    #[serde(default)]
    pub cadence: Cadence,
    /// Whether and how findings may be auto-fixed.
    #[serde(default)]
    pub auto_fix: AutoFix,
    /// Per-check severity override; falls back to the `quality_gate` rule default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<RuleSeverity>,
}

impl CheckConfig {
    /// Synthesize the back-compat `test` check from a legacy `test_command`
    /// string. The string is split on whitespace into a program and arguments;
    /// returns `None` when it is blank.
    #[must_use]
    fn from_test_command(command: &str) -> Option<Self> {
        let mut parts = command.split_whitespace();
        let program = parts.next()?.to_string();
        Some(Self {
            name: "test".to_string(),
            program,
            args: parts.map(str::to_string).collect(),
            fix_program: None,
            fix_args: Vec::new(),
            cadence: Cadence::Phase,
            auto_fix: AutoFix::No,
            severity: None,
        })
    }
}

/// When a quality-gate check runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cadence {
    /// Fast check; runs at step completion.
    #[default]
    Step,
    /// Full check; runs at a phase boundary.
    Phase,
}

/// Whether a check's findings may be auto-fixed. Deserializes from `true`
/// ([`AutoFix::Full`]), `false`/absent ([`AutoFix::No`]), or `"safe"`
/// ([`AutoFix::Safe`], the tool's own safe-fix mode only).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AutoFix {
    /// Never auto-fix; report findings only.
    #[default]
    No,
    /// Apply only the tool's documented safe-fix mode.
    Safe,
    /// Apply the configured fixer in full.
    Full,
}

impl serde::Serialize for AutoFix {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            AutoFix::No => serializer.serialize_bool(false),
            AutoFix::Safe => serializer.serialize_str("safe"),
            AutoFix::Full => serializer.serialize_bool(true),
        }
    }
}

impl<'de> serde::Deserialize<'de> for AutoFix {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AutoFixVisitor;
        impl serde::de::Visitor<'_> for AutoFixVisitor {
            type Value = AutoFix;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(r#"a bool or the string "safe""#)
            }
            fn visit_bool<E>(self, value: bool) -> Result<AutoFix, E> {
                Ok(if value { AutoFix::Full } else { AutoFix::No })
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<AutoFix, E> {
                match value {
                    "safe" => Ok(AutoFix::Safe),
                    "full" | "true" => Ok(AutoFix::Full),
                    "no" | "none" | "off" | "false" => Ok(AutoFix::No),
                    other => Err(E::custom(format!(
                        r#"invalid auto_fix {other:?}; expected true, false, or "safe""#
                    ))),
                }
            }
        }
        deserializer.deserialize_any(AutoFixVisitor)
    }
}

impl HarnessConfig {
    /// The effective quality-gate checks: the configured `checks`, or — when
    /// `checks` is empty and `test_command` is set — a single synthesized phase
    /// `test` check, preserving the legacy single-command behavior.
    #[must_use]
    pub fn resolved_checks(&self) -> Vec<CheckConfig> {
        if !self.checks.is_empty() {
            return self.checks.clone();
        }
        self.test_command
            .as_deref()
            .and_then(CheckConfig::from_test_command)
            .into_iter()
            .collect()
    }
}

/// Permission configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    pub profile: PermissionProfile,
}

/// Permission profile. `Bypass` is never the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionProfile {
    #[default]
    Default,
    Relaxed,
    Bypass,
}

/// Quota wait/resume configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaConfig {
    pub auto_resume: QuotaAutoResume,
    pub max_wait_minutes: u32,
    pub resume_requires_clean_workspace: bool,
    pub resume_requires_no_pending_approval: bool,
    pub resume_only_at_step_boundary: bool,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            auto_resume: QuotaAutoResume::default(),
            max_wait_minutes: 360,
            resume_requires_clean_workspace: true,
            resume_requires_no_pending_approval: true,
            resume_only_at_step_boundary: true,
        }
    }
}

/// When to resume a quota-paused run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaAutoResume {
    #[default]
    Off,
    Ask,
    Run,
    Global,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn auto_fix_deserializes_from_bool_and_safe() {
        assert_eq!(
            serde_json::from_value::<AutoFix>(json!(true)).unwrap(),
            AutoFix::Full
        );
        assert_eq!(
            serde_json::from_value::<AutoFix>(json!(false)).unwrap(),
            AutoFix::No
        );
        assert_eq!(
            serde_json::from_value::<AutoFix>(json!("safe")).unwrap(),
            AutoFix::Safe
        );
        assert!(serde_json::from_value::<AutoFix>(json!("bogus")).is_err());
    }

    #[test]
    fn auto_fix_round_trips_through_serialization() {
        for variant in [AutoFix::No, AutoFix::Safe, AutoFix::Full] {
            let value = serde_json::to_value(variant).unwrap();
            assert_eq!(serde_json::from_value::<AutoFix>(value).unwrap(), variant);
        }
        // No serializes as the bool `false`, Full as `true`, Safe as the string.
        assert_eq!(serde_json::to_value(AutoFix::No).unwrap(), json!(false));
        assert_eq!(serde_json::to_value(AutoFix::Full).unwrap(), json!(true));
        assert_eq!(serde_json::to_value(AutoFix::Safe).unwrap(), json!("safe"));
    }

    #[test]
    fn cadence_defaults_to_step() {
        assert_eq!(Cadence::default(), Cadence::Step);
    }

    #[test]
    fn claim_gate_defaults_off_and_parses_warn() {
        // The reachable opt-in surface: unset is off; `warn` enables the gate.
        assert_eq!(HarnessConfig::default().claim_gate, ClaimGate::Off);
        assert!(!ClaimGate::Off.is_enabled());
        assert!(ClaimGate::Warn.is_enabled());
        assert_eq!(
            serde_json::from_value::<ClaimGate>(json!("warn")).unwrap(),
            ClaimGate::Warn
        );
        let harness: HarnessConfig =
            serde_json::from_value(json!({ "claim_gate": "warn" })).unwrap();
        assert!(harness.claim_gate.is_enabled());
    }

    #[test]
    fn storage_defaults_to_a_conservative_cap_and_round_trips() {
        let storage = StorageConfig::default();
        assert!(storage.auto_prune);
        assert_eq!(storage.max_sessions, 100);
        assert_eq!(storage.max_age_days, 90);

        let value = serde_json::to_value(storage).unwrap();
        assert_eq!(
            serde_json::from_value::<StorageConfig>(value).unwrap(),
            storage
        );

        // Partial config fills the rest from defaults.
        let partial: StorageConfig = serde_json::from_value(json!({ "max_sessions": 10 })).unwrap();
        assert_eq!(partial.max_sessions, 10);
        assert!(partial.auto_prune);
        assert_eq!(partial.max_age_days, 90);
    }

    #[test]
    fn tools_config_defaults_reproduce_prior_behaviour() {
        // Absent [tools] block ⇒ broker off ⇒ the full tool set is advertised,
        // exactly as before, and the marker/learning triggers are off.
        let tools = ToolsConfig::default();
        assert!(!tools.broker);
        assert!(!tools.marker);
        assert!(!tools.learning);
        assert!(tools.core.is_empty());
        assert_eq!(tools.working_set_cap, 24);
        assert_eq!(tools.score_floor, 1);
        assert_eq!(tools.graduation_threshold, 3);

        // A whole Config with no tools key fills the defaults.
        let config: Config = serde_json::from_value(json!({})).unwrap();
        assert_eq!(config.tools, ToolsConfig::default());

        // Partial config fills the rest from defaults and round-trips.
        let partial: ToolsConfig = serde_json::from_value(json!({ "broker": true })).unwrap();
        assert!(partial.broker);
        assert_eq!(partial.working_set_cap, 24);
        let value = serde_json::to_value(&partial).unwrap();
        assert_eq!(
            serde_json::from_value::<ToolsConfig>(value).unwrap(),
            partial
        );
    }

    #[test]
    fn ingest_mode_defaults_to_pull() {
        // Pull is the lean default: ingested knowledge is reached on demand via
        // the knowledge_search tool, not seeded into every turn.
        assert_eq!(IngestMode::default(), IngestMode::Pull);
        assert_eq!(IngestConfig::default().mode, IngestMode::Pull);
    }

    #[test]
    fn ingest_mode_round_trips_and_reads_push() {
        for mode in [IngestMode::Pull, IngestMode::Push] {
            let value = serde_json::to_value(mode).unwrap();
            assert_eq!(serde_json::from_value::<IngestMode>(value).unwrap(), mode);
        }
        assert_eq!(
            serde_json::to_value(IngestMode::Pull).unwrap(),
            json!("pull")
        );
        let config: IngestConfig = serde_json::from_value(json!({ "mode": "push" })).unwrap();
        assert_eq!(config.mode, IngestMode::Push);
    }

    #[test]
    fn check_config_round_trips() {
        let check = CheckConfig {
            name: "clippy".to_string(),
            program: "cargo".to_string(),
            args: vec!["clippy".to_string(), "--workspace".to_string()],
            fix_program: Some("cargo".to_string()),
            fix_args: vec!["clippy".to_string(), "--fix".to_string()],
            cadence: Cadence::Step,
            auto_fix: AutoFix::Safe,
            severity: Some(RuleSeverity::Block),
        };
        let value = serde_json::to_value(&check).unwrap();
        assert_eq!(serde_json::from_value::<CheckConfig>(value).unwrap(), check);
    }

    #[test]
    fn check_minimal_fields_default() {
        // Only name + program required; the rest default.
        let check: CheckConfig =
            serde_json::from_value(json!({ "name": "fmt", "program": "cargo" })).unwrap();
        assert_eq!(check.cadence, Cadence::Step);
        assert_eq!(check.auto_fix, AutoFix::No);
        assert!(check.args.is_empty());
        assert!(check.severity.is_none());
    }

    #[test]
    fn resolved_checks_synthesizes_a_test_check_from_test_command() {
        let harness = HarnessConfig {
            test_command: Some("cargo test --workspace".to_string()),
            ..HarnessConfig::default()
        };
        let resolved = harness.resolved_checks();
        assert_eq!(resolved.len(), 1);
        let check = &resolved[0];
        assert_eq!(check.name, "test");
        assert_eq!(check.program, "cargo");
        assert_eq!(
            check.args,
            vec!["test".to_string(), "--workspace".to_string()]
        );
        assert_eq!(check.cadence, Cadence::Phase);
    }

    #[test]
    fn resolved_checks_prefers_explicit_checks_over_test_command() {
        let harness = HarnessConfig {
            test_command: Some("cargo test".to_string()),
            checks: vec![CheckConfig {
                name: "fmt".to_string(),
                program: "cargo".to_string(),
                args: vec!["fmt".to_string(), "--check".to_string()],
                fix_program: None,
                fix_args: Vec::new(),
                cadence: Cadence::Step,
                auto_fix: AutoFix::Full,
                severity: None,
            }],
            ..HarnessConfig::default()
        };
        let resolved = harness.resolved_checks();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "fmt");
    }

    #[test]
    fn resolved_checks_is_empty_without_checks_or_test_command() {
        assert!(HarnessConfig::default().resolved_checks().is_empty());
    }

    #[test]
    fn harness_rule_severity_overrides_round_trip() {
        // `[harness.rules]` is a free-form severity map, so a rule key such as
        // `check_before_launch` is carried without a dedicated schema field; an
        // absent key leaves the rule at its own default.
        let harness: HarnessConfig = serde_json::from_value(json!({
            "rules": { "check_before_launch": "block" }
        }))
        .unwrap();
        assert_eq!(
            harness.rules.get("check_before_launch"),
            Some(&RuleSeverity::Block)
        );
        for severity in [RuleSeverity::Off, RuleSeverity::Warn, RuleSeverity::Block] {
            let value = serde_json::to_value(severity).unwrap();
            assert_eq!(
                serde_json::from_value::<RuleSeverity>(value).unwrap(),
                severity
            );
        }
        assert!(HarnessConfig::default().rules.is_empty());
    }

    #[test]
    fn default_tool_call_budget_bounds_are_parity() {
        // max == soft start by default, so the adaptive ceiling reproduces the
        // flat fixed-budget stop until an operator raises the max.
        let harness = HarnessConfig::default();
        assert_eq!(harness.tool_call_budget, 50);
        assert_eq!(harness.tool_call_budget_max, harness.tool_call_budget);
    }

    #[test]
    fn omitted_budget_fields_fall_back_to_defaults() {
        // An existing config that predates these fields still loads: the
        // struct-level serde default fills the missing budget fields.
        let harness: HarnessConfig =
            serde_json::from_str(r#"{"context_token_limit": 8000}"#).unwrap();
        assert_eq!(harness.context_token_limit, 8000);
        assert_eq!(harness.tool_call_budget, 50);
        assert_eq!(harness.tool_call_budget_max, 50);
    }
}
