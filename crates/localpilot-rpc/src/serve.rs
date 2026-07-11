//! The serve loop: drive a [`SessionRuntime`] over any byte transport.
//!
//! Generic over reader/writer so tests exercise it through an in-memory
//! duplex and `localpilot rpc` wires real stdin/stdout. One command record in
//! per line, streamed event records out; the runtime, permission engine, and
//! event log behave exactly as they do for interactive hosts.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use localpilot_harness::{RuntimeEvent, SessionRuntime, StopReason};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::approver::{AskRegistry, PendingAsk};
use crate::framing::LineFraming;
use crate::protocol::{
    ClientCommand, ClientRecord, InputDisposition, PlanStepWire, ServerEvent, ServerRecord,
    RPC_PROTOCOL_VERSION,
};

/// Errors from the serve loop itself (transport failures; protocol problems
/// are reported to the client as `error` events instead).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RpcError {
    /// The transport failed.
    #[error("rpc transport error: {0}")]
    Transport(#[from] std::io::Error),
}

/// Static session facts for `status` replies (captured at serve start so
/// status stays answerable while the runtime is mid-turn).
#[derive(Debug, Clone)]
pub struct ServeContext {
    /// The model the session runs.
    pub model: String,
    /// The active permission profile's display label.
    pub profile: String,
    /// The workspace root, for harness-step inspection.
    pub root: Option<PathBuf>,
}

/// Incremental record reader over any byte source.
struct RecordReader<R> {
    source: R,
    framing: LineFraming,
    queued: VecDeque<Vec<u8>>,
    eof: bool,
}

impl<R: AsyncRead + Unpin> RecordReader<R> {
    fn new(source: R) -> Self {
        Self {
            source,
            framing: LineFraming::default(),
            queued: VecDeque::new(),
            eof: false,
        }
    }

    /// The next complete record, or `None` at end of input.
    async fn next(&mut self) -> Result<Option<Vec<u8>>, std::io::Error> {
        loop {
            if let Some(record) = self.queued.pop_front() {
                return Ok(Some(record));
            }
            if self.eof {
                return Ok(None);
            }
            let mut chunk = [0u8; 4096];
            let read = self.source.read(&mut chunk).await?;
            if read == 0 {
                self.eof = true;
                continue;
            }
            self.queued.extend(self.framing.push(&chunk[..read])?);
        }
    }
}

async fn emit<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: Option<String>,
    event: ServerEvent,
) -> Result<(), RpcError> {
    let record = ServerRecord {
        v: RPC_PROTOCOL_VERSION,
        id,
        event,
    };
    let mut line = serde_json::to_vec(&record).unwrap_or_else(|_| b"{}".to_vec());
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;
    Ok(())
}

/// Serve one client over `reader`/`writer` until shutdown or end of input.
///
/// The runtime must have been built with the [`crate::RpcApprover`] whose
/// halves are passed here, so permission asks flow to this client.
///
/// # Errors
/// Returns [`RpcError`] on transport failure.
pub async fn serve<R, W>(
    runtime: &mut SessionRuntime,
    mut ask_rx: mpsc::UnboundedReceiver<PendingAsk>,
    asks: AskRegistry,
    reader: R,
    mut writer: W,
    context: &ServeContext,
) -> Result<(), RpcError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = RecordReader::new(reader);
    let session_id = runtime.session_id().to_string();
    let model = context.model.clone();
    let mut follow_ups: VecDeque<String> = VecDeque::new();

    loop {
        // Idle: wait for a command (or surface a stray ask, e.g. from a host
        // running gate checks on this runtime's engine).
        let record = tokio::select! {
            record = reader.next() => record?,
            Some(ask) = ask_rx.recv() => {
                emit_ask(&mut writer, &ask).await?;
                continue;
            }
        };
        let Some(bytes) = record else { break };
        let (id, command) = match parse(&bytes) {
            Ok(parsed) => parsed,
            Err(message) => {
                emit(&mut writer, None, ServerEvent::Error { message }).await?;
                continue;
            }
        };

        let prompt = match command {
            ClientCommand::Hello => {
                emit(
                    &mut writer,
                    id,
                    ServerEvent::Hello {
                        protocol_version: RPC_PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        model: model.clone(),
                    },
                )
                .await?;
                continue;
            }
            ClientCommand::Status => {
                let status = status_event(&session_id, &model, context, false, &asks);
                emit(&mut writer, id, status).await?;
                continue;
            }
            ClientCommand::Cancel => {
                emit(
                    &mut writer,
                    id,
                    ServerEvent::Error {
                        message: "no turn is running".to_string(),
                    },
                )
                .await?;
                continue;
            }
            ClientCommand::PermissionReply { ask_id, allow } => {
                if !asks.resolve(&ask_id, allow) {
                    emit(
                        &mut writer,
                        id,
                        ServerEvent::Error {
                            message: format!("unknown ask id {ask_id}"),
                        },
                    )
                    .await?;
                }
                continue;
            }
            ClientCommand::Shutdown => {
                emit(&mut writer, id, ServerEvent::Closed).await?;
                break;
            }
            // Any disposition starts a turn now when the session is idle.
            ClientCommand::Prompt { text, .. } => text,
        };

        let mut next = Some(prompt);
        while let Some(text) = next.take() {
            let shutdown = run_prompt(
                runtime,
                &mut ask_rx,
                &asks,
                &mut reader,
                &mut writer,
                context,
                &session_id,
                &model,
                &mut follow_ups,
                &text,
            )
            .await?;
            if shutdown {
                return Ok(());
            }
            next = follow_ups.pop_front();
        }
    }
    Ok(())
}

/// Drive one turn while staying responsive to commands. Returns whether the
/// client asked to shut down.
#[allow(clippy::too_many_arguments)] // the serve loop genuinely threads these
async fn run_prompt<R, W>(
    runtime: &mut SessionRuntime,
    ask_rx: &mut mpsc::UnboundedReceiver<PendingAsk>,
    asks: &AskRegistry,
    reader: &mut RecordReader<R>,
    writer: &mut W,
    context: &ServeContext,
    session_id: &str,
    model: &str,
    follow_ups: &mut VecDeque<String>,
    text: &str,
) -> Result<bool, RpcError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let steer = runtime.steer_queue();
    let (events, mut rx) = broadcast::channel::<RuntimeEvent>(1024);
    let cancel = CancellationToken::new();
    let mut shutdown = false;
    {
        let turn = runtime.run_turn(text, &events, &cancel);
        tokio::pin!(turn);
        loop {
            tokio::select! {
                _ = &mut turn => break,
                event = rx.recv() => {
                    if let Ok(event) = event {
                        emit(writer, None, map_event(event)).await?;
                    }
                }
                Some(ask) = ask_rx.recv() => emit_ask(writer, &ask).await?,
                record = reader.next() => match record? {
                    // Client gone: cancel; outstanding asks deny by timeout or
                    // registry drop — never silently approve.
                    None => {
                        cancel.cancel();
                        shutdown = true;
                    }
                    Some(bytes) => match parse(&bytes) {
                        Err(message) => {
                            emit(writer, None, ServerEvent::Error { message }).await?;
                        }
                        Ok((id, command)) => match command {
                            ClientCommand::Cancel => cancel.cancel(),
                            ClientCommand::PermissionReply { ask_id, allow } => {
                                if !asks.resolve(&ask_id, allow) {
                                    emit(writer, id, ServerEvent::Error {
                                        message: format!("unknown ask id {ask_id}"),
                                    }).await?;
                                }
                            }
                            ClientCommand::Prompt { text, disposition } => match disposition {
                                InputDisposition::Steer => {
                                    steer.push(text);
                                    emit(writer, id, ServerEvent::Queued {
                                        disposition: InputDisposition::Steer,
                                    }).await?;
                                }
                                InputDisposition::FollowUp => {
                                    follow_ups.push_back(text);
                                    emit(writer, id, ServerEvent::Queued {
                                        disposition: InputDisposition::FollowUp,
                                    }).await?;
                                }
                                InputDisposition::Immediate => {
                                    emit(writer, id, ServerEvent::Error {
                                        message: "a turn is already running; \
                                                  cancel first or use a steer/follow_up disposition"
                                            .to_string(),
                                    }).await?;
                                }
                            },
                            ClientCommand::Status => {
                                let status = status_event(session_id, model, context, true, asks);
                                emit(writer, id, status).await?;
                            }
                            ClientCommand::Hello => {
                                emit(writer, id, ServerEvent::Hello {
                                    protocol_version: RPC_PROTOCOL_VERSION,
                                    session_id: session_id.to_string(),
                                    model: model.to_string(),
                                }).await?;
                            }
                            ClientCommand::Shutdown => {
                                cancel.cancel();
                                shutdown = true;
                            }
                        }
                    }
                }
            }
        }
    }
    // Flush events still buffered when the turn future completed.
    while let Ok(event) = rx.try_recv() {
        emit(writer, None, map_event(event)).await?;
    }
    if shutdown {
        emit(writer, None, ServerEvent::Closed).await?;
    }
    Ok(shutdown)
}

fn parse(bytes: &[u8]) -> Result<(Option<String>, ClientCommand), String> {
    let record: ClientRecord =
        serde_json::from_slice(bytes).map_err(|e| format!("malformed record: {e}"))?;
    if record.v > RPC_PROTOCOL_VERSION {
        return Err(format!(
            "protocol version {} is newer than this agent supports (up to {})",
            record.v, RPC_PROTOCOL_VERSION
        ));
    }
    Ok((record.id, record.command))
}

async fn emit_ask<W: AsyncWrite + Unpin>(writer: &mut W, ask: &PendingAsk) -> Result<(), RpcError> {
    emit(
        writer,
        None,
        ServerEvent::PermissionAsk {
            ask_id: ask.ask_id.clone(),
            tool: ask.tool.clone(),
            detail: ask.detail.clone(),
            risk: ask.risk.clone(),
        },
    )
    .await
}

fn status_event(
    session_id: &str,
    model: &str,
    context: &ServeContext,
    busy: bool,
    asks: &AskRegistry,
) -> ServerEvent {
    ServerEvent::Status {
        session_id: session_id.to_string(),
        model: model.to_string(),
        profile: context.profile.clone(),
        busy,
        pending_asks: asks.outstanding(),
        next_step: context.root.as_deref().and_then(next_incomplete_step),
    }
}

/// The next incomplete step from the project's plan file, when one exists.
pub(crate) fn next_incomplete_step(root: &Path) -> Option<String> {
    let progress = std::fs::read_to_string(root.join("PROGRESS.md")).ok()?;
    progress
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("- [ ]"))
        .map(|line| line.trim_start_matches("- [ ]").trim().to_string())
}

pub(crate) fn map_event(event: RuntimeEvent) -> ServerEvent {
    match event {
        RuntimeEvent::Text(text) => ServerEvent::TextDelta { text },
        RuntimeEvent::Reasoning(text) => ServerEvent::ReasoningDelta { text },
        RuntimeEvent::ToolStarted { id, name } => ServerEvent::ToolStarted { id, name },
        RuntimeEvent::ToolFinished {
            id,
            name,
            is_error,
            output,
        } => ServerEvent::ToolFinished {
            id,
            name,
            is_error,
            output,
        },
        RuntimeEvent::Usage(usage) => ServerEvent::Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        },
        RuntimeEvent::ContextUsage { used, limit } => ServerEvent::ContextUsage { used, limit },
        RuntimeEvent::Warning(message) => ServerEvent::Warning { message },
        RuntimeEvent::Plan(steps) => ServerEvent::Plan {
            steps: steps
                .into_iter()
                .map(|step| PlanStepWire {
                    title: step.title,
                    status: step.status,
                })
                .collect(),
        },
        RuntimeEvent::QuotaPaused { reset } => ServerEvent::QuotaPaused { reset },
        RuntimeEvent::Recovery { health } => ServerEvent::Recovery {
            health: format!("{health:?}").to_ascii_lowercase(),
        },
        RuntimeEvent::Stopped(reason) => ServerEvent::Stopped {
            reason: stop_reason_label(reason).to_string(),
        },
        RuntimeEvent::ToolStuck { name, count } => ServerEvent::ToolStuck { name, count },
    }
}

fn stop_reason_label(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Done => "done",
        StopReason::Cancelled => "cancelled",
        StopReason::Degraded => "degraded",
        StopReason::ProviderError => "provider_error",
        StopReason::BudgetExceeded => "budget_exceeded",
        StopReason::NoProgress => "no_progress",
        StopReason::TimedOut => "timed_out",
    }
}
