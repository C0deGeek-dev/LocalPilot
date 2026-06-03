//! `unshackled chat` — the interactive terminal REPL.
//!
//! This is the terminal driver: it maps real crossterm key events into the
//! backend-agnostic `unshackled-tui` core, runs a session turn per submission,
//! and forwards the runtime event stream into the UI. It is the un-testable
//! terminal-I/O edge; the rendering and input logic it drives are unit-tested in
//! `unshackled-tui`.

use std::future::Future;
use std::io::{self, Stdout};
use std::pin::Pin;
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use unshackled_config::{CliOverrides, ConfigPaths};
use unshackled_harness::{ModelHealth, RuntimeEvent, SessionConfig, SessionRuntime};
use unshackled_llm::ProviderRegistry;
use unshackled_recovery::{RecoveryBudget, RecoveryEngine};
use unshackled_sandbox::{
    Approver, Effect, Interactivity, PermissionEngine, PermissionRequest, Profile, Workspace,
};
use unshackled_store::Store;
use unshackled_tui::{
    handle_input, render, AppInput, AppState, ApprovalRequest, Header, Key, Mode, PlanItem,
    Profile as UiProfile, UiEvent,
};

/// A pending approval handed from the [`TuiApprover`] (running inside the turn)
/// to the event loop, which raises the modal and replies with the user's answer.
struct ApprovalCall {
    request: ApprovalRequest,
    reply: oneshot::Sender<bool>,
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
    let config = unshackled_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())?;
    let model = model
        .map(str::to_string)
        .or_else(|| config.resolve_model(provider_id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no model: pass --model, or set a default in .unshackled.toml \
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

    // Ask-gated actions suspend the turn and prompt in the TUI; the user's
    // y/n answer flows back through this channel to the permission engine.
    let (approval_tx, mut approval_rx) = mpsc::unbounded_channel::<ApprovalCall>();
    let mut runtime = SessionRuntime::new(
        provider,
        crate::mcp::McpTools::load(&config).await.registry(),
        PermissionEngine::new(profile, Vec::new()),
        Box::new(TuiApprover { tx: approval_tx }),
        Store::open(&cwd),
        Workspace::new(&cwd)?,
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            model: model.to_string(),
            interactivity: Interactivity::Interactive,
            trusted: profile == Profile::Bypass,
            ..SessionConfig::default()
        },
        Vec::new(),
    );

    let header = Header {
        version: env!("UNSHACKLED_VERSION").to_string(),
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

    let session_id = runtime.session_id();
    let mut terminal = enter_terminal()?;
    let result = event_loop(
        &mut terminal,
        &mut state,
        &mut runtime,
        &mut approval_rx,
        &cwd,
    )
    .await;
    leave_terminal(&mut terminal)?;
    // Learn from the finished session (no-op without the learning feature).
    crate::context_inject::close_out(&cwd, session_id);
    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    runtime: &mut SessionRuntime,
    approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>,
    cwd: &std::path::Path,
) -> anyhow::Result<()> {
    loop {
        terminal.draw(|frame| render(frame, state))?;
        if state.should_quit {
            return Ok(());
        }

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                // Only key *presses*: Windows consoles also emit Release/Repeat
                // events, which would otherwise double every character.
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if is_submit(key, &state.input) {
                        let prompt = std::mem::take(&mut state.input);
                        // Seed relevant accepted memory for this prompt (no-op
                        // without the learning feature or when nothing matches).
                        crate::context_inject::seed(cwd, runtime, &prompt);
                        state.apply(UiEvent::UserMessage(prompt.clone()));
                        state.busy = true;
                        let outcome =
                            run_turn(terminal, state, runtime, approval_rx, &prompt).await;
                        state.busy = false;
                        outcome?;
                    } else if let Some(mapped) = map_key(key) {
                        handle_input(state, AppInput::Key(mapped));
                    }
                }
                // Bracketed paste: insert the pasted text into the input.
                Event::Paste(text) => state.input.push_str(&text),
                _ => {}
            }
        }
    }
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
    let turn = runtime.run_turn(prompt, &events, &cancel);
    tokio::pin!(turn);

    // The reply channel for an approval the user has not yet answered.
    let mut pending: Option<oneshot::Sender<bool>> = None;

    loop {
        terminal.draw(|frame| render(frame, state))?;
        tokio::select! {
            _ = &mut turn => {
                // Drain any events still buffered so a fast response is not lost
                // when the turn future completes in the same poll.
                while let Ok(event) = rx.try_recv() {
                    if let Some(ui) = map_event(event, started.elapsed().as_secs_f64()) {
                        state.apply(ui);
                    }
                }
                state.apply(UiEvent::TurnComplete);
                break;
            }
            received = rx.recv() => {
                if let Ok(event) = received {
                    if let Some(ui) = map_event(event, started.elapsed().as_secs_f64()) {
                        state.apply(ui);
                    }
                }
            }
            Some(call) = approval_rx.recv() => {
                state.apply(UiEvent::ApprovalRequested(call.request));
                pending = Some(call.reply);
            }
            // Tick: advance the working indicator and poll the keyboard (answer an
            // open modal, or cancel the turn with Ctrl-C).
            _ = tokio::time::sleep(Duration::from_millis(80)) => {
                state.spinner = state.spinner.wrapping_add(1);
                state.working_secs = started.elapsed().as_secs();
                if event::poll(Duration::ZERO)? {
                    if let Event::Key(key) = event::read()? {
                        if key.kind == KeyEventKind::Press {
                            pending = resolve_key(state, pending, key, &cancel);
                        }
                    }
                }
            }
        }
    }
    terminal.draw(|frame| render(frame, state))?;
    Ok(())
}

/// Apply a keypress received mid-turn. When a modal is open, `y`/Enter approves
/// and `n`/Esc denies; other keys are ignored so a stray press cannot answer.
/// With no modal open, Ctrl-C cancels the turn.
fn resolve_key(
    state: &mut AppState,
    pending: Option<oneshot::Sender<bool>>,
    key: KeyEvent,
    cancel: &CancellationToken,
) -> Option<oneshot::Sender<bool>> {
    if let Some(reply) = pending {
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
        if is_cancel(key) {
            cancel.cancel();
        }
        None
    }
}

fn is_cancel(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL))
}

fn is_submit(key: KeyEvent, input: &str) -> bool {
    // Only a plain Enter submits; any modifier (or Ctrl+J) makes a newline.
    key.code == KeyCode::Enter
        && key.modifiers.is_empty()
        && !input.trim().is_empty()
        && !input.trim_start().starts_with('/')
}

fn map_key(key: KeyEvent) -> Option<Key> {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Key::CtrlC),
        // Newline in the input. Ctrl+J is the reliable binding — terminals often
        // capture Alt+Enter (Windows Terminal toggles fullscreen) — but a
        // modified Enter (Alt/Shift/Ctrl) is accepted too where it gets through.
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(Key::Char('\n'))
        }
        KeyCode::Enter if !key.modifiers.is_empty() => Some(Key::Char('\n')),
        KeyCode::Char(c) => Some(Key::Char(c)),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Backspace => Some(Key::Backspace),
        KeyCode::Esc => Some(Key::Esc),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        _ => None,
    }
}

fn map_event(event: RuntimeEvent, elapsed_secs: f64) -> Option<UiEvent> {
    match event {
        RuntimeEvent::Text(text) => Some(UiEvent::TextDelta(text)),
        RuntimeEvent::Reasoning(text) => Some(UiEvent::ReasoningDelta(text)),
        RuntimeEvent::Usage(usage) => Some(UiEvent::Usage {
            tokens_in: usage.input_tokens,
            tokens_out: usage.output_tokens,
            tokens_per_sec: if elapsed_secs > 0.0 {
                usage.output_tokens as f64 / elapsed_secs
            } else {
                0.0
            },
        }),
        RuntimeEvent::QuotaPaused { reset } => Some(UiEvent::QuotaPaused { reset }),
        // Surface provider warnings/errors in the transcript so a failed turn is
        // visible instead of silently producing no response.
        RuntimeEvent::Warning(message) => Some(UiEvent::Notice(message)),
        // Surface the recovery outcome after a bad turn.
        RuntimeEvent::Recovery { health } => match health {
            ModelHealth::Recovering => Some(UiEvent::Notice(
                "recovering from a bad response…".to_string(),
            )),
            ModelHealth::Degraded => Some(UiEvent::Notice(
                "model marked degraded after repeated bad output".to_string(),
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

fn ui_profile(profile: Profile) -> UiProfile {
    match profile {
        Profile::Default => UiProfile::Default,
        Profile::Relaxed => UiProfile::Relaxed,
        Profile::Bypass => UiProfile::Bypass,
    }
}

fn enter_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, EnableBracketedPaste)?;
    // Ask the terminal to report keys unambiguously (the kitty keyboard
    // protocol), so modified keys like Alt+Enter / Shift+Enter reach the app.
    // Pushed unconditionally (as Codex does): a terminal that doesn't support it
    // ignores the sequence, and the support query can false-negative. The flags
    // are popped on exit.
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn leave_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    terminal::disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        terminal::LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}
