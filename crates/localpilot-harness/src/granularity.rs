//! Work sizing is independent of permissions, correctness and model names.
//! Only observed, session-local outcomes establish reliability. An active unit
//! may tighten as pressure grows; compaction never refunds its work budget.

use std::collections::{BTreeMap, BTreeSet};

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
            } else if small && matches!(reliability, Reliability::Weak | Reliability::Unknown) {
                2
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
            " Then stop after verification and describe the remaining work; the host owns any checkpoint."
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

/// Unit material plus conservative attempted writes. Successful exact tools
/// may install measured accounting; only typed pre-write match failures refund.
/// Read paths never spend file slots.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkUnit {
    files: BTreeSet<std::path::PathBuf>,
    regions: usize,
    lines: usize,
    measured: BTreeMap<std::path::PathBuf, MeasuredFile>,
    measured_edits_enabled: bool,
}

#[derive(Clone)]
struct MeasuredFile {
    original: std::sync::Arc<str>,
    current: std::sync::Arc<str>,
    normalize_crlf: bool,
    attribute_fingerprint: String,
    regions: usize,
    lines: usize,
}

impl std::fmt::Debug for MeasuredFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeasuredFile")
            .field("regions", &self.regions)
            .field("lines", &self.lines)
            .finish_non_exhaustive()
    }
}

/// Shared ending replaced only when the host actually ends a bounded unit.
pub(crate) const REFUSAL_STEP: &str = "Next step: ";

/// Keep recovery guidance separate from a bounds denial: only the latter
/// prevents an opaque process from retrying the refused mutation.
pub(crate) struct WorkRefusal {
    pub(crate) message: String,
    pub(crate) blocks_opaque_retry: bool,
}

impl WorkUnit {
    pub(crate) fn enable_measured_edits(&mut self, enabled: bool) {
        self.measured_edits_enabled = enabled;
    }

    pub(crate) fn settle_attempt(
        &mut self,
        before: &Self,
        dispatch: &localpilot_tools::ToolDispatchResult,
        accepted: Option<Self>,
    ) {
        if dispatch.edit_precondition_failed || dispatch.work_refusal.is_some() {
            *self = before.clone();
        } else if dispatch.result.is_error() {
            // Denied/partial/unknown writes retain attempted counts. No future
            // correction may discount cached text after a possible mutation.
            self.measured.clear();
        } else if let Some(accepted) = accepted {
            *self = accepted;
        }
    }

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
    fn refusal_message(&self, profile: WorkProfile, reason: &str) -> String {
        let step = if self.has_mutations() {
            "Correct an already admitted exact region without increasing its footprint.".to_string()
        } else if profile.max_regions == 1 {
            "Keep the whole change inside one contiguous region within these limits.".to_string()
        } else {
            format!(
                "Keep the whole change within {} regions and {} files, within these limits.",
                profile.max_regions, profile.max_files
            )
        };
        format!("work envelope: {reason}. Used/allowed: files {}/{}; regions {}/{}; lines {}/{}. {REFUSAL_STEP}{step} If the fix cannot fit, preserve the current work and stop with the remaining work.", self.files.len(), profile.max_files, self.regions, profile.max_regions, self.lines, profile.max_changed_lines)
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
    ) -> Option<WorkRefusal> {
        let reject = |reason: &str| {
            Some(WorkRefusal {
                message: self.refusal_message(profile, reason),
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
            // Regex/global substitution has no argument-derived region bound.
            return Some(WorkRefusal {
                message: self.refusal_message(
                    profile,
                    "use bounded exact edit_file hunks instead of global/regex replacement",
                ),
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
        let credit = if self.measured_edits_enabled && measured_edit_input(name, input) {
            self.measured
                .values()
                .fold((0usize, 0usize), |(regions, lines), file| {
                    (
                        regions.saturating_add(file.regions),
                        lines.saturating_add(file.lines),
                    )
                })
        } else {
            (0, 0)
        };
        proposed.regions = proposed.regions.saturating_sub(credit.0);
        proposed.lines = proposed.lines.saturating_sub(credit.1);
        for op in operations {
            // Sizing must cover every target, including scratch and external
            // paths. Containment belongs to permission checks; a failed
            // containment check must not admit the rest of a mixed patch.
            let path = match workspace.normalize(std::path::Path::new(op.get("path")?.as_str()?)) {
                Ok(path) => path,
                Err(_) => return reject("write target cannot be normalized for bounded sizing"),
            };
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
                for field in ["old_text", "new_text", "content"] {
                    if let Some(text) = hunk.get(field).and_then(Value::as_str) {
                        // Byte-equivalent cost also bounds giant single lines.
                        proposed.lines = proposed
                            .lines
                            .saturating_add(text.lines().count().max(text.len().div_ceil(80)));
                    }
                }
                // Bound material before allocating the argument-only diff. A
                // single submitted hunk can hide edits separated by equal lines.
                if proposed.lines > profile.max_changed_lines {
                    return reject(
                        "cumulative files, regions or patch size exceeds the active profile",
                    );
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
                proposed.regions = proposed.regions.saturating_add(regions);
            }
        }
        if proposed.files.len() > profile.max_files
            || proposed.regions > profile.max_regions
            || proposed.lines > profile.max_changed_lines
        {
            return reject("cumulative files, regions or patch size exceeds the active profile");
        }
        // Keep the attempt spent until the authorized tool either proves a
        // match-precondition miss or successfully installs measured accounting.
        proposed.regions = proposed.regions.saturating_add(credit.0);
        proposed.lines = proposed.lines.saturating_add(credit.1);
        *self = proposed;
        None
    }
}

fn measured_edit_input(name: &str, input: &Value) -> bool {
    match name {
        "edit_file" | "multi_edit" => true,
        "apply_patch" => input
            .get("operations")
            .and_then(Value::as_array)
            .is_some_and(|ops| {
                !ops.is_empty()
                    && ops
                        .iter()
                        .all(|op| op.get("action").and_then(Value::as_str) == Some("update"))
            }),
        _ => false,
    }
}

/// Per-call host guard. The exact tool owns preparation and invokes this only
/// after all permissions, immediately before committing its prepared batch.
pub(crate) struct ExactEditGuard {
    before: WorkUnit,
    profile: WorkProfile,
    workspace: Workspace,
    baseline: Option<crate::resume::WorkDiffBaseline>,
    name: String,
    input: Value,
    accepted: std::sync::Mutex<Option<WorkUnit>>,
}

impl ExactEditGuard {
    pub(crate) fn new(
        before: WorkUnit,
        profile: WorkProfile,
        workspace: Workspace,
        baseline: Option<crate::resume::WorkDiffBaseline>,
        name: &str,
        input: &Value,
    ) -> Self {
        Self {
            before,
            profile,
            workspace,
            baseline,
            name: name.into(),
            input: input.clone(),
            accepted: std::sync::Mutex::new(None),
        }
    }

    pub(crate) fn accepted(&self) -> Option<WorkUnit> {
        self.accepted.lock().ok()?.clone()
    }

    fn project(&self, edits: &[localpilot_tools::PreparedEdit<'_>]) -> Option<WorkUnit> {
        if !self.before.measured_edits_enabled || !measured_edit_input(&self.name, &self.input) {
            return None;
        }
        let baseline = self.baseline.as_ref()?;
        let root = self.workspace.root();
        let mut next = self.before.clone();
        for edit in edits {
            const MAX_TEXT: usize = 1024 * 1024;
            if edit.before.len() > MAX_TEXT
                || edit.after.len() > MAX_TEXT
                || edit.before.contains('\0')
                || edit.after.contains('\0')
            {
                return None;
            }
            let relative = edit
                .path
                .strip_prefix(root)
                .ok()?
                .to_str()?
                .replace('\\', "/");
            if crate::resume::path_has_link(root, &relative).ok()? {
                return None;
            }
            let attribute_fingerprint =
                crate::resume::plain_edit_policy(root, &relative).ok()??;
            if [edit.before, edit.after].iter().any(|text| {
                text.chars()
                    .any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
            }) {
                return None;
            }
            if [edit.before, edit.after]
                .iter()
                .any(|text| text.replace("\r\n", "\n").contains('\r'))
                || (!edit.before.contains("\r\n") && edit.after.contains("\r\n"))
            {
                return None;
            }
            let previous = next.measured.get(edit.path);
            let (original, normalize_crlf) = if let Some(previous) = previous {
                if previous.current.as_ref() != edit.before
                    || previous.attribute_fingerprint != attribute_fingerprint
                {
                    return None;
                }
                (previous.original.clone(), previous.normalize_crlf)
            } else {
                // Existing dirty material is conservatively sized by the final
                // observer; it is not a baseline for admission credit.
                if baseline.diff_base.is_none() && baseline.files.contains_key(&relative) {
                    return None;
                }
                let original = crate::resume::original_edit_text(root, baseline, &relative).ok()?;
                // The actual Git comparison is empty at this base. Infer only
                // its built-in CRLF normalization, never a custom transform.
                let normalize = if original == edit.before {
                    false
                } else if original == edit.before.replace("\r\n", "\n") {
                    true
                } else {
                    return None;
                };
                (std::sync::Arc::from(original), normalize)
            };
            let after = if normalize_crlf {
                edit.after.replace("\r\n", "\n")
            } else {
                edit.after.to_owned()
            };
            let size = crate::resume::projected_text_size(root, &original, &after).ok()?;
            if size.regions == 0 && previous.is_none() {
                return None;
            }
            if let Some(previous) = previous {
                next.regions = next.regions.saturating_sub(previous.regions);
                next.lines = next.lines.saturating_sub(previous.lines);
            }
            let lines = size.lines.max(size.material);
            next.regions = next.regions.saturating_add(size.regions);
            next.lines = next.lines.saturating_add(lines);
            next.files.insert(edit.path.to_owned());
            next.measured.insert(
                edit.path.to_owned(),
                MeasuredFile {
                    original,
                    current: std::sync::Arc::from(edit.after),
                    normalize_crlf,
                    attribute_fingerprint,
                    regions: size.regions,
                    lines,
                },
            );
        }
        Some(next)
    }
}

impl localpilot_tools::FileEditGuard for ExactEditGuard {
    fn check(
        &self,
        edits: &[localpilot_tools::PreparedEdit<'_>],
    ) -> Result<(), localpilot_tools::ToolError> {
        let proposed = if let Some(proposed) = self.project(edits) {
            proposed
        } else {
            let mut conservative = self.before.clone();
            conservative.enable_measured_edits(false);
            if let Some(refusal) = conservative.check_and_reserve(
                self.profile,
                &self.name,
                &self.input,
                &self.workspace,
            ) {
                return Err(localpilot_tools::ToolError::WorkEnvelope(refusal.message));
            }
            return Ok(());
        };
        if proposed.files.len() > self.profile.max_files
            || proposed.regions > self.profile.max_regions
            || proposed.lines > self.profile.max_changed_lines
        {
            return Err(localpilot_tools::ToolError::WorkEnvelope(
                self.before.refusal_message(
                    self.profile,
                    "cumulative files, regions or patch size exceeds the active profile",
                ),
            ));
        }
        let mut accepted = self.accepted.lock().map_err(|_| {
            localpilot_tools::ToolError::WorkEnvelope(
                "work envelope: prepared edit accounting unavailable".into(),
            )
        })?;
        *accepted = Some(proposed);
        Ok(())
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
    fn scratch_and_external_targets_spend_the_same_mutation_budget() {
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
            &GranularityConfig {
                max_regions: Some(1),
                ..GranularityConfig::default()
            },
        );
        for path in [scratch, external] {
            let mut unit = WorkUnit::default();
            let input = serde_json::json!({"path":path, "content":"small\n"});
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
        }
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
            &GranularityConfig {
                max_regions: Some(1),
                ..GranularityConfig::default()
            },
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
            &GranularityConfig {
                max_regions: Some(1),
                ..GranularityConfig::default()
            },
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
            &GranularityConfig {
                max_regions: Some(1),
                ..GranularityConfig::default()
            },
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
    fn two_regions_are_limited_to_weak_or_unknown_small_profiles() {
        for reliability in [
            Reliability::Unknown,
            Reliability::Weak,
            Reliability::Malformed,
            Reliability::Strong,
        ] {
            for (limit, used, provenance, small) in [
                (8_000, 0, ContextProvenance::CallerSupplied, true),
                (128_000, 0, ContextProvenance::CallerSupplied, false),
                (128_000, 90_000, ContextProvenance::RuntimeUsage, true),
                (128_000, 0, ContextProvenance::Unknown, true),
            ] {
                let context = ContextCapacity {
                    used,
                    limit,
                    provenance,
                };
                let profile =
                    WorkProfile::resolve(context, reliability, &GranularityConfig::default());
                let expected = match (reliability, small) {
                    (Reliability::Strong, false) => 4,
                    (Reliability::Malformed, _) => 1,
                    (_, true) => 2,
                    _ => 1,
                };
                assert_eq!(
                    profile.max_regions, expected,
                    "{reliability:?}, small={small}"
                );
                assert_eq!(
                    profile.max_files,
                    if reliability == Reliability::Strong && !small {
                        3
                    } else {
                        1
                    }
                );
                assert_eq!(profile.max_changed_lines, if small { 80 } else { 200 });
                assert_eq!(profile.max_read_lines, if small { 80 } else { 200 });
                let capped = WorkProfile::resolve(
                    context,
                    reliability,
                    &GranularityConfig {
                        max_regions: Some(1),
                        ..GranularityConfig::default()
                    },
                );
                assert_eq!(capped.max_regions, 1);
                let mut active = capped;
                active.tighten(profile);
                assert_eq!(active.max_regions, 1);
                let mut scoped = profile;
                scoped.bound_to_scope(WorkScope {
                    files: 1,
                    regions: 1,
                    decisions: 1,
                    changed_lines: 10,
                });
                assert_eq!(scoped.max_regions, 1);
            }
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
}
