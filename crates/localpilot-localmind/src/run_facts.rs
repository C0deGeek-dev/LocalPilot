//! Facts captured from a completed harness run, before any model reads them.
//!
//! The completion retrospective used to see only `brief.md` and `PROGRESS.md`,
//! while what actually happened — tool failures, verifier verdicts, abandoned
//! attempts, a driver steering the session — sat in the event log of each step's
//! session. [`capture_run_facts`] reads both into one bounded, redacted set of
//! [`EvidenceRef`]s, each with a canonical id and a locator back to where it was
//! read, so later analysis can cite what was observed and nothing else.
//!
//! Three rules shape it:
//!
//! - **A fact is something recorded.** Absence is never a fact. A call with no
//!   result, a step with no linked session, a session with no verifier verdict:
//!   each is a [`FactGap`], which is not an `EvidenceRef` and cannot be cited, so
//!   "not recorded" can never be read as "did not happen" or "failed".
//! - **Redaction comes first.** Labels and excerpts pass the LocalMind redactor
//!   before they are hashed, bounded, or returned, so no model and no store ever
//!   sees the unredacted text through this path.
//! - **Corrections are structured signals only**: a driver intervention, a
//!   cancellation, an abandoned attempt, a tool-input repair or refusal, a
//!   permission decision, a turn that stopped short. Prose is never classified
//!   as a correction here. A harness step's user-role messages are prompts the
//!   harness wrote, so they are not captured at all.
//!
//! Capture is read-only: it writes nothing, creates no LocalMind config, and
//! touches no memory.

use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;

use localmind_core::{EvidenceKind, EvidenceRef, Observation};
use localmind_store::{ProjectConfig, Redactor};
use localpilot_core::{SessionId, ToolOutcome};
use localpilot_harness::{Brief, BriefRevision, CallOutcome, EvidenceLedger, Progress};
use localpilot_store::{SessionEvent, SessionEventKind, Store};
use sha2::{Digest, Sha256};

/// Most facts one run yields. Past this, the least informative are dropped —
/// successful calls first, then verdicts — and the drop is recorded as a gap.
pub const MAX_RUN_FACTS: usize = 40;

/// Most acceptance criteria captured as individual facts.
pub const MAX_ACCEPTANCE_FACTS: usize = 8;

/// Metadata key naming the ratified check a fact records a run of. The fact's
/// signature pairs the name with the command digest, so a changed command is a
/// different attempt.
pub const RATIFIED_CHECK_KEY: &str = "ratified_check";

/// Character ceiling on a fact's label. The label names the fact; what was
/// observed rides in the bounded excerpt.
pub const MAX_LABEL_CHARS: usize = 160;

/// A fact-set property worth knowing that is not itself an observation.
///
/// Not citable by construction: nothing here is an `EvidenceRef`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FactGap {
    /// `brief.md` could not be read or parsed, so the run's intent is unknown.
    BriefUnreadable,
    /// `PROGRESS.md` could not be read or parsed, so the steps are unknown.
    ProgressUnreadable,
    /// A completed step records no session — it predates the link, or the link
    /// was removed — so what happened during it cannot be read back.
    StepNotLinked { step: usize },
    /// A step's `sessions:` line names something that is not a session id.
    SessionIdInvalid { step: usize, value: String },
    /// A linked session's log could not be read at all.
    SessionUnreadable {
        step: usize,
        session: String,
        reason: String,
    },
    /// A linked session has no event log, or an empty one.
    SessionEmpty { step: usize, session: String },
    /// Some lines of a linked session's log were damaged and skipped.
    SessionPartlyUnreadable {
        step: usize,
        session: String,
        skipped_lines: usize,
    },
    /// A linked session never records starting the step it is linked to.
    SessionWithoutStep { step: usize, session: String },
    /// A call's result was never recorded. `pending` when the log ends while
    /// the call's turn is still open, rather than carrying on past it.
    ResultNotRecorded {
        session: String,
        call: String,
        tool: String,
        pending: bool,
    },
    /// The log records two disagreeing results for one call; the fact carries
    /// the first, and its outcome is not settled.
    ConflictingResult {
        session: String,
        call: String,
        tool: String,
    },
    /// Calls in a session that carry no verifier verdict.
    NoVerifierVerdict {
        session: String,
        unverified: usize,
        calls: usize,
    },
    /// Facts dropped to stay within [`MAX_RUN_FACTS`].
    Truncated { dropped: usize },
}

impl FactGap {
    /// Whether, and where, this gap leaves the record incomplete. A lesson may
    /// not rest on a record with pieces missing where it looks; a call no
    /// verifier examined, or a fact dropped to stay within the bound, leaves the
    /// record whole.
    #[must_use]
    pub fn incompleteness(&self) -> Option<localmind_store::Incompleteness> {
        use localmind_store::Incompleteness;
        let session = |session: &str| {
            Some(Incompleteness::Source(format!(
                "localpilot-session:{session}"
            )))
        };
        match self {
            Self::BriefUnreadable
            | Self::ProgressUnreadable
            | Self::StepNotLinked { .. }
            | Self::SessionIdInvalid { .. } => Some(Incompleteness::Run),
            Self::SessionUnreadable { session: id, .. }
            | Self::SessionEmpty { session: id, .. }
            | Self::SessionPartlyUnreadable { session: id, .. }
            | Self::SessionWithoutStep { session: id, .. }
            | Self::ResultNotRecorded { session: id, .. }
            | Self::ConflictingResult { session: id, .. } => session(id),
            Self::NoVerifierVerdict { .. } | Self::Truncated { .. } => None,
        }
    }

    /// One line a reviewer, or a drafting model, can read.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::BriefUnreadable => "brief.md was unreadable: the run's intent is unknown".into(),
            Self::ProgressUnreadable => {
                "PROGRESS.md was unreadable: the run's steps are unknown".into()
            }
            Self::StepNotLinked { step } => {
                format!("step {step} records no session: what happened during it is unknown")
            }
            Self::SessionIdInvalid { step, value } => {
                format!("step {step} names `{value}`, which is not a session id")
            }
            Self::SessionUnreadable {
                step,
                session,
                reason,
            } => format!("session {session} (step {step}) could not be read: {reason}"),
            Self::SessionEmpty { step, session } => {
                format!("session {session} (step {step}) has no recorded events")
            }
            Self::SessionPartlyUnreadable {
                step,
                session,
                skipped_lines,
            } => format!(
                "session {session} (step {step}): {skipped_lines} damaged log line(s) skipped"
            ),
            Self::SessionWithoutStep { step, session } => {
                format!("session {session} is linked to step {step} but never records starting it")
            }
            Self::ResultNotRecorded {
                session,
                call,
                tool,
                pending,
            } => {
                let why = if *pending {
                    "the log ends before it"
                } else {
                    "the log moves past it without one"
                };
                format!("`{tool}` call `{call}` in session {session}: no result recorded ({why})")
            }
            Self::ConflictingResult {
                session,
                call,
                tool,
            } => format!(
                "`{tool}` call `{call}` in session {session}: the log records disagreeing results"
            ),
            Self::NoVerifierVerdict {
                session,
                unverified,
                calls,
            } => format!(
                "session {session}: {unverified} of {calls} call(s) carry no verifier verdict"
            ),
            Self::Truncated { dropped } => {
                format!("{dropped} lower-priority fact(s) dropped to stay within {MAX_RUN_FACTS}")
            }
        }
    }
}

/// The facts of one completed run and what they cannot say.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunFacts {
    /// Redacted, bounded, deduplicated, in capture order.
    pub facts: Vec<EvidenceRef>,
    /// What is unknown about the run. Never citable.
    pub gaps: Vec<FactGap>,
}

impl RunFacts {
    /// The gaps as a short list for a reviewer, or `None` when there are none.
    /// Worded as what is *not* known, so it cannot pass for an observation.
    #[must_use]
    pub fn render_gaps(&self) -> Option<String> {
        if self.gaps.is_empty() {
            return None;
        }
        let mut out = String::from("Not recorded (these are gaps, not observations):");
        for gap in &self.gaps {
            out.push_str("\n- ");
            out.push_str(&gap.describe());
        }
        Some(out)
    }
}

/// Read the run rooted at `root` — its brief, its plan, and the event log of
/// every session its steps link to — into [`RunFacts`].
///
/// Never fails: an unreadable input becomes a gap, so a finished run is never
/// broken by capture.
#[must_use]
pub fn capture_run_facts(root: &Path, store: &Store) -> RunFacts {
    let mut capture = Capture::new(root);

    let brief = std::fs::read_to_string(root.join("brief.md"))
        .ok()
        .and_then(|text| Brief::parse(&text).ok());
    let progress = std::fs::read_to_string(root.join("PROGRESS.md"))
        .ok()
        .and_then(|text| Progress::parse(&text).ok());

    let run = match (&brief, &progress) {
        (Some(brief), Some(progress)) => {
            format!("harness-run:{}@{}", progress.name, BriefRevision::of(brief))
        }
        (None, Some(progress)) => format!("harness-run:{}", progress.name),
        (Some(brief), None) => format!("harness-run:{}", brief.name),
        (None, None) => "harness-run".to_string(),
    };

    match &brief {
        Some(brief) => capture.intent(&run, brief),
        None => capture.gaps.push(FactGap::BriefUnreadable),
    }
    match &progress {
        Some(progress) => {
            capture.plan(&run, progress);
            for step in progress.steps.iter().filter(|step| step.done) {
                if step.sessions.is_empty() {
                    capture
                        .gaps
                        .push(FactGap::StepNotLinked { step: step.number });
                }
                for value in &step.sessions {
                    capture.session(store, step.number, value);
                }
            }
        }
        None => capture.gaps.push(FactGap::ProgressUnreadable),
    }

    capture.finish()
}

/// Priority when the run has more facts than fit. Lower is kept first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    /// Intent, plan steps, commits, final state.
    Frame,
    /// Failures, corrections, recoveries, turns that stopped short.
    Signal,
    /// Verifier verdicts.
    Verdict,
    /// Successful calls and calls with no recorded result.
    Routine,
}

struct Capture {
    redactor: Redactor,
    facts: Vec<(Tier, EvidenceRef)>,
    gaps: Vec<FactGap>,
}

impl Capture {
    fn new(root: &Path) -> Self {
        // The project's configured sensitive paths, when it has a LocalMind
        // config. Discovery only reads; a project without one redacts with the
        // built-in patterns alone.
        let excluded = ProjectConfig::discover(root)
            .map(|config| config.config.learning.excluded_paths)
            .unwrap_or_default();
        Self {
            redactor: Redactor::new(excluded),
            facts: Vec::new(),
            gaps: Vec::new(),
        }
    }

    fn redact(&self, text: &str) -> String {
        self.redactor.redact(text).redacted_text
    }

    /// Add one fact. `content` is what the fact observed — hashed after
    /// redaction into the fact's identity — and `excerpt`, when given, is shown.
    #[allow(clippy::too_many_arguments)] // one call site shape, all parts named
    fn fact(
        &mut self,
        tier: Tier,
        kind: EvidenceKind,
        label: &str,
        source: &str,
        locator: String,
        content: &str,
        excerpt: Option<&str>,
    ) {
        let label = bound_label(&self.redact(label));
        let content = self.redact(content);
        let mut fact =
            EvidenceRef::identified(kind, label, source, locator, sha256(&content)).redacted();
        if let Some(excerpt) = excerpt {
            fact = fact.with_excerpt(self.redact(excerpt));
        }
        self.facts.push((tier, fact));
    }

    /// Say what the fact just added observed, and what it repeats, so the
    /// abstention check can tell a one-off from a pattern without reading
    /// prose.
    fn mark_last(&mut self, observation: Observation, signature: Option<&str>) {
        if let Some((tier, fact)) = self.facts.pop() {
            let mut fact = fact.with_observation(observation);
            if let Some(signature) = signature {
                fact = fact.with_signature(signature);
            }
            self.facts.push((tier, fact));
        }
    }

    fn intent(&mut self, run: &str, brief: &Brief) {
        self.fact(
            Tier::Frame,
            EvidenceKind::Other("task_intent".to_string()),
            &format!("task: {}", first_line(&brief.summary)),
            run,
            "brief.md#summary".to_string(),
            &brief.summary,
            Some(&brief.summary),
        );
        for (index, criterion) in brief
            .acceptance_criteria
            .iter()
            .take(MAX_ACCEPTANCE_FACTS)
            .enumerate()
        {
            self.fact(
                Tier::Frame,
                EvidenceKind::Other("task_intent".to_string()),
                &format!("acceptance criterion {}: {criterion}", index + 1),
                run,
                format!("brief.md#acceptance:{}", index + 1),
                criterion,
                None,
            );
        }
    }

    fn plan(&mut self, run: &str, progress: &Progress) {
        for step in &progress.steps {
            let state = if step.done { "done" } else { "not done" };
            self.fact(
                Tier::Frame,
                EvidenceKind::Other("plan_step".to_string()),
                &format!("step {} ({state}): {}", step.number, step.description),
                run,
                format!("PROGRESS.md#step:{}", step.number),
                &format!("{}\n{state}\n{}", step.number, step.description),
                None,
            );
            if let Some(commit) = &step.commit {
                self.fact(
                    Tier::Frame,
                    EvidenceKind::Commit,
                    &format!("step {} committed as {commit}", step.number),
                    run,
                    format!("git:{commit}"),
                    &format!("{}\n{commit}", step.number),
                    None,
                );
            }
        }
        let done = progress.completed_count();
        let total = progress.steps.len();
        self.fact(
            Tier::Frame,
            EvidenceKind::Other("final_state".to_string()),
            &format!("{done} of {total} plan step(s) complete"),
            run,
            "PROGRESS.md#steps".to_string(),
            &progress.render(),
            None,
        );
    }

    fn session(&mut self, store: &Store, step: usize, value: &str) {
        let Ok(session) = SessionId::from_str(value) else {
            self.gaps.push(FactGap::SessionIdInvalid {
                step,
                value: value.to_string(),
            });
            return;
        };
        let id = session.to_string();
        let recovered = match store.read_events_recovering(session) {
            Ok(recovered) => recovered,
            Err(error) => {
                self.gaps.push(FactGap::SessionUnreadable {
                    step,
                    session: id,
                    reason: error.to_string(),
                });
                return;
            }
        };
        if recovered.skipped_lines > 0 {
            self.gaps.push(FactGap::SessionPartlyUnreadable {
                step,
                session: id.clone(),
                skipped_lines: recovered.skipped_lines,
            });
        }
        let events = recovered.events;
        if events.is_empty() {
            self.gaps.push(FactGap::SessionEmpty { step, session: id });
            return;
        }
        let starts_step = events.iter().any(|event| {
            matches!(event.kind, SessionEventKind::StepStarted { number, .. } if number == step)
        });
        if !starts_step {
            self.gaps.push(FactGap::SessionWithoutStep {
                step,
                session: id.clone(),
            });
        }

        let source = format!("localpilot-session:{id}");
        self.calls(&source, &id, &events);
        for event in &events {
            self.signal(&source, &id, event);
        }
    }

    fn calls(&mut self, source: &str, session: &str, events: &[SessionEvent]) {
        let ledger = EvidenceLedger::project(events);
        let calls = ledger.calls();
        for call in calls {
            let locator = format!("{source}#event:{}", call.invoked_event);
            let input = serde_json::to_string(&call.input).unwrap_or_default();
            // The same tool with the same (redacted) arguments is the same
            // attempt, whichever call id the model gave it.
            let digest = sha256(&self.redact(&input));
            let digest = digest.strip_prefix("sha256:").unwrap_or(&digest);
            let signature = format!("{}:{}", call.name, digest.get(..16).unwrap_or(digest));
            let content = format!(
                "{}\n{}\n{:?}\n{:?}\n{input}\n{}",
                call.name,
                call.id,
                call.outcome,
                call.refinement,
                call.output()
            );
            let conflict = if call.conflicting_result {
                self.gaps.push(FactGap::ConflictingResult {
                    session: session.to_string(),
                    call: call.id.clone(),
                    tool: call.name.clone(),
                });
                "; a later result disagreed"
            } else {
                ""
            };
            match call.outcome {
                CallOutcome::Ok => {
                    self.fact(
                        Tier::Routine,
                        EvidenceKind::ToolEvent,
                        &format!("`{}` call `{}` succeeded{conflict}", call.name, call.id),
                        source,
                        locator,
                        &content,
                        None,
                    );
                    self.mark_last(Observation::Success, Some(&signature));
                }
                CallOutcome::Error => {
                    let how = match call.refinement {
                        Some(ToolOutcome::ReportedFailure) => " (it ran and reported failure)",
                        Some(ToolOutcome::Unusable) => " (it could not run)",
                        Some(ToolOutcome::Ok) | None => "",
                    };
                    self.fact(
                        Tier::Signal,
                        EvidenceKind::ToolEvent,
                        &format!("`{}` call `{}` failed{how}{conflict}", call.name, call.id),
                        source,
                        locator,
                        &content,
                        Some(&format!("input: {input}\noutput: {}", call.output())),
                    );
                    self.mark_last(Observation::Failure, Some(&signature));
                }
                CallOutcome::Pending | CallOutcome::Missing => {
                    self.gaps.push(FactGap::ResultNotRecorded {
                        session: session.to_string(),
                        call: call.id.clone(),
                        tool: call.name.clone(),
                        pending: call.outcome == CallOutcome::Pending,
                    });
                    self.fact(
                        Tier::Routine,
                        EvidenceKind::ToolEvent,
                        &format!("`{}` call `{}` was invoked", call.name, call.id),
                        source,
                        locator,
                        &format!("{}\n{}\n{input}", call.name, call.id),
                        None,
                    );
                }
            }
            if let Some(verdict) = &call.verdict {
                self.fact(
                    Tier::Verdict,
                    EvidenceKind::Other("verifier_verdict".to_string()),
                    &format!("verifier: `{}` call `{}` {verdict}", call.name, call.id),
                    source,
                    format!("{source}#call:{}/verdict", call.id),
                    &format!("{}\n{verdict}", call.id),
                    None,
                );
            }
        }
        let unverified = calls.iter().filter(|call| call.verdict.is_none()).count();
        if unverified > 0 {
            self.gaps.push(FactGap::NoVerifierVerdict {
                session: session.to_string(),
                unverified,
                calls: calls.len(),
            });
        }
    }

    /// A structured correction, recovery, early stop, or ratified check run —
    /// or nothing.
    fn signal(&mut self, source: &str, session: &str, event: &SessionEvent) {
        let locator = format!("{source}#event:{}", event.id);
        if let SessionEventKind::CheckRan {
            name,
            cadence,
            command_digest,
            status,
            detail,
        } = &event.kind
        {
            let failed = status != "passed";
            let content = format!(
                "{session}
{name}
{command_digest}
{status}
{detail}"
            );
            self.fact(
                Tier::Signal,
                EvidenceKind::TestOutput,
                &format!("ratified check `{name}` {status} ({cadence})"),
                source,
                locator,
                &content,
                (failed && !detail.is_empty()).then_some(detail.as_str()),
            );
            // Only a pass or a real failure says anything about the code; a
            // check that could not run says nothing either way.
            let observation = match status.as_str() {
                "passed" => Some(Observation::Success),
                "failed" => Some(Observation::Failure),
                _ => None,
            };
            if let Some(observation) = observation {
                self.mark_last(observation, Some(&format!("check:{name}:{command_digest}")));
            }
            if let Some((_, fact)) = self.facts.last_mut() {
                fact.metadata
                    .insert(RATIFIED_CHECK_KEY.to_string(), name.clone());
            }
            return;
        }
        let recovery = || EvidenceKind::RecoveryEvent;
        let (kind, label, excerpt): (EvidenceKind, String, Option<String>) = match &event.kind {
            SessionEventKind::DriverIntervention {
                action,
                detail,
                activity,
                client,
            } => (
                EvidenceKind::UserCorrection,
                format!("driver `{client}` intervened: {action}"),
                Some(match activity {
                    Some(activity) => format!("{detail}\nwhile: {activity}"),
                    None => detail.clone(),
                }),
            ),
            SessionEventKind::Cancelled => (
                EvidenceKind::Other("cancellation".to_string()),
                "the run was cancelled".to_string(),
                None,
            ),
            SessionEventKind::BranchClosed { summary } => (
                recovery(),
                format!("attempt abandoned: {}", summary.title),
                Some(summary.entries.join("\n")),
            ),
            SessionEventKind::ToolInputInvalid {
                tool,
                class,
                issue_paths,
                ..
            } => (
                recovery(),
                format!("`{tool}` arguments rejected before dispatch: {class}"),
                Some(format!("fields: {}", issue_paths.join(", "))),
            ),
            SessionEventKind::ToolInputRepaired {
                tool, class, rules, ..
            } => (
                recovery(),
                format!("`{tool}` arguments repaired before dispatch: {class}"),
                Some(format!("rules: {}", rules.join(", "))),
            ),
            SessionEventKind::ToolRepairRejectedHighRisk { tool, risk, .. } => (
                recovery(),
                format!("`{tool}` argument repair refused: {risk}"),
                None,
            ),
            SessionEventKind::PermissionDecided {
                tool,
                decision,
                detail,
            } => (
                EvidenceKind::Other("permission_decision".to_string()),
                format!("permission for `{tool}`: {decision}"),
                (!detail.is_empty()).then(|| detail.clone()),
            ),
            SessionEventKind::TurnEnded { stop, detail } if !stop.eq_ignore_ascii_case("done") => (
                EvidenceKind::Other("turn_stop".to_string()),
                format!("a turn stopped: {stop}"),
                detail.clone(),
            ),
            _ => return,
        };
        let content = format!("{session}\n{label}\n{}", excerpt.as_deref().unwrap_or(""));
        // A driver steering or anyone stopping the run corrected it from
        // outside; a repair or a turn stopping short is the run itself.
        let corrected = matches!(
            event.kind,
            SessionEventKind::DriverIntervention { .. } | SessionEventKind::Cancelled
        );
        self.fact(
            Tier::Signal,
            kind,
            &label,
            source,
            locator,
            &content,
            excerpt.as_deref(),
        );
        if corrected {
            self.mark_last(Observation::Correction, None);
        }
    }

    /// Deduplicate by id, keep within [`MAX_RUN_FACTS`] by priority, and return
    /// the facts in capture order.
    fn finish(self) -> RunFacts {
        let mut seen = HashSet::new();
        let mut unique: Vec<(usize, Tier, EvidenceRef)> = Vec::new();
        for (tier, fact) in self.facts {
            if seen.insert(fact.id.clone()) {
                unique.push((unique.len(), tier, fact));
            }
        }
        let mut gaps = self.gaps;
        if unique.len() > MAX_RUN_FACTS {
            let dropped = unique.len() - MAX_RUN_FACTS;
            // Stable: within a tier, earlier facts are kept first.
            unique.sort_by_key(|(index, tier, _)| (*tier, *index));
            unique.truncate(MAX_RUN_FACTS);
            unique.sort_by_key(|(index, _, _)| *index);
            gaps.push(FactGap::Truncated { dropped });
        }
        RunFacts {
            facts: unique.into_iter().map(|(_, _, fact)| fact).collect(),
            gaps,
        }
    }
}

fn sha256(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut hex = String::with_capacity(7 + digest.len() * 2);
    hex.push_str("sha256:");
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

fn bound_label(label: &str) -> String {
    let label = label.trim();
    if label.chars().count() <= MAX_LABEL_CHARS {
        return label.to_string();
    }
    let mut cut: String = label.chars().take(MAX_LABEL_CHARS - 1).collect();
    cut.push('…');
    cut
}
