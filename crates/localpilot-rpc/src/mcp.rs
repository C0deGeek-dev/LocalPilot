//! MCP (Model Context Protocol) server adapter over the session runtime.
//!
//! Implements the server side of the published MCP specification (version
//! 2025-06-18): JSON-RPC 2.0 over LF-delimited stdio. An MCP client — an
//! agent host such as Claude Code or Codex — drives one LocalPilot session
//! through tools: `prompt` starts a turn (with `steer`/`follow_up`
//! dispositions mid-turn), `events` pages the session's event feed with a
//! bounded wait, `reply_permission` answers a pending ask, plus `cancel`,
//! `status`, and `transcript`.
//!
//! The permission engine stays authoritative, exactly as on the native and
//! ACP adapters: the client only answers asks it is shown, an unanswered ask
//! denies, and [`McpServeOptions::approvals`] can withhold the reply tool
//! entirely (watch-and-steer mode — every ask then denies).
//!
//! MCP is request/response, so the event stream is pull-based: each event
//! gets a monotonic sequence number in a bounded feed, and `events` returns
//! the page after a client-held cursor, optionally waiting (server-capped)
//! for the first new event. Overflow drops the oldest entries and reports
//! the count — a lagging client sees that it lagged, never a silent gap.
//!
//! Provenance: implemented from the published protocol documentation and
//! schema only; no other implementation was consulted.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use localpilot_harness::SessionRuntime;
use localpilot_store::{SessionEventKind, Store};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::approver::{AskRegistry, PendingAsk};
use crate::framing::JsonRecordReader;
use crate::protocol::{InputDisposition, ServerEvent};
use crate::serve::{map_event, next_incomplete_step, RpcError};

/// The MCP protocol revision this adapter implements.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Cap on buffered feed entries; older entries drop (and are counted) first.
const EVENT_FEED_CAP: usize = 4096;

/// Server-side cap on an `events` wait, kept well under common client
/// tool-call timeouts so a long poll can never trip one.
const MAX_EVENT_WAIT_MS: u64 = 20_000;

/// Default transcript-tail length when the tool call names none.
const DEFAULT_TRANSCRIPT_TAIL: usize = 20;

/// Static facts and posture for one serve call.
#[derive(Debug, Clone)]
pub struct McpServeOptions {
    /// The model the session runs.
    pub model: String,
    /// The active permission profile's display label.
    pub profile: String,
    /// The workspace root, for harness-step inspection.
    pub root: Option<PathBuf>,
    /// Expose the `reply_permission` tool. When false the client can watch
    /// and steer but never answer an ask — every ask denies, fail-closed.
    pub approvals: bool,
}

/// One session event with its feed sequence number.
struct FeedEntry {
    seq: u64,
    event: ServerEvent,
}

/// Bounded, monotonically numbered event feed.
struct EventFeed {
    entries: VecDeque<FeedEntry>,
    next_seq: u64,
    dropped: u64,
}

impl EventFeed {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            next_seq: 1,
            dropped: 0,
        }
    }

    fn push(&mut self, event: ServerEvent) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push_back(FeedEntry { seq, event });
        while self.entries.len() > EVENT_FEED_CAP {
            self.entries.pop_front();
            self.dropped += 1;
        }
    }

    /// Entries after `cursor`, serialized for the wire.
    fn page_after(&self, cursor: u64) -> Vec<Value> {
        self.entries
            .iter()
            .filter(|entry| entry.seq > cursor)
            .map(|entry| {
                json!({
                    "seq": entry.seq,
                    "event": serde_json::to_value(&entry.event).unwrap_or_else(|_| json!({})),
                })
            })
            .collect()
    }

    /// The highest sequence number handed out so far (the cursor a client
    /// should pass next).
    fn head(&self) -> u64 {
        self.next_seq - 1
    }
}

/// A parked `events` call waiting for the first new entry.
struct PendingPoll {
    id: Value,
    cursor: u64,
    deadline: tokio::time::Instant,
}

/// One driver intervention captured during a serve, in order: the client
/// steered the turn, cancelled it, or answered a permission ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverInterventionRecord {
    /// `steer`, `cancel`, `allow`, or `deny`.
    pub action: String,
    /// The steer text, or the ask's tool and detail.
    pub detail: String,
    /// What the session was doing at the time, when known.
    pub activity: Option<String>,
}

/// What one MCP serve observed: the driving client's self-reported identity
/// and every intervention it made, so the host can offer corrections to the
/// review-gated lesson queue with honest provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServeReport {
    /// The client's `clientInfo` name and version from `initialize`.
    pub client: String,
    /// Interventions in the order they happened.
    pub interventions: Vec<DriverInterventionRecord>,
}

/// Serve-loop state shared between the idle and in-turn phases.
struct McpState {
    feed: EventFeed,
    pending_poll: Option<PendingPoll>,
    follow_ups: VecDeque<String>,
    session_id: String,
    session: localpilot_core::SessionId,
    store: Store,
    /// The driving client's self-reported identity (from `initialize`).
    client: String,
    /// Interventions captured so far; `recorded` marks how many are already
    /// in the durable event log (written whenever the runtime is free).
    interventions: Vec<DriverInterventionRecord>,
    recorded: usize,
    /// The most recent activity marker from the event stream (running tool).
    last_activity: Option<String>,
    /// Pending-ask context so a permission reply can name what it answered.
    ask_context: HashMap<String, String>,
}

impl McpState {
    fn intervene(&mut self, action: &str, detail: String) {
        self.interventions.push(DriverInterventionRecord {
            action: action.to_string(),
            detail,
            activity: self.last_activity.clone(),
        });
    }
}

/// Write captured interventions to the durable event log. Callable only while
/// the runtime is not driving a turn; a failure is logged by the runtime, not
/// fatal (the log is an audit record, not a gate).
fn flush_interventions(runtime: &mut SessionRuntime, state: &mut McpState) {
    while state.recorded < state.interventions.len() {
        let record = &state.interventions[state.recorded];
        runtime.record_event(SessionEventKind::DriverIntervention {
            action: record.action.clone(),
            detail: record.detail.clone(),
            activity: record.activity.clone(),
            client: state.client.clone(),
        });
        state.recorded += 1;
    }
}

/// Serve one MCP client over `reader`/`writer` until end of input.
///
/// The runtime must have been built with the [`crate::RpcApprover`] whose
/// halves are passed here, so permission asks surface on the event feed and
/// `reply_permission` routes back to the engine.
///
/// # Errors
/// Returns [`RpcError`] on transport failure.
pub async fn serve_mcp<R, W>(
    runtime: &mut SessionRuntime,
    mut ask_rx: mpsc::UnboundedReceiver<PendingAsk>,
    asks: AskRegistry,
    reader: R,
    mut writer: W,
    options: &McpServeOptions,
) -> Result<McpServeReport, RpcError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = JsonRecordReader::new(reader);
    let mut state = McpState {
        feed: EventFeed::new(),
        pending_poll: None,
        follow_ups: VecDeque::new(),
        session_id: runtime.session_id().to_string(),
        session: runtime.session_id(),
        store: runtime.store().clone(),
        client: "mcp-client".to_string(),
        interventions: Vec::new(),
        recorded: 0,
        last_activity: None,
        ask_context: HashMap::new(),
    };

    loop {
        let message = tokio::select! {
            message = reader.next() => message?,
            Some(ask) = ask_rx.recv() => {
                push_event(&mut state, &mut writer, ask_event(&ask)).await?;
                continue;
            }
            () = poll_deadline(&state.pending_poll) => {
                expire_poll(&mut state, &mut writer).await?;
                continue;
            }
        };
        let Some(message) = message else { break };

        let method = message["method"].as_str().unwrap_or_default().to_string();
        let id = message.get("id").cloned();
        match method.as_str() {
            "initialize" => {
                state.client = client_label(&message["params"]["clientInfo"]);
                respond(&mut writer, id, initialize_result()).await?;
            }
            "ping" => respond(&mut writer, id, json!({})).await?,
            "tools/list" => {
                respond(&mut writer, id, json!({ "tools": tool_catalog(options) })).await?;
            }
            "tools/call" => {
                let name = message["params"]["name"].as_str().unwrap_or_default();
                let args = message["params"]["arguments"].clone();
                if name == "prompt" {
                    // Any disposition starts a turn when the session is idle,
                    // matching the native protocol.
                    let text = match prompt_text(&args) {
                        Ok(text) => text,
                        Err(error) => {
                            respond_error(&mut writer, id, -32602, &error).await?;
                            continue;
                        }
                    };
                    respond(&mut writer, id, tool_ok(json!({ "started": true }))).await?;
                    let mut next = Some(text);
                    while let Some(text) = next.take() {
                        let client_gone = drive_turn(
                            runtime,
                            &mut ask_rx,
                            &asks,
                            &mut reader,
                            &mut writer,
                            &mut state,
                            options,
                            &text,
                        )
                        .await?;
                        // The runtime is free between turns: persist any
                        // interventions the turn captured.
                        flush_interventions(runtime, &mut state);
                        if client_gone {
                            return Ok(report(state));
                        }
                        next = state.follow_ups.pop_front();
                    }
                } else {
                    dispatch_tool(
                        &mut writer,
                        &mut state,
                        &asks,
                        options,
                        id,
                        name,
                        &args,
                        false,
                    )
                    .await?;
                    // An idle-time intervention (e.g. answering a stray ask)
                    // can persist immediately.
                    flush_interventions(runtime, &mut state);
                }
            }
            // Notifications (initialized, cancelled, …) need no reply; a
            // response record has no method and nothing to route here.
            "" => {}
            other if other.starts_with("notifications/") => {}
            other => {
                if id.is_some() {
                    respond_error(
                        &mut writer,
                        id,
                        -32601,
                        &format!("method not found: {other}"),
                    )
                    .await?;
                }
            }
        }
    }
    Ok(report(state))
}

fn report(state: McpState) -> McpServeReport {
    McpServeReport {
        client: state.client,
        interventions: state.interventions,
    }
}

/// The client's self-reported name and version from `initialize`.
fn client_label(client_info: &Value) -> String {
    let name = client_info["name"].as_str().unwrap_or("mcp-client");
    match client_info["version"].as_str() {
        Some(version) if !version.is_empty() => format!("{name} {version}"),
        _ => name.to_string(),
    }
}

/// Drive one turn while staying responsive to JSON-RPC traffic. Returns
/// whether the client disconnected.
#[allow(clippy::too_many_arguments)] // the adapter loop genuinely threads these
async fn drive_turn<R, W>(
    runtime: &mut SessionRuntime,
    ask_rx: &mut mpsc::UnboundedReceiver<PendingAsk>,
    asks: &AskRegistry,
    reader: &mut JsonRecordReader<R>,
    writer: &mut W,
    state: &mut McpState,
    options: &McpServeOptions,
    text: &str,
) -> Result<bool, RpcError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let steer = runtime.steer_queue();
    let (events, mut rx) = broadcast::channel(1024);
    let cancel = CancellationToken::new();
    let mut client_gone = false;
    {
        let turn = runtime.run_turn(text, &events, &cancel);
        tokio::pin!(turn);
        loop {
            tokio::select! {
                _ = &mut turn => break,
                event = rx.recv() => {
                    if let Ok(event) = event {
                        push_event(state, writer, map_event(event)).await?;
                    }
                }
                Some(ask) = ask_rx.recv() => {
                    push_event(state, writer, ask_event(&ask)).await?;
                }
                () = poll_deadline(&state.pending_poll) => {
                    expire_poll(state, writer).await?;
                }
                message = reader.next() => match message? {
                    // Client gone: cancel; outstanding asks deny by timeout or
                    // registry drop — never silently approve.
                    None => {
                        cancel.cancel();
                        client_gone = true;
                    }
                    Some(message) => {
                        let method = message["method"].as_str().unwrap_or_default().to_string();
                        let id = message.get("id").cloned();
                        match method.as_str() {
                            "initialize" => {
                                state.client = client_label(&message["params"]["clientInfo"]);
                                respond(writer, id, initialize_result()).await?;
                            }
                            "ping" => respond(writer, id, json!({})).await?,
                            "tools/list" => {
                                respond(writer, id, json!({ "tools": tool_catalog(options) }))
                                    .await?;
                            }
                            "tools/call" => {
                                let name =
                                    message["params"]["name"].as_str().unwrap_or_default();
                                let args = message["params"]["arguments"].clone();
                                match name {
                                    "prompt" => {
                                        in_turn_prompt(writer, state, &steer, id, &args).await?;
                                    }
                                    "cancel" => {
                                        cancel.cancel();
                                        state.intervene("cancel", "turn cancelled".to_string());
                                        respond(
                                            writer,
                                            id,
                                            tool_ok(json!({ "cancelled": true })),
                                        )
                                        .await?;
                                    }
                                    _ => {
                                        dispatch_tool(
                                            writer, state, asks, options, id, name, &args, true,
                                        )
                                        .await?;
                                    }
                                }
                            }
                            "" => {}
                            other if other.starts_with("notifications/") => {}
                            other => {
                                if id.is_some() {
                                    respond_error(
                                        writer,
                                        id,
                                        -32601,
                                        &format!("method not found: {other}"),
                                    )
                                    .await?;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // Flush events still buffered when the turn future completed (the
    // runtime's own `stopped` event arrives this way).
    while let Ok(event) = rx.try_recv() {
        push_event(state, writer, map_event(event)).await?;
    }
    Ok(client_gone)
}

/// A `prompt` call while a turn is running: steer and follow-up queue;
/// an immediate prompt is refused, exactly like the native protocol.
async fn in_turn_prompt<W: AsyncWrite + Unpin>(
    writer: &mut W,
    state: &mut McpState,
    steer: &localpilot_harness::SteerQueue,
    id: Option<Value>,
    args: &Value,
) -> Result<(), RpcError> {
    let text = match prompt_text(args) {
        Ok(text) => text,
        Err(error) => return respond_error(writer, id, -32602, &error).await,
    };
    match prompt_disposition(args) {
        InputDisposition::Steer => {
            state.intervene("steer", text.clone());
            steer.push(text);
            respond(writer, id, tool_ok(json!({ "queued": "steer" }))).await
        }
        InputDisposition::FollowUp => {
            state.follow_ups.push_back(text);
            respond(writer, id, tool_ok(json!({ "queued": "follow_up" }))).await
        }
        InputDisposition::Immediate => {
            respond(
                writer,
                id,
                tool_error(
                    "a turn is already running; cancel first or use a \
                     steer/follow_up disposition",
                ),
            )
            .await
        }
    }
}

/// Tools that behave the same whether a turn is running or not.
#[allow(clippy::too_many_arguments)] // the adapter loop genuinely threads these
async fn dispatch_tool<W: AsyncWrite + Unpin>(
    writer: &mut W,
    state: &mut McpState,
    asks: &AskRegistry,
    options: &McpServeOptions,
    id: Option<Value>,
    name: &str,
    args: &Value,
    busy: bool,
) -> Result<(), RpcError> {
    match name {
        "status" => {
            let status = json!({
                "session_id": state.session_id,
                "model": options.model,
                "profile": options.profile,
                "busy": busy,
                "pending_asks": asks.outstanding(),
                "next_step": options.root.as_deref().and_then(next_incomplete_step),
            });
            respond(writer, id, tool_ok(status)).await
        }
        "events" => {
            let cursor = args["cursor"].as_u64().unwrap_or(0);
            let wait_ms = args["wait_ms"].as_u64().unwrap_or(0).min(MAX_EVENT_WAIT_MS);
            let page = state.feed.page_after(cursor);
            if page.is_empty() && wait_ms > 0 {
                // Park the call until the next event or the deadline. One
                // poll at a time: a newer one answers the old immediately.
                expire_poll(state, writer).await?;
                state.pending_poll = Some(PendingPoll {
                    id: id.unwrap_or(Value::Null),
                    cursor,
                    deadline: tokio::time::Instant::now()
                        + std::time::Duration::from_millis(wait_ms),
                });
                Ok(())
            } else {
                respond(writer, id, events_result(state, busy, page)).await
            }
        }
        "transcript" => {
            let last = args["last_n"]
                .as_u64()
                .map_or(DEFAULT_TRANSCRIPT_TAIL, |n| n as usize);
            let messages = state
                .store
                .read_transcript(state.session)
                .unwrap_or_default();
            let tail: Vec<Value> = messages
                .iter()
                .rev()
                .take(last)
                .rev()
                .map(|message| serde_json::to_value(message).unwrap_or_else(|_| json!({})))
                .collect();
            respond(writer, id, tool_ok(json!({ "messages": tail }))).await
        }
        "reply_permission" if options.approvals => {
            let ask_id = args["ask_id"].as_str().unwrap_or_default();
            let allow = args["allow"].as_bool().unwrap_or(false);
            if asks.resolve(ask_id, allow) {
                let context = state
                    .ask_context
                    .remove(ask_id)
                    .unwrap_or_else(|| ask_id.to_string());
                state.intervene(if allow { "allow" } else { "deny" }, context);
                respond(
                    writer,
                    id,
                    tool_ok(json!({ "resolved": true, "allow": allow })),
                )
                .await
            } else {
                respond(writer, id, tool_error(&format!("unknown ask id {ask_id}"))).await
            }
        }
        "cancel" => {
            // Reachable only when idle; the in-turn loop intercepts it.
            respond(writer, id, tool_error("no turn is running")).await
        }
        other => respond_error(writer, id, -32602, &format!("unknown tool: {other}")).await,
    }
}

/// Buffer one event and satisfy a parked poll if one is waiting. Also keeps
/// the activity marker and pending-ask context current, so an intervention
/// can say what the session was doing and what an answered ask was about.
async fn push_event<W: AsyncWrite + Unpin>(
    state: &mut McpState,
    writer: &mut W,
    event: ServerEvent,
) -> Result<(), RpcError> {
    match &event {
        ServerEvent::ToolStarted { name, .. } => {
            state.last_activity = Some(format!("running {name}"));
        }
        ServerEvent::Stopped { .. } => state.last_activity = None,
        ServerEvent::PermissionAsk {
            ask_id,
            tool,
            detail,
            ..
        } => {
            state
                .ask_context
                .insert(ask_id.clone(), format!("{tool}: {detail}"));
        }
        _ => {}
    }
    state.feed.push(event);
    if let Some(poll) = state.pending_poll.take() {
        let page = state.feed.page_after(poll.cursor);
        // `busy` is not knowable here without threading turn state; the page
        // itself tells the client what happened. Report busy from the events
        // present (a stopped event means the turn ended).
        let busy = !page_has_stopped(&page);
        respond(writer, Some(poll.id), events_result(state, busy, page)).await?;
    }
    Ok(())
}

/// Answer a parked poll with an empty page (its deadline passed, or a newer
/// poll displaced it).
async fn expire_poll<W: AsyncWrite + Unpin>(
    state: &mut McpState,
    writer: &mut W,
) -> Result<(), RpcError> {
    if let Some(poll) = state.pending_poll.take() {
        let page = state.feed.page_after(poll.cursor);
        let busy = !page_has_stopped(&page);
        respond(writer, Some(poll.id), events_result(state, busy, page)).await?;
    }
    Ok(())
}

fn page_has_stopped(page: &[Value]) -> bool {
    page.iter()
        .any(|entry| entry["event"]["type"].as_str() == Some("stopped"))
}

/// Sleep until the parked poll's deadline; pend forever when none is parked.
async fn poll_deadline(pending: &Option<PendingPoll>) {
    match pending {
        Some(poll) => tokio::time::sleep_until(poll.deadline).await,
        None => std::future::pending().await,
    }
}

fn events_result(state: &McpState, busy: bool, page: Vec<Value>) -> Value {
    tool_ok(json!({
        "events": page,
        "next_cursor": state.feed.head(),
        "dropped": state.feed.dropped,
        "busy": busy,
    }))
}

fn ask_event(ask: &PendingAsk) -> ServerEvent {
    ServerEvent::PermissionAsk {
        ask_id: ask.ask_id.clone(),
        tool: ask.tool.clone(),
        detail: ask.detail.clone(),
        risk: ask.risk.clone(),
    }
}

fn prompt_text(args: &Value) -> Result<String, String> {
    match args["text"].as_str() {
        Some(text) if !text.is_empty() => Ok(text.to_string()),
        _ => Err("prompt requires a non-empty `text` argument".to_string()),
    }
}

fn prompt_disposition(args: &Value) -> InputDisposition {
    match args["disposition"].as_str() {
        Some("steer") => InputDisposition::Steer,
        Some("follow_up") => InputDisposition::FollowUp,
        _ => InputDisposition::Immediate,
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "localpilot",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "Drives one LocalPilot coding session. Call `prompt` to \
            start a turn, then poll `events` (pass the returned next_cursor and a \
            wait_ms up to 20000) until a `stopped` event arrives. While a turn \
            runs: `prompt` with disposition=steer injects guidance at the next \
            safe boundary, disposition=follow_up queues another turn, and \
            `cancel` aborts. A `permission_ask` event holds a tool until \
            `reply_permission` answers it; an unanswered ask is denied.",
    })
}

/// A successful tool result: structured content plus its serialized text form.
fn tool_ok(value: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": value.to_string() }],
        "structuredContent": value,
        "isError": false,
    })
}

/// A tool-level failure (the call was valid; the operation was not).
fn tool_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

fn tool_catalog(options: &McpServeOptions) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "prompt",
            "description": "Submit input to the session. Starts a turn when idle \
                (any disposition). While a turn runs: disposition=steer injects \
                the text at the next safe boundary; disposition=follow_up queues \
                it as the next turn; an immediate prompt is refused.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The input text." },
                    "disposition": {
                        "type": "string",
                        "enum": ["immediate", "steer", "follow_up"],
                        "description": "How to admit the input. Default: immediate.",
                    },
                },
                "required": ["text"],
            },
        }),
        json!({
            "name": "events",
            "description": "Page the session event feed after `cursor`. When the \
                page would be empty and wait_ms > 0, waits (bounded) for the next \
                event. Returns events, next_cursor, a dropped-entry count, and \
                whether a turn is running.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cursor": {
                        "type": "integer",
                        "description": "Return events after this sequence number. Default 0.",
                    },
                    "wait_ms": {
                        "type": "integer",
                        "description": "Bounded wait for the first new event (max 20000). Default 0.",
                    },
                },
            },
        }),
        json!({
            "name": "status",
            "description": "Session, model, profile, busy state, pending permission \
                asks, and the next incomplete harness step.",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "cancel",
            "description": "Cancel the running turn.",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "transcript",
            "description": "The tail of the session transcript (already redacted \
                at write time).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "last_n": {
                        "type": "integer",
                        "description": "How many trailing messages. Default 20.",
                    },
                },
            },
        }),
    ];
    if options.approvals {
        tools.push(json!({
            "name": "reply_permission",
            "description": "Answer a pending permission ask from a permission_ask \
                event. An unanswered ask is denied.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ask_id": { "type": "string", "description": "The ask to answer." },
                    "allow": { "type": "boolean", "description": "Allow (true) or deny (false)." },
                },
                "required": ["ask_id", "allow"],
            },
        }));
    }
    tools
}

async fn respond<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: Option<Value>,
    result: Value,
) -> Result<(), RpcError> {
    write_line(
        writer,
        &json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result }),
    )
    .await
}

async fn respond_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: Option<Value>,
    code: i64,
    message: &str,
) -> Result<(), RpcError> {
    write_line(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": id.unwrap_or(Value::Null),
            "error": { "code": code, "message": message },
        }),
    )
    .await
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> Result<(), RpcError> {
    let mut line = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;
    Ok(())
}
