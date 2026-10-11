//! Bounded unit continuation for a headless task under a conservative work
//! profile: a unit whose full verification fails may still end as a unit
//! checkpoint when its own executed check passes, and the host then opens the
//! next unit. Completion still requires an actual passing full check.

use std::path::Path;

use localpilot_config::CheckConfig;
use localpilot_core::{Message, Role};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::{RuntimeEvent, SessionRuntime, StopReason};
use crate::quality::CheckStatus;
use crate::rules::Trigger;

const REFUSAL_STEP: &str =
    "split into a smaller coherent unit, verify it, and checkpoint before continuing";
const UNIT_STEP: &str = "stop editing and end this unit now: give your final answer naming \
     the changes that remain; the harness checks this unit and opens the next one with the \
     same task";

/// A host-owned syntax check for one root-relative Python source file. It
/// parses with the standard library in isolated mode and cannot import a
/// project module, so it never certifies that the task is complete.
#[must_use]
pub fn python_syntax_check(root: &Path, source: &str) -> Option<CheckConfig> {
    let mut check = crate::resolve_verify_check(
        root,
        Some(if cfg!(windows) {
            "python -I -m ast"
        } else {
            "python3 -I -m ast"
        }),
    )?;
    check.args.extend(["--".to_owned(), source.to_owned()]);
    Some(check)
}

/// The unit's changed paths when every one is an existing regular Python
/// source file inside the workspace, reached without links; `None` otherwise.
fn syntax_check_targets(root: &Path, changed: &[String], max_files: usize) -> Option<Vec<String>> {
    if changed.is_empty() || changed.len() > max_files {
        return None;
    }
    for path in changed {
        let relative = Path::new(path);
        let python = relative.extension().is_some_and(|ext| ext == "py");
        let plain = relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)));
        if !python || !plain || crate::resume::path_has_link(root, path).ok()? {
            return None;
        }
        let meta = std::fs::symlink_metadata(root.join(relative)).ok()?;
        if !meta.file_type().is_file() {
            return None;
        }
    }
    Some(changed.to_vec())
}

/// What a unit earned after its full check failed.
pub(super) enum UnitAttempt {
    /// Every unit check ran and passed on an unchanged workspace.
    Checkpoint(String),
    /// The unit is ineligible or its own check failed: the normal failed-check
    /// path continues.
    NotEarned,
    /// A check changed the workspace, the unit exceeds its bounds, or a unit
    /// check could not run (denied or errored): the task stops here.
    Stop(String),
}

impl SessionRuntime {
    /// Replace the generic checkpoint step of a work-envelope refusal with the
    /// step this host actually offers, while a further unit is available.
    pub(super) fn unit_refusal_guidance(&self, message: String) -> String {
        if self.unit_checkpoints_remaining == 0 {
            return message;
        }
        message.replace(REFUSAL_STEP, UNIT_STEP)
    }

    /// Run the unit's own check after its full check failed. A checkpoint needs
    /// an eligible unit, a workspace (`HEAD` and every material path) identical
    /// before the full check, after it and after the unit check, bounds that
    /// still hold, and every unit check run and passed.
    pub(super) async fn try_unit_checkpoint(
        &mut self,
        root: &Path,
        diff_mutated: bool,
        before_full_check: Option<&crate::resume::WorkDiffBaseline>,
    ) -> UnitAttempt {
        if self.unit_checkpoints_remaining == 0
            || !diff_mutated
            || self.harness_checkpoint_owner
            || self.work_mcp_call_attempted
            || self.work_opaque_launch_dispatched
            || self.work_write_error_may_have_run
        {
            return UnitAttempt::NotEarned;
        }
        let (Some(profile), Some(baseline), Some(before)) = (
            self.work_profile(),
            self.work_diff_baseline.clone(),
            before_full_check.cloned(),
        ) else {
            return UnitAttempt::NotEarned;
        };
        if !profile.stop_after_checkpoint || before.head != baseline.head {
            return UnitAttempt::NotEarned;
        }
        let moved = "unit checkpoint refused: a verification command changed the workspace; the work is preserved for review";
        match crate::resume::work_diff_baseline(root) {
            Ok(now) if now == before => {}
            _ => return UnitAttempt::Stop(moved.to_string()),
        }
        match crate::resume::inspect_work_diff(root, profile, Some(&baseline), false) {
            Ok(inspection) if inspection.blocked.is_none() => {}
            Ok(inspection) => {
                return UnitAttempt::Stop(inspection.blocked.unwrap_or_default());
            }
            Err(error) => {
                return UnitAttempt::Stop(format!("cannot inspect bounded unit: {error}"));
            }
        }
        let Ok(changed) = crate::resume::work_changed_paths(root, &baseline) else {
            return UnitAttempt::NotEarned;
        };
        let Some(files) = syntax_check_targets(root, &changed, profile.max_files) else {
            return UnitAttempt::NotEarned;
        };
        let Some(checks) = files
            .iter()
            .map(|path| python_syntax_check(root, path))
            .collect::<Option<Vec<_>>>()
        else {
            return UnitAttempt::NotEarned;
        };
        let outcomes = self
            .run_gate_checks(&checks, Trigger::PhaseComplete, root)
            .await;
        if outcomes.len() != checks.len() {
            return UnitAttempt::Stop("unit check did not run".to_string());
        }
        if let Some(outcome) = outcomes
            .iter()
            .find(|o| matches!(o.status, CheckStatus::Denied | CheckStatus::Errored))
        {
            // A unit check that could not run is the actual verifier's result,
            // not an ineligible unit: record it and stop.
            self.turn_verification = Some(outcome.status);
            return UnitAttempt::Stop(format!(
                "unit check {:?}: {}; the unit remains unverified; restore the verifier and resume",
                outcome.status, outcome.detail
            ));
        }
        if outcomes.iter().any(|o| o.status != CheckStatus::Passed) {
            return UnitAttempt::NotEarned;
        }
        match crate::resume::work_diff_baseline(root) {
            Ok(after) if after == before => {}
            _ => return UnitAttempt::Stop(moved.to_string()),
        }
        self.unit_checkpoints_remaining -= 1;
        UnitAttempt::Checkpoint(format!(
            "unit checkpoint: syntax check passed for {}; the full verification still fails, so the task is not complete",
            files.join(", ")
        ))
    }

    /// Run a headless task, continuing in bounded units when
    /// `[harness.granularity] headless_units` allows it. With the setting off
    /// this is exactly [`SessionRuntime::run_turn`].
    pub async fn run_turn_in_units(
        &mut self,
        prompt: &str,
        events: &broadcast::Sender<RuntimeEvent>,
        cancel: &CancellationToken,
    ) -> StopReason {
        let units = self.config.granularity.as_ref().map_or(0, |caps| {
            caps.headless_units
                .min(localpilot_config::MAX_HEADLESS_UNITS)
        });
        if units == 0 {
            return self.run_turn(prompt, events, cancel).await;
        }
        self.unit_checkpoints_remaining = units - 1;
        let mut unit = 1u8;
        let mut reason = self.run_turn(prompt, events, cancel).await;
        while reason == StopReason::UnitCheckpoint && !cancel.is_cancelled() {
            unit += 1;
            self.append(
                Message::text(
                    Role::User,
                    format!(
                        "Unit {} of at most {units} is recorded: its syntax check passed, but the full verification still fails. Continue the same task with the next bounded unit. The task is complete only when the full verification passes.",
                        unit - 1
                    ),
                )
                .into_synthetic("unit checkpoint"),
            );
            reason = self.run_turn(prompt, events, cancel).await;
        }
        // The obligation stays until an executed full check passes: a resumed
        // turn on this runtime cannot finish unresolved work.
        self.unit_checkpoints_remaining = 0;
        reason
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_contained_regular_python_files_are_unit_checked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "x\n").unwrap();
        std::fs::create_dir(dir.path().join("pkg")).unwrap();
        let root = dir.path();
        let one = |paths: &[&str], max| {
            let changed: Vec<String> = paths.iter().map(|p| (*p).to_string()).collect();
            syntax_check_targets(root, &changed, max)
        };
        assert_eq!(one(&["a.py"], 1), Some(vec!["a.py".to_string()]));
        assert_eq!(one(&[], 1), None, "an unchanged unit earns nothing");
        assert_eq!(one(&["a.py", "b.txt"], 2), None, "mixed material");
        assert_eq!(one(&["b.txt"], 1), None);
        assert_eq!(one(&["gone.py"], 1), None, "a deletion");
        assert_eq!(one(&["pkg"], 1), None, "not a regular file");
        assert_eq!(one(&["../a.py"], 1), None, "outside the workspace");
        assert_eq!(one(&["a.py", "a.py"], 1), None, "over the file cap");
    }

    #[test]
    fn refusal_guidance_names_the_unit_step_only_while_a_unit_remains() {
        let message = format!("work envelope: too big; {REFUSAL_STEP}");
        assert!(message.contains(REFUSAL_STEP));
        let replaced = message.replace(REFUSAL_STEP, UNIT_STEP);
        assert!(replaced.contains("end this unit now"));
        assert!(!replaced.contains(REFUSAL_STEP));
    }
}
