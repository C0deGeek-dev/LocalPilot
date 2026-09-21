//! Turning an idea into a brief the user has actually seen.
//!
//! The shipped intake path generated a brief and wrote `brief.md` in the same
//! breath, so the first time anyone saw what the model produced, it was already
//! the project's truth. There was nothing to discuss, nothing to reject, and no
//! way to say "close, but the acceptance criteria are wrong" short of editing
//! the file afterwards.
//!
//! This module owns the half of that flow which produces a brief without
//! committing to it. A [`BriefDraft`] exists only in the session that made it;
//! [`persist_approved`] is the only thing here that touches the project, and it
//! runs after a person has approved the exact text.
//!
//! Two properties the hosts depend on:
//!
//! * **Generation never writes.** Drafting, revising, and recovering from a bad
//!   model reply leave the project byte-identical. A user who cancels has lost
//!   nothing but their own time.
//! * **The stage is a type, not a convention.** [`BriefStage`] enumerates where
//!   a conversation actually is, including the two states where a model call is
//!   in flight and the one where an attempt failed but the work is still
//!   recoverable. A host reads the type rather than inferring position from
//!   which fields happen to be set.
//!
//! Presentation and input collection stay with the host. Nothing here prints,
//! prompts, or knows what a terminal is.

use serde_json::json;

use localpilot_llm::ModelProvider;

use crate::brief::Brief;
use crate::error::HarnessError;
use crate::guidance::{assess_guidance, DecisionAxis, GuidanceAssessment};
use crate::planning::run_intake;

/// The guidance gate's parameters for one drafting run.
///
/// The same numbers the CLI has always used, passed explicitly so a host does
/// not have to reach into configuration to reproduce them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuidanceParams {
    /// Minimum score that proceeds straight to a draft. Clamped to `0..=1`.
    pub threshold: f32,
    /// Cap on how many open questions are put to the user. Floored at 1.
    pub max_questions: usize,
}

/// What the guidance gate recorded about one drafting run.
///
/// Kept beside the draft so approval can write the same `intake.jsonl` record
/// the CLI has always written — the audit trail is a shipped contract, and a
/// reviewed brief must not lose the provenance an unreviewed one had.
#[derive(Debug, Clone, PartialEq)]
pub struct GuidanceRecord {
    /// The assessment's score, or `None` when the gate was off.
    pub score: Option<f32>,
    /// The threshold in force, clamped as it was applied.
    pub threshold: Option<f32>,
    /// Every axis the model identified.
    pub axes: Vec<DecisionAxis>,
    /// The questions that were put, capped exactly as they were asked.
    ///
    /// `None` when no clarification leg opened at all — the gate was off, or
    /// the idea was at or above the threshold. `Some` otherwise, empty list
    /// included: every below-threshold leg records the questions it put, and a
    /// reader of the log distinguishes "asked nothing" from "never got there".
    pub questions: Option<Vec<String>>,
    /// Answers the user gave, as `(axis, answer)`. Empty when none were asked.
    pub answers: Vec<(String, String)>,
    /// Whether the open decisions were delegated to the model's judgment.
    pub assumed_judgment: bool,
    /// Whether this run went through the clarification leg at all.
    ///
    /// The shipped log distinguishes "asked, and every answer was delegated"
    /// from "never asked, judgment assumed up front": the former always carries
    /// an `answers` array, empty or not. Losing that distinction would make two
    /// different runs read identically in the audit trail.
    pub clarified: bool,
    /// The re-assessment score after answers were folded in, when one ran.
    pub rescore: Option<f32>,
}

impl GuidanceRecord {
    /// The gate-off record: no assessment happened.
    #[must_use]
    pub fn none() -> Self {
        Self {
            score: None,
            threshold: None,
            axes: Vec::new(),
            questions: None,
            answers: Vec::new(),
            assumed_judgment: false,
            clarified: false,
            rescore: None,
        }
    }

    /// The record for a below-threshold leg that reported its questions and
    /// drafted nothing.
    ///
    /// Shared with the drafting legs so the reported leg cannot drift into a
    /// second shape: `localpilot harness intake --clarify=emit` and the
    /// conversation write the same `guidance` object for the same facts.
    #[must_use]
    pub fn reported(
        score: f32,
        threshold: f32,
        axes: Vec<DecisionAxis>,
        questions: Vec<String>,
    ) -> Self {
        Self {
            score: Some(score),
            threshold: Some(threshold.clamp(0.0, 1.0)),
            axes,
            questions: Some(questions),
            answers: Vec::new(),
            assumed_judgment: false,
            clarified: false,
            rescore: None,
        }
    }

    /// The `guidance` object as it appears in `.localpilot/intake.jsonl`.
    ///
    /// Field-for-field what the CLI wrote before this module existed: absent
    /// keys stay absent rather than becoming nulls, because a reader of the log
    /// distinguishes "not asked" from "asked and empty".
    #[must_use]
    pub fn to_json(&self) -> Option<serde_json::Value> {
        let (score, threshold) = (self.score?, self.threshold?);
        let mut value = json!({
            "score": score,
            "threshold": threshold,
            "axes": self.axes,
        });
        // Every below-threshold leg carried its questions before this module
        // existed, and a log reader keys off them; an empty list still means
        // "the leg ran", so the key is present whenever it did.
        if let Some(questions) = &self.questions {
            value["questions"] = json!(questions);
        }
        // The clarification leg always records its answers, even when every one
        // was delegated and the array is empty.
        if self.clarified || !self.answers.is_empty() {
            value["answers"] = json!(self
                .answers
                .iter()
                .map(|(axis, answer)| json!({ "axis": axis, "answer": answer }))
                .collect::<Vec<_>>());
        }
        if self.assumed_judgment {
            value["assumed_judgment"] = json!(true);
        }
        if let Some(rescore) = self.rescore {
            value["rescore"] = json!(rescore);
        }
        Some(value)
    }
}

/// A brief that exists only in this session.
///
/// It is not the project's brief until someone approves it, and until then the
/// project does not know it exists.
#[derive(Debug, Clone, PartialEq)]
pub struct BriefDraft {
    /// The validated brief as it currently stands.
    pub brief: Brief,
    /// The user's own idea, as they gave it. This is what the audit log records,
    /// so the trail says what a person asked for rather than what was assembled
    /// to ask the model.
    pub idea: String,
    /// What was actually sent to the model — the idea plus any decisions folded
    /// in. Equal to `idea` when nothing was folded.
    pub model_input: String,
    /// What the guidance gate found, carried through to the audit record.
    pub guidance: GuidanceRecord,
    /// How many accepted revisions this draft has been through.
    pub revisions: usize,
}

/// What one drafting attempt produced.
#[derive(Debug, Clone, PartialEq)]
pub enum DraftOutcome {
    /// A validated draft, ready to show.
    Drafted(Box<BriefDraft>),
    /// The idea does not settle enough decisions to draft from. The caller
    /// decides what to do: ask the questions, report them, or delegate.
    NeedsGuidance {
        /// The full assessment, for inspection alongside the number.
        assessment: Box<GuidanceAssessment>,
        /// The open axes, already capped at `max_questions`.
        open: Vec<DecisionAxis>,
        /// One question per open axis, in the same order.
        questions: Vec<String>,
    },
}

/// Where a brief conversation currently is.
///
/// Every live position is named, including the two where a model call is in
/// flight: a host has to attribute input submitted *while the model is
/// thinking* to the conversation that was thinking, and it cannot do that from
/// a state that does not exist.
#[derive(Debug, Clone, PartialEq)]
pub enum BriefStage {
    /// No idea supplied yet; the next input is the idea.
    AwaitingIdea,
    /// The guidance gate found open decisions and the user is answering them.
    ///
    /// The idea, the threshold and the assessment travel with the questions.
    /// Without them the leg cannot finish the way `localpilot harness intake`
    /// does: the record would lose its score, threshold, axes and re-assessment,
    /// and the brief would be drafted from the answers alone rather than from
    /// the idea they qualify.
    Clarifying {
        /// The user's idea, unchanged.
        idea: String,
        /// The threshold in force, already clamped.
        threshold: f32,
        /// The assessment the questions came from.
        assessment: Box<GuidanceAssessment>,
        /// Every question put, capped, in the order they were asked. Fixed for
        /// the life of the leg: `pending` shrinks as they are answered, and the
        /// record needs what was asked, not what is left.
        questions: Vec<String>,
        /// Axes still unanswered, in the order they are asked.
        pending: Vec<DecisionAxis>,
        /// Answers collected so far, as `(axis, answer)`.
        answers: Vec<(String, String)>,
    },
    /// A first draft is being generated.
    Generating,
    /// A draft is on screen, awaiting discussion, revision, or a decision.
    Reviewing(Box<BriefDraft>),
    /// A revision of the shown draft is being generated.
    Revising(Box<BriefDraft>),
    /// An attempt failed and the work survives.
    ///
    /// A provider error, a malformed reply, or an exhausted repair budget is a
    /// machine failure, not a decision, so it must not throw away what the user
    /// typed. The stage stays live, and it carries enough to run the failed
    /// attempt again — restoring the state without being able to retry it would
    /// leave the user holding their work with nothing to do with it.
    RecoverableFailure {
        /// What to run again.
        retry: RetryTarget,
        /// Why it failed, in the caller's words.
        detail: String,
    },
}

/// The attempt a recoverable failure can repeat.
///
/// Each variant carries its own inputs, so a retry reruns what actually failed
/// rather than something reassembled from what happened to survive.
#[derive(Debug, Clone, PartialEq)]
pub enum RetryTarget {
    /// Drafting from an idea failed.
    Draft { idea: String },
    /// Drafting after clarification failed; everything the record needs is here.
    Clarified {
        idea: String,
        threshold: f32,
        assessment: Box<GuidanceAssessment>,
        questions: Vec<String>,
        answers: Vec<(String, String)>,
    },
    /// Revising a draft failed. The draft is untouched and the instruction is
    /// kept, so an empty message retries it verbatim.
    Revision {
        draft: Box<BriefDraft>,
        instruction: String,
    },
}

/// How a brief conversation ended.
///
/// All four are a person's decision or a newer conversation replacing this one.
/// A failure is not here, deliberately: a machine fault does not get to end
/// something a person started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageOutcome {
    /// The reviewed draft became `brief.md`.
    Approved,
    /// The user rejected the draft. Any existing brief is untouched.
    Rejected,
    /// The user left the conversation.
    Cancelled,
    /// A newer conversation replaced this one.
    Superseded,
}

impl StageOutcome {
    /// A short reason, for telling a queued input why its conversation is gone.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::Approved => "the brief was approved",
            Self::Rejected => "the draft was rejected",
            Self::Cancelled => "the brief conversation was cancelled",
            Self::Superseded => "a newer brief conversation replaced this one",
        }
    }
}

/// Produce a draft from an idea, writing nothing.
///
/// With `gate: None` the guidance assessment is skipped entirely, matching the
/// behaviour of a gate-off CLI run.
///
/// # Errors
/// Returns [`HarnessError::Provider`] when the provider fails or never produces
/// a valid brief within the repair budget.
pub async fn draft_brief(
    provider: &dyn ModelProvider,
    model: &str,
    idea: &str,
    gate: Option<GuidanceParams>,
) -> Result<DraftOutcome, HarnessError> {
    let Some(gate) = gate else {
        let brief = run_intake(provider, model, idea).await?;
        return Ok(DraftOutcome::Drafted(Box::new(BriefDraft {
            brief,
            idea: idea.to_string(),
            model_input: idea.to_string(),
            guidance: GuidanceRecord::none(),
            revisions: 0,
        })));
    };

    let threshold = gate.threshold.clamp(0.0, 1.0);
    let assessment = assess_guidance(provider, model, idea).await?;
    if assessment.score >= threshold {
        let brief = run_intake(provider, model, idea).await?;
        return Ok(DraftOutcome::Drafted(Box::new(BriefDraft {
            brief,
            idea: idea.to_string(),
            model_input: idea.to_string(),
            guidance: GuidanceRecord {
                score: Some(assessment.score),
                threshold: Some(threshold),
                axes: assessment.axes,
                // Nothing was asked: the idea settled enough on its own.
                questions: None,
                answers: Vec::new(),
                assumed_judgment: false,
                clarified: false,
                rescore: None,
            },
            revisions: 0,
        })));
    }

    let open: Vec<DecisionAxis> = assessment
        .open_axes()
        .into_iter()
        .take(gate.max_questions.max(1))
        .cloned()
        .collect();
    let questions = open.iter().map(question_for).collect();
    Ok(DraftOutcome::NeedsGuidance {
        assessment: Box::new(assessment),
        open,
        questions,
    })
}

/// Draft from an idea plus the answers a user gave to the open questions.
///
/// Empty answers are dropped by the caller before they get here; when none
/// remain, the run is recorded as delegated judgment, exactly as
/// `--assume-judgment` is.
///
/// # Errors
/// Returns [`HarnessError::Provider`] when the provider fails or never produces
/// a valid brief within the repair budget.
#[allow(clippy::too_many_arguments)]
pub async fn draft_with_answers(
    provider: &dyn ModelProvider,
    model: &str,
    idea: &str,
    threshold: f32,
    assessment: GuidanceAssessment,
    questions: Vec<String>,
    answers: Vec<(String, String)>,
    clarified: bool,
) -> Result<BriefDraft, HarnessError> {
    let (brief_idea, rescore, assumed) = if answers.is_empty() {
        (idea.to_string(), None, true)
    } else {
        let decisions = answers
            .iter()
            .map(|(axis, answer)| format!("- {axis}: {answer}"))
            .collect::<Vec<_>>()
            .join("\n");
        let folded = format!("{idea}\n\nDecisions provided by the user:\n{decisions}");
        // One bounded re-assessment records whether the answers settled the open
        // axes. It informs the record; it does not re-gate, so there is no loop.
        let second = assess_guidance(provider, model, &folded).await?;
        (folded, Some(second.score), false)
    };

    let brief = run_intake(provider, model, &brief_idea).await?;
    Ok(BriefDraft {
        brief,
        // The user's words, not the assembled prompt: the audit trail records
        // what was asked for.
        idea: idea.to_string(),
        model_input: brief_idea,
        guidance: GuidanceRecord {
            score: Some(assessment.score),
            threshold: Some(threshold.clamp(0.0, 1.0)),
            axes: assessment.axes,
            // This leg is below the threshold by construction, so the questions
            // it put are part of its record even when it put none.
            questions: Some(questions),
            answers,
            assumed_judgment: assumed,
            clarified,
            rescore,
        },
        revisions: 0,
    })
}

/// The revision prompt. Original to this repository.
const REVISE_PROMPT: &str = "\
You are revising an existing project brief. You are given the current brief and \
one instruction describing what to change.\n\
\n\
Apply exactly that instruction. Leave everything the instruction does not \
mention exactly as it is — same wording, same order, same items. Do not add \
requirements, criteria, or risks that were not asked for, and do not tidy or \
reword sections you were not asked to change.\n\
\n\
Respond with ONLY the complete revised Markdown brief, in the same shape as the \
one you were given, and nothing else.";

/// Apply one revision instruction to a draft, writing nothing.
///
/// The model is given the current brief as text and one instruction, and the
/// reply is parsed and validated like any other generated document — so a
/// revision cannot introduce a brief the rest of the system cannot read.
///
/// # Errors
/// Returns [`HarnessError::Provider`] when the provider fails or never produces
/// a valid brief within the repair budget.
pub async fn revise_brief(
    provider: &dyn ModelProvider,
    model: &str,
    draft: &BriefDraft,
    instruction: &str,
) -> Result<BriefDraft, HarnessError> {
    use localpilot_core::{Message, Role};

    let user = format!(
        "Current brief:\n\n{}\n\nInstruction:\n\n{instruction}",
        draft.brief.render()
    );
    let seed = vec![
        Message::text(Role::System, REVISE_PROMPT),
        Message::text(Role::User, user),
    ];
    let brief = crate::planning::generate(provider, model, seed, "brief.md", Brief::parse).await?;
    Ok(BriefDraft {
        brief,
        idea: draft.idea.clone(),
        model_input: draft.model_input.clone(),
        guidance: draft.guidance.clone(),
        revisions: draft.revisions + 1,
    })
}

/// Write an approved draft to the project.
///
/// The only function here that touches the project, and it runs only after a
/// person approved this exact text. The brief is replaced atomically — a plain
/// truncating write would leave a half-written `brief.md` if the process died
/// mid-approval, destroying the document by accepting it.
///
/// The `intake.jsonl` record is appended separately and keeps its shape: it is
/// an append-only audit log, and a reviewed brief must carry the same
/// provenance an unreviewed one did.
///
/// # Errors
/// Returns [`Approval::BriefNotWritten`] when nothing changed, or
/// [`Approval::RecordNotAppended`] when the brief was replaced but its audit
/// record was not. The two are separate because they call for different things:
/// the first can be retried, and the second must not be, since retrying would
/// rewrite a brief that is already correct.
pub fn persist_approved(root: &std::path::Path, draft: &BriefDraft) -> Result<(), Approval> {
    let path = root.join("brief.md");
    localpilot_store::atomic_write(&path, draft.brief.render().as_bytes()).map_err(|error| {
        Approval::BriefNotWritten(HarnessError::Io {
            path: path.display().to_string(),
            source: std::io::Error::other(error.to_string()),
        })
    })?;

    let mut record = json!({ "idea": draft.idea, "name": draft.brief.name });
    if let Some(guidance) = draft.guidance.to_json() {
        record["guidance"] = guidance;
    }
    append_intake_record(root, &record).map_err(Approval::RecordNotAppended)
}

/// How an approval failed.
///
/// Approval touches two files, and a caller has to tell the user which of them
/// changed. Reporting "brief.md was not written" after replacing it is worse
/// than reporting nothing.
#[derive(Debug, thiserror::Error)]
pub enum Approval {
    /// Nothing was written; the project is exactly as it was.
    #[error("brief.md was not written: {0}")]
    BriefNotWritten(#[source] HarnessError),
    /// `brief.md` now holds the approved brief, but the audit record was not
    /// appended. The brief is saved; only its provenance is missing.
    #[error("brief.md was saved, but its intake record was not appended: {0}")]
    RecordNotAppended(#[source] HarnessError),
}

/// Append one line to the `.localpilot/intake.jsonl` provenance log.
///
/// # Errors
/// Returns [`HarnessError::Io`] if the log cannot be created or appended to.
pub fn append_intake_record(
    root: &std::path::Path,
    record: &serde_json::Value,
) -> Result<(), HarnessError> {
    use std::io::Write as _;

    let dir = root.join(".localpilot");
    let io = |path: &std::path::Path, source: std::io::Error| HarnessError::Io {
        path: path.display().to_string(),
        source,
    };
    std::fs::create_dir_all(&dir).map_err(|source| io(&dir, source))?;
    let path = dir.join("intake.jsonl");
    let mut line = serde_json::to_string(record)
        .map_err(|error| HarnessError::Provider(format!("intake record is not JSON: {error}")))?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|source| io(&path, source))?;
    file.write_all(line.as_bytes())
        .map_err(|source| io(&path, source))
}

/// The question asked for an open axis: the model's own settling question when
/// it supplied one, otherwise a generic prompt naming the axis.
#[must_use]
pub fn question_for(axis: &DecisionAxis) -> String {
    if axis.question.trim().is_empty() {
        format!("What should be decided about: {}?", axis.axis)
    } else {
        axis.question.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> BriefDraft {
        BriefDraft {
            brief: Brief {
                name: "thing".to_string(),
                summary: "Do the thing.".to_string(),
                requirements: vec!["It works".to_string()],
                constraints: vec!["Be small".to_string()],
                non_goals: vec!["World peace".to_string()],
                acceptance_criteria: vec!["A test passes".to_string()],
                risks: Vec::new(),
            },
            idea: "do a thing".to_string(),
            model_input: "do a thing".to_string(),
            guidance: GuidanceRecord::none(),
            revisions: 0,
        }
    }

    #[test]
    fn a_gate_off_record_carries_no_guidance_object() {
        // The log distinguishes "the gate did not run" from "it ran and found
        // nothing", so an absent gate must not synthesise an empty object.
        assert!(draft().guidance.to_json().is_none());
    }

    #[test]
    fn the_guidance_record_omits_keys_it_has_nothing_for() {
        let record = GuidanceRecord {
            score: Some(0.4),
            threshold: Some(0.7),
            axes: Vec::new(),
            questions: None,
            answers: Vec::new(),
            assumed_judgment: false,
            clarified: false,
            rescore: None,
        };
        let json = record.to_json().unwrap();
        assert!(json.get("answers").is_none(), "{json}");
        assert!(json.get("questions").is_none(), "nothing was asked: {json}");
        assert!(json.get("assumed_judgment").is_none(), "{json}");
        assert!(json.get("rescore").is_none(), "{json}");
        // `f32` widens to `f64` on the way into JSON, so compare at the width
        // the value actually has. The field stays `f32` because that is what the
        // shipped log records and this record must match it byte for byte.
        assert!((json["score"].as_f64().unwrap() as f32 - 0.4).abs() < f32::EPSILON);
        assert!((json["threshold"].as_f64().unwrap() as f32 - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn a_delegated_run_is_recorded_as_assumed_judgment() {
        let record = GuidanceRecord {
            score: Some(0.2),
            threshold: Some(0.7),
            axes: Vec::new(),
            questions: Some(vec!["Which platform?".to_string()]),
            answers: Vec::new(),
            assumed_judgment: true,
            clarified: true,
            rescore: None,
        };
        let json = record.to_json().unwrap();
        assert_eq!(json["assumed_judgment"], true);
        // Delegation is delegation *of* something: the questions the gate put
        // stay in the record, which is what makes the decision reviewable.
        assert_eq!(json["questions"], json!(["Which platform?"]));
    }

    #[test]
    fn every_terminal_outcome_states_its_own_reason() {
        // A queued input whose conversation ended is told which of these
        // happened; "the stage is gone" is the absence of a reason, not one.
        let reasons: Vec<&str> = [
            StageOutcome::Approved,
            StageOutcome::Rejected,
            StageOutcome::Cancelled,
            StageOutcome::Superseded,
        ]
        .into_iter()
        .map(StageOutcome::reason)
        .collect();
        let unique: std::collections::BTreeSet<_> = reasons.iter().collect();
        assert_eq!(unique.len(), reasons.len(), "{reasons:?}");
    }
}
