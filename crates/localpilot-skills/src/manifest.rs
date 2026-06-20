//! The LocalPilot skill manifest (`skill.toml`).

use serde::{Deserialize, Serialize};

use crate::error::SkillError;

/// How a skill may be reached (the invocation axis of the skill model, ADR-0027).
/// Independent of authority; carried in a `SKILL.md` by `disable-model-invocation`
/// and in a `skill.toml` by an optional `invocation` field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Invocation {
    /// Reachable only by a human typing the skill's name (deterministic load).
    /// Set by `disable-model-invocation: true`. Never returned by skill search.
    UserOnly,
    /// Reachable by on-demand search (and still by name). The default when the
    /// invocation field is absent, matching the `SKILL.md` convention that omitting
    /// `disable-model-invocation` leaves a skill model-reachable.
    #[default]
    Discoverable,
}

impl Invocation {
    /// Whether this skill may be surfaced by on-demand search (the `skill_search`
    /// tool). User-only skills are excluded from the search candidate set.
    #[must_use]
    pub fn is_discoverable(self) -> bool {
        matches!(self, Invocation::Discoverable)
    }
}

/// A parsed `skill.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillManifest {
    pub name: String,
    pub description: String,
    pub version: String,
    /// Who can reach the skill. Defaults to [`Invocation::Discoverable`] when absent.
    #[serde(default)]
    pub invocation: Invocation,
    /// Optional hint describing the argument a user-invoked skill expects, carried
    /// from the `SKILL.md` `argument-hint` frontmatter. Recorded, not yet consumed.
    #[serde(
        default,
        rename = "argument-hint",
        skip_serializing_if = "Option::is_none"
    )]
    pub argument_hint: Option<String>,
    #[serde(default)]
    pub triggers: SkillTriggers,
    /// Builtin tools the skill needs.
    #[serde(default)]
    pub required_tools: Vec<String>,
    /// Permission declarations a script/asset needs; surfaced before execution
    /// and enforced by the permission engine (never a bypass).
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub assets: Vec<String>,
    #[serde(default)]
    pub scripts: Vec<String>,
}

/// How a skill is triggered. Description-based relevance is the default; these
/// are optional explicit triggers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillTriggers {
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub file_globs: Vec<String>,
    #[serde(default)]
    pub regexes: Vec<String>,
}

impl SkillManifest {
    /// Parse a manifest from TOML.
    ///
    /// # Errors
    /// Returns [`SkillError::InvalidManifest`] naming the offending field.
    pub fn parse(toml_str: &str) -> Result<Self, SkillError> {
        use figment::providers::Format;
        figment::Figment::new()
            .merge(figment::providers::Toml::string(toml_str))
            .extract()
            .map_err(|e| SkillError::InvalidManifest(e.to_string()))
    }

    /// Parse a manifest from a standard `SKILL.md` file's YAML frontmatter
    /// (the agentskills.io format: required `name` and `description`; the
    /// name is at most 64 characters of lowercase letters, digits, and
    /// hyphens). Returns the manifest and the markdown body after the
    /// frontmatter.
    ///
    /// # Errors
    /// Returns [`SkillError::InvalidManifest`] if the frontmatter is missing,
    /// malformed, or violates the name constraints.
    pub fn parse_skill_md(content: &str) -> Result<(Self, String), SkillError> {
        let rest = content.strip_prefix("---").ok_or_else(|| {
            SkillError::InvalidManifest(
                "SKILL.md must start with `---` YAML frontmatter".to_string(),
            )
        })?;
        let (front, body) = rest.split_once("\n---").ok_or_else(|| {
            SkillError::InvalidManifest("unterminated SKILL.md frontmatter".to_string())
        })?;
        let front: SkillFrontmatter =
            serde_yaml::from_str(front).map_err(|e| SkillError::InvalidManifest(e.to_string()))?;
        if front.name.is_empty()
            || front.name.len() > 64
            || !front
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(SkillError::InvalidManifest(format!(
                "skill name `{}` must be 1-64 lowercase letters, digits, or hyphens",
                front.name
            )));
        }
        let manifest = Self {
            name: front.name,
            description: front.description,
            version: front
                .metadata
                .get("version")
                .cloned()
                .unwrap_or_else(|| "0.0.0".to_string()),
            // The invocation axis the loader previously dropped: a skill marked
            // `disable-model-invocation: true` is user-only; otherwise discoverable.
            invocation: if front.disable_model_invocation {
                Invocation::UserOnly
            } else {
                Invocation::Discoverable
            },
            argument_hint: front.argument_hint,
            triggers: SkillTriggers::default(),
            required_tools: Vec::new(),
            permissions: Vec::new(),
            assets: Vec::new(),
            scripts: Vec::new(),
        };
        Ok((manifest, body.trim_start_matches('\n').to_string()))
    }
}

/// The frontmatter fields of a standard `SKILL.md` file.
#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    /// `disable-model-invocation: true` makes the skill user-only (the invocation
    /// axis the loader previously discarded on load).
    #[serde(default, rename = "disable-model-invocation")]
    disable_model_invocation: bool,
    /// Optional hint describing the argument the skill expects.
    #[serde(default, rename = "argument-hint")]
    argument_hint: Option<String>,
    #[serde(default)]
    metadata: std::collections::BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "\
name = \"clean-room-guard\"\n\
description = \"Apply clean-room provenance rules\"\n\
version = \"0.1.0\"\n\
required_tools = [\"read_file\"]\n\
permissions = [\"read:docs\"]\n\
\n\
[triggers]\n\
commands = [\"guard\"]\n\
file_globs = [\"**/*.rs\"]\n";

    #[test]
    fn parses_a_valid_manifest() {
        let manifest = SkillManifest::parse(VALID).unwrap();
        assert_eq!(manifest.name, "clean-room-guard");
        assert_eq!(manifest.required_tools, vec!["read_file"]);
        assert_eq!(manifest.triggers.commands, vec!["guard"]);
        assert_eq!(manifest.permissions, vec!["read:docs"]);
    }

    #[test]
    fn invalid_manifest_reports_the_bad_field() {
        // Missing the required `name` field.
        let err = SkillManifest::parse("description = \"x\"\nversion = \"0.1.0\"\n").unwrap_err();
        match err {
            SkillError::InvalidManifest(message) => assert!(message.contains("name"), "{message}"),
            other => panic!("expected InvalidManifest, got {other:?}"),
        }
    }

    #[test]
    fn skill_md_disable_model_invocation_is_user_only() {
        let content = "---\n\
name: handoff\n\
description: Compact the conversation into a handoff document.\n\
argument-hint: \"What will the next session focus on?\"\n\
disable-model-invocation: true\n\
---\n\
\n\
Write the handoff.\n";
        let (manifest, body) = SkillManifest::parse_skill_md(content).unwrap();
        assert_eq!(manifest.invocation, Invocation::UserOnly);
        assert!(!manifest.invocation.is_discoverable());
        assert_eq!(
            manifest.argument_hint.as_deref(),
            Some("What will the next session focus on?")
        );
        assert!(body.starts_with("Write the handoff"));
    }

    #[test]
    fn skill_md_without_the_flag_defaults_to_discoverable() {
        let content = "---\n\
name: add-provider\n\
description: Guide adding a model provider.\n\
---\n\
\n\
Steps.\n";
        let (manifest, _body) = SkillManifest::parse_skill_md(content).unwrap();
        assert_eq!(manifest.invocation, Invocation::Discoverable);
        assert!(manifest.invocation.is_discoverable());
        assert!(manifest.argument_hint.is_none());
    }

    #[test]
    fn toml_invocation_round_trips_and_defaults_to_discoverable() {
        // Absent ⇒ discoverable.
        let absent = SkillManifest::parse(VALID).unwrap();
        assert_eq!(absent.invocation, Invocation::Discoverable);

        // Explicit user-only round-trips through the manifest.
        let user_only = SkillManifest::parse(
            "name = \"local-only\"\n\
description = \"x\"\n\
version = \"0.1.0\"\n\
invocation = \"user-only\"\n",
        )
        .unwrap();
        assert_eq!(user_only.invocation, Invocation::UserOnly);
    }
}
