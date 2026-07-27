//! Discovery and precedence for subagent definitions.
//!
//! Definitions come from a per-user global baseline (`~/.localpilot/agents`,
//! `~/.agents/agents`) and the active project overlay
//! (`<project>/.localpilot/agents`, `<project>/.agents/agents`), and resolve to
//! **one effective agent per name**. A project definition shadows a global one
//! of the same name; within a scope the LocalPilot-native directory outranks the
//! cross-harness one. This is deliberately the same rule skills already use —
//! users should not have to learn a second precedence order.
//!
//! Resolution is by parsed `name`, not by filename, and never depends on
//! filesystem enumeration order.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::definition::AgentDefinition;
use crate::error::AgentError;

/// Where a definition was discovered — its precedence scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentScope {
    /// `<project>/.localpilot/agents` — highest precedence.
    ProjectLocalPilot,
    /// `<project>/.agents/agents`.
    ProjectAgents,
    /// `~/.localpilot/agents`.
    GlobalLocalPilot,
    /// `~/.agents/agents` — lowest precedence.
    GlobalAgents,
}

impl AgentScope {
    fn precedence(self) -> u8 {
        match self {
            Self::ProjectLocalPilot => 3,
            Self::ProjectAgents => 2,
            Self::GlobalLocalPilot => 1,
            Self::GlobalAgents => 0,
        }
    }

    /// Whether this is a per-user global directory rather than a project one.
    #[must_use]
    pub fn is_global(self) -> bool {
        matches!(self, Self::GlobalLocalPilot | Self::GlobalAgents)
    }

    /// A short origin label for listings and diagnostics.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ProjectLocalPilot => "project (.localpilot)",
            Self::ProjectAgents => "project (.agents)",
            Self::GlobalLocalPilot => "global (.localpilot)",
            Self::GlobalAgents => "global (.agents)",
        }
    }
}

/// The directory name definitions live in, inside each scope root.
const AGENTS_DIR: &str = "agents";
/// The file suffix a definition must carry.
const SUFFIX: &str = ".agent.yaml";

/// One discovered definition with its origin.
#[derive(Clone, Debug)]
pub struct DiscoveredAgent {
    pub definition: AgentDefinition,
    pub scope: AgentScope,
    pub path: PathBuf,
}

/// A definition that could not be loaded, kept so a broken file is explained
/// rather than silently missing.
#[derive(Clone, Debug)]
pub struct AgentLoadError {
    pub path: PathBuf,
    pub reason: String,
}

/// The resolved set: one effective agent per name, plus every load failure and
/// every shadowed definition, so `agents list`/`doctor` can explain both.
#[derive(Clone, Debug, Default)]
pub struct AgentSet {
    effective: BTreeMap<String, DiscoveredAgent>,
    shadowed: Vec<DiscoveredAgent>,
    errors: Vec<AgentLoadError>,
}

impl AgentSet {
    /// Discover and resolve definitions from `roots`, in any order.
    ///
    /// A missing directory is not an error. A file that fails to parse is
    /// recorded in [`AgentSet::errors`] and skipped — one broken definition must
    /// not hide the rest.
    #[must_use]
    pub fn resolve(roots: &[(PathBuf, AgentScope)]) -> Self {
        let mut set = Self::default();
        for (root, scope) in roots {
            let dir = root.join(AGENTS_DIR);
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue; // absent scope: normal, not an error
            };
            let mut paths: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.ends_with(SUFFIX))
                })
                .collect();
            // Sort so the recorded order is stable across platforms; precedence
            // still comes from the scope, never from enumeration order.
            paths.sort();
            for path in paths {
                match std::fs::read_to_string(&path)
                    .map_err(|e| AgentError::Io(e.to_string()))
                    .and_then(|text| AgentDefinition::from_yaml(&text))
                {
                    Ok(definition) => set.insert(DiscoveredAgent {
                        definition,
                        scope: *scope,
                        path,
                    }),
                    Err(error) => set.errors.push(AgentLoadError {
                        path,
                        reason: error.to_string(),
                    }),
                }
            }
        }
        set
    }

    fn insert(&mut self, candidate: DiscoveredAgent) {
        match self.effective.get(&candidate.definition.name) {
            Some(existing) if existing.scope.precedence() >= candidate.scope.precedence() => {
                self.shadowed.push(candidate);
            }
            Some(_) => {
                let displaced = self
                    .effective
                    .insert(candidate.definition.name.clone(), candidate);
                if let Some(displaced) = displaced {
                    self.shadowed.push(displaced);
                }
            }
            None => {
                self.effective
                    .insert(candidate.definition.name.clone(), candidate);
            }
        }
    }

    /// Every effective agent, ordered by name.
    #[must_use]
    pub fn agents(&self) -> Vec<&DiscoveredAgent> {
        self.effective.values().collect()
    }

    /// The effective agent for `name`, if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&DiscoveredAgent> {
        self.effective.get(name)
    }

    /// Definitions that lost a name collision — still discoverable so a user can
    /// see *why* their file is not the one running.
    #[must_use]
    pub fn shadowed(&self) -> &[DiscoveredAgent] {
        &self.shadowed
    }

    /// Files that could not be loaded, with the reason.
    #[must_use]
    pub fn errors(&self) -> &[AgentLoadError] {
        &self.errors
    }

    /// The standard scope roots for a project: the project overlay first, then
    /// the per-user global baseline. `home` is `None` when the platform does not
    /// report one — the project scopes still resolve.
    #[must_use]
    pub fn standard_roots(project: &Path, home: Option<&Path>) -> Vec<(PathBuf, AgentScope)> {
        let mut roots = vec![
            (project.join(".localpilot"), AgentScope::ProjectLocalPilot),
            (project.join(".agents"), AgentScope::ProjectAgents),
        ];
        if let Some(home) = home {
            roots.push((home.join(".localpilot"), AgentScope::GlobalLocalPilot));
            roots.push((home.join(".agents"), AgentScope::GlobalAgents));
        }
        roots
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).expect("dir");
        fs::write(dir.join(format!("{name}{SUFFIX}")), body).expect("write");
    }

    fn definition(name: &str, description: &str) -> String {
        format!(
            "format_version: 1\nname: {name}\ndescription: {description}\nprompt: Do the thing.\n"
        )
    }

    #[test]
    fn a_project_definition_shadows_a_global_one_and_the_loser_stays_visible() {
        let tmp = tempfile::tempdir().expect("tmp");
        let project = tmp.path().join("proj");
        let home = tmp.path().join("home");
        write(
            &project.join(".localpilot").join(AGENTS_DIR),
            "reviewer",
            &definition("reviewer", "project version"),
        );
        write(
            &home.join(".localpilot").join(AGENTS_DIR),
            "reviewer",
            &definition("reviewer", "global version"),
        );

        let set = AgentSet::resolve(&AgentSet::standard_roots(&project, Some(&home)));
        let effective = set.get("reviewer").expect("resolved");
        assert_eq!(effective.scope, AgentScope::ProjectLocalPilot);
        assert_eq!(effective.definition.description, "project version");
        assert_eq!(set.shadowed().len(), 1, "the global one stays discoverable");
        assert_eq!(set.shadowed()[0].scope, AgentScope::GlobalLocalPilot);
    }

    #[test]
    fn the_native_directory_outranks_the_cross_harness_one_within_a_scope() {
        let tmp = tempfile::tempdir().expect("tmp");
        let project = tmp.path().join("proj");
        write(
            &project.join(".agents").join(AGENTS_DIR),
            "a",
            &definition("reviewer", "cross-harness"),
        );
        write(
            &project.join(".localpilot").join(AGENTS_DIR),
            "b",
            &definition("reviewer", "native"),
        );
        let set = AgentSet::resolve(&AgentSet::standard_roots(&project, None));
        assert_eq!(
            set.get("reviewer")
                .expect("resolved")
                .definition
                .description,
            "native"
        );
    }

    #[test]
    fn precedence_does_not_depend_on_root_order() {
        let tmp = tempfile::tempdir().expect("tmp");
        let project = tmp.path().join("proj");
        let home = tmp.path().join("home");
        write(
            &project.join(".localpilot").join(AGENTS_DIR),
            "x",
            &definition("reviewer", "project version"),
        );
        write(
            &home.join(".localpilot").join(AGENTS_DIR),
            "x",
            &definition("reviewer", "global version"),
        );
        let mut roots = AgentSet::standard_roots(&project, Some(&home));
        roots.reverse();
        let set = AgentSet::resolve(&roots);
        assert_eq!(
            set.get("reviewer")
                .expect("resolved")
                .definition
                .description,
            "project version",
            "scope precedence must win regardless of the order roots are scanned"
        );
    }

    #[test]
    fn a_broken_file_is_reported_and_does_not_hide_the_others() {
        let tmp = tempfile::tempdir().expect("tmp");
        let project = tmp.path().join("proj");
        let dir = project.join(".localpilot").join(AGENTS_DIR);
        write(&dir, "good", &definition("good", "fine"));
        write(&dir, "bad", "format_version: 1\nname: bad\n");
        let set = AgentSet::resolve(&AgentSet::standard_roots(&project, None));
        assert!(set.get("good").is_some(), "the valid one still loads");
        assert_eq!(set.errors().len(), 1);
        assert!(set.errors()[0].path.to_string_lossy().contains("bad"));
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let set = AgentSet::resolve(&AgentSet::standard_roots(&tmp.path().join("nope"), None));
        assert!(set.agents().is_empty());
        assert!(set.errors().is_empty());
    }

    #[test]
    fn only_the_expected_suffix_is_loaded() {
        let tmp = tempfile::tempdir().expect("tmp");
        let project = tmp.path().join("proj");
        let dir = project.join(".localpilot").join(AGENTS_DIR);
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join("notes.yaml"), definition("notes", "not an agent")).expect("write");
        fs::write(dir.join("README.md"), "# hi").expect("write");
        let set = AgentSet::resolve(&AgentSet::standard_roots(&project, None));
        assert!(
            set.agents().is_empty(),
            "a bare .yaml is not a definition: {:?}",
            set.agents()
                .iter()
                .map(|a| &a.definition.name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn agents_are_listed_by_name_in_a_stable_order() {
        let tmp = tempfile::tempdir().expect("tmp");
        let project = tmp.path().join("proj");
        let dir = project.join(".localpilot").join(AGENTS_DIR);
        for name in ["zeta", "alpha", "mid"] {
            write(&dir, name, &definition(name, "x"));
        }
        let set = AgentSet::resolve(&AgentSet::standard_roots(&project, None));
        let names: Vec<&str> = set
            .agents()
            .iter()
            .map(|a| a.definition.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "mid", "zeta"]);
    }
}
