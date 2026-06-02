//! Skill discovery and loading (project-local and user-local).

use std::path::{Path, PathBuf};

use crate::error::SkillError;
use crate::manifest::SkillManifest;

/// A loaded skill: its manifest, its instruction text, and where it lives.
#[derive(Debug, Clone)]
pub struct Skill {
    pub manifest: SkillManifest,
    pub instructions: String,
    pub dir: PathBuf,
}

impl Skill {
    /// The permission declarations to show before executing this skill.
    #[must_use]
    pub fn declared_permissions(&self) -> &[String] {
        &self.manifest.permissions
    }
}

/// A set of discovered skills.
#[derive(Debug, Clone, Default)]
pub struct SkillSet {
    skills: Vec<Skill>,
}

impl SkillSet {
    /// Load skills from each directory: every immediate subdirectory containing a
    /// `skill.toml` and a `SKILL.md` is a skill. Later directories do not override
    /// earlier ones; all are collected.
    ///
    /// # Errors
    /// Returns [`SkillError::InvalidManifest`] if a manifest fails to parse.
    pub fn load(dirs: &[PathBuf]) -> Result<Self, SkillError> {
        let mut skills = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let skill_dir = entry.path();
                let manifest_path = skill_dir.join("skill.toml");
                let instructions_path = skill_dir.join("SKILL.md");
                if !manifest_path.is_file() || !instructions_path.is_file() {
                    continue;
                }
                let manifest = SkillManifest::parse(&read(&manifest_path)?)?;
                let instructions = read(&instructions_path)?;
                skills.push(Skill {
                    manifest,
                    instructions,
                    dir: skill_dir,
                });
            }
        }
        Ok(Self { skills })
    }

    /// The names of all loaded skills.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.skills
            .iter()
            .map(|s| s.manifest.name.as_str())
            .collect()
    }

    /// Find a skill by exact name (manual invocation).
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.manifest.name == name)
    }

    /// Skills relevant to `query`: a description keyword match or an explicit
    /// command trigger. Description-based relevance is the default.
    #[must_use]
    pub fn relevant(&self, query: &str) -> Vec<&Skill> {
        let query_lower = query.to_ascii_lowercase();
        let query_words: Vec<&str> = query_lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| w.len() > 2)
            .collect();
        self.skills
            .iter()
            .filter(|skill| {
                let description = skill.manifest.description.to_ascii_lowercase();
                let trigger_hit = skill
                    .manifest
                    .triggers
                    .commands
                    .iter()
                    .any(|c| query_lower.contains(&c.to_ascii_lowercase()));
                trigger_hit || query_words.iter().any(|w| description.contains(w))
            })
            .collect()
    }
}

fn read(path: &Path) -> Result<String, SkillError> {
    std::fs::read_to_string(path).map_err(|source| SkillError::Io {
        path: path.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str, description: &str, permissions: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("skill.toml"),
            format!(
                "name = \"{name}\"\ndescription = \"{description}\"\nversion = \"0.1.0\"\npermissions = [{permissions}]\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("# {name}\n\nDo the thing.\n")).unwrap();
    }

    #[test]
    fn loads_a_local_skill_and_exposes_instructions_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "harness-helper",
            "guide a harness step",
            "\"read:repo\"",
        );
        let set = SkillSet::load(&[dir.path().to_path_buf()]).unwrap();

        assert_eq!(set.names(), vec!["harness-helper"]);
        let skill = set.by_name("harness-helper").unwrap();
        assert!(skill.instructions.contains("Do the thing"));
        // Permissions are visible before execution.
        assert_eq!(skill.declared_permissions(), &["read:repo".to_string()]);
    }

    #[test]
    fn relevance_matches_description_and_triggers() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "harness-helper", "guide a harness step", "");
        write_skill(dir.path(), "gardening", "water the plants", "");
        let set = SkillSet::load(&[dir.path().to_path_buf()]).unwrap();

        let relevant = set.relevant("how do I run a harness step");
        assert_eq!(relevant.len(), 1);
        assert_eq!(relevant[0].manifest.name, "harness-helper");
    }
}
