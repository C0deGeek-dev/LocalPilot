//! The local "memories used this turn" inspector.
//!
//! Records of which memories were used to answer a turn live in the session
//! event log (`MemoriesUsed`). This module reads the most recent such record and
//! enriches each id with its LocalMind provenance — source session, confidence,
//! epistemic status, contradictions, staleness — then renders it. Fully local
//! and offline: it only reads the project's own event log and memory store.

use crate::error::LearningError;
use localmind_core::MemoryEntryId;
use localmind_store::{MemoryPersistence, MemoryProvenance};
use localpilot_store::{MemoryUsed, SessionEvent, SessionEventKind};
use std::fmt::Write as _;
use std::path::Path;

/// A used memory enriched with its provenance for display.
#[derive(Debug, Clone, PartialEq)]
pub struct InspectedMemory {
    pub id: String,
    pub score: i64,
    pub layer: String,
    /// Provenance when the id resolves to an accepted memory; `None` for derived
    /// items (e.g. ingest chunks) that have no memory provenance.
    pub provenance: Option<ProvenanceView>,
}

/// A flattened, host-owned view of a memory's provenance (the adapter does not
/// leak LocalMind types past its boundary).
#[derive(Debug, Clone, PartialEq)]
pub struct ProvenanceView {
    pub source_session: Option<String>,
    pub confidence: f32,
    pub epistemic_status: String,
    pub status: String,
    pub stale_candidate: bool,
    pub contradicts: Vec<String>,
}

impl From<MemoryProvenance> for ProvenanceView {
    fn from(provenance: MemoryProvenance) -> Self {
        Self {
            source_session: provenance.source_session,
            confidence: provenance.confidence,
            epistemic_status: provenance.epistemic_status.as_str().to_string(),
            status: provenance.status,
            stale_candidate: provenance.stale_candidate,
            contradicts: provenance
                .contradicts
                .into_iter()
                .map(|id| id.as_str().to_string())
                .collect(),
        }
    }
}

/// The most recent turn's memories-used from a session's event log — the last
/// `MemoriesUsed` event, or empty when none was recorded.
#[must_use]
pub fn last_turn_memories_used(events: &[SessionEvent]) -> Vec<MemoryUsed> {
    events
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            SessionEventKind::MemoriesUsed { memories } => Some(memories.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Enrich a turn's used memories with provenance from the project's LocalMind
/// store. A missing memory store (project never learned) yields the bare used
/// records with no provenance rather than an error.
///
/// # Errors
/// Returns [`LearningError::Memory`] only when an existing store cannot be read.
pub fn inspect(
    project_root: &Path,
    used: &[MemoryUsed],
) -> Result<Vec<InspectedMemory>, LearningError> {
    let persistence = match MemoryPersistence::open_project(project_root) {
        Ok(persistence) => Some(persistence),
        // No store yet → no provenance, but still list what was used.
        Err(_) => None,
    };
    let mut inspected = Vec::with_capacity(used.len());
    for memory in used {
        let provenance = persistence
            .as_ref()
            .and_then(|store| {
                store
                    .provenance(&MemoryEntryId::new(&memory.id))
                    .ok()
                    .flatten()
            })
            .map(ProvenanceView::from);
        inspected.push(InspectedMemory {
            id: memory.id.clone(),
            score: memory.score,
            layer: memory.layer.clone(),
            provenance,
        });
    }
    Ok(inspected)
}

/// Render the inspection as plain text — the shared surface for the CLI and the
/// TUI panel. Deterministic, so it can be snapshot-tested.
#[must_use]
pub fn render(memories: &[InspectedMemory]) -> String {
    if memories.is_empty() {
        return "No memories were recorded as used for the last turn.".to_string();
    }
    let mut out = String::from("Memories used this turn:\n");
    for memory in memories {
        let _ = write!(
            out,
            "- [{}] {} (score {})",
            memory.layer, memory.id, memory.score
        );
        match &memory.provenance {
            Some(provenance) => {
                let _ = write!(
                    out,
                    "\n    status: {} · confidence: {:.2} · {}",
                    provenance.epistemic_status, provenance.confidence, provenance.status
                );
                if let Some(session) = &provenance.source_session {
                    let _ = write!(out, " · from session {session}");
                }
                if provenance.stale_candidate {
                    out.push_str("\n    ⚠ stale: anchored code changed, awaiting review");
                }
                if !provenance.contradicts.is_empty() {
                    let _ = write!(
                        out,
                        "\n    ⚠ contradicts: {}",
                        provenance.contradicts.join(", ")
                    );
                }
            }
            None => out.push_str(" — no memory provenance (derived item)"),
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use localpilot_core::EventId;
    use localpilot_store::SESSION_EVENT_FORMAT_VERSION;

    fn used_event(memories: Vec<MemoryUsed>) -> SessionEvent {
        SessionEvent {
            v: SESSION_EVENT_FORMAT_VERSION,
            id: EventId::new(),
            parent_id: None,
            at_unix: 1,
            kind: SessionEventKind::MemoriesUsed { memories },
        }
    }

    #[test]
    fn last_turn_picks_the_most_recent_record() {
        let events = vec![
            used_event(vec![MemoryUsed {
                id: "old".to_string(),
                score: 1,
                layer: "memory".to_string(),
            }]),
            used_event(vec![MemoryUsed {
                id: "new".to_string(),
                score: 2,
                layer: "fetch".to_string(),
            }]),
        ];
        let last = last_turn_memories_used(&events);
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].id, "new");
    }

    #[test]
    fn no_record_renders_a_clear_empty_message() {
        assert!(last_turn_memories_used(&[]).is_empty());
        assert_eq!(
            render(&[]),
            "No memories were recorded as used for the last turn."
        );
    }

    #[test]
    fn render_shows_provenance_staleness_and_contradictions() {
        let inspected = vec![
            InspectedMemory {
                id: "mem-1".to_string(),
                score: 42,
                layer: "memory".to_string(),
                provenance: Some(ProvenanceView {
                    source_session: Some("session-7".to_string()),
                    confidence: 0.8,
                    epistemic_status: "decision".to_string(),
                    status: "active".to_string(),
                    stale_candidate: true,
                    contradicts: vec!["mem-2".to_string()],
                }),
            },
            InspectedMemory {
                id: "chunk-9".to_string(),
                score: 5,
                layer: "index".to_string(),
                provenance: None,
            },
        ];
        let text = render(&inspected);
        assert!(text.contains("mem-1"));
        assert!(text.contains("decision"));
        assert!(text.contains("confidence: 0.80"));
        assert!(text.contains("from session session-7"));
        assert!(text.contains("stale"));
        assert!(text.contains("contradicts: mem-2"));
        assert!(
            text.contains("no memory provenance"),
            "derived item: {text}"
        );
    }
}
