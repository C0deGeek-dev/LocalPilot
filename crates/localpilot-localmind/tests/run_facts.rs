//! Facts captured from a completed harness run: every fact attributable to
//! where it was read, absence kept as a gap rather than an observation, and the
//! set bounded, redacted and deterministic.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use localmind_core::{EvidenceKind, EvidenceRef};
use localpilot_core::{
    ContentBlock, Message, Role, SessionId, StructuredSummary, ToolCall, ToolOutcome, ToolResult,
};
use localpilot_harness::{Brief, BriefRevision};
use localpilot_localmind::{capture_run_facts, FactGap, RunFacts, MAX_RUN_FACTS};
use localpilot_store::{MessageOrigin, SessionEventKind, Store};
use serde_json::json;

const SECRET: &str = "sk-proj-abcdefghijklmnopqrstuvwxyz123456";

const BRIEF: &str = "# Brief: greeting\n\n## Summary\n\nGreet the user by name.\n\n\
## Requirements\n\n- Print a greeting\n\n## Constraints\n\n- Keep it small\n\n\
## Non-Goals\n\n- Internationalization\n\n## Acceptance Criteria\n\n\
- A greeting is printed\n- The greeting names the user\n";

/// A project whose single step is done and linked to `sessions`.
fn project(sessions: &[SessionId]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("brief.md"), BRIEF).unwrap();
    let revision = BriefRevision::of(&Brief::parse(BRIEF).unwrap());
    let linked = if sessions.is_empty() {
        String::new()
    } else {
        let ids: Vec<String> = sessions.iter().map(ToString::to_string).collect();
        format!("  - sessions: {}\n", ids.join(", "))
    };
    std::fs::write(
        dir.path().join("PROGRESS.md"),
        format!(
            "# Progress: greeting\nBranch: feature/greeting\nBrief: {revision}\n\n## Steps\n\n\
- [x] 1. Print a greeting\n  - commit: abc1234\n  - attempts: 1\n{linked}"
        ),
    )
    .unwrap();
    dir
}

struct Log<'a> {
    store: &'a Store,
    session: SessionId,
}

impl Log<'_> {
    fn push(&self, kind: SessionEventKind) {
        self.store.append_event(self.session, None, kind).unwrap();
    }

    fn step_started(&self) {
        self.push(SessionEventKind::StepStarted {
            number: 1,
            description: "Print a greeting".to_string(),
        });
    }

    fn call(&self, id: &str, name: &str, input: serde_json::Value) {
        self.push(SessionEventKind::Message {
            message: Message::new(
                Role::Assistant,
                vec![ContentBlock::ToolUse(ToolCall::new(id.into(), name, input))],
            ),
            origin: MessageOrigin::Assistant,
        });
    }

    fn result(&self, id: &str, output: &str, is_error: bool) {
        let result = if is_error {
            ToolResult::error(id.into(), output)
        } else {
            ToolResult::success(id.into(), output)
        };
        self.push(SessionEventKind::Message {
            message: Message::new(Role::Tool, vec![ContentBlock::ToolResult(result)]),
            origin: MessageOrigin::ToolResult,
        });
    }

    fn turn_ended(&self, stop: &str) {
        self.push(SessionEventKind::TurnEnded {
            stop: stop.to_string(),
            detail: None,
        });
    }
}

fn capture(dir: &tempfile::TempDir) -> RunFacts {
    capture_run_facts(dir.path(), &Store::open(dir.path()))
}

fn labelled<'a>(facts: &'a RunFacts, needle: &str) -> Vec<&'a EvidenceRef> {
    facts
        .facts
        .iter()
        .filter(|fact| fact.label.contains(needle))
        .collect()
}

#[test]
fn every_fact_has_a_verifiable_id_and_a_locator_back_to_its_source() {
    let session = SessionId::new();
    let dir = project(&[session]);
    let store = Store::open(dir.path());
    let log = Log {
        store: &store,
        session,
    };
    log.step_started();
    log.call("c1", "write_file", json!({ "path": "hello.txt" }));
    log.result("c1", "written", false);
    log.turn_ended("done");

    let run = capture(&dir);

    for expected in [
        "task: Greet the user by name.",
        "acceptance criterion 1: A greeting is printed",
        "acceptance criterion 2: The greeting names the user",
        "step 1 (done): Print a greeting",
        "step 1 committed as abc1234",
        "1 of 1 plan step(s) complete",
        "`write_file` call `c1` succeeded",
    ] {
        assert_eq!(labelled(&run, expected).len(), 1, "missing: {expected}");
    }
    for fact in &run.facts {
        assert!(fact.has_canonical_id(), "{fact:?}");
        assert!(fact.identity_is_intact(), "{fact:?}");
        assert!(fact.redacted);
        assert!(fact.uri.is_some() && fact.source().is_some());
    }
    let call = labelled(&run, "`write_file` call `c1`")[0];
    assert_eq!(call.kind, EvidenceKind::ToolEvent);
    assert!(call
        .uri
        .as_deref()
        .unwrap()
        .starts_with(&format!("localpilot-session:{session}#event:")));
}

#[test]
fn a_failure_carries_its_redacted_input_and_output_and_a_success_carries_neither() {
    let session = SessionId::new();
    let dir = project(&[session]);
    let store = Store::open(dir.path());
    let log = Log {
        store: &store,
        session,
    };
    log.step_started();
    log.call(
        "c1",
        "run_shell",
        json!({ "command": format!("deploy --token {SECRET}") }),
    );
    log.result("c1", &format!("401: token {SECRET} rejected"), true);
    log.push(SessionEventKind::ToolFinished {
        id: "c1".to_string(),
        name: "run_shell".to_string(),
        is_error: true,
        outcome: Some(ToolOutcome::ReportedFailure),
    });
    log.call("c2", "read_file", json!({ "path": "README.md" }));
    log.result("c2", "# readme", false);
    log.turn_ended("done");

    let run = capture(&dir);

    let failed = labelled(
        &run,
        "`run_shell` call `c1` failed (it ran and reported failure)",
    );
    assert_eq!(failed.len(), 1);
    let excerpt = failed[0].excerpt.as_deref().unwrap();
    assert!(excerpt.contains("rejected") && excerpt.contains("deploy --token"));
    // The event store already redacts this pattern on write, so this holds
    // before capture runs; the next test shows capture's own pass.
    let everything = serde_json::to_string(&run.facts).unwrap();
    assert!(
        !everything.contains(SECRET),
        "nothing captured carries the secret: {everything}"
    );
    assert_eq!(
        labelled(&run, "`read_file` call `c2` succeeded")[0].excerpt,
        None
    );
}

#[test]
fn capture_redacts_what_the_event_store_does_not_know_to() {
    let session = SessionId::new();
    let dir = project(&[session]);
    // A path the project configured as sensitive. The event store's redactor
    // knows nothing of LocalMind configuration, so it survives into the log.
    std::fs::write(
        dir.path().join(".localmind.toml"),
        "[learning]\nenabled = true\nallowed_scopes = [\"project\"]\n\
excluded_paths = [\"internal/acquisition\"]\n",
    )
    .unwrap();
    let store = Store::open(dir.path());
    let log = Log {
        store: &store,
        session,
    };
    log.step_started();
    log.call(
        "c1",
        "read_file",
        json!({ "path": "internal/acquisition/terms.md" }),
    );
    log.result(
        "c1",
        "internal/acquisition/terms.md: permission denied",
        true,
    );
    let logged = std::fs::read_to_string(
        store
            .root()
            .join("sessions")
            .join(format!("{session}.events.jsonl")),
    )
    .unwrap();
    assert!(
        logged.contains("internal/acquisition"),
        "precondition: the log itself still carries the path"
    );

    let run = capture(&dir);

    let everything = serde_json::to_string(&run.facts).unwrap();
    assert!(!everything.contains("internal/acquisition"), "{everything}");
    let failed = labelled(&run, "`read_file` call `c1` failed");
    assert!(failed[0]
        .excerpt
        .as_deref()
        .unwrap()
        .contains("[REDACTED:sensitive_path]"));
    assert!(
        failed[0].identity_is_intact(),
        "the id is taken over the redacted content"
    );
}

#[test]
fn a_partial_log_yields_what_it_recorded_and_says_what_it_did_not() {
    let session = SessionId::new();
    let dir = project(&[session]);
    let store = Store::open(dir.path());
    let log = Log {
        store: &store,
        session,
    };
    log.step_started();
    log.call("c1", "write_file", json!({ "path": "a.txt" }));
    // The process died mid-write: a damaged line, and no result for c1.
    let path = store
        .root()
        .join("sessions")
        .join(format!("{session}.events.jsonl"));
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("{\"v\":1,\"id\":\"trunc");
    std::fs::write(&path, text).unwrap();

    let run = capture(&dir);

    assert!(run.gaps.contains(&FactGap::SessionPartlyUnreadable {
        step: 1,
        session: session.to_string(),
        skipped_lines: 1,
    }));
    assert!(run.gaps.contains(&FactGap::ResultNotRecorded {
        session: session.to_string(),
        call: "c1".to_string(),
        tool: "write_file".to_string(),
        pending: true,
    }));
    // The invocation is recorded, so it is a fact; its outcome is not, so no
    // fact claims one.
    assert_eq!(
        labelled(&run, "`write_file` call `c1` was invoked").len(),
        1
    );
    assert!(labelled(&run, "`write_file` call `c1` failed").is_empty());
    assert!(labelled(&run, "`write_file` call `c1` succeeded").is_empty());
}

#[test]
fn a_call_the_log_moved_past_is_missing_not_failed() {
    let session = SessionId::new();
    let dir = project(&[session]);
    let store = Store::open(dir.path());
    let log = Log {
        store: &store,
        session,
    };
    log.step_started();
    log.call("c1", "write_file", json!({ "path": "a.txt" }));
    log.turn_ended("done");

    let run = capture(&dir);

    assert!(run.gaps.contains(&FactGap::ResultNotRecorded {
        session: session.to_string(),
        call: "c1".to_string(),
        tool: "write_file".to_string(),
        pending: false,
    }));
    assert!(labelled(&run, "failed").is_empty());
}

#[test]
fn a_repeating_log_yields_one_fact_per_observation_and_the_same_set_every_time() {
    let session = SessionId::new();
    let dir = project(&[session]);
    let store = Store::open(dir.path());
    let log = Log {
        store: &store,
        session,
    };
    log.step_started();
    for _ in 0..3 {
        log.call("c1", "read_file", json!({ "path": "a.txt" }));
        log.result("c1", "contents", false);
    }
    log.turn_ended("done");

    let first = capture(&dir);
    let second = capture(&dir);

    assert_eq!(labelled(&first, "`read_file` call `c1`").len(), 1);
    assert_eq!(first, second, "capture is deterministic");
    let mut ids: Vec<_> = first.facts.iter().map(|fact| fact.id.clone()).collect();
    let count = ids.len();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    ids.dedup();
    assert_eq!(ids.len(), count, "no id appears twice");
}

#[test]
fn a_disagreeing_repeat_is_flagged_not_resolved() {
    let session = SessionId::new();
    let dir = project(&[session]);
    let store = Store::open(dir.path());
    let log = Log {
        store: &store,
        session,
    };
    log.step_started();
    log.call("c1", "write_file", json!({ "path": "a.txt" }));
    log.result("c1", "written", false);
    log.result("c1", "disk full", true);

    let run = capture(&dir);

    assert_eq!(
        labelled(
            &run,
            "`write_file` call `c1` succeeded; a later result disagreed"
        )
        .len(),
        1
    );
    assert!(run.gaps.contains(&FactGap::ConflictingResult {
        session: session.to_string(),
        call: "c1".to_string(),
        tool: "write_file".to_string(),
    }));
}

#[test]
fn missing_verifier_data_is_a_gap_and_a_recorded_verdict_is_a_fact() {
    let session = SessionId::new();
    let dir = project(&[session]);
    let store = Store::open(dir.path());
    let log = Log {
        store: &store,
        session,
    };
    log.step_started();
    log.call("c1", "write_file", json!({ "path": "a.txt" }));
    log.result("c1", "written", false);
    log.push(SessionEventKind::ToolVerified {
        id: "c1".to_string(),
        verdict: "verified".to_string(),
    });
    log.call("c2", "read_file", json!({ "path": "a.txt" }));
    log.result("c2", "contents", false);

    let run = capture(&dir);

    assert_eq!(
        labelled(&run, "verifier: `write_file` call `c1` verified").len(),
        1
    );
    assert!(run.gaps.contains(&FactGap::NoVerifierVerdict {
        session: session.to_string(),
        unverified: 1,
        calls: 2,
    }));
    assert!(
        labelled(&run, "`read_file` call `c2`")
            .iter()
            .all(|fact| !fact.label.contains("verif")),
        "an unverified call is not described as failing verification"
    );
}

#[test]
fn corrections_are_the_structured_signals_and_prose_is_never_one() {
    let session = SessionId::new();
    let dir = project(&[session]);
    let store = Store::open(dir.path());
    let log = Log {
        store: &store,
        session,
    };
    log.step_started();
    // A user-role message that reads like a correction. Harness user turns are
    // prompts the harness wrote; none of them becomes a fact.
    log.push(SessionEventKind::Message {
        message: Message::text(Role::User, "No, that's wrong — use the other config."),
        origin: MessageOrigin::UserInput,
    });
    log.push(SessionEventKind::DriverIntervention {
        action: "steer".to_string(),
        detail: "use config/dev.toml instead".to_string(),
        activity: Some("run_shell".to_string()),
        client: "agent-host".to_string(),
    });
    log.push(SessionEventKind::ToolInputRepaired {
        tool: "edit_file".to_string(),
        provider: "p".to_string(),
        model: "m".to_string(),
        class: "bare_string_for_array".to_string(),
        rules: vec!["wrap_bare_string_as_array".to_string()],
    });
    log.push(SessionEventKind::BranchClosed {
        summary: StructuredSummary::new(
            "Step attempt abandoned:",
            vec!["lint failed: unused import".to_string()],
        ),
    });
    log.turn_ended("no_progress");
    log.push(SessionEventKind::Cancelled);

    let run = capture(&dir);

    let driver = labelled(&run, "driver `agent-host` intervened: steer");
    assert_eq!(driver.len(), 1);
    assert_eq!(driver[0].kind, EvidenceKind::UserCorrection);
    assert!(driver[0]
        .excerpt
        .as_deref()
        .unwrap()
        .contains("config/dev.toml"));
    assert_eq!(
        labelled(&run, "`edit_file` arguments repaired before dispatch")[0].kind,
        EvidenceKind::RecoveryEvent
    );
    assert_eq!(
        labelled(&run, "attempt abandoned: Step attempt abandoned:")[0]
            .excerpt
            .as_deref(),
        Some("lint failed: unused import")
    );
    assert_eq!(labelled(&run, "a turn stopped: no_progress").len(), 1);
    assert_eq!(labelled(&run, "the run was cancelled").len(), 1);
    assert!(
        labelled(&run, "that's wrong").is_empty() && labelled(&run, "No,").is_empty(),
        "prose is not a correction"
    );
    assert_eq!(
        run.facts
            .iter()
            .filter(|fact| fact.kind == EvidenceKind::UserCorrection)
            .count(),
        1,
        "only the driver's structured intervention is a correction"
    );
}

#[test]
fn a_step_that_cannot_be_read_back_is_a_gap_never_a_guess() {
    let valid = SessionId::new();
    let dir = project(&[valid]);
    // Replace the link with one invalid id, and log a session that never
    // records starting the step it is linked to.
    let progress = std::fs::read_to_string(dir.path().join("PROGRESS.md")).unwrap();
    std::fs::write(
        dir.path().join("PROGRESS.md"),
        progress.replace(
            &format!("sessions: {valid}"),
            &format!("sessions: not-a-session, {valid}"),
        ),
    )
    .unwrap();
    let store = Store::open(dir.path());
    Log {
        store: &store,
        session: valid,
    }
    .turn_ended("done");

    let run = capture(&dir);

    assert!(run.gaps.contains(&FactGap::SessionIdInvalid {
        step: 1,
        value: "not-a-session".to_string(),
    }));
    assert!(run.gaps.contains(&FactGap::SessionWithoutStep {
        step: 1,
        session: valid.to_string(),
    }));

    // A step completed before the link existed.
    let unlinked = project(&[]);
    assert!(capture(&unlinked)
        .gaps
        .contains(&FactGap::StepNotLinked { step: 1 }));

    // A linked session with no log at all.
    let absent = SessionId::new();
    let empty = project(&[absent]);
    assert!(capture(&empty).gaps.contains(&FactGap::SessionEmpty {
        step: 1,
        session: absent.to_string(),
    }));
}

#[test]
fn an_oversized_run_keeps_its_frame_and_failures_and_says_what_it_dropped() {
    let session = SessionId::new();
    let dir = project(&[session]);
    let store = Store::open(dir.path());
    let log = Log {
        store: &store,
        session,
    };
    log.step_started();
    for index in 0..MAX_RUN_FACTS {
        let id = format!("ok{index}");
        log.call(&id, "read_file", json!({ "path": format!("{index}.txt") }));
        log.result(&id, "contents", false);
    }
    log.call("bad", "run_shell", json!({ "command": "cargo test" }));
    log.result("bad", "1 test failed", true);

    let run = capture(&dir);

    assert_eq!(run.facts.len(), MAX_RUN_FACTS);
    let dropped = run
        .gaps
        .iter()
        .find_map(|gap| match gap {
            FactGap::Truncated { dropped } => Some(*dropped),
            _ => None,
        })
        .expect("the drop is stated");
    assert!(dropped > 0);
    assert_eq!(labelled(&run, "task:").len(), 1, "the frame survives");
    assert_eq!(labelled(&run, "`run_shell` call `bad` failed").len(), 1);
    // What survives keeps capture order: the frame comes first, and the kept
    // successful calls are the earliest ones.
    assert!(run.facts[0].label.starts_with("task:"));
    assert_eq!(labelled(&run, "`read_file` call `ok0`").len(), 1);
}

#[test]
fn unreadable_documents_are_gaps_and_capture_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("brief.md"), "not a brief").unwrap();
    let before = listing(dir.path());

    let run = capture_run_facts(dir.path(), &Store::open(dir.path()));

    assert!(run.facts.is_empty());
    assert_eq!(
        run.gaps,
        vec![FactGap::BriefUnreadable, FactGap::ProgressUnreadable]
    );
    assert_eq!(listing(dir.path()), before, "capture is read-only");
    assert!(run.render_gaps().unwrap().starts_with("Not recorded"));
}

fn listing(root: &Path) -> Vec<String> {
    let mut entries: Vec<String> = walk(root)
        .into_iter()
        .map(|path| path.strip_prefix(root).unwrap().display().to_string())
        .collect();
    entries.sort();
    entries
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        }
        out.push(path);
    }
    out
}
