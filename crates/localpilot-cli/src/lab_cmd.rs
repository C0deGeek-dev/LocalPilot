//! `localpilot lab`: the lesson lab's explicit tiers.
//!
//! Logic runs by itself when a harness run completes. Replay does not: it runs
//! the project's own ratified check on real commits, so it is off unless the
//! project's committed `.localpilot.toml` enables it, and every run is shown
//! first and needs its own confirmation.

use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use localmind_core::AssignmentSource;
use localpilot_harness::{CancelSignal, QUALITY_CHECK_TOOL};

/// The permission engine Replay runs under: the configured profile, with the
/// quality-check identity allowlisted as it is for the ratified gate.
///
/// # Errors
/// Loading configuration.
pub fn engine(root: &Path) -> anyhow::Result<PermissionEngine> {
    let config = localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(root),
        &localpilot_config::CliOverrides::default(),
    )?;
    Ok(PermissionEngine::new(
        crate::session_cmd::resolve_profile_from_config(&config),
        vec![QUALITY_CHECK_TOOL.to_string()],
    ))
}
use localpilot_localmind::{
    attach_lab_evidence, lab_candidate, plan_replay, read_lab_records, replay_preview, run_replay,
    ReplayPlan,
};
use localpilot_sandbox::{Interactivity, PermissionEngine};
use localpilot_store::Store;

/// How a Replay run is confirmed.
pub enum Confirmation<'a> {
    /// `--yes`: confirmed up front, headless.
    Yes,
    /// Ask on this terminal.
    Prompt(&'a mut dyn BufRead),
    /// No terminal and no `--yes`: nothing may run.
    Unavailable,
}

/// Every lesson the lab classified, what it can run, and the results each
/// lesson carries in review.
///
/// # Errors
/// Writing the output, or reading the review queue.
pub fn list(root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let store = Store::open(root);
    let records = read_lab_records(store.root());
    if records.is_empty() {
        writeln!(out, "No lessons have been classified for the lab yet.")?;
        return Ok(());
    }
    for record in &records {
        let sources: Vec<&str> = record
            .assignments
            .iter()
            .map(|assignment| source_name(assignment.source.as_ref()))
            .collect();
        let detail = if sources.is_empty() {
            record
                .reasons
                .iter()
                .map(|reason| format!("{reason:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            sources.join(", ")
        };
        writeln!(
            out,
            "{}  {:?} — {detail}",
            record.candidate_identity, record.eligibility
        )?;
        match lab_candidate(root, &record.candidate_identity)? {
            Some((_, candidate)) if !candidate.experiments.is_empty() => {
                for result in &candidate.experiments {
                    let stale = if result.is_stale_for(&candidate) {
                        " (stale)"
                    } else {
                        ""
                    };
                    writeln!(
                        out,
                        "    {:?} {:?}{}{stale}",
                        result.tier,
                        result.verdict,
                        reasons_suffix(&result.reasons)
                    )?;
                }
            }
            Some(_) => writeln!(out, "    no results yet")?,
            None => writeln!(out, "    no longer in review")?,
        }
    }
    Ok(())
}

/// Plan every Replay assignment for the lessons matching `selection` (a
/// candidate identity or a prefix of one; all when `None`), show what would
/// run, and run it only once confirmed. Each result goes onto its lesson in
/// review; an assignment that no longer holds is recorded as `Invalid` without
/// running anything.
///
/// # Errors
/// Writing the output, loading configuration, or reading the review queue.
#[allow(clippy::too_many_arguments)] // one command's whole context, each part named
pub async fn replay(
    root: &Path,
    selection: Option<&str>,
    confirmation: Confirmation<'_>,
    timeout: Duration,
    engine: &PermissionEngine,
    cancel: &CancelSignal,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let store = Store::open(root);
    let revision = git_head(root);
    let mut plans: Vec<ReplayPlan> = Vec::new();
    for record in read_lab_records(store.root()) {
        if selection.is_some_and(|wanted| !record.candidate_identity.starts_with(wanted)) {
            continue;
        }
        let replayable: Vec<_> = record
            .assignments
            .iter()
            .filter(|assignment| {
                matches!(
                    assignment.source,
                    Some(
                        AssignmentSource::FailFixPair { .. }
                            | AssignmentSource::ControlledMutation { .. }
                    )
                )
            })
            .collect();
        if replayable.is_empty() {
            continue;
        }
        let Some((_, candidate)) = lab_candidate(root, &record.candidate_identity)? else {
            writeln!(
                out,
                "{}: no longer in review; skipped",
                record.candidate_identity
            )?;
            continue;
        };
        for assignment in replayable {
            match plan_replay(root, &candidate, assignment, timeout) {
                Ok(plan) => plans.push(plan),
                Err(refusal) => {
                    writeln!(out, "{}: {refusal}", record.candidate_identity)?;
                    if let Some(evidence) = refusal.evidence(&candidate, assignment, &revision) {
                        attach_lab_evidence(root, evidence)?;
                        writeln!(out, "  recorded as Invalid on the lesson in review")?;
                    }
                }
            }
        }
    }
    if plans.is_empty() {
        writeln!(out, "Nothing to replay.")?;
        return Ok(());
    }
    for plan in &plans {
        write!(out, "{}", replay_preview(plan))?;
    }

    let interactivity = match confirmation {
        Confirmation::Yes => Interactivity::NonInteractive,
        Confirmation::Unavailable => {
            writeln!(
                out,
                "Nothing ran: confirm on a terminal, or pass --yes to run without asking."
            )?;
            return Ok(());
        }
        Confirmation::Prompt(input) => {
            write!(out, "Run {} Replay run(s)? [y/N] ", plans.len())?;
            out.flush()?;
            let mut answer = String::new();
            input.read_line(&mut answer)?;
            if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                writeln!(out, "Nothing ran.")?;
                return Ok(());
            }
            Interactivity::Interactive
        }
    };

    for plan in &plans {
        if cancel.is_cancelled() {
            writeln!(out, "Cancelled; nothing further ran.")?;
            break;
        }
        let outcome = match run_replay(root, plan, engine, interactivity, cancel).await {
            Ok(outcome) => outcome,
            Err(refusal) => {
                writeln!(out, "Nothing ran: {refusal}")?;
                break;
            }
        };
        writeln!(
            out,
            "{}: Replay {:?}{}",
            plan.candidate_identity,
            outcome.evidence.verdict,
            reasons_suffix(&outcome.evidence.reasons)
        )?;
        for arm in &outcome.receipt.arms {
            writeln!(out, "  {}: {} ({})", arm.name, arm.end, arm.cleanup)?;
        }
        for swept in &outcome.receipt.swept_worktrees {
            writeln!(out, "  swept a leftover worktree: {swept}")?;
        }
        if let Some(path) = &outcome.receipt_path {
            writeln!(out, "  receipt: {}", path.display())?;
        }
        if !attach_lab_evidence(root, outcome.evidence)? {
            writeln!(
                out,
                "  the lesson is no longer in review; the result was not kept"
            )?;
        }
    }
    Ok(())
}

/// The one classified lesson `selection` names: a candidate identity or an
/// unambiguous prefix of one, still in review.
fn resolve(
    root: &Path,
    selection: &str,
) -> anyhow::Result<(
    localpilot_localmind::LabClassification,
    localmind_core::CandidateLesson,
)> {
    let store = Store::open(root);
    let mut matches: Vec<_> = read_lab_records(store.root())
        .into_iter()
        .filter(|record| record.candidate_identity.starts_with(selection))
        .collect();
    let record = match matches.len() {
        0 => {
            anyhow::bail!("no classified lesson matches `{selection}` (see `localpilot lab list`)")
        }
        1 => matches.remove(0),
        n => anyhow::bail!("`{selection}` matches {n} lessons; give more of the identity"),
    };
    let Some((_, candidate)) = lab_candidate(root, &record.candidate_identity)? else {
        anyhow::bail!("{} is no longer in review", record.candidate_identity);
    };
    Ok((record, candidate))
}

fn print_tasks(set: &localpilot_localmind::LabTaskSet, out: &mut dyn Write) -> std::io::Result<()> {
    for task in &set.tasks {
        writeln!(out, "  {}: {}", task.id, task.prompt)?;
        writeln!(out, "      expects: {}", task.expect)?;
    }
    Ok(())
}

/// Have the configured model draft uplift tasks for a lesson, and write them as
/// a draft for a person to read, edit and approve. A draft tests nothing.
///
/// Sends the lesson and its hindsight — not the run's raw facts — to the
/// provider the project is already configured to use.
///
/// # Errors
/// The lesson cannot be resolved, the model cannot draft, or the draft cannot
/// be written.
pub async fn tasks_draft(
    root: &Path,
    selection: &str,
    model: &str,
    provider: &dyn localpilot_llm::ModelProvider,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let (record, candidate) = resolve(root, selection)?;
    let drafted = localpilot_localmind::draft_tasks(provider, model, &candidate)
        .await
        .map_err(|failure| anyhow::anyhow!("no draft: {failure}"))?;
    let store = Store::open(root);
    let path = localpilot_localmind::write_draft(store.root(), &drafted.set)?;
    writeln!(
        out,
        "Drafted {} task(s) for {} with {model} ({} model call(s){}):",
        drafted.set.tasks.len(),
        record.candidate_identity,
        drafted.model_calls,
        if drafted.repaired { ", one repair" } else { "" }
    )?;
    print_tasks(&drafted.set, out)?;
    writeln!(out, "Draft: {}", path.display())?;
    writeln!(
        out,
        "A draft tests nothing. Read it — would a model get these wrong without the lesson, and \
         is the expected text the behaviour the lesson is about? Edit the file if needed, then:"
    )?;
    writeln!(
        out,
        "  localpilot lab tasks approve {} --reviewer <your name>",
        record.candidate_identity
    )?;
    Ok(())
}

/// Show a lesson's draft and its approved task set, if any.
///
/// # Errors
/// The lesson cannot be resolved or a file cannot be read.
pub fn tasks_show(root: &Path, selection: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let (record, candidate) = resolve(root, selection)?;
    let store = Store::open(root);
    writeln!(
        out,
        "{}: {}",
        record.candidate_identity,
        candidate.summary()
    )?;
    let draft = localpilot_localmind::read_task_set(&localpilot_localmind::draft_path(
        store.root(),
        &record.candidate_identity,
    ))
    .map_err(anyhow::Error::msg)?;
    match &draft {
        Some(set) => {
            writeln!(
                out,
                "Draft ({} task(s){}):",
                set.tasks.len(),
                set.drafted_by
                    .as_ref()
                    .map(|model| format!(", drafted by {model}"))
                    .unwrap_or_default()
            )?;
            print_tasks(set, out)?;
            if let Err(problems) = localpilot_localmind::validate_tasks(set, &candidate) {
                for problem in problems {
                    writeln!(out, "  cannot be approved: {problem}")?;
                }
            }
        }
        None => writeln!(out, "No draft.")?,
    }
    match localpilot_localmind::approved_tasks(store.root(), &candidate) {
        Ok(Some(set)) => {
            writeln!(
                out,
                "Approved by {} ({} task(s), {}):",
                set.approved_by.as_deref().unwrap_or("?"),
                set.tasks.len(),
                set.content_hash()
            )?;
            print_tasks(&set, out)?;
        }
        Ok(None) => writeln!(out, "Not approved.")?,
        Err(problem) => writeln!(out, "The approved set no longer holds: {problem}")?,
    }
    Ok(())
}

/// Approve a lesson's draft in `reviewer`'s name: freeze it as the lesson's
/// uplift assignment and keep it in the lesson's lab record.
///
/// # Errors
/// The lesson cannot be resolved, the approval is refused, or the record
/// cannot be written.
pub fn tasks_approve(
    root: &Path,
    selection: &str,
    reviewer: &str,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let (mut record, candidate) = resolve(root, selection)?;
    let store = Store::open(root);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        });
    let (set, assignment) =
        localpilot_localmind::approve_tasks(store.root(), &candidate, reviewer, now)
            .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    // One approved task set per lesson: a new approval replaces the old one.
    record.assignments.retain(|assignment| {
        !matches!(
            assignment.source,
            Some(AssignmentSource::ApprovedTaskSet { .. })
        )
    });
    record.assignments.push(assignment);
    localpilot_localmind::write_lab_record(store.root(), &record)?;
    writeln!(
        out,
        "Approved {} task(s) for {} as {}. Frozen as {}.",
        set.tasks.len(),
        record.candidate_identity,
        set.approved_by.as_deref().unwrap_or_default(),
        set.content_hash()
    )?;
    Ok(())
}

fn source_name(source: Option<&AssignmentSource>) -> &'static str {
    match source {
        Some(AssignmentSource::RecordedTrajectory { .. }) => "recorded trajectory",
        Some(AssignmentSource::FailFixPair { .. }) => "fail/fix pair",
        Some(AssignmentSource::ControlledMutation { .. }) => "controlled mutation",
        Some(AssignmentSource::RatifiedCheck { .. }) => "ratified check",
        Some(AssignmentSource::ApprovedTaskSet { .. }) => "approved task set",
        _ => "other",
    }
}

fn reasons_suffix(reasons: &[localmind_core::VerdictReason]) -> String {
    if reasons.is_empty() {
        String::new()
    } else {
        format!(
            " — {}",
            reasons
                .iter()
                .map(|reason| format!("{reason:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn git_head(root: &Path) -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(
            || "unknown".to_string(),
            |output| String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use localmind_core::{
        CandidateLesson, CausalHypothesis, Confidence, EvidenceKind, EvidenceRef, HindsightDraft,
        LabVerdict, LessonCategory, LessonId, Observation, SuggestedAction, VerdictReason,
    };
    use localpilot_config::CheckConfig;
    use localpilot_harness::{check_command_digest, Progress};
    use localpilot_localmind::{classify_for_lab, LabContext, RATIFIED_CHECK_KEY};
    use localpilot_sandbox::Profile;

    const SESSION: &str = "0b0e6c1e-0000-4000-8000-000000000009";

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit(root: &Path, files: &[(&str, &str)], message: &str) -> String {
        for (path, content) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", message]);
        git(root, &["rev-parse", "HEAD"])
    }

    #[cfg(windows)]
    fn check() -> CheckConfig {
        check_of("findstr", &["/b", "/c:fixed", "state.txt"])
    }
    #[cfg(not(windows))]
    fn check() -> CheckConfig {
        check_of("grep", &["-qx", "fixed", "state.txt"])
    }

    fn check_of(program: &str, args: &[&str]) -> CheckConfig {
        CheckConfig {
            name: "test".to_string(),
            program: program.to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            fix_program: None,
            fix_args: Vec::new(),
            cadence: localpilot_config::Cadence::default(),
            auto_fix: localpilot_config::AutoFix::default(),
            severity: None,
        }
    }

    fn config_text(check: &CheckConfig) -> String {
        format!(
            "[lab]\nreplay = true\n\n[[harness.checks]]\nname = \"test\"\nprogram = {:?}\nargs = {:?}\n",
            check.program, check.args
        )
    }

    /// A project with a broken base, a fixing step, its lesson queued in
    /// review, and the lesson's frozen lab record — what a completed run
    /// leaves behind.
    fn project() -> (tempfile::TempDir, CandidateLesson) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        std::fs::write(
            root.join(".localmind.toml"),
            "[learning]\nenabled = true\nallowed_scopes = [\"project\"]\n",
        )
        .unwrap();
        let check = check();
        commit(
            root,
            &[
                (".localpilot.toml", &config_text(&check)),
                (".gitignore", ".localpilot/\n.localmind/\n.localmind.toml\n"),
                ("state.txt", "broken\n"),
            ],
            "base",
        );
        let fix = commit(
            root,
            &[("state.txt", "fixed\n")],
            "harness: write the state",
        );

        let mut failed = EvidenceRef::identified(
            EvidenceKind::TestOutput,
            "ratified check `test` failed (step)",
            format!("localpilot-session:{SESSION}"),
            format!("localpilot-session:{SESSION}#event:c1"),
            "sha256:c1",
        )
        .redacted()
        .with_observation(Observation::Failure)
        .with_signature(format!("check:test:{}", check_command_digest(&check)));
        failed
            .metadata
            .insert(RATIFIED_CHECK_KEY.to_string(), "test".to_string());
        let mut draft = HindsightDraft::new("Write the state", "The check passes").with_hypothesis(
            CausalHypothesis {
                claim: "the state file had not been written yet".to_string(),
                evidence_ids: vec![failed.id.clone()],
                confidence: Confidence::new(0.6).unwrap(),
            },
        );
        let lesson = "Write the state file before the check that reads it";
        draft.proposed_lesson = Some(lesson.to_string());
        let candidate = CandidateLesson::new(
            LessonId::new("retro-1"),
            lesson,
            LessonCategory::Process,
            Confidence::new(0.4).unwrap(),
            SuggestedAction::PromoteToMemory,
        )
        .with_evidence(failed)
        .with_hindsight(draft);

        localmind_store::ReviewQueue::open_project(root)
            .unwrap()
            .enqueue_candidates(
                &localmind_core::SessionId::new("completion-retrospective"),
                std::slice::from_ref(&candidate),
            )
            .unwrap();
        let progress = Progress::parse(&format!(
            "# Progress: state\nBranch: feature/state\n\n## Steps\n\n\
- [x] 1. Write the state\n  - commit: {}\n  - attempts: 1\n  - sessions: {SESSION}\n",
            &fix[..7]
        ))
        .unwrap();
        let record = classify_for_lab(
            &candidate,
            &LabContext {
                root,
                progress: Some(&progress),
                checks: std::slice::from_ref(&check),
            },
        );
        assert!(!record.assignments.is_empty(), "{record:?}");
        std::fs::create_dir_all(root.join(".localpilot").join("lab").join("assignments")).unwrap();
        std::fs::write(
            root.join(".localpilot")
                .join("lab")
                .join("assignments")
                .join(format!("{}.json", record.candidate_identity)),
            serde_json::to_string_pretty(&record).unwrap(),
        )
        .unwrap();
        (dir, candidate)
    }

    fn engine() -> PermissionEngine {
        PermissionEngine::new(Profile::Default, vec![QUALITY_CHECK_TOOL.to_string()])
    }

    async fn run(root: &Path, confirmation: Confirmation<'_>) -> String {
        let mut out = Vec::new();
        replay(
            root,
            None,
            confirmation,
            Duration::from_secs(60),
            &engine(),
            &CancelSignal::new(),
            &mut out,
        )
        .await
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    fn results(root: &Path, candidate: &CandidateLesson) -> Vec<(LabVerdict, Vec<VerdictReason>)> {
        let (_, stored) = lab_candidate(root, &candidate.content_identity())
            .unwrap()
            .expect("still in review");
        stored
            .experiments
            .iter()
            .map(|result| (result.verdict, result.reasons.clone()))
            .collect()
    }

    fn worktrees(root: &Path) -> usize {
        std::fs::read_dir(root.join(".localpilot").join("worktrees"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn yes_runs_the_previewed_replay_and_the_result_reaches_review() {
        let (dir, candidate) = project();
        let root = dir.path();

        let printed = run(root, Confirmation::Yes).await;

        assert!(
            printed.contains("not a sandbox"),
            "the preview comes first: {printed}"
        );
        assert!(printed.contains("Replay Valid"), "{printed}");
        assert!(
            printed.contains("expect-fail: exited 1 (removed)"),
            "{printed}"
        );
        assert!(printed.contains("receipt: "), "{printed}");
        assert_eq!(
            results(root, &candidate),
            vec![(LabVerdict::Valid, Vec::new())]
        );
        assert_eq!(worktrees(root), 0);

        let mut listing = Vec::new();
        list(root, &mut listing).unwrap();
        let listing = String::from_utf8(listing).unwrap();
        assert!(listing.contains("Replay — fail/fix pair"), "{listing}");
        assert!(listing.contains("Replay Valid"), "{listing}");

        // Run again: the same inputs and verdict are not stored twice.
        run(root, Confirmation::Yes).await;
        assert_eq!(results(root, &candidate).len(), 1);
    }

    #[tokio::test]
    async fn nothing_runs_without_a_confirmation() {
        let (dir, candidate) = project();
        let root = dir.path();

        let printed = run(root, Confirmation::Unavailable).await;
        assert!(printed.contains("Nothing ran"), "{printed}");

        let mut declined = std::io::Cursor::new(b"n\n".to_vec());
        let printed = run(root, Confirmation::Prompt(&mut declined)).await;
        assert!(
            printed.contains("[y/N]") && printed.contains("Nothing ran."),
            "{printed}"
        );

        assert!(results(root, &candidate).is_empty());
        assert_eq!(worktrees(root), 0);
        assert!(!root.join(".localpilot").join("lab").join("runs").exists());

        let mut accepted = std::io::Cursor::new(b"y\n".to_vec());
        let printed = run(root, Confirmation::Prompt(&mut accepted)).await;
        assert!(printed.contains("Replay Valid"), "{printed}");
    }

    #[tokio::test]
    async fn an_assignment_that_no_longer_holds_is_recorded_invalid_without_running() {
        let (dir, candidate) = project();
        let root = dir.path();
        let loosened = check_of("findstr", &["/c:e", "state.txt"]);
        commit(
            root,
            &[(".localpilot.toml", &config_text(&loosened))],
            "loosen",
        );

        let printed = run(root, Confirmation::Yes).await;

        assert!(printed.contains("no longer holds"), "{printed}");
        assert!(printed.contains("recorded as Invalid"), "{printed}");
        assert!(printed.contains("Nothing to replay."), "{printed}");
        assert_eq!(
            results(root, &candidate),
            vec![(LabVerdict::Invalid, vec![VerdictReason::OracleMutable])]
        );
        assert_eq!(worktrees(root), 0);
    }

    #[tokio::test]
    async fn a_drafted_task_set_runs_nothing_until_a_named_person_approves_it() {
        use localpilot_llm::FakeProvider;

        let (dir, candidate) = project();
        let root = dir.path();
        let identity = candidate.content_identity();
        let reply = r#"{"tasks":[
            {"prompt":"The check reads a file that is not there yet. What should happen first?","expect":"write the state file"},
            {"prompt":"In which order do the state file and its check go?","expect":"state file first"}
        ]}"#;
        let provider = FakeProvider::new().text(reply);

        // Draft, by an unambiguous prefix of the identity.
        let mut out = Vec::new();
        tasks_draft(root, &identity[..10], "local-model", &provider, &mut out)
            .await
            .unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("Drafted 2 task(s)"), "{printed}");
        assert!(printed.contains("A draft tests nothing"), "{printed}");
        let store = Store::open(root);
        assert!(localpilot_localmind::draft_path(store.root(), &identity).is_file());

        let uplift = |root: &Path| {
            read_lab_records(Store::open(root).root())[0]
                .assignments
                .iter()
                .find(|assignment| {
                    matches!(
                        assignment.source,
                        Some(AssignmentSource::ApprovedTaskSet { .. })
                    )
                })
                .cloned()
        };
        assert_eq!(uplift(root), None, "a draft is not an assignment");
        let mut out = Vec::new();
        tasks_show(root, &identity, &mut out).unwrap();
        let shown = String::from_utf8(out).unwrap();
        assert!(shown.contains("drafted by local-model") && shown.contains("Not approved."));

        // An approval needs a name.
        let mut out = Vec::new();
        assert!(tasks_approve(root, &identity, " ", &mut out).is_err());
        assert_eq!(uplift(root), None);

        let mut out = Vec::new();
        tasks_approve(root, &identity, "reviewer", &mut out).unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("Approved 2 task(s)") && printed.contains("as reviewer"));

        // The frozen assignment is in the lesson's lab record, beside its
        // Replay assignment, and projects into a run.
        let assignment = uplift(root).expect("the approval froze an assignment");
        assert_eq!(
            assignment.source,
            Some(AssignmentSource::ApprovedTaskSet {
                approved_by: "reviewer".to_string(),
                drafted_by: Some("local-model".to_string()),
            })
        );
        assert!(read_lab_records(store.root())[0].assignments.len() >= 2);
        let tasks = localpilot_localmind::approved_tasks(store.root(), &candidate)
            .unwrap()
            .unwrap();
        let projection =
            localpilot_localmind::project_uplift(&candidate, &assignment, &tasks, "rev").unwrap();
        assert_eq!(projection.lineage.candidate_identity, identity);

        let mut listing = Vec::new();
        list(root, &mut listing).unwrap();
        assert!(String::from_utf8(listing)
            .unwrap()
            .contains("approved task set"));

        // Approving again replaces the approved set rather than adding a second.
        let mut out = Vec::new();
        tasks_approve(root, &identity, "second reviewer", &mut out).unwrap();
        let approved: Vec<_> = read_lab_records(store.root())[0]
            .assignments
            .iter()
            .filter(|a| matches!(a.source, Some(AssignmentSource::ApprovedTaskSet { .. })))
            .cloned()
            .collect();
        assert_eq!(approved.len(), 1);

        let mut out = Vec::new();
        assert!(tasks_show(root, "cnd-does-not-exist", &mut out).is_err());
    }
}
