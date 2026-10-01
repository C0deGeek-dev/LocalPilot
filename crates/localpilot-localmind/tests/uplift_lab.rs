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
}

impl StandIn {
    fn new(project: &Path, script: Script) -> Self {
        Self {
            script,
            project: project.to_path_buf(),
            seen: RefCell::new(Vec::new()),
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
                config_digest: text_digest(config.as_bytes()),
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
        serde_json::json!(outcome.expected.unwrap().lessons.injection.intended)
    );
    assert!(!outcome.run_dir.join("workspace").exists());
    assert!(memory_list_readonly(fixture.root.path())
        .unwrap()
        .is_empty());

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
