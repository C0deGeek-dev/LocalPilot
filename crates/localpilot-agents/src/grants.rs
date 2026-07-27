//! Resolving a definition's tool list into a child's actual tool set.
//!
//! The rule is one line and the whole security story rests on it:
//!
//! > effective tools = **parent's tools ∩ definition's list**
//!
//! Intersection, never union. A definition names an *upper bound* on what its
//! agent may use; it can only ever narrow what the parent session already holds.
//! A definition naming a tool the parent lacks does not gain it — the entry is
//! dropped and reported.
//!
//! Two different failures are deliberately distinguished:
//!
//! - a name that is not a **registered tool at all** is an authoring mistake and
//!   fails at load, naming the entry;
//! - a name that is registered but absent from *this* parent's set is normal
//!   narrowing — the caller is told, and the agent runs without it.
//!
//! Conflating them would make a typo look like a permission decision.

use std::collections::BTreeSet;

use crate::definition::AgentDefinition;
use crate::error::AgentError;

/// The outcome of resolving a definition's tool list against a parent session.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Grants {
    /// The child's tools: always a subset of the parent's, sorted and deduped.
    pub tools: Vec<String>,
    /// Entries that are registered tools but were not in the parent's set, so
    /// the child does not get them. Reported, never silently dropped.
    pub narrowed: Vec<String>,
}

impl Grants {
    /// Whether the child ended up with nothing to call.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Resolve `definition`'s tool list against the host's full registry and the
/// parent session's own tools.
///
/// `registered` is every tool name the host knows; `parent` is the subset the
/// parent session actually holds.
///
/// # Errors
/// Returns [`AgentError::UnknownTool`] when an entry names something that is not
/// a registered tool at all.
pub fn resolve(
    definition: &AgentDefinition,
    registered: &[&str],
    parent: &[&str],
) -> Result<Grants, AgentError> {
    let registered: BTreeSet<&str> = registered.iter().copied().collect();
    let parent_set: BTreeSet<&str> = parent.iter().copied().collect();

    // A definition asking for everything gets everything *the parent has* —
    // which is the intersection with a universal upper bound, not an escape.
    if definition.wants_all_tools() {
        return Ok(Grants {
            tools: parent_set.iter().map(|s| (*s).to_string()).collect(),
            narrowed: Vec::new(),
        });
    }

    let mut tools = BTreeSet::new();
    let mut narrowed = BTreeSet::new();
    for entry in &definition.tools {
        if entry == "*" {
            continue; // handled above
        }
        if !registered.contains(entry.as_str()) {
            return Err(AgentError::UnknownTool(format!(
                "{entry:?} is not a registered tool"
            )));
        }
        if parent_set.contains(entry.as_str()) {
            tools.insert(entry.clone());
        } else {
            narrowed.insert(entry.clone());
        }
    }

    Ok(Grants {
        tools: tools.into_iter().collect(),
        narrowed: narrowed.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(tools: &[&str]) -> AgentDefinition {
        let list = tools
            .iter()
            .map(|t| format!("  - '{t}'\n"))
            .collect::<String>();
        let text = format!(
            "format_version: 1\nname: probe\ndescription: d\nprompt: p\ntools:\n{}",
            if list.is_empty() {
                "  []\n".to_string()
            } else {
                list
            }
        );
        AgentDefinition::from_yaml(&text).expect("fixture parses")
    }

    #[test]
    fn the_child_gets_the_intersection() {
        let d = definition(&["read_file", "search_text"]);
        let grants = resolve(
            &d,
            &["read_file", "search_text", "write_file"],
            &["read_file", "write_file"],
        )
        .expect("resolves");
        assert_eq!(grants.tools, ["read_file"]);
        assert_eq!(
            grants.narrowed,
            ["search_text"],
            "a registered tool the parent lacks is reported, not granted"
        );
    }

    #[test]
    fn a_definition_can_never_widen_the_parent() {
        // Exhaustive over a small universe: for every parent subset and every
        // definition subset, the child's tools must be a subset of the parent's.
        let universe = ["a", "b", "c", "d"];
        for parent_mask in 0u8..16 {
            for def_mask in 0u8..16 {
                let parent: Vec<&str> = universe
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| parent_mask & (1 << i) != 0)
                    .map(|(_, t)| *t)
                    .collect();
                let wanted: Vec<&str> = universe
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| def_mask & (1 << i) != 0)
                    .map(|(_, t)| *t)
                    .collect();
                let d = definition(&wanted);
                let grants = resolve(&d, &universe, &parent).expect("all names are registered");
                for tool in &grants.tools {
                    assert!(
                        parent.contains(&tool.as_str()),
                        "child got {tool:?} which the parent (mask {parent_mask:#06b}) does not hold"
                    );
                }
                assert!(
                    grants.tools.len() <= parent.len(),
                    "child set can never be larger than the parent's"
                );
            }
        }
    }

    #[test]
    fn the_wildcard_is_the_parents_set_not_the_registry() {
        let d = definition(&["*"]);
        let grants = resolve(&d, &["a", "b", "c"], &["a", "b"]).expect("resolves");
        assert_eq!(
            grants.tools,
            ["a", "b"],
            "`*` means everything the parent has, never everything registered"
        );
    }

    #[test]
    fn an_unregistered_tool_is_a_load_error_naming_the_entry() {
        let d = definition(&["read_file", "teleport"]);
        let message = resolve(&d, &["read_file"], &["read_file"])
            .expect_err("teleport is not a tool")
            .to_string();
        assert!(message.contains("teleport"), "{message}");
    }

    #[test]
    fn an_empty_definition_list_yields_an_empty_child_set() {
        let d = definition(&[]);
        let grants = resolve(&d, &["a"], &["a"]).expect("resolves");
        assert!(grants.is_empty(), "no tools asked for, none granted");
    }

    #[test]
    fn duplicate_entries_collapse() {
        let d = definition(&["a", "a", "a"]);
        let grants = resolve(&d, &["a"], &["a"]).expect("resolves");
        assert_eq!(grants.tools, ["a"]);
    }

    #[test]
    fn a_namespaced_entry_resolves_against_the_registered_name() {
        let d = definition(&["github/get_issue"]);
        let grants = resolve(
            &d,
            &["github/get_issue", "read_file"],
            &["github/get_issue"],
        )
        .expect("resolves");
        assert_eq!(grants.tools, ["github/get_issue"]);
    }
}
