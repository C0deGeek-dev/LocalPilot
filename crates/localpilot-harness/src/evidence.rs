//! A read-only projection of a session event log into per-tool-call records.
//!
//! [`EvidenceLedger::project`] reads an event slice and derives, for each tool
//! call, the discipline-relevant facts a benchmark scores: which tool ran, the
//! arguments supplied, the recorded permission verdict, whether the call
//! succeeded, and whether a later assistant message grounded a claim in the
//! call. It is **compute-only** — it owns no storage, performs no IO, and writes
//! nothing, so it can measure the *current* loop without changing any behaviour.
//!
//! The event log records a tool's outcome in two places (see
//! `localpilot-store`): a `ToolFinished` event carries only `is_error`, while
//! the tool's *output text* arrives as a separate `Message` whose origin is a
//! tool result. The projection pairs both back to the originating call by id.
//!
//! A log can be partial, and can repeat itself. The projection keeps the first
//! occurrence of a call and of its result, marks a later result that disagrees
//! as a conflict rather than silently taking either, and tells a call whose
//! result simply has not arrived yet (`Pending`) from one the log moved past
//! without ever resulting (`Missing`).

use std::collections::HashMap;

use localpilot_core::{ContentBlock, EventId, Role, ToolOutcome};
use localpilot_store::{SessionEvent, SessionEventKind};

/// The recorded result status of a tool call within a log slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallOutcome {
    /// A tool result reported success.
    Ok,
    /// A tool result reported an error.
    Error,
    /// No result appears, and the slice ends while the call's turn is still
    /// open — the result may simply not have been written yet.
    Pending,
    /// No result appears, although the slice carries on past the call: its
    /// turn ended, the session closed, or the run was cancelled. The outcome is
    /// unknown, and is not a failure.
    Missing,
}

/// The permission decision recorded for a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionVerdict {
    /// A permission decision was logged, carrying its decision label.
    Decided(String),
    /// No permission decision was logged for the call (the permission hook may
    /// not route every decision through the event log yet).
    Unrecorded,
}

/// One tool call projected from the event log, with the facts the discipline
/// benchmark scores. Produced only by [`EvidenceLedger::project`].
#[derive(Debug, Clone)]
pub struct CallRecord {
    /// Tool-call correlation id.
    pub id: String,
    /// Tool name as the model invoked it.
    pub name: String,
    /// The raw arguments the model supplied. Kept so a caller that knows the
    /// tool's schema can validate them; the projection itself does not.
    pub input: serde_json::Value,
    /// Whether the arguments validated against the tool schema, when a caller
    /// has filled this in. `None` while unknown to the projection.
    pub schema_valid: Option<bool>,
    /// The permission decision recorded for the call.
    pub permission: PermissionVerdict,
    /// The recorded result status.
    pub outcome: CallOutcome,
    /// Whether a later assistant message grounded a claim in this call (it named
    /// the tool or echoed a distinctive token from the call's output).
    pub claim_referenced: bool,
    /// The verifier's verdict for this call, if one was recorded (`"verified"`,
    /// `"unverified"`, `"failed"`). `None` until a verifier runs.
    pub verdict: Option<String>,
    /// The finer outcome a `ToolFinished` event recorded — whether a failing
    /// tool could not run at all or ran and reported failure. `None` when the
    /// log predates the distinction or carries no `ToolFinished` for the call.
    pub refinement: Option<ToolOutcome>,
    /// The event that invoked the call, so a reader can find it in the log.
    pub invoked_event: EventId,
    /// The event that first delivered the call's result, when one did.
    pub result_event: Option<EventId>,
    /// A later result for the same call disagreed with the first. The log
    /// contradicts itself about this call, so its outcome is not settled.
    pub conflicting_result: bool,
    /// Event ordinal at which the call was invoked, used to order later claims.
    invoked_at: usize,
    /// The tool's output text, retained only to detect grounded claims.
    output: String,
}

/// A read-only projection over a session event log.
#[derive(Debug, Clone, Default)]
pub struct EvidenceLedger {
    records: Vec<CallRecord>,
}

impl EvidenceLedger {
    /// Project an ordered session event slice into per-call records.
    ///
    /// The slice is read in order; nothing is written or persisted.
    #[must_use]
    pub fn project(events: &[SessionEvent]) -> Self {
        let mut records: Vec<CallRecord> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        // Assistant text blocks, as (event ordinal, lowercased text), used after
        // the walk to decide which calls a later claim grounded itself in.
        let mut claims: Vec<(usize, String)> = Vec::new();
        // Ordinals at which the log moved past any call still open: a turn
        // ended, the session closed, or the run was cancelled.
        let mut closings: Vec<usize> = Vec::new();

        for (ordinal, event) in events.iter().enumerate() {
            match &event.kind {
                SessionEventKind::Message { message, .. } => {
                    Self::ingest_message(
                        message,
                        event.id,
                        ordinal,
                        &mut records,
                        &mut index,
                        &mut claims,
                    );
                }
                SessionEventKind::ToolFinished {
                    id,
                    is_error,
                    outcome,
                    ..
                } => {
                    if let Some(&pos) = index.get(id.as_str()) {
                        let record = &mut records[pos];
                        settle(record, outcome_of(*is_error), event.id);
                        if record.refinement.is_none() {
                            record.refinement = *outcome;
                        }
                    }
                }
                SessionEventKind::PermissionDecided { tool, decision, .. } => {
                    // The event names a tool, not a call. A decision precedes its
                    // call's result, so it belongs to the earliest call of that
                    // tool still awaiting both — which is right for a batch of
                    // same-tool calls as well as for one call at a time.
                    if let Some(record) = records.iter_mut().find(|r| {
                        r.name == *tool
                            && r.permission == PermissionVerdict::Unrecorded
                            && r.outcome == CallOutcome::Pending
                    }) {
                        record.permission = PermissionVerdict::Decided(decision.clone());
                    }
                }
                SessionEventKind::TurnEnded { .. }
                | SessionEventKind::SessionClosed
                | SessionEventKind::Cancelled => closings.push(ordinal),
                SessionEventKind::ToolVerified { id, verdict } => {
                    if let Some(&pos) = index.get(id.as_str()) {
                        records[pos].verdict = Some(verdict.clone());
                    }
                }
                _ => {}
            }
        }

        for record in &mut records {
            if record.outcome == CallOutcome::Pending
                && closings.iter().any(|&closed| closed > record.invoked_at)
            {
                record.outcome = CallOutcome::Missing;
            }
        }

        mark_grounded_claims(&mut records, &claims);
        Self { records }
    }

    /// Fold one transcript message into the in-progress projection.
    fn ingest_message(
        message: &localpilot_core::Message,
        event: EventId,
        ordinal: usize,
        records: &mut Vec<CallRecord>,
        index: &mut HashMap<String, usize>,
        claims: &mut Vec<(usize, String)>,
    ) {
        for block in &message.content {
            match block {
                ContentBlock::ToolUse(call) if message.role == Role::Assistant => {
                    let id = call.id.as_str().to_string();
                    // A repeated invocation (a replayed or re-appended event) is
                    // the same call, not a second one.
                    if index.contains_key(&id) {
                        continue;
                    }
                    index.insert(id.clone(), records.len());
                    records.push(CallRecord {
                        id,
                        name: call.name.clone(),
                        input: call.input.clone(),
                        schema_valid: None,
                        permission: PermissionVerdict::Unrecorded,
                        outcome: CallOutcome::Pending,
                        claim_referenced: false,
                        verdict: None,
                        refinement: None,
                        invoked_event: event,
                        result_event: None,
                        conflicting_result: false,
                        invoked_at: ordinal,
                        output: String::new(),
                    });
                }
                ContentBlock::Text { text } if message.role == Role::Assistant => {
                    claims.push((ordinal, text.to_lowercase()));
                }
                ContentBlock::ToolResult(result) => {
                    if let Some(&pos) = index.get(result.id.as_str()) {
                        let record = &mut records[pos];
                        settle(record, outcome_of(result.is_error()), event);
                        // The first delivered output is the one the model saw.
                        if record.output.is_empty() {
                            record.output = result.output.clone();
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// The projected call records, in invocation order.
    #[must_use]
    pub fn calls(&self) -> &[CallRecord] {
        &self.records
    }

    /// Fill [`CallRecord::schema_valid`] for every projected call from a caller
    /// that holds the tool schemas. `validate` maps a call's `(name, input)` to
    /// `Some(true)`/`Some(false)` when the tool is known and its arguments can be
    /// checked, or `None` when the tool is unknown (an unavailable-tool trap, or
    /// an MCP tool with no typed schema). This is the production hook that lights
    /// up the dormant validity metric — the projection itself reads no schemas.
    pub fn fill_schema_validity(
        &mut self,
        validate: impl Fn(&str, &serde_json::Value) -> Option<bool>,
    ) {
        for record in &mut self.records {
            record.schema_valid = validate(&record.name, &record.input);
        }
    }

    /// Whether the named tool was invoked at least once.
    #[must_use]
    pub fn used(&self, tool: &str) -> bool {
        self.records.iter().any(|r| r.name == tool)
    }
}

impl CallRecord {
    /// The first output delivered for the call, empty when none was. Unbounded
    /// and unredacted: a caller that stores or shows it must bound and redact
    /// it first.
    #[must_use]
    pub fn output(&self) -> &str {
        &self.output
    }
}

/// Record `outcome` as the call's result if it has none yet; otherwise note a
/// disagreement rather than choosing between the two.
fn settle(record: &mut CallRecord, outcome: CallOutcome, event: EventId) {
    if record.outcome == CallOutcome::Pending {
        record.outcome = outcome;
        record.result_event = Some(event);
    } else if record.outcome != outcome {
        record.conflicting_result = true;
    }
}

/// Map an `is_error` flag to a result outcome.
fn outcome_of(is_error: bool) -> CallOutcome {
    if is_error {
        CallOutcome::Error
    } else {
        CallOutcome::Ok
    }
}

/// Mark each call whose call a later assistant claim grounded itself in: the
/// claim named the tool, or echoed a distinctive token from the call's output.
fn mark_grounded_claims(records: &mut [CallRecord], claims: &[(usize, String)]) {
    for record in records.iter_mut() {
        let name = record.name.to_lowercase();
        let tokens = distinctive_tokens(&record.output);
        record.claim_referenced = claims.iter().any(|(ordinal, text)| {
            *ordinal > record.invoked_at
                && (text.contains(&name) || tokens.iter().any(|token| text.contains(token)))
        });
    }
}

/// The minimum length an output token must reach to count as distinctive enough
/// that echoing it in a claim implies grounding rather than coincidence.
const DISTINCTIVE_TOKEN_LEN: usize = 6;

/// The distinctive lowercased alphanumeric tokens of `output` (length at least
/// [`DISTINCTIVE_TOKEN_LEN`]), used to detect a claim grounded in the output.
fn distinctive_tokens(output: &str) -> Vec<String> {
    output
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| token.len() >= DISTINCTIVE_TOKEN_LEN)
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_core::{ContentBlock, EventId, Message, Role, ToolCall, ToolResult};
    use localpilot_store::{
        MessageOrigin, SessionEvent, SessionEventKind, SESSION_EVENT_FORMAT_VERSION,
    };

    /// Build a minimal event envelope around a kind; ids/timestamps are inert
    /// for the projection, which reads only order and payload.
    fn event(kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            v: SESSION_EVENT_FORMAT_VERSION,
            id: EventId::new(),
            parent_id: None,
            at_unix: 0,
            kind,
        }
    }

    fn assistant(content: Vec<ContentBlock>) -> SessionEvent {
        event(SessionEventKind::Message {
            message: Message::new(Role::Assistant, content),
            origin: MessageOrigin::Assistant,
        })
    }

    fn tool_result(id: &str, output: &str, is_error: bool) -> SessionEvent {
        let result = if is_error {
            ToolResult::error(id.into(), output)
        } else {
            ToolResult::success(id.into(), output)
        };
        event(SessionEventKind::Message {
            message: Message::new(Role::Tool, vec![ContentBlock::ToolResult(result)]),
            origin: MessageOrigin::ToolResult,
        })
    }

    fn call_block(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
        ContentBlock::ToolUse(ToolCall::new(id.into(), name, input))
    }

    #[test]
    fn projects_call_outcome_permission_and_grounded_claim() {
        let events = vec![
            assistant(vec![call_block(
                "c1",
                "search",
                serde_json::json!({ "query": "normalize_path" }),
            )]),
            event(SessionEventKind::PermissionDecided {
                tool: "search".to_string(),
                decision: "allowed".to_string(),
                detail: String::new(),
            }),
            tool_result("c1", "match in src/pathing/normalize_path.rs", false),
            assistant(vec![ContentBlock::text(
                "The symbol lives in normalize_path.rs, found via search.",
            )]),
        ];

        let ledger = EvidenceLedger::project(&events);
        let calls = ledger.calls();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.name, "search");
        assert_eq!(call.outcome, CallOutcome::Ok);
        assert_eq!(
            call.permission,
            PermissionVerdict::Decided("allowed".to_string())
        );
        assert!(
            call.claim_referenced,
            "the final claim echoes a distinctive output token"
        );
        assert_eq!(call.schema_valid, None);
    }

    #[test]
    fn unreferenced_failed_call_is_not_grounded() {
        let events = vec![
            assistant(vec![call_block(
                "w1",
                "write_file",
                serde_json::json!({ "path": "out.txt" }),
            )]),
            tool_result("w1", "permission denied", true),
            assistant(vec![ContentBlock::text("All done — the file is saved.")]),
        ];

        let ledger = EvidenceLedger::project(&events);
        let call = &ledger.calls()[0];
        assert_eq!(call.outcome, CallOutcome::Error);
        assert!(
            !call.claim_referenced,
            "the wrap-up claim grounds itself in nothing the failed call produced"
        );
        assert_eq!(call.permission, PermissionVerdict::Unrecorded);
    }

    #[test]
    fn tool_finished_event_supplies_outcome_without_a_result_message() {
        let events = vec![
            assistant(vec![call_block("c1", "run_shell", serde_json::json!({}))]),
            event(SessionEventKind::ToolFinished {
                id: "c1".to_string(),
                name: "run_shell".to_string(),
                is_error: true,
                outcome: None,
            }),
        ];

        let ledger = EvidenceLedger::project(&events);
        assert_eq!(ledger.calls()[0].outcome, CallOutcome::Error);
    }

    #[test]
    fn projection_writes_nothing_to_the_filesystem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let before = std::fs::read_dir(dir.path()).expect("read tempdir").count();

        let events = vec![assistant(vec![call_block(
            "c1",
            "search",
            serde_json::json!({ "query": "x" }),
        )])];
        let _ = EvidenceLedger::project(&events);

        let after = std::fs::read_dir(dir.path()).expect("read tempdir").count();
        assert_eq!(before, 0);
        assert_eq!(after, 0, "a compute-only projection must not write");
    }

    #[test]
    fn fill_schema_validity_sets_each_call_from_the_validator() {
        let events = vec![
            assistant(vec![call_block(
                "c1",
                "read_file",
                serde_json::json!({ "path": "a.rs" }),
            )]),
            assistant(vec![call_block("c2", "read_file", serde_json::json!({}))]),
            assistant(vec![call_block("c3", "mystery", serde_json::json!({}))]),
        ];
        let mut ledger = EvidenceLedger::project(&events);
        // A valid call, a known-invalid call, and an unknown tool.
        ledger.fill_schema_validity(|name, input| match name {
            "read_file" => Some(input.get("path").is_some()),
            _ => None,
        });
        let calls = ledger.calls();
        assert_eq!(calls[0].schema_valid, Some(true));
        assert_eq!(calls[1].schema_valid, Some(false));
        assert_eq!(calls[2].schema_valid, None, "unknown tool stays None");
    }

    #[test]
    fn a_verdict_event_binds_to_its_call() {
        let events = vec![
            assistant(vec![call_block(
                "c1",
                "write_file",
                serde_json::json!({ "path": "out.txt" }),
            )]),
            event(SessionEventKind::ToolVerified {
                id: "c1".to_string(),
                verdict: "verified".to_string(),
            }),
        ];
        let ledger = EvidenceLedger::project(&events);
        assert_eq!(ledger.calls()[0].verdict.as_deref(), Some("verified"));
    }

    fn turn_ended() -> SessionEvent {
        event(SessionEventKind::TurnEnded {
            stop: "done".to_string(),
            detail: None,
        })
    }

    #[test]
    fn a_call_the_log_moved_past_without_a_result_is_missing_not_pending() {
        let open = vec![assistant(vec![call_block(
            "c1",
            "run_shell",
            serde_json::json!({}),
        )])];
        assert_eq!(
            EvidenceLedger::project(&open).calls()[0].outcome,
            CallOutcome::Pending,
            "the log ends mid-turn: the result may not have been written yet"
        );

        let mut closed = open;
        closed.push(turn_ended());
        assert_eq!(
            EvidenceLedger::project(&closed).calls()[0].outcome,
            CallOutcome::Missing,
            "the turn ended and no result ever arrived"
        );
    }

    #[test]
    fn a_repeated_invocation_and_result_is_one_call() {
        let call = assistant(vec![call_block(
            "c1",
            "search",
            serde_json::json!({ "q": "x" }),
        )]);
        let result = tool_result("c1", "found", false);
        let events = vec![call.clone(), result.clone(), call, result];

        let ledger = EvidenceLedger::project(&events);

        assert_eq!(ledger.calls().len(), 1);
        let record = &ledger.calls()[0];
        assert_eq!(record.outcome, CallOutcome::Ok);
        assert!(
            !record.conflicting_result,
            "a repeat that agrees is no conflict"
        );
        assert_eq!(record.invoked_event, events[0].id);
        assert_eq!(record.result_event, Some(events[1].id));
    }

    #[test]
    fn a_result_that_contradicts_the_first_is_flagged_not_chosen() {
        let events = vec![
            assistant(vec![call_block("c1", "write_file", serde_json::json!({}))]),
            tool_result("c1", "written", false),
            tool_result("c1", "disk full", true),
        ];

        let ledger = EvidenceLedger::project(&events);
        let record = &ledger.calls()[0];

        assert_eq!(record.outcome, CallOutcome::Ok, "the first result stands");
        assert_eq!(record.output(), "written");
        assert!(record.conflicting_result);
    }

    #[test]
    fn the_finished_event_refines_a_failure() {
        let events = vec![
            assistant(vec![call_block("c1", "run_shell", serde_json::json!({}))]),
            tool_result("c1", "exit status 1", true),
            event(SessionEventKind::ToolFinished {
                id: "c1".to_string(),
                name: "run_shell".to_string(),
                is_error: true,
                outcome: Some(ToolOutcome::ReportedFailure),
            }),
        ];

        let ledger = EvidenceLedger::project(&events);
        let record = &ledger.calls()[0];

        assert_eq!(record.outcome, CallOutcome::Error);
        assert_eq!(record.refinement, Some(ToolOutcome::ReportedFailure));
        assert!(!record.conflicting_result);
    }

    #[test]
    fn a_permission_decision_binds_to_the_earliest_waiting_call_of_its_tool() {
        let events = vec![
            assistant(vec![
                call_block("c1", "write_file", serde_json::json!({ "path": "a" })),
                call_block("c2", "write_file", serde_json::json!({ "path": "b" })),
            ]),
            event(SessionEventKind::PermissionDecided {
                tool: "write_file".to_string(),
                decision: "allowed".to_string(),
                detail: String::new(),
            }),
            tool_result("c1", "ok", false),
            event(SessionEventKind::PermissionDecided {
                tool: "write_file".to_string(),
                decision: "denied".to_string(),
                detail: String::new(),
            }),
            tool_result("c2", "permission denied for write_file", true),
        ];

        let calls = EvidenceLedger::project(&events).calls().to_vec();

        assert_eq!(
            calls[0].permission,
            PermissionVerdict::Decided("allowed".to_string())
        );
        assert_eq!(
            calls[1].permission,
            PermissionVerdict::Decided("denied".to_string())
        );
    }

    #[test]
    fn the_verdict_record_lives_in_the_execution_store_not_memory() {
        use localpilot_core::{SessionId, ToolCall};
        use localpilot_store::Store;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        let session = SessionId::new();
        let parent = store
            .append_event(
                session,
                None,
                SessionEventKind::Message {
                    message: Message::new(
                        Role::Assistant,
                        vec![ContentBlock::ToolUse(ToolCall::new(
                            "c1".into(),
                            "write_file",
                            serde_json::json!({ "path": "out.txt" }),
                        ))],
                    ),
                    origin: MessageOrigin::Assistant,
                },
            )
            .unwrap();
        store
            .append_event(
                session,
                Some(parent),
                SessionEventKind::ToolVerified {
                    id: "c1".to_string(),
                    verdict: "verified".to_string(),
                },
            )
            .unwrap();

        // The verdict round-trips out of the execution-record store and binds to
        // its call — and it lives under `.localpilot/`, not in LocalMind memory.
        let events = store.read_events(session).unwrap();
        let ledger = EvidenceLedger::project(&events);
        assert_eq!(ledger.calls()[0].verdict.as_deref(), Some("verified"));
        assert!(store.root().join("sessions").exists());
    }
}
