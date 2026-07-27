//! The subagent definition format.
//!
//! A definition is **data, never code**: it names a model, the tools the agent
//! may use, which parts of the system prompt it wants, and its own instructions.
//! It cannot execute anything, reference a host path, or grant itself authority
//! — the tool list is an upper bound that the caller intersects with what the
//! parent session already holds.
//!
//! Unknown fields are refused at load rather than ignored. A typo in a field
//! name is the difference between "this agent has three tools" and "this agent
//! has every tool", so silence is the wrong default.

use serde::Deserialize;

use crate::error::AgentError;
use crate::template;

/// The only format version this build understands. Bumping it is a breaking
/// change to user-authored files, so it is versioned from the first release
/// rather than retrofitted.
pub const FORMAT_VERSION: u32 = 1;

/// Reasoning effort a definition may request, mapped by the host onto whatever
/// the active provider supports. Deliberately a closed set: a definition cannot
/// pass an arbitrary provider knob through.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    /// A stable lowercase identifier for display.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Which parts of the host's system prompt this agent wants.
///
/// A narrow agent should not pay context for guidance it cannot act on: an agent
/// with two read-only tools has no use for shell discipline or commit etiquette.
/// Every field defaults to the value that keeps an under-specified definition
/// safe and useful rather than surprising.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct PromptParts {
    /// The agent-mode preamble and workspace discipline. On by default.
    pub include_base: bool,
    /// Editing guidance (which write tool to prefer, modular files). On by
    /// default; pointless for a read-only agent.
    pub include_editing_guidance: bool,
    /// Permission-profile and commit-etiquette framing. **On by default and
    /// costly to turn off** — see [`PromptParts::include_safety`].
    pub include_safety: bool,
    /// The tool-use loop and shell discipline. On by default.
    pub include_tool_instructions: bool,
    /// Inspect-before-launch guidance. On by default.
    pub include_look_before_launch: bool,
}

impl Default for PromptParts {
    fn default() -> Self {
        Self {
            include_base: true,
            include_editing_guidance: true,
            include_safety: true,
            include_tool_instructions: true,
            include_look_before_launch: true,
        }
    }
}

impl PromptParts {
    /// Every part on — what the main session uses.
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }

    /// Whether the safety framing is present. Kept as a named query because the
    /// loader warns when a definition turns it off while keeping tools that can
    /// write, execute, or reach the network.
    #[must_use]
    pub fn include_safety(self) -> bool {
        self.include_safety
    }
}

/// One subagent definition, as parsed from a `*.agent.yaml` file.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentDefinition {
    /// Format version; must equal [`FORMAT_VERSION`].
    pub format_version: u32,
    /// Stable identifier used to invoke the agent. Lowercase letters, digits,
    /// `-` and `_`; see [`validate_name`].
    pub name: String,
    /// Human-readable name for listings. Defaults to `name`.
    #[serde(default)]
    pub display_name: Option<String>,
    /// One line telling a caller when to delegate to this agent. Required —
    /// an agent nobody can tell apart from another is not usable.
    pub description: String,
    /// Model this agent runs on. `None` inherits the parent session's model.
    #[serde(default)]
    pub model: Option<String>,
    /// Requested reasoning effort, when the provider supports one.
    #[serde(default)]
    pub effort: Option<Effort>,
    /// Tools this agent may use — an **upper bound**, intersected by the host
    /// with the parent session's own tools. Entries are tool names, optionally
    /// namespaced (`server/tool`), or the single wildcard `*` meaning "whatever
    /// the parent has".
    #[serde(default)]
    pub tools: Vec<String>,
    /// Which parts of the host system prompt to include.
    #[serde(default)]
    pub prompt_parts: PromptParts,
    /// The agent's own instructions, appended after the selected parts. May use
    /// the closed placeholder vocabulary in [`crate::template`].
    pub prompt: String,
}

impl AgentDefinition {
    /// Parse and validate one definition from YAML.
    ///
    /// # Errors
    /// Returns [`AgentError::Parse`] for malformed YAML or an unknown field, and
    /// [`AgentError::Invalid`] when a field fails validation.
    pub fn from_yaml(text: &str) -> Result<Self, AgentError> {
        let definition: Self =
            serde_yaml::from_str(text).map_err(|e| AgentError::Parse(e.to_string()))?;
        definition.validate()?;
        Ok(definition)
    }

    /// The name shown in listings.
    #[must_use]
    pub fn display(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// Whether this definition asks for every tool the parent holds.
    #[must_use]
    pub fn wants_all_tools(&self) -> bool {
        self.tools.iter().any(|t| t == "*")
    }

    fn validate(&self) -> Result<(), AgentError> {
        if self.format_version != FORMAT_VERSION {
            return Err(AgentError::Invalid(format!(
                "format_version {} is not supported; this build understands {FORMAT_VERSION}",
                self.format_version
            )));
        }
        validate_name(&self.name)?;
        if self.description.trim().is_empty() {
            return Err(AgentError::Invalid(
                "description must not be empty — it is how a caller tells agents apart".to_string(),
            ));
        }
        if self.prompt.trim().is_empty() {
            return Err(AgentError::Invalid("prompt must not be empty".to_string()));
        }
        template::validate(&self.prompt)?;
        for entry in &self.tools {
            validate_tool_entry(entry)?;
        }
        Ok(())
    }
}

/// Names reserved because they would collide with the host's own vocabulary in
/// listings and command output.
const RESERVED_NAMES: &[&str] = &["all", "none", "self", "parent", "default", "list", "show"];

/// Validate an agent name: lowercase ASCII letters, digits, `-`, `_`; must start
/// with a letter; 1–48 characters; not reserved.
///
/// # Errors
/// Returns [`AgentError::Invalid`] describing the first rule broken.
pub fn validate_name(name: &str) -> Result<(), AgentError> {
    if name.is_empty() || name.len() > 48 {
        return Err(AgentError::Invalid(format!(
            "name {name:?} must be 1-48 characters"
        )));
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Err(AgentError::Invalid(format!(
            "name {name:?} must start with a lowercase letter"
        )));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_'))
    {
        return Err(AgentError::Invalid(format!(
            "name {name:?} contains {bad:?}; use lowercase letters, digits, '-' or '_'"
        )));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(AgentError::Invalid(format!(
            "name {name:?} is reserved; pick another"
        )));
    }
    Ok(())
}

/// Validate one tool-list entry: `*`, `tool_name`, or `server/tool_name`.
fn validate_tool_entry(entry: &str) -> Result<(), AgentError> {
    if entry == "*" {
        return Ok(());
    }
    if entry.trim().is_empty() {
        return Err(AgentError::Invalid(
            "a tool entry must not be empty".to_string(),
        ));
    }
    let segments: Vec<&str> = entry.split('/').collect();
    if segments.len() > 2 || segments.iter().any(|s| s.is_empty()) {
        return Err(AgentError::Invalid(format!(
            "tool entry {entry:?} must be `name` or `server/name`"
        )));
    }
    if entry.contains(char::is_whitespace) {
        return Err(AgentError::Invalid(format!(
            "tool entry {entry:?} must not contain whitespace"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "\
format_version: 1
name: reviewer
description: Reviews a diff and reports findings.
prompt: |
  Review the diff and report findings.
";

    #[test]
    fn a_minimal_definition_parses_with_safe_defaults() {
        let d = AgentDefinition::from_yaml(MINIMAL).expect("parses");
        assert_eq!(d.name, "reviewer");
        assert_eq!(d.display(), "reviewer", "display falls back to name");
        assert!(
            d.model.is_none(),
            "model defaults to inheriting the parent's"
        );
        assert!(d.tools.is_empty(), "no tools unless asked for");
        assert_eq!(
            d.prompt_parts,
            PromptParts::all(),
            "an unspecified definition gets the full prompt, not a stripped one"
        );
    }

    #[test]
    fn an_unknown_field_is_refused_and_named() {
        let text = format!("{MINIMAL}toolz:\n  - read_file\n");
        let error = AgentDefinition::from_yaml(&text).expect_err("unknown field must be refused");
        let message = error.to_string();
        assert!(
            message.contains("toolz"),
            "the offending key must be named: {message}"
        );
    }

    #[test]
    fn a_missing_required_field_is_refused() {
        let text = "format_version: 1\nname: reviewer\nprompt: hi\n";
        assert!(
            AgentDefinition::from_yaml(text).is_err(),
            "description is required"
        );
    }

    #[test]
    fn a_wrong_format_version_is_refused_with_the_supported_one() {
        let text = MINIMAL.replace("format_version: 1", "format_version: 7");
        let message = AgentDefinition::from_yaml(&text)
            .expect_err("version 7 is unsupported")
            .to_string();
        assert!(message.contains('7') && message.contains('1'), "{message}");
    }

    #[test]
    fn an_empty_description_or_prompt_is_refused() {
        let no_desc = MINIMAL.replace(
            "description: Reviews a diff and reports findings.",
            "description: '   '",
        );
        assert!(AgentDefinition::from_yaml(&no_desc).is_err());
        let no_prompt = "\
format_version: 1
name: reviewer
description: Reviews a diff.
prompt: '   '
";
        assert!(AgentDefinition::from_yaml(no_prompt).is_err());
    }

    #[test]
    fn names_are_validated() {
        assert!(validate_name("code-review").is_ok());
        assert!(validate_name("agent_2").is_ok());
        assert!(validate_name("").is_err(), "empty");
        assert!(validate_name("Reviewer").is_err(), "uppercase start");
        assert!(validate_name("2fast").is_err(), "digit start");
        assert!(validate_name("has space").is_err());
        assert!(validate_name("has/slash").is_err());
        assert!(validate_name("all").is_err(), "reserved");
        assert!(validate_name(&"x".repeat(49)).is_err(), "too long");
    }

    #[test]
    fn tool_entries_accept_plain_namespaced_and_wildcard() {
        let text = format!("{MINIMAL}tools:\n  - read_file\n  - github/get_issue\n  - '*'\n");
        let d = AgentDefinition::from_yaml(&text).expect("parses");
        assert_eq!(d.tools.len(), 3);
        assert!(d.wants_all_tools());
    }

    #[test]
    fn a_malformed_tool_entry_is_refused() {
        for bad in ["a/b/c", "with space", "/leading", "trailing/"] {
            let text = format!("{MINIMAL}tools:\n  - '{bad}'\n");
            assert!(
                AgentDefinition::from_yaml(&text).is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn prompt_parts_can_be_narrowed_individually() {
        let text = format!(
            "{MINIMAL}prompt_parts:\n  include_editing_guidance: false\n  include_safety: false\n"
        );
        let d = AgentDefinition::from_yaml(&text).expect("parses");
        assert!(!d.prompt_parts.include_editing_guidance);
        assert!(!d.prompt_parts.include_safety());
        assert!(
            d.prompt_parts.include_base,
            "unlisted parts keep their default"
        );
    }

    #[test]
    fn an_unknown_prompt_part_is_refused() {
        let text = format!("{MINIMAL}prompt_parts:\n  include_everything: true\n");
        assert!(AgentDefinition::from_yaml(&text).is_err());
    }
}
