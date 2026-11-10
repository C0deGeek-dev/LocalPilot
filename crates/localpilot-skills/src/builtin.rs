//! Built-in task-guidance packages.
//!
//! Short, reviewed procedures for the six task intents and supplements for the
//! first stacks and products, in the ordinary package format (`skill.toml`
//! with a `[guidance]` section and a `SKILL.md` body). They are compiled into
//! the binary, take the lowest precedence, and are only part of a catalog when
//! task guidance is switched on: a user or project package with the same name
//! replaces one, and with guidance off nothing about discovery changes.

use std::path::PathBuf;

use crate::builtin_packs::PACKS;
use crate::loader::{Skill, SkillScope, SkillSet};
use crate::manifest::SkillManifest;

/// The built-in packages, parsed. A package that fails to parse is left out;
/// a test keeps every one of them valid.
#[must_use]
pub fn builtin_guidance() -> Vec<Skill> {
    PACKS
        .iter()
        .filter_map(|(name, manifest, body)| {
            let parsed = SkillManifest::parse(manifest).ok()?;
            let dir = PathBuf::from(format!("<built-in>/{name}"));
            Some(Skill {
                guidance: crate::read_applicability(manifest, &dir, body),
                manifest: parsed,
                instructions: (*body).to_string(),
                dir,
                scope: SkillScope::Builtin,
            })
        })
        .collect()
}

impl SkillSet {
    /// This set with the built-in task-guidance packages added beneath every
    /// discovered package: a discovered package with the same name wins.
    #[must_use]
    pub fn with_builtin_guidance(mut self) -> Self {
        for skill in builtin_guidance() {
            self.add_lowest(skill);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::guidance::{Applicability, GuidanceRole};

    #[test]
    fn every_built_in_package_parses_with_usable_guidance() {
        let packs = builtin_guidance();
        assert_eq!(packs.len(), PACKS.len());
        for skill in &packs {
            let Applicability::Valid(metadata) = &skill.guidance else {
                panic!("{}: {:?}", skill.manifest.name, skill.guidance);
            };
            assert!(skill.manifest.name.starts_with("guide-"));
            assert!(skill.manifest.invocation.is_discoverable());
            // Every declared section exists, so selection loads a real part.
            assert!(
                !metadata.body_sections.is_empty(),
                "{}",
                skill.manifest.name
            );
        }
    }

    #[test]
    fn there_is_one_procedure_per_intent_except_packaging() {
        let packs = builtin_guidance();
        for intent in [
            "explain",
            "investigate",
            "implement",
            "refactor",
            "review",
            "measure",
        ] {
            let procedures: Vec<&str> = packs
                .iter()
                .filter(|s| {
                    s.guidance.metadata().is_some_and(|m| {
                        m.role == GuidanceRole::Procedure && m.intents.iter().any(|i| i == intent)
                    })
                })
                .map(|s| s.manifest.name.as_str())
                .collect();
            assert_eq!(procedures.len(), 1, "{intent}: {procedures:?}");
        }
    }

    #[test]
    fn explanation_and_review_do_not_ask_for_edits_or_a_launch() {
        for name in ["guide-explain", "guide-review"] {
            let skill = builtin_guidance()
                .into_iter()
                .find(|s| s.manifest.name == name)
                .unwrap();
            let body = skill.instructions.to_lowercase();
            assert!(
                body.contains("do not edit") || body.contains("do not repair"),
                "{name}"
            );
            assert!(body.contains("launch"), "{name} says when not to launch");
        }
    }

    #[test]
    fn runtime_supplements_name_the_evidence_and_what_does_not_count() {
        for (name, evidence) in [
            ("guide-cli", "exit code"),
            ("guide-service", "response"),
            ("guide-desktop", "window"),
            ("guide-web", "console"),
            ("guide-game", "input"),
            ("guide-godot4", "scene"),
        ] {
            let skill = builtin_guidance()
                .into_iter()
                .find(|s| s.manifest.name == name)
                .unwrap();
            let body = skill.instructions.to_lowercase();
            assert!(body.contains(evidence), "{name} names {evidence}");
            assert!(
                body.contains("not ") || body.contains("does not"),
                "{name} says what is not evidence"
            );
            let metadata = skill.guidance.metadata().unwrap();
            assert!(
                !metadata.capabilities_required_for_claim.is_empty(),
                "{name} names the capability its claims need"
            );
        }
    }
}
