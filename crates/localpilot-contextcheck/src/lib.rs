//! Read-only inventory of the statically authored context a LocalPilot session
//! assembles.
//!
//! A session's authored context is built from three kinds of source: the
//! composed system prompt, the discovered instruction files
//! (Navigator/CLAUDE/AGENTS/Copilot, plus path-scoped `*.instructions.md`), and
//! the skills visible to the project. This crate enumerates those layers and
//! attaches a token estimate to each, reusing the same discovery, redaction, and
//! token seams the live harness uses so the inventory cannot drift from what a
//! real session would assemble. It reads; it never edits.
//!
//! Layer bodies are held in memory for downstream analysis but are redacted at
//! construction (the same canonical redactor the injection path uses) and are
//! deliberately excluded from the serializable [`InventorySummary`] — a body can
//! contain a cleartext secret, and the summary is what rides a machine-readable
//! report.
//!
//! Reserved harness runtime files (`brief.md`, `PROGRESS.md`, `DECISIONS.md`,
//! `LESSONS.md`) are never inventoried: the discovery seam only yields the
//! specific instruction filenames, so those runtime documents are excluded by
//! construction.
#![forbid(unsafe_code)]

use std::path::Path;

use localpilot_config::{redact::redact, ContextDiscovery};
use localpilot_core::{Message, Role};
use localpilot_harness::estimate_tokens;
use localpilot_skills::{discovery_roots, SkillSet};
use serde::Serialize;

pub mod analyze;
pub use analyze::{analyze, ContextFinding, ContextFindingKind, ContextReport, Thresholds};

/// Which authored context layer a piece of text belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "layer", rename_all = "snake_case")]
pub enum LayerKind {
    /// The composed system prompt (supplied by the caller — the harness builds
    /// it from the tool registry, so it is passed in rather than rebuilt here).
    SystemPrompt,
    /// A discovered instruction file. `kind` is the discovery classification
    /// (e.g. `Claude`, `Agents`, `Navigator`, `Copilot`) and `scope` its tier.
    Instruction { kind: String, scope: String },
    /// A resolved skill's instruction body.
    Skill { name: String },
}

/// One authored context layer: its kind, a source label, the (redacted) body,
/// and a token estimate. The body is retained for analysis but is never
/// serialized — see the module docs.
#[derive(Debug, Clone)]
pub struct ContextLayer {
    /// What kind of layer this is.
    pub kind: LayerKind,
    /// A stable, human-readable source label — a path, or `system-prompt`.
    pub source: String,
    /// The redacted layer text. Not serialized (it can hold a secret).
    pub body: String,
    /// A rough token estimate for the body, via the harness estimator.
    pub tokens: usize,
}

/// The full in-memory inventory. Feeds the analyzers; not serialized directly.
#[derive(Debug, Clone)]
pub struct ContextInventory {
    /// Every authored layer, in assembly order (system prompt, then instruction
    /// files in discovery precedence, then skills).
    pub layers: Vec<ContextLayer>,
}

impl ContextInventory {
    /// A serializable, body-free view: per-layer weights plus totals. This is
    /// the shape safe to print in a machine-readable report.
    #[must_use]
    pub fn summary(&self) -> InventorySummary {
        let layers: Vec<LayerWeight> = self
            .layers
            .iter()
            .map(|layer| LayerWeight {
                kind: layer.kind.clone(),
                source: layer.source.clone(),
                tokens: layer.tokens,
                chars: layer.body.chars().count(),
            })
            .collect();
        let total_tokens = layers.iter().map(|l| l.tokens).sum();
        let total_chars = layers.iter().map(|l| l.chars).sum();
        InventorySummary {
            layers,
            total_tokens,
            total_chars,
        }
    }
}

/// A serializable per-layer weight — no body text, safe to print.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LayerWeight {
    /// The layer kind, flattened so its tag rides alongside the weight fields.
    #[serde(flatten)]
    pub kind: LayerKind,
    /// The layer's source label.
    pub source: String,
    /// Rough token estimate.
    pub tokens: usize,
    /// Character count of the (redacted) body.
    pub chars: usize,
}

/// A body-free summary of an inventory: per-layer weights and totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InventorySummary {
    /// Per-layer weights, in inventory order.
    pub layers: Vec<LayerWeight>,
    /// Sum of per-layer token estimates.
    pub total_tokens: usize,
    /// Sum of per-layer character counts.
    pub total_chars: usize,
}

/// Estimate tokens for a single block of text, reusing the harness estimator
/// (rather than duplicating its heuristic) by wrapping the text in one message.
fn tokens_of(text: &str) -> usize {
    estimate_tokens(&[Message::text(Role::User, text.to_string())])
}

/// Build the authored-context inventory for the project rooted at `root`.
///
/// `home` is the user's home directory (for global skills); `trusted` gates the
/// project skill overlay, mirroring the skills discovery contract. `system_prompt`
/// is the composed system prompt when the caller can supply it (the harness
/// builds it from the tool registry); pass `None` to omit that layer.
///
/// Instruction discovery matches the live harness exactly
/// (`ContextDiscovery::new(root).discover()`), so the inventory reflects what a
/// session would actually assemble. Every body is redacted at construction.
#[must_use]
pub fn inventory(
    root: &Path,
    home: Option<&Path>,
    trusted: bool,
    system_prompt: Option<String>,
) -> ContextInventory {
    let mut layers = Vec::new();

    if let Some(prompt) = system_prompt {
        let body = redact(&prompt);
        let tokens = tokens_of(&body);
        layers.push(ContextLayer {
            kind: LayerKind::SystemPrompt,
            source: "system-prompt".to_string(),
            body,
            tokens,
        });
    }

    // Instruction files, matching the harness discovery seam so the inventory
    // cannot drift from the session. The seam yields only the specific
    // instruction filenames, so reserved runtime files are excluded here.
    let project = ContextDiscovery::new(root).discover();
    for file in project.files {
        let body = redact(&file.body);
        let tokens = tokens_of(&body);
        layers.push(ContextLayer {
            kind: LayerKind::Instruction {
                kind: format!("{:?}", file.kind),
                scope: format!("{:?}", file.scope),
            },
            source: file.path.display().to_string(),
            body,
            tokens,
        });
    }

    // Skills: text only (pull-based advisory modules). The project overlay is
    // gated on `trusted`, as in the skills discovery contract.
    if let Ok(set) = SkillSet::resolve(&discovery_roots(root, home, trusted)) {
        for name in set.names() {
            if let Some(skill) = set.by_name(name) {
                let body = redact(&skill.instructions);
                let tokens = tokens_of(&body);
                layers.push(ContextLayer {
                    kind: LayerKind::Skill {
                        name: name.to_string(),
                    },
                    source: skill.dir.display().to_string(),
                    body,
                    tokens,
                });
            }
        }
    }

    ContextInventory { layers }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn discovers_an_instruction_file_with_a_token_weight() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join("CLAUDE.md"),
            "Always match surrounding style.",
        );
        let inv = inventory(dir.path(), None, true, None);
        let instr: Vec<_> = inv
            .layers
            .iter()
            .filter(|l| matches!(l.kind, LayerKind::Instruction { .. }))
            .collect();
        assert_eq!(instr.len(), 1, "one instruction layer expected");
        assert!(instr[0].source.contains("CLAUDE.md"));
        assert!(instr[0].tokens > 0, "instruction layer should have tokens");
    }

    #[test]
    fn system_prompt_layer_is_present_only_when_supplied() {
        let dir = TempDir::new().unwrap();
        let without = inventory(dir.path(), None, false, None);
        assert!(!without
            .layers
            .iter()
            .any(|l| l.kind == LayerKind::SystemPrompt));

        let with = inventory(
            dir.path(),
            None,
            false,
            Some("You are a coding agent.".to_string()),
        );
        assert_eq!(
            with.layers.first().map(|l| &l.kind),
            Some(&LayerKind::SystemPrompt)
        );
        assert!(with.layers[0].tokens > 0);
    }

    #[test]
    fn summary_carries_weights_but_never_a_body() {
        let dir = TempDir::new().unwrap();
        let marker = "UNIQUE-BODY-MARKER-should-not-serialize";
        write(&dir.path().join("AGENTS.md"), marker);
        let inv = inventory(dir.path(), None, true, None);
        let summary = inv.summary();
        assert!(summary.total_tokens > 0);
        assert!(!summary.layers.is_empty());

        let json = serde_json::to_string(&summary).unwrap();
        assert!(
            !json.contains(marker),
            "the serialized summary must not carry layer body text"
        );
    }

    #[test]
    fn reserved_runtime_files_are_not_inventoried() {
        let dir = TempDir::new().unwrap();
        // A harness runtime file sitting next to a real instruction file.
        write(&dir.path().join("PROGRESS.md"), "step 1 done");
        write(&dir.path().join("CLAUDE.md"), "Prefer small functions.");
        let inv = inventory(dir.path(), None, true, None);
        assert!(
            !inv.layers.iter().any(|l| l.source.contains("PROGRESS.md")),
            "reserved runtime files must never be inventoried as context"
        );
        assert!(inv.layers.iter().any(|l| l.source.contains("CLAUDE.md")));
    }

    #[test]
    fn resolves_a_project_skill_as_a_layer() {
        let dir = TempDir::new().unwrap();
        let skill_md = dir
            .path()
            .join(".localpilot")
            .join("skills")
            .join("demo")
            .join("SKILL.md");
        write(
            &skill_md,
            "---\nname: demo\ndescription: a demo skill\n---\n\nDo the demo thing.\n",
        );
        let inv = inventory(dir.path(), None, true, None);
        let skill = inv.layers.iter().find(|l| {
            l.kind
                == LayerKind::Skill {
                    name: "demo".to_string(),
                }
        });
        let skill = skill.expect("the demo skill should be inventoried as a layer");
        assert!(skill.tokens > 0);
    }

    #[test]
    fn instruction_bodies_are_redacted_at_construction() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join("CLAUDE.md"),
            "Use sk-abcdefghijklmnopqrstuvwxyz0123 as the key.",
        );
        let inv = inventory(dir.path(), None, true, None);
        let layer = inv
            .layers
            .iter()
            .find(|l| l.source.contains("CLAUDE.md"))
            .expect("the instruction layer");
        assert!(!layer.body.contains("sk-abcdefghijklmnopqrstuvwxyz0123"));
        assert!(layer.body.contains("[REDACTED]"));
    }
}
