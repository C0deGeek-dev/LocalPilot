//! Work sizing is independent of permissions, correctness and model names.
//! Only observed, session-local outcomes establish reliability. An active unit
//! may tighten as pressure grows; compaction never refunds its work budget.

use std::collections::BTreeSet;

use localpilot_config::GranularityConfig;
use localpilot_sandbox::Workspace;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Aggregate old + new material in a binary work unit, independent of text lines.
pub(crate) const MAX_BINARY_CHANGE_BYTES: u64 = 64 * 1024;

/// Context usage supplied by the calibrated runtime, not a model's claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextCapacity {
    pub used: usize,
    pub limit: usize,
    pub provenance: ContextProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextProvenance {
    RuntimeUsage,
    ResolvedBudget,
    CallerSupplied,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapabilityProvenance {
    Unknown,
    SessionObservations,
    RejectedObservation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reliability {
    Unknown,
    Weak,
    Strong,
    Malformed,
}

/// Counts only; never raw tool arguments, output, names or persisted scores.
#[derive(Debug, Clone, Default)]
pub struct CapabilityEvidence {
    valid_successes: usize,
    verified_units: usize,
    malformed: bool,
    failures: usize,
    last_input_valid: bool,
}

impl CapabilityEvidence {
    pub fn observe_input(&mut self, valid: bool) {
        self.last_input_valid = valid;
        self.malformed |= !valid;
    }

    pub fn observe_outcome(&mut self, success: bool) {
        if success && self.last_input_valid {
            self.valid_successes = self.valid_successes.saturating_add(1);
        } else if !success {
            self.failures = self.failures.saturating_add(1);
        }
    }

    pub fn observe_verification(&mut self) {
        self.verified_units = self.verified_units.saturating_add(1);
    }

    #[must_use]
    pub fn reliability(&self) -> Reliability {
        if self.malformed {
            Reliability::Malformed
        } else if self.failures != 0 {
            Reliability::Weak
        } else if self.valid_successes >= 8 && self.verified_units >= 2 {
            Reliability::Strong
        } else {
            Reliability::Unknown
        }
    }
}

/// An explicit declaration of one coherent plan unit, not proof of its diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkScope {
    pub files: usize,
    pub regions: usize,
    pub decisions: usize,
    pub changed_lines: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkProfile {
    pub context_provenance: ContextProvenance,
    pub capability_provenance: CapabilityProvenance,
    pub max_files: usize,
    pub max_regions: usize,
    pub max_read_lines: usize,
    pub max_changed_lines: usize,
    pub max_decisions: usize,
    pub max_output_bytes: usize,
    pub stop_after_checkpoint: bool,
    pub reliability: Reliability,
    pub constrained_context: bool,
}

impl WorkProfile {
    #[must_use]
    pub fn resolve(
        context: ContextCapacity,
        reliability: Reliability,
        caps: &GranularityConfig,
    ) -> Self {
        // Invalid capacity and >=70% pressure are conservative. A large context
        // cannot substitute for evidence of reliable structured work.
        let small = context.provenance == ContextProvenance::Unknown
            || context.limit < 32_000
            || context.used >= context.limit.saturating_mul(7) / 10;
        let strong = reliability == Reliability::Strong;
        let mut profile = Self {
            context_provenance: if context.limit == 0 {
                ContextProvenance::Unknown
            } else {
                context.provenance
            },
            capability_provenance: match reliability {
                Reliability::Unknown => CapabilityProvenance::Unknown,
                Reliability::Malformed => CapabilityProvenance::RejectedObservation,
                Reliability::Weak | Reliability::Strong => {
                    CapabilityProvenance::SessionObservations
                }
            },
            max_files: if small || !strong { 1 } else { 3 },
            max_regions: if strong {
                if small {
                    2
                } else {
                    4
                }
            } else {
                1
            },
            max_read_lines: if small { 80 } else { 200 },
            max_changed_lines: if small { 80 } else { 200 },
            max_decisions: if strong { 2 } else { 1 },
            max_output_bytes: if small { 4 * 1024 } else { 12 * 1024 },
            stop_after_checkpoint: small || !strong,
            reliability,
            constrained_context: small,
        };
        profile.tighten_caps(caps);
        profile
    }

    /// User configuration can only tighten an already captured profile.
    pub fn tighten_caps(&mut self, caps: &GranularityConfig) {
        for (bound, cap) in [
            (&mut self.max_files, caps.max_files),
            (&mut self.max_regions, caps.max_regions),
            (&mut self.max_read_lines, caps.max_read_lines),
            (&mut self.max_changed_lines, caps.max_changed_lines),
            (&mut self.max_decisions, caps.max_decisions),
        ] {
            if let Some(cap) = cap {
                *bound = (*bound).min(cap.max(1));
            }
        }
    }

    /// A reviewed step's declared footprint is a ceiling, including zero for
    /// read-only work. Configuration's positive floor does not widen that scope.
    pub fn bound_to_scope(&mut self, scope: WorkScope) {
        self.max_files = self.max_files.min(scope.files);
        self.max_regions = self.max_regions.min(scope.regions);
        self.max_decisions = self.max_decisions.min(scope.decisions);
        self.max_changed_lines = self.max_changed_lines.min(scope.changed_lines);
    }

    /// Tighten an active unit. Better telemetry or a compacted context cannot
    /// enlarge it until the next user turn / durable harness checkpoint.
    pub fn tighten(&mut self, next: Self) {
        self.max_files = self.max_files.min(next.max_files);
        self.max_regions = self.max_regions.min(next.max_regions);
        self.max_read_lines = self.max_read_lines.min(next.max_read_lines);
        self.max_changed_lines = self.max_changed_lines.min(next.max_changed_lines);
        self.max_decisions = self.max_decisions.min(next.max_decisions);
        self.max_output_bytes = self.max_output_bytes.min(next.max_output_bytes);
        self.stop_after_checkpoint |= next.stop_after_checkpoint;
        self.constrained_context |= next.constrained_context;
        if next.reliability != Reliability::Strong {
            self.reliability = next.reliability;
            self.capability_provenance = next.capability_provenance;
        }
    }

    #[must_use]
    pub fn accepts(&self, scope: WorkScope) -> bool {
        scope.files <= self.max_files
            && scope.regions <= self.max_regions
            && scope.decisions <= self.max_decisions
            && scope.changed_lines <= self.max_changed_lines
            && scope.regions >= scope.files
            && scope.decisions > 0
    }

    #[must_use]
    pub fn instruction(&self) -> String {
        // Only what the model acts on: the limits, that they hold through
        // shell/MCP and across compaction, verification, and the stop rule.
        // How the profile was derived stays in the session's events.
        let stop = if self.stop_after_checkpoint {
            " Then stop at that verified checkpoint and describe the next action."
        } else {
            ""
        };
        format!(
            "Work unit: change at most {} files, {} regions, {} decisions and {} lines; read at most {} lines at a time; prefer exact edits to rewriting large files. These limits also apply through shell and MCP commands, and compaction does not reset them. Verify the unit with the smallest relevant check, keeping every acceptance criterion and required quality gate.{stop}",
            self.max_files, self.max_regions, self.max_decisions, self.max_changed_lines,
            self.max_read_lines,
        )
    }
}

/// Cumulative *attempted* mutation budget. Failed/partial writes cannot refund
/// it; repeated small calls cannot evade it. Read paths do not spend file slots.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkUnit {
    files: BTreeSet<std::path::PathBuf>,
    regions: usize,
    lines: usize,
}

impl WorkUnit {
    pub(crate) fn refusal(
        &self,
        profile: WorkProfile,
        name: &str,
        input: &Value,
        workspace: &Workspace,
    ) -> Option<String> {
        self.clone()
            .check_and_reserve(profile, name, input, workspace)
    }
    pub(crate) fn has_mutations(&self) -> bool {
        self.regions != 0
    }
    pub(crate) fn check_and_reserve(
        &mut self,
        profile: WorkProfile,
        name: &str,
        input: &Value,
        workspace: &Workspace,
    ) -> Option<String> {
        let reject = |reason: &str| {
            Some(format!("work envelope: {reason}; split into a smaller coherent unit, verify it, and checkpoint before continuing"))
        };
        if name == "swarm" {
            return reject("fan-out has no single coherent mutation scope; split it into independently reviewed units");
        }
        if name == "delegate" {
            if self.regions >= profile.max_regions {
                return reject("delegation would exceed the active region budget");
            }
            self.regions = self.regions.saturating_add(1);
            return None;
        }
        if matches!(name, "read_file" | "read_tool_output") {
            let start = input.get("start_line").and_then(Value::as_u64).unwrap_or(1);
            let end = input.get("end_line").and_then(Value::as_u64);
            // The builtin serves an implicit bounded page only after permission.
            // Leave missing-path and content errors to that authorized read too.
            if name == "read_file" && end.is_none() {
                return None;
            }
            let bounded =
                end.is_some_and(|end| end >= start && end - start < profile.max_read_lines as u64);
            if !bounded {
                return Some(format!(
                    "work envelope: request an explicit start_line/end_line page within the read limit. {}",
                    localpilot_tools::bounded_read_hint(name, input, profile.max_read_lines)
                ));
            }
            return None;
        }
        if name == "replace_in_file" {
            // Regex/global substitution has no argument-derived region bound.
            return reject("use bounded exact edit_file hunks instead of global/regex replacement");
        }
        let operations: Vec<&Value> = match name {
            "apply_patch" => input
                .get("operations")
                .and_then(Value::as_array)?
                .iter()
                .collect(),
            "write_file" | "append_file" | "edit_file" | "multi_edit" => vec![input],
            _ => return None,
        };
        let mut proposed = self.clone();
        for op in operations {
            let path = workspace
                .resolve(std::path::Path::new(op.get("path")?.as_str()?))
                .ok()?;
            let action = op.get("action").and_then(Value::as_str).unwrap_or(name);
            if matches!(action, "delete" | "write_file") {
                // Bounds deletion/overwrite of existing material, not just its replacement.
                if let Ok(meta) = std::fs::metadata(&path) {
                    if meta.len() > profile.max_changed_lines as u64 {
                        return reject(
                            "whole-file overwrite/deletion is unbounded; use exact region edits",
                        );
                    }
                    // Byte length is a conservative upper bound on removed
                    // lines, without reading content before permission.
                    proposed.lines = proposed.lines.saturating_add(meta.len() as usize);
                }
            }
            proposed.files.insert(path);
            let hunks: Vec<&Value> = match name {
                "multi_edit" => op.get("edits").and_then(Value::as_array)?.iter().collect(),
                "apply_patch" if action == "update" => {
                    op.get("hunks").and_then(Value::as_array)?.iter().collect()
                }
                _ => vec![op],
            };
            for hunk in hunks {
                proposed.regions = proposed.regions.saturating_add(1);
                for field in ["old_text", "new_text", "content"] {
                    if let Some(text) = hunk.get(field).and_then(Value::as_str) {
                        // Byte-equivalent cost also bounds giant single lines.
                        proposed.lines = proposed
                            .lines
                            .saturating_add(text.lines().count().max(text.len().div_ceil(80)));
                    }
                }
            }
        }
        if proposed.files.len() > profile.max_files
            || proposed.regions > profile.max_regions
            || proposed.lines > profile.max_changed_lines
        {
            return reject("cumulative files, regions or patch size exceeds the active profile");
        }
        *self = proposed;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_axes_and_monotonic_caps() {
        for reliability in [
            Reliability::Unknown,
            Reliability::Weak,
            Reliability::Strong,
            Reliability::Malformed,
        ] {
            let small = WorkProfile::resolve(
                ContextCapacity {
                    used: 0,
                    limit: 8_000,
                    provenance: crate::granularity::ContextProvenance::CallerSupplied,
                },
                reliability,
                &GranularityConfig::default(),
            );
            let large = WorkProfile::resolve(
                ContextCapacity {
                    used: 0,
                    limit: 128_000,
                    provenance: crate::granularity::ContextProvenance::CallerSupplied,
                },
                reliability,
                &GranularityConfig::default(),
            );
            assert!(small.max_read_lines < large.max_read_lines);
            assert!(small.max_changed_lines < large.max_changed_lines);
            assert_eq!(
                large.max_decisions,
                if reliability == Reliability::Strong {
                    2
                } else {
                    1
                }
            );
            assert_eq!(
                large.max_files,
                if reliability == Reliability::Strong {
                    3
                } else {
                    1
                }
            );
            let capped = WorkProfile::resolve(
                ContextCapacity {
                    used: 0,
                    limit: 128_000,
                    provenance: crate::granularity::ContextProvenance::CallerSupplied,
                },
                reliability,
                &GranularityConfig {
                    max_files: Some(100),
                    max_read_lines: Some(10),
                    ..GranularityConfig::default()
                },
            );
            assert_eq!(capped.max_files, large.max_files);
            assert_eq!(capped.max_read_lines, 10);
        }
    }

    #[test]
    fn compaction_and_success_cannot_enlarge_active_unit() {
        let mut active = WorkProfile::resolve(
            ContextCapacity {
                used: 100_000,
                limit: 128_000,
                provenance: crate::granularity::ContextProvenance::CallerSupplied,
            },
            Reliability::Unknown,
            &GranularityConfig::default(),
        );
        let original = active;
        active.tighten(WorkProfile::resolve(
            ContextCapacity {
                used: 0,
                limit: 128_000,
                provenance: crate::granularity::ContextProvenance::CallerSupplied,
            },
            Reliability::Strong,
            &GranularityConfig::default(),
        ));
        assert_eq!(active, original);
    }

    #[test]
    fn unknown_capacity_and_read_only_scope_never_authorize_more_work() {
        let mut profile = WorkProfile::resolve(
            ContextCapacity {
                used: 0,
                limit: 128_000,
                provenance: ContextProvenance::Unknown,
            },
            Reliability::Strong,
            &GranularityConfig::default(),
        );
        assert_eq!(profile.max_read_lines, 80);
        assert_eq!(profile.max_files, 1);
        profile.bound_to_scope(WorkScope {
            files: 0,
            regions: 0,
            decisions: 1,
            changed_lines: 0,
        });
        assert_eq!(profile.max_files, 0);
        assert_eq!(profile.max_regions, 0);
        assert_eq!(profile.max_changed_lines, 0);
    }

    #[test]
    fn observations_need_repeated_valid_success_and_verification() {
        let mut evidence = CapabilityEvidence::default();
        evidence.observe_input(true);
        evidence.observe_outcome(true);
        assert_eq!(evidence.reliability(), Reliability::Unknown);
        for _ in 0..7 {
            evidence.observe_input(true);
            evidence.observe_outcome(true);
        }
        evidence.observe_verification();
        assert_eq!(evidence.reliability(), Reliability::Unknown);
        evidence.observe_verification();
        assert_eq!(evidence.reliability(), Reliability::Strong);
        evidence.observe_input(false);
        evidence.observe_outcome(true);
        assert_eq!(evidence.reliability(), Reliability::Malformed);
    }
}
