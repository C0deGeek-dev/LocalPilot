//! ACP (Agent Client Protocol) adapter over the session runtime.
//!
//! Implements the agent side of the public ACP specification
//! (<https://agentclientprotocol.com>, protocol version 1): JSON-RPC 2.0 over
//! LF-delimited stdio. The editor renders prompts and permission asks;
//! LocalPilot owns every decision — ACP permission requests map onto the
//! permission engine's verdicts, and a cancelled/unanswered request is a
//! denial.
//!
//! Provenance: implemented from the published protocol documentation and
//! schema only; no other agent's implementation was consulted.

use localpilot_harness::{RuntimeEvent, SessionRuntime, StopReason};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::approver::{AskRegistry, PendingAsk};
use crate::framing::JsonRecordReader as Reader;
use crate::serve::RpcError;

/// The ACP protocol version this adapter implements.
pub const ACP_PROTOCOL_VERSION: u16 = 1;

/// Serve one ACP client (an editor) over `reader`/`writer`.
///
/// The runtime must have been built with the [`crate::RpcApprover`] whose
/// halves are passed here, so permission asks become
/// `session/request_permission` requests.
///
/// # Errors
/// Returns [`RpcError`] on transport failure.
pub async fn serve_acp<R, W>(
    runtime: &mut SessionRuntime,
    mut ask_rx: mpsc::UnboundedReceiver<PendingAsk>,
    asks: AskRegistry,
    reader: R,
    mut writer: W,
) -> Result<(), RpcError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = Reader::new(reader);
    let session_id = runtime.session_id().to_string();
    // Agent-initiated request ids (permission requests) live in their own
    // numeric space, distinguishable from client request ids we only echo.
    let mut next_request_id: u64 = 1;

    loop {
        let message = tokio::select! {
            message = reader.next() => message?,
            Some(ask) = ask_rx.recv() => {
                // An ask outside a prompt turn (e.g. a host-driven gate check)
                // still reaches the client.
                send_permission_request(&mut writer, &mut next_request_id, &session_id, &ask)
                    .await?;
                continue;
            }
        };
        let Some(message) = message else { break };

        let method = message["method"].as_str().unwrap_or_default().to_string();
        let id = message.get("id").cloned();
        match method.as_str() {
            "initialize" => {
                let client_version = message["params"]["protocolVersion"]
                    .as_u64()
                    .unwrap_or(u64::from(ACP_PROTOCOL_VERSION));
                let version = client_version.min(u64::from(ACP_PROTOCOL_VERSION));
                respond(
                    &mut writer,
                    id,
                    json!({
                        "protocolVersion": version,
                        "agentInfo": {
                            "name": "localpilot",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "agentCapabilities": {
                            "loadSession": false,
                            "promptCapabilities": {
                                "image": false,
                                "audio": false,
                                "embeddedContext": false,
                            },
                        },
                        "authMethods": [],
                    }),
                )
                .await?;
            }
            "session/new" => {
                respond(&mut writer, id, json!({ "sessionId": session_id })).await?;
            }
            "session/prompt" => {
                let text = prompt_text(&message["params"]);
                let stop = run_prompt_turn(
                    runtime,
                    &mut ask_rx,
                    &asks,
                    &mut reader,
                    &mut writer,
                    &mut next_request_id,
                    &session_id,
                    &text,
                )
                .await?;
                respond(&mut writer, id, json!({ "stopReason": stop })).await?;
            }
            "session/cancel" => {
                // Nothing to cancel outside a turn; inside a turn the inner
                // loop handles it.
            }
            "" => {
                // A response without a method: a stray permission reply after
                // its turn ended. Resolve it if it matches; otherwise ignore.
                resolve_permission_response(&asks, &message);
            }
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
    Ok(())
}

/// Drive one prompt turn: stream `session/update` notifications, surface
/// permission asks as `session/request_permission`, honor `session/cancel`.
/// Returns the ACP stop reason.
#[allow(clippy::too_many_arguments)] // the adapter loop genuinely threads these
async fn run_prompt_turn<R, W>(
    runtime: &mut SessionRuntime,
    ask_rx: &mut mpsc::UnboundedReceiver<PendingAsk>,
    asks: &AskRegistry,
    reader: &mut Reader<R>,
    writer: &mut W,
    next_request_id: &mut u64,
    session_id: &str,
    text: &str,
) -> Result<&'static str, RpcError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (events, mut rx) = broadcast::channel::<RuntimeEvent>(1024);
    let cancel = CancellationToken::new();
    let stop;
    {
        let turn = runtime.run_turn(text, &events, &cancel);
        tokio::pin!(turn);
        loop {
            tokio::select! {
                reason = &mut turn => {
                    stop = stop_reason(reason);
                    break;
                }
                event = rx.recv() => {
                    if let Ok(event) = event {
                        forward_event(writer, session_id, event).await?;
                    }
                }
                Some(ask) = ask_rx.recv() => {
                    send_permission_request(writer, next_request_id, session_id, &ask).await?;
                }
                message = reader.next() => match message? {
                    // Client gone: cancel; outstanding asks deny, never
                    // approve.
                    None => cancel.cancel(),
                    Some(message) => {
                        if message["method"].as_str() == Some("session/cancel") {
                            cancel.cancel();
                        } else {
                            resolve_permission_response(asks, &message);
                        }
                    }
                }
            }
        }
    }
    while let Ok(event) = rx.try_recv() {
        forward_event(writer, session_id, event).await?;
    }
    Ok(stop)
}

/// Map a runtime event onto a `session/update` notification.
async fn forward_event<W: AsyncWrite + Unpin>(
    writer: &mut W,
    session_id: &str,
    event: RuntimeEvent,
) -> Result<(), RpcError> {
    let update = match event {
        RuntimeEvent::Text(text) => json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": text },
        }),
        RuntimeEvent::Reasoning(text) => json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": text },
        }),
        RuntimeEvent::ToolStarted { id, name } => json!({
            "sessionUpdate": "tool_call",
            "toolCallId": id,
            "title": name,
            "kind": "other",
            "status": "in_progress",
        }),
        RuntimeEvent::ToolFinished { id, is_error, .. } => json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": if is_error { "failed" } else { "completed" },
        }),
        // Usage, context, warnings, plan, quota, and recovery have no ACP
        // update kind in version 1; they remain visible through the durable
        // event log and the native RPC protocol.
        _ => return Ok(()),
    };
    notify(
        writer,
        "session/update",
        json!({ "sessionId": session_id, "update": update }),
    )
    .await
}

/// Surface a permission ask as an agent-initiated
/// `session/request_permission` request. The reply is routed back to the
/// permission engine by id; the option ids carry the verdict.
async fn send_permission_request<W: AsyncWrite + Unpin>(
    writer: &mut W,
    next_request_id: &mut u64,
    session_id: &str,
    ask: &PendingAsk,
) -> Result<(), RpcError> {
    let request_id = format!("perm-{}-{}", *next_request_id, ask.ask_id);
    *next_request_id += 1;
    let title = if ask.detail.is_empty() {
        format!("{} ({})", ask.tool, ask.risk)
    } else {
        format!("{}: {}", ask.tool, ask.detail)
    };
    let message = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/request_permission",
        "params": {
            "sessionId": session_id,
            "toolCall": {
                "toolCallId": ask.ask_id,
                "title": title,
                "kind": "other",
                "status": "pending",
            },
            "options": [
                { "optionId": format!("allow:{}", ask.ask_id), "name": "Allow", "kind": "allow_once" },
                { "optionId": format!("reject:{}", ask.ask_id), "name": "Reject", "kind": "reject_once" },
            ],
        },
    });
    write_line(writer, &message).await
}

/// Route a client response to a `session/request_permission` back into the
/// permission engine. A cancelled outcome — or anything unrecognizable — is a
/// denial.
fn resolve_permission_response(asks: &AskRegistry, message: &Value) {
    let Some(id) = message["id"].as_str() else {
        return;
    };
    let Some(ask_id) = id.splitn(3, '-').nth(2) else {
        return;
    };
    let outcome = &message["result"]["outcome"];
    let allow = outcome["outcome"].as_str() == Some("selected")
        && outcome["optionId"]
            .as_str()
            .is_some_and(|option| option.starts_with("allow:"));
    let _ = asks.resolve(ask_id, allow);
}

/// The text of a prompt request: its text content blocks, joined.
fn prompt_text(params: &Value) -> String {
    params["prompt"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block["type"].as_str() == Some("text"))
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Map the loop's stop reason onto the ACP stop-reason vocabulary.
fn stop_reason(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Done => "end_turn",
        StopReason::Cancelled => "cancelled",
        StopReason::Degraded
        | StopReason::ProviderError
        | StopReason::BudgetExceeded
        | StopReason::NoProgress
        | StopReason::TimedOut => "error",
    }
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

async fn notify<W: AsyncWrite + Unpin>(
    writer: &mut W,
    method: &str,
    params: Value,
) -> Result<(), RpcError> {
    write_line(
        writer,
        &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
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
