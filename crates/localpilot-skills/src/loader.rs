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

use localpilot_core::word_overlap;

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

    /// Discoverable skills matching `query`, each paired with its relevance
    /// score, ranked highest-first with a stable name tie-break. This is the one
    /// ranked entry point: the inclusion gate ([`SkillSet::relevant`]) and the
    /// `skill_search` ranking both derive from it, so a returned skill always has
    /// a nonzero score and the gate and the ranking can never encode independent
    /// match rules. Matching is via [`match_score`] over the skill name,
    /// description, and command triggers — a query and each field are normalized
    /// with one shared signal (Unicode-lowercased alphanumeric tokens plus a
    /// compact all-alphanumeric form, so `three.js` and `threejs` match). Only
    /// **discoverable** skills are candidates — user-only skills are reached
    /// solely by [`SkillSet::by_name`] (a typed name), never by search — so a
    /// model can never auto-surface a skill the author marked user-only.
    #[must_use]
    pub fn ranked(&self, query: &str) -> Vec<(&Skill, u32)> {
        let query = normalize_query(query);
        let mut hits: Vec<(&Skill, u32)> = self
            .skills
            .iter()
            .filter(|skill| skill.manifest.invocation == Invocation::Discoverable)
            .filter_map(|skill| {
                let score = match_score(skill, &query);
                (score > 0).then_some((skill, score))
            })
            .collect();
        // Highest score first; ties broken by name for a stable order.
        hits.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.0.manifest.name.cmp(&b.0.manifest.name))
        });
        hits
    }

    /// Skills relevant to `query`, for on-demand discovery (the `skill_search`
    /// tool). The inclusion view of [`SkillSet::ranked`] — same match signal,
    /// scores dropped. Only **discoverable** skills are candidates.
    #[must_use]
    pub fn relevant(&self, query: &str) -> Vec<&Skill> {
        self.ranked(query)
            .into_iter()
            .map(|(skill, _)| skill)
            .collect()
    }

    /// The number of **discoverable** skills in the set — never counting
    /// user-only skills. The narrow count `skill_search` reports when a query has
    /// no strong match, so the model learns that installed skills exist without a
    /// user-only name or description ever being surfaced.
    #[must_use]
    pub fn discoverable_count(&self) -> usize {
        self.skills
            .iter()
            .filter(|skill| skill.manifest.invocation == Invocation::Discoverable)
            .count()
    }
}

/// The minimum length, in characters, of a query or field token considered for
/// matching. Two-character runs are dropped to keep token matches signal-bearing;
/// the compact whole-string form still catches short product names.
const MIN_TOKEN_CHARS: usize = 3;

/// A query normalized once for skill matching, so the inclusion gate and the
/// ranking share one signal and cannot drift. `tokens` are the Unicode-lowercased
/// alphanumeric runs of length ≥ [`MIN_TOKEN_CHARS`]; `compact` is the whole query
/// reduced to lowercased alphanumerics, so a punctuation-separated product name
/// (`three.js`) and its natural spelling (`threejs`) share a compact form.
struct NormalizedQuery {
    tokens: Vec<String>,
    compact: String,
}

/// Normalize a raw query once into its shared match signal. See
/// [`NormalizedQuery`].
fn normalize_query(query: &str) -> NormalizedQuery {
    NormalizedQuery {
        tokens: tokens_of(query),
        compact: compact_of(query),
    }
}

/// The Unicode-lowercased alphanumeric tokens of `text`, each of length ≥
/// [`MIN_TOKEN_CHARS`].
fn tokens_of(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= MIN_TOKEN_CHARS)
        .map(str::to_lowercase)
        .collect()
}

/// `text` reduced to lowercased alphanumerics only — punctuation, spaces, and
/// separators dropped — so `three.js` becomes `threejs`.
fn compact_of(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The one relevance signal shared by the inclusion gate and the ranking.
/// Returns `0` when `skill` does not match `query`, and a positive score when it
/// does — so every admitted match scores at least 1 and `relevant` (score > 0)
/// and `ranked` can never diverge. Matches the skill name, its description, and
/// its command triggers. The query's compact form is compared against the
/// compact name and the compact description **separately** — never a
/// name+description concatenation — so a field boundary cannot fabricate a match.
/// `word_overlap` (the shared relevance core) is fed pre-normalized text, so the
/// core stays untouched.
fn match_score(skill: &Skill, query: &NormalizedQuery) -> u32 {
    let token_refs: Vec<&str> = query.tokens.iter().map(String::as_str).collect();

    let name_lower = skill.manifest.name.to_lowercase();
    let desc_lower = skill.manifest.description.to_lowercase();
    let name_word_hits = word_overlap(&name_lower, &token_refs);
    let desc_word_hits = word_overlap(&desc_lower, &token_refs);

    let name_compact = compact_of(&skill.manifest.name);
    let desc_compact = compact_of(&skill.manifest.description);
    let name_compact_hit = !query.compact.is_empty() && name_compact.contains(&query.compact);
    let desc_compact_hit = !query.compact.is_empty() && desc_compact.contains(&query.compact);

    let trigger_hit = skill.manifest.triggers.commands.iter().any(|c| {
        let c_compact = compact_of(c);
        !c_compact.is_empty() && query.compact.contains(&c_compact)
    });

    // A name match weighs more than a description match; every positive signal
    // contributes at least 1, so any admitted match has a nonzero score.
    name_word_hits * 2
        + desc_word_hits
        + u32::from(name_compact_hit) * 2
        + u32::from(desc_compact_hit)
        + u32::from(trigger_hit) * 2
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

/// The user-global discovery roots only (no project overlay), for a read scoped
/// to the global baseline (`skills list -g` / `show -g`). A missing home yields an
/// empty root set, so a global-only view is simply empty rather than an error.
#[must_use]
pub fn global_only_roots(home: Option<&Path>) -> Vec<(PathBuf, SkillScope)> {
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

/// Read just the manifest of a skill package directory — the same rule the
/// loader applies: a `skill.toml` if present, otherwise the `SKILL.md`
/// frontmatter. Shared by discovery-time loading and catalog validation so both
/// agree on what a valid package is (LocalHub#40).
///
/// # Errors
/// Returns [`SkillError::Io`] if a file cannot be read or [`SkillError::InvalidManifest`]
/// if the manifest/frontmatter is malformed.
pub(crate) fn read_manifest(skill_dir: &Path) -> Result<SkillManifest, SkillError> {
    let manifest_path = skill_dir.join("skill.toml");
    if manifest_path.is_file() {
        SkillManifest::parse(&read(&manifest_path)?)
    } else {
        Ok(SkillManifest::parse_skill_md(&read(&skill_dir.join("SKILL.md"))?)?.0)
    }
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
        // The narrow discoverable count never includes the user-only skill.
        assert_eq!(set.discoverable_count(), 1);
    }

    #[test]
    fn compact_matching_finds_a_punctuated_product_name_by_its_natural_spelling() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "three-fiber", "Three.js scene helpers", "");
        write_skill(dir.path(), "gardening", "water the plants", "");
        let set = resolve_dir(dir.path());

        // `threejs` (no dot) matches `Three.js` because both compact to `threejs`.
        let hits = set.relevant("threejs");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].manifest.name, "three-fiber");
        // The plain token spelling matches too, and the unrelated skill never does.
        assert_eq!(set.relevant("three").len(), 1);
    }

    #[test]
    fn compact_matching_finds_a_hyphenated_name_by_its_run_together_spelling() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "react-three-fiber", "renderer bindings", "");
        let set = resolve_dir(dir.path());

        // `reactthreefiber` matches the hyphenated name via its compact form.
        let hits = set.relevant("reactthreefiber");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].manifest.name, "react-three-fiber");
    }

    #[test]
    fn compact_name_and_description_are_matched_separately_never_concatenated() {
        let dir = tempfile::tempdir().unwrap();
        // `reactthreefiber` exists only ACROSS the name/description boundary
        // (`react-three` + `fiber ...`); comparing the compact fields separately
        // must not fabricate a match by concatenating them.
        write_skill(dir.path(), "react-three", "fiber helpers", "");
        let set = resolve_dir(dir.path());

        assert!(set.relevant("reactthreefiber").is_empty());
    }

    #[test]
    fn an_unrelated_query_has_no_strong_match_rather_than_a_fabricated_one() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "three-fiber", "Three.js scene helpers", "");
        let set = resolve_dir(dir.path());

        // A non-English/unrelated query truthfully matches nothing — lexical
        // search does not invent a semantic hit.
        assert!(set.relevant("welke skills heb je beschikbaar").is_empty());
    }

    #[test]
    fn the_gate_and_the_ranking_share_one_signal_and_every_match_scores_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "three-fiber", "Three.js scene helpers", "");
        write_skill(dir.path(), "three-loader", "load three meshes", "");
        write_skill(dir.path(), "gardening", "water the plants", "");
        let set = resolve_dir(dir.path());

        let ranked = set.ranked("three");
        // Every admitted match has a nonzero score.
        assert!(ranked.iter().all(|(_, score)| *score > 0));
        // The inclusion gate is exactly the ranked set (same names, same order),
        // so gate and ranking cannot encode independent rules.
        let gate: Vec<&str> = set
            .relevant("three")
            .iter()
            .map(|s| s.manifest.name.as_str())
            .collect();
        let ranked_names: Vec<&str> = ranked
            .iter()
            .map(|(s, _)| s.manifest.name.as_str())
            .collect();
        assert_eq!(gate, ranked_names);
    }

    #[test]
    fn a_name_match_outranks_a_description_only_match() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "json", "unrelated helper text", "");
        write_skill(dir.path(), "other", "json formatting helper", "");
        let set = resolve_dir(dir.path());

        let ranked = set.ranked("json");
        assert_eq!(ranked.len(), 2);
        // The skill whose NAME is the query outranks the description-only match.
        assert_eq!(ranked[0].0.manifest.name, "json");
        assert!(ranked[0].1 > ranked[1].1);
    }

    #[test]
    fn a_command_trigger_matches_by_its_normalized_spelling() {
        let dir = tempfile::tempdir().unwrap();
        // A skill whose name and description are deliberately unrelated to the
        // query — only its punctuated command trigger connects.
        let sdir = dir.path().join("releaser");
        std::fs::create_dir_all(&sdir).unwrap();
        std::fs::write(
            sdir.join("skill.toml"),
            "name = \"releaser\"\ndescription = \"unrelated helper text\"\nversion = \"0.1.0\"\n\
             [triggers]\ncommands = [\"deploy-prod\"]\n",
        )
        .unwrap();
        std::fs::write(sdir.join("SKILL.md"), "# releaser\n\nBody.\n").unwrap();
        write_skill(dir.path(), "gardening", "water the plants", "");
        let set = resolve_dir(dir.path());

        // The natural run-together spelling reaches the skill through its trigger
        // (compact `deployprod`), with a nonzero ranked score, though neither the
        // name nor the description matches.
        let ranked = set.ranked("deployprod");
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0.manifest.name, "releaser");
        assert!(ranked[0].1 > 0);
        // An unrelated query reaches neither skill (no trigger, no name/desc hit).
        assert!(set.relevant("xyzzy quux").is_empty());
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
