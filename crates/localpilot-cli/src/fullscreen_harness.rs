//! Guided choices orchestrate the shared document stages and existing runner.

use super::*;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum GuideStage {
    BriefDecision(String),
    Resume(Box<ResumeConsent>),
    Options,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResumeConsent {
    progress: String,
    model: String,
    provider: String,
    profile: String,
    trusted: bool,
    execution: ExecutionSnapshot,
}

impl ResumeConsent {
    fn capture(runtime: &SessionRuntime, ctx: &SlashContext<'_>) -> Option<Self> {
        let state = localpilot_harness::inspect(localpilot_harness::WorkspaceInputs::at(ctx.cwd));
        let progress = localpilot_harness::resumable(&state).ok()?;
        Some(Self {
            progress: progress.render(),
            model: runtime.active_model().to_string(),
            provider: runtime.active_provider_id().to_string(),
            profile: crate::repl::ui_profile(runtime.permission_engine_handle().profile())
                .label()
                .to_string(),
            trusted: runtime.trusted(),
            execution: ctx.execution.clone(),
        })
    }
}

pub(super) fn status(app: &mut AppModel, cwd: &Path, stages: &StageHost) {
    let mut out = Vec::new();
    let liveness = if stages.operation_active.get() {
        localpilot_harness::OperationLiveness::Running
    } else {
        localpilot_harness::OperationLiveness::Idle
    };
    let result = crate::harness_cmd::gather_status_with_liveness(cwd, liveness)
        .map(|report| out.extend_from_slice(report.render().as_bytes()));
    let mut output = crate::repl::command_output_from_buffer(out, result);
    output.lines.push(if stages.operation_active.get() {
        "operation: active harness work; /harness-stop signals cancellation".to_string()
    } else if let Some((_, stage)) = &stages.live {
        format!(
            "conversation: {}; no harness operation running",
            stage.subject()
        )
    } else {
        "operation: no active harness work or conversation".to_string()
    });
    present_command_report(app, command_report("harness-status", output));
}

pub(super) fn stop(app: &mut AppModel, stages: &mut StageHost) {
    if stages.live.is_none() {
        app.apply_runtime(RuntimeUpdate::Notice(
            "no active harness operation or conversation".to_string(),
        ));
        return;
    }
    end_stage_conversation(app, stages, "/harness-stop");
    stages.guided = false;
    app.set_shared_mode(localpilot_slash::Mode::Agent);
}

pub(super) fn confirm_resume(
    app: &mut AppModel,
    runtime: &SessionRuntime,
    ctx: &mut SlashContext<'_>,
) -> bool {
    if runtime.is_incognito() {
        app.apply_runtime(RuntimeUpdate::Warning(
            "harness execution writes project files and commits; leave incognito before resuming"
                .to_string(),
        ));
        return false;
    }
    if ctx.stages.brief().is_some() || ctx.stages.plan().is_some() {
        app.apply_runtime(RuntimeUpdate::Warning(
            "finish or cancel the document review before resuming".to_string(),
        ));
        return false;
    }
    let captured = match &ctx.stages.live {
        Some((_, LiveStage::Guide(GuideStage::Resume(consent)))) => Some(consent.as_ref().clone()),
        _ => None,
    };
    let current = ResumeConsent::capture(runtime, ctx);
    if let (Some(captured), Some(current)) = (&captured, &current) {
        if captured == current {
            ctx.stages.end(localpilot_harness::StageOutcome::Approved);
            return true;
        }
        app.apply_runtime(RuntimeUpdate::Warning(
            "execution inputs changed since review; review the updated settings and confirm again"
                .to_string(),
        ));
    }
    offer_resume(app, runtime, ctx);
    false
}

fn offer_resume(app: &mut AppModel, runtime: &SessionRuntime, ctx: &mut SlashContext<'_>) {
    let state = localpilot_harness::inspect(localpilot_harness::WorkspaceInputs::at(ctx.cwd));
    match localpilot_harness::resumable(&state) {
        Ok(progress) => {
            let mut lines = execution_disclosure(
                crate::repl::ui_profile(runtime.permission_engine_handle().profile()).label(),
                &ctx.execution,
                progress,
            );
            lines.push(format!(
                "  provider: {}; model: {}",
                runtime.active_provider_id(),
                runtime.active_model()
            ));
            present_command_report(
                app,
                command_report(
                    "harness execution review",
                    crate::repl::CommandOutput { lines, error: None },
                ),
            );
            if let Some(consent) = ResumeConsent::capture(runtime, ctx) {
                ctx.stages
                    .begin(LiveStage::Guide(GuideStage::Resume(Box::new(consent))));
                app.apply_runtime(RuntimeUpdate::Notice("resume this plan? Reply resume or yes, or run /harness-resume again. Reply no to keep reviewing; /agent leaves. Nothing has started.".to_string()));
            }
        }
        Err(reason) => {
            app.apply_runtime(RuntimeUpdate::Warning(
                crate::harness_cmd::blocked_reason_on(&reason, true),
            ));
            ctx.stages.begin(LiveStage::Guide(GuideStage::Options));
        }
    }
}

pub(super) async fn enter_on<P, R, D>(
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    ctx: &mut SlashContext<'_>,
    queue: &mut VecDeque<QueuedOperation>,
    io: &mut TerminalIo<P, R, D>,
    after_brief: bool,
) -> Result<()>
where
    P: FnMut(Duration) -> io::Result<bool>,
    R: FnMut() -> io::Result<Event>,
    D: FnMut(&AppModel) -> Result<localpilot_terminal_ui::HitMap>,
{
    app.set_shared_mode(localpilot_slash::Mode::Harness);
    ctx.stages.guided = true;
    if !after_brief && ctx.stages.live.is_some() {
        status(app, ctx.cwd, ctx.stages);
        app.apply_runtime(RuntimeUpdate::Notice(
            "continue the active harness conversation, or /harness-stop to leave it".to_string(),
        ));
        return Ok(());
    }
    let state = localpilot_harness::inspect(localpilot_harness::WorkspaceInputs::at(ctx.cwd));
    if !after_brief {
        if let Some(brief) = state.documents.brief() {
            let lines = brief.render().lines().map(str::to_string).collect();
            present_command_report(
                app,
                command_report(
                    "current brief",
                    crate::repl::CommandOutput { lines, error: None },
                ),
            );
            let revision = localpilot_harness::BriefRevision::of(brief)
                .as_str()
                .to_string();
            ctx.stages
                .begin(LiveStage::Guide(GuideStage::BriefDecision(revision)));
            app.apply_runtime(RuntimeUpdate::Notice("does the brief need changes? Reply no to keep it and inspect the plan, or describe the changes. /agent leaves without changing the project.".to_string()));
            return Ok(());
        }
    }
    use localpilot_harness::DocumentState;
    match state.documents {
        DocumentState::NoBrief => {
            drive_harness_intake_on(app, runtime, ctx, queue, io, None).await?
        }
        DocumentState::BriefOnly { .. } => {
            drive_harness_plan_on(
                app,
                runtime,
                ctx,
                queue,
                io,
                PlanCommand::First,
                localpilot_slash::ReviewAction::Show,
            )
            .await?
        }
        DocumentState::PlanStale { .. } => {
            drive_harness_plan_on(
                app,
                runtime,
                ctx,
                queue,
                io,
                PlanCommand::Again,
                localpilot_slash::ReviewAction::Show,
            )
            .await?
        }
        DocumentState::PlanReady { .. } => offer_resume(app, runtime, ctx),
        DocumentState::PlanComplete { .. } => {
            status(app, ctx.cwd, ctx.stages);
            ctx.stages.begin(LiveStage::Guide(GuideStage::Options));
            app.apply_runtime(RuntimeUpdate::Notice("all plan steps are complete. Use /harness-status to inspect evidence, /harness-brief to review requirements, or /harness-feature <description> to extend the approved work. /agent leaves.".to_string()));
        }
        DocumentState::BriefUnreadable(_)
        | DocumentState::BriefMalformed(_)
        | DocumentState::PlanUnreadable { .. }
        | DocumentState::PlanMalformed { .. }
        | DocumentState::PlanUnbound { .. }
        | DocumentState::PlanBindingUnsupported { .. } => {
            status(app, ctx.cwd, ctx.stages);
            ctx.stages.begin(LiveStage::Guide(GuideStage::Options));
            app.apply_runtime(RuntimeUpdate::Notice("repair the named document error, then reply retry to inspect again. No repair or execution was attempted.".to_string()));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn input_on<P, R, D>(
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    ctx: &mut SlashContext<'_>,
    queue: &mut VecDeque<QueuedOperation>,
    io: &mut TerminalIo<P, R, D>,
    generation: StageGeneration,
    text: &str,
) -> Result<bool>
where
    P: FnMut(Duration) -> io::Result<bool>,
    R: FnMut() -> io::Result<Event>,
    D: FnMut(&AppModel) -> Result<localpilot_terminal_ui::HitMap>,
{
    let guide = match &ctx.stages.live {
        Some((current, LiveStage::Guide(guide))) if *current == generation => guide.clone(),
        _ => {
            let reason = ctx
                .stages
                .ending_of(generation)
                .unwrap_or_else(|| "that conversation has ended".to_string());
            app.apply_runtime(RuntimeUpdate::Notice(reason));
            return Ok(false);
        }
    };
    let answer = text.trim();
    match guide {
        GuideStage::BriefDecision(revision) => {
            let state =
                localpilot_harness::inspect(localpilot_harness::WorkspaceInputs::at(ctx.cwd));
            if state
                .documents
                .brief()
                .map(|brief| {
                    localpilot_harness::BriefRevision::of(brief)
                        .as_str()
                        .to_string()
                })
                .as_ref()
                != Some(&revision)
            {
                app.apply_runtime(RuntimeUpdate::Warning(
                    "the brief changed since it was shown; review it again before choosing"
                        .to_string(),
                ));
                ctx.stages.end(localpilot_harness::StageOutcome::Superseded);
                enter_on(app, runtime, ctx, queue, io, false).await?;
            } else if answer.eq_ignore_ascii_case("no") || answer.eq_ignore_ascii_case("unchanged")
            {
                ctx.stages.end(localpilot_harness::StageOutcome::Approved);
                enter_on(app, runtime, ctx, queue, io, true).await?;
            } else if answer.eq_ignore_ascii_case("yes") || answer.is_empty() {
                app.apply_runtime(RuntimeUpdate::Notice(
                    "describe the changes to the brief, or reply no to keep it".to_string(),
                ));
            } else {
                drive_harness_brief_on(
                    app,
                    runtime,
                    ctx,
                    queue,
                    io,
                    localpilot_slash::ReviewAction::Show,
                )
                .await?;
                if let Some(next) = ctx.stages.live_generation() {
                    drive_stage_input_on(
                        app,
                        runtime,
                        ctx,
                        queue,
                        io,
                        next,
                        answer,
                        BeginWork::Own,
                    )
                    .await?;
                }
            }
        }
        GuideStage::Resume(_) => {
            if answer.eq_ignore_ascii_case("yes") || answer.eq_ignore_ascii_case("resume") {
                return Ok(confirm_resume(app, runtime, ctx));
            }
            app.apply_runtime(RuntimeUpdate::Notice("execution has not started. Reply resume to confirm, inspect with /harness-status, or /agent to leave.".to_string()));
        }
        GuideStage::Options => {
            if answer.eq_ignore_ascii_case("retry") {
                ctx.stages.end(localpilot_harness::StageOutcome::Superseded);
                enter_on(app, runtime, ctx, queue, io, false).await?;
                return Ok(false);
            }
            status(app, ctx.cwd, ctx.stages);
            app.apply_runtime(RuntimeUpdate::Notice("use the displayed harness commands or /agent to leave; this input was not sent as an agent task".to_string()));
        }
    }
    Ok(false)
}
