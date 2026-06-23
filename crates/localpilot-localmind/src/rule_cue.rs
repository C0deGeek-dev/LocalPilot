//! Promote a curated lesson to an always-on "rule cue".
//!
//! A rule cue is terse guidance the agent reads every turn — advisory content
//! (ADR-0027), not a retrieval-dependent memory and not an enforced harness rule.
//! A weak model acts on a short, always-present rule better than on a paragraph it
//! has to retrieve. A curated lesson opts in by carrying the [`RULE_CUE_TAG`] tag
//! in its seed pack; at seed time its memory id is recorded in a host-side cue
//! registry, and the context hook injects those memories always-on, independent
//! of prompt relevance. The promotion list is host state (the host owns injection
//! policy, ADR-0036), keyed to ids in the engine's accepted memory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::LearningError;

/// The seed-pack tag that promotes a curated lesson to an always-on rule cue.
pub const RULE_CUE_TAG: &str = "rule-cue";

/// Host-side registry of memory ids promoted to always-on rule cues, under the
/// project's `.localmind/` state dir.
fn cue_store_path(project_root: &Path) -> PathBuf {
    project_root.join(".localmind").join("rule-cues.json")
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CueStore {
    ids: Vec<String>,
}

/// The memory ids currently promoted to rule cues. Empty when none are promoted
/// or the registry is absent/unreadable — best-effort, never fails a turn.
#[must_use]
pub fn rule_cue_ids(project_root: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(cue_store_path(project_root)) else {
        return Vec::new();
    };
    serde_json::from_str::<CueStore>(&text)
        .map(|store| store.ids)
        .unwrap_or_default()
}

/// Add memory ids to the rule-cue registry (idempotent + deduped), creating the
/// `.localmind/` dir if needed. A no-op for an empty id list.
///
/// # Errors
/// Returns [`LearningError::Memory`] if the registry cannot be written.
pub fn register_rule_cues(project_root: &Path, ids: &[String]) -> Result<(), LearningError> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut store_ids = rule_cue_ids(project_root);
    for id in ids {
        if !store_ids.iter().any(|existing| existing == id) {
            store_ids.push(id.clone());
        }
    }
    let path = cue_store_path(project_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| LearningError::Memory(format!("create cue dir: {e}")))?;
    }
    let json = serde_json::to_string_pretty(&CueStore { ids: store_ids })
        .map_err(|e| LearningError::Memory(format!("serialize cue store: {e}")))?;
    std::fs::write(&path, json)
        .map_err(|e| LearningError::Memory(format!("write cue store: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_read_round_trips_and_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(rule_cue_ids(root).is_empty());

        register_rule_cues(root, &["seed-a".to_string(), "seed-b".to_string()]).unwrap();
        // Re-registering an existing id does not duplicate it.
        register_rule_cues(root, &["seed-a".to_string(), "seed-c".to_string()]).unwrap();

        let ids = rule_cue_ids(root);
        assert_eq!(ids, vec!["seed-a", "seed-b", "seed-c"]);
    }

    #[test]
    fn registering_no_ids_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        register_rule_cues(root, &[]).unwrap();
        assert!(rule_cue_ids(root).is_empty());
    }
}
