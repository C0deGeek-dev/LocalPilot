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
        let outcome = run_replay(root, plan, engine, interactivity, cancel).await;
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

fn source_name(source: Option<&AssignmentSource>) -> &'static str {
    match source {
        Some(AssignmentSource::RecordedTrajectory { .. }) => "recorded trajectory",
        Some(AssignmentSource::FailFixPair { .. }) => "fail/fix pair",
        Some(AssignmentSource::ControlledMutation { .. }) => "controlled mutation",
        Some(AssignmentSource::RatifiedCheck { .. }) => "ratified check",
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
}
