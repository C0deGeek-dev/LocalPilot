//! Uplift tasks for a lesson: a model may draft them, a person makes them final.
//!
//! An uplift run asks a model the same questions with and without the lesson and
//! grades the answers against fixed expectations. Nothing in a run's record or a
//! project's history is such a question, so the tasks have to be written. The
//! project's own model drafts them from the lesson and its hindsight; every reply
//! is validated, with one repair pass. But a draft is not an oracle and nothing
//! may run one. Only a named person's approval freezes it — by content hash, as
//! an assignment whose source says who approved it and which model drafted it.
//!
//! The checks here are deliberately mechanical. A question that already contains
//! its expected answer measures nothing, and a question that quotes the lesson
//! hands it to both arms. Whether the questions are *good* — whether a model
//! would get them wrong without the lesson, and whether the expected answer is
//! the behaviour the lesson is about — is the approver's judgement, which is why
//! there is one.

use std::path::{Path, PathBuf};

use futures::StreamExt;
use localmind_core::{
    AssignmentSource, CandidateLesson, FixtureRef, LessonAssignment, OracleOrigin, OracleRef,
    Sensitivity, VerifierRef, LESSON_ASSIGNMENT_VERSION,
};
use localmind_inference::extract_json_payload;
use localpilot_core::{Message, Role};
use localpilot_llm::{ModelEvent, ModelProvider, ModelRequest};
use localx_eval_core::uplift::{content_digest, UPLIFT_RECEIPT_SCHEMA};
use serde::{Deserialize, Serialize};

/// Where drafts and approved task sets are kept, under `.localpilot/`.
pub const LAB_TASKS_DIR: &str = "lab/tasks";
/// Fewest and most tasks a set may hold.
pub const MIN_TASKS: usize = 1;
pub const MAX_TASKS: usize = 8;
/// Longest question, and longest expected answer, in characters.
pub const MAX_PROMPT_CHARS: usize = 600;
pub const MAX_EXPECT_CHARS: usize = 160;
/// Shortest expected answer worth grading: shorter matches by accident.
pub const MIN_EXPECT_CHARS: usize = 3;
/// The lesson id every task names, and the seed lesson carries.
pub const CANDIDATE_LESSON_ID: &str = "candidate";

const TASK_SET_VERSION: u32 = 1;

/// One question and what a right answer must contain.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LabTask {
    pub id: String,
    pub prompt: String,
    /// The text a right answer contains (matched case-insensitively, with
    /// whitespace collapsed, by the uplift grader).
    pub expect: String,
}

/// A task set for one lesson: a draft until `approved_by` is set.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LabTaskSet {
    pub version: u32,
    /// The lesson the tasks were written for.
    pub candidate_identity: String,
    pub tasks: Vec<LabTask>,
    /// The model that drafted the tasks, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drafted_by: Option<String>,
    /// Who made the set final. `None` is a draft, which nothing may run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
    /// Unix seconds of the approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<i64>,
}

impl LabTaskSet {
    /// The hash an approval freezes: the lesson it is for and the tasks, and
    /// nothing about who drafted or approved them.
    #[must_use]
    pub fn content_hash(&self) -> String {
        content_digest(&(self.version, &self.candidate_identity, &self.tasks))
    }
}

/// What is wrong with a task set.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TaskProblem {
    #[error("the set was written for another version of the lesson")]
    WrongCandidate,
    #[error("unsupported task-set version {0}")]
    Version(u32),
    #[error("a set needs between {MIN_TASKS} and {MAX_TASKS} tasks, not {0}")]
    Count(usize),
    #[error("task {0}: the id is empty or repeated")]
    Id(String),
    #[error("task {0}: the question is empty or longer than {MAX_PROMPT_CHARS} characters")]
    Prompt(String),
    #[error(
        "task {0}: the expected answer must be {MIN_EXPECT_CHARS} to {MAX_EXPECT_CHARS} characters"
    )]
    Expect(String),
    #[error("task {0}: the question contains its own expected answer")]
    AnswerInPrompt(String),
    #[error("task {0}: the question quotes the lesson, which would give it to both arms")]
    LessonInPrompt(String),
    #[error("task {0}: the same question appears twice")]
    DuplicatePrompt(String),
}

fn normalise(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Check `set` is fit to approve for `candidate`. Returns every problem.
///
/// # Errors
/// The [`TaskProblem`]s found.
pub fn validate(set: &LabTaskSet, candidate: &CandidateLesson) -> Result<(), Vec<TaskProblem>> {
    let mut problems = Vec::new();
    if set.version != TASK_SET_VERSION {
        problems.push(TaskProblem::Version(set.version));
    }
    if set.candidate_identity != candidate.content_identity() {
        problems.push(TaskProblem::WrongCandidate);
    }
    if !(MIN_TASKS..=MAX_TASKS).contains(&set.tasks.len()) {
        problems.push(TaskProblem::Count(set.tasks.len()));
    }
    let lesson = normalise(candidate.summary());
    let mut ids = Vec::new();
    let mut prompts = Vec::new();
    for task in &set.tasks {
        let id = task.id.trim().to_string();
        if id.is_empty() || ids.contains(&id) {
            problems.push(TaskProblem::Id(task.id.clone()));
        }
        ids.push(id);
        let prompt = normalise(&task.prompt);
        let expect = normalise(&task.expect);
        if prompt.is_empty() || task.prompt.chars().count() > MAX_PROMPT_CHARS {
            problems.push(TaskProblem::Prompt(task.id.clone()));
        }
        let expect_chars = expect.chars().count();
        if !(MIN_EXPECT_CHARS..=MAX_EXPECT_CHARS).contains(&expect_chars) {
            problems.push(TaskProblem::Expect(task.id.clone()));
        } else if prompt.contains(&expect) {
            problems.push(TaskProblem::AnswerInPrompt(task.id.clone()));
        }
        if !lesson.is_empty() && prompt.contains(&lesson) {
            problems.push(TaskProblem::LessonInPrompt(task.id.clone()));
        }
        if prompts.contains(&prompt) {
            problems.push(TaskProblem::DuplicatePrompt(task.id.clone()));
        }
        prompts.push(prompt);
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Why no draft was produced.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DraftFailure {
    #[error("the model could not be reached: {0}")]
    Unavailable(String),
    #[error("the model's reply was not a usable task set after one repair: {0}")]
    Unusable(String),
}

/// A draft, and how many model calls it took.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Drafted {
    pub set: LabTaskSet,
    pub model_calls: u32,
    pub repaired: bool,
}

/// Have `provider` draft tasks for `candidate`. The reply is validated; a reply
/// that fails gets one repair request naming what was wrong, and a second
/// failure ends the attempt. The result is a draft: nothing may run it.
///
/// Only the lesson and its hindsight are sent — to the provider the project is
/// already configured to use — never the run's raw facts.
///
/// # Errors
/// [`DraftFailure`] when the model cannot be reached or keeps failing.
pub async fn draft_tasks(
    provider: &dyn ModelProvider,
    model: &str,
    candidate: &CandidateLesson,
) -> Result<Drafted, DraftFailure> {
    let mut messages = vec![
        Message::text(Role::System, SYSTEM_PROMPT.to_string()),
        Message::text(Role::User, user_prompt(candidate)),
    ];
    let mut last_problem = String::new();
    for attempt in 0..2u32 {
        let reply = ask(provider, model, &messages).await?;
        match parse_reply(&reply, candidate, model) {
            Ok(set) => {
                return Ok(Drafted {
                    set,
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

const SYSTEM_PROMPT: &str = "You write short test questions for a coding assistant. Each question \
must be one a capable assistant would likely answer wrongly without having been told a specific \
lesson, and rightly with it. Reply with one JSON object and nothing else: \
{\"tasks\":[{\"prompt\":\"...\",\"expect\":\"...\"}]}. Write 2 to 5 tasks. `prompt` is the \
question, phrased the way a user would ask it, and must not state the lesson or contain the \
expected answer. `expect` is a short exact phrase (3 to 160 characters) that a right answer \
contains and a wrong answer does not.";

fn user_prompt(candidate: &CandidateLesson) -> String {
    let mut text = format!("The lesson: {}\n", candidate.summary());
    if let Some(draft) = &candidate.hindsight {
        text.push_str(&format!("What was intended: {}\n", draft.intended_outcome));
        text.push_str(&format!("What happened: {}\n", draft.observed_outcome));
        for hypothesis in &draft.hypotheses {
            text.push_str(&format!("Why: {}\n", hypothesis.claim));
        }
        if let Some(intervention) = &draft.intervention {
            text.push_str(&format!("What would have avoided it: {intervention}\n"));
        }
    }
    text.push_str("Write the tasks.");
    text
}

#[derive(Deserialize)]
struct ReplyTask {
    prompt: String,
    expect: String,
}

#[derive(Deserialize)]
struct Reply {
    tasks: Vec<ReplyTask>,
}

fn parse_reply(
    reply: &str,
    candidate: &CandidateLesson,
    model: &str,
) -> Result<LabTaskSet, String> {
    let payload = extract_json_payload(reply).ok_or("it holds no JSON object")?;
    let parsed: Reply = serde_json::from_str(payload).map_err(|error| error.to_string())?;
    let set = LabTaskSet {
        version: TASK_SET_VERSION,
        candidate_identity: candidate.content_identity(),
        tasks: parsed
            .tasks
            .into_iter()
            .enumerate()
            .map(|(index, task)| LabTask {
                id: format!("t{}", index + 1),
                prompt: task.prompt.trim().to_string(),
                expect: task.expect.trim().to_string(),
            })
            .collect(),
        drafted_by: Some(model.to_string()),
        approved_by: None,
        approved_at: None,
    };
    validate(&set, candidate).map_err(|problems| {
        problems
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    Ok(set)
}

async fn ask(
    provider: &dyn ModelProvider,
    model: &str,
    messages: &[Message],
) -> Result<String, DraftFailure> {
    let mut stream = provider
        .stream(ModelRequest::new(model, messages.to_vec()))
        .await
        .map_err(|error| DraftFailure::Unavailable(error.to_string()))?;
    let mut content = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(ModelEvent::TextDelta(delta)) => content.push_str(&delta),
            Ok(ModelEvent::Done) => break,
            Ok(_) => {}
            Err(error) => return Err(DraftFailure::Unavailable(error.to_string())),
        }
    }
    Ok(content)
}

/// Where a lesson's draft is kept.
#[must_use]
pub fn draft_path(localpilot_dir: &Path, candidate_identity: &str) -> PathBuf {
    localpilot_dir
        .join(LAB_TASKS_DIR)
        .join(format!("{candidate_identity}.draft.json"))
}

/// Where a lesson's approved task set is kept.
#[must_use]
pub fn approved_path(localpilot_dir: &Path, candidate_identity: &str) -> PathBuf {
    localpilot_dir
        .join(LAB_TASKS_DIR)
        .join(format!("{candidate_identity}.approved.json"))
}

/// Write `set` as the lesson's draft, for a person to read and edit.
///
/// # Errors
/// An I/O or encoding error.
pub fn write_draft(localpilot_dir: &Path, set: &LabTaskSet) -> std::io::Result<PathBuf> {
    let path = draft_path(localpilot_dir, &set.candidate_identity);
    write_set(&path, set)?;
    Ok(path)
}

fn write_set(path: &Path, set: &LabTaskSet) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(set).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

/// Read a task set file. `None` when there is none.
///
/// # Errors
/// The file exists and is not a task set.
pub fn read_set(path: &Path) -> Result<Option<LabTaskSet>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| format!("{} is not a task set: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{} cannot be read: {error}", path.display())),
    }
}

/// Why an approval was refused.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ApprovalRefusal {
    #[error("an approval needs the approver's name")]
    NoApprover,
    #[error("there is no draft to approve; draft the tasks first")]
    NoDraft,
    #[error("{0}")]
    Unreadable(String),
    #[error("the draft cannot be approved: {}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
    Invalid(Vec<TaskProblem>),
    #[error("the approved set could not be written: {0}")]
    Write(String),
}

/// Make the lesson's draft — as it now stands on disk, edits included — final,
/// in `approver`'s name, and return the frozen uplift assignment built from it.
/// The draft is validated again: an edit can break it as easily as a model can.
///
/// # Errors
/// An [`ApprovalRefusal`].
pub fn approve_tasks(
    localpilot_dir: &Path,
    candidate: &CandidateLesson,
    approver: &str,
    approved_at: i64,
) -> Result<(LabTaskSet, LessonAssignment), ApprovalRefusal> {
    let approver = approver.trim();
    if approver.is_empty() {
        return Err(ApprovalRefusal::NoApprover);
    }
    let identity = candidate.content_identity();
    let mut set = read_set(&draft_path(localpilot_dir, &identity))
        .map_err(ApprovalRefusal::Unreadable)?
        .ok_or(ApprovalRefusal::NoDraft)?;
    validate(&set, candidate).map_err(ApprovalRefusal::Invalid)?;
    set.approved_by = Some(approver.to_string());
    set.approved_at = Some(approved_at);
    write_set(&approved_path(localpilot_dir, &identity), &set)
        .map_err(|error| ApprovalRefusal::Write(error.to_string()))?;
    let assignment = uplift_assignment(candidate, &set);
    Ok((set, assignment))
}

/// The lesson's approved task set, when it has one that still holds: approved,
/// written for this version of the lesson, and valid.
///
/// # Errors
/// The approved file exists but is unreadable or no longer valid.
pub fn approved_tasks(
    localpilot_dir: &Path,
    candidate: &CandidateLesson,
) -> Result<Option<LabTaskSet>, String> {
    let Some(set) = read_set(&approved_path(
        localpilot_dir,
        &candidate.content_identity(),
    ))?
    else {
        return Ok(None);
    };
    if set.approved_by.is_none() {
        return Err("the stored task set carries no approval".to_string());
    }
    validate(&set, candidate).map_err(|problems| {
        problems
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    Ok(Some(set))
}

/// The uplift assignment an approved task set freezes. Its oracle is the
/// approved content by hash; its fixture is the lesson that will be seeded.
#[must_use]
pub fn uplift_assignment(candidate: &CandidateLesson, set: &LabTaskSet) -> LessonAssignment {
    let identity = candidate.content_identity();
    LessonAssignment {
        version: LESSON_ASSIGNMENT_VERSION,
        candidate_identity: identity.clone(),
        task: format!(
            "Answer {} approved question(s) with and without the lesson",
            set.tasks.len()
        ),
        task_evidence: Vec::new(),
        oracle: OracleRef {
            locator: format!("approved-task-set:{identity}"),
            content_hash: set.content_hash(),
            // Ratified by a person. A model may have drafted it; the source
            // says so.
            origin: OracleOrigin::Human,
        },
        fixture: FixtureRef {
            locator: "seed-lesson".to_string(),
            content_hash: content_digest(&candidate.summary()),
        },
        initial_state: "an empty workspace whose memory store holds nothing".to_string(),
        allowed_tools: Vec::new(),
        success_observations: vec![
            "the lesson arm's answers contain the expected text more often than the baseline's"
                .to_string(),
        ],
        failure_observations: Vec::new(),
        verifier: VerifierRef {
            name: "localbench-uplift".to_string(),
            version: UPLIFT_RECEIPT_SCHEMA.to_string(),
        },
        cleanup: "Remove the trial workspace and its memory store".to_string(),
        sensitivity: Sensitivity::LocalOnly,
        source: Some(AssignmentSource::ApprovedTaskSet {
            approved_by: set.approved_by.clone().unwrap_or_default(),
            drafted_by: set.drafted_by.clone(),
        }),
        preconditions: Vec::new(),
        counterfactual: candidate
            .hindsight
            .as_ref()
            .and_then(|draft| draft.counterfactual_prediction.clone()),
    }
}
