//! The uplift adapter: projection, per-arm staging, the two-arm run and the
//! receipt import.
//!
//! LocalBench is stood in for by `StandIn`, which builds its receipt from the
//! shared identity types and reports what was *actually* staged in the
//! workspace when each arm ran — the configuration on disk and the memories in
//! the store — so a staging mistake shows up as it would for real. One test
//! runs the real `localbench` binary when `LOCALPILOT_TEST_LOCALBENCH` names
//! it. No model is called anywhere: this proves the adapter, never efficacy.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use localmind_core::{
    CandidateLesson, CausalHypothesis, Confidence, EvidenceKind, EvidenceRef, EvidenceTier,
    HindsightDraft, InjectionMode, LabVerdict, LessonAssignment, LessonCategory, LessonId,
    SuggestedAction, VerdictReason,
};
use localpilot_localmind::{
    import_receipt, memory_list, memory_list_readonly, project_uplift, run_uplift, seed_memory,
    stage_baseline, stage_lessons, uplift_assignment, ArmCall, BenchFailure, ImportRefusal,
    LabTask, LabTaskSet, Projection, ProjectionRefusal, SeedLesson, UpliftBench, UpliftOutcome,
    UpliftSettings, BENCH_FAILED, MIS_STAGED, RECEIPT_REJECTED,
};
use localx_eval_core::uplift::{
    text_digest, ArmIdentity, ArmRunIdentity, InjectionIdentity, TaskSetIdentity, UpliftIdentity,
    UPLIFT_RECEIPT_SCHEMA,
};

const LESSON: &str = "Run foo db sync before the integration tests";
const BASELINE_CONFIG: &str = "[learning]\nenabled = false\n";
const LESSONS_CONFIG: &str = "[learning]\nenabled = true\nallowed_scopes = [\"project\"]\n";

fn candidate(lesson: &str) -> CandidateLesson {
    let fact = EvidenceRef::identified(
        EvidenceKind::ToolEvent,
        "`run_shell` call `c1` failed",
        "localpilot-session:s",
        "localpilot-session:s#event:c1",
        "sha256:c1",
    )
    .redacted();
    let mut draft = HindsightDraft::new("Run the integration tests", "They failed on a stale db")
        .with_hypothesis(CausalHypothesis {
            claim: "the database had not been synced".to_string(),
            evidence_ids: vec![fact.id.clone()],
            confidence: Confidence::new(0.6).unwrap(),
        });
    draft.proposed_lesson = Some(lesson.to_string());
    CandidateLesson::new(
        LessonId::new("retro-1"),
        lesson,
        LessonCategory::Process,
        Confidence::new(0.4).unwrap(),
        SuggestedAction::PromoteToMemory,
    )
    .with_evidence(fact)
    .with_hindsight(draft)
}

fn approved(candidate: &CandidateLesson) -> LabTaskSet {
    LabTaskSet {
        version: 1,
        candidate_identity: candidate.content_identity(),
        tasks: vec![
            LabTask {
                id: "t1".to_string(),
                prompt: "The integration tests fail on a stale schema. What do I run first?"
                    .to_string(),
                expect: "foo db sync".to_string(),
            },
            LabTask {
                id: "t2".to_string(),
                prompt: "How do I refresh the database for the test suite?".to_string(),
                expect: "foo db sync".to_string(),
            },
        ],
        drafted_by: Some("local-model".to_string()),
        approved_by: Some("reviewer".to_string()),
        approved_at: Some(1_790_000_000),
    }
}

fn settings() -> UpliftSettings {
    UpliftSettings {
        answer_only: false,
        model: "fixture-model".to_string(),
        trials: 3,
        timeout_secs: 120,
        source_revision: "rev-1".to_string(),
    }
}

struct Fixture {
    root: tempfile::TempDir,
    candidate: CandidateLesson,
    assignment: LessonAssignment,
    projection: Projection,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join(".localmind.toml"),
        "[learning]\nenabled = true\nallowed_scopes = [\"project\"]\n",
    )
    .unwrap();
    let candidate = candidate(LESSON);
    let tasks = approved(&candidate);
    let assignment = uplift_assignment(&candidate, &tasks);
    let projection = project_uplift(&candidate, &assignment, &tasks, "rev-1").unwrap();
    Fixture {
        root,
        candidate,
        assignment,
        projection,
    }
}

/// What the stand-in does differently from a faithful run.
#[derive(Clone, Default)]
struct Script {
    baseline_passes: bool,
    lessons_pass: bool,
    /// Memories the baseline's turns record (a faithful baseline records what
    /// is in its store, which must be nothing).
    baseline_used: Option<Vec<String>>,
    /// Memories the lesson arm's turns record (`None`: what is in its store).
    lessons_used: Option<Vec<String>>,
    fail_arm: Option<(bool, BenchFailure)>,
    fail_prepare: Option<BenchFailure>,
    /// A seed pack other than the task set's own.
    seed_pack: Option<String>,
    tamper: Option<Tamper>,
    /// How long each arm takes, in milliseconds.
    arm_millis: u64,
    /// Tokens each arm's trial session reports.
    tokens_per_arm: u64,
    /// Cancel the run as the baseline arm finishes.
    cancel_after_baseline: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Tamper {
    Binding,
    TaskSetDigest,
    TaskSetNameOnly,
    OldSchema,
    RunId,
    BaselineConfig,
    Neither,
}

/// What the stand-in saw when an arm ran.
#[derive(Clone, Debug)]
struct ArmSeen {
    lesson_arm: bool,
    config: String,
    store: Vec<(String, String)>,
    project_untouched: bool,
}

struct StandIn {
    script: Script,
    project: PathBuf,
    seen: RefCell<Vec<ArmSeen>>,
    cancel: localpilot_harness::CancelSignal,
}

impl StandIn {
    fn new(project: &Path, script: Script) -> Self {
        Self::cancellable(project, script, localpilot_harness::CancelSignal::new())
    }

    /// A stand-in that stops when `cancel` fires, as the real runner does.
    fn cancellable(
        project: &Path,
        script: Script,
        cancel: localpilot_harness::CancelSignal,
    ) -> Self {
        Self {
            script,
            project: project.to_path_buf(),
            seen: RefCell::new(Vec::new()),
            cancel,
        }
    }
}

#[async_trait(?Send)]
impl UpliftBench for StandIn {
    async fn arm_config(&self, lesson_arm: bool) -> Result<String, BenchFailure> {
        if let Some(failure) = &self.script.fail_prepare {
            return Err(failure.clone());
        }
        Ok(if lesson_arm {
            LESSONS_CONFIG
        } else {
            BASELINE_CONFIG
        }
        .to_string())
    }

    async fn seed_pack(&self, task_set: &Path) -> Result<String, BenchFailure> {
        if let Some(pack) = &self.script.seed_pack {
            return Ok(pack.clone());
        }
        let set: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(task_set).unwrap()).unwrap();
        let lessons: Vec<serde_json::Value> = set["lessons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|lesson| {
                serde_json::json!({
                    "body": lesson["body"],
                    "category": lesson["category"],
                    "confidence": 0.9,
                    "tags": [],
                })
            })
            .collect();
        Ok(
            serde_json::to_string_pretty(&serde_json::json!({ "lessons": lessons })).unwrap()
                + "\n",
        )
    }

    async fn run_arm(&self, call: &ArmCall<'_>) -> Result<(), BenchFailure> {
        if let Some((arm, failure)) = &self.script.fail_arm {
            if *arm == call.lesson_arm {
                return Err(failure.clone());
            }
        }
        let config = std::fs::read_to_string(call.workspace.join(".localmind.toml")).unwrap();
        let store: Vec<(String, String)> = memory_list_readonly(call.workspace)
            .unwrap()
            .into_iter()
            .map(|memory| (memory.id, memory.body))
            .collect();
        self.seen.borrow_mut().push(ArmSeen {
            lesson_arm: call.lesson_arm,
            config: config.clone(),
            store: store.clone(),
            project_untouched: memory_list_readonly(&self.project).unwrap().is_empty(),
        });
        if self.script.tokens_per_arm > 0 {
            let sessions = call.workspace.join(".localpilot").join("sessions");
            std::fs::create_dir_all(&sessions).unwrap();
            let name = if call.lesson_arm {
                "lessons"
            } else {
                "baseline"
            };
            std::fs::write(
                sessions.join(format!("{name}.jsonl")),
                format!(
                    "{{\"kind\":{{\"type\":\"usage_reported\",\"input_tokens\":{},\"output_tokens\":0}}}}\n",
                    self.script.tokens_per_arm
                ),
            )
            .unwrap();
        }
        let ends =
            std::time::Instant::now() + std::time::Duration::from_millis(self.script.arm_millis);
        while std::time::Instant::now() < ends {
            if self.cancel.is_cancelled() {
                return Err(BenchFailure::Cancelled);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        if self.script.cancel_after_baseline && !call.lesson_arm {
            self.cancel.cancel();
        }
        let in_store: Vec<String> = store.into_iter().map(|(id, _)| id).collect();
        let (passes, used) = if call.lesson_arm {
            (
                self.script.lessons_pass,
                self.script.lessons_used.clone().unwrap_or(in_store),
            )
        } else {
            (
                self.script.baseline_passes,
                self.script.baseline_used.clone().unwrap_or(in_store),
            )
        };
        let set_bytes = std::fs::read(call.task_set).unwrap();
        let set: serde_json::Value = serde_json::from_slice(&set_bytes).unwrap();
        let identity = ArmRunIdentity {
            binding: call.binding.to_string(),
            task_set: TaskSetIdentity {
                name: set["name"].as_str().unwrap().to_string(),
                digest: text_digest(&set_bytes),
                task_count: set["tasks"].as_array().unwrap().len(),
            },
            arm: ArmIdentity {
                arm: if call.lesson_arm {
                    "lessons"
                } else {
                    "baseline"
                }
                .to_string(),
                is_lesson_arm: call.lesson_arm,
                config_digest: if call.settings.answer_only {
                    text_digest(
                        format!("answer-only-context-v1:{}", text_digest(config.as_bytes()))
                            .as_bytes(),
                    )
                } else {
                    text_digest(config.as_bytes())
                },
                model: call.settings.model.clone(),
                trials: call.settings.trials,
                timeout_secs: call.settings.timeout_secs,
                injection: if call.lesson_arm {
                    let pack = self.seed_pack(call.task_set).await?;
                    InjectionIdentity::lessons(
                        localx_eval_core::uplift::InjectionMode::Retrieved,
                        call.intended.to_vec(),
                        text_digest(pack.trim_end().as_bytes()),
                    )
                } else {
                    InjectionIdentity::none()
                },
            },
        };
        let file = serde_json::json!({ "identity": identity, "passes": passes, "used": used });
        std::fs::write(call.out, serde_json::to_string(&file).unwrap()).unwrap();
        Ok(())
    }

    async fn combine(
        &self,
        baseline: &Path,
        lessons: &Path,
        out: &Path,
    ) -> Result<(), BenchFailure> {
        let read = |path: &Path| -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
        };
        let (baseline, lessons) = (read(baseline), read(lessons));
        let arm_identity = |file: &serde_json::Value| -> ArmRunIdentity {
            serde_json::from_value(file["identity"].clone()).unwrap()
        };
        let mut identity =
            UpliftIdentity::pair(&arm_identity(&baseline), &arm_identity(&lessons)).unwrap();
        let used = |file: &serde_json::Value| -> Vec<String> {
            serde_json::from_value(file["used"].clone()).unwrap()
        };
        let intended = identity.lessons.injection.intended.clone();
        let baseline_void = !used(&baseline).is_empty();
        let lessons_void = !used(&lessons).iter().any(|id| intended.contains(id));
        let rate = |file: &serde_json::Value| if file["passes"] == true { 1.0 } else { 0.0 };
        let row = |file: &serde_json::Value, arm: &ArmIdentity, void: bool, intended: &[String]| {
            serde_json::json!({
                "arm": arm.arm,
                "is_lesson_arm": arm.is_lesson_arm,
                "trials": arm.trials,
                "task_count": identity.task_set.task_count,
                "per_trial_success_rate": vec![rate(file); arm.trials as usize],
                "mean": rate(file),
                "stddev": 0.0,
                "injection": if void { serde_json::Value::Null } else { serde_json::json!({
                    "arm": arm.arm,
                    "is_lesson_arm": arm.is_lesson_arm,
                    "intended": intended,
                    "injected": used(file),
                }) },
            })
        };
        let arms = vec![
            row(&baseline, &identity.baseline, baseline_void, &[]),
            row(&lessons, &identity.lessons, lessons_void, &intended),
        ];
        let delta = rate(&lessons) - rate(&baseline);
        let void = baseline_void || lessons_void;
        let mut uplift = (!void).then(|| {
            serde_json::json!({
                "baseline_arm": "baseline",
                "lesson_arm": "lessons",
                "baseline_mean": rate(&baseline),
                "lesson_mean": rate(&lessons),
                "delta": delta,
                "pooled_std_dev": 0.0,
                "band": 0.05,
                "effect_size": null,
                "verdict": if delta > 0.05 { "uplift" } else if delta < -0.05 { "regression" } else { "no-effect" },
            })
        });
        let mut void_reason = void.then(|| "an arm did not inject as configured".to_string());
        let mut schema = serde_json::json!(UPLIFT_RECEIPT_SCHEMA);
        let mut run_id = identity.run_id();
        match self.script.tamper {
            Some(Tamper::Binding) => identity.binding = "sha256:another-request".to_string(),
            Some(Tamper::TaskSetDigest) => {
                identity.task_set.digest = "sha256:another-task-set".to_string();
            }
            Some(Tamper::TaskSetNameOnly) => {
                identity.task_set.name = "renamed by the producer".to_string();
            }
            Some(Tamper::BaselineConfig) => {
                identity.baseline.config_digest = text_digest(LESSONS_CONFIG.as_bytes());
            }
            Some(Tamper::OldSchema) => schema = serde_json::json!(1),
            Some(Tamper::RunId) => {}
            Some(Tamper::Neither) => {
                uplift = None;
                void_reason = None;
            }
            None => {}
        }
        if self.script.tamper != Some(Tamper::RunId) {
            run_id = identity.run_id();
        } else {
            identity.lessons.trials += 1;
        }
        let receipt = serde_json::json!({
            "schema": schema,
            "run_id": run_id,
            "identity": identity,
            "arms": arms,
            "uplift": uplift,
            "void": void_reason,
        });
        std::fs::write(out, serde_json::to_string_pretty(&receipt).unwrap()).unwrap();
        Ok(())
    }
}

async fn run(fixture: &Fixture, script: Script, downweight: bool) -> (UpliftOutcome, Vec<ArmSeen>) {
    let bench = StandIn::new(fixture.root.path(), script);
    let outcome = run_uplift(
        fixture.root.path(),
        &fixture.candidate,
        &fixture.projection,
        &settings(),
        &bench,
        downweight,
    )
    .await;
    outcome
        .evidence
        .validate(&fixture.candidate)
        .unwrap_or_else(|violations| panic!("{violations:?}\n{:#?}", outcome.evidence));
    assert!(
        !outcome.run_dir.join("workspace").exists(),
        "the trial workspace is removed"
    );
    let seen = bench.seen.borrow().clone();
    (outcome, seen)
}

fn other(code: &str) -> VerdictReason {
    VerdictReason::Other(code.to_string())
}

#[test]
fn a_projection_keeps_the_lineage_back_to_the_lesson() {
    let fixture = fixture();
    let projection = &fixture.projection;
    assert_eq!(
        projection.lineage.candidate_identity,
        fixture.candidate.content_identity()
    );
    assert_eq!(
        projection.lineage.assignment_identity,
        fixture.assignment.identity()
    );
    assert_eq!(
        projection.lineage.oracle_hash,
        fixture.assignment.oracle.content_hash
    );
    assert_eq!(projection.lineage.source_revision, "rev-1");
    assert_eq!(
        projection.task_set.digest,
        text_digest(projection.task_set_json.as_bytes())
    );

    let set: serde_json::Value = serde_json::from_str(&projection.task_set_json).unwrap();
    assert_eq!(set["schema"], 1);
    assert_eq!(set["tasks"].as_array().unwrap().len(), 2);
    assert_eq!(set["tasks"][0]["expect"]["mode"], "substring");
    assert_eq!(
        set["tasks"][0]["lesson_ids"],
        serde_json::json!(["candidate"])
    );
    assert_eq!(set["lessons"].as_array().unwrap().len(), 1);
    assert_eq!(set["lessons"][0]["body"], LESSON);
    assert_eq!(set["lessons"][0]["category"], "Process");

    // Any change to what the run is bound to is another binding.
    let moved = project_uplift(
        &fixture.candidate,
        &fixture.assignment,
        &approved(&fixture.candidate),
        "rev-2",
    )
    .unwrap();
    assert_ne!(moved.lineage.binding(), projection.lineage.binding());
}

#[test]
fn a_lesson_is_projected_only_from_the_approved_tasks_it_was_frozen_with() {
    let fixture = fixture();
    let tasks = approved(&fixture.candidate);
    let project =
        |candidate: &CandidateLesson, assignment: &LessonAssignment, tasks: &LabTaskSet| {
            project_uplift(candidate, assignment, tasks, "rev-1").unwrap_err()
        };

    let revised = candidate("Run foo db sync before any test at all");
    assert_eq!(
        project(&revised, &fixture.assignment, &tasks),
        ProjectionRefusal::StaleAssignment
    );

    let mut draft = tasks.clone();
    draft.approved_by = None;
    assert_eq!(
        project(&fixture.candidate, &fixture.assignment, &draft),
        ProjectionRefusal::NotApproved
    );

    let mut edited = tasks.clone();
    edited.tasks[0].expect = "foo db migrate".to_string();
    assert_eq!(
        project(&fixture.candidate, &fixture.assignment, &edited),
        ProjectionRefusal::OracleChanged
    );

    let mut replay = fixture.assignment.clone();
    replay.source = Some(localmind_core::AssignmentSource::RatifiedCheck {
        name: "test".to_string(),
    });
    assert_eq!(
        project(&fixture.candidate, &replay, &tasks),
        ProjectionRefusal::NotAnUpliftAssignment
    );
}

#[tokio::test]
async fn the_arms_differ_only_in_the_seeded_lesson_and_a_passing_treatment_is_supported() {
    let fixture = fixture();
    let (outcome, seen) = run(
        &fixture,
        Script {
            lessons_pass: true,
            ..Script::default()
        },
        false,
    )
    .await;

    // Baseline: learning off, and a store that holds nothing.
    assert!(!seen[0].lesson_arm);
    assert_eq!(seen[0].config, BASELINE_CONFIG);
    assert!(seen[0].store.is_empty());
    // Lesson arm: learning on, and exactly the lesson under test.
    assert!(seen[1].lesson_arm);
    assert_eq!(seen[1].config, LESSONS_CONFIG);
    assert_eq!(seen[1].store.len(), 1);
    assert_eq!(seen[1].store[0].1, LESSON);
    // The project's own memory was never touched.
    assert!(seen.iter().all(|arm| arm.project_untouched));
    assert!(memory_list_readonly(fixture.root.path())
        .unwrap()
        .is_empty());

    let evidence = &outcome.evidence;
    assert_eq!(evidence.verdict, LabVerdict::Supported, "{evidence:#?}");
    assert_eq!(evidence.tier, EvidenceTier::Uplift);
    let proof = evidence.injection.unwrap();
    assert!(proof.assertion_passed);
    assert_eq!(
        proof.mode,
        InjectionMode::Retrieved,
        "never claimed as forced"
    );
    let receipt = evidence.receipt.as_ref().unwrap();
    assert_eq!(receipt.schema, UPLIFT_RECEIPT_SCHEMA);
    assert!(receipt.is_intact());
    assert_eq!(
        evidence.inputs.candidate_identity,
        fixture.candidate.content_identity()
    );
    assert_eq!(evidence.arms.len(), 2);
    assert_eq!((evidence.arms[0].attempts, evidence.arms[0].passed), (6, 0));
    assert_eq!((evidence.arms[1].attempts, evidence.arms[1].passed), (6, 6));

    // The intended id is the seeded memory's own id, proven by the receipt.
    let expected = outcome.expected.as_ref().unwrap();
    assert_eq!(
        expected.lessons.injection.intended,
        vec![seen[1].store[0].0.clone()]
    );
    for name in [
        "task-set.json",
        "lineage.json",
        "baseline.json",
        "lessons.json",
        "receipt.json",
    ] {
        assert!(outcome.run_dir.join(name).is_file(), "{name} is kept");
    }
    assert!(outcome.run_dir.starts_with(
        fixture
            .root
            .path()
            .join(".localpilot")
            .join("lab")
            .join("uplift")
    ));
}

#[tokio::test]
async fn no_effect_and_harm_are_results_and_stay_distinct_from_invalid() {
    let fixture = fixture();
    let verdict = |baseline_passes, lessons_pass| {
        let fixture = &fixture;
        async move {
            run(
                fixture,
                Script {
                    baseline_passes,
                    lessons_pass,
                    ..Script::default()
                },
                false,
            )
            .await
            .0
            .evidence
        }
    };
    let both_pass = verdict(true, true).await;
    assert_eq!(both_pass.verdict, LabVerdict::Inconclusive);
    assert!(both_pass.injection.unwrap().assertion_passed);
    assert!(both_pass.receipt.is_some());
    let both_fail = verdict(false, false).await;
    assert_eq!(both_fail.verdict, LabVerdict::Inconclusive);
    let harmful = verdict(true, false).await;
    assert_eq!(harmful.verdict, LabVerdict::Contradicted);
    assert!(harmful.injection.unwrap().assertion_passed);
    assert!(harmful.reasons.is_empty());
}

#[tokio::test]
async fn a_broken_injection_contract_is_an_invalid_experiment_never_a_verdict() {
    let fixture = fixture();
    // The control saw a memory.
    let (contaminated, _) = run(
        &fixture,
        Script {
            baseline_passes: true,
            baseline_used: Some(vec!["mem-stray".to_string()]),
            ..Script::default()
        },
        false,
    )
    .await;
    assert_eq!(contaminated.evidence.verdict, LabVerdict::InvalidExperiment);
    assert_eq!(
        contaminated.evidence.reasons,
        vec![VerdictReason::ArmContaminated]
    );
    assert!(!contaminated.evidence.injection.unwrap().assertion_passed);

    // The treatment used some other memory, or none: the lesson was never there.
    for used in [vec!["mem-some-other-lesson".to_string()], Vec::new()] {
        let (outcome, _) = run(
            &fixture,
            Script {
                lessons_pass: true,
                lessons_used: Some(used),
                ..Script::default()
            },
            false,
        )
        .await;
        assert_eq!(outcome.evidence.verdict, LabVerdict::InvalidExperiment);
        assert_eq!(
            outcome.evidence.reasons,
            vec![VerdictReason::InjectionNotObserved]
        );
        assert!(!outcome.evidence.injection.unwrap().assertion_passed);
    }
}

#[tokio::test]
async fn a_lesson_arm_that_also_used_another_memory_proves_nothing_about_the_lesson() {
    let fixture = fixture();
    let bench = StandIn::new(fixture.root.path(), Script::default());
    // Find the id the lesson will be seeded under, then script a turn that
    // records it together with a stray memory.
    let probe = tempfile::tempdir().unwrap();
    let pack = bench
        .seed_pack(&write_task_set(&fixture, probe.path()))
        .await
        .unwrap();
    let id = stage_lessons(probe.path(), LESSONS_CONFIG, &pack, LESSON).unwrap();

    let (outcome, _) = run(
        &fixture,
        Script {
            lessons_pass: true,
            lessons_used: Some(vec![id, "mem-stray".to_string()]),
            ..Script::default()
        },
        false,
    )
    .await;
    assert_eq!(outcome.evidence.verdict, LabVerdict::InvalidExperiment);
    assert_eq!(
        outcome.evidence.reasons,
        vec![VerdictReason::ArmContaminated]
    );
}

fn write_task_set(fixture: &Fixture, dir: &Path) -> PathBuf {
    let path = dir.join("task-set.json");
    std::fs::write(&path, &fixture.projection.task_set_json).unwrap();
    path
}

#[tokio::test]
async fn a_seed_pack_that_is_not_the_lesson_voids_before_the_lesson_arm_runs() {
    let fixture = fixture();
    let (outcome, seen) = run(
        &fixture,
        Script {
            seed_pack: Some(
                r#"{"lessons":[{"body":"Some other lesson entirely","category":"Process"}]}"#
                    .to_string(),
            ),
            ..Script::default()
        },
        false,
    )
    .await;
    assert_eq!(outcome.evidence.verdict, LabVerdict::InvalidExperiment);
    assert_eq!(outcome.evidence.reasons, vec![other(MIS_STAGED)]);
    assert_eq!(seen.len(), 1, "the lesson arm never ran");
    assert!(outcome.evidence.receipt.is_none());
}

#[test]
fn staging_proves_the_store_rather_than_assuming_it() {
    let workspace = tempfile::tempdir().unwrap();
    // A workspace a previous arm left a lesson in.
    std::fs::write(workspace.path().join(".localmind.toml"), LESSONS_CONFIG).unwrap();
    seed_memory(
        workspace.path(),
        &[SeedLesson {
            body: "left over".to_string(),
            category: None,
            confidence: None,
            related_files: Vec::new(),
            related_entities: Vec::new(),
            evidence: None,
            tags: Vec::new(),
        }],
        false,
    )
    .unwrap();
    assert_eq!(memory_list(workspace.path()).unwrap().len(), 1);

    stage_baseline(workspace.path(), BASELINE_CONFIG).unwrap();
    assert!(memory_list_readonly(workspace.path()).unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(workspace.path().join(".localmind.toml")).unwrap(),
        BASELINE_CONFIG
    );

    let pack = format!(r#"{{"lessons":[{{"body":{LESSON:?},"category":"Process"}}]}}"#);
    let id = stage_lessons(workspace.path(), LESSONS_CONFIG, &pack, LESSON).unwrap();
    let held = memory_list(workspace.path()).unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].id, id);

    let two = format!(r#"{{"lessons":[{{"body":{LESSON:?}}},{{"body":"and a second lesson"}}]}}"#);
    assert!(
        stage_lessons(workspace.path(), LESSONS_CONFIG, &two, LESSON)
            .unwrap_err()
            .contains("2 lessons")
    );
}

#[tokio::test]
async fn half_a_pair_or_a_stopped_run_is_invalid_with_its_reason() {
    let fixture = fixture();
    let failed = |arm: bool, failure: BenchFailure| Script {
        lessons_pass: true,
        fail_arm: Some((arm, failure)),
        ..Script::default()
    };
    let cases = [
        (
            failed(
                true,
                BenchFailure::Failed("localbench exited 1".to_string()),
            ),
            vec![other(BENCH_FAILED), VerdictReason::PartialPair],
        ),
        (
            failed(true, BenchFailure::Cancelled),
            vec![VerdictReason::Cancelled, VerdictReason::PartialPair],
        ),
        (
            failed(true, BenchFailure::TimedOut),
            vec![VerdictReason::BudgetExceeded, VerdictReason::PartialPair],
        ),
        (
            failed(false, BenchFailure::Denied),
            vec![other("PermissionDenied")],
        ),
        (
            Script {
                fail_prepare: Some(BenchFailure::Failed("localbench not found".to_string())),
                ..Script::default()
            },
            vec![other(BENCH_FAILED)],
        ),
    ];
    for (script, reasons) in cases {
        let (outcome, _) = run(&fixture, script, false).await;
        assert_eq!(outcome.evidence.verdict, LabVerdict::InvalidExperiment);
        assert_eq!(outcome.evidence.reasons, reasons);
        assert!(outcome.evidence.receipt.is_none(), "no half result is kept");
        assert!(outcome.evidence.arms.is_empty());
    }
}

#[tokio::test]
async fn a_receipt_that_is_not_the_requested_run_is_rejected_not_coerced() {
    let fixture = fixture();
    for tamper in [
        Tamper::Binding,
        Tamper::TaskSetDigest,
        Tamper::BaselineConfig,
        Tamper::OldSchema,
        Tamper::RunId,
        Tamper::Neither,
    ] {
        let (outcome, _) = run(
            &fixture,
            Script {
                lessons_pass: true,
                tamper: Some(tamper),
                ..Script::default()
            },
            false,
        )
        .await;
        assert_eq!(outcome.evidence.verdict, LabVerdict::InvalidExperiment);
        assert_eq!(outcome.evidence.reasons, vec![other(RECEIPT_REJECTED)]);
        assert!(
            outcome.evidence.receipt.is_none(),
            "a rejected receipt is not attached"
        );
        assert!(outcome.evidence.injection.is_none());
    }

    // A task set's name is not what a receipt is matched on.
    let (renamed, _) = run(
        &fixture,
        Script {
            lessons_pass: true,
            tamper: Some(Tamper::TaskSetNameOnly),
            ..Script::default()
        },
        false,
    )
    .await;
    assert_eq!(renamed.evidence.verdict, LabVerdict::Supported);
}

#[tokio::test]
async fn import_matches_by_identity_and_says_exactly_why_it_refuses() {
    let fixture = fixture();
    let (outcome, _) = run(
        &fixture,
        Script {
            lessons_pass: true,
            ..Script::default()
        },
        false,
    )
    .await;
    let payload = std::fs::read_to_string(outcome.run_dir.join("receipt.json")).unwrap();
    let expected = outcome.expected.unwrap();
    assert_eq!(
        import_receipt(&payload, &expected).unwrap().verdict,
        LabVerdict::Supported
    );

    // The same receipt, offered for a run that asked for a different lesson.
    let mut another = expected.clone();
    another.lessons.injection.intended = vec!["mem-of-another-lesson".to_string()];
    let refusal = import_receipt(&payload, &another).unwrap_err();
    assert!(
        matches!(&refusal, ImportRefusal::Mismatch(detail) if detail.contains("injection")),
        "{refusal}"
    );
    let mut another = expected.clone();
    another.baseline.model = "another-model".to_string();
    assert!(matches!(
        import_receipt(&payload, &another).unwrap_err(),
        ImportRefusal::Mismatch(_)
    ));
    assert!(matches!(
        import_receipt("not json", &expected).unwrap_err(),
        ImportRefusal::Unreadable(_)
    ));
    assert_eq!(
        import_receipt(r#"{"schema":1,"task_set":"headroom"}"#, &expected).unwrap_err(),
        ImportRefusal::Schema("1".to_string())
    );
}

#[tokio::test]
async fn a_harmful_result_routes_accepted_memory_to_review_only_when_asked() {
    let harmful = || Script {
        baseline_passes: true,
        ..Script::default()
    };

    // The lesson is still a candidate: the result is the recommendation.
    let fixture_a = fixture();
    let (outcome, _) = run(&fixture_a, harmful(), true).await;
    assert_eq!(outcome.evidence.verdict, LabVerdict::Contradicted);
    assert!(outcome.flagged_for_review.is_empty());

    // The lesson is already accepted memory in the project.
    let fixture_b = fixture();
    let accept = |root: &Path| {
        seed_memory(
            root,
            &[SeedLesson {
                body: LESSON.to_string(),
                category: Some("Process".to_string()),
                confidence: Some(0.8),
                related_files: Vec::new(),
                related_entities: Vec::new(),
                evidence: None,
                tags: Vec::new(),
            }],
            false,
        )
        .unwrap();
        memory_list(root).unwrap()[0].id.clone()
    };
    let id = accept(fixture_b.root.path());
    let bench = StandIn::new(fixture_b.root.path(), harmful());
    let flagged = run_uplift(
        fixture_b.root.path(),
        &fixture_b.candidate,
        &fixture_b.projection,
        &settings(),
        &bench,
        true,
    )
    .await;
    assert_eq!(flagged.flagged_for_review, vec![id.clone()]);
    assert_eq!(
        memory_list(fixture_b.root.path()).unwrap().len(),
        1,
        "routed to review, never deleted"
    );
    assert!(flagged
        .evidence
        .limitations
        .iter()
        .any(|l| l.contains("routed to review")));

    // Off by default: nothing is flagged.
    let fixture_c = fixture();
    accept(fixture_c.root.path());
    let bench = StandIn::new(fixture_c.root.path(), harmful());
    let quiet = run_uplift(
        fixture_c.root.path(),
        &fixture_c.candidate,
        &fixture_c.projection,
        &settings(),
        &bench,
        false,
    )
    .await;
    assert_eq!(quiet.evidence.verdict, LabVerdict::Contradicted);
    assert!(quiet.flagged_for_review.is_empty());

    // A supported result never flags anything.
    let fixture_d = fixture();
    accept(fixture_d.root.path());
    let bench = StandIn::new(
        fixture_d.root.path(),
        Script {
            lessons_pass: true,
            ..Script::default()
        },
    );
    let good = run_uplift(
        fixture_d.root.path(),
        &fixture_d.candidate,
        &fixture_d.projection,
        &settings(),
        &bench,
        true,
    )
    .await;
    assert_eq!(good.evidence.verdict, LabVerdict::Supported);
    assert!(good.flagged_for_review.is_empty());
}

/// The review output: the predeclared verdict mapping, with the manifest each
/// receipt was matched against. Run with `--nocapture` to print it.
#[tokio::test]
async fn review_verdict_mapping() {
    let fixture = fixture();
    let cases: Vec<(&str, Script)> = vec![
        (
            "control fails, treatment passes",
            Script {
                lessons_pass: true,
                ..Script::default()
            },
        ),
        (
            "both pass",
            Script {
                baseline_passes: true,
                lessons_pass: true,
                ..Script::default()
            },
        ),
        ("both fail", Script::default()),
        (
            "harmful: control passes, treatment fails",
            Script {
                baseline_passes: true,
                ..Script::default()
            },
        ),
        (
            "invalid: the control saw a memory",
            Script {
                baseline_used: Some(vec!["mem-stray".to_string()]),
                ..Script::default()
            },
        ),
        (
            "invalid: the treatment used another lesson",
            Script {
                lessons_pass: true,
                lessons_used: Some(vec!["mem-other".to_string()]),
                ..Script::default()
            },
        ),
        (
            "mis-staged: the seed pack is not the lesson",
            Script {
                seed_pack: Some(r#"{"lessons":[{"body":"Another lesson"}]}"#.to_string()),
                ..Script::default()
            },
        ),
        (
            "partial pair: the lesson arm failed",
            Script {
                fail_arm: Some((true, BenchFailure::Failed("exit 1".to_string()))),
                ..Script::default()
            },
        ),
        (
            "receipt of another request",
            Script {
                lessons_pass: true,
                tamper: Some(Tamper::Binding),
                ..Script::default()
            },
        ),
    ];
    println!("| Pair | Verdict | Reasons | Injection proof | Receipt kept |");
    println!("|---|---|---|---|---|");
    let mut manifest = None;
    for (name, script) in cases {
        let (outcome, _) = run(&fixture, script, false).await;
        let evidence = &outcome.evidence;
        let reasons = evidence
            .reasons
            .iter()
            .map(|reason| format!("{reason:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let proof = evidence.injection.map_or("—".to_string(), |proof| {
            format!(
                "{:?}, {}",
                proof.mode,
                if proof.assertion_passed {
                    "passed"
                } else {
                    "failed"
                }
            )
        });
        println!(
            "| {name} | {:?} | {} | {proof} | {} |",
            evidence.verdict,
            if reasons.is_empty() { "—" } else { &reasons },
            if evidence.receipt.is_some() {
                "yes"
            } else {
                "no"
            }
        );
        if manifest.is_none() {
            manifest = outcome.expected;
        }
    }
    let manifest = manifest.unwrap();
    println!("MANIFEST binding={}", short(&manifest.binding));
    println!(
        "MANIFEST task_set={} ({} tasks)",
        short(&manifest.task_set.digest),
        manifest.task_set.task_count
    );
    for arm in [&manifest.baseline, &manifest.lessons] {
        println!(
            "MANIFEST {} config={} model={} trials={} timeout={}s intended={} seed_pack={}",
            arm.arm,
            short(&arm.config_digest),
            arm.model,
            arm.trials,
            arm.timeout_secs,
            arm.injection.intended.len(),
            arm.injection
                .seed_pack_digest
                .as_deref()
                .map_or("—".to_string(), short)
        );
    }
}

fn short(digest: &str) -> String {
    digest.chars().take(19).collect()
}

/// A stand-in solver for the real LocalBench: it answers from what is actually
/// in the workspace's memory store, and records the memory it found there.
fn stand_in_solver(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        let path = dir.join("solver.cmd");
        std::fs::write(
            &path,
            "@echo off\r\n\
             if not exist .localpilot\\sessions mkdir .localpilot\\sessions\r\n\
             set ID=\r\n\
             for %%f in (.localmind\\memory\\project\\*.md) do set ID=%%~nf\r\n\
             if \"%ID%\"==\"\" goto none\r\n\
             echo {\"kind\":{\"type\":\"memories_used\",\"memories\":[{\"id\":\"%ID%\",\"score\":5,\"layer\":\"memory\"}]}}> .localpilot\\sessions\\turn.jsonl\r\n\
             echo Run foo db sync first.\r\n\
             exit /b 0\r\n\
             :none\r\n\
             echo {\"kind\":{\"type\":\"turn_done\"}}> .localpilot\\sessions\\turn.jsonl\r\n\
             echo I am not sure.\r\n",
        )
        .unwrap();
        path
    } else {
        let path = dir.join("solver.sh");
        std::fs::write(
            &path,
            "#!/bin/sh\n\
             mkdir -p .localpilot/sessions\n\
             file=$(ls .localmind/memory/project/*.md 2>/dev/null | head -n 1)\n\
             if [ -n \"$file\" ]; then\n\
               id=$(basename \"$file\" .md)\n\
               printf '{\"kind\":{\"type\":\"memories_used\",\"memories\":[{\"id\":\"%s\",\"score\":5,\"layer\":\"memory\"}]}}\n' \"$id\" > .localpilot/sessions/turn.jsonl\n\
               echo 'Run foo db sync first.'\n\
             else\n\
               printf '{\"kind\":{\"type\":\"turn_done\"}}\n' > .localpilot/sessions/turn.jsonl\n\
               echo 'I am not sure.'\n\
             fi\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }
}

/// The production path: the real `localbench` binary, run through the
/// permission-gated runner, against the stand-in solver. Runs only when
/// `LOCALPILOT_TEST_LOCALBENCH` names a `localbench` built from a revision
/// with the per-arm surface; the two repositories do not link each other.
#[tokio::test]
async fn the_real_localbench_runs_both_arms_and_its_receipt_is_imported() {
    let Some(program) = std::env::var_os("LOCALPILOT_TEST_LOCALBENCH") else {
        eprintln!("NOTICE: LOCALPILOT_TEST_LOCALBENCH is not set; the real-binary run was skipped");
        return;
    };
    use localpilot_localmind::LocalBenchCli;
    use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover};

    let fixture = fixture();
    let tools = tempfile::tempdir().unwrap();
    let engine = PermissionEngine::new(Profile::Bypass, Vec::new());
    let approver = ScriptedApprover::always();
    let bench = LocalBenchCli {
        program: program.to_string_lossy().into_owned(),
        solver: stand_in_solver(tools.path()).to_string_lossy().into_owned(),
        engine: &engine,
        approver: &approver,
        interactivity: Interactivity::NonInteractive,
        cancel: localpilot_harness::CancelSignal::new(),
        arm_timeout: std::time::Duration::from_secs(120),
        cwd: tools.path().to_path_buf(),
    };

    let outcome = run_uplift(
        fixture.root.path(),
        &fixture.candidate,
        &fixture.projection,
        &settings(),
        &bench,
        false,
    )
    .await;

    let evidence = &outcome.evidence;
    assert_eq!(evidence.verdict, LabVerdict::Supported, "{evidence:#?}");
    evidence.validate(&fixture.candidate).unwrap();
    assert!(evidence.injection.unwrap().assertion_passed);
    let receipt: serde_json::Value =
        serde_json::from_str(&evidence.receipt.as_ref().unwrap().payload).unwrap();
    assert_eq!(receipt["schema"], UPLIFT_RECEIPT_SCHEMA);
    assert_eq!(
        receipt["identity"]["binding"],
        fixture.projection.lineage.binding()
    );
    assert_eq!(receipt["uplift"]["verdict"], "uplift");
    assert_eq!(
        receipt["arms"][1]["injection"]["injected"],
        serde_json::json!(
            outcome
                .expected
                .as_ref()
                .unwrap()
                .lessons
                .injection
                .intended
        )
    );
    assert!(!outcome.run_dir.join("workspace").exists());
    assert!(memory_list_readonly(fixture.root.path())
        .unwrap()
        .is_empty());

    let mut answer_settings = settings();
    answer_settings.answer_only = true;
    let answer = run_uplift(
        fixture.root.path(),
        &fixture.candidate,
        &fixture.projection,
        &answer_settings,
        &bench,
        false,
    )
    .await;
    assert_eq!(
        answer.evidence.verdict,
        LabVerdict::Supported,
        "{:?}",
        answer.evidence
    );
    let receipt: serde_json::Value =
        serde_json::from_str(&answer.evidence.receipt.as_ref().unwrap().payload).unwrap();
    assert_eq!(receipt["answer_only"], true);
    assert_ne!(
        answer.expected.as_ref().unwrap().baseline.config_digest,
        outcome.expected.as_ref().unwrap().baseline.config_digest
    );

    // A denied command never reaches LocalBench: the run is invalid, not a verdict.
    let strict = PermissionEngine::new(Profile::Default, Vec::new());
    let refusing = ScriptedApprover::new(Vec::new());
    let denied = LocalBenchCli {
        engine: &strict,
        approver: &refusing,
        ..bench
    };
    let outcome = run_uplift(
        fixture.root.path(),
        &fixture.candidate,
        &fixture.projection,
        &settings(),
        &denied,
        false,
    )
    .await;
    assert_eq!(outcome.evidence.verdict, LabVerdict::InvalidExperiment);
    assert_eq!(outcome.evidence.reasons, vec![other("PermissionDenied")]);
}

// --- starting a run: authorization, ceilings, status and restart -----------

use localpilot_localmind::{
    approve_tasks, plan_uplift, read_run_state, run_planned_with, run_statuses, sweep_uplift_runs,
    uplift_authorization, write_draft, PreviewedCommands, RunStanding, UpliftCeilings, UpliftPlan,
    UpliftRefusal, UpliftTools,
};

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A project that enables uplift in its committed configuration, with a lesson
/// whose tasks a person has approved.
struct Enabled {
    root: tempfile::TempDir,
    candidate: CandidateLesson,
    assignment: LessonAssignment,
}

fn enabled_with(config: &str) -> Enabled {
    let root = tempfile::tempdir().unwrap();
    let path = root.path();
    git(path, &["init", "-q"]);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["config", "user.name", "Test"]);
    git(path, &["config", "core.autocrlf", "false"]);
    std::fs::write(path.join(".localpilot.toml"), config).unwrap();
    std::fs::write(
        path.join(".gitignore"),
        ".localpilot/\n.localmind/\n.localmind.toml\n",
    )
    .unwrap();
    std::fs::write(
        path.join(".localmind.toml"),
        "[learning]\nenabled = true\nallowed_scopes = [\"project\"]\n",
    )
    .unwrap();
    git(path, &["add", "-A"]);
    git(path, &["commit", "-q", "-m", "base"]);
    let candidate = candidate(LESSON);
    let mut draft = approved(&candidate);
    draft.approved_by = None;
    draft.approved_at = None;
    let localpilot = path.join(".localpilot");
    write_draft(&localpilot, &draft).unwrap();
    let (_, assignment) = approve_tasks(&localpilot, &candidate, "reviewer", 1).unwrap();
    Enabled {
        root,
        candidate,
        assignment,
    }
}

fn enabled() -> Enabled {
    enabled_with("[lab]\nuplift = true\n")
}

fn tools() -> UpliftTools {
    UpliftTools {
        localbench: "localbench".to_string(),
        solver: "localpilot".to_string(),
    }
}

impl Enabled {
    fn plan_with(&self, ceilings: UpliftCeilings) -> Result<UpliftPlan, UpliftRefusal> {
        plan_uplift(
            self.root.path(),
            &self.candidate,
            &self.assignment,
            "fixture-model",
            ceilings,
            tools(),
        )
    }

    fn plan(&self) -> UpliftPlan {
        self.plan_with(UpliftCeilings::default()).unwrap()
    }

    async fn run(
        &self,
        plan: &UpliftPlan,
        script: Script,
        reuse_baseline: bool,
    ) -> (UpliftOutcome, Vec<ArmSeen>) {
        let cancel = localpilot_harness::CancelSignal::new();
        let bench = StandIn::cancellable(self.root.path(), script, cancel.clone());
        let outcome = run_planned_with(
            self.root.path(),
            &self.candidate,
            plan,
            &bench,
            &cancel,
            reuse_baseline,
            false,
        )
        .await
        .unwrap();
        outcome.evidence.validate(&self.candidate).unwrap();
        let seen = bench.seen.borrow().clone();
        (outcome, seen)
    }
}

fn passing() -> Script {
    Script {
        lessons_pass: true,
        ..Script::default()
    }
}

#[test]
fn a_run_is_planned_only_where_the_committed_config_enables_it_and_tasks_are_approved() {
    let off = enabled_with("[lab]\nreplay = true\n");
    assert_eq!(
        off.plan_with(UpliftCeilings::default()).unwrap_err(),
        UpliftRefusal::NotEnabled
    );
    // Enabled only in the working copy: not the trust boundary.
    std::fs::write(
        off.root.path().join(".localpilot.toml"),
        "[lab]\nuplift = true\n",
    )
    .unwrap();
    assert!(matches!(
        off.plan_with(UpliftCeilings::default()).unwrap_err(),
        UpliftRefusal::Untrusted(_)
    ));

    let project = enabled();
    assert!(project.plan_with(UpliftCeilings::default()).is_ok());
    assert!(matches!(
        project
            .plan_with(UpliftCeilings {
                wall_secs: 0,
                ..UpliftCeilings::default()
            })
            .unwrap_err(),
        UpliftRefusal::Ceiling(_)
    ));

    // Stale inputs: the lesson was revised after its tasks were approved.
    let revised = candidate("Run foo db sync before any test at all");
    assert_eq!(
        plan_uplift(
            project.root.path(),
            &revised,
            &project.assignment,
            "fixture-model",
            UpliftCeilings::default(),
            tools(),
        )
        .unwrap_err(),
        UpliftRefusal::NoApprovedTasks
    );
    assert!(
        run_statuses(project.root.path()).is_empty(),
        "planning starts nothing"
    );
}

#[test]
fn the_authorization_states_the_product_the_ceilings_and_every_command() {
    let project = enabled();
    let plan = project.plan();
    let shown = uplift_authorization(&plan);

    assert!(shown.contains("real model sessions"), "{shown}");
    assert!(
        shown.contains("12 turns of `localpilot` with model `fixture-model`"),
        "{shown}"
    );
    assert!(
        shown
            .contains("2 task(s) x 3 trial(s) x 2 arms = 12 model turn(s), with no limit per turn"),
        "the default has no per-turn limit and says so: {shown}"
    );
    assert!(
        shown.contains("--timeout 0 "),
        "the solver is told no bound: {shown}"
    );
    // A limit someone asks for is shown as the product, not the per-turn number.
    let bounded = uplift_authorization(
        &project
            .plan_with(UpliftCeilings {
                turn_timeout_secs: 120,
                ..UpliftCeilings::default()
            })
            .unwrap(),
    );
    assert!(
        bounded.contains("2 task(s) x 3 trial(s) x 2 arms x 120 s per turn = at most 24 min"),
        "{bounded}"
    );
    assert!(bounded.contains("--timeout 120 "), "{bounded}");
    assert!(shown.contains("wall clock: 30 min"), "{shown}");
    assert!(shown.contains("tokens: 400000"), "{shown}");
    assert!(
        shown.contains("Your project and your own memory are not touched"),
        "{shown}"
    );
    assert_eq!(plan.commands.len(), 6);
    for command in &plan.commands {
        assert!(command.starts_with("localbench uplift "), "{command}");
        assert!(shown.contains(command.as_str()), "{command}");
    }
    assert!(
        plan.commands[4].contains("--arm lessons")
            && plan.commands[4].ends_with("--intended <seeded-id>")
    );
    assert_eq!(plan.resume, None);
    assert!(!plan.run_dir.exists(), "a preview creates nothing");
}

#[tokio::test]
async fn the_confirmation_answers_only_the_previewed_commands() {
    use localpilot_sandbox::{Approver, Effect, Interactivity, PermissionRequest};
    let project = enabled();
    let plan = project.plan();
    let approver = PreviewedCommands::of(&plan);
    let request = |tool: &str, detail: String| PermissionRequest {
        tool: tool.to_string(),
        effect: Effect::RunCommand(localpilot_sandbox::CommandClass::Unknown),
        interactivity: Interactivity::Interactive,
        trusted: true,
        detail,
    };
    // The lesson arm's command, with the real seeded id in place.
    let real = plan.commands[4].replace("<seeded-id>", "seed-0123456789abcdef");
    assert!(
        approver
            .approve(&request("quality_check", real.clone()))
            .await
    );
    assert!(
        approver
            .approve(&request("quality_check", plan.commands[0].clone()))
            .await
    );
    // Not another tool, and not a command that was never shown.
    assert!(!approver.approve(&request("run_shell", real.clone())).await);
    assert!(
        !approver
            .approve(&request("quality_check", format!("{real} --extra")))
            .await
    );
    assert!(
        !approver
            .approve(&request("quality_check", "localbench findbest".to_string()))
            .await
    );
}

#[tokio::test]
async fn an_authorized_run_reports_its_status_and_what_it_measured() {
    let project = enabled();
    let plan = project.plan();
    let (outcome, seen) = project
        .run(
            &plan,
            Script {
                lessons_pass: true,
                tokens_per_arm: 700,
                arm_millis: 60,
                ..Script::default()
            },
            false,
        )
        .await;

    assert_eq!(outcome.evidence.verdict, LabVerdict::Supported);
    assert_eq!(seen.len(), 2);
    assert_eq!(
        outcome.run_dir, plan.run_dir,
        "the previewed directory is the one used"
    );

    // Reported separately from the result; what was not measured says so.
    let telemetry = &outcome.telemetry;
    assert!(telemetry.baseline_wall_ms.unwrap() >= 50);
    assert!(telemetry.lessons_wall_ms.unwrap() >= 50);
    assert!(telemetry.total_wall_ms >= 100);
    assert_eq!(telemetry.tokens, Some(1_400));
    assert_eq!(
        (&telemetry.model_state, &telemetry.ram, &telemetry.gpu),
        (&None, &None, &None)
    );
    assert!(outcome.evidence.arms.iter().all(|arm| arm.wall_ms >= 50));

    let statuses = run_statuses(project.root.path());
    assert_eq!(statuses.len(), 1);
    let (dir, state, standing) = &statuses[0];
    assert_eq!(dir, &plan.run_dir);
    assert_eq!(*standing, RunStanding::Ended);
    assert_eq!(state.stage, "finished");
    assert_eq!(state.verdict.as_deref(), Some("Supported"));
    assert_eq!(state.telemetry, *telemetry);
    assert!(
        !project
            .root
            .path()
            .join(".localpilot/lab/uplift.lock")
            .exists(),
        "the lock is released"
    );
}

#[tokio::test]
async fn a_breached_ceiling_cancels_the_run_and_is_never_a_partial_verdict() {
    let project = enabled();

    // Wall clock, during the baseline arm.
    let plan = project
        .plan_with(UpliftCeilings {
            wall_secs: 1,
            ..UpliftCeilings::default()
        })
        .unwrap();
    let started = std::time::Instant::now();
    let (wall, seen) = project
        .run(
            &plan,
            Script {
                lessons_pass: true,
                arm_millis: 20_000,
                ..Script::default()
            },
            false,
        )
        .await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "cancelled, not waited out"
    );
    assert_eq!(wall.evidence.verdict, LabVerdict::InvalidExperiment);
    assert_eq!(wall.evidence.reasons, vec![VerdictReason::BudgetExceeded]);
    assert!(wall
        .evidence
        .limitations
        .iter()
        .any(|l| l.contains("wall-clock ceiling of 1 s")));
    assert_eq!(seen.len(), 1, "the lesson arm never started");
    assert!(wall.evidence.receipt.is_none());

    // Tokens, watched while the lesson arm runs: the baseline had finished.
    let plan = project
        .plan_with(UpliftCeilings {
            max_tokens: 1_000,
            ..UpliftCeilings::default()
        })
        .unwrap();
    let (tokens, seen) = project
        .run(
            &plan,
            Script {
                lessons_pass: true,
                arm_millis: 900,
                tokens_per_arm: 600,
                ..Script::default()
            },
            false,
        )
        .await;
    assert_eq!(tokens.evidence.verdict, LabVerdict::InvalidExperiment);
    assert_eq!(
        tokens.evidence.reasons,
        vec![VerdictReason::BudgetExceeded, VerdictReason::PartialPair]
    );
    assert!(
        tokens
            .evidence
            .limitations
            .iter()
            .any(|l| l.contains("1200 tokens, past its ceiling of 1000")),
        "{:?}",
        tokens.evidence.limitations
    );
    assert_eq!(seen.len(), 2);
    let state = read_run_state(&tokens.run_dir).unwrap();
    assert_eq!(state.stage, "invalid");
    assert_eq!(state.reasons, vec!["BudgetExceeded", "PartialPair"]);
}

#[tokio::test]
async fn a_run_cancelled_between_the_arms_is_invalid_and_its_baseline_is_only_offered() {
    let project = enabled();
    let plan = project.plan();
    assert_eq!(plan.resume, None);

    let (cancelled, seen) = project
        .run(
            &plan,
            Script {
                lessons_pass: true,
                cancel_after_baseline: true,
                ..Script::default()
            },
            false,
        )
        .await;
    assert_eq!(cancelled.evidence.verdict, LabVerdict::InvalidExperiment);
    assert_eq!(
        cancelled.evidence.reasons,
        vec![VerdictReason::Cancelled, VerdictReason::PartialPair]
    );
    assert_eq!(seen.len(), 1, "the lesson arm never ran");
    assert!(
        cancelled.evidence.receipt.is_none(),
        "half a pair is not a result"
    );

    // The same request again: the finished baseline is offered, not taken.
    let again = project.plan();
    let offer = again
        .resume
        .clone()
        .expect("a finished baseline is offered");
    assert_eq!(offer.run_dir, plan.run_dir);
    assert!(uplift_authorization(&again).contains("can be reused instead of running it again"));

    // A historical coding-agent baseline (including a state without the new
    // field) cannot be offered to an answer-only run of the same tasks.
    let state_path = cancelled.run_dir.join("state.json");
    let original_state = std::fs::read_to_string(&state_path).unwrap();
    let mut legacy_state: serde_json::Value = serde_json::from_str(&original_state).unwrap();
    legacy_state.as_object_mut().unwrap().remove("answer_only");
    std::fs::write(&state_path, serde_json::to_string(&legacy_state).unwrap()).unwrap();
    assert_eq!(project.plan().resume, None);
    std::fs::write(&state_path, original_state).unwrap();

    // Declined: both arms run.
    let (fresh, seen) = project.run(&again, passing(), false).await;
    assert_eq!(fresh.evidence.verdict, LabVerdict::Supported);
    assert_eq!(seen.len(), 2);
    assert!(!read_run_state(&fresh.run_dir).unwrap().baseline_reused);

    // Accepted: only the lesson arm runs, and the pair still matches.
    let resumed_plan = project.plan();
    assert!(resumed_plan.resume.is_some());
    let (resumed, seen) = project.run(&resumed_plan, passing(), true).await;
    assert_eq!(
        resumed.evidence.verdict,
        LabVerdict::Supported,
        "{:#?}",
        resumed.evidence
    );
    assert_eq!(seen.len(), 1);
    assert!(seen[0].lesson_arm);
    assert!(read_run_state(&resumed.run_dir).unwrap().baseline_reused);
    assert_eq!(
        resumed.telemetry.baseline_wall_ms, None,
        "not run, so not measured"
    );

    // Different inputs are a different request: nothing is offered.
    let other_settings = project
        .plan_with(UpliftCeilings {
            trials: 5,
            ..UpliftCeilings::default()
        })
        .unwrap();
    assert_eq!(other_settings.resume, None);
}

#[tokio::test]
async fn a_killed_run_reads_as_interrupted_and_a_live_one_refuses_a_second() {
    let project = enabled();
    let plan = project.plan();
    let (finished, _) = project.run(&plan, passing(), false).await;

    // A run killed during its lesson arm: its state never reached an end.
    let killed = project
        .root
        .path()
        .join(".localpilot/lab/uplift/killed-run");
    std::fs::create_dir_all(&killed).unwrap();
    let mut state = read_run_state(&finished.run_dir).unwrap();
    state.stage = "lessons".to_string();
    state.verdict = None;
    state.started_at += 100;
    std::fs::write(
        killed.join("state.json"),
        serde_json::to_string(&state).unwrap(),
    )
    .unwrap();
    std::fs::copy(
        finished.run_dir.join("baseline.json"),
        killed.join("baseline.json"),
    )
    .unwrap();

    let statuses = run_statuses(project.root.path());
    assert_eq!(statuses.len(), 2);
    assert_eq!(statuses[0].2, RunStanding::Ended);
    assert_eq!(
        statuses[1].2,
        RunStanding::Interrupted,
        "no live lock: not running"
    );
    let offer = project
        .plan()
        .resume
        .expect("its finished baseline is offered");
    assert_eq!(offer.run_dir, killed);
    assert!(offer.ended.contains("interrupted during `lessons`"));

    // While a run holds the lock, the open run reads as running and a second
    // run is refused.
    let lock = project.root.path().join(".localpilot/lab/uplift.lock");
    std::fs::write(&lock, "4242 0\n").unwrap();
    assert_eq!(run_statuses(project.root.path())[1].2, RunStanding::Running);
    let bench = StandIn::new(project.root.path(), passing());
    let busy = run_planned_with(
        project.root.path(),
        &project.candidate,
        &project.plan(),
        &bench,
        &localpilot_harness::CancelSignal::new(),
        false,
        false,
    )
    .await
    .unwrap_err();
    assert!(matches!(busy, UpliftRefusal::Busy(_)), "{busy}");
    assert!(bench.seen.borrow().is_empty());
}

#[tokio::test]
async fn runs_past_retention_are_swept_by_location() {
    let project = enabled();
    let (kept, _) = project.run(&project.plan(), passing(), false).await;
    let old = project.root.path().join(".localpilot/lab/uplift/old-run");
    std::fs::create_dir_all(&old).unwrap();
    let state = old.join("state.json");
    std::fs::write(&state, "{}").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&state)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86_400))
        .unwrap();

    assert_eq!(
        sweep_uplift_runs(project.root.path()),
        vec!["old-run".to_string()]
    );
    assert!(!old.exists());
    assert!(kept.run_dir.is_dir(), "within retention");
}

/// Cancellation through the production runner reaps the whole tree: a
/// grandchild that keeps writing stops when the run is cancelled. The effect is
/// observed, not an exit code.
#[tokio::test]
async fn cancelling_the_real_runner_reaps_the_process_tree() {
    use localpilot_localmind::LocalBenchCli;
    use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover};

    let dir = tempfile::tempdir().unwrap();
    let heartbeat = dir.path().join("heartbeat.txt");
    let program = if cfg!(windows) {
        std::fs::write(
            dir.path().join("beat.cmd"),
            "@echo off\r\n:loop\r\necho x>>heartbeat.txt\r\nping -n 2 127.0.0.1 >nul\r\ngoto loop\r\n",
        )
        .unwrap();
        let path = dir.path().join("hang.cmd");
        std::fs::write(
            &path,
            "@echo off\r\nstart /b cmd /c %~dp0beat.cmd\r\nping -n 60 127.0.0.1 >nul\r\n",
        )
        .unwrap();
        path
    } else {
        let path = dir.path().join("hang.sh");
        std::fs::write(
            &path,
            "#!/bin/sh\n(while true; do echo x >> heartbeat.txt; sleep 0.2; done) &\nsleep 60\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    };
    let engine = PermissionEngine::new(Profile::Bypass, Vec::new());
    let approver = ScriptedApprover::always();
    let cancel = localpilot_harness::CancelSignal::new();
    let bench = LocalBenchCli {
        program: program.to_string_lossy().into_owned(),
        solver: "localpilot".to_string(),
        engine: &engine,
        approver: &approver,
        interactivity: Interactivity::NonInteractive,
        cancel: cancel.clone(),
        arm_timeout: std::time::Duration::from_secs(120),
        cwd: dir.path().to_path_buf(),
    };
    let trigger = {
        let cancel = cancel.clone();
        let heartbeat = heartbeat.clone();
        tokio::spawn(async move {
            for _ in 0..200 {
                if heartbeat.is_file() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            cancel.cancel();
        })
    };
    let settings = settings();
    let out = dir.path().join("arm.json");
    let call = ArmCall {
        lesson_arm: false,
        task_set: &dir.path().join("tasks.json"),
        workspace: dir.path(),
        settings: &settings,
        binding: "b",
        intended: &[],
        out: &out,
    };

    let result = bench.run_arm(&call).await;
    trigger.await.unwrap();

    assert_eq!(result, Err(BenchFailure::Cancelled));
    assert!(heartbeat.is_file(), "the grandchild ran");
    let settled = std::fs::metadata(&heartbeat).unwrap().len();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    assert_eq!(
        std::fs::metadata(&heartbeat).unwrap().len(),
        settled,
        "a reaped tree writes nothing more"
    );
}
