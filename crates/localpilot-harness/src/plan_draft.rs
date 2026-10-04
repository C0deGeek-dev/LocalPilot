//! A plan that exists only in this session, until someone approves it.
//!
//! The host-neutral half of interactive planning, mirroring [`crate::intake`]:
//! drafting and revising touch nothing, and one function writes. The CLI's
//! `harness plan` drives the same API, so there is one planning contract rather
//! than a command and a conversation that drift apart.
//!
//! A draft carries the brief revision it was generated against. Approval binds
//! the plan to exactly that revision — not to whatever `brief.md` says by then —
//! because the criterion numbers a reviewer approved must mean what they meant
//! when the decision was made.

use localpilot_core::{Message, Role};
use localpilot_llm::ModelProvider;

use crate::brief::Brief;
use crate::error::HarnessError;
use crate::plan_reconcile::ReconcileConflict;
use crate::plan_review::{validate_for_approval, PlanDefect};
use crate::planning::{generate, PLANNER_PROMPT};
use crate::progress::{Progress, Step};

/// A plan that has not replaced anything yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDraft {
    /// The validated plan as it currently stands.
    pub progress: Progress,
    /// The brief revision this plan was drafted against, and the one approval
    /// will bind it to.
    pub brief_revision: String,
    /// The repository summary the model was given, kept so a revision runs
    /// against the same context the first draft had.
    pub repo_summary: String,
    /// How many accepted revisions this draft has been through.
    pub revisions: usize,
    pub work_profile: Option<crate::granularity::WorkProfile>,
}

/// How an approval failed.
///
/// Separated from a generic error because the two call for different things: a
/// plan that does not satisfy its brief is a conversation to continue, and a
/// plan that could not be written is a filesystem problem to fix.
#[derive(Debug, thiserror::Error)]
pub enum PlanApproval {
    /// The requirements or saved plan changed after this review started.
    #[error("{0} changed during review; start a new planning conversation before approving")]
    SourceChanged(&'static str),
    /// A draft cannot invent or restate the evidence of completed work.
    #[error(
        "the draft changes recorded completion evidence; replan and review the reconciled plan"
    )]
    HistoryChanged,
    /// The draft is not fit to replace the project's plan, and why.
    #[error("the plan does not satisfy the brief: {}", .0.iter().map(std::string::ToString::to_string).collect::<Vec<_>>().join("; "))]
    NotSatisfied(Vec<PlanDefect>),
    /// Nothing was written; the project is exactly as it was.
    #[error("PROGRESS.md was not written: {0}")]
    NotWritten(#[source] HarnessError),
}

/// Where a planning conversation currently is.
///
/// Mirrors [`crate::BriefStage`], including the states where a model call is in
/// flight: a host has to attribute input submitted *while the model is thinking*
/// to the conversation that was thinking, and it cannot do that from a state
/// that does not exist.
///
/// There is no `AwaitingIdea` counterpart. A plan is drafted from an approved
/// brief, so the conversation starts with everything it needs; there is nothing
/// to ask for first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanStage {
    /// A first draft is being generated.
    Generating,
    /// A draft is on screen, awaiting discussion, revision, or a decision.
    Reviewing(Box<PlanDraft>),
    /// A revision of the shown draft is being generated.
    Revising(Box<PlanDraft>),
    /// A redraft that cannot be squared with finished work without a person
    /// deciding. The draft survives: the conflicts are about how it meets the
    /// old plan, and the answer may be to revise it rather than to abandon it.
    Conflicted {
        draft: Box<PlanDraft>,
        conflicts: Vec<ReconcileConflict>,
    },
    /// An attempt failed and the work survives.
    ///
    /// A provider error, a malformed reply, or an exhausted repair budget is a
    /// machine failure, not a decision, so it must not throw away the
    /// conversation. The stage stays live and carries enough to run the failed
    /// attempt again.
    RecoverableFailure { retry: PlanRetry, detail: String },
}

/// The attempt a recoverable failure can repeat.
///
/// Each variant carries its own inputs, so a retry reruns what actually failed
/// rather than something reassembled from whatever happened to survive. In
/// particular the brief travels with the retry: re-reading `brief.md` at retry
/// time would silently plan against a different brief than the one that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanRetry {
    /// First planning from the brief.
    Draft {
        brief: Box<Brief>,
        brief_revision: String,
        repo_summary: String,
        work_profile: Option<crate::granularity::WorkProfile>,
    },
    /// Redrafting around work that is already finished.
    Replan {
        brief: Box<Brief>,
        brief_revision: String,
        repo_summary: String,
        work_profile: Option<crate::granularity::WorkProfile>,
        completed: Vec<Step>,
    },
    /// Revising the draft on screen.
    Revise {
        draft: Box<PlanDraft>,
        instruction: String,
    },
}

/// The instruction that turns one plan into a revised one.
const REVISE_PROMPT: &str = "\
You are revising an implementation plan. You are given the current plan and one \
instruction describing what to change.\n\
\n\
Apply exactly that instruction. Leave every step the instruction does not \
mention exactly as it is — same wording, same order, same numbering, same \
metadata. Do not renumber steps that did not change, and do not drop the \
covers, verify or depends lines of steps you are not changing.\n\
\n\
Respond with ONLY the complete revised Markdown plan, in the same shape as the \
one you were given, and nothing else.";

/// The instruction that redrafts a plan around work that is already done.
const REPLAN_PROMPT: &str = "\
You are replanning a software project. The brief has changed, or the existing \
plan was rejected. Some steps are already finished and committed.\n\
\n\
Reproduce every finished step first, in the order given, with its wording \
character for character. Do not reword, merge, split or reorder them — their \
wording is how the finished work is recognised, and changing it loses the \
commit behind it. Then plan the remaining work as new steps after them, \
numbering the whole plan from 1 with no gaps.\n\
\n\
Do not credit a finished step with an acceptance criterion it does not already \
carry. The brief has moved; those commits were never checked against the new \
criteria, and claiming otherwise is the one thing a replan must never do. Where \
changed criteria need work that a finished step appears to cover, write a new \
step for it.\n\
\n\
Respond with ONLY the complete Markdown plan, in the shape you were given, and \
nothing else.";

/// What an automatic work profile adds to a planning request.
///
/// The planner and replan prompts show a step with three metadata lines and say
/// every step carries "all three". A model told both that and "declare scope" in
/// a sentence that never says where, follows the template. So this message
/// restates the shape — the line, where it goes, a whole step — and says it
/// overrides the template. It is sent after the template, not before it, so the
/// last word on the shape is the right one.
fn scope_addendum(profile: crate::granularity::WorkProfile, tail: &str) -> Message {
    let line = crate::plan_review::scope_line_example(profile);
    Message::text(
        Role::System,
        format!(
            "{instruction}\n\n\
Because work is sized in advance, a step gains a fourth metadata line, and this \
overrides 'all three metadata lines'. Every step that is not finished carries a \
scope line of its own, indented like the other metadata lines, between verify and \
depends:\n\
\n\
- [ ] 1. <small, verifiable step>\n  \
- covers: AC1\n  \
- verify: <command>\n\
{line}\n  \
- depends: none\n\
\n\
A scope is four integers separated by commas: files, regions, decisions, \
changed_lines. Decisions is at least 1, and regions is at least files. A step may \
touch at most {files} files, {regions} regions, {decisions} decisions and \
{lines} changed lines. Split anything larger into several steps, keeping every \
acceptance criterion covered and every dependency pointing at an earlier step.\n\
{tail}",
            instruction = profile.instruction(),
            files = profile.max_files,
            regions = profile.max_regions,
            decisions = profile.max_decisions,
            lines = profile.max_changed_lines,
        ),
    )
}

/// How many times a plan that parses but cannot be approved is sent back.
const PLAN_REPAIRS: usize = 2;

/// The defects that would stop `plan` being approved: criteria without an owner
/// or with metadata missing (when the brief is at hand to judge them against),
/// then the work envelope. Criteria come first because splitting a step to fit
/// the envelope is where a criterion most easily loses its owner.
fn repairable_defects(
    plan: &Progress,
    profile: crate::granularity::WorkProfile,
    brief: Option<&Brief>,
) -> Vec<PlanDefect> {
    let mut defects = brief
        .and_then(|brief| validate_for_approval(plan, brief).err())
        .unwrap_or_default();
    defects.extend(
        crate::plan_review::validate_work_scope(plan, profile)
            .err()
            .unwrap_or_default(),
    );
    defects
}

/// Generate a plan and, when a work profile applies, send it back with the
/// defects that would block its approval until there are none or the repair
/// budget is spent.
///
/// A plan is already retried when it does not parse; this is the same idea one
/// level up, for a plan that parses but is missing a scope line, declares a step
/// too large, or leaves an acceptance criterion without an owner. The budget is
/// separate from the parse retries so a malformed reply cannot use up the chance
/// to fix an oversized one. `brief` is given only for a fresh draft: a revision
/// has no brief to judge criteria against, and a replan carries finished steps
/// whose metadata is not the model's to change.
///
/// Running out of budget is not an error. The last plan is returned with its
/// defects intact, because a reviewer who can see a nearly-right draft and say
/// "split step 1" is better served than one handed a refusal. Likewise a repair
/// reply that cannot be parsed falls back to the plan before it: the repair is
/// best effort and must not lose a plan that was usable.
async fn generate_fitted(
    provider: &dyn ModelProvider,
    model: &str,
    seed: Vec<Message>,
    work_profile: Option<crate::granularity::WorkProfile>,
    brief: Option<&Brief>,
) -> Result<Progress, HarnessError> {
    let mut messages = seed;
    let mut progress = generate(
        provider,
        model,
        messages.clone(),
        "PROGRESS.md",
        Progress::parse,
    )
    .await?;
    let Some(profile) = work_profile else {
        return Ok(progress);
    };
    for _ in 0..PLAN_REPAIRS {
        let defects = repairable_defects(&progress, profile, brief);
        if defects.is_empty() {
            break;
        }
        let listed = defects
            .iter()
            .map(|defect| format!("- {defect}"))
            .collect::<Vec<_>>()
            .join("\n");
        messages.push(Message::text(Role::Assistant, progress.render()));
        messages.push(Message::text(
            Role::User,
            format!(
                "That plan cannot be approved yet:\n{listed}\n\n\
Reply again with ONLY the complete corrected Markdown plan. Fix exactly these \
defects: give every acceptance criterion at least one step that covers it, add or \
correct scope lines, and split any oversized step into several, renumbering from \
1 with no gaps. Keep every dependency pointing at an earlier step, and leave \
everything else as it is."
            ),
        ));
        match generate(
            provider,
            model,
            messages.clone(),
            "PROGRESS.md",
            Progress::parse,
        )
        .await
        {
            Ok(next) => progress = next,
            Err(_) => break,
        }
    }
    Ok(progress)
}

/// Draft a plan from an approved brief, writing nothing.
///
/// # Errors
/// Returns [`HarnessError::Provider`] when the provider fails or never produces
/// a valid plan within the repair budget.
pub async fn draft_plan(
    provider: &dyn ModelProvider,
    model: &str,
    brief: &Brief,
    brief_revision: &str,
    repo_summary: &str,
) -> Result<PlanDraft, HarnessError> {
    draft_plan_with_profile(provider, model, brief, brief_revision, repo_summary, None).await
}

/// Generate using a captured automatic work envelope.
///
/// # Errors
/// Returns a provider or document validation error.
pub async fn draft_plan_with_profile(
    provider: &dyn ModelProvider,
    model: &str,
    brief: &Brief,
    brief_revision: &str,
    repo_summary: &str,
    work_profile: Option<crate::granularity::WorkProfile>,
) -> Result<PlanDraft, HarnessError> {
    let user = format!(
        "Project brief:\n\n{}\n\nRepository summary:\n\n{repo_summary}",
        brief.render()
    );
    let mut seed = vec![Message::text(Role::System, PLANNER_PROMPT)];
    if let Some(profile) = work_profile {
        seed.push(scope_addendum(profile, ""));
    }
    seed.push(Message::text(Role::User, user));
    let mut progress = generate_fitted(provider, model, seed, work_profile, Some(brief)).await?;
    progress.bind_to_brief(brief_revision);
    Ok(PlanDraft {
        progress,
        brief_revision: brief_revision.to_string(),
        repo_summary: repo_summary.to_string(),
        revisions: 0,
        work_profile,
    })
}

/// Draft a replacement plan around work that is already finished.
///
/// The model is given the current brief and the completed steps verbatim, and
/// nothing else about the old plan: unfinished steps were written for a brief
/// that has since moved, and feeding them back is how a replan turns into a
/// reshuffle of stale intent.
///
/// The result still has to be reconciled — see [`crate::reconcile`] — because a
/// prompt is an instruction, not a guarantee, and a model that drops or reworks
/// a finished step must be caught rather than trusted.
///
/// # Errors
/// Returns [`HarnessError::Provider`] when the provider fails or never produces
/// a valid plan within the repair budget.
pub async fn draft_replan(
    provider: &dyn ModelProvider,
    model: &str,
    brief: &Brief,
    brief_revision: &str,
    repo_summary: &str,
    completed: &[Step],
) -> Result<PlanDraft, HarnessError> {
    draft_replan_with_profile(
        provider,
        model,
        brief,
        brief_revision,
        repo_summary,
        completed,
        None,
    )
    .await
}

/// Generate using a captured automatic work envelope.
///
/// # Errors
/// Returns a provider or document validation error.
pub async fn draft_replan_with_profile(
    provider: &dyn ModelProvider,
    model: &str,
    brief: &Brief,
    brief_revision: &str,
    repo_summary: &str,
    completed: &[Step],
    work_profile: Option<crate::granularity::WorkProfile>,
) -> Result<PlanDraft, HarnessError> {
    let finished = if completed.is_empty() {
        "None; nothing has been completed yet.".to_string()
    } else {
        completed
            .iter()
            .map(render_completed)
            .collect::<Vec<_>>()
            .join("")
    };
    let user = format!(
        "Project brief:\n\n{}\n\nRepository summary:\n\n{repo_summary}\n\n\
Finished steps, to reproduce verbatim:\n\n{finished}",
        brief.render()
    );
    let mut seed = vec![
        Message::text(Role::System, PLANNER_PROMPT),
        Message::text(Role::System, REPLAN_PROMPT),
    ];
    if let Some(profile) = work_profile {
        seed.push(scope_addendum(
            profile,
            "Finished steps are reproduced exactly as given and carry no scope line.\n",
        ));
    }
    seed.push(Message::text(Role::User, user));
    let mut progress = generate_fitted(provider, model, seed, work_profile, None).await?;
    progress.bind_to_brief(brief_revision);
    Ok(PlanDraft {
        progress,
        brief_revision: brief_revision.to_string(),
        repo_summary: repo_summary.to_string(),
        revisions: 0,
        work_profile,
    })
}

/// One finished step as the replan prompt shows it.
///
/// Its own `covers` goes in so the model can see what the step was credited
/// with and leave it alone; `commit` and `attempts` do not, because they are
/// evidence reconciliation carries across and not something the model should be
/// inventing or echoing.
fn render_completed(step: &Step) -> String {
    let covers = step.covers.as_ref().map_or_else(
        || "  - covers: not stated\n".to_string(),
        |covers| {
            if covers.is_empty() {
                "  - covers: none\n".to_string()
            } else {
                format!(
                    "  - covers: {}\n",
                    covers
                        .iter()
                        .map(|number| format!("AC{number}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        },
    );
    format!("- [x] {}. {}\n{covers}", step.number, step.description)
}

/// Apply one revision instruction to a draft, writing nothing.
///
/// # Errors
/// Returns [`HarnessError::Provider`] when the provider fails or never produces
/// a valid plan within the repair budget.
pub async fn revise_plan(
    provider: &dyn ModelProvider,
    model: &str,
    draft: &PlanDraft,
    instruction: &str,
) -> Result<PlanDraft, HarnessError> {
    let user = format!(
        "Current plan:\n\n{}\n\nInstruction:\n\n{instruction}",
        draft.progress.render()
    );
    let mut seed = vec![Message::text(Role::System, REVISE_PROMPT)];
    if let Some(profile) = draft.work_profile {
        seed.push(scope_addendum(
            profile,
            "Keep the scope line of every step you are not changing. Give a scope line to any step that lacks one and to every step you add.\n",
        ));
    }
    seed.push(Message::text(Role::User, user));
    let mut progress = generate_fitted(provider, model, seed, draft.work_profile, None).await?;
    progress.bind_to_brief(&draft.brief_revision);
    Ok(PlanDraft {
        progress,
        brief_revision: draft.brief_revision.clone(),
        repo_summary: draft.repo_summary.clone(),
        revisions: draft.revisions + 1,
        work_profile: draft.work_profile,
    })
}

/// Write an approved plan to the project.
///
/// The only function here that touches it. The draft is validated against the
/// brief one final time — approval is the last moment the check is free, and a
/// plan that reaches disk unsatisfying its brief is a plan `resume` will execute
/// anyway. The write is atomic: a truncating write would leave a half-written
/// `PROGRESS.md` if the process died mid-approval, destroying the plan by
/// accepting it.
///
/// # Errors
/// Returns [`PlanApproval::NotSatisfied`] when the draft does not satisfy the
/// brief, or [`PlanApproval::NotWritten`] when nothing could be written.
pub fn persist_approved_plan(
    root: &std::path::Path,
    draft: &PlanDraft,
    brief: &Brief,
    expected_progress: Option<&Progress>,
) -> Result<(), PlanApproval> {
    if crate::BriefRevision::of(brief).as_str() != draft.brief_revision {
        return Err(PlanApproval::SourceChanged("brief.md"));
    }
    let brief_path = root.join("brief.md");
    let current_brief = std::fs::read_to_string(&brief_path).map_err(|source| {
        PlanApproval::NotWritten(HarnessError::Io {
            path: brief_path.display().to_string(),
            source,
        })
    })?;
    let current_brief = Brief::parse(&current_brief).map_err(PlanApproval::NotWritten)?;
    if crate::BriefRevision::of(&current_brief).as_str() != draft.brief_revision {
        return Err(PlanApproval::SourceChanged("brief.md"));
    }
    let path = root.join("PROGRESS.md");
    let saved = match std::fs::read_to_string(&path) {
        Ok(text) => Some(Progress::parse(&text).map_err(PlanApproval::NotWritten)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(PlanApproval::NotWritten(HarnessError::Io {
                path: path.display().to_string(),
                source,
            }));
        }
    };
    if saved.as_ref() != expected_progress {
        return Err(PlanApproval::SourceChanged("PROGRESS.md"));
    }
    if draft.progress.steps.iter().any(|step| {
        step.done
            && !saved.as_ref().is_some_and(|saved| {
                saved.steps.iter().any(|recorded| {
                    recorded.done
                        && recorded.number == step.number
                        && recorded.description == step.description
                        && recorded.commit == step.commit
                        && recorded.attempts == step.attempts
                        && recorded.sessions == step.sessions
                        && recorded.verify == step.verify
                })
            })
    }) {
        return Err(PlanApproval::HistoryChanged);
    }
    if let Some(saved) = &saved {
        if saved.steps.iter().any(|step| step.done) {
            let reconciled = crate::reconcile(saved, &draft.progress, &draft.brief_revision)
                .map_err(|_| PlanApproval::HistoryChanged)?;
            if reconciled != draft.progress {
                return Err(PlanApproval::HistoryChanged);
            }
        }
    }
    validate_for_approval(&draft.progress, brief).map_err(PlanApproval::NotSatisfied)?;
    if let Some(profile) = draft.work_profile {
        crate::plan_review::validate_work_scope(&draft.progress, profile)
            .map_err(PlanApproval::NotSatisfied)?;
    }

    let mut progress = draft.progress.clone();
    // Bound to the revision the draft was made against, which is the one the
    // reviewer's criterion numbers referred to.
    progress.bind_to_brief(&draft.brief_revision);

    localpilot_store::atomic_write(&path, progress.render().as_bytes()).map_err(|error| {
        PlanApproval::NotWritten(HarnessError::Io {
            path: path.display().to_string(),
            source: std::io::Error::other(error.to_string()),
        })
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::BriefRevision;
    use localpilot_llm::FakeProvider;

    const BRIEF: &str = "# Brief: thing\n\n## Summary\n\nDo it.\n\n## Requirements\n\n\
- It works\n\n## Constraints\n\n- Be small\n\n## Non-Goals\n\n- Else\n\n\
## Acceptance Criteria\n\n- The parser accepts a valid file\n";

    const PLAN: &str = "# Progress: thing\nBranch: feature/thing\n\n## Steps\n\n\
- [ ] 1. Write the parser\n  - covers: AC1\n  - verify: cargo test parser\n  - depends: none\n";

    const REVISED: &str = "# Progress: thing\nBranch: feature/thing\n\n## Steps\n\n\
- [ ] 1. Write the parser\n  - covers: AC1\n  - verify: cargo test parser\n  - depends: none\n\
- [ ] 2. Document it\n  - covers: none\n  - verify: none - documentation only\n  - depends: 1\n";

    fn brief() -> Brief {
        Brief::parse(BRIEF).unwrap()
    }

    #[tokio::test]
    async fn approval_refuses_a_plan_created_or_completed_during_review() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("brief.md"), BRIEF).unwrap();
        let revision = BriefRevision::of(&brief()).as_str().to_string();
        let draft = draft_plan(
            &FakeProvider::new().text(PLAN),
            "m",
            &brief(),
            &revision,
            "repo",
        )
        .await
        .unwrap();
        let path = dir.path().join("PROGRESS.md");
        std::fs::write(&path, PLAN).unwrap();
        assert!(matches!(
            persist_approved_plan(dir.path(), &draft, &brief(), None),
            Err(PlanApproval::SourceChanged("PROGRESS.md"))
        ));
        let original = Progress::parse(PLAN).unwrap();
        let finished = PLAN.replace("- [ ]", "- [x]")
            + "  - commit: fresh-commit\n  - attempts: 2\n  - sessions: first, resumed\n";
        std::fs::write(&path, &finished).unwrap();
        assert!(matches!(
            persist_approved_plan(dir.path(), &draft, &brief(), Some(&original)),
            Err(PlanApproval::SourceChanged("PROGRESS.md"))
        ));
        assert_eq!(std::fs::read_to_string(path).unwrap(), finished);
    }

    #[tokio::test]
    async fn approval_refuses_unearned_completion_and_changed_brief() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("brief.md"), BRIEF).unwrap();
        let revision = BriefRevision::of(&brief()).as_str().to_string();
        let invented = PLAN.replace("- [ ]", "- [x]");
        let draft = draft_plan(
            &FakeProvider::new().text(&invented),
            "m",
            &brief(),
            &revision,
            "repo",
        )
        .await
        .unwrap();
        assert!(matches!(
            persist_approved_plan(dir.path(), &draft, &brief(), None),
            Err(PlanApproval::HistoryChanged)
        ));
        let changed = Brief::parse(&BRIEF.replace("It works", "It works differently")).unwrap();
        assert!(matches!(
            persist_approved_plan(dir.path(), &draft, &changed, None),
            Err(PlanApproval::SourceChanged("brief.md"))
        ));
        assert!(!dir.path().join("PROGRESS.md").exists());
    }

    #[tokio::test]
    async fn approval_never_overwrites_a_malformed_saved_plan() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("brief.md"), BRIEF).unwrap();
        let path = dir.path().join("PROGRESS.md");
        std::fs::write(&path, "unfinished user notes").unwrap();
        let revision = BriefRevision::of(&brief()).as_str().to_string();
        let draft = draft_plan(
            &FakeProvider::new().text(PLAN),
            "m",
            &brief(),
            &revision,
            "repo",
        )
        .await
        .unwrap();
        assert!(matches!(
            persist_approved_plan(dir.path(), &draft, &brief(), None),
            Err(PlanApproval::NotWritten(_))
        ));
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "unfinished user notes"
        );
    }

    #[tokio::test]
    async fn drafting_and_revising_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("brief.md"), BRIEF).unwrap();
        let provider = FakeProvider::new().text(PLAN).text(REVISED);
        let revision = BriefRevision::of(&brief()).as_str().to_string();

        let draft = draft_plan(&provider, "m", &brief(), &revision, "an empty repository")
            .await
            .unwrap();
        assert_eq!(draft.progress.steps.len(), 1);
        assert!(!dir.path().join("PROGRESS.md").exists());

        let revised = revise_plan(&provider, "m", &draft, "add a documentation step")
            .await
            .unwrap();
        assert_eq!(revised.progress.steps.len(), 2);
        assert_eq!(revised.revisions, 1);
        // The revision keeps the brief it was drafted against: a plan does not
        // quietly re-aim at a brief that moved while it was being discussed.
        assert_eq!(revised.brief_revision, draft.brief_revision);
        assert!(!dir.path().join("PROGRESS.md").exists());
    }

    #[tokio::test]
    async fn approval_writes_the_reviewed_plan_bound_to_its_brief() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("brief.md"), BRIEF).unwrap();
        let provider = FakeProvider::new().text(PLAN);
        let revision = BriefRevision::of(&brief()).as_str().to_string();
        let draft = draft_plan(&provider, "m", &brief(), &revision, "summary")
            .await
            .unwrap();

        persist_approved_plan(dir.path(), &draft, &brief(), None).unwrap();

        let written = std::fs::read_to_string(dir.path().join("PROGRESS.md")).unwrap();
        let parsed = Progress::parse(&written).unwrap();
        assert_eq!(parsed.brief_binding, Some(revision));
        assert_eq!(parsed.steps[0].covers, Some(vec![1]));
    }

    #[tokio::test]
    async fn a_plan_that_does_not_satisfy_the_brief_is_refused_and_writes_nothing() {
        // The last moment this check is free. A plan that reaches disk without
        // satisfying its brief is one `resume` will execute regardless.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("brief.md"), BRIEF).unwrap();
        let orphaned = "# Progress: thing\nBranch: feature/thing\n\n## Steps\n\n\
- [ ] 1. Do something else\n  - covers: none\n  - verify: cargo test\n  - depends: none\n";
        let provider = FakeProvider::new().text(orphaned);
        let revision = BriefRevision::of(&brief()).as_str().to_string();
        let draft = draft_plan(&provider, "m", &brief(), &revision, "summary")
            .await
            .unwrap();

        let error = persist_approved_plan(dir.path(), &draft, &brief(), None).unwrap_err();
        assert!(
            matches!(error, PlanApproval::NotSatisfied(ref defects) if defects
                .iter()
                .any(|defect| matches!(defect, PlanDefect::OrphanCriterion { criterion: 1, .. }))),
            "{error}"
        );
        assert!(!dir.path().join("PROGRESS.md").exists());
    }

    fn profile_with(
        caps: &localpilot_config::GranularityConfig,
    ) -> crate::granularity::WorkProfile {
        use crate::granularity::{ContextCapacity, ContextProvenance, Reliability, WorkProfile};
        WorkProfile::resolve(
            ContextCapacity {
                used: 0,
                limit: 262_144,
                provenance: ContextProvenance::RuntimeUsage,
            },
            Reliability::Unknown,
            caps,
        )
    }

    /// The role and text of each message of the only request the provider saw.
    fn request_texts(provider: &FakeProvider) -> Vec<(bool, String)> {
        use localpilot_core::ContentBlock;
        let requests = provider.requests();
        let request = requests.first().expect("a request");
        request
            .messages
            .iter()
            .map(|message| {
                let text = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (matches!(message.role, Role::System), text)
            })
            .collect()
    }

    const SCOPED: &str = "# Progress: thing\nBranch: feature/thing\n\n## Steps\n\n\
- [ ] 1. Write the parser\n  - covers: AC1\n  - verify: cargo test parser\n  - scope: 1, 1, 1, 80\n  - depends: none\n";

    #[tokio::test]
    async fn a_profiled_draft_shows_the_scope_line_after_the_template_it_overrides() {
        let profile = profile_with(&localpilot_config::GranularityConfig::default());
        let provider = FakeProvider::new().text(SCOPED);
        draft_plan_with_profile(&provider, "m", &brief(), "rev", "repo", Some(profile))
            .await
            .unwrap();

        let texts = request_texts(&provider);
        assert_eq!(texts.len(), 3, "planner, scope addendum, user");
        assert!(texts[0].1.contains("exactly this shape"));
        assert!(
            texts[1]
                .1
                .contains(&crate::plan_review::scope_line_example(profile)),
            "{}",
            texts[1].1
        );
        assert!(texts[1].1.contains("overrides 'all three metadata lines'"));
        assert!(!texts[2].0, "the user turn comes last");
    }

    #[tokio::test]
    async fn without_a_profile_the_request_is_the_plain_template() {
        let provider = FakeProvider::new().text(PLAN);
        draft_plan(&provider, "m", &brief(), "rev", "repo")
            .await
            .unwrap();
        let texts = request_texts(&provider);
        assert_eq!(texts.len(), 2, "planner and user only");
        assert!(texts.iter().all(|(_, text)| !text.contains("scope")));
    }

    #[tokio::test]
    async fn a_profiled_replan_and_revision_carry_the_scope_line_after_their_own_prompt() {
        let profile = profile_with(&localpilot_config::GranularityConfig::default());

        let provider = FakeProvider::new().text(SCOPED);
        draft_replan_with_profile(&provider, "m", &brief(), "rev", "repo", &[], Some(profile))
            .await
            .unwrap();
        let texts = request_texts(&provider);
        assert_eq!(texts.len(), 4, "planner, replan, scope addendum, user");
        assert!(texts[1].1.contains("replanning"));
        assert!(texts[2]
            .1
            .contains(&crate::plan_review::scope_line_example(profile)));
        assert!(texts[2].1.contains("carry no scope line"));

        let draft = draft_plan_with_profile(
            &FakeProvider::new().text(SCOPED),
            "m",
            &brief(),
            "rev",
            "repo",
            Some(profile),
        )
        .await
        .unwrap();
        let provider = FakeProvider::new().text(SCOPED);
        revise_plan(&provider, "m", &draft, "rename the step")
            .await
            .unwrap();
        let texts = request_texts(&provider);
        assert_eq!(texts.len(), 3, "revise, scope addendum, user");
        assert!(texts[0].1.contains("revising"));
        assert!(texts[1]
            .1
            .contains(&crate::plan_review::scope_line_example(profile)));
        assert!(texts[1]
            .1
            .contains("Give a scope line to any step that lacks one"));
        assert!(
            texts[2].1.contains("scope: 1, 1, 1, 80"),
            "the plan shown keeps its scope"
        );
    }

    #[test]
    fn the_example_line_is_accepted_by_the_profile_it_was_derived_from() {
        for cap in [None, Some(200), Some(80), Some(10), Some(1)] {
            let caps = localpilot_config::GranularityConfig {
                max_changed_lines: cap,
                ..Default::default()
            };
            let profile = profile_with(&caps);
            let plan = format!(
                "# Progress: t\nBranch: feature/t\n\n## Steps\n\n- [ ] 1. Do it\n{}\n",
                crate::plan_review::scope_line_example(profile)
            );
            let progress = Progress::parse(&plan).unwrap();
            assert!(
                crate::plan_review::validate_work_scope(&progress, profile).is_ok(),
                "cap {cap:?}: {plan}"
            );
        }
    }

    const OVERSIZED: &str = "# Progress: thing\nBranch: feature/thing\n\n## Steps\n\n\
- [ ] 1. Write the parser\n  - covers: AC1\n  - verify: cargo test parser\n  - scope: 2, 2, 1, 300\n  - depends: none\n";

    fn fits(progress: &Progress, profile: crate::granularity::WorkProfile) -> bool {
        crate::plan_review::validate_work_scope(progress, profile).is_ok()
    }

    #[tokio::test]
    async fn an_oversized_plan_is_sent_back_with_its_defects_and_the_fixed_plan_is_kept() {
        let profile = profile_with(&localpilot_config::GranularityConfig::default());
        let provider = FakeProvider::new().text(OVERSIZED).text(SCOPED);
        let draft = draft_plan_with_profile(&provider, "m", &brief(), "rev", "repo", Some(profile))
            .await
            .unwrap();

        assert!(fits(&draft.progress, profile));
        let requests = provider.requests();
        assert_eq!(requests.len(), 2, "one draft, one repair");
        let second = &requests[1].messages;
        let text_of = |message: &Message| {
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    localpilot_core::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let feedback = text_of(second.last().unwrap());
        assert!(feedback.contains("2 files (at most 1)"), "{feedback}");
        assert!(
            feedback.contains("300 changed lines (at most 200)"),
            "{feedback}"
        );
        assert!(matches!(second[second.len() - 2].role, Role::Assistant));
        assert!(
            text_of(&second[second.len() - 2]).contains("scope: 2, 2, 1, 300"),
            "the model is shown the plan it wrote"
        );
    }

    #[tokio::test]
    async fn a_criterion_left_without_an_owner_is_sent_back_even_when_the_scope_fits() {
        let profile = profile_with(&localpilot_config::GranularityConfig::default());
        let orphaned = "# Progress: thing\nBranch: feature/thing\n\n## Steps\n\n\
- [ ] 1. Write the parser\n  - covers: none\n  - verify: cargo test parser\n  - scope: 1, 1, 1, 50\n  - depends: none\n";
        let provider = FakeProvider::new().text(orphaned).text(SCOPED);
        let draft = draft_plan_with_profile(&provider, "m", &brief(), "rev", "repo", Some(profile))
            .await
            .unwrap();

        assert_eq!(provider.requests().len(), 2);
        assert_eq!(draft.progress.steps[0].covers, Some(vec![1]));
        let sent = &provider.requests()[1].messages;
        let last = format!("{:?}", sent.last().unwrap().content);
        assert!(last.contains("no step covers AC1"), "{last}");
    }

    #[tokio::test]
    async fn a_revision_is_not_judged_against_a_brief_it_does_not_have() {
        // Only the work envelope can be repaired on a revision. A plan whose
        // criteria are unowned is the reviewer's to see, not a reason to resend.
        let profile = profile_with(&localpilot_config::GranularityConfig::default());
        let draft = draft_plan_with_profile(
            &FakeProvider::new().text(SCOPED),
            "m",
            &brief(),
            "rev",
            "repo",
            Some(profile),
        )
        .await
        .unwrap();
        let orphaned = "# Progress: thing\nBranch: feature/thing\n\n## Steps\n\n\
- [ ] 1. Write the parser\n  - covers: none\n  - verify: cargo test parser\n  - scope: 1, 1, 1, 50\n  - depends: none\n";
        let provider = FakeProvider::new().text(orphaned);
        revise_plan(&provider, "m", &draft, "drop the criterion")
            .await
            .unwrap();
        assert_eq!(provider.requests().len(), 1);
    }

    #[tokio::test]
    async fn repair_is_bounded_and_the_last_plan_is_returned_with_its_defects() {
        let profile = profile_with(&localpilot_config::GranularityConfig::default());
        let provider = FakeProvider::new()
            .text(OVERSIZED)
            .text(OVERSIZED)
            .text(OVERSIZED);
        let draft = draft_plan_with_profile(&provider, "m", &brief(), "rev", "repo", Some(profile))
            .await
            .expect("a draft with defects is still a draft");

        assert_eq!(provider.requests().len(), 1 + PLAN_REPAIRS);
        assert!(
            !fits(&draft.progress, profile),
            "defects stay visible to the reviewer"
        );
    }

    #[tokio::test]
    async fn a_repair_reply_that_cannot_be_parsed_keeps_the_plan_before_it() {
        let profile = profile_with(&localpilot_config::GranularityConfig::default());
        let provider = FakeProvider::new()
            .text(OVERSIZED)
            .text("not a plan")
            .text("still not")
            .text("nope");
        let draft = draft_plan_with_profile(&provider, "m", &brief(), "rev", "repo", Some(profile))
            .await
            .expect("a failed repair must not lose a usable draft");

        assert_eq!(draft.progress.steps[0].scope.map(|s| s.files), Some(2));
    }

    #[tokio::test]
    async fn without_a_profile_nothing_is_sent_back() {
        let provider = FakeProvider::new().text(PLAN);
        let draft = draft_plan(&provider, "m", &brief(), "rev", "repo")
            .await
            .unwrap();
        assert_eq!(provider.requests().len(), 1);
        assert!(draft.progress.steps[0].scope.is_none());
    }

    #[test]
    fn an_oversized_step_is_reported_with_what_it_declared_and_which_limits_it_broke() {
        let profile = profile_with(&localpilot_config::GranularityConfig::default());
        let plan = "# Progress: t\nBranch: feature/t\n\n## Steps\n\n\
- [ ] 1. Do a lot\n  - scope: 2, 2, 1, 300\n- [ ] 2. Do a little\n  - scope: 1, 1, 1, 50\n";
        let progress = Progress::parse(plan).unwrap();
        let defects = crate::plan_review::validate_work_scope(&progress, profile).unwrap_err();
        assert_eq!(defects.len(), 1, "only step 1 is over: {defects:?}");
        let text = defects[0].to_string();
        assert!(text.contains("step 1"), "{text}");
        assert!(text.contains("2 files (at most 1)"), "{text}");
        assert!(text.contains("2 regions (at most 1)"), "{text}");
        assert!(text.contains("300 changed lines (at most 200)"), "{text}");
        assert!(
            !text.contains("1 decisions"),
            "within its limit, so not named: {text}"
        );
        assert!(text.contains("split"), "{text}");
    }

    #[test]
    fn a_missing_scope_is_reported_with_the_line_to_add_and_not_as_a_split() {
        let profile = profile_with(&localpilot_config::GranularityConfig::default());
        let progress = Progress::parse(PLAN).unwrap();
        let defects = crate::plan_review::validate_work_scope(&progress, profile).unwrap_err();
        let text = defects[0].to_string();
        assert!(
            matches!(defects[0], PlanDefect::MissingScope { step: 1, .. }),
            "{text}"
        );
        assert!(text.contains("  - scope: 1, 1, 1, 80"), "{text}");
        assert!(!text.contains("split"), "{text}");
    }
}
