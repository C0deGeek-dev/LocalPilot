//! Bounded unit continuation for a headless task under a conservative work
//! profile: a unit whose full verification fails may still end as a unit
//! checkpoint when its own executed check passes, and the host then opens the
//! next unit. Completion still requires an actual passing full check.

use std::path::Path;

use localpilot_config::CheckConfig;
use localpilot_core::{Message, Role, ToolCall, ToolUseId};
use localpilot_sandbox::Interactivity;
use localpilot_tools::ToolContext;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::{RuntimeEvent, SessionRuntime, StopReason, VerifyGate};
use crate::quality::CheckStatus;
use crate::rules::Trigger;

const MOVED: &str = "unit checkpoint refused: a verification command changed the workspace; the work is preserved for review";
const REFUSAL_STEP: &str =
    "split into a smaller coherent unit, verify it, and checkpoint before continuing";
const UNIT_STEP: &str = "this call was not executed; the harness now checks earlier successful \
     changes and, when the check allows, opens the next unit with the same task. Retry this \
     refused edit in that unit if it is still needed";

const REFUSED_REQUEST_CAP: usize = 1024;

/// Carry declined input as bounded, redacted context, never as a tool call.
fn refused_request_context(name: &str, input: &serde_json::Value) -> String {
    #[derive(serde::Serialize)]
    struct Request<'a> {
        tool: &'a str,
        input: &'a serde_json::Value,
    }
    let mut bytes = [0u8; REFUSED_REQUEST_CAP];
    let mut writer = std::io::Cursor::new(bytes.as_mut_slice());
    if serde_json::to_writer(&mut writer, &Request { tool: name, input }).is_ok() {
        let length = writer.position() as usize;
        let encoded = String::from_utf8_lossy(&bytes[..length]);
        let redacted = localpilot_config::redact::redact(&encoded);
        if redacted.len() <= REFUSED_REQUEST_CAP
            && serde_json::from_str::<serde_json::Value>(&redacted).is_ok()
        {
            return redacted;
        }
    }
    let tool = localpilot_config::redact::redact(&name.chars().take(64).collect::<String>());
    let target = input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(|path| localpilot_config::redact::redact(&path.chars().take(64).collect::<String>()));
    let fallback =
        serde_json::json!({"tool":tool,"target_hint":target,"input_omitted":true}).to_string();
    if fallback.len() <= REFUSED_REQUEST_CAP {
        fallback
    } else {
        "{\"input_omitted\":true}".to_string()
    }
}

#[derive(Default)]
pub(super) struct RenameContinuation {
    pair: Option<super::rename::RenamePair>,
    pub(super) proof: Option<crate::resume::WorkDiffBaseline>,
    pub(super) attempted: bool,
    pub(super) batch_refused: bool,
}

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
    /// Refuse a proven rename spanning too many hunks before any write. A hint
    /// spends no attempt and grants no authority; the normal retry still goes
    /// through permissions, reservation, source inspection and verification.
    pub(super) fn rename_admission_guidance(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<String> {
        use std::io::Read as _;
        let state = self.rename_continuation.as_ref()?;
        let profile = self.active_work_profile?;
        if self.unit_checkpoints_remaining == 0
            || self.work_unit.has_mutations()
            || self.work_mcp_call_attempted
            || self.work_opaque_launch_dispatched
            || self.work_write_error_may_have_run
        {
            return None;
        }
        let (path, old_text, new_text) = match name {
            "write_file" => (
                input.get("path")?.as_str()?,
                None,
                input.get("content")?.as_str()?,
            ),
            "edit_file" => (
                input.get("path")?.as_str()?,
                Some(input.get("old_text")?.as_str()?),
                input.get("new_text")?.as_str()?,
            ),
            "apply_patch" => {
                let operations = input.get("operations")?.as_array()?;
                let [operation] = operations.as_slice() else {
                    return None;
                };
                if operation.get("action")?.as_str()? != "update" {
                    return None;
                }
                let hunks = operation.get("hunks")?.as_array()?;
                let [hunk] = hunks.as_slice() else {
                    return None;
                };
                (
                    operation.get("path")?.as_str()?,
                    Some(hunk.get("old_text")?.as_str()?),
                    hunk.get("new_text")?.as_str()?,
                )
            }
            _ => return None,
        };
        if new_text.len() > super::rename::SOURCE_CAP {
            return None;
        }
        let root = self.workspace.process_dir();
        let normalized = self.workspace.resolve(Path::new(path)).ok()?;
        let relative = normalized
            .strip_prefix(self.workspace.root())
            .ok()?
            .to_str()?
            .replace('\\', "/");
        syntax_check_targets(&root, std::slice::from_ref(&relative), 1)?;
        let call = ToolCall::new(
            ToolUseId::from("rename-admission-read"),
            "read_file",
            serde_json::json!({"path": relative}),
        );
        let ctx = ToolContext {
            workspace: &self.workspace,
            interactivity: self.config.interactivity,
            trusted: self.config.trusted,
            retention: None,
            processes: None,
            agents: None,
            prompter: None,
            peers: None,
        };
        self.tools
            .check_headless_permissions(&call, &ctx, &self.engine.snapshot())
            .ok()?;
        let before =
            crate::resume::work_base_source(&root, self.work_diff_baseline.as_ref()?, &relative)
                .ok()?;
        if before.len() > super::rename::SOURCE_CAP {
            return None;
        }
        let mut current = String::new();
        std::fs::File::open(&normalized)
            .ok()?
            .take(super::rename::SOURCE_CAP as u64 + 1)
            .read_to_string(&mut current)
            .ok()?;
        if current != before || current.len() > super::rename::SOURCE_CAP {
            return None;
        }
        let after = if let Some(old) = old_text {
            if old.is_empty() || before.matches(old).count() != 1 {
                return None;
            }
            before.replacen(old, new_text, 1)
        } else {
            new_text.to_owned()
        };
        let hint = super::rename::first_bounded_edit(
            &before,
            &after,
            state.pair.as_ref(),
            profile.max_regions,
        )?;
        if profile.max_changed_lines < 2 {
            return None;
        }
        let message = match hint {
            super::rename::AdmissionHint::Exact { old, new, regions } => {
                if old
                    .len()
                    .div_ceil(80)
                    .saturating_add(new.len().div_ceil(80))
                    > profile.max_changed_lines
                {
                    return None;
                }
                let retry = serde_json::json!({"tool":"edit_file", "input":{"path":relative,"old_text":old,"new_text":new}}).to_string();
                format!("work envelope: this rename changes {regions} separate regions, but the current unit allows {}. Nothing was written. Start with one exact edit: {retry}. Remaining sites still require bounded verified checkpoints and final full verification.", profile.max_regions)
            }
            super::rename::AdmissionHint::DeclarationFirst { pair, regions } => {
                format!("work envelope: this proposed name change spans {regions} separate regions, but the current unit allows {}. Nothing was written. The first selective unit must rename the function declaration: find {:?} and rename only its declaration to {:?} with one exact edit. Caller changes alone cannot activate continuation. Remaining sites require verified checkpoints and final full verification.", profile.max_regions, pair.old, pair.new)
            }
        };
        (message.len() <= profile.max_output_bytes).then_some(message)
    }

    /// A bounds-refused write ends the unit when a further unit is available
    /// and this unit already holds a mutation; the host settles it after the
    /// current tool batch instead of waiting for the model to stop. Returns
    /// whether the end is now scheduled.
    pub(super) fn request_unit_end(&mut self, name: &str, input: &serde_json::Value) -> bool {
        if self.unit_checkpoints_remaining == 0 || !self.work_unit.has_mutations() {
            return false;
        }
        // An attempt budget is not a change: require the actual workspace to
        // differ from where this unit started.
        let changed = self.work_diff_baseline.as_ref().is_some_and(|before| {
            crate::resume::work_diff_baseline(&self.workspace.process_dir())
                .is_ok_and(|after| &after != before)
        });
        if changed {
            if self.rename_continuation.is_some() && !self.prepare_rename_unit() {
                return false;
            }
            self.unit_end_requested = true;
            self.unit_refused_request = Some(refused_request_context(name, input));
        }
        changed
    }

    fn prepare_rename_unit(&mut self) -> bool {
        let Some(state) = &mut self.rename_continuation else {
            return false;
        };
        if state.attempted {
            return state.proof.is_some();
        }
        state.attempted = true;
        if self.harness_checkpoint_owner
            || self.work_mcp_call_attempted
            || self.work_opaque_launch_dispatched
            || self.work_write_error_may_have_run
            || !self.work_profile().is_some_and(|p| p.stop_after_checkpoint)
        {
            return false;
        }
        let Some(baseline) = self.work_diff_baseline.as_ref() else {
            return false;
        };
        let root = self.workspace.process_dir();
        let Ok(paths) = crate::resume::work_changed_paths(&root, baseline) else {
            return false;
        };
        let Some(paths) = syntax_check_targets(&root, &paths, 1) else {
            return false;
        };
        let path = &paths[0];
        let call = ToolCall::new(
            ToolUseId::from("rename-evidence-read"),
            "read_file",
            serde_json::json!({"path":path}),
        );
        let ctx = ToolContext {
            workspace: &self.workspace,
            interactivity: Interactivity::NonInteractive,
            trusted: self.config.trusted,
            retention: None,
            processes: None,
            agents: None,
            prompter: None,
            peers: None,
        };
        if self
            .tools
            .check_headless_permissions(&call, &ctx, &self.engine.snapshot())
            .is_err()
        {
            return false;
        }
        let Ok(material) = crate::resume::work_diff_baseline(&root) else {
            return false;
        };
        let Ok(before) = crate::resume::work_base_source(&root, baseline, path) else {
            return false;
        };
        use std::io::Read;
        let Ok(file) = std::fs::File::open(root.join(path)) else {
            return false;
        };
        let mut bytes = Vec::new();
        if file
            .take(super::rename::SOURCE_CAP as u64 + 1)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() > super::rename::SOURCE_CAP
        {
            return false;
        }
        let Ok(after) = std::str::from_utf8(&bytes) else {
            return false;
        };
        let Some(evidence) = super::rename::rename_evidence(&before, after) else {
            return false;
        };
        if !crate::resume::work_diff_baseline(&root).is_ok_and(|now| now == material) {
            return false;
        }
        let Some(state) = &mut self.rename_continuation else {
            return false;
        };
        let pair = evidence.pair;
        if state.pair.is_none() && !evidence.declares_function {
            return false;
        }
        if state.pair.as_ref().is_some_and(|old| old != &pair) {
            return false;
        }
        state.pair = Some(pair);
        state.proof = Some(material);
        true
    }

    /// Settle a unit the host ended after a bounds refusal. A passing full
    /// check is not a finished task here: the refused write shows more work
    /// was intended, so it can only become a checkpoint.
    pub(super) async fn settle_host_unit(
        &mut self,
        attempts: &mut usize,
        events: &broadcast::Sender<RuntimeEvent>,
    ) -> Option<StopReason> {
        self.settling_host_unit = true;
        let stop = self.settle_gate(attempts, events).await;
        self.settling_host_unit = false;
        self.unit_refused_request = None;
        stop
    }

    /// Settle a would-be final answer through the verify gate. `Some` ends
    /// the turn with that stop; `None` continues it (failure feedback added).
    pub(super) async fn settle_gate(
        &mut self,
        attempts: &mut usize,
        events: &broadcast::Sender<RuntimeEvent>,
    ) -> Option<StopReason> {
        match self.verify_before_done(attempts, events).await {
            VerifyGate::Finalize => Some(self.stop(events, StopReason::Done)),
            VerifyGate::Retry(feedback) => {
                self.append(Message::text(Role::User, feedback).into_synthetic("verify gate"));
                None
            }
            VerifyGate::GiveUp(detail) => {
                if detail.contains("work envelope:") {
                    self.append(
                        Message::text(
                            Role::System,
                            format!(
                                "System notice from LocalPilot, not user input: bounded work was refused or could not be completed. Preserve the existing work; do not revert it to free work budget. Diagnostic: {detail}"
                            ),
                        )
                        .into_synthetic("bounded unit stop"),
                    );
                } else {
                    self.append(
                        Message::text(Role::User, &detail).into_synthetic("bounded unit stop"),
                    );
                }
                Some(self.stop_with_detail(events, StopReason::NoProgress, Some(detail)))
            }
            VerifyGate::UnitCheckpoint(detail) => {
                self.outstanding_full_check = true;
                let _ = events.send(RuntimeEvent::Warning(detail.clone()));
                Some(self.stop_with_detail(events, StopReason::UnitCheckpoint, Some(detail)))
            }
        }
    }

    /// Replace the generic checkpoint step of a work-envelope refusal with the
    /// unit step only when this refusal actually scheduled the host's unit
    /// end; every other refusal keeps the default text unchanged.
    pub(super) fn unit_refusal_guidance(message: String, unit_ended: bool) -> String {
        if !unit_ended {
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
        let (profile, baseline, before) =
            match self.unit_precheck(root, diff_mutated, before_full_check) {
                Ok(eligible) => eligible,
                Err(attempt) => return attempt,
            };
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
            _ => return UnitAttempt::Stop(MOVED.to_string()),
        }
        self.grant_unit(
            format!(
                "syntax check passed for {}; the full verification still fails",
                files.join(", ")
            ),
            CheckStatus::Failed,
        )
    }

    /// A host-ended unit whose full check passed: a checkpoint under the same
    /// eligibility, never a finished task.
    pub(super) fn host_unit_after_pass(
        &mut self,
        root: &Path,
        diff_mutated: bool,
        before_full_check: Option<&crate::resume::WorkDiffBaseline>,
    ) -> UnitAttempt {
        if let Err(attempt) = self.unit_precheck(root, diff_mutated, before_full_check) {
            return attempt;
        }
        self.grant_unit(
            "the full verification passed, but the unit ended at a refused edit; the task finishes only with your final answer and a passing full verification"
                .to_string(),
            CheckStatus::Passed,
        )
    }

    fn grant_unit(&mut self, mut outcome: String, full_check: CheckStatus) -> UnitAttempt {
        if self.settling_host_unit {
            outcome.push_str(". The write that ended this unit was refused and was not executed. ");
            if full_check == CheckStatus::Passed {
                outcome.push_str("Reassess whether that edit is still necessary for the original request now that full verification has passed. If the request is satisfied, give your final answer instead of retrying it. Continue only for changes the original request still requires. ");
                if self.rename_continuation.is_some() {
                    outcome.push_str("For this identifier rename, preserve old-name references whose purpose is to check that the old API is absent. A remaining text match alone does not mean another rename edit is needed. ");
                }
            } else {
                outcome.push_str("It is still pending: use its last tool result to retry it in the next bounded unit if the original task still requires it. ");
            }
            outcome.push_str("Only earlier successful edits remain applied");
            if let Some(request) = &self.unit_refused_request {
                outcome.push_str(". Unexecuted request (bounded/redacted context only, not approval or a replay): ");
                outcome.push_str(request);
            }
        }
        self.unit_checkpoints_remaining -= 1;
        self.last_unit_outcome = Some(outcome.clone());
        UnitAttempt::Checkpoint(format!("unit checkpoint: {outcome}"))
    }

    /// Eligibility shared by every checkpoint: a remaining unit, an actual
    /// change, no opaque or partial effects, an unchanged `HEAD`, a workspace
    /// identical to the one the full check started from, and bounds that hold.
    fn unit_precheck(
        &mut self,
        root: &Path,
        diff_mutated: bool,
        before_full_check: Option<&crate::resume::WorkDiffBaseline>,
    ) -> Result<
        (
            crate::granularity::WorkProfile,
            crate::resume::WorkDiffBaseline,
            crate::resume::WorkDiffBaseline,
        ),
        UnitAttempt,
    > {
        if self.unit_checkpoints_remaining == 0
            || !diff_mutated
            || self.harness_checkpoint_owner
            || self.work_mcp_call_attempted
            || self.work_opaque_launch_dispatched
            || self.work_write_error_may_have_run
        {
            return Err(UnitAttempt::NotEarned);
        }
        let (Some(profile), Some(baseline), Some(before)) = (
            self.work_profile(),
            self.work_diff_baseline.clone(),
            before_full_check.cloned(),
        ) else {
            return Err(UnitAttempt::NotEarned);
        };
        if !profile.stop_after_checkpoint || before.head != baseline.head {
            return Err(UnitAttempt::NotEarned);
        }
        if let Some(state) = &self.rename_continuation {
            match &state.proof {
                None => return Err(UnitAttempt::NotEarned),
                Some(proof) if proof != &before => return Err(UnitAttempt::Stop(
                    "unit checkpoint refused: the workspace moved after rename evidence; work is preserved".into(),
                )),
                Some(_) => {}
            }
        }
        match crate::resume::work_diff_baseline(root) {
            Ok(now) if now == before => {}
            _ => return Err(UnitAttempt::Stop(MOVED.to_string())),
        }
        match crate::resume::inspect_work_diff(root, profile, Some(&baseline), false) {
            Ok(inspection) if inspection.blocked.is_none() => {}
            Ok(inspection) => {
                return Err(UnitAttempt::Stop(inspection.blocked.unwrap_or_default()));
            }
            Err(error) => {
                return Err(UnitAttempt::Stop(format!(
                    "cannot inspect bounded unit: {error}"
                )));
            }
        }
        Ok((profile, baseline, before))
    }

    /// Run a headless task, continuing in bounded units when
    /// the user enables global units or selective Python rename units. With
    /// both settings off this is exactly [`SessionRuntime::run_turn`].
    pub async fn run_turn_in_units(
        &mut self,
        prompt: &str,
        events: &broadcast::Sender<RuntimeEvent>,
        cancel: &CancellationToken,
    ) -> StopReason {
        let (units, rename_only) = self.config.granularity.as_ref().map_or((0, false), |caps| {
            if caps.headless_units > 0 {
                (
                    caps.headless_units
                        .min(localpilot_config::MAX_HEADLESS_UNITS),
                    false,
                )
            } else {
                (
                    caps.headless_rename_units
                        .min(localpilot_config::MAX_HEADLESS_UNITS),
                    true,
                )
            }
        });
        if units == 0 {
            return self.run_turn(prompt, events, cancel).await;
        }
        self.unit_checkpoints_remaining = units - 1;
        self.rename_continuation = rename_only.then(RenameContinuation::default);
        let mut unit = 1u8;
        let mut reason = self.run_turn(prompt, events, cancel).await;
        while reason == StopReason::UnitCheckpoint && !cancel.is_cancelled() {
            unit += 1;
            let outcome = self.last_unit_outcome.take().unwrap_or_default();
            let note = Message::text(
                    Role::User,
                    format!(
                        "Unit {} of at most {units} is recorded: {}. A checkpoint does not mean the task or any refused edit is complete. If the original request still needs changes, continue with the next bounded unit. Repair only failures your changes caused; report unrelated existing failures instead of fixing them.",
                        unit - 1,
                        outcome
                    ),
                )
                .into_synthetic("unit checkpoint");
            reason = self
                .run_turn_with_followup(prompt, &[], Some(note), events, cancel)
                .await;
        }
        // The obligation stays until an executed full check passes: a resumed
        // turn on this runtime cannot finish unresolved work.
        self.unit_checkpoints_remaining = 0;
        self.rename_continuation = None;
        reason
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_request_context_is_exact_when_small_and_bounded_when_large() {
        let input = serde_json::json!({"path":"money.py","old_text":"y = 1","new_text":"y = 2"});
        let context = refused_request_context("edit_file", &input);
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(parsed["tool"], "edit_file");
        assert_eq!(parsed["input"], input);
        assert!(context.len() <= REFUSED_REQUEST_CAP);

        let large = serde_json::json!({"path":"\u{1}🙂".repeat(512),"content":"x".repeat(65_536)});
        let context = refused_request_context("write_file", &large);
        assert!(context.len() <= REFUSED_REQUEST_CAP);
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(parsed["input_omitted"], true);
        assert!(parsed.get("input").is_none());
    }

    #[test]
    fn refused_request_context_does_not_repeat_credentials() {
        let secret = format!("sk-{}", "a".repeat(40));
        let context = refused_request_context(
            "edit_file",
            &serde_json::json!({"path":"money.py","old_text":secret,"new_text":"replacement"}),
        );
        assert!(!context.contains(&secret));
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(parsed["input"]["new_text"], "replacement");
        assert!(context.len() <= REFUSED_REQUEST_CAP);
    }

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
    fn refusal_guidance_names_the_unit_step_only_when_the_unit_ended() {
        let message = format!("work envelope: too big; {REFUSAL_STEP}");
        let kept = SessionRuntime::unit_refusal_guidance(message.clone(), false);
        assert_eq!(kept, message);
        let replaced = SessionRuntime::unit_refusal_guidance(message, true);
        assert!(replaced.contains("this call was not executed"));
        assert!(!replaced.contains(REFUSAL_STEP));
    }
}
