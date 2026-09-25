//! Hindsight over a finished harness run: drive LocalMind's distiller with the
//! model the run already used, and offer what it earns to review.
//!
//! LocalMind owns the contract — the prompts, validation, the one repair pass,
//! the fallback, and the abstention check that decides whether a lesson is
//! earned. This module is only the transport and the hand-off: it turns each
//! request into a call on the harness provider, reports honestly what became of
//! any output constraint, and passes the finished distillation to the same
//! review-gated bridge the retrospective has always used.
//!
//! The provider is the one the run was configured with, so nothing new leaves
//! the machine that the run did not already send there: the facts are the run's
//! own events, redacted before capture.

use std::path::Path;

use futures::StreamExt;
use localmind_core::HindsightOutcome;
use localmind_inference::{ChatMessage, ConstraintDisposition};
use localmind_store::{
    DistillInput, DistillPlan, DistillReply, DistillRequest, DistillStep, Distillation, Distiller,
    InputGap, ProjectConfig, ReviewQueue,
};
use localpilot_core::{Message, Role};
use localpilot_llm::{ModelEvent, ModelProvider, ModelRequest};

use crate::error::LearningError;
use crate::retrospective_lesson::{write_retrospective_lesson, RetrospectiveLesson};
use crate::RunFacts;

/// What happened to a run's hindsight.
#[derive(Clone, Debug, PartialEq)]
pub struct HindsightOffer {
    pub distillation: Distillation,
    /// The review-queue id, when a candidate or review-only record was queued
    /// and was not already pending.
    pub enqueued: Option<String>,
    /// The lesson to mirror into `LESSONS.md` — set only for a `Candidate`.
    pub lesson: Option<String>,
    /// Whether and how the lesson can be tested, for a lesson that reached
    /// review. `None` for an abstention or a review-only record.
    pub lab: Option<crate::LabClassification>,
}

/// Distil `run` with `provider`. Never fails: an unreachable model or a reply
/// that keeps breaking the contract ends in LocalMind's fallback, which keeps
/// the facts and proposes nothing.
///
/// The plan adapts to what the provider declares: its context picks one pass or
/// the staged passes, and a declared constrained-decoding capability has the
/// output schema attempted.
pub async fn distil_run(
    provider: &dyn ModelProvider,
    model: &str,
    run: &RunFacts,
    intended: &str,
    observed: &str,
) -> Distillation {
    let declaration = provider.declaration();
    let plan = DistillPlan::adaptive(
        declaration
            .max_context_tokens
            .map(|tokens| u32::try_from(tokens).unwrap_or(u32::MAX)),
        declaration.capabilities.constrained_decoding,
    );
    distil_run_with(provider, model, run, intended, observed, plan).await
}

/// [`distil_run`] with an explicit plan, for a caller that has measured which
/// strategy an endpoint does better with.
pub async fn distil_run_with(
    provider: &dyn ModelProvider,
    model: &str,
    run: &RunFacts,
    intended: &str,
    observed: &str,
    plan: DistillPlan,
) -> Distillation {
    let input = DistillInput {
        facts: run.facts.clone(),
        gaps: run
            .gaps
            .iter()
            .map(|gap| InputGap {
                description: gap.describe(),
                incompleteness: gap.incompleteness(),
            })
            .collect(),
        intended: intended.to_string(),
        observed: observed.to_string(),
    };
    let mut distiller = Distiller::new(input, plan);
    let mut step = distiller.start();
    loop {
        match step {
            DistillStep::Ask(request) => {
                let reply = send(provider, model, request).await;
                step = distiller.reply(reply);
            }
            DistillStep::Done(distillation) => return *distillation,
        }
    }
}

/// Distil `run` and offer the result to the project's review queue under the
/// project's `[review] record_abstentions` setting.
///
/// # Errors
/// [`LearningError::Config`] when learning is off for the project — checked
/// before any model call, so a disabled project spends nothing — and
/// [`LearningError::Review`] when the queue cannot take the record.
pub async fn offer_run_hindsight(
    project_root: &Path,
    provider: &dyn ModelProvider,
    model: &str,
    run: &RunFacts,
    run_name: &str,
    intended: &str,
    observed: &str,
) -> Result<HindsightOffer, LearningError> {
    crate::initialize(project_root)?;
    let record_abstentions = ProjectConfig::discover(project_root)
        .map_err(|error| LearningError::Config(error.to_string()))?
        .config
        .review
        .record_abstentions;

    let distillation = distil_run(provider, model, run, intended, observed).await;
    let lesson = (distillation.outcome == HindsightOutcome::Candidate)
        .then(|| distillation.draft.proposed_lesson.clone())
        .flatten();
    let record =
        RetrospectiveLesson::from_hindsight(&distillation, run, run_name, record_abstentions);
    let enqueued = match &record {
        Some(record) => write_retrospective_lesson(project_root, record)?,
        None => None,
    };
    let lab = match (&lesson, &record) {
        (Some(_), Some(record)) => classify_queued(project_root, &record.id())?,
        _ => None,
    };
    Ok(HindsightOffer {
        distillation,
        enqueued,
        lesson,
        lab,
    })
}

/// Classify the lesson queued as `item_id` for the lab, keep the frozen record
/// for the runs that come later, and — when no honest test exists — put that
/// result on the candidate so review shows it.
///
/// Reads the stored candidate, not the one handed to the queue: the identity
/// every assignment binds to is the identity of what review holds.
fn classify_queued(
    project_root: &Path,
    item_id: &str,
) -> Result<Option<crate::LabClassification>, LearningError> {
    let queue = ReviewQueue::open_project(project_root)
        .map_err(|e| LearningError::Review(e.to_string()))?;
    let Some(item) = queue
        .get(&localmind_core::ReviewItemId::new(item_id))
        .map_err(|e| LearningError::Review(e.to_string()))?
    else {
        // Merged into a near-duplicate under another id; that row keeps its
        // own classification.
        return Ok(None);
    };
    let checks = localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(project_root),
        &localpilot_config::CliOverrides::default(),
    )
    .map(|config| config.harness.checks)
    .unwrap_or_default();
    let progress = std::fs::read_to_string(project_root.join("PROGRESS.md"))
        .ok()
        .and_then(|text| localpilot_harness::Progress::parse(&text).ok());
    let context = crate::LabContext {
        root: project_root,
        progress: progress.as_ref(),
        checks: &checks,
    };
    let classification = crate::classify_for_lab(&item.candidate, &context);

    let store = localpilot_store::Store::open(project_root);
    crate::lab_eligibility::write_record(store.root(), &classification)
        .map_err(|e| LearningError::Review(format!("could not keep the lab record: {e}")))?;
    let produced_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        });
    if let Some(evidence) = crate::lab_eligibility::not_executable_evidence(
        &classification,
        &crate::lab_eligibility::current_revision(project_root),
        produced_at,
    ) {
        // A restatement carrying a new result: the queue merges the result into
        // the pending row rather than creating a second one.
        queue
            .enqueue_candidates(
                &item.session_id,
                &[item.candidate.with_experiment(evidence)],
            )
            .map_err(|e| LearningError::Review(e.to_string()))?;
    }
    Ok(Some(classification))
}

/// One request over the provider, and an honest account of the constraint.
async fn send(provider: &dyn ModelProvider, model: &str, request: DistillRequest) -> DistillReply {
    let refused_before = provider.constraint_refused();
    let constraint = request
        .schema
        .as_ref()
        .filter(|_| !refused_before)
        .map(|schema| schema.schema().clone());
    let sent_constraint = constraint.is_some();
    let messages = request.messages.iter().map(to_message).collect();
    let model_request = ModelRequest::new(model, messages).with_tool_constraint(constraint);

    let mut stream = match provider.stream(model_request).await {
        Ok(stream) => stream,
        Err(error) => {
            return DistillReply::Unavailable {
                detail: error.to_string(),
            }
        }
    };
    let mut content = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(ModelEvent::TextDelta(delta)) => content.push_str(&delta),
            Ok(ModelEvent::Done) => break,
            Ok(_) => {}
            Err(error) => {
                return DistillReply::Unavailable {
                    detail: error.to_string(),
                }
            }
        }
    }

    let disposition = if !sent_constraint {
        ConstraintDisposition::NotRequested
    } else if provider.constraint_refused() {
        ConstraintDisposition::RefusedByTransport
    } else {
        ConstraintDisposition::Requested
    };
    DistillReply::Text {
        content,
        disposition,
    }
}

fn to_message(message: &ChatMessage) -> Message {
    let role = match message.role.as_str() {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        _ => Role::User,
    };
    Message::text(role, message.content.clone())
}
