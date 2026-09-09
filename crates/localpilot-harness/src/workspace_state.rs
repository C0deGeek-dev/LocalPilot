//! One read-only answer to "what state is this harness project in?".
//!
//! Every command that touches a harness project needs the same facts: does a
//! brief exist, is it readable, does a plan exist, does that plan still belong
//! to the brief, is there work left, and is something already running. Before
//! this module each command answered those questions for itself, and the
//! answers disagreed — a missing `PROGRESS.md` and an unparseable one both
//! reported zero steps, so a corrupt plan looked like a fresh project.
//!
//! Two rules hold everywhere here:
//!
//! * **Inspection never writes.** No provider call, no repair, no git mutation,
//!   no file write. Looking at a project cannot change it.
//! * **Missing and broken are different states.** Every failure keeps the named
//!   error that produced it, so a caller can say *why* rather than guess.
//!
//! Document state and operation state are separate axes because they answer
//! different questions and are observed differently. Whether a plan is stale
//! comes from two files on disk; whether something is running right now cannot
//! be derived from those files at all, and persisting it there would leave a
//! crashed run permanently "active".

use std::path::Path;

use crate::binding::{BindingSupport, BriefRevision};
use crate::brief::Brief;
use crate::error::HarnessError;
use crate::progress::Progress;

/// What the host knows about a harness operation running *now*.
///
/// This is live process state, so it is supplied by the caller rather than
/// discovered here. A CLI invocation is itself the operation and reports
/// [`OperationLiveness::Idle`]; an interactive host reports [`Running`] while
/// its operation pump holds a harness operation.
///
/// [`Running`]: OperationLiveness::Running
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OperationLiveness {
    /// Nothing is running that this caller knows of.
    #[default]
    Idle,
    /// A harness operation is in flight.
    Running,
}

/// A recorded interruption: an operation that started, stopped, and left a
/// resumable record behind.
///
/// Supplied by the caller because the record lives in the caller's store, not
/// in the project's documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptedRun {
    /// The run stopped on a provider quota or rate limit and persisted enough
    /// state to continue later.
    QuotaPause,
}

/// Whether an operation is running, interrupted, or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationState {
    /// Nothing running and nothing interrupted.
    Idle,
    /// An operation is in flight; starting another would duplicate it.
    Active,
    /// An operation stopped and left a record. Nothing is running, so this is
    /// not `Active`; the record exists, so it is not `Idle` either — offering a
    /// plain start here would discard the recorded reason for stopping.
    Interrupted(InterruptedRun),
}

/// The state of the two runtime documents.
///
/// Ordered from "nothing here" to "finished". Every variant that carries an
/// error carries the named error, never a rendered string.
///
/// Not `Clone` or `PartialEq`: the carried [`HarnessError`] can wrap a
/// [`std::io::Error`], which is neither. Callers match on the variant, which is
/// what the contract promises anyway.
#[derive(Debug)]
pub enum DocumentState {
    /// No `brief.md`. The project has not been described yet.
    NoBrief,
    /// `brief.md` exists but could not be read (permissions, encoding, a
    /// directory in its place). Distinct from absent: something is there.
    BriefUnreadable(HarnessError),
    /// `brief.md` was read but is not a valid brief. Distinct from absent: the
    /// user has content worth repairing, not a blank slate.
    BriefMalformed(HarnessError),
    /// A valid brief with no plan. Planning is the next action.
    BriefOnly { brief: Brief },
    /// `PROGRESS.md` exists but could not be read.
    PlanUnreadable { brief: Brief, error: HarnessError },
    /// `PROGRESS.md` was read but is not a valid plan.
    PlanMalformed { brief: Brief, error: HarnessError },
    /// Both documents are valid, but the plan records no brief revision, so
    /// whether it matches this brief is *unknowable*. Every plan written before
    /// the binding existed is in this state.
    PlanUnbound { brief: Brief, progress: Progress },
    /// The plan records a revision that is not this brief's. The requirements
    /// moved after the plan was made.
    PlanStale {
        brief: Brief,
        progress: Progress,
        /// What the plan recorded.
        recorded: String,
        /// What the brief hashes to now.
        current: BriefRevision,
    },
    /// The plan records a binding this build cannot interpret — a newer
    /// canonicalisation, or a hand-edited value. Whether it matches is
    /// unknowable, so it is neither stale (a claim about the user's
    /// requirements) nor current.
    PlanBindingUnsupported {
        brief: Brief,
        progress: Progress,
        /// What the plan recorded.
        recorded: String,
    },
    /// Bound to this brief, with work left.
    PlanReady { brief: Brief, progress: Progress },
    /// Bound to this brief, with every step done.
    PlanComplete { brief: Brief, progress: Progress },
}

impl DocumentState {
    /// The brief, when one parsed.
    #[must_use]
    pub fn brief(&self) -> Option<&Brief> {
        match self {
            Self::NoBrief | Self::BriefUnreadable(_) | Self::BriefMalformed(_) => None,
            Self::BriefOnly { brief }
            | Self::PlanUnreadable { brief, .. }
            | Self::PlanMalformed { brief, .. }
            | Self::PlanUnbound { brief, .. }
            | Self::PlanStale { brief, .. }
            | Self::PlanBindingUnsupported { brief, .. }
            | Self::PlanReady { brief, .. }
            | Self::PlanComplete { brief, .. } => Some(brief),
        }
    }

    /// The plan, when one parsed.
    ///
    /// A plan is returned for stale and unbound states too: the work recorded in
    /// it — completed steps, their commits, their attempt counts — remains true
    /// and inspectable no matter what happened to the brief.
    #[must_use]
    pub fn progress(&self) -> Option<&Progress> {
        match self {
            Self::NoBrief
            | Self::BriefUnreadable(_)
            | Self::BriefMalformed(_)
            | Self::BriefOnly { .. }
            | Self::PlanUnreadable { .. }
            | Self::PlanMalformed { .. } => None,
            Self::PlanUnbound { progress, .. }
            | Self::PlanStale { progress, .. }
            | Self::PlanBindingUnsupported { progress, .. }
            | Self::PlanReady { progress, .. }
            | Self::PlanComplete { progress, .. } => Some(progress),
        }
    }
}

/// The full picture: documents plus whatever is running.
#[derive(Debug)]
pub struct WorkspaceState {
    pub documents: DocumentState,
    pub operation: OperationState,
}

/// What [`inspect`] is given. The two operation inputs come from the caller
/// because neither is derivable from the project's files.
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceInputs<'a> {
    /// The project root holding `brief.md` and `PROGRESS.md`.
    pub root: &'a Path,
    /// Whether the caller has a harness operation in flight.
    pub liveness: OperationLiveness,
    /// A persisted interruption record the caller found in its store.
    pub interrupted: Option<InterruptedRun>,
}

impl<'a> WorkspaceInputs<'a> {
    /// Inputs for a caller that has no operation of its own — the common case
    /// for a one-shot command.
    #[must_use]
    pub fn at(root: &'a Path) -> Self {
        Self {
            root,
            liveness: OperationLiveness::Idle,
            interrupted: None,
        }
    }
}

/// Inspect a harness project. Reads at most two files and writes nothing.
#[must_use]
pub fn inspect(inputs: WorkspaceInputs<'_>) -> WorkspaceState {
    WorkspaceState {
        documents: inspect_documents(inputs.root),
        // A live operation outranks a recorded one: if something is running now,
        // an older pause record is not what the caller should be told about.
        operation: match (inputs.liveness, inputs.interrupted) {
            (OperationLiveness::Running, _) => OperationState::Active,
            (OperationLiveness::Idle, Some(run)) => OperationState::Interrupted(run),
            (OperationLiveness::Idle, None) => OperationState::Idle,
        },
    }
}

fn inspect_documents(root: &Path) -> DocumentState {
    let brief = match read_document(root, "brief.md") {
        Read::Absent => return DocumentState::NoBrief,
        Read::Failed(error) => return DocumentState::BriefUnreadable(error),
        Read::Text(text) => match Brief::parse(&text) {
            Ok(brief) => brief,
            Err(error) => return DocumentState::BriefMalformed(error),
        },
    };

    let progress = match read_document(root, "PROGRESS.md") {
        Read::Absent => return DocumentState::BriefOnly { brief },
        Read::Failed(error) => return DocumentState::PlanUnreadable { brief, error },
        Read::Text(text) => match Progress::parse(&text) {
            Ok(progress) => progress,
            Err(error) => return DocumentState::PlanMalformed { brief, error },
        },
    };

    let current = BriefRevision::of(&brief);
    let Some(recorded) = progress.brief_binding.clone() else {
        return DocumentState::PlanUnbound { brief, progress };
    };
    match current.classify(&recorded) {
        BindingSupport::Unsupported => DocumentState::PlanBindingUnsupported {
            brief,
            progress,
            recorded,
        },
        BindingSupport::Stale => DocumentState::PlanStale {
            brief,
            progress,
            recorded,
            current,
        },
        BindingSupport::Current if progress.next_incomplete().is_none() => {
            DocumentState::PlanComplete { brief, progress }
        }
        BindingSupport::Current => DocumentState::PlanReady { brief, progress },
    }
}

enum Read {
    Absent,
    Text(String),
    Failed(HarnessError),
}

fn read_document(root: &Path, name: &str) -> Read {
    let path = root.join(name);
    match std::fs::read_to_string(&path) {
        Ok(text) => Read::Text(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Read::Absent,
        Err(error) => Read::Failed(HarnessError::Io {
            path: path.display().to_string(),
            source: error,
        }),
    }
}

/// Why a project cannot run its next step.
///
/// Each variant is a distinct answer with a distinct remedy, so a host can say
/// what to do instead of reporting a generic failure.
/// Owned rather than borrowing the inspected state: this travels inside
/// [`HarnessError`] out of the executor, and an error cannot borrow the state it
/// was derived from. Each variant still carries its own named data — the
/// rendered detail of a document error, the two revisions of a mismatch — so
/// callers match on the variant rather than parsing a message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotResumable {
    /// There is no brief yet.
    #[error("brief.md not found")]
    NoBrief,
    /// The brief exists but could not be read or parsed.
    #[error("brief.md cannot be used: {detail}")]
    BriefBroken { detail: String },
    /// There is a brief but no plan.
    #[error("PROGRESS.md not found")]
    NoPlan,
    /// The plan exists but could not be read or parsed.
    #[error("PROGRESS.md cannot be used: {detail}")]
    PlanBroken { detail: String },
    /// The plan predates the brief binding, so whether it matches this brief is
    /// unknown. It must be adopted deliberately before it can run.
    #[error("PROGRESS.md records no brief revision")]
    Unbound,
    /// The plan was built against a different revision of the brief.
    #[error(
        "PROGRESS.md was built against brief revision {recorded}, but brief.md is now {current}"
    )]
    Stale {
        /// What the plan recorded.
        recorded: String,
        /// What the brief hashes to now.
        current: String,
    },
    /// The plan records a binding this build cannot interpret, so whether it
    /// matches the brief is unknown.
    #[error("PROGRESS.md records a brief binding this build does not understand ({recorded})")]
    BindingUnsupported { recorded: String },
    /// Every step is done.
    #[error("every step in PROGRESS.md is already done")]
    Complete,
    /// A harness operation is already running.
    #[error("a harness operation is already running")]
    OperationActive,
}

/// The plan to run, or the named reason there is none.
///
/// This is the single gate every execution path goes through, so "a stale plan
/// cannot resume" is one rule in one place rather than a check each caller is
/// trusted to repeat.
///
/// An interrupted run does **not** block: a recorded quota pause is resumable
/// state, and the existing wait-and-resume path is what consumes it.
///
/// # Errors
/// Returns the named reason the project cannot resume.
pub fn resumable(state: &WorkspaceState) -> Result<&Progress, NotResumable> {
    if state.operation == OperationState::Active {
        return Err(NotResumable::OperationActive);
    }
    match &state.documents {
        DocumentState::NoBrief => Err(NotResumable::NoBrief),
        DocumentState::BriefUnreadable(error) | DocumentState::BriefMalformed(error) => {
            Err(NotResumable::BriefBroken {
                detail: error.to_string(),
            })
        }
        DocumentState::BriefOnly { .. } => Err(NotResumable::NoPlan),
        DocumentState::PlanUnreadable { error, .. }
        | DocumentState::PlanMalformed { error, .. } => Err(NotResumable::PlanBroken {
            detail: error.to_string(),
        }),
        DocumentState::PlanUnbound { .. } => Err(NotResumable::Unbound),
        DocumentState::PlanBindingUnsupported { recorded, .. } => {
            Err(NotResumable::BindingUnsupported {
                recorded: recorded.clone(),
            })
        }
        DocumentState::PlanStale {
            recorded, current, ..
        } => Err(NotResumable::Stale {
            recorded: recorded.clone(),
            current: current.to_string(),
        }),
        DocumentState::PlanComplete { .. } => Err(NotResumable::Complete),
        DocumentState::PlanReady { progress, .. } => Ok(progress),
    }
}

/// Why an adoption was refused.
#[derive(Debug, thiserror::Error)]
pub enum AdoptError {
    /// The project is not in a state where adoption means anything.
    #[error("{0}")]
    NotAdoptable(String),
    /// Writing the binding failed.
    #[error(transparent)]
    Write(#[from] HarnessError),
}

/// Bind an existing plan to the current brief, and write only that.
///
/// This exists for plans written before the binding did. Such a plan cannot be
/// resumed automatically: with no recorded revision there is no way to tell a
/// plan that still matches its brief from one whose brief was edited years ago,
/// and quietly running it would present a guess as a fact. Adoption is the
/// user's explicit statement that this plan belongs to this brief. It is
/// consent about that relationship specifically — never inferred from a tool
/// approval, a permission profile, or any other yes.
///
/// Only the binding is written. Steps, their completion marks, their commits,
/// and their attempt counts are re-rendered exactly as they were parsed.
///
/// # Errors
/// Returns [`AdoptError::NotAdoptable`] unless the project is in the unbound
/// state, or [`AdoptError::Write`] if the plan cannot be written back.
pub fn adopt_plan(root: &Path) -> Result<BriefRevision, AdoptError> {
    let state = inspect(WorkspaceInputs::at(root));
    let (brief, progress) = match state.documents {
        DocumentState::PlanUnbound { brief, progress } => (brief, progress),
        DocumentState::PlanReady { progress, .. }
        | DocumentState::PlanComplete { progress, .. } => {
            return Err(AdoptError::NotAdoptable(format!(
                "PROGRESS.md is already bound to this brief ({})",
                progress.brief_binding.unwrap_or_default()
            )));
        }
        DocumentState::PlanBindingUnsupported { recorded, .. } => {
            return Err(AdoptError::NotAdoptable(format!(
                "PROGRESS.md records a brief binding this build does not understand \
                 ({recorded}); it was written by a different version of LocalPilot. Update \
                 LocalPilot, or replan, rather than overwriting a binding whose meaning is \
                 unknown"
            )));
        }
        DocumentState::PlanStale { .. } => {
            return Err(AdoptError::NotAdoptable(
                "PROGRESS.md was built against a different revision of brief.md; replan instead \
                 of adopting, so a superseded plan is not declared current"
                    .to_string(),
            ));
        }
        DocumentState::NoBrief | DocumentState::BriefUnreadable(_) => {
            return Err(AdoptError::NotAdoptable(
                "brief.md is missing or unreadable".to_string(),
            ));
        }
        DocumentState::BriefMalformed(_) => {
            return Err(AdoptError::NotAdoptable(
                "brief.md is malformed; fix it before adopting a plan against it".to_string(),
            ));
        }
        DocumentState::BriefOnly { .. } => {
            return Err(AdoptError::NotAdoptable(
                "there is no PROGRESS.md to adopt".to_string(),
            ));
        }
        DocumentState::PlanUnreadable { .. } | DocumentState::PlanMalformed { .. } => {
            return Err(AdoptError::NotAdoptable(
                "PROGRESS.md is unreadable or malformed".to_string(),
            ));
        }
    };

    let revision = BriefRevision::of(&brief);
    let mut bound = progress;
    bound.bind_to_brief(revision.as_str());
    let path = root.join("PROGRESS.md");
    std::fs::write(&path, bound.render()).map_err(|error| HarnessError::Io {
        path: path.display().to_string(),
        source: error,
    })?;
    Ok(revision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_operation_outranks_a_recorded_pause() {
        let root = Path::new(".");
        let state = inspect(WorkspaceInputs {
            root,
            liveness: OperationLiveness::Running,
            interrupted: Some(InterruptedRun::QuotaPause),
        });
        assert_eq!(
            state.operation,
            OperationState::Active,
            "something running now is the more urgent fact"
        );
    }

    #[test]
    fn an_idle_caller_with_a_record_is_interrupted_not_idle() {
        let state = inspect(WorkspaceInputs {
            root: Path::new("."),
            liveness: OperationLiveness::Idle,
            interrupted: Some(InterruptedRun::QuotaPause),
        });
        assert_eq!(
            state.operation,
            OperationState::Interrupted(InterruptedRun::QuotaPause)
        );
    }
}
