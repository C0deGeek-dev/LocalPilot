//! Configuration schema.
//!
//! These types mirror `.unshackled.toml`. They are deliberately permissive about
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
}
