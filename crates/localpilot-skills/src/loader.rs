//! Skill discovery and loading (project-local and user-local).

use std::path::{Path, PathBuf};

use crate::error::SkillError;
use crate::manifest::{Invocation, SkillManifest};

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
    /// Skills that failed to parse, as `path: error` lines. A malformed skill
    /// (bad frontmatter, unreadable file) is skipped and recorded here rather
    /// than aborting the whole set — one bad file must never hide every valid
    /// project skill (LocalHub#38).
    skipped: Vec<String>,
}

impl SkillSet {
    /// Load skills from each directory: every immediate subdirectory containing
    /// a `SKILL.md` is a skill. A directory with a `skill.toml` uses the
    /// LocalPilot manifest (triggers, required tools, permission
    /// declarations); a directory with only a `SKILL.md` is read in the
    /// standard agentskills.io format (YAML frontmatter `name` +
    /// `description`), so cross-harness skill directories load as-is. Later
    /// directories do not override earlier ones; all are collected.
    ///
    /// A malformed skill (unparseable frontmatter, unreadable file) is skipped
    /// and recorded in [`SkillSet::skipped`] with its path, so one bad file
    /// never hides every valid project skill (LocalHub#38).
    ///
    /// # Errors
    /// Currently never returns `Err` — per-skill failures are collected, not
    /// fatal. The `Result` is kept so a future catastrophic failure can be
    /// surfaced without a breaking signature change.
    pub fn load(dirs: &[PathBuf]) -> Result<Self, SkillError> {
        let mut skills = Vec::new();
        let mut skipped = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let skill_dir = entry.path();
                let instructions_path = skill_dir.join("SKILL.md");
                if !instructions_path.is_file() {
                    continue;
                }
                match Self::load_one(&skill_dir) {
                    Ok(skill) => skills.push(skill),
                    Err(error) => {
                        skipped.push(format!("{}: {error}", instructions_path.display()));
                    }
                }
            }
        }
        Ok(Self { skills, skipped })
    }

    /// Load a single skill directory: a `skill.toml` uses the LocalPilot
    /// manifest, otherwise `SKILL.md` frontmatter is read in the standard
    /// format. The error path/diagnostic is added by the caller.
    fn load_one(skill_dir: &Path) -> Result<Skill, SkillError> {
        let manifest_path = skill_dir.join("skill.toml");
        let instructions_path = skill_dir.join("SKILL.md");
        if manifest_path.is_file() {
            Ok(Skill {
                manifest: SkillManifest::parse(&read(&manifest_path)?)?,
                instructions: read(&instructions_path)?,
                dir: skill_dir.to_path_buf(),
            })
        } else {
            let (manifest, body) = SkillManifest::parse_skill_md(&read(&instructions_path)?)?;
            Ok(Skill {
                manifest,
                instructions: body,
                dir: skill_dir.to_path_buf(),
            })
        }
    }

    /// Skills that failed to parse and were skipped, as `path: error` lines.
    /// A caller (e.g. `skills list`) surfaces these as warnings so a malformed
    /// skill is visible without hiding the valid ones (LocalHub#38).
    #[must_use]
    pub fn skipped(&self) -> &[String] {
        &self.skipped
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

    /// Skills relevant to `query`, for on-demand discovery (the `skill_search`
    /// tool): a description keyword match or an explicit command trigger.
    /// Only **discoverable** skills are candidates — user-only skills are reached
    /// solely by [`SkillSet::by_name`] (a typed name), never by search — so a model
    /// can never auto-surface a skill the author marked user-only. Description-based
    /// relevance is the default.
    #[must_use]
    pub fn relevant(&self, query: &str) -> Vec<&Skill> {
        let query_lower = query.to_ascii_lowercase();
        let query_words: Vec<&str> = query_lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| w.len() > 2)
            .collect();
        self.skills
            .iter()
            .filter(|skill| skill.manifest.invocation == Invocation::Discoverable)
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

/// The project-local skill directories LocalPilot reads, in load order: its
/// own directory first, then cross-harness standard locations. Project-local
/// skills load only behind the workspace trust gate (the caller enforces it).
#[must_use]
pub fn standard_skill_dirs(project_root: &Path) -> Vec<PathBuf> {
    vec![
        project_root.join(".localpilot").join("skills"),
        project_root.join(".agents").join("skills"),
    ]
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
    fn loads_a_standard_skill_md_without_a_toml_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("pdf-processing");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---
name: pdf-processing
description: Extract text from PDF files
metadata:
  version: \"1.2.0\"
---

# PDF Processing

Use the bundled script.
",
        )
        .unwrap();

        let set = SkillSet::load(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(set.names(), vec!["pdf-processing"]);
        let skill = set.by_name("pdf-processing").unwrap();
        assert_eq!(skill.manifest.version, "1.2.0");
        assert!(skill.instructions.starts_with("# PDF Processing"));
        // No declared permissions: the manifest grants nothing implicitly.
        assert!(skill.declared_permissions().is_empty());
    }

    #[test]
    fn a_bad_standard_skill_is_skipped_and_reported_not_fatal() {
        // LocalHub#38: a malformed skill is skipped with its path recorded, and
        // valid skills in the same directory still load — one bad file must not
        // hide the rest.
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(
            bad.join("SKILL.md"),
            "---\nname: Not Valid\ndescription: x\n---\nbody\n",
        )
        .unwrap();
        write_skill(dir.path(), "good-skill", "a valid skill", "");

        let set = SkillSet::load(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(
            set.names(),
            vec!["good-skill"],
            "the valid skill still loads"
        );
        assert_eq!(set.skipped().len(), 1, "the bad skill is reported");
        assert!(
            set.skipped()[0].contains("bad") && set.skipped()[0].contains("SKILL.md"),
            "the skipped report names the offending path: {}",
            set.skipped()[0]
        );
    }

    #[test]
    fn a_bom_prefixed_standard_skill_loads() {
        // LocalHub#38: a SKILL.md saved as UTF-8 with a BOM (EF BB BF) loads
        // identically to its BOM-free form.
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("bom-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let body =
            "---\nname: bom-skill\ndescription: a skill saved with a BOM\n---\nDo the thing.\n";
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(body.as_bytes());
        std::fs::write(skill_dir.join("SKILL.md"), bytes).unwrap();

        let set = SkillSet::load(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(set.names(), vec!["bom-skill"]);
        assert!(
            set.skipped().is_empty(),
            "a BOM is tolerated, not a parse failure"
        );
        assert!(set
            .by_name("bom-skill")
            .unwrap()
            .instructions
            .starts_with("Do the thing."));
    }

    #[test]
    fn standard_dirs_cover_localpilot_and_cross_harness_locations() {
        let dirs = standard_skill_dirs(Path::new("/repo"));
        assert!(dirs[0].ends_with("skills"));
        assert_eq!(dirs.len(), 2);
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

    #[test]
    fn user_only_skills_are_excluded_from_search_but_found_by_name() {
        let dir = tempfile::tempdir().unwrap();
        // A discoverable skill (skill.toml, no invocation field ⇒ discoverable).
        write_skill(dir.path(), "provider-helper", "guide adding a provider", "");
        // A user-only skill via SKILL.md frontmatter, with a description that would
        // otherwise match the same query.
        let user_dir = dir.path().join("secret-handoff");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::write(
            user_dir.join("SKILL.md"),
            "---\n\
name: secret-handoff\n\
description: guide adding a provider by hand\n\
disable-model-invocation: true\n\
---\n\
body\n",
        )
        .unwrap();

        let set = SkillSet::load(&[dir.path().to_path_buf()]).unwrap();
        // Both descriptions match the query, but search returns only the
        // discoverable skill — the user-only one is never auto-surfaced.
        let relevant = set.relevant("how do I guide adding a provider");
        assert_eq!(relevant.len(), 1);
        assert_eq!(relevant[0].manifest.name, "provider-helper");
        // The user-only skill is still reachable by its exact name (a typed load).
        assert!(set.by_name("secret-handoff").is_some());
    }
}
