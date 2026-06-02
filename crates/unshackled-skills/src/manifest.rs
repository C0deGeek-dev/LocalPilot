//! The Unshackled skill manifest (`skill.toml`).

use serde::{Deserialize, Serialize};

use crate::error::SkillError;

/// A parsed `skill.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillManifest {
    pub name: String,
    pub description: String,
    pub version: String,
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
}
