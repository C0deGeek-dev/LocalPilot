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
    /// Host refusals and completed negative observations supply no execution
    /// evidence. Keeping them neutral also prevents refusals earning successes.
    pub(crate) fn observe_tool_outcome(
        &mut self,
        outcome: localpilot_core::ToolOutcome,
        attributable: bool,
    ) {
        if attributable {
            match outcome {
                localpilot_core::ToolOutcome::Ok => self.observe_outcome(true),
                localpilot_core::ToolOutcome::Unusable => self.observe_outcome(false),
                localpilot_core::ToolOutcome::ReportedFailure => {}
            }
        }
    }

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
    /// Cost allowed for the files a unit creates, counted apart from
    /// `max_changed_lines`: a new file is one region however long it is, and the
    /// edits that follow it do not add regions.
    pub max_new_file_lines: usize,
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
            max_new_file_lines: if small { 200 } else { 600 },
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
        // A configured line cap is the user's ceiling for a whole file too.
        if let Some(cap) = caps.max_changed_lines {
            self.max_new_file_lines = self.max_new_file_lines.min(cap.max(1));
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
        self.max_new_file_lines = self.max_new_file_lines.min(next.max_new_file_lines);
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
            "Work unit: change at most {} files, {} regions, {} decisions and {} lines; read at most {} lines at a time; a new file may be up to {} lines; prefer exact edits to rewriting large files, and use replace_in_file only for one exact block (regex false, all false). These limits also apply through shell and MCP commands, and compaction does not reset them. Verify the unit with the smallest relevant check, keeping every acceptance criterion and required quality gate.{stop}",
            self.max_files, self.max_regions, self.max_decisions, self.max_changed_lines,
            self.max_read_lines, self.max_new_file_lines,
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
    /// Files this unit created, with the cost of their content so far. The
    /// repository diff sees such a file as one new region, so edits to it spend
    /// lines but no further region.
    created: std::collections::BTreeMap<std::path::PathBuf, usize>,
}

/// Keep recovery guidance separate from a bounds denial: only the latter
/// prevents an opaque process from retrying the refused mutation.
pub(crate) struct WorkRefusal {
    pub(crate) message: String,
    pub(crate) blocks_opaque_retry: bool,
}

impl WorkUnit {
    pub(crate) fn refusal(
        &self,
        profile: WorkProfile,
        name: &str,
        input: &Value,
        workspace: &Workspace,
    ) -> Option<WorkRefusal> {
        self.clone()
            .check_and_reserve(profile, name, input, workspace)
    }
    pub(crate) fn has_mutations(&self) -> bool {
        self.regions != 0
    }

    fn new_lines(&self) -> usize {
        self.created.values().sum()
    }

    fn usage(&self, profile: WorkProfile) -> String {
        format!(
            "files {}/{}, regions {}/{}, changed lines {}/{}, new-file lines {}/{}",
            self.files.len(),
            profile.max_files,
            self.regions,
            profile.max_regions,
            self.lines,
            profile.max_changed_lines,
            self.new_lines(),
            profile.max_new_file_lines,
        )
    }
    pub(crate) fn check_and_reserve(
        &mut self,
        profile: WorkProfile,
        name: &str,
        input: &Value,
        workspace: &Workspace,
    ) -> Option<WorkRefusal> {
        let reject = |reason: &str| {
            Some(WorkRefusal {
                message: format!("work envelope: {reason}; split into a smaller coherent unit, verify it, and checkpoint before continuing"),
                blocks_opaque_retry: true,
            })
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
                return Some(WorkRefusal { message: format!(
                    "work envelope: request an explicit start_line/end_line page within the read limit. {}",
                    localpilot_tools::bounded_read_hint(name, input, profile.max_read_lines)
                ), blocks_opaque_retry: false });
            }
            return None;
        }
        if name == "replace_in_file" {
            // One literal block replaced once is an exact edit: the same text in
            // the same place, so it is sized as one. A regex, or a replacement of
            // every occurrence (the tool's default), has no argument-derived
            // region bound.
            let literal_once = input.get("regex").and_then(Value::as_bool) != Some(true)
                && input.get("all").and_then(Value::as_bool) == Some(false);
            if literal_once {
                let (Some(path), Some(find), Some(replace)) = (
                    input.get("path").and_then(Value::as_str),
                    input.get("find").and_then(Value::as_str),
                    input.get("replace").and_then(Value::as_str),
                ) else {
                    return None; // The tool reports the malformed input itself.
                };
                let edit = serde_json::json!({"path": path, "old_text": find, "new_text": replace});
                return self.check_and_reserve(profile, "edit_file", &edit, workspace);
            }
            return Some(WorkRefusal {
                message: "work envelope: use bounded exact edit_file hunks instead of global/regex replacement (replace_in_file is admitted only as one exact block: regex false and all false); split into a smaller coherent unit, verify it, and checkpoint before continuing".to_string(),
                blocks_opaque_retry: false,
            });
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
            // Sizing must cover every target, including scratch and external
            // paths. Containment belongs to permission checks; a failed
            // containment check must not admit the rest of a mixed patch.
            let path = match workspace.normalize(std::path::Path::new(op.get("path")?.as_str()?)) {
                Ok(path) => path,
                Err(_) => return reject("write target cannot be normalized for bounded sizing"),
            };
            // The unit's budget sizes changes to the repository, which the diff
            // inspection after the step measures. The session's own scratch
            // directory is outside it, and a script written there changes the
            // repository only through a command, which is gated and inspected on
            // its own. Other external paths still spend the budget.
            if workspace.scratch_contains(&path) {
                continue;
            }
            let action = op.get("action").and_then(Value::as_str).unwrap_or(name);
            let on_disk = std::fs::metadata(&path);
            let writes_content = matches!(name, "write_file" | "append_file")
                || (name == "apply_patch" && action == "create");
            let creates =
                writes_content && on_disk.is_err() && !proposed.created.contains_key(&path);
            let ours = creates || proposed.created.contains_key(&path);
            if matches!(action, "delete" | "write_file") && !ours {
                // Bounds deletion/overwrite of existing material, not just its replacement.
                if let Ok(meta) = &on_disk {
                    if meta.len() > profile.max_changed_lines as u64 {
                        return reject(&format!(
                            "whole-file overwrite/deletion is unbounded ({} bytes against {} changed lines); use exact region edits",
                            meta.len(),
                            profile.max_changed_lines
                        ));
                    }
                    // Byte length is a conservative upper bound on removed
                    // lines, without reading content before permission.
                    proposed.lines = proposed.lines.saturating_add(meta.len() as usize);
                }
            }
            if action == "write_file" && ours && !creates {
                // Rewriting what this unit created replaces its cost, not adds to it.
                proposed.created.insert(path.clone(), 0);
            }
            if creates {
                proposed.created.insert(path.clone(), 0);
            }
            proposed.files.insert(path.clone());
            let hunks: Vec<&Value> = match name {
                "multi_edit" => op.get("edits").and_then(Value::as_array)?.iter().collect(),
                "apply_patch" if action == "update" => {
                    op.get("hunks").and_then(Value::as_array)?.iter().collect()
                }
                _ => vec![op],
            };
            for hunk in hunks {
                let mut cost = 0usize;
                for field in ["old_text", "new_text", "content"] {
                    if let Some(text) = hunk.get(field).and_then(Value::as_str) {
                        // Byte-equivalent cost also bounds giant single lines.
                        cost =
                            cost.saturating_add(text.lines().count().max(text.len().div_ceil(80)));
                    }
                }
                if ours {
                    if let Some(spent) = proposed.created.get_mut(&path) {
                        *spent = spent.saturating_add(cost);
                    }
                } else {
                    proposed.lines = proposed.lines.saturating_add(cost);
                }
                // Bound material before allocating the argument-only diff. A
                // single submitted hunk can hide edits separated by equal lines.
                if proposed.lines > profile.max_changed_lines
                    || proposed.new_lines() > profile.max_new_file_lines
                {
                    return reject(&format!(
                        "cumulative files, regions or patch size exceeds the active profile ({})",
                        proposed.usage(profile)
                    ));
                }
                let regions = match (
                    hunk.get("old_text").and_then(Value::as_str),
                    hunk.get("new_text").and_then(Value::as_str),
                ) {
                    (Some(old), Some(new)) => match argument_change_regions(old, new) {
                        Some(regions) => regions.max(1), // No-op attempts still spend a hunk.
                        None => return reject("exact edit preview exceeds its computation bound"),
                    },
                    _ => 1,
                };
                // The repository diff counts a created file as one region, so
                // edits to it spend no further region; its creation spent one.
                let regions = if ours && !creates { 0 } else { regions };
                proposed.regions = proposed.regions.saturating_add(regions);
            }
        }
        if proposed.files.len() > profile.max_files
            || proposed.regions > profile.max_regions
            || proposed.lines > profile.max_changed_lines
            || proposed.new_lines() > profile.max_new_file_lines
        {
            return reject(&format!(
                "cumulative files, regions or patch size exceeds the active profile ({})",
                proposed.usage(profile)
            ));
        }
        *self = proposed;
        None
    }
}

/// Preview separated changes using only supplied text, never workspace content.
/// This tightens admission; the actual repository diff remains authoritative.
fn argument_change_regions(old: &str, new: &str) -> Option<usize> {
    let old: Vec<_> = old.lines().collect();
    let new: Vec<_> = new.lines().collect();
    // Live profiles permit at most 200 aggregate lines. Independently bound
    // argument scanning even for an externally constructed profile.
    if old.len().checked_add(new.len())? > 400 {
        return None;
    }
    let mut anchors = Vec::new();
    let mut seen = BTreeSet::new();
    for (i, line) in old.iter().enumerate() {
        let mut positions = new.iter().enumerate().filter(|(_, other)| *other == line);
        if let Some((j, _)) = positions.next() {
            // Repeated or reordered shared lines permit competing alignments.
            // Retain the previous one-hunk admission in those cases; the actual
            // post-write diff still enforces the effective region budget.
            if positions.next().is_some()
                || !seen.insert(line)
                || anchors.last().is_some_and(|&(_, previous)| j <= previous)
            {
                return Some(1);
            }
            anchors.push((i, j));
        }
    }
    let (mut old_end, mut new_end, mut regions) = (0, 0, 0);
    for (i, j) in anchors {
        if i > old_end || j > new_end {
            regions += 1;
        }
        old_end = i + 1;
        new_end = j + 1;
    }
    if old_end < old.len() || new_end < new.len() {
        regions += 1;
    }
    Some(regions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_edit_preview_counts_material_runs_with_bounded_memory() {
        for (old, new, expected) in [
            ("", "", 0),
            ("same\n", "same\n", 0),
            ("a\nb\nc\n", "a\nB\nC\n", 1),
            ("a\nb\nc\n", "a\nx\nb\nc\n", 1),
            ("a\nb\nc\n", "a\nc\n", 1),
            ("a\nb\na\nc\n", "a\nB\na\nC\n", 1),
            ("a\n", "b\na\na\n", 1),
            ("a\nb\n", "b\nb\n", 1),
            ("a\nb\n", "b\na\n", 1),
            ("first\nanchor\nlast\n", "FIRST\nanchor\nLAST\n", 2),
            ("a\r\nb\r\nc\r\n", "a\r\nB\r\nc\r\n", 1),
            ("a\nb\n", "", 1),
        ] {
            assert_eq!(argument_change_regions(old, new), Some(expected));
        }
        let large = "line\n".repeat(256);
        assert_eq!(argument_change_regions(&large, &large), None);
    }

    #[test]
    fn external_targets_spend_the_mutation_budget_and_the_sessions_scratch_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut workspace = Workspace::new(dir.path()).unwrap();
        workspace.set_scratch_root(localpilot_sandbox::ScratchRoot::Parent(
            outside.path().to_path_buf(),
        ));
        workspace.start_scratch("bounded-sizing").unwrap();
        let scratch = workspace.scratch_dir().unwrap().join("scratch.txt");
        let external = outside.path().join("external.txt");
        let profile = WorkProfile::resolve(
            ContextCapacity {
                used: 0,
                limit: 8_000,
                provenance: ContextProvenance::CallerSupplied,
            },
            Reliability::Unknown,
            &GranularityConfig::default(),
        );
        // An external path still spends the budget.
        let mut unit = WorkUnit::default();
        let input = serde_json::json!({"path":external, "content":"small
"});
        assert!(unit
            .check_and_reserve(profile, "write_file", &input, &workspace)
            .is_none());
        assert!(
            unit.has_mutations(),
            "non-workspace targets must be counted"
        );
        let second = serde_json::json!({"path":"tracked.txt", "old_text":"a", "new_text":"b"});
        assert!(unit
            .check_and_reserve(profile, "edit_file", &second, &workspace)
            .is_some());

        // The session's scratch does not: any number of probe files, however
        // long, leave the unit's own file and region free.
        let mut unit = WorkUnit::default();
        for name in ["probe.ps1", "second.ps1", "third.ps1"] {
            let path = workspace.scratch_dir().unwrap().join(name);
            let input = serde_json::json!({"path":path, "content":"x
".repeat(500)});
            assert!(unit
                .check_and_reserve(profile, "write_file", &input, &workspace)
                .is_none());
        }
        assert!(!unit.has_mutations(), "scratch is not repository work");
        let own = serde_json::json!({"path":"tracked.txt", "old_text":"a", "new_text":"b"});
        assert!(unit
            .check_and_reserve(profile, "edit_file", &own, &workspace)
            .is_none());
        let _ = scratch;
    }

    #[test]
    fn replace_in_file_is_sized_as_an_exact_edit_only_for_one_literal_block() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let profile = WorkProfile::resolve(
            ContextCapacity {
                used: 0,
                limit: 64_000,
                provenance: ContextProvenance::ResolvedBudget,
            },
            Reliability::Unknown,
            &GranularityConfig::default(),
        );
        let once = serde_json::json!({
            "path":"a.html", "find":"old", "replace":"new", "regex":false, "all":false
        });
        let mut unit = WorkUnit::default();
        assert!(unit
            .check_and_reserve(profile, "replace_in_file", &once, &workspace)
            .is_none());
        assert!(unit.has_mutations(), "it spends a region like an edit");
        let again = serde_json::json!({
            "path":"a.html", "find":"other", "replace":"new", "all":false
        });
        assert!(
            unit.check_and_reserve(profile, "replace_in_file", &again, &workspace)
                .is_some(),
            "the second block is a second region"
        );
        for input in [
            serde_json::json!({"path":"a.html","find":"x","replace":"y"}),
            serde_json::json!({"path":"a.html","find":"x","replace":"y","all":true}),
            serde_json::json!({"path":"a.html","find":"x.*","replace":"y","regex":true,"all":false}),
        ] {
            let refusal = WorkUnit::default()
                .check_and_reserve(profile, "replace_in_file", &input, &workspace)
                .expect("a regex or every-occurrence replacement has no bound");
            assert!(refusal.message.contains("regex false and all false"));
        }
        // A replacement that is itself too large is refused like an edit.
        let big = serde_json::json!({
            "path":"a.html", "find":"old", "replace":"line
        ".repeat(250), "all":false
        });
        assert!(WorkUnit::default()
            .check_and_reserve(profile, "replace_in_file", &big, &workspace)
            .is_some());
    }

    #[test]
    fn mixed_patch_refusal_is_atomic_and_does_not_spend_a_partial_budget() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let profile = WorkProfile::resolve(
            ContextCapacity {
                used: 0,
                limit: 8_000,
                provenance: ContextProvenance::CallerSupplied,
            },
            Reliability::Unknown,
            &GranularityConfig::default(),
        );
        for external_first in [true, false] {
            let create = serde_json::json!({"action":"create", "path":outside.path().join("header.txt"), "content":"small\n"});
            let delete = serde_json::json!({"action":"delete", "path":"tracked.txt"});
            let operations = if external_first {
                vec![create, delete]
            } else {
                vec![delete, create]
            };
            let mut unit = WorkUnit::default();
            assert!(unit
                .check_and_reserve(
                    profile,
                    "apply_patch",
                    &serde_json::json!({"operations":operations}),
                    &workspace
                )
                .is_some());
            assert!(
                !unit.has_mutations(),
                "refusal must not reserve earlier operations"
            );
            let bounded = serde_json::json!({"operations":[{"action":"update", "path":"tracked.txt", "hunks":[{"old_text":"a", "new_text":"b"}]}]});
            assert!(unit
                .check_and_reserve(profile, "apply_patch", &bounded, &workspace)
                .is_none());
        }
    }

    #[test]
    fn no_op_edit_still_spends_the_existing_attempt_budget() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let profile = WorkProfile::resolve(
            ContextCapacity {
                used: 0,
                limit: 8_000,
                provenance: ContextProvenance::CallerSupplied,
            },
            Reliability::Unknown,
            &GranularityConfig::default(),
        );
        let input = serde_json::json!({"path":"missing.py", "old_text":"same", "new_text":"same"});
        let mut unit = WorkUnit::default();
        assert!(unit
            .check_and_reserve(profile, "edit_file", &input, &workspace)
            .is_none());
        assert!(unit.has_mutations());
        assert!(unit
            .check_and_reserve(profile, "edit_file", &input, &workspace)
            .is_some());
    }

    #[test]
    fn separated_material_changes_are_refused_before_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let profile = WorkProfile::resolve(
            ContextCapacity {
                used: 0,
                limit: 8_000,
                provenance: ContextProvenance::CallerSupplied,
            },
            Reliability::Unknown,
            &GranularityConfig::default(),
        );
        let old = "def round_cents(amount):\n    \"\"\"Round to cents.\"\"\"\n    return int(amount * 100) / 100";
        let new = "import math\n\ndef round_cents(amount):\n    \"\"\"Round to cents.\"\"\"\n    return math.floor(amount * 100 + 0.5) / 100";
        for (name, input) in [
            (
                "edit_file",
                serde_json::json!({"path":"money.py", "old_text":old, "new_text":new}),
            ),
            (
                "multi_edit",
                serde_json::json!({"path":"money.py", "edits":[{"old_text":old, "new_text":new}]}),
            ),
            (
                "apply_patch",
                serde_json::json!({"operations":[{"action":"update", "path":"money.py", "hunks":[{"old_text":old, "new_text":new}]}]}),
            ),
        ] {
            let mut unit = WorkUnit::default();
            assert!(
                unit.check_and_reserve(profile, name, &input, &workspace)
                    .is_some(),
                "{name}"
            );
            assert!(
                !unit.has_mutations(),
                "a refused edit must not spend the unit"
            );
            let contiguous = serde_json::json!({"path":"money.py", "old_text":"    return int(amount * 100) / 100", "new_text":"    import math\n    return math.floor(amount * 100 + 0.5) / 100"});
            assert!(unit
                .check_and_reserve(profile, "edit_file", &contiguous, &workspace)
                .is_none());
        }
    }

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

    #[test]
    fn neutral_outcomes_neither_poison_nor_establish_reliability() {
        use localpilot_core::ToolOutcome;
        let mut evidence = CapabilityEvidence::default();
        evidence.observe_verification();
        evidence.observe_verification();
        for _ in 0..10 {
            evidence.observe_input(true);
            evidence.observe_tool_outcome(ToolOutcome::Unusable, false);
            evidence.observe_tool_outcome(ToolOutcome::Ok, false);
            evidence.observe_tool_outcome(ToolOutcome::ReportedFailure, true);
        }
        assert_eq!(evidence.reliability(), Reliability::Unknown);
        for _ in 0..8 {
            evidence.observe_input(true);
            evidence.observe_tool_outcome(ToolOutcome::Ok, true);
        }
        assert_eq!(evidence.reliability(), Reliability::Strong);
        evidence.observe_tool_outcome(ToolOutcome::Unusable, false);
        evidence.observe_tool_outcome(ToolOutcome::ReportedFailure, true);
        assert_eq!(evidence.reliability(), Reliability::Strong);
        evidence.observe_tool_outcome(ToolOutcome::Unusable, true);
        assert_eq!(evidence.reliability(), Reliability::Weak);
        evidence.observe_input(false);
        evidence.observe_tool_outcome(ToolOutcome::Unusable, false);
        assert_eq!(evidence.reliability(), Reliability::Malformed);
    }

    fn unknown_profile(limit: usize) -> WorkProfile {
        WorkProfile::resolve(
            ContextCapacity {
                used: 0,
                limit,
                provenance: ContextProvenance::ResolvedBudget,
            },
            Reliability::Unknown,
            &GranularityConfig::default(),
        )
    }

    fn page(lines: usize) -> String {
        "<p class=\"row\">item one two three four five</p>
"
        .repeat(lines)
    }

    #[test]
    fn a_created_file_has_its_own_allowance_and_its_edits_cost_no_region() {
        for limit in [64_000, 16_000] {
            let dir = tempfile::tempdir().unwrap();
            let workspace = Workspace::new(dir.path()).unwrap();
            let profile = unknown_profile(limit);
            let mut unit = WorkUnit::default();
            // Far above the 80/200 line edit budget, inside the file allowance.
            let write = serde_json::json!({"path":"index.html","content":page(110)});
            assert!(unit
                .check_and_reserve(profile, "write_file", &write, &workspace)
                .is_none());
            std::fs::write(dir.path().join("index.html"), page(110)).unwrap();
            let edit = serde_json::json!({"path":"index.html","old_text":"item one","new_text":"item two"});
            assert!(
                unit.check_and_reserve(profile, "edit_file", &edit, &workspace)
                    .is_none(),
                "the file the unit created can be corrected in the same unit (context {limit})"
            );
            // Another file is still a second file and a second region.
            let other = serde_json::json!({"path":"other.html","content":"x
"});
            assert!(unit
                .check_and_reserve(profile, "write_file", &other, &workspace)
                .is_some());
        }
    }

    #[test]
    fn a_file_above_the_allowance_names_the_limit_it_hit() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let profile = unknown_profile(16_000);
        let write = serde_json::json!({"path":"index.html","content":page(201)});
        let refusal = WorkUnit::default()
            .check_and_reserve(profile, "write_file", &write, &workspace)
            .expect("201 lines exceed the 200-line allowance under 32k context");
        assert!(refusal.message.contains("new-file lines 201/200"));
        assert!(refusal.message.contains("active profile"));
    }

    #[test]
    fn rewriting_a_created_file_replaces_its_cost() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let profile = unknown_profile(16_000);
        let mut unit = WorkUnit::default();
        for _ in 0..3 {
            let write = serde_json::json!({"path":"index.html","content":page(100)});
            assert!(unit
                .check_and_reserve(profile, "write_file", &write, &workspace)
                .is_none());
            std::fs::write(dir.path().join("index.html"), page(100)).unwrap();
        }
    }

    #[test]
    fn an_existing_file_keeps_the_edit_budget_and_one_region() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.html"),
            "a
",
        )
        .unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let profile = unknown_profile(64_000);
        let mut unit = WorkUnit::default();
        let first = serde_json::json!({"path":"a.html","old_text":"a","new_text":"b"});
        assert!(unit
            .check_and_reserve(profile, "edit_file", &first, &workspace)
            .is_none());
        let second = serde_json::json!({"path":"a.html","old_text":"b
","new_text":"b
c
"});
        assert!(
            unit.check_and_reserve(profile, "edit_file", &second, &workspace)
                .is_some(),
            "edits to a tracked file still count one region each"
        );
    }

    #[test]
    fn a_configured_line_cap_also_bounds_a_created_file() {
        let caps = GranularityConfig {
            max_changed_lines: Some(40),
            ..GranularityConfig::default()
        };
        let profile = WorkProfile::resolve(
            ContextCapacity {
                used: 0,
                limit: 64_000,
                provenance: ContextProvenance::ResolvedBudget,
            },
            Reliability::Unknown,
            &caps,
        );
        assert_eq!(profile.max_new_file_lines, 40);
    }
}
