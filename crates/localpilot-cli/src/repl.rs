//! `localpilot chat` — the interactive terminal REPL.
//!
//! This is the terminal driver: it maps real crossterm key events into the
//! backend-agnostic `localpilot-tui` core, runs a session turn per submission,
//! and forwards the runtime event stream into the UI. It is the un-testable
//! terminal-I/O edge; the rendering and input logic it drives are unit-tested in
//! `localpilot-tui`.

use std::future::Future;
use std::io::{self, Stdout};
use std::pin::Pin;
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{execute, terminal};
use localpilot_config::{CliOverrides, ConfigPaths};
use localpilot_harness::{ModelHealth, RuntimeEvent, SessionConfig, SessionRuntime};
use localpilot_llm::ProviderRegistry;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{
    Approver, Effect, Interactivity, PermissionEngine, PermissionRequest, Profile, Workspace,
};
use localpilot_store::Store;
use localpilot_tui::{
    handle_input, parse_slash, render, AppInput, AppState, ApprovalRequest, Header, Key, Mode,
    PlanItem, Profile as UiProfile, SlashAction, TrustPrompt, UiEvent,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::key_input::{is_cancel, is_key_action, is_newline, is_submit};

/// A pending approval handed from the [`TuiApprover`] (running inside the turn)
/// to the event loop, which raises the modal and replies with the user's answer.
struct ApprovalCall {
    request: ApprovalRequest,
    reply: oneshot::Sender<bool>,
}

/// Host context needed by slash commands that leave pure UI state and run CLI
/// workflows.
struct CommandHost<'a> {
    approval_tx: mpsc::UnboundedSender<ApprovalCall>,
    cwd: &'a std::path::Path,
    model: &'a str,
    provider_id: Option<&'a str>,
}

/// An [`Approver`] that suspends the turn and asks the user through the TUI.
struct TuiApprover {
    tx: mpsc::UnboundedSender<ApprovalCall>,
}

impl Approver for TuiApprover {
    fn approve<'a>(
        &'a self,
        request: &'a PermissionRequest,
    ) -> Pin<Box<dyn Future<Output = bool> + 'a>> {
        let (reply, answer) = oneshot::channel();
        let sent = self.tx.send(ApprovalCall {
            request: describe(request),
            reply,
        });
        Box::pin(async move {
            // A closed channel (UI gone) is a denial, never a silent approval.
            if sent.is_err() {
                return false;
            }
            answer.await.unwrap_or(false)
        })
    }
}

/// Map a permission request into the UI's approval view model.
fn describe(request: &PermissionRequest) -> ApprovalRequest {
    let (target_kind, risk_class) = match request.effect {
        Effect::ReadPath { secret_like, .. } => (
            "path",
            if secret_like {
                "read a secret-like path"
            } else {
                "read outside the workspace"
            },
        ),
        Effect::WritePath { overwrite, .. } => (
            "path",
            if overwrite {
                "overwrite a file"
            } else {
                "write a file"
            },
        ),
        Effect::RunCommand(_) => ("command", "run a command"),
        Effect::Network => ("network", "make a network request"),
    };
    let target = if request.detail.is_empty() {
        format!("({target_kind})")
    } else {
        request.detail.clone()
    };
    ApprovalRequest {
        tool: request.tool.to_string(),
        target,
        risk_class: risk_class.to_string(),
    }
}

/// Launch the interactive REPL.
///
/// # Errors
/// Returns an error if configuration, the provider, the workspace, or the
/// terminal cannot be set up.
pub async fn run_chat(
    model: Option<&str>,
    provider_id: Option<&str>,
    profile: Profile,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config = localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())?;
    let model = model
        .map(str::to_string)
        .or_else(|| config.resolve_model(provider_id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no model: pass --model, or set a default in .localpilot.toml \
                 ([providers.<id>] model = \"...\")"
            )
        })?;
    let registry = ProviderRegistry::from_config(&config)?;
    let provider = match provider_id {
        Some(id) => registry.get(id),
        None => registry.default_provider(),
    }
    .cloned()
    .ok_or_else(|| anyhow::anyhow!("no provider is configured"))?;

    // The real context window: per-provider config first, then best-effort
    // discovery from the local server's model listing. Failure means falling
    // back to the configured global budget, never an error.
    let mut context_window = provider.declaration().max_context_tokens;
    if context_window.is_none() {
        context_window = discovered_window(&config, provider_id, &model).await;
    }

    // Ask-gated actions suspend the turn and prompt in the TUI; the user's
    // y/n answer flows back through this channel to the permission engine.
    let (approval_tx, mut approval_rx) = mpsc::unbounded_channel::<ApprovalCall>();
    let mut runtime = SessionRuntime::new(
        provider,
        crate::mcp::McpTools::load(&config).await.registry(),
        PermissionEngine::new(profile, Vec::new()),
        Box::new(TuiApprover {
            tx: approval_tx.clone(),
        }),
        Store::open(&cwd),
        Workspace::new(&cwd)?,
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            model: model.to_string(),
            interactivity: Interactivity::Interactive,
            trusted: profile == Profile::Bypass,
            context_token_limit: localpilot_harness::effective_context_limit(
                context_window,
                config.harness.context_token_limit,
            ),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    // Relevant accepted LocalMind memory is contributed per turn through the
    // context-hook fabric.
    crate::context_inject::register(&cwd, &mut runtime);

    let header = Header {
        version: env!("LOCALPILOT_VERSION").to_string(),
        provider: provider_id.unwrap_or(&config.provider.default).to_string(),
        model: model.to_string(),
        workspace: cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| cwd.display().to_string()),
        session_id: runtime.session_id().to_string(),
        update: crate::update::cached_notice(&cwd).await,
    };
    let mut state = AppState::new(header, Mode::Agent, ui_profile(profile));
    // Ask once per folder before doing anything in it; trust is remembered across
    // sessions. Already-trusted folders (and bypass, which is explicit) skip it.
    if profile != Profile::Bypass && !crate::trust::is_trusted(&cwd) {
        state.trust = Some(TrustPrompt {
            path: cwd.display().to_string(),
        });
    } else {
        state.trusted = true;
    }

    let session_id = runtime.session_id();
    let mut terminal = enter_terminal()?;
    let result = event_loop(
        &mut terminal,
        &mut state,
        &mut runtime,
        &mut approval_rx,
        CommandHost {
            approval_tx,
            cwd: &cwd,
            model: &model,
            provider_id,
        },
    )
    .await;
    leave_terminal(&mut terminal)?;
    // Learn from the finished session. This is best-effort so terminal teardown
    // is never held hostage by the learning subsystem.
    crate::context_inject::close_out(&cwd, session_id);
    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    runtime: &mut SessionRuntime,
    approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>,
    host: CommandHost<'_>,
) -> anyhow::Result<()> {
    loop {
        terminal.draw(|frame| render(frame, state))?;
        if state.should_quit {
            return Ok(());
        }

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if is_key_action(key) => {
                    if state.trust.is_some() {
                        // While the trust gate is up, route keys to it and persist
                        // the decision when the folder is trusted.
                        if let Some(mapped) = map_key(key) {
                            handle_input(state, AppInput::Key(mapped));
                        }
                        if state.trusted {
                            crate::trust::remember(host.cwd);
                        }
                    } else if is_newline(key, &state.input) {
                        state.insert_input_newline();
                    } else if is_submit(key, &state.input) {
                        // Expand collapsed pastes for the model, but keep the
                        // compact form in the transcript.
                        let shown = std::mem::take(&mut state.input);
                        state.input_cursor = 0;
                        let prompt = state.expand_pastes(&shown);
                        state.pastes.clear();
                        if let Some(action) = parse_slash(&prompt) {
                            run_slash(terminal, state, runtime, approval_rx, &host, action).await?;
                        } else {
                            state.apply(UiEvent::UserMessage(shown));
                            state.busy = true;
                            let outcome =
                                run_turn(terminal, state, runtime, approval_rx, &prompt).await;
                            state.busy = false;
                            outcome?;
                        }
                    } else if let Some(mapped) = map_key(key) {
                        handle_input(state, AppInput::Key(mapped));
                    }
                }
                // Bracketed paste: insert small pastes inline, but collapse large
                // ones to a placeholder so the input line stays readable.
                Event::Paste(text) if state.trust.is_none() => insert_paste(state, text),
                Event::Mouse(mouse) if state.trust.is_none() => {
                    if let Some(mapped) = map_mouse(mouse.kind) {
                        handle_input(state, AppInput::Key(mapped));
                    }
                }
                _ => {}
            }
        }
    }
}

async fn run_slash(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    runtime: &mut SessionRuntime,
    approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>,
    host: &CommandHost<'_>,
    action: SlashAction,
) -> anyhow::Result<()> {
    match action {
        SlashAction::SetMode(mode) => state.mode = mode,
        SlashAction::SetProfile(profile) => {
            state.profile = profile;
            runtime.set_permission_profile(sandbox_profile(profile), Vec::new());
        }
        SlashAction::ToggleThinking => state.thinking.visible = !state.thinking.visible,
        SlashAction::NewSession => {
            runtime.start_new_session();
            state.clear_conversation_view();
            state.header.session_id = runtime.session_id().to_string();
            state.apply(UiEvent::Notice(format!(
                "started new session {}",
                runtime.session_id()
            )));
        }
        action @ (SlashAction::Fork | SlashAction::CloneSession) => {
            let mark_fork = matches!(action, SlashAction::Fork);
            match runtime.fork_session(mark_fork) {
                Ok(id) => {
                    state.header.session_id = id.to_string();
                    let verb = if mark_fork { "forked" } else { "cloned" };
                    state.apply(UiEvent::Notice(format!("{verb} into session {id}")));
                }
                Err(error) => {
                    state.apply(UiEvent::Notice(format!("branch failed: {error}")));
                }
            }
        }
        SlashAction::Tree => match runtime.store().read_events(runtime.session_id()) {
            Ok(events) => {
                for line in render_session_tree(&events) {
                    state.apply(UiEvent::Notice(line));
                }
            }
            Err(error) => {
                state.apply(UiEvent::Notice(format!("event log unreadable: {error}")));
            }
        },
        SlashAction::Sessions => match runtime.store().list_sessions() {
            Ok(mut sessions) => {
                sessions.sort_by(|a, b| b.updated_unix.cmp(&a.updated_unix));
                if sessions.is_empty() {
                    state.apply(UiEvent::Notice("no sessions in this workspace".to_string()));
                }
                for entry in sessions.into_iter().take(10) {
                    let current = if entry.id == runtime.session_id() {
                        " (current)"
                    } else {
                        ""
                    };
                    state.apply(UiEvent::Notice(format!(
                        "{} — {} message(s){current}",
                        entry.id, entry.message_count
                    )));
                }
            }
            Err(error) => {
                state.apply(UiEvent::Notice(format!(
                    "session index unreadable: {error}"
                )));
            }
        },
        SlashAction::LoadSession(id) => match id.parse::<localpilot_core::SessionId>() {
            Ok(session) => match runtime.load_session(session) {
                Ok(()) => {
                    state.clear_conversation_view();
                    state.header.session_id = session.to_string();
                    state.apply(UiEvent::Notice(format!(
                        "resumed session {session}; current profile and trust apply"
                    )));
                }
                Err(error) => {
                    state.apply(UiEvent::Notice(format!("resume failed: {error}")));
                }
            },
            Err(_) => {
                state.apply(UiEvent::Notice(format!("not a session id: {id}")));
            }
        },
        SlashAction::SetEffort(level) => match localpilot_llm::ReasoningEffort::parse(&level) {
            Some(effort) => {
                runtime.set_reasoning_effort(Some(effort));
                state.footer.effort = Some(effort.as_str().to_string());
                state.apply(UiEvent::Notice(format!(
                    "reasoning effort set to {}",
                    effort.as_str()
                )));
            }
            None => {
                state.apply(UiEvent::Notice(format!(
                    "invalid effort {level:?}; use minimal, low, medium, or high"
                )));
            }
        },
        SlashAction::Clear => {
            runtime.clear_conversation();
            state.clear_conversation_view();
            let (context_used, context_limit) = runtime.context_usage();
            state.apply(UiEvent::ContextUsage {
                context_used,
                context_limit,
            });
            state.apply(UiEvent::Notice("conversation cleared".to_string()));
        }
        SlashAction::Compact => {
            let summary = runtime.compact_conversation();
            state.apply(UiEvent::ContextUsage {
                context_used: summary.context_used,
                context_limit: summary.context_limit,
            });
            let notice = if summary.compacted {
                format!(
                    "compacted conversation history; context {}/{}",
                    summary.context_used, summary.context_limit
                )
            } else {
                format!(
                    "conversation already compact enough; context {}/{}",
                    summary.context_used, summary.context_limit
                )
            };
            state.apply(UiEvent::Notice(notice));
        }
        SlashAction::Search(query) => state.set_search(query),
        SlashAction::Resume => {
            state.mode = Mode::Harness;
            state.apply(UiEvent::Notice("running harness resume".to_string()));
            run_harness_command(terminal, state, approval_rx, host, false).await?;
        }
        SlashAction::WaitResume => {
            state.mode = Mode::Harness;
            state.apply(UiEvent::Notice("checking paused harness run".to_string()));
            run_harness_command(terminal, state, approval_rx, host, true).await?;
        }
        SlashAction::Quit => state.should_quit = true,
        SlashAction::Invalid { command, reason } => {
            state.apply(UiEvent::Notice(format!("invalid /{command}: {reason}")));
        }
        SlashAction::Unknown(command) => {
            state.apply(UiEvent::Notice(format!(
                "unknown slash command: /{command}"
            )));
        }
    }
    Ok(())
}

async fn run_harness_command(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>,
    host: &CommandHost<'_>,
    wait_resume: bool,
) -> anyhow::Result<()> {
    let (events, mut rx) = broadcast::channel::<RuntimeEvent>(1024);
    let cancel = CancellationToken::new();
    let started = std::time::Instant::now();
    let profile = sandbox_profile(state.profile);
    let trusted = state.trusted;
    let tx = host.approval_tx.clone();
    let operation_events = events.clone();
    let operation_cancel = cancel.clone();
    let cwd = host.cwd;
    let model = host.model;
    let provider_id = host.provider_id;
    state.busy = true;

    let operation = async move {
        let mut output = Vec::new();
        let run = crate::harness_cmd::ResumeRun {
            profile,
            interactivity: Interactivity::Interactive,
            trusted,
            approver: move || Box::new(TuiApprover { tx: tx.clone() }) as Box<dyn Approver>,
        };
        if wait_resume {
            crate::harness_cmd::wait_resume_with_events(
                cwd,
                model,
                provider_id,
                run,
                &operation_events,
                &operation_cancel,
                &mut output,
            )
            .await?;
        } else {
            crate::harness_cmd::resume_with_events(
                cwd,
                model,
                provider_id,
                run,
                &operation_events,
                &operation_cancel,
                &mut output,
            )
            .await?;
        }
        Ok(String::from_utf8_lossy(&output).into_owned())
    };

    let summary = drive_runtime_operation(
        terminal,
        state,
        approval_rx,
        &mut rx,
        &cancel,
        started,
        None,
        operation,
    )
    .await;
    state.busy = false;
    let summary = summary?;
    let summary = summary.trim();
    if !summary.is_empty() {
        state.apply(UiEvent::Notice(summary.to_string()));
    }
    Ok(())
}

async fn run_turn(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    runtime: &mut SessionRuntime,
    approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>,
    prompt: &str,
) -> anyhow::Result<()> {
    let (events, mut rx) = broadcast::channel::<RuntimeEvent>(1024);
    let cancel = CancellationToken::new();
    let started = std::time::Instant::now();
    // Input submitted while the turn runs becomes steering: admitted at the
    // next safe provider-turn boundary instead of being swallowed.
    let steer = runtime.steer_queue();
    let turn = async {
        let _ = runtime.run_turn(prompt, &events, &cancel).await;
        Ok(())
    };
    drive_runtime_operation(
        terminal,
        state,
        approval_rx,
        &mut rx,
        &cancel,
        started,
        Some(&steer),
        turn,
    )
    .await
}

#[allow(clippy::too_many_arguments)] // the REPL event pump genuinely threads these
async fn drive_runtime_operation<F, T>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>,
    rx: &mut broadcast::Receiver<RuntimeEvent>,
    cancel: &CancellationToken,
    started: std::time::Instant,
    steer: Option<&localpilot_harness::SteerQueue>,
    operation: F,
) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    tokio::pin!(operation);

    // The reply channel for an approval the user has not yet answered.
    let mut pending: Option<oneshot::Sender<bool>> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let value = loop {
        tokio::select! {
            biased;
            _ = tick.tick() => {
                state.spinner = state.spinner.wrapping_add(1);
                state.working_secs = started.elapsed().as_secs();
                // Process a bounded batch so held keys and pasted text remain
                // responsive without starving model events indefinitely.
                for _ in 0..64 {
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                    pending = resolve_event(state, pending, event::read()?, cancel, steer);
                }
                terminal.draw(|frame| render(frame, state))?;
            }
            result = &mut operation => {
                // Drain any events still buffered so a fast response is not lost
                // when the turn future completes in the same poll. Continue past
                // Lagged errors: the receiver advances to the oldest available
                // message, so calling try_recv again still returns events.
                loop {
                    match rx.try_recv() {
                        Ok(event) => {
                            if let Some(ui) = map_event(event, started.elapsed().as_secs_f64()) {
                                state.apply(ui);
                            }
                        }
                        Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
                state.apply(UiEvent::TurnComplete);
                break result?;
            }
            Some(call) = approval_rx.recv() => {
                state.apply(UiEvent::ApprovalRequested(call.request));
                pending = Some(call.reply);
            }
            received = rx.recv() => {
                match received {
                    Ok(event) => {
                        if let Some(ui) = map_event(event, started.elapsed().as_secs_f64()) {
                            state.apply(ui);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {}
                }
            }
        }
    };
    terminal.draw(|frame| render(frame, state))?;
    Ok(value)
}

/// Apply a terminal event received mid-turn. Approval dialogs capture their
/// decision keys; otherwise Ctrl-C cancels while ordinary editing and paste
/// events continue updating the next prompt.
fn resolve_event(
    state: &mut AppState,
    pending: Option<oneshot::Sender<bool>>,
    event: Event,
    cancel: &CancellationToken,
    steer: Option<&localpilot_harness::SteerQueue>,
) -> Option<oneshot::Sender<bool>> {
    if let Some(reply) = pending {
        let Event::Key(key) = event else {
            return Some(reply);
        };
        if !is_key_action(key) {
            return Some(reply);
        }
        if is_cancel(key) {
            let _ = reply.send(false);
            state.apply(UiEvent::ApprovalResolved);
            cancel.cancel();
            return None;
        }
        let decision = match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => Some(true),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
            _ => None,
        };
        match decision {
            Some(answer) => {
                let _ = reply.send(answer);
                state.apply(UiEvent::ApprovalResolved);
                None
            }
            None => Some(reply),
        }
    } else {
        match event {
            Event::Key(key) if is_key_action(key) => {
                if is_cancel(key) {
                    cancel.cancel();
                } else if is_newline(key, &state.input) {
                    state.insert_input_newline();
                } else if is_submit(key, &state.input) {
                    // Submitting while a turn runs queues steering input,
                    // admitted at the next safe provider-turn boundary.
                    if let Some(steer) = steer {
                        if !state.input.trim().is_empty() {
                            let shown = std::mem::take(&mut state.input);
                            state.input_cursor = 0;
                            let prompt = state.expand_pastes(&shown);
                            state.pastes.clear();
                            steer.push(prompt);
                            state.apply(UiEvent::UserMessage(shown));
                            state.apply(UiEvent::Notice(
                                "steering queued for the next safe boundary".to_string(),
                            ));
                        }
                    }
                } else if !matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                    if let Some(mapped) = map_key(key) {
                        handle_input(state, AppInput::Key(mapped));
                    }
                }
            }
            Event::Paste(text) => insert_paste(state, text),
            Event::Mouse(mouse) => {
                if let Some(mapped) = map_mouse(mouse.kind) {
                    handle_input(state, AppInput::Key(mapped));
                }
            }
            _ => {}
        }
        None
    }
}

fn insert_paste(state: &mut AppState, text: String) {
    if text.lines().count() >= 4 || text.len() > 400 {
        let placeholder = state.register_paste(text);
        state.insert_input(&placeholder);
    } else {
        state.insert_input(&text);
    }
}

fn map_key(key: KeyEvent) -> Option<Key> {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Key::CtrlC),
        KeyCode::Char(c) => Some(Key::Char(c)),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Backspace => Some(Key::Backspace),
        KeyCode::Delete => Some(Key::Delete),
        KeyCode::Esc => Some(Key::Esc),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        KeyCode::Left => Some(Key::Left),
        KeyCode::Right => Some(Key::Right),
        KeyCode::Home => Some(Key::Home),
        KeyCode::End => Some(Key::End),
        KeyCode::PageUp => Some(Key::PageUp),
        KeyCode::PageDown => Some(Key::PageDown),
        _ => None,
    }
}

fn map_mouse(kind: MouseEventKind) -> Option<Key> {
    match kind {
        MouseEventKind::ScrollUp => Some(Key::ScrollUp),
        MouseEventKind::ScrollDown => Some(Key::ScrollDown),
        _ => None,
    }
}

fn map_event(event: RuntimeEvent, elapsed_secs: f64) -> Option<UiEvent> {
    match event {
        RuntimeEvent::Text(text) => Some(UiEvent::TextDelta(text)),
        RuntimeEvent::Reasoning(text) => Some(UiEvent::ReasoningDelta(text)),
        RuntimeEvent::ToolStarted { id, name } => Some(UiEvent::ToolStarted { id, name }),
        RuntimeEvent::ToolFinished {
            id,
            name,
            is_error,
            output,
        } => Some(UiEvent::ToolFinished {
            id,
            name,
            is_error,
            output,
        }),
        RuntimeEvent::Usage(usage) => Some(UiEvent::Usage {
            tokens_in: usage.input_tokens,
            tokens_out: usage.output_tokens,
            tokens_per_sec: if elapsed_secs > 0.0 {
                usage.output_tokens as f64 / elapsed_secs
            } else {
                0.0
            },
        }),
        RuntimeEvent::ContextUsage { used, limit } => Some(UiEvent::ContextUsage {
            context_used: used,
            context_limit: limit,
        }),
        RuntimeEvent::QuotaPaused { reset } => Some(UiEvent::QuotaPaused { reset }),
        // Surface provider warnings/errors in the transcript so a failed turn is
        // visible instead of silently producing no response.
        RuntimeEvent::Warning(message) => Some(UiEvent::Notice(message)),
        // Surface the recovery outcome after a bad turn.
        RuntimeEvent::Recovery { health } => match health {
            ModelHealth::Recovering => Some(UiEvent::RecoveryNotice(
                "recovering from a bad response…".to_string(),
            )),
            ModelHealth::Degraded => Some(UiEvent::RecoveryNotice(
                "model marked degraded after repeated bad output — try a stronger \
                 model/quant or check the endpoint"
                    .to_string(),
            )),
            ModelHealth::Healthy => None,
        },
        RuntimeEvent::Plan(steps) => Some(UiEvent::PlanUpdated(
            steps
                .into_iter()
                .map(|step| PlanItem {
                    title: step.title,
                    status: step.status,
                })
                .collect(),
        )),
        _ => None,
    }
}

/// Render the session's durable event log as an indented tree of lifecycle
/// landmarks: opens, turns, steps, branch closures, and forks.
fn render_session_tree(events: &[localpilot_store::SessionEvent]) -> Vec<String> {
    use localpilot_store::SessionEventKind as Kind;
    let mut lines = Vec::new();
    let mut in_step = false;
    for event in events {
        match &event.kind {
            Kind::SessionOpened { reason } => {
                in_step = false;
                lines.push(format!("* session opened ({reason:?})").to_lowercase());
            }
            Kind::StepStarted {
                number,
                description,
            } => {
                in_step = true;
                lines.push(format!("* step {number}: {description}"));
            }
            Kind::StepCompleted {
                number, attempts, ..
            } => {
                in_step = false;
                lines.push(format!("* step {number} completed ({attempts} attempt(s))"));
            }
            Kind::BranchClosed { summary } => {
                lines.push(format!("  x branch closed: {}", summary.title));
            }
            Kind::BranchForked { .. } => {
                lines.push("  > forked from an earlier point".to_string());
            }
            Kind::TurnStarted { model } => {
                let indent = if in_step { "    " } else { "  " };
                lines.push(format!("{indent}- turn ({model})"));
            }
            Kind::Cancelled => lines.push("  ! cancelled".to_string()),
            _ => {}
        }
    }
    if lines.is_empty() {
        lines.push("event log is empty".to_string());
    }
    lines
}

fn ui_profile(profile: Profile) -> UiProfile {
    match profile {
        Profile::Default => UiProfile::Default,
        Profile::Relaxed => UiProfile::Relaxed,
        Profile::Bypass => UiProfile::Bypass,
    }
}

fn sandbox_profile(profile: UiProfile) -> Profile {
    match profile {
        UiProfile::Default => Profile::Default,
        UiProfile::Relaxed => Profile::Relaxed,
        UiProfile::Bypass => Profile::Bypass,
    }
}

/// Best-effort context window for `model` from the provider's own model
/// listing, when the provider speaks the OpenAI-compatible protocol and a base
/// URL is known. Silent on failure: discovery is metadata, not a gate.
async fn discovered_window(
    config: &localpilot_config::Config,
    provider_id: Option<&str>,
    model: &str,
) -> Option<u64> {
    let id = provider_id.unwrap_or(&config.provider.default);
    let entry = config.providers.get(id)?;
    if entry.kind == "anthropic" {
        return None;
    }
    let base_url = entry.base_url.clone().or_else(|| {
        std::env::var("OPENAI_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
    })?;
    let credential = config.resolve_credential(id);
    let models = localpilot_llm::discover_models(&base_url, credential.as_ref())
        .await
        .ok()?;
    models
        .into_iter()
        .find(|m| m.id == model)
        .and_then(|m| m.context_window)
}

fn enter_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    // Ask the terminal to report keys unambiguously (the kitty keyboard
    // protocol), so modified keys like Alt+Enter / Shift+Enter reach the app.
    // Pushed unconditionally (as Codex does): a terminal that doesn't support it
    // ignores the sequence, and the support query can false-negative. The flags
    // are popped on exit.
    // REPORT_EVENT_TYPES is required alongside DISAMBIGUATE_ESCAPE_CODES so that
    // release/repeat events carry an explicit kind in the CSI sequence. Without it
    // Windows Terminal emits both a legacy press event and a Kitty-encoded event
    // for the same keypress, both parsed as KeyEventKind::Press, doubling input.
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
        )
    );
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn leave_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    terminal::disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste,
        terminal::LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}
