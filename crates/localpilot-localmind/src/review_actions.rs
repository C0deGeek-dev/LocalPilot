//! Reviewer actions on a lesson that the lab has a stake in: rewrite, split,
//! and asking for a rerun.
//!
//! LocalMind owns the decision — a rewrite or a split makes new review items
//! and leaves the original as history, with its lab results still bound to the
//! text they tested. This module is the adapter in front of that, plus the two
//! things that are LocalPilot's alone:
//!
//! - **A split is drafted by a model and approved by a person.** The draft is a
//!   file; nothing happens until a named reviewer approves it as it then stands
//!   on disk.
//! - **A rerun is a request, never a run.** It is a note on the lesson that a
//!   person wants the lab to look again. Starting Replay or Uplift keeps its
//!   own opt-in and its own confirmation; a request satisfies neither.

use std::path::{Path, PathBuf};

use localmind_core::{CandidateLesson, EvidenceTier, LessonRevision, ReviewItemId, ReviewState};
use localmind_inference::extract_json_payload;
use localmind_store::{ReviewQueue, ReviewQueueItem};
use localpilot_core::{Message, Role};
use localpilot_llm::ModelProvider;
use serde::{Deserialize, Serialize};

use crate::lab_tasks::{ask, DraftFailure};
use crate::ops::open_memory;
use crate::LearningError;

/// The directory, under the project's `.localpilot/`, split drafts live in.
pub const SPLIT_DRAFTS_DIR: &str = "review/splits";
/// The directory, under the project's `.localpilot/`, rerun requests live in.
pub const RERUN_REQUESTS_DIR: &str = "lab/reruns";
/// A split has at least this many parts…
pub const MIN_SPLIT_PARTS: usize = 2;
/// …and at most this many.
pub const MAX_SPLIT_PARTS: usize = 5;
/// The longest a part's lesson sentence may be.
pub const MAX_PART_CHARS: usize = 400;
/// The shortest a part's lesson sentence may be.
pub const MIN_PART_CHARS: usize = 12;
/// The split draft format.
pub const SPLIT_DRAFT_VERSION: u32 = 1;

fn review_err(error: impl std::fmt::Display) -> LearningError {
    LearningError::Review(error.to_string())
}

fn memory_err(error: impl std::fmt::Display) -> LearningError {
    LearningError::Memory(error.to_string())
}

fn queue(root: &Path) -> Result<ReviewQueue, LearningError> {
    ReviewQueue::open_project(root).map_err(review_err)
}

/// What a rewrite left behind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rewritten {
    /// The original item, closed as history.
    pub original: String,
    /// The rewritten item, accepted.
    pub revised: String,
    /// How many lab results stayed with the original.
    pub results_left_behind: usize,
}

/// Rewrite a lesson in review as `reviewer`. The original is kept as history
/// with its lab results; the rewrite is a new, accepted, untested item.
///
/// # Errors
/// [`LearningError::Review`] when the item is already decided or the change is
/// empty, [`LearningError::Memory`] when the audit cannot be written.
pub fn review_rewrite(
    root: &Path,
    item_id: &str,
    revision: &LessonRevision,
    reviewer: &str,
    note: Option<String>,
) -> Result<Rewritten, LearningError> {
    let outcome = queue(root)?
        .rewrite(&ReviewItemId::new(item_id), revision, reviewer, note)
        .map_err(review_err)?;
    let persistence = open_memory(root)?;
    for item in [&outcome.original, &outcome.revised] {
        persistence
            .record_review_item_audit(item)
            .map_err(memory_err)?;
    }
    Ok(Rewritten {
        original: outcome.original.id.to_string(),
        revised: outcome.revised.id.to_string(),
        results_left_behind: outcome.original.candidate.experiments.len(),
    })
}

/// One pending part a split produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SplitPart {
    pub id: String,
    pub summary: String,
}

/// Split a lesson in review into `parts` as `reviewer`. Each part becomes its
/// own pending, untested item; the original is kept as history.
///
/// # Errors
/// [`LearningError::Review`] when the item is already decided or the parts are
/// not a usable split, [`LearningError::Memory`] when the audit cannot be
/// written.
pub fn review_split(
    root: &Path,
    item_id: &str,
    parts: &[String],
    reviewer: &str,
    note: Option<String>,
) -> Result<Vec<SplitPart>, LearningError> {
    let revisions: Vec<LessonRevision> = parts
        .iter()
        .map(|part| LessonRevision::of_summary(part.as_str()))
        .collect();
    let outcome = queue(root)?
        .split(&ReviewItemId::new(item_id), &revisions, reviewer, note)
        .map_err(review_err)?;
    open_memory(root)?
        .record_review_item_audit(&outcome.original)
        .map_err(memory_err)?;
    Ok(outcome
        .parts
        .iter()
        .map(|part| SplitPart {
            id: part.id.to_string(),
            summary: part.candidate.summary().to_string(),
        })
        .collect())
}

/// A proposed split of one review item, waiting for a person.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SplitDraft {
    pub version: u32,
    /// The review item the draft is for.
    pub item_id: String,
    /// The content identity of the lesson when it was drafted. A draft of a
    /// lesson that has since changed is not approved.
    pub candidate_identity: String,
    /// The lesson sentence of each part.
    pub parts: Vec<String>,
    /// The model that proposed it, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drafted_by: Option<String>,
}

/// Why a split draft cannot be approved.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SplitProblem {
    #[error("a split needs {MIN_SPLIT_PARTS} to {MAX_SPLIT_PARTS} parts, not {0}")]
    PartCount(usize),
    #[error("part {0} must be {MIN_PART_CHARS} to {MAX_PART_CHARS} characters")]
    PartLength(usize),
    #[error("parts {0} and {1} say the same thing")]
    Duplicate(usize, usize),
    #[error("part {0} is the lesson unchanged, which splits nothing")]
    SameAsOriginal(usize),
    #[error("the lesson changed after this draft was written; draft it again")]
    StaleLesson,
    #[error("the draft is in a format this version does not read")]
    Version,
}

/// A sentence reduced to its words: case, spacing and punctuation dropped, so
/// two parts that differ only in those read as the same part.
fn comparable(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Check a split draft against the lesson it would split.
///
/// # Errors
/// Every [`SplitProblem`] found.
pub fn validate_split(
    draft: &SplitDraft,
    candidate: &CandidateLesson,
) -> Result<(), Vec<SplitProblem>> {
    let mut problems = Vec::new();
    if draft.version != SPLIT_DRAFT_VERSION {
        problems.push(SplitProblem::Version);
    }
    if draft.candidate_identity != candidate.content_identity() {
        problems.push(SplitProblem::StaleLesson);
    }
    if !(MIN_SPLIT_PARTS..=MAX_SPLIT_PARTS).contains(&draft.parts.len()) {
        problems.push(SplitProblem::PartCount(draft.parts.len()));
    }
    let original = comparable(candidate.summary());
    let normalised: Vec<String> = draft.parts.iter().map(|part| comparable(part)).collect();
    for (index, part) in draft.parts.iter().enumerate() {
        let length = part.trim().chars().count();
        if !(MIN_PART_CHARS..=MAX_PART_CHARS).contains(&length) {
            problems.push(SplitProblem::PartLength(index + 1));
        }
        if normalised[index] == original {
            problems.push(SplitProblem::SameAsOriginal(index + 1));
        }
        if let Some(earlier) = normalised[..index]
            .iter()
            .position(|other| *other == normalised[index])
        {
            problems.push(SplitProblem::Duplicate(earlier + 1, index + 1));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// A split draft, and how many model calls it took.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DraftedSplit {
    pub draft: SplitDraft,
    pub model_calls: u32,
    pub repaired: bool,
}

const SPLIT_SYSTEM_PROMPT: &str = "You split one lesson for a coding assistant into narrower \
lessons. Each part must be a complete, reusable sentence that stands on its own, says one thing, \
and is true without the other parts. Do not add claims the lesson does not make. Reply with one \
JSON object and nothing else: {\"parts\":[\"...\",\"...\"]}. Write 2 to 5 parts. If the lesson \
already says one thing, still reply with the two narrowest lessons it contains.";

fn split_prompt(candidate: &CandidateLesson) -> String {
    let mut text = format!("The lesson: {}\n", candidate.summary());
    if let Some(draft) = &candidate.hindsight {
        text.push_str(&format!("What was intended: {}\n", draft.intended_outcome));
        text.push_str(&format!("What happened: {}\n", draft.observed_outcome));
        for hypothesis in &draft.hypotheses {
            text.push_str(&format!("Why: {}\n", hypothesis.claim));
        }
        if let Some(applicability) = &draft.applicability {
            text.push_str(&format!("Where it applies: {applicability}\n"));
        }
    }
    text.push_str("Write the parts.");
    text
}

#[derive(Deserialize)]
struct SplitReply {
    parts: Vec<String>,
}

fn parse_split(reply: &str, item: &ReviewQueueItem, model: &str) -> Result<SplitDraft, String> {
    let payload = extract_json_payload(reply).ok_or("it holds no JSON object")?;
    let parsed: SplitReply = serde_json::from_str(payload).map_err(|error| error.to_string())?;
    let draft = SplitDraft {
        version: SPLIT_DRAFT_VERSION,
        item_id: item.id.to_string(),
        candidate_identity: item.candidate.content_identity(),
        parts: parsed
            .parts
            .into_iter()
            .map(|part| part.trim().to_string())
            .collect(),
        drafted_by: Some(model.to_string()),
    };
    validate_split(&draft, &item.candidate).map_err(|problems| {
        problems
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    Ok(draft)
}

/// The review item `item_id` names, when a reviewer can still split it.
///
/// # Errors
/// [`LearningError::Review`] when there is no such item or it is decided.
pub fn splittable_item(root: &Path, item_id: &str) -> Result<ReviewQueueItem, LearningError> {
    let item = queue(root)?
        .get(&ReviewItemId::new(item_id))
        .map_err(review_err)?
        .ok_or_else(|| review_err(format!("no review item `{item_id}`")))?;
    if !matches!(item.state, ReviewState::Pending | ReviewState::Deferred) {
        return Err(review_err(format!(
            "review item {item_id} is already decided ({:?}); it is history and cannot be split",
            item.state
        )));
    }
    Ok(item)
}

/// Have `provider` draft a split of `item`. The reply is validated; a reply
/// that fails gets one repair request, and a second failure ends the attempt.
/// The result is a draft: it changes nothing in review.
///
/// Only the lesson and its hindsight are sent — to the provider the project is
/// already configured to use — never the run's raw facts.
///
/// # Errors
/// [`DraftFailure`] when the model cannot be reached or keeps failing.
pub async fn draft_split(
    provider: &dyn ModelProvider,
    model: &str,
    item: &ReviewQueueItem,
) -> Result<DraftedSplit, DraftFailure> {
    let mut messages = vec![
        Message::text(Role::System, SPLIT_SYSTEM_PROMPT.to_string()),
        Message::text(Role::User, split_prompt(&item.candidate)),
    ];
    let mut last_problem = String::new();
    for attempt in 0..2u32 {
        let reply = ask(provider, model, &messages).await?;
        match parse_split(&reply, item, model) {
            Ok(draft) => {
                return Ok(DraftedSplit {
                    draft,
                    model_calls: attempt + 1,
                    repaired: attempt > 0,
                })
            }
            Err(problem) => {
                messages.push(Message::text(Role::Assistant, reply));
                messages.push(Message::text(
                    Role::User,
                    format!(
                        "That reply cannot be used: {problem}. Reply again with only the JSON \
                         object, fixing exactly that."
                    ),
                ));
                last_problem = problem;
            }
        }
    }
    Err(DraftFailure::Unusable(last_problem))
}

fn file_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Where an item's split draft is kept.
#[must_use]
pub fn split_draft_path(localpilot_dir: &Path, item_id: &str) -> PathBuf {
    localpilot_dir
        .join(SPLIT_DRAFTS_DIR)
        .join(format!("{}.draft.json", file_name(item_id)))
}

/// Write a split draft for a person to read and edit.
///
/// # Errors
/// The file cannot be written.
pub fn write_split_draft(localpilot_dir: &Path, draft: &SplitDraft) -> std::io::Result<PathBuf> {
    let path = split_draft_path(localpilot_dir, &draft.item_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(draft).map_err(std::io::Error::other)?;
    std::fs::write(&path, text)?;
    Ok(path)
}

/// The split draft on disk for `item_id`, if there is one.
///
/// # Errors
/// The file exists and cannot be read as a draft.
pub fn read_split_draft(
    localpilot_dir: &Path,
    item_id: &str,
) -> Result<Option<SplitDraft>, String> {
    let path = split_draft_path(localpilot_dir, item_id);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| format!("{}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Approve the split draft as it now stands on disk, in `reviewer`'s name. The
/// draft is consumed: the parts are in review, and the file is removed.
///
/// # Errors
/// [`LearningError::Review`] when there is no draft, it no longer fits the
/// lesson, or the queue refuses the split.
pub fn approve_split(
    root: &Path,
    localpilot_dir: &Path,
    item_id: &str,
    reviewer: &str,
    note: Option<String>,
) -> Result<Vec<SplitPart>, LearningError> {
    let reviewer = reviewer.trim();
    if reviewer.is_empty() {
        return Err(review_err("a split is approved by a named reviewer"));
    }
    let item = splittable_item(root, item_id)?;
    let draft = read_split_draft(localpilot_dir, item_id)
        .map_err(review_err)?
        .ok_or_else(|| review_err(format!("no split draft for {item_id}; draft one first")))?;
    validate_split(&draft, &item.candidate).map_err(|problems| {
        review_err(
            problems
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        )
    })?;
    let note = note.or_else(|| {
        draft
            .drafted_by
            .as_ref()
            .map(|model| format!("parts drafted by {model}, approved by {reviewer}"))
    });
    let parts = review_split(root, item_id, &draft.parts, reviewer, note)?;
    let _ = std::fs::remove_file(split_draft_path(localpilot_dir, item_id));
    Ok(parts)
}

/// A person's request that the lab look at a lesson again.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RerunRequest {
    /// The lesson, by content identity — the id a result is bound to.
    pub candidate_identity: String,
    /// Which tier to run again.
    pub tier: EvidenceTier,
    pub requested_by: String,
    /// Seconds since the Unix epoch.
    pub requested_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Why a rerun cannot be requested.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RerunRefusal {
    #[error("a rerun is requested by a named reviewer")]
    NoReviewer,
    #[error("only Replay and Uplift runs are started by a person; Logic runs on its own")]
    Tier,
    #[error("{0} is no longer a live lesson in review ({1}); its results are history")]
    NotLive(String, String),
    #[error("the request could not be written: {0}")]
    Io(String),
}

fn tier_name(tier: EvidenceTier) -> &'static str {
    match tier {
        EvidenceTier::Logic => "logic",
        EvidenceTier::Replay => "replay",
        EvidenceTier::Uplift => "uplift",
    }
}

fn rerun_path(localpilot_dir: &Path, candidate_identity: &str, tier: EvidenceTier) -> PathBuf {
    localpilot_dir.join(RERUN_REQUESTS_DIR).join(format!(
        "{}.{}.json",
        file_name(candidate_identity),
        tier_name(tier)
    ))
}

/// Whether the lesson with this content identity is still one the lab should
/// run: undecided, or accepted. A rejected, merged, rewritten or split lesson
/// is history. `None` when no review item holds it.
///
/// # Errors
/// [`LearningError::Review`] when the queue cannot be read.
pub fn lab_lesson_state(
    root: &Path,
    candidate_identity: &str,
) -> Result<Option<ReviewState>, LearningError> {
    Ok(queue(root)?
        .list()
        .map_err(review_err)?
        .into_iter()
        .find(|item| item.candidate.content_identity() == candidate_identity)
        .map(|item| item.state))
}

/// Whether a lesson in this state is still one to test.
#[must_use]
pub fn is_live(state: &ReviewState) -> bool {
    matches!(
        state,
        ReviewState::Pending | ReviewState::Deferred | ReviewState::Accepted | ReviewState::Edited
    )
}

/// Record that `reviewer` wants `tier` run again for a lesson. This writes a
/// note and runs nothing: it does not enable the tier for the project, and it
/// does not stand in for the confirmation a run needs.
///
/// # Errors
/// [`RerunRefusal`] when nobody is named, the tier is not one a person starts,
/// the lesson is history, or the note cannot be written.
pub fn request_rerun(
    root: &Path,
    localpilot_dir: &Path,
    candidate_identity: &str,
    tier: EvidenceTier,
    reviewer: &str,
    note: Option<String>,
    now: i64,
) -> Result<RerunRequest, RerunRefusal> {
    let reviewer = reviewer.trim();
    if reviewer.is_empty() {
        return Err(RerunRefusal::NoReviewer);
    }
    if tier == EvidenceTier::Logic {
        return Err(RerunRefusal::Tier);
    }
    let state = lab_lesson_state(root, candidate_identity)
        .map_err(|error| RerunRefusal::Io(error.to_string()))?;
    match state {
        Some(state) if is_live(&state) => {}
        Some(state) => {
            return Err(RerunRefusal::NotLive(
                candidate_identity.to_string(),
                format!("{state:?}"),
            ))
        }
        None => {
            return Err(RerunRefusal::NotLive(
                candidate_identity.to_string(),
                "not in review".to_string(),
            ))
        }
    }
    let request = RerunRequest {
        candidate_identity: candidate_identity.to_string(),
        tier,
        requested_by: reviewer.to_string(),
        requested_at: now,
        note,
    };
    let path = rerun_path(localpilot_dir, candidate_identity, tier);
    let write = || -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(&request).map_err(std::io::Error::other)?;
        std::fs::write(&path, text)
    };
    write().map_err(|error| RerunRefusal::Io(error.to_string()))?;
    Ok(request)
}

/// Every open rerun request for a lesson, oldest first.
#[must_use]
pub fn rerun_requests(localpilot_dir: &Path, candidate_identity: &str) -> Vec<RerunRequest> {
    let mut requests: Vec<RerunRequest> = [EvidenceTier::Replay, EvidenceTier::Uplift]
        .into_iter()
        .filter_map(|tier| {
            let text =
                std::fs::read_to_string(rerun_path(localpilot_dir, candidate_identity, tier))
                    .ok()?;
            serde_json::from_str(&text).ok()
        })
        .collect();
    requests.sort_by_key(|request: &RerunRequest| request.requested_at);
    requests
}

/// Close a rerun request: the tier ran to a result, or the request was
/// withdrawn. `true` when there was one.
pub fn clear_rerun(localpilot_dir: &Path, candidate_identity: &str, tier: EvidenceTier) -> bool {
    std::fs::remove_file(rerun_path(localpilot_dir, candidate_identity, tier)).is_ok()
}

fn lab_source(source: Option<&localmind_core::AssignmentSource>) -> &'static str {
    use localmind_core::AssignmentSource;
    match source {
        Some(AssignmentSource::RecordedTrajectory { .. }) => "Logic, against the recorded run",
        Some(AssignmentSource::FailFixPair { .. }) => "Replay, on the failing commit and its fix",
        Some(AssignmentSource::RatifiedCheck { .. }) => "Replay, with a ratified check",
        Some(AssignmentSource::ControlledMutation { .. }) => "Replay, on a controlled change",
        Some(AssignmentSource::ApprovedTaskSet { .. }) => "Uplift, on its approved task set",
        None => "a test",
    }
}

/// The lab's part of a review item's cards: what it can run for this lesson
/// and any open rerun request. Empty when the lab has nothing to say, which is
/// the ordinary case — most lessons were never classified.
#[must_use]
pub fn lab_notes(root: &Path, item: &ReviewQueueItem) -> String {
    let localpilot_dir = localpilot_store::Store::open(root);
    let localpilot_dir = localpilot_dir.root();
    let identity = item.candidate.content_identity();
    let record = crate::lab_eligibility::read_records(localpilot_dir)
        .into_iter()
        .find(|record| record.candidate_identity == identity);
    let requests = rerun_requests(localpilot_dir, &identity);
    let runs: Vec<_> = crate::uplift_run::run_statuses(root)
        .into_iter()
        .filter(|(_, state, _)| state.candidate_identity == identity)
        .collect();
    if record.is_none() && requests.is_empty() && runs.is_empty() {
        return String::new();
    }
    let mut out = String::from("Lab\n");
    out.push_str(&format!("  lesson identity: {identity}\n"));
    if let Some(record) = &record {
        if record.assignments.is_empty() {
            out.push_str(
                "  No mechanical test exists for this lesson. That describes the lesson and is \
                 not a mark against it.\n",
            );
        }
        for assignment in &record.assignments {
            out.push_str(&format!(
                "  can run: {}\n",
                lab_source(assignment.source.as_ref())
            ));
        }
    }
    if !is_live(&item.state) {
        out.push_str("  This item is history: the lab runs nothing against it.\n");
    }
    for (dir, state, standing) in runs {
        use crate::uplift_run::RunStanding;
        let name = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let standing = match standing {
            RunStanding::Running => format!("running ({})", state.stage),
            RunStanding::Interrupted => {
                format!("interrupted during `{}` — not a result", state.stage)
            }
            RunStanding::Ended => state.verdict.clone().unwrap_or_default(),
        };
        out.push_str(&format!(
            "  uplift run {name}: {standing} (model {})\n",
            state.model
        ));
    }
    for request in requests {
        let tier = tier_name(request.tier);
        out.push_str(&format!(
            "  rerun requested: {tier} by {}{} — not run. Start it with `localpilot lab {tier} \
             {identity}`; it shows what will run and asks first.\n",
            request.requested_by,
            request
                .note
                .as_ref()
                .map(|note| format!(" ({note})"))
                .unwrap_or_default(),
        ));
    }
    out
}
