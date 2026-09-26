//! The Logic tier: run a frozen recorded-trajectory assignment against virtual
//! tools, and judge it from the run's own log.
//!
//! A `Logic` assignment is a failed attempt, the change or changes made after
//! it, and the same attempt passing — all observations the run recorded. This
//! module rebuilds that as a small stateful world: the attempt answers with its
//! recorded failure until every recorded change has been applied, in order, and
//! then with its recorded pass. The harness's own `SessionRuntime` drives it
//! with a scripted actor, through a tool registry holding only the world's
//! virtual tools. No built-in tool is registered, so no shell, file write or
//! project executable can start, and no model is involved.
//!
//! Two arms run, each from a fresh temporary root. **Without the change** the
//! actor retries the attempt up to the retry limit and must see only the
//! recorded failure. **With the change** it applies the recorded changes and
//! must reach the recorded pass. Both logs are read back through
//! [`EvidenceLedger`], so order, tool errors, retries, recovery and the final
//! verification are checked by the machinery real runs are judged with.
//!
//! What this proves, and no more: the assignment, its oracle and its fixture
//! are well-formed and replay deterministically. The actor is a script, not a
//! policy that reads the lesson, so it makes the same moves with or without the
//! lesson. The tier can therefore say `Valid`, `Invalid` or — through
//! eligibility — `NotExecutable`, plus `InvalidExperiment` when the run itself
//! cannot be interpreted. It can never say `Supported`, `Contradicted` or
//! `Inconclusive`; only an uplift run with a real actor can.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use localmind_core::{
    ArmRecord, AssignmentSource, CandidateLesson, EvidenceRef, EvidenceTier, ExperimentEvidence,
    ExperimentInputs, ExperimentProvenance, LabVerdict, LessonAssignment, Observation,
    OracleOrigin, VerdictReason, EXPERIMENT_EVIDENCE_VERSION,
};
use localpilot_core::ToolOutcome;
use localpilot_harness::{
    CallOutcome, CallRecord, EvidenceLedger, SessionConfig, SessionRuntime, StopReason,
};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{
    Effect, Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace,
};
use localpilot_store::Store;
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// How many times the arm without the change tries the attempt. Three is also
/// where the harness's same-failure breaker speaks up, so the arm shows the
/// harness recognising the repeat rather than retrying forever.
pub const LOGIC_RETRY_LIMIT: u32 = 3;

/// The arm that retries the attempt without making the change.
pub const WITHOUT_CHANGE_ARM: &str = "without-change";
/// The arm that makes the recorded change and tries again.
pub const WITH_CHANGE_ARM: &str = "with-change";

/// What the harness appends to a result when it recognises a repeated failure.
const RECOVERY_MARK: &str = "[recovery]";
/// The observation an arm records when the harness recognised the repeat.
pub const REPEAT_FLAGGED: &str = "the harness flagged the repeated failure at the retry limit";

/// The contract version every virtual tool reports.
const VIRTUAL_TOOL_VERSION: &str = "virtual/1";

/// The reason a result carries when the lab's own run could not be
/// interpreted — a runtime that stopped on its own, a log that does not account
/// for a call, a root that could not be made. Never a finding about the lesson.
pub const INFRASTRUCTURE_FAILURE: &str = "InfrastructureFailure";
/// The reason a result carries when the assignment was frozen for a different
/// candidate: the lesson changed, and the assignment must be rebuilt.
pub const STALE_ASSIGNMENT: &str = "StaleAssignment";
/// The reason a result carries when the assignment is not a recorded
/// trajectory, which is the only kind this tier runs.
pub const NOT_A_LOGIC_ASSIGNMENT: &str = "NotALogicAssignment";

/// What a Logic run may use besides the candidate and its assignment.
#[derive(Clone, Debug)]
pub struct LogicOptions {
    /// Where each run makes its fresh temporary root. The system temporary
    /// directory when `None`.
    pub scratch_root: Option<PathBuf>,
    /// The tool-call ceiling for each arm. Breaching it is `BudgetExceeded`.
    pub max_tool_calls: usize,
    /// The project revision the result is bound to.
    pub source_revision: String,
    /// Stops the run between and inside arms. A cancelled run is
    /// `InvalidExperiment`, never a finding.
    pub cancel: CancellationToken,
}

impl Default for LogicOptions {
    fn default() -> Self {
        Self {
            scratch_root: None,
            max_tool_calls: 64,
            source_revision: "unknown".to_string(),
            cancel: CancellationToken::new(),
        }
    }
}

/// Run `assignment` for `candidate` and return the evidence record.
///
/// `recorded` is the run's facts as captured now. The assignment's oracle and
/// fixture are checked against them before either arm runs: a recorded outcome
/// that has since changed or gone is a changed oracle, and the result is
/// `Invalid` rather than a replay of what the assignment remembers.
///
/// Never fails: every problem is a verdict with its reason.
pub async fn run_logic(
    candidate: &CandidateLesson,
    assignment: &LessonAssignment,
    recorded: &[EvidenceRef],
    options: &LogicOptions,
) -> ExperimentEvidence {
    let mut result = Assembly::new(candidate, assignment, options);

    if assignment.candidate_identity != candidate.content_identity() {
        // Built for another version of the lesson. Nothing here can say
        // whether it fits this one; classification has to run again.
        return result.unbound(STALE_ASSIGNMENT);
    }
    if !matches!(
        assignment.source,
        Some(AssignmentSource::RecordedTrajectory { .. })
    ) {
        return result.unbound(NOT_A_LOGIC_ASSIGNMENT);
    }
    let trajectory = match Trajectory::resolve(assignment, recorded) {
        Ok(trajectory) => trajectory,
        Err(reasons) => return result.finish(LabVerdict::Invalid, reasons),
    };
    result.tools(&trajectory);
    let trajectory = Arc::new(trajectory);

    let root = match scratch(options) {
        Ok(root) => root,
        Err(_) => return result.infrastructure(),
    };
    let arms = [
        (WITHOUT_CHANGE_ARM, trajectory.without_change_script()),
        (WITH_CHANGE_ARM, trajectory.with_change_script()),
    ];
    for (name, script) in arms {
        if options.cancel.is_cancelled() {
            return result.stopped(name, VerdictReason::Cancelled);
        }
        match run_arm(&root, name, &trajectory, &script, options).await {
            Ok(arm) => result.arm(arm),
            Err(ArmStop::Cancelled) => return result.stopped(name, VerdictReason::Cancelled),
            Err(ArmStop::Budget) => return result.stopped(name, VerdictReason::BudgetExceeded),
            Err(ArmStop::Infrastructure) => return result.infrastructure(),
        }
    }
    if root.close().is_err() {
        result.limitation("the run's temporary root could not be removed");
    }

    let (verdict, reasons) = judge(&trajectory, &result.arms);
    result.finish(verdict, reasons)
}

/// The recorded trajectory an assignment names, checked against the record.
#[derive(Debug)]
struct Trajectory {
    attempt: Step,
    changes: Vec<Step>,
    pass: Step,
    success_observations: Vec<String>,
    failure_observations: Vec<String>,
}

/// One recorded action and what it answered.
#[derive(Clone, Debug)]
struct Step {
    tool: String,
    signature: String,
    /// The recorded observation, as capture kept it: redacted and bounded.
    output: String,
}

impl Step {
    fn of(fact: &EvidenceRef) -> Option<Self> {
        let signature = fact.signature()?.to_string();
        let tool = signature.split(':').next()?.to_string();
        let output = match &fact.excerpt {
            Some(excerpt) => format!("{}\n{excerpt}", fact.label),
            None => fact.label.clone(),
        };
        Some(Self {
            tool,
            signature,
            output,
        })
    }
}

impl Trajectory {
    /// Resolve the assignment's facts in `recorded` and check the oracle and
    /// fixture are the ones it froze.
    fn resolve(
        assignment: &LessonAssignment,
        recorded: &[EvidenceRef],
    ) -> Result<Self, Vec<VerdictReason>> {
        let mut reasons = Vec::new();
        let oracle = &assignment.oracle;
        if oracle.origin == OracleOrigin::DerivedFromLesson {
            reasons.push(VerdictReason::OracleNotIndependent);
        }
        // The oracle is the recorded pass: it must still be on record, with the
        // content it was frozen with.
        let oracle_intact = !oracle.content_hash.trim().is_empty()
            && recorded.iter().any(|fact| {
                fact.uri.as_deref() == Some(oracle.locator.as_str())
                    && fact.content_hash.as_deref() == Some(oracle.content_hash.as_str())
            });
        if !oracle_intact {
            reasons.push(VerdictReason::OracleMutable);
        }
        // The fixture is the trajectory itself: every fact still on record, and
        // the list the same one that was frozen.
        let facts: Option<Vec<&EvidenceRef>> = assignment
            .task_evidence
            .iter()
            .map(|id| recorded.iter().find(|fact| &fact.id == id))
            .collect();
        let frozen = sha256_hex(
            &assignment
                .task_evidence
                .iter()
                .map(localmind_core::EvidenceId::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let facts = match facts {
            Some(facts) if assignment.fixture.content_hash == frozen => facts,
            _ => {
                reasons.push(VerdictReason::FixtureUnavailable);
                return Err(reasons);
            }
        };
        if !reasons.is_empty() {
            return Err(reasons);
        }

        let shape = Self::shape(&facts, &oracle.locator);
        let Some((attempt, changes, pass)) = shape else {
            return Err(vec![VerdictReason::NotReplayable]);
        };
        Ok(Self {
            attempt,
            changes,
            pass,
            success_observations: assignment.success_observations.clone(),
            failure_observations: assignment.failure_observations.clone(),
        })
    }

    /// A failure, changes that each succeeded as something else, and the same
    /// attempt succeeding at the oracle.
    fn shape(facts: &[&EvidenceRef], oracle: &str) -> Option<(Step, Vec<Step>, Step)> {
        let (first, rest) = facts.split_first()?;
        let (last, middle) = rest.split_last()?;
        if first.observation() != Some(Observation::Failure)
            || last.observation() != Some(Observation::Success)
            || last.uri.as_deref() != Some(oracle)
        {
            return None;
        }
        let attempt = Step::of(first)?;
        let pass = Step::of(last)?;
        if pass.signature != attempt.signature {
            return None;
        }
        let changes = middle
            .iter()
            .map(|fact| {
                let step = Step::of(fact)?;
                (fact.observation() == Some(Observation::Success)
                    && step.signature != attempt.signature)
                    .then_some(step)
            })
            .collect::<Option<Vec<_>>>()?;
        Some((attempt, changes, pass))
    }

    /// The distinct virtual tools the world needs, in a fixed order.
    fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::iter::once(&self.attempt)
            .chain(&self.changes)
            .map(|step| step.tool.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Retry the attempt to the limit, changing nothing.
    fn without_change_script(&self) -> Vec<(String, String)> {
        (0..LOGIC_RETRY_LIMIT)
            .map(|_| (self.attempt.tool.clone(), self.attempt.signature.clone()))
            .collect()
    }

    /// Try, apply each recorded change in order, try again.
    fn with_change_script(&self) -> Vec<(String, String)> {
        std::iter::once(&self.attempt)
            .chain(&self.changes)
            .chain(std::iter::once(&self.attempt))
            .map(|step| (step.tool.clone(), step.signature.clone()))
            .collect()
    }
}

/// The world a run's virtual tools share: how many recorded changes have been
/// applied so far.
struct World {
    trajectory: Arc<Trajectory>,
    applied: Mutex<usize>,
}

impl World {
    fn act(&self, signature: &str) -> Result<ToolOutput, ToolError> {
        let trajectory = &self.trajectory;
        let mut applied = self
            .applied
            .lock()
            .map_err(|_| ToolError::Failed("the virtual world is unavailable".to_string()))?;
        if signature == trajectory.attempt.signature {
            return Ok(if *applied == trajectory.changes.len() {
                ToolOutput::ok(trajectory.pass.output.clone())
            } else {
                ToolOutput::ok(trajectory.attempt.output.clone())
                    .with_outcome(ToolOutcome::ReportedFailure)
            });
        }
        let Some(position) = trajectory
            .changes
            .iter()
            .position(|change| change.signature == signature)
        else {
            return Err(ToolError::InvalidInput(
                "not an action in the recorded trajectory".to_string(),
            ));
        };
        if position < *applied {
            // Already made: making it again changes nothing.
            return Ok(ToolOutput::ok(trajectory.changes[position].output.clone()));
        }
        if position > *applied {
            return Ok(
                ToolOutput::ok("an earlier recorded change has not been made yet")
                    .with_outcome(ToolOutcome::ReportedFailure),
            );
        }
        *applied += 1;
        Ok(ToolOutput::ok(trajectory.changes[position].output.clone()))
    }
}

/// A tool that exists only in the recorded world. It declares no effect, so
/// nothing reaches the permission engine, and it touches nothing outside the
/// world's state.
struct VirtualTool {
    name: String,
    world: Arc<World>,
}

#[async_trait]
impl Tool for VirtualTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "A recorded action, replayed: answers as the run recorded it."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "action": { "type": "string" } },
            "required": ["action"]
        })
    }
    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(Vec::new())
    }
    async fn invoke(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("`action` is required".to_string()))?;
        self.world.act(action)
    }
}

/// The registry an arm runs with: the world's virtual tools and nothing else.
fn registry(trajectory: &Arc<Trajectory>) -> ToolRegistry {
    let world = Arc::new(World {
        trajectory: Arc::clone(trajectory),
        applied: Mutex::new(0),
    });
    let mut registry = ToolRegistry::new();
    for name in trajectory.tool_names() {
        registry.register(Box::new(VirtualTool {
            name,
            world: Arc::clone(&world),
        }));
    }
    registry
}

/// What one arm did, read back from its log.
#[derive(Debug)]
struct ArmRun {
    record: ArmRecord,
    calls: Vec<CallRecord>,
}

enum ArmStop {
    Cancelled,
    Budget,
    Infrastructure,
}

async fn run_arm(
    root: &tempfile::TempDir,
    name: &str,
    trajectory: &Arc<Trajectory>,
    script: &[(String, String)],
    options: &LogicOptions,
) -> Result<ArmRun, ArmStop> {
    let started = Instant::now();
    let dir = root.path().join(name);
    std::fs::create_dir_all(&dir).map_err(|_| ArmStop::Infrastructure)?;
    let workspace = Workspace::new(&dir).map_err(|_| ArmStop::Infrastructure)?;

    let actor = script
        .iter()
        .enumerate()
        .fold(FakeProvider::new(), |actor, (index, (tool, action))| {
            actor.tool_call(
                &format!("{name}-{}", index + 1),
                tool,
                json!({ "action": action }),
            )
        })
        .text("The scripted actor has finished.");
    let mut runtime = SessionRuntime::new(
        Arc::new(actor),
        registry(trajectory),
        // No virtual tool declares an effect, so nothing asks; were one to, the
        // least-privilege profile would ask and the headless approver refuse.
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::new(Vec::new())),
        Store::open(&dir),
        workspace,
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            interactivity: Interactivity::NonInteractive,
            trusted: false,
            tool_call_budget: Some(options.max_tool_calls),
            tool_call_budget_max: Some(options.max_tool_calls),
            tool_budget_explicit: true,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let (events, _receiver) = broadcast::channel(256);
    let stop = runtime
        .run_turn(
            "Replay the recorded trajectory.",
            &events,
            &options.cancel.child_token(),
        )
        .await;
    match stop {
        StopReason::Done => {}
        StopReason::Cancelled | StopReason::Quiesced => return Err(ArmStop::Cancelled),
        StopReason::BudgetExceeded | StopReason::TimedOut => return Err(ArmStop::Budget),
        StopReason::Degraded | StopReason::ProviderError | StopReason::NoProgress => {
            return Err(ArmStop::Infrastructure)
        }
    }
    let log = runtime
        .store()
        .read_events(runtime.session_id())
        .map_err(|_| ArmStop::Infrastructure)?;
    let ledger = EvidenceLedger::project(&log);
    let calls = ledger.calls().to_vec();
    // A call the log does not settle, or settles twice two ways, is a run
    // that cannot be read — not an outcome of the world.
    if calls.len() != script.len()
        || calls.iter().any(|call| {
            call.conflicting_result
                || !matches!(call.outcome, CallOutcome::Ok | CallOutcome::Error)
                || call.refinement == Some(ToolOutcome::Unusable)
        })
    {
        return Err(ArmStop::Infrastructure);
    }

    let attempts: Vec<&CallRecord> = calls
        .iter()
        .filter(|call| action_of(call) == Some(trajectory.attempt.signature.as_str()))
        .collect();
    let observed = |wanted: &[String], call: &CallRecord| -> Vec<String> {
        wanted
            .iter()
            .filter(|observation| call.output().contains(observation.as_str()))
            .cloned()
            .collect()
    };
    let mut observations = Vec::new();
    for call in &attempts {
        let seen = if call.outcome == CallOutcome::Ok {
            observed(&trajectory.success_observations, call)
        } else {
            observed(&trajectory.failure_observations, call)
        };
        for observation in seen {
            if !observations.contains(&observation) {
                observations.push(observation);
            }
        }
    }
    // The harness's same-failure breaker marks a result once the same failure
    // has repeated to its limit; the arm that never changes anything should
    // show it.
    if attempts.last().is_some_and(|call| {
        call.outcome == CallOutcome::Error && call.output().contains(RECOVERY_MARK)
    }) && attempts.len() >= LOGIC_RETRY_LIMIT as usize
    {
        observations.push(REPEAT_FLAGGED.to_string());
    }
    let record = ArmRecord {
        arm: name.to_string(),
        attempts: u32::try_from(attempts.len()).unwrap_or(u32::MAX),
        passed: u32::try_from(
            attempts
                .iter()
                .filter(|call| call.outcome == CallOutcome::Ok)
                .count(),
        )
        .unwrap_or(u32::MAX),
        observations,
        // The run's log is the lab's own scratch; it is not kept and its text
        // never leaves this function.
        logs: Vec::new(),
        wall_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        truncated: false,
        cancelled: false,
    };
    Ok(ArmRun { record, calls })
}

fn action_of(call: &CallRecord) -> Option<&str> {
    call.input.get("action").and_then(Value::as_str)
}

/// Read the two arms against the assignment.
fn judge(trajectory: &Trajectory, arms: &[ArmRun]) -> (LabVerdict, Vec<VerdictReason>) {
    let (Some(without), Some(with)) = (
        arms.iter().find(|arm| arm.record.arm == WITHOUT_CHANGE_ARM),
        arms.iter().find(|arm| arm.record.arm == WITH_CHANGE_ARM),
    ) else {
        return (
            LabVerdict::InvalidExperiment,
            vec![VerdictReason::PartialPair],
        );
    };

    // Without the change, the attempt must never pass: if it can, the recorded
    // pass does not tell the change apart from no change.
    if without.record.passed > 0 {
        return (
            LabVerdict::Invalid,
            vec![VerdictReason::NoDiscriminatingVerifier],
        );
    }

    // With the change: the recorded order, the recorded failure first, every
    // change landing, and the attempt passing last.
    let expected: Vec<&str> = std::iter::once(&trajectory.attempt)
        .chain(&trajectory.changes)
        .chain(std::iter::once(&trajectory.attempt))
        .map(|step| step.signature.as_str())
        .collect();
    let actions: Vec<Option<&str>> = with.calls.iter().map(action_of).collect();
    let in_order = actions.len() == expected.len()
        && actions
            .iter()
            .zip(&expected)
            .all(|(action, expected)| *action == Some(*expected));
    let outcomes_ok = with.calls.first().map(|call| call.outcome) == Some(CallOutcome::Error)
        && with
            .calls
            .iter()
            .skip(1)
            .all(|call| call.outcome == CallOutcome::Ok);
    if !in_order || !outcomes_ok {
        return (LabVerdict::Invalid, vec![VerdictReason::NotReplayable]);
    }

    // Every observation the assignment requires must have been seen, each on
    // its own side.
    let required_seen = |arm: &ArmRun, required: &[String]| {
        required
            .iter()
            .all(|observation| arm.record.observations.contains(observation))
    };
    if !required_seen(without, &trajectory.failure_observations)
        || !required_seen(with, &trajectory.failure_observations)
        || !required_seen(with, &trajectory.success_observations)
    {
        return (LabVerdict::Invalid, vec![VerdictReason::NotReplayable]);
    }

    (LabVerdict::Valid, Vec::new())
}

/// The evidence record, built up as the run goes.
struct Assembly<'a> {
    assignment: &'a LessonAssignment,
    inputs: ExperimentInputs,
    arms: Vec<ArmRun>,
    limitations: Vec<String>,
}

impl<'a> Assembly<'a> {
    fn new(
        candidate: &CandidateLesson,
        assignment: &'a LessonAssignment,
        options: &LogicOptions,
    ) -> Self {
        Self {
            assignment,
            inputs: ExperimentInputs {
                candidate_identity: candidate.content_identity(),
                assignment_identity: Some(assignment.identity()),
                source_revision: options.source_revision.clone(),
                model: None,
                runtime: Some(format!("localpilot-logic/{}", env!("CARGO_PKG_VERSION"))),
                settings_digest: None,
                seed: None,
                budgets_digest: Some(sha256_hex(&format!(
                    "retry_limit={LOGIC_RETRY_LIMIT}\nmax_tool_calls={}",
                    options.max_tool_calls
                ))),
                tool_versions: BTreeMap::new(),
                validation_profile: None,
                verifier: Some(assignment.verifier.clone()),
            },
            arms: Vec::new(),
            limitations: vec![
                "Logic validates the assignment, its oracle and its fixture against the run's \
                 recorded observations; a scripted actor makes the same moves with or without \
                 the lesson, so this says nothing about whether the lesson helps"
                    .to_string(),
            ],
        }
    }

    fn tools(&mut self, trajectory: &Trajectory) {
        self.inputs.tool_versions = trajectory
            .tool_names()
            .into_iter()
            .map(|name| (name, VIRTUAL_TOOL_VERSION.to_string()))
            .collect();
    }

    fn arm(&mut self, arm: ArmRun) {
        self.arms.push(arm);
    }

    fn limitation(&mut self, text: &str) {
        self.limitations.push(text.to_string());
    }

    /// No assignment this run can be bound to.
    fn unbound(mut self, reason: &str) -> ExperimentEvidence {
        self.inputs.assignment_identity = None;
        self.inputs.verifier = None;
        let mut evidence = self.finish(
            LabVerdict::InvalidExperiment,
            vec![VerdictReason::Other(reason.to_string())],
        );
        evidence.assignment = None;
        evidence
    }

    /// The run stopped inside `arm` for `reason`.
    fn stopped(mut self, arm: &str, reason: VerdictReason) -> ExperimentEvidence {
        if reason == VerdictReason::Cancelled {
            self.arms.push(ArmRun {
                record: ArmRecord {
                    arm: arm.to_string(),
                    attempts: 0,
                    passed: 0,
                    observations: Vec::new(),
                    logs: Vec::new(),
                    wall_ms: 0,
                    truncated: false,
                    cancelled: true,
                },
                calls: Vec::new(),
            });
        }
        self.finish(LabVerdict::InvalidExperiment, vec![reason])
    }

    fn infrastructure(self) -> ExperimentEvidence {
        self.finish(
            LabVerdict::InvalidExperiment,
            vec![VerdictReason::Other(INFRASTRUCTURE_FAILURE.to_string())],
        )
    }

    fn finish(self, verdict: LabVerdict, reasons: Vec<VerdictReason>) -> ExperimentEvidence {
        let produced_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
            });
        ExperimentEvidence {
            version: EXPERIMENT_EVIDENCE_VERSION,
            tier: EvidenceTier::Logic,
            inputs: self.inputs,
            assignment: Some(self.assignment.clone()),
            verdict,
            reasons,
            injection: None,
            receipt: None,
            arms: self.arms.into_iter().map(|arm| arm.record).collect(),
            provenance: ExperimentProvenance {
                producer: format!("localpilot-logic-lab/{}", env!("CARGO_PKG_VERSION")),
                produced_at,
            },
            limitations: self.limitations,
        }
    }
}

/// A fresh temporary root for one run.
fn scratch(options: &LogicOptions) -> std::io::Result<tempfile::TempDir> {
    let builder = {
        let mut builder = tempfile::Builder::new();
        builder.prefix("localpilot-logic-");
        builder
    };
    match &options.scratch_root {
        Some(root) => builder.tempdir_in(root),
        None => builder.tempdir(),
    }
}

fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut hex = String::from("sha256:");
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn step(tool: &str, signature: &str, output: &str) -> Step {
        Step {
            tool: tool.to_string(),
            signature: signature.to_string(),
            output: output.to_string(),
        }
    }

    /// A failed test run, two changes that must land in order, the pass.
    fn world() -> World {
        World {
            trajectory: Arc::new(Trajectory {
                attempt: step("run_shell", "run_shell:tests", "tests failed"),
                changes: vec![
                    step("write_file", "write_file:migration", "wrote the migration"),
                    step("run_shell", "run_shell:migrate", "migrated"),
                ],
                pass: step("run_shell", "run_shell:tests", "tests passed"),
                success_observations: Vec::new(),
                failure_observations: Vec::new(),
            }),
            applied: Mutex::new(0),
        }
    }

    fn outcome(result: &Result<ToolOutput, ToolError>) -> Option<(ToolOutcome, String)> {
        result
            .as_ref()
            .ok()
            .map(|output| (output.outcome, output.text.clone()))
    }

    #[test]
    fn the_attempt_fails_as_recorded_until_every_change_has_landed() {
        let world = world();
        let failing = Some((ToolOutcome::ReportedFailure, "tests failed".to_string()));
        assert_eq!(outcome(&world.act("run_shell:tests")), failing);
        world.act("write_file:migration").unwrap();
        assert_eq!(
            outcome(&world.act("run_shell:tests")),
            failing,
            "half a fix"
        );
        world.act("run_shell:migrate").unwrap();
        assert_eq!(
            outcome(&world.act("run_shell:tests")),
            Some((ToolOutcome::Ok, "tests passed".to_string()))
        );
    }

    #[test]
    fn a_change_made_before_its_precondition_is_refused_and_changes_nothing() {
        let world = world();
        let early = world.act("run_shell:migrate").unwrap();
        assert_eq!(early.outcome, ToolOutcome::ReportedFailure);
        assert_eq!(*world.applied.lock().unwrap(), 0);
        world.act("write_file:migration").unwrap();
        // Making a landed change again is harmless.
        let again = world.act("write_file:migration").unwrap();
        assert_eq!(again.outcome, ToolOutcome::Ok);
        assert_eq!(*world.applied.lock().unwrap(), 1);
    }

    #[test]
    fn an_action_the_run_never_recorded_is_a_tool_error() {
        let world = world();
        assert!(matches!(
            world.act("run_shell:rm -rf /"),
            Err(ToolError::InvalidInput(_))
        ));
    }

    #[test]
    fn an_arm_can_reach_only_the_worlds_virtual_tools() {
        let trajectory = Arc::clone(&world().trajectory);
        let registry = registry(&trajectory);
        let mut names = registry.names();
        names.sort_unstable();
        assert_eq!(names, vec!["run_shell", "write_file"]);
    }
}
