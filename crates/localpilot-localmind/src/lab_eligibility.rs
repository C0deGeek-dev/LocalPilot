//! Whether a lesson can be tested, and the frozen assignment that tests it.
//!
//! A plausible lesson is not automatically testable, and the easiest test to
//! build — one the lesson itself describes — proves nothing. So assignments come
//! only from trusted sources that existed before the lesson did:
//!
//! - **the run's recorded trajectory** — an attempt that failed, a change, and
//!   the identical attempt passing. Its observations are replayed; nothing
//!   executes (`Logic`).
//! - **the step's own fail/fix commits**, judged by a ratified project check
//!   the run saw fail — the check must fail at the step's base revision and pass
//!   at its commit (`Replay`);
//! - **a controlled mutation** — the same fix taken back out of the current
//!   code, so the repair is known (`Replay`).
//!
//! Before any of that, the lesson itself is read conservatively. Testing it
//! would take a real-world action; it states a preference or someone's intent;
//! it is about style no ratified check can verify: each is `NotExecutable` with
//! an honest reason code, and the lesson keeps its ordinary review path. A lesson
//! that is checkable in principle but has no trusted source here is
//! `UpliftOnly`: only a real actor on a separately trusted task set can test it.
//!
//! Every assignment's oracle is independent and frozen here, before either arm
//! runs. It must predate the fix and be untouched by it — a fix that also edited
//! the tests could have edited them to pass — and it may not merely restate the
//! lesson's wording. The oracle's content is hashed into the assignment, whose
//! identity then fixes it.
//!
//! Reading the project's history is read-only: `git rev-parse`, `diff`,
//! `ls-tree`. Nothing here executes a check.

use std::path::Path;
use std::process::Command;

use localmind_core::{
    AssignmentSource, CandidateLesson, EvidenceRef, FixtureRef, LessonAssignment, Observation,
    OracleOrigin, OracleRef, Sensitivity, VerdictReason, VerifierRef, LESSON_ASSIGNMENT_VERSION,
};
use localpilot_config::CheckConfig;
use localpilot_harness::{check_command_digest, Progress};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::RATIFIED_CHECK_KEY;

/// Which tier, if any, can test a lesson.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Eligibility {
    /// A recorded trajectory can be replayed against scripted tools.
    Logic,
    /// A ratified check can be run on a fail/fix pair in a temporary worktree.
    Replay,
    /// Checkable in principle, with no trusted source in this run: only an
    /// uplift run on a separately trusted task set can test it.
    UpliftOnly,
    /// No honest test exists. The lesson keeps its ordinary review path.
    NotExecutable,
}

/// A source that was considered and refused, kept so a reviewer can see why an
/// assignment does not exist.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RejectedAssignment {
    pub source: AssignmentSource,
    pub reason: VerdictReason,
    pub detail: String,
}

/// What the lab concluded about one lesson.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LabClassification {
    /// The candidate's content identity, which every assignment is bound to.
    pub candidate_identity: String,
    pub eligibility: Eligibility,
    /// Why, for `NotExecutable` and `UpliftOnly`.
    pub reasons: Vec<VerdictReason>,
    /// Frozen and validated. Empty unless `Logic` or `Replay`.
    pub assignments: Vec<LessonAssignment>,
    pub rejected: Vec<RejectedAssignment>,
}

/// What the classification may read besides the candidate.
pub struct LabContext<'a> {
    /// The project repository.
    pub root: &'a Path,
    /// The run's plan, to find which step a session belongs to and its commit.
    pub progress: Option<&'a Progress>,
    /// The project's ratified checks.
    pub checks: &'a [CheckConfig],
}

/// The directory, under the project's `.localpilot/`, that frozen lab records
/// live in: one file per candidate identity.
pub const LAB_ASSIGNMENTS_DIR: &str = "lab/assignments";

/// Where a classification is kept, so the runs that come later read the
/// assignment exactly as it was frozen.
#[must_use]
pub fn record_path(localpilot_dir: &Path, candidate_identity: &str) -> std::path::PathBuf {
    localpilot_dir
        .join(LAB_ASSIGNMENTS_DIR)
        .join(format!("{candidate_identity}.json"))
}

/// Write the classification to its record. The record is the frozen form: an
/// existing record for the same candidate identity is replaced only by an
/// identical one, since the candidate — and so everything built from it — is
/// the same.
///
/// # Errors
/// An I/O or encoding error.
pub fn write_record(
    localpilot_dir: &Path,
    classification: &LabClassification,
) -> std::io::Result<std::path::PathBuf> {
    let path = record_path(localpilot_dir, &classification.candidate_identity);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(classification).map_err(std::io::Error::other)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// Every classification kept under `localpilot_dir`, sorted by candidate
/// identity. A record that does not parse is skipped: it is the lab's own
/// output, and one damaged file must not hide the rest.
#[must_use]
pub fn read_records(localpilot_dir: &Path) -> Vec<LabClassification> {
    let Ok(entries) = std::fs::read_dir(localpilot_dir.join(LAB_ASSIGNMENTS_DIR)) else {
        return Vec::new();
    };
    let mut records: Vec<LabClassification> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|text| serde_json::from_str(&text).ok())
        .collect();
    records.sort_by(|a, b| a.candidate_identity.cmp(&b.candidate_identity));
    records
}

/// The `NotExecutable` result a classification carries onto its candidate, so
/// review shows why no test exists. `None` for every other eligibility.
#[must_use]
pub fn not_executable_evidence(
    classification: &LabClassification,
    source_revision: &str,
    produced_at: i64,
) -> Option<localmind_core::ExperimentEvidence> {
    use localmind_core::{
        EvidenceTier, ExperimentEvidence, ExperimentInputs, ExperimentProvenance, LabVerdict,
        EXPERIMENT_EVIDENCE_VERSION,
    };
    (classification.eligibility == Eligibility::NotExecutable).then(|| ExperimentEvidence {
        version: EXPERIMENT_EVIDENCE_VERSION,
        // Eligibility is the Logic tier's judgement: no model, nothing executed.
        tier: EvidenceTier::Logic,
        inputs: ExperimentInputs {
            candidate_identity: classification.candidate_identity.clone(),
            assignment_identity: None,
            source_revision: source_revision.to_string(),
            model: None,
            runtime: None,
            settings_digest: None,
            seed: None,
            budgets_digest: None,
            tool_versions: std::collections::BTreeMap::new(),
            validation_profile: None,
            verifier: None,
        },
        assignment: None,
        verdict: LabVerdict::NotExecutable,
        reasons: classification.reasons.clone(),
        injection: None,
        receipt: None,
        arms: Vec::new(),
        provenance: ExperimentProvenance {
            producer: format!("localpilot-lab-eligibility/{}", env!("CARGO_PKG_VERSION")),
            produced_at,
        },
        limitations: Vec::new(),
    })
}

/// The project's current revision, for binding a result. `"unknown"` outside a
/// repository.
#[must_use]
pub fn current_revision(root: &Path) -> String {
    git(root, &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string())
}

/// Classify `candidate` and build its assignments.
#[must_use]
pub fn classify(candidate: &CandidateLesson, context: &LabContext<'_>) -> LabClassification {
    let candidate_identity = candidate.content_identity();
    let lesson = lesson_text(candidate);
    let not_executable = |reasons: Vec<VerdictReason>| LabClassification {
        candidate_identity: candidate_identity.clone(),
        eligibility: Eligibility::NotExecutable,
        reasons,
        assignments: Vec::new(),
        rejected: Vec::new(),
    };

    // The lesson itself first: nothing below may coerce these into a test.
    let lower = lesson.to_lowercase();
    if mentions(&lower, UNSAFE_MARKERS) {
        return not_executable(vec![VerdictReason::UnsafeAction]);
    }
    let style = mentions(&lower, STYLE_MARKERS);
    if style && !context.checks.iter().any(is_style_check) {
        return not_executable(vec![VerdictReason::UnverifiableStyle]);
    }
    if !style {
        let mut reasons = Vec::new();
        if mentions(&lower, PREFERENCE_MARKERS) {
            reasons.push(VerdictReason::Preference);
        }
        if mentions(&lower, INTENT_MARKERS) {
            reasons.push(VerdictReason::HumanIntent);
        }
        if !reasons.is_empty() {
            return not_executable(reasons);
        }
    }

    let mut builder = Builder {
        candidate,
        candidate_identity: candidate_identity.clone(),
        lesson,
        context,
        assignments: Vec::new(),
        rejected: Vec::new(),
    };
    builder.recorded_trajectories();
    builder.fail_fix_pairs();
    if style {
        // A style lesson is tested only by the check that verifies style. A
        // test command passing says nothing about how the code is indented.
        builder.keep_style_oracles();
    }

    let logic = builder.assignments.iter().any(|assignment| {
        matches!(
            assignment.source,
            Some(AssignmentSource::RecordedTrajectory { .. })
        )
    });
    let (eligibility, reasons) = if logic {
        (Eligibility::Logic, Vec::new())
    } else if builder.assignments.is_empty() {
        let mut reasons = vec![VerdictReason::NoTrustedSource];
        for rejected in &builder.rejected {
            if !reasons.contains(&rejected.reason) {
                reasons.push(rejected.reason.clone());
            }
        }
        (Eligibility::UpliftOnly, reasons)
    } else {
        (Eligibility::Replay, Vec::new())
    };
    LabClassification {
        candidate_identity,
        eligibility,
        reasons,
        assignments: builder.assignments,
        rejected: builder.rejected,
    }
}

struct Builder<'a> {
    candidate: &'a CandidateLesson,
    candidate_identity: String,
    lesson: String,
    context: &'a LabContext<'a>,
    assignments: Vec<LessonAssignment>,
    rejected: Vec<RejectedAssignment>,
}

impl Builder<'_> {
    fn facts(&self) -> &[EvidenceRef] {
        self.candidate.evidence()
    }

    /// The failures the hindsight cites, in capture order.
    fn cited_failures(&self) -> Vec<(usize, &EvidenceRef)> {
        let cited: Vec<_> = self
            .candidate
            .hindsight
            .as_ref()
            .map(|draft| draft.cited_evidence_ids().into_iter().cloned().collect())
            .unwrap_or_default();
        self.facts()
            .iter()
            .enumerate()
            .filter(|(_, fact)| {
                cited.contains(&fact.id) && fact.observation() == Some(Observation::Failure)
            })
            .collect()
    }

    /// `Logic`: a cited failure, a different success in between (the change),
    /// then the identical attempt succeeding — all from the same session.
    fn recorded_trajectories(&mut self) {
        let mut built = Vec::new();
        for (position, failure) in self.cited_failures() {
            let Some(signature) = failure.signature() else {
                continue;
            };
            let later = &self.facts()[position + 1..];
            let same_source = |fact: &&EvidenceRef| fact.source() == failure.source();
            let Some(offset) = later.iter().position(|fact| {
                same_source(&fact)
                    && fact.observation() == Some(Observation::Success)
                    && fact.signature() == Some(signature)
            }) else {
                continue;
            };
            let between = &later[..offset];
            let changes: Vec<&EvidenceRef> = between
                .iter()
                .filter(|fact| {
                    same_source(fact)
                        && fact.observation() == Some(Observation::Success)
                        && fact.signature() != Some(signature)
                })
                .collect();
            if changes.is_empty() {
                // It went away on its own: nothing a lesson changed.
                continue;
            }
            built.push((
                failure.clone(),
                later[offset].clone(),
                changes.into_iter().cloned().collect::<Vec<_>>(),
            ));
        }
        for (failure, success, changes) in built {
            self.recorded_trajectory(&failure, &success, &changes);
        }
    }

    fn recorded_trajectory(
        &mut self,
        failure: &EvidenceRef,
        success: &EvidenceRef,
        changes: &[EvidenceRef],
    ) {
        let session = failure.source().unwrap_or("unknown").to_string();
        let source = AssignmentSource::RecordedTrajectory {
            session: session.clone(),
        };
        let oracle_text = format!(
            "{} {}",
            success.label,
            success.excerpt.as_deref().unwrap_or("")
        );
        if matches_lesson_wording(&self.lesson, &oracle_text) {
            self.reject(
                source,
                VerdictReason::OracleNotIndependent,
                "the recorded outcome restates the lesson's wording",
            );
            return;
        }
        let trajectory: Vec<&EvidenceRef> = std::iter::once(failure)
            .chain(changes.iter())
            .chain(std::iter::once(success))
            .collect();
        // A tool call's signature starts with the tool; a ratified check's with
        // `check:`, and a check is the judge, not a tool the actor may call.
        let tool = |fact: &EvidenceRef| {
            fact.signature()
                .and_then(|signature| signature.split(':').next())
                .filter(|tool| *tool != "check")
                .map(str::to_string)
        };
        let mut allowed_tools: Vec<String> =
            trajectory.iter().filter_map(|fact| tool(fact)).collect();
        allowed_tools.sort();
        allowed_tools.dedup();
        let attempt = match failure.metadata.get(RATIFIED_CHECK_KEY) {
            Some(check) => format!("the ratified check `{check}`"),
            None => format!(
                "`{}`",
                tool(failure).unwrap_or_else(|| "the attempt".to_string())
            ),
        };
        let assignment = LessonAssignment {
            version: LESSON_ASSIGNMENT_VERSION,
            candidate_identity: self.candidate_identity.clone(),
            task: bounded(&format!(
                "Reach the recorded passing outcome of {attempt} from the state where it failed"
            )),
            task_evidence: trajectory.iter().map(|fact| fact.id.clone()).collect(),
            oracle: OracleRef {
                locator: success.uri.clone().unwrap_or_default(),
                content_hash: success.content_hash.clone().unwrap_or_default(),
                // Recorded by the run, before the lesson existed.
                origin: OracleOrigin::Preexisting,
            },
            fixture: FixtureRef {
                locator: format!("{session}#trajectory"),
                content_hash: sha256_hex(
                    &trajectory
                        .iter()
                        .map(|fact| fact.id.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            },
            initial_state: bounded(&format!("the recorded state in which {}", failure.label)),
            allowed_tools,
            success_observations: vec![bounded(&success.label)],
            failure_observations: vec![bounded(&failure.label)],
            verifier: VerifierRef {
                name: "recorded-observation".to_string(),
                version: "1".to_string(),
            },
            cleanup: "Nothing to undo: the recorded observations are replayed and nothing executes"
                .to_string(),
            sensitivity: Sensitivity::Redacted,
            source: Some(source.clone()),
            preconditions: self.preconditions(),
            counterfactual: self.counterfactual(),
        };
        self.accept(assignment, source);
    }

    /// `Replay`: a cited failure of a ratified check, the step that session
    /// belongs to, and that step's commit — plus the same fix taken back out of
    /// the current code when the code has moved on since.
    fn fail_fix_pairs(&mut self) {
        let failures: Vec<EvidenceRef> = self
            .cited_failures()
            .into_iter()
            .filter(|(_, fact)| fact.metadata.contains_key(RATIFIED_CHECK_KEY))
            .map(|(_, fact)| fact.clone())
            .collect();
        let mut seen = Vec::new();
        for failure in failures {
            let Some(name) = failure.metadata.get(RATIFIED_CHECK_KEY).cloned() else {
                continue;
            };
            let Some(digest) = failure
                .signature()
                .and_then(|signature| signature.rsplit(':').next())
                .map(str::to_string)
            else {
                continue;
            };
            if seen.contains(&(name.clone(), digest.clone())) {
                continue;
            }
            seen.push((name.clone(), digest.clone()));
            self.fail_fix_pair(&failure, &name, &digest);
        }
    }

    fn fail_fix_pair(&mut self, failure: &EvidenceRef, name: &str, digest: &str) {
        let check_source = AssignmentSource::RatifiedCheck {
            name: name.to_string(),
        };
        // The check must still be ratified exactly as it ran: a changed command
        // is a different check, and an unratified one is not trusted.
        let Some(check) = self.context.checks.iter().find(|check| {
            check.name == name && check_command_digest(check) == format!("sha256:{digest}")
        }) else {
            self.reject(
                check_source,
                VerdictReason::NoTrustedSource,
                "the check is no longer ratified as it ran",
            );
            return;
        };
        let Some(fix) = self.step_commit(failure) else {
            self.reject(
                check_source,
                VerdictReason::FixtureUnavailable,
                "no committed step owns the session the check ran in",
            );
            return;
        };
        let root = self.context.root;
        let (Some(fix), Some(base)) = (
            git(
                root,
                &["rev-parse", "--verify", &format!("{fix}^{{commit}}")],
            ),
            git(root, &["rev-parse", "--verify", &format!("{fix}^")]),
        ) else {
            self.reject(
                check_source,
                VerdictReason::FixtureUnavailable,
                "the step's commit or its parent is not in this repository",
            );
            return;
        };
        let pair = AssignmentSource::FailFixPair {
            base_revision: base.clone(),
            fix_revision: fix.clone(),
        };
        if let Some(touched) = oracle_touched(root, &base, &fix) {
            self.reject(
                pair,
                VerdictReason::OracleChangedByFix,
                &format!("the fix also changed test code: {touched}"),
            );
            return;
        }
        if matches_lesson_wording(&self.lesson, &check_text(check)) {
            self.reject(
                pair,
                VerdictReason::OracleNotIndependent,
                "the check restates the lesson's wording",
            );
            return;
        }
        let Some(base_tree) = git(root, &["rev-parse", &format!("{base}^{{tree}}")]) else {
            self.reject(
                pair,
                VerdictReason::FixtureUnavailable,
                "the base revision's tree cannot be read",
            );
            return;
        };
        let assignment = self.replay_assignment(
            check,
            name,
            failure,
            &base,
            FixtureRef {
                locator: format!("git:{base}"),
                content_hash: format!("git-tree:{base_tree}"),
            },
            format!("a checkout of the step's base revision {}", short(&base)),
            pair.clone(),
        );
        if let Some(assignment) = assignment {
            self.accept(assignment, pair);
        }

        // The same fix, taken back out of today's code.
        let Some(head) = git(root, &["rev-parse", "HEAD"]) else {
            return;
        };
        if head == fix {
            return;
        }
        let mutation = AssignmentSource::ControlledMutation {
            applied_to: head.clone(),
            repair_revision: fix.clone(),
        };
        let (Some(head_tree), Some(patch)) = (
            git(root, &["rev-parse", "HEAD^{tree}"]),
            git(root, &["diff", "--no-color", "--no-ext-diff", &base, &fix]),
        ) else {
            self.reject(
                mutation,
                VerdictReason::FixtureUnavailable,
                "the current tree or the fix cannot be read",
            );
            return;
        };
        let assignment = self.replay_assignment(
            check,
            name,
            failure,
            &head,
            FixtureRef {
                locator: format!("git:{head}~revert:{fix}"),
                content_hash: sha256_hex(&format!("{head_tree}\n{patch}")),
            },
            format!(
                "the current revision {} with the step's fix {} reverted",
                short(&head),
                short(&fix)
            ),
            mutation.clone(),
        );
        if let Some(assignment) = assignment {
            self.accept(assignment, mutation);
        }
    }

    #[allow(clippy::too_many_arguments)] // one assignment shape, every part named
    fn replay_assignment(
        &mut self,
        check: &CheckConfig,
        name: &str,
        failure: &EvidenceRef,
        oracle_revision: &str,
        fixture: FixtureRef,
        initial_state: String,
        source: AssignmentSource,
    ) -> Option<LessonAssignment> {
        let Some(test_surface) = test_surface(self.context.root, oracle_revision) else {
            self.reject(
                source,
                VerdictReason::FixtureUnavailable,
                "the test files at the oracle's revision cannot be listed",
            );
            return None;
        };
        let digest = check_command_digest(check);
        Some(LessonAssignment {
            version: LESSON_ASSIGNMENT_VERSION,
            candidate_identity: self.candidate_identity.clone(),
            task: bounded(&format!("Make the ratified check `{name}` pass")),
            task_evidence: vec![failure.id.clone()],
            oracle: OracleRef {
                locator: format!("ratified-check:{name}@{oracle_revision}"),
                content_hash: sha256_hex(&format!("{digest}\n{oracle_revision}\n{test_surface}")),
                origin: OracleOrigin::Preexisting,
            },
            fixture,
            initial_state: bounded(&initial_state),
            allowed_tools: Vec::new(),
            success_observations: vec![bounded(&format!("ratified check `{name}` passes"))],
            failure_observations: vec![bounded(&failure.label)],
            verifier: VerifierRef {
                name: format!("ratified-check:{name}"),
                version: digest,
            },
            cleanup: "Remove the temporary worktree and its build output".to_string(),
            sensitivity: Sensitivity::LocalOnly,
            source: Some(source),
            preconditions: self.preconditions(),
            counterfactual: self.counterfactual(),
        })
    }

    /// Keep only assignments judged by a ratified style check; refuse the rest
    /// as unable to tell whether the style was followed.
    fn keep_style_oracles(&mut self) {
        let style_checks: Vec<String> = self
            .context
            .checks
            .iter()
            .filter(|check| is_style_check(check))
            .map(|check| check.name.clone())
            .collect();
        let facts = self.candidate.evidence();
        let judged_by_style = |assignment: &LessonAssignment| {
            let check = match &assignment.source {
                Some(AssignmentSource::RecordedTrajectory { .. }) => assignment
                    .task_evidence
                    .last()
                    .and_then(|id| facts.iter().find(|fact| &fact.id == id))
                    .and_then(|fact| fact.metadata.get(RATIFIED_CHECK_KEY).cloned()),
                _ => assignment
                    .verifier
                    .name
                    .strip_prefix("ratified-check:")
                    .map(str::to_string),
            };
            check.is_some_and(|name| style_checks.contains(&name))
        };
        let (kept, refused): (Vec<_>, Vec<_>) = std::mem::take(&mut self.assignments)
            .into_iter()
            .partition(judged_by_style);
        self.assignments = kept;
        for assignment in refused {
            if let Some(source) = assignment.source {
                self.reject(
                    source,
                    VerdictReason::NoDiscriminatingVerifier,
                    "a style lesson needs a ratified style check as its oracle",
                );
            }
        }
    }

    /// The commit of the step whose `sessions:` line names the fact's session.
    fn step_commit(&self, fact: &EvidenceRef) -> Option<String> {
        let session = fact.source()?.strip_prefix("localpilot-session:")?;
        self.context
            .progress?
            .steps
            .iter()
            .find(|step| step.done && step.sessions.iter().any(|linked| linked == session))
            .and_then(|step| step.commit.clone())
    }

    fn preconditions(&self) -> Vec<String> {
        self.candidate
            .hindsight
            .as_ref()
            .map(|draft| {
                draft
                    .preconditions
                    .iter()
                    .map(|item| bounded(item))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn counterfactual(&self) -> Option<String> {
        let draft = self.candidate.hindsight.as_ref()?;
        draft
            .counterfactual_prediction
            .clone()
            .or_else(|| {
                draft
                    .intervention
                    .as_ref()
                    .map(|intervention| format!("had this been done: {intervention}"))
            })
            .map(|text| bounded(&text))
    }

    fn accept(&mut self, assignment: LessonAssignment, source: AssignmentSource) {
        match assignment.validate() {
            Ok(()) => self.assignments.push(assignment),
            Err(violations) => self.reject(
                source,
                VerdictReason::Other("assignment failed validation".to_string()),
                &violations
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
            ),
        }
    }

    fn reject(&mut self, source: AssignmentSource, reason: VerdictReason, detail: &str) {
        self.rejected.push(RejectedAssignment {
            source,
            reason,
            detail: bounded(detail),
        });
    }
}

/// The lesson as it would be applied: its summary and the hindsight's proposal.
fn lesson_text(candidate: &CandidateLesson) -> String {
    let mut text = candidate.summary().to_string();
    if let Some(draft) = &candidate.hindsight {
        for part in [
            draft.proposed_lesson.as_deref(),
            draft.intervention.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            text.push('\n');
            text.push_str(part);
        }
    }
    text
}

/// Testing it would reach outside the machine or destroy something real.
const UNSAFE_MARKERS: &[&str] = &[
    "production",
    "deploy",
    "publish",
    "release to",
    "push to",
    "force-push",
    "force push",
    "send an email",
    "send email",
    "send a message",
    "payment",
    "charge the",
    "credential",
    "rotate the",
    "rm -rf",
    "drop table",
    "drop database",
    "customer data",
];

/// It says what someone likes.
const PREFERENCE_MARKERS: &[&str] = &["prefer", "i like", "likes to", "favourite", "favorite"];

/// It says what someone wants, which only they can confirm.
const INTENT_MARKERS: &[&str] = &[
    "the user wants",
    "the user intends",
    "the user asked",
    "the maintainer wants",
    "the maintainer asked",
    "the owner wants",
    "ask the user",
    "confirm with the user",
    "check with the user",
];

/// It is about how code looks.
const STYLE_MARKERS: &[&str] = &[
    "indent",
    "tabs",
    "whitespace",
    "formatting",
    "code style",
    "coding style",
    "naming convention",
    "line length",
    "trailing comma",
];

/// A ratified check that can verify style.
const STYLE_CHECK_MARKERS: &[&str] = &[
    "fmt",
    "format",
    "lint",
    "clippy",
    "prettier",
    "eslint",
    "ruff",
    "black",
    "stylelint",
];

fn mentions(text: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| text.contains(marker))
}

fn is_style_check(check: &CheckConfig) -> bool {
    let text = check_text(check).to_lowercase();
    mentions(&text, STYLE_CHECK_MARKERS)
}

fn check_text(check: &CheckConfig) -> String {
    format!("{} {} {}", check.name, check.program, check.args.join(" "))
}

/// Whether an oracle merely restates the lesson: most of the lesson's content
/// words appear in it, or it carries a five-word run of the lesson verbatim.
/// A criterion that echoes the lesson can be satisfied by the lesson whether or
/// not the lesson is right.
fn matches_lesson_wording(lesson: &str, oracle: &str) -> bool {
    let lesson_words = content_words(lesson);
    let oracle_words = content_words(oracle);
    if lesson_words.len() >= 4 {
        let shared = lesson_words
            .iter()
            .filter(|word| oracle_words.contains(word))
            .count();
        if shared * 10 >= lesson_words.len() * 6 {
            return true;
        }
    }
    let lesson_all = words(lesson);
    let oracle_text = words(oracle).join(" ");
    lesson_all
        .windows(5)
        .any(|run| oracle_text.contains(&run.join(" ")))
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn content_words(text: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "that", "this", "with", "from", "before", "after", "when", "then", "than", "into", "have",
        "been", "will", "would", "should", "always", "never", "every", "each", "their", "there",
        "which", "about", "only", "what",
    ];
    let mut out: Vec<String> = words(text)
        .into_iter()
        .filter(|word| word.len() >= 4 && !STOP.contains(&word.as_str()))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Paths that are tests by name or place.
pub(crate) fn is_test_path(path: &str) -> bool {
    let path = path.replace('\\', "/").to_lowercase();
    let file = path.rsplit('/').next().unwrap_or(&path);
    path.starts_with("tests/")
        || path.starts_with("test/")
        || path.contains("/tests/")
        || path.contains("/test/")
        || path.contains("__tests__/")
        || path.starts_with("spec/")
        || path.contains("/spec/")
        || file.starts_with("test_")
        || file.contains("_test.")
        || file.contains(".test.")
        || file.contains("_spec.")
        || file.contains(".spec.")
}

/// Whether the fix touched what judges it: a test file, or test code inside a
/// source file (an added or removed test attribute or test module). `Some`
/// names what was touched.
fn oracle_touched(root: &Path, base: &str, fix: &str) -> Option<String> {
    let names = git(root, &["diff", "--name-only", base, fix])?;
    let tests: Vec<&str> = names.lines().filter(|path| is_test_path(path)).collect();
    if !tests.is_empty() {
        return Some(tests.join(", "));
    }
    let diff = git(
        root,
        &["diff", "--no-color", "--no-ext-diff", "-U0", base, fix],
    )?;
    let touched = diff.lines().any(|line| {
        (line.starts_with('+') || line.starts_with('-'))
            && !line.starts_with("+++")
            && !line.starts_with("---")
            && TEST_CODE_MARKERS.iter().any(|marker| line.contains(marker))
    });
    touched.then(|| "test code inside a source file".to_string())
}

const TEST_CODE_MARKERS: &[&str] = &[
    "#[test]",
    "#[cfg(test)]",
    "#[tokio::test]",
    "def test_",
    "@Test",
];

/// The test files at `revision`, as `path blob` lines, sorted: what the oracle
/// reads, fixed by content.
pub(crate) fn test_surface(root: &Path, revision: &str) -> Option<String> {
    let listing = git(root, &["ls-tree", "-r", revision])?;
    let mut entries: Vec<String> = listing
        .lines()
        .filter_map(|line| {
            let (meta, path) = line.split_once('\t')?;
            let blob = meta.split_whitespace().nth(2)?;
            is_test_path(path).then(|| format!("{path} {blob}"))
        })
        .collect();
    entries.sort();
    Some(entries.join("\n"))
}

/// Run a read-only git query in `root`. `None` on any failure.
pub(crate) fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string()
    })
}

pub(crate) fn short(revision: &str) -> &str {
    revision.get(..10).unwrap_or(revision)
}

fn bounded(text: &str) -> String {
    text.chars()
        .take(localmind_core::MAX_OBSERVATION_CHARS)
        .collect()
}

pub(crate) fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut hex = String::from("sha256:");
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}
