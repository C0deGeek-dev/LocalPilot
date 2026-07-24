//! Skill discovery and loading (project-local and user-global).
//!
//! Skills are discovered from two kinds of scope — a per-user global baseline
//! (`~/.localpilot/skills`, `~/.agents/skills`) and the active project overlay
//! (`<project>/.localpilot/skills`, `<project>/.agents/skills`) — and resolved
//! into **one effective skill per manifest name**. A project definition shadows
//! a global one of the same name; within a scope the LocalPilot-native
//! `.localpilot/skills` outranks the cross-harness `.agents/skills`. Resolution
//! is by parsed manifest `name` and independent of filesystem enumeration order
//! (LocalHub#39).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::SkillError;
use crate::manifest::{Invocation, SkillManifest};

/// Where a skill was discovered — its precedence scope. A project scope always
/// outranks a global scope, and within a scope the LocalPilot-native
/// `.localpilot/skills` outranks the cross-harness `.agents/skills`. Carried on
/// every [`Skill`] so the effective origin is available for diagnostics and
/// user inspection (LocalHub#39).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    /// `<project>/.localpilot/skills` — highest precedence.
    ProjectLocalPilot,
    /// `<project>/.agents/skills`.
    ProjectAgents,
    /// `~/.localpilot/skills`.
    GlobalLocalPilot,
    /// `~/.agents/skills` — lowest precedence.
    GlobalAgents,
}

impl SkillScope {
    /// Precedence rank; a higher rank wins a name collision. Project scopes
    /// outrank global scopes, and the native `.localpilot/skills` outranks the
    /// cross-harness `.agents/skills` within a scope.
    fn precedence(self) -> u8 {
        match self {
            SkillScope::ProjectLocalPilot => 3,
            SkillScope::ProjectAgents => 2,
            SkillScope::GlobalLocalPilot => 1,
            SkillScope::GlobalAgents => 0,
        }
    }

    /// Whether this scope is a per-user global directory (as opposed to a
    /// project-local one). Global skills are discovered independently of
    /// workspace trust; project skills are trust-gated.
    #[must_use]
    pub fn is_global(self) -> bool {
        matches!(
            self,
            SkillScope::GlobalLocalPilot | SkillScope::GlobalAgents
        )
    }

    /// A short human-readable origin label for `skills list`/`show` and
    /// diagnostics.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            SkillScope::ProjectLocalPilot => "project (.localpilot)",
            SkillScope::ProjectAgents => "project (.agents)",
            SkillScope::GlobalLocalPilot => "global (.localpilot)",
            SkillScope::GlobalAgents => "global (.agents)",
        }
    }
}

/// A loaded skill: its manifest, its instruction text, where it lives, and the
/// scope it was discovered in.
#[derive(Debug, Clone)]
pub struct Skill {
    pub manifest: SkillManifest,
    pub instructions: String,
    pub dir: PathBuf,
    /// The discovery scope this skill's effective definition came from.
    pub scope: SkillScope,
}

impl Skill {
    /// The permission declarations to show before executing this skill.
    #[must_use]
    pub fn declared_permissions(&self) -> &[String] {
        &self.manifest.permissions
    }

    /// Whether this effective definition came from a per-user global directory.
    #[must_use]
    pub fn is_global(&self) -> bool {
        self.scope.is_global()
    }

    /// Whether `self` should supersede `other` for the same manifest name. A
    /// higher-precedence scope always wins; ties within a scope (two skill
    /// directories that declare the same manifest name) break by the
    /// lexicographically smaller directory path, so resolution is deterministic
    /// and independent of filesystem enumeration order.
    fn supersedes(&self, other: &Skill) -> bool {
        let (mine, theirs) = (self.scope.precedence(), other.scope.precedence());
        mine > theirs || (mine == theirs && self.dir < other.dir)
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
    /// Resolve the effective skill set from scoped discovery roots. Each root is
    /// a directory paired with the [`SkillScope`] it contributes; every immediate
    /// subdirectory containing a `SKILL.md` is a skill. A directory with a
    /// `skill.toml` uses the LocalPilot manifest (triggers, required tools,
    /// permission declarations); a directory with only a `SKILL.md` is read in the
    /// standard agentskills.io format (YAML frontmatter `name` + `description`),
    /// so cross-harness skill directories load as-is.
    ///
    /// Skills are resolved by parsed manifest `name` into **one effective skill
    /// per name**: the highest-precedence scope wins a collision, and the winning
    /// package replaces the shadowed one atomically — no field, trigger,
    /// permission, asset, or script is ever merged across scopes. Roots may be
    /// listed in any order; precedence comes from the [`SkillScope`], not from
    /// position, and ties within a scope break by directory path, so resolution
    /// is independent of filesystem enumeration order (LocalHub#39).
    ///
    /// A malformed skill (unparseable frontmatter, unreadable file) is skipped in
    /// every scope and recorded in [`SkillSet::skipped`] with its path, so one bad
    /// file never hides the rest (LocalHub#38).
    ///
    /// # Errors
    /// Currently never returns `Err` — per-skill failures are collected, not
    /// fatal. The `Result` is kept so a future catastrophic failure can be
    /// surfaced without a breaking signature change.
    pub fn resolve(roots: &[(PathBuf, SkillScope)]) -> Result<Self, SkillError> {
        // One effective skill per manifest name; a BTreeMap keys resolution to the
        // name (not directory enumeration) and yields a deterministic, sorted set.
        let mut effective: BTreeMap<String, Skill> = BTreeMap::new();
        let mut skipped = Vec::new();
        for (dir, scope) in roots {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let skill_dir = entry.path();
                let instructions_path = skill_dir.join("SKILL.md");
                if !instructions_path.is_file() {
                    continue;
                }
                match Self::load_one(&skill_dir, *scope) {
                    Ok(skill) => match effective.get(&skill.manifest.name) {
                        // Keep the incumbent unless the candidate outranks it.
                        Some(current) if !skill.supersedes(current) => {}
                        _ => {
                            effective.insert(skill.manifest.name.clone(), skill);
                        }
                    },
                    Err(error) => {
                        skipped.push(format!("{}: {error}", instructions_path.display()));
                    }
                }
            }
        }
        Ok(Self {
            skills: effective.into_values().collect(),
            skipped,
        })
    }

    /// Load a single skill directory into `scope`: a `skill.toml` uses the
    /// LocalPilot manifest, otherwise `SKILL.md` frontmatter is read in the
    /// standard format. The error path/diagnostic is added by the caller.
    fn load_one(skill_dir: &Path, scope: SkillScope) -> Result<Skill, SkillError> {
        let manifest_path = skill_dir.join("skill.toml");
        let instructions_path = skill_dir.join("SKILL.md");
        if manifest_path.is_file() {
            Ok(Skill {
                manifest: SkillManifest::parse(&read(&manifest_path)?)?,
                instructions: read(&instructions_path)?,
                dir: skill_dir.to_path_buf(),
                scope,
            })
        } else {
            let (manifest, body) = SkillManifest::parse_skill_md(&read(&instructions_path)?)?;
            Ok(Skill {
                manifest,
                instructions: body,
                dir: skill_dir.to_path_buf(),
                scope,
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

/// The project-local skill directories LocalPilot reads: its own directory
/// first, then the cross-harness standard location. Project-local skills load
/// only behind the workspace trust gate (the caller enforces it).
#[must_use]
pub fn standard_skill_dirs(project_root: &Path) -> Vec<PathBuf> {
    vec![
        project_root.join(".localpilot").join("skills"),
        project_root.join(".agents").join("skills"),
    ]
}

/// The per-user global skill directories, resolved from `home`: the LocalPilot-
/// native directory and the cross-harness standard location. These form the
/// baseline every project inherits, independently of workspace trust.
#[must_use]
pub fn global_skill_dirs(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".localpilot").join("skills"),
        home.join(".agents").join("skills"),
    ]
}

/// The scoped discovery roots for a workspace: the per-user global baseline
/// (always, when a home directory resolves) plus the project overlay (only when
/// the workspace is `trusted`). Order is irrelevant to resolution — each root
/// carries its own [`SkillScope`] — but globals are listed first to read as a
/// baseline. A missing home directory cleanly omits the global layer and leaves
/// project-only behavior unchanged.
#[must_use]
pub fn discovery_roots(
    project_root: &Path,
    home: Option<&Path>,
    trusted: bool,
) -> Vec<(PathBuf, SkillScope)> {
    let mut roots = Vec::new();
    if let Some(home) = home {
        roots.push((
            home.join(".localpilot").join("skills"),
            SkillScope::GlobalLocalPilot,
        ));
        roots.push((
            home.join(".agents").join("skills"),
            SkillScope::GlobalAgents,
        ));
    }
    // The project overlay is gated on workspace trust: an untrusted project
    // cannot contribute skills, and so cannot shadow a global skill.
    if trusted {
        roots.push((
            project_root.join(".localpilot").join("skills"),
            SkillScope::ProjectLocalPilot,
        ));
        roots.push((
            project_root.join(".agents").join("skills"),
            SkillScope::ProjectAgents,
        ));
    }
    roots
}

/// The per-user home directory, resolved cross-platform, consistent with the
/// global instruction directory under `~/.localpilot/`. `None` when no home is
/// set, in which case the global skill layer is omitted cleanly.
#[cfg(windows)]
#[must_use]
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

#[cfg(not(windows))]
#[must_use]
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
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

    /// Write a standard `SKILL.md`-only skill (no `skill.toml`) under `root`.
    fn write_skill_md(root: &Path, name: &str, description: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    /// Resolve a single directory as a project `.localpilot/skills` scope — the
    /// scope-agnostic shorthand the single-directory tests use.
    fn resolve_dir(dir: &Path) -> SkillSet {
        SkillSet::resolve(&[(dir.to_path_buf(), SkillScope::ProjectLocalPilot)]).unwrap()
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
        let set = resolve_dir(dir.path());

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

        let set = resolve_dir(dir.path());
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

        let set = resolve_dir(dir.path());
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

        let set = resolve_dir(dir.path());
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
        let set = resolve_dir(dir.path());

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

        let set = resolve_dir(dir.path());
        // Both descriptions match the query, but search returns only the
        // discoverable skill — the user-only one is never auto-surfaced.
        let relevant = set.relevant("how do I guide adding a provider");
        assert_eq!(relevant.len(), 1);
        assert_eq!(relevant[0].manifest.name, "provider-helper");
        // The user-only skill is still reachable by its exact name (a typed load).
        assert!(set.by_name("secret-handoff").is_some());
    }

    // --- LocalHub#39: user-global baseline and project overlay precedence. ---

    #[test]
    fn a_project_skill_shadows_a_global_skill_of_the_same_name_atomically() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        // Same manifest name in both scopes, distinct bodies.
        write_skill_md(
            &home.path().join(".agents").join("skills"),
            "modern-web-design",
            "the GLOBAL definition",
            "GLOBAL BODY",
        );
        write_skill_md(
            &project.path().join(".agents").join("skills"),
            "modern-web-design",
            "the PROJECT definition",
            "PROJECT BODY",
        );
        let roots = discovery_roots(project.path(), Some(home.path()), true);
        let set = SkillSet::resolve(&roots).unwrap();

        // One effective skill per name, and it is the project one.
        assert_eq!(set.names(), vec!["modern-web-design"]);
        let skill = set.by_name("modern-web-design").unwrap();
        assert_eq!(skill.scope, SkillScope::ProjectAgents);
        assert!(!skill.is_global());
        // The whole package is replaced — no shadowed body/description leaks.
        assert!(skill.instructions.contains("PROJECT BODY"));
        assert!(!skill.instructions.contains("GLOBAL BODY"));
        assert_eq!(skill.manifest.description, "the PROJECT definition");
    }

    #[test]
    fn a_global_only_skill_is_reachable_from_an_unrelated_project() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap(); // no project skills at all
        write_skill_md(
            &home.path().join(".agents").join("skills"),
            "threejs-webgl",
            "global three.js helper",
            "body",
        );
        let set =
            SkillSet::resolve(&discovery_roots(project.path(), Some(home.path()), true)).unwrap();
        let skill = set
            .by_name("threejs-webgl")
            .expect("global skill is reachable");
        assert!(skill.is_global());
        assert_eq!(skill.scope, SkillScope::GlobalAgents);
    }

    #[test]
    fn removing_a_project_override_reveals_the_global_skill_again() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_skill_md(
            &home.path().join(".localpilot").join("skills"),
            "modern-web-design",
            "the global definition",
            "GLOBAL BODY",
        );
        let project_skill = project.path().join(".localpilot").join("skills");
        write_skill_md(
            &project_skill,
            "modern-web-design",
            "the project one",
            "PROJECT BODY",
        );

        let overridden =
            SkillSet::resolve(&discovery_roots(project.path(), Some(home.path()), true)).unwrap();
        assert!(overridden
            .by_name("modern-web-design")
            .unwrap()
            .instructions
            .contains("PROJECT BODY"));

        // Delete the project override; the unchanged global becomes effective.
        std::fs::remove_dir_all(project_skill.join("modern-web-design")).unwrap();
        let revealed =
            SkillSet::resolve(&discovery_roots(project.path(), Some(home.path()), true)).unwrap();
        let skill = revealed.by_name("modern-web-design").unwrap();
        assert!(skill.is_global());
        assert!(skill.instructions.contains("GLOBAL BODY"));
    }

    #[test]
    fn localpilot_scope_wins_over_agents_scope_within_the_same_tier() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        // Project tier: .localpilot must beat .agents.
        write_skill_md(
            &project.path().join(".agents").join("skills"),
            "dup",
            "agents",
            "AGENTS BODY",
        );
        write_skill_md(
            &project.path().join(".localpilot").join("skills"),
            "dup",
            "localpilot",
            "LOCALPILOT BODY",
        );
        let set =
            SkillSet::resolve(&discovery_roots(project.path(), Some(home.path()), true)).unwrap();
        let skill = set.by_name("dup").unwrap();
        assert_eq!(skill.scope, SkillScope::ProjectLocalPilot);
        assert!(skill.instructions.contains("LOCALPILOT BODY"));

        // Global tier: same rule, .localpilot beats .agents.
        let home2 = tempfile::tempdir().unwrap();
        write_skill_md(&home2.path().join(".agents").join("skills"), "g", "a", "GA");
        write_skill_md(
            &home2.path().join(".localpilot").join("skills"),
            "g",
            "l",
            "GL",
        );
        let empty = tempfile::tempdir().unwrap();
        let gset =
            SkillSet::resolve(&discovery_roots(empty.path(), Some(home2.path()), true)).unwrap();
        assert_eq!(
            gset.by_name("g").unwrap().scope,
            SkillScope::GlobalLocalPilot
        );
    }

    #[test]
    fn an_untrusted_project_cannot_shadow_a_global_skill() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_skill_md(
            &home.path().join(".localpilot").join("skills"),
            "shared",
            "the global one",
            "GLOBAL",
        );
        write_skill_md(
            &project.path().join(".localpilot").join("skills"),
            "shared",
            "the project one",
            "PROJECT",
        );
        // Untrusted: the project overlay is omitted; the global remains effective.
        let untrusted =
            SkillSet::resolve(&discovery_roots(project.path(), Some(home.path()), false)).unwrap();
        let skill = untrusted.by_name("shared").unwrap();
        assert!(
            skill.is_global(),
            "untrusted project must not shadow the global"
        );
        assert!(skill.instructions.contains("GLOBAL"));

        // Trusted: the project override becomes effective.
        let trusted =
            SkillSet::resolve(&discovery_roots(project.path(), Some(home.path()), true)).unwrap();
        assert!(trusted
            .by_name("shared")
            .unwrap()
            .instructions
            .contains("PROJECT"));
    }

    #[test]
    fn resolution_is_stable_regardless_of_root_and_directory_order() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_skill_md(
            &home.path().join(".agents").join("skills"),
            "x",
            "global",
            "GLOBAL",
        );
        write_skill_md(
            &project.path().join(".localpilot").join("skills"),
            "x",
            "project",
            "PROJECT",
        );

        // Resolve with the roots reversed: precedence comes from the scope, not
        // list position, so the project skill still wins.
        let mut roots = discovery_roots(project.path(), Some(home.path()), true);
        roots.reverse();
        let set = SkillSet::resolve(&roots).unwrap();
        assert_eq!(
            set.by_name("x").unwrap().scope,
            SkillScope::ProjectLocalPilot
        );
    }

    #[test]
    fn two_dirs_in_one_scope_with_the_same_name_resolve_deterministically() {
        // Two skill directories under the same scope declaring the same manifest
        // name: the tie must break by directory path, independent of enumeration.
        let dir = tempfile::tempdir().unwrap();
        let scope_root = dir.path();
        write_skill_md(scope_root, "aaa", "first dir", "FROM AAA");
        // A second directory whose SKILL.md declares the *same* manifest name.
        let bbb = scope_root.join("bbb");
        std::fs::create_dir_all(&bbb).unwrap();
        std::fs::write(
            bbb.join("SKILL.md"),
            "---\nname: dup-name\ndescription: second dir\n---\nFROM BBB\n",
        )
        .unwrap();
        // Rename aaa's manifest name to also be `dup-name` so both collide.
        std::fs::write(
            scope_root.join("aaa").join("SKILL.md"),
            "---\nname: dup-name\ndescription: first dir\n---\nFROM AAA\n",
        )
        .unwrap();

        let a =
            SkillSet::resolve(&[(scope_root.to_path_buf(), SkillScope::ProjectAgents)]).unwrap();
        let b =
            SkillSet::resolve(&[(scope_root.to_path_buf(), SkillScope::ProjectAgents)]).unwrap();
        // Same winner every time (lexicographically smaller dir `aaa`).
        assert_eq!(
            a.by_name("dup-name").unwrap().dir,
            b.by_name("dup-name").unwrap().dir
        );
        assert!(a
            .by_name("dup-name")
            .unwrap()
            .instructions
            .contains("FROM AAA"));
    }

    #[test]
    fn no_home_directory_yields_project_only_discovery() {
        let project = tempfile::tempdir().unwrap();
        write_skill_md(
            &project.path().join(".localpilot").join("skills"),
            "only-project",
            "project skill",
            "body",
        );
        let roots = discovery_roots(project.path(), None, true);
        // No global roots contributed.
        assert!(roots.iter().all(|(_, scope)| !scope.is_global()));
        let set = SkillSet::resolve(&roots).unwrap();
        assert_eq!(set.names(), vec!["only-project"]);
    }
}
