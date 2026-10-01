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
    let mut seed = vec![
        Message::text(Role::System, PLANNER_PROMPT),
        Message::text(Role::User, user),
    ];
    if let Some(profile) = work_profile {
        seed.insert(0, Message::text(Role::System, format!("{}\nEvery future step must declare scope: files, regions, decisions, changed_lines as four comma-separated integer counts. Split oversized work without dropping coverage or ordering.", profile.instruction())));
    }
    let mut progress = generate(provider, model, seed, "PROGRESS.md", Progress::parse).await?;
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
        Message::text(Role::User, user),
    ];
    if let Some(profile) = work_profile {
        seed.insert(0, Message::text(Role::System, format!("{}\nEvery future step must declare scope: files, regions, decisions, changed_lines as four comma-separated integer counts. Split oversized work without dropping coverage or ordering.", profile.instruction())));
    }
    let mut progress = generate(provider, model, seed, "PROGRESS.md", Progress::parse).await?;
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
    let mut seed = vec![
        Message::text(Role::System, REVISE_PROMPT),
        Message::text(Role::User, user),
    ];
    if let Some(profile) = draft.work_profile {
        seed.insert(
            0,
            Message::text(
                Role::System,
                format!(
                    "{}\nPreserve scope metadata and split oversized future work.",
                    profile.instruction()
                ),
            ),
        );
    }
    let mut progress = generate(provider, model, seed, "PROGRESS.md", Progress::parse).await?;
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
}
