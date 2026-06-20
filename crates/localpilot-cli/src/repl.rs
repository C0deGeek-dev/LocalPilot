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
use std::time::{Duration, Instant};

use crossterm::cursor::MoveTo;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
    banner_text, handle_input, history_block_text, parse_slash, render, AppInput, AppState,
    ApprovalRequest, BackgroundCommand, BackgroundProcess, Header, IngestAction, Key, Mode,
    PlanItem, Profile as UiProfile, SlashAction, TrustPrompt, UiEvent,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::key_input::{
    is_cancel, is_key_action, is_newline, is_submit, is_unbracketed_paste_newline_key,
    may_be_unbracketed_paste_key, PasteAction, PasteBurst,
};

/// Fixed height of the inline live region. The region reserves a constant, modest
/// band and is **not** re-initialised per frame: the activity tail, composer, and
/// status line render within it (each already caps and scrolls internally), and
/// only an actual terminal-dimension change re-inits the viewport. The previous
/// per-content re-init tore the viewport down on every height change, which dropped
/// freshly committed history from native scrollback before it had scrolled
/// off-screen. Tunable: a larger band shows more in-progress output at once but
/// leaves a larger blank gap above the composer when idle.
const LIVE_REGION_HEIGHT: u16 = 8;

/// Blank rows between the launch banner and the composer at startup.
const BANNER_GAP_ROWS: u16 = 2;

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

    // Best-effort retention so `.localpilot/` cannot grow without bound. Errors
    // are ignored — cleanup must never block starting a chat — and it runs before
    // the live region is drawn.
    if config.storage.auto_prune {
        let policy = crate::session_cmd::retention_policy(&config.storage, None, None);
        if !policy.is_unbounded() {
            let _ = Store::open(&cwd).prune(policy, crate::session_cmd::now_unix(), false);
        }
    }

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
    let mut registry = crate::mcp::McpTools::load(&config).await.registry();
    let broker = crate::mcp::install_broker(&config.tools, &mut registry);
    let mut runtime = SessionRuntime::new(
        provider,
        registry,
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
            compaction_mode: compaction_mode(config.compaction.mode),
            summarizer_tuning: localpilot_harness::SummarizerTuning::from_config(
                &config.compaction,
            ),
            tool_call_budget: config.harness.tool_call_budget,
            tool_call_budget_max: config.harness.tool_call_budget_max,
            rules: config.harness.rules.clone(),
            enforce_claim_gate: config.harness.claim_gate.is_enabled(),
            tool_marker_enabled: config.tools.marker,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    runtime.set_broker(broker);
    localpilot_harness::register_project_analysis_context(
        &cwd,
        config.context.project_analysis,
        config.docs.lookup_policy,
        &mut runtime,
    );
    // Relevant accepted LocalMind memory is contributed per turn through the
    // context-hook fabric; ingested folder knowledge is pulled on demand via the
    // knowledge_search tool rather than seeded here.
    localpilot_localmind::register_context_hook(&cwd, &mut runtime);

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
    // Seed the `@`-mention file list; refreshed after each turn (files may change).
    state.set_workspace_files(workspace_files(&cwd));

    // Build the project knowledge index in the background on first use, so
    // `knowledge_search` has data without the first turn paying for a full walk.
    // Interactive REPL only (non-interactive paths never create project files),
    // and only once the workspace is trusted, so we never write `.localmind`
    // before the user has consented. Detached: the ingest is bounded by its own
    // budgets and writes its index atomically at the end. `session_open_mode`
    // decides what to do — a first build, a resume of an interrupted run, or a
    // staleness refresh when a completed index's sources changed — and returns
    // nothing when ingest is disabled or the index is already current.
    if state.trusted {
        if let Some(mode) = localpilot_localmind::session_open_mode(&cwd, &config.ingest) {
            let ingest_root = cwd.clone();
            let ingest_config = config.ingest.clone();
            tokio::task::spawn_blocking(move || {
                let _ = localpilot_localmind::ingest_run(&ingest_root, &ingest_config, mode);
            });
        }
    }

    let session_id = runtime.session_id();
    let mut terminal = enter_terminal()?;
    // Print the launch banner once and seat the live region at the screen bottom.
    launch_banner(&mut terminal, banner_text(&state.header))?;
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
    let mut paste_burst = PasteBurst::default();
    loop {
        // Commit a paste once its key-event stream has gone idle (it may have been
        // absorbed without a final flush because a trailing event looked like more
        // input). Time-based, so a momentary gap mid-paste never commits a half.
        if let Some(text) = paste_burst.flush_if_idle(Instant::now()) {
            insert_paste(state, text);
        }
        draw_ui(terminal, state)?;
        if state.should_quit {
            return Ok(());
        }
        // Poll briefly while a burst is pending so we re-check the idle flush
        // promptly; idle at the normal cadence otherwise.
        let timeout = if paste_burst.has_pending() {
            Duration::from_millis(20)
        } else {
            Duration::from_millis(100)
        };
        if !event::poll(timeout)? {
            continue;
        }
        // Drain all currently-buffered events in one pass before redrawing. A
        // terminal that delivers no bracketed paste sends one key event per
        // pasted character; redrawing per character made a large paste crawl.
        for _ in 0..4096 {
            let mut submitted = false;
            match event::read()? {
                Event::Key(key) if is_key_action(key) => {
                    let buffered_after = buffered_after_key(key)?;
                    if state.trust.is_some() {
                        // While the trust gate is up, route keys to it and persist
                        // the decision when the folder is trusted.
                        if let Some(mapped) = map_key(key) {
                            handle_input(state, AppInput::Key(mapped));
                        }
                        if state.trusted {
                            crate::trust::remember(host.cwd);
                        }
                    } else if handle_paste_burst(state, &mut paste_burst, key, buffered_after) {
                    } else if slash_picker_exact_submit(state, key) {
                        state.close_slash_picker();
                        submit_current_input(terminal, state, runtime, approval_rx, &host).await?;
                        submitted = true;
                    } else if slash_picker_captures(state, key) || file_picker_captures(state, key)
                    {
                        if let Some(mapped) = map_key(key) {
                            handle_input(state, AppInput::Key(mapped));
                        }
                    } else if is_newline(key, &state.input) {
                        state.insert_input_newline();
                    } else if is_submit(key, &state.input) {
                        submit_current_input(terminal, state, runtime, approval_rx, &host).await?;
                        submitted = true;
                    } else if let Some(mapped) = map_key(key) {
                        handle_input(state, AppInput::Key(mapped));
                    }
                }
                // Bracketed paste: insert small pastes inline, but collapse large
                // ones to a placeholder so the input line stays readable.
                Event::Paste(text) if state.trust.is_none() => insert_paste(state, text),
                _ => {}
            }
            if submitted || state.should_quit {
                break;
            }
            // Keep draining while events remain so a paste is absorbed in one pass;
            // committing it is left to the idle flush at the loop top.
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }
}

async fn submit_current_input(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    runtime: &mut SessionRuntime,
    approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>,
    host: &CommandHost<'_>,
) -> anyhow::Result<()> {
    // Expand collapsed pastes for the model, but keep the compact form in the
    // transcript.
    let (shown, prompt) = state.take_input_for_submit();
    if prompt.trim().is_empty() {
        return Ok(());
    }
    let result = if let Some(action) = parse_slash(&prompt) {
        run_slash(terminal, state, runtime, approval_rx, host, action).await
    } else {
        state.apply(UiEvent::UserMessage(shown));
        state.busy = true;
        let outcome = run_turn(terminal, state, runtime, approval_rx, &prompt).await;
        state.busy = false;
        // The turn may have created or removed files; refresh the @-mention list.
        state.set_workspace_files(workspace_files(host.cwd));
        outcome
    };
    // A turn may have started a background process and a `/bg`/`/new` may have
    // changed the set; keep the status-line indicator current either way.
    refresh_background(state, runtime);
    result
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
        SlashAction::LoadSession(id) => load_session_from_input(state, runtime, &id),
        SlashAction::ContinueSession(id) => continue_session(state, runtime, id.as_deref()),
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
        SlashAction::Compact { force } => {
            let summary = if force {
                runtime.compact_conversation_force().await
            } else {
                runtime.compact_conversation().await
            };
            state.apply(UiEvent::ContextUsage {
                context_used: summary.context_used,
                context_limit: summary.context_limit,
            });
            let notice = if summary.compacted {
                let fallback = summary
                    .fallback_reason
                    .map(|reason| format!("; fallback: {reason}"))
                    .unwrap_or_default();
                format!(
                    "compacted conversation history using {}; context {}/{}{}",
                    harness_compaction_mode_label(summary.used_mode),
                    summary.context_used,
                    summary.context_limit,
                    fallback
                )
            } else if force {
                format!(
                    "nothing left to compact using {}; context {}/{}",
                    harness_compaction_mode_label(summary.requested_mode),
                    summary.context_used,
                    summary.context_limit
                )
            } else {
                format!(
                    "conversation already compact enough using {}; context {}/{}",
                    harness_compaction_mode_label(summary.requested_mode),
                    summary.context_used,
                    summary.context_limit
                )
            };
            state.apply(UiEvent::Notice(notice));
        }
        SlashAction::HarnessResume => {
            state.mode = Mode::Harness;
            state.apply(UiEvent::Notice("running harness resume".to_string()));
            run_harness_command(terminal, state, approval_rx, host, false).await?;
        }
        SlashAction::WaitResume => {
            state.mode = Mode::Harness;
            state.apply(UiEvent::Notice("checking paused harness run".to_string()));
            run_harness_command(terminal, state, approval_rx, host, true).await?;
        }
        // The walk-and-chunk actions can run for many seconds; drive them through
        // a spinner/progress loader so the UI never just freezes. The rest are
        // cheap state reads/writes and stay synchronous.
        SlashAction::Ingest(IngestAction::Run) => {
            run_ingest_progress(
                terminal,
                state,
                host.cwd,
                localpilot_localmind::RunMode::Full,
                false,
            )
            .await?;
        }
        SlashAction::Ingest(IngestAction::Refresh) => {
            run_ingest_progress(
                terminal,
                state,
                host.cwd,
                localpilot_localmind::RunMode::Refresh,
                false,
            )
            .await?;
        }
        SlashAction::Ingest(IngestAction::Resume) => {
            run_ingest_progress(
                terminal,
                state,
                host.cwd,
                localpilot_localmind::RunMode::Refresh,
                true,
            )
            .await?;
        }
        SlashAction::Ingest(action) => run_ingest_slash(state, host.cwd, action),
        SlashAction::Knowledge(query) => {
            let mut output = Vec::new();
            let result = crate::ingest_cmd::knowledge_search(host.cwd, &query, &mut output);
            apply_command_result(state, output, result);
        }
        SlashAction::ContextBuild(task) => {
            let mut output = Vec::new();
            let result = crate::ingest_cmd::knowledge_pack(host.cwd, &task, &mut output);
            apply_command_result(state, output, result);
        }
        SlashAction::Background(command) => apply_background_command(state, runtime, command),
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

/// List or stop the session's background processes, posting the result as
/// notices. Stopping is synchronous, so it runs directly off the input loop.
fn apply_background_command(
    state: &mut AppState,
    runtime: &SessionRuntime,
    command: BackgroundCommand,
) {
    match command {
        BackgroundCommand::List => {
            let processes = runtime.background_processes();
            if processes.is_empty() {
                state.apply(UiEvent::Notice("no background processes".to_string()));
            } else {
                state.apply(UiEvent::Notice(
                    "background processes (stop with /bg stop <id> or /bg stop all):".to_string(),
                ));
                for process in processes {
                    let status = if process.alive { "running" } else { "exited" };
                    state.apply(UiEvent::Notice(format!(
                        "  {} [{}] {}s · {}",
                        process.id, status, process.age_secs, process.command
                    )));
                }
            }
        }
        BackgroundCommand::Stop(id) => {
            if runtime.stop_background_process(&id) {
                state.apply(UiEvent::Notice(format!("stopped background process {id}")));
            } else {
                state.apply(UiEvent::Notice(format!("no background process {id}")));
            }
        }
        BackgroundCommand::StopAll => {
            let count = runtime.background_processes().len();
            runtime.stop_all_background_processes();
            state.apply(UiEvent::Notice(format!(
                "stopped {count} background process(es)"
            )));
        }
    }
}

/// Push the current background-process set into the UI so the status-line
/// indicator and `/bg` listing stay in sync after a turn or a `/bg` command.
fn refresh_background(state: &mut AppState, runtime: &SessionRuntime) {
    let processes = runtime
        .background_processes()
        .into_iter()
        .map(|process| BackgroundProcess {
            id: process.id,
            command: process.command,
            alive: process.alive,
        })
        .collect();
    state.apply(UiEvent::BackgroundProcesses(processes));
}

fn continue_session(state: &mut AppState, runtime: &mut SessionRuntime, id: Option<&str>) {
    if let Some(id) = id {
        load_session_from_input(state, runtime, id);
        return;
    }

    let current = runtime.session_id();
    let session = match runtime.store().list_sessions() {
        Ok(mut sessions) => {
            sessions.sort_by(|a, b| b.updated_unix.cmp(&a.updated_unix));
            sessions
                .into_iter()
                .find(|entry| entry.id != current)
                .map(|entry| entry.id)
        }
        Err(error) => {
            state.apply(UiEvent::Notice(format!(
                "session index unreadable: {error}"
            )));
            return;
        }
    };

    match session {
        Some(session) => load_session_id(state, runtime, session),
        None => state.apply(UiEvent::Notice(
            "no previous session in this workspace".to_string(),
        )),
    }
}

fn load_session_from_input(state: &mut AppState, runtime: &mut SessionRuntime, id: &str) {
    match id.parse::<localpilot_core::SessionId>() {
        Ok(session) => load_session_id(state, runtime, session),
        Err(_) => {
            state.apply(UiEvent::Notice(format!("not a session id: {id}")));
        }
    }
}

fn load_session_id(
    state: &mut AppState,
    runtime: &mut SessionRuntime,
    session: localpilot_core::SessionId,
) {
    match runtime.load_session(session) {
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
    }
}

/// Handle the synchronous, fast `/ingest` actions (state reads/writes that
/// return promptly). The walking actions — `run`, `refresh`, `resume` — are
/// intercepted in [`run_slash`] and driven through [`run_ingest_progress`] with a
/// loader instead; the arms for them here are a correct fallback if this is ever
/// called directly.
fn run_ingest_slash(state: &mut AppState, cwd: &std::path::Path, action: IngestAction) {
    let mut output = Vec::new();
    let result = match action {
        IngestAction::Run => {
            crate::ingest_cmd::run(cwd, localpilot_localmind::RunMode::Full, &mut output)
        }
        IngestAction::Preview => crate::ingest_cmd::preview(cwd, &mut output),
        IngestAction::Status => crate::ingest_cmd::status(cwd, &mut output),
        IngestAction::Pause => {
            crate::ingest_cmd::control(cwd, crate::ingest_cmd::ControlAction::Pause, &mut output)
        }
        IngestAction::Resume => crate::ingest_cmd::resume(cwd, &mut output),
        IngestAction::Cancel => {
            crate::ingest_cmd::control(cwd, crate::ingest_cmd::ControlAction::Cancel, &mut output)
        }
        IngestAction::Refresh => {
            crate::ingest_cmd::run(cwd, localpilot_localmind::RunMode::Refresh, &mut output)
        }
        IngestAction::Rebuild => crate::ingest_cmd::rebuild(cwd, &mut output),
        IngestAction::Skipped => crate::ingest_cmd::skipped(cwd, &mut output),
        IngestAction::Include(path) => crate::ingest_cmd::rule(
            cwd,
            crate::ingest_cmd::RuleAction::Include,
            std::path::Path::new(&path),
            &mut output,
        ),
        IngestAction::Exclude(path) => crate::ingest_cmd::rule(
            cwd,
            crate::ingest_cmd::RuleAction::Exclude,
            std::path::Path::new(&path),
            &mut output,
        ),
        IngestAction::Forget(target) => crate::ingest_cmd::forget(cwd, &target, &mut output),
        IngestAction::Review => crate::ingest_cmd::review(cwd, &mut output),
        IngestAction::Promote(id) => crate::ingest_cmd::promote(cwd, &id, &mut output),
    };
    apply_command_result(state, output, result);
}

/// Run a folder-ingestion walk on a blocking task while keeping the TUI live:
/// the working spinner animates, stage milestones post as notices, and Ctrl-C
/// pauses the run (partial chunks are kept, so `/ingest resume` continues it).
/// Used for the long-running `run`/`refresh`/`resume` actions; the cheap ingest
/// actions stay on the synchronous path.
async fn run_ingest_progress(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    cwd: &std::path::Path,
    requested_mode: localpilot_localmind::RunMode,
    resume: bool,
) -> anyhow::Result<()> {
    use localpilot_localmind::{JobStatus, RunMode};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let config = match crate::ingest_cmd::load_ingest_config(cwd) {
        Ok(config) => config,
        Err(error) => {
            state.apply(UiEvent::Notice(format!("ingest config error: {error}")));
            return Ok(());
        }
    };

    // `resume` resolves the same decision the session-open trigger uses: resume an
    // interrupted job, rebuild, or report nothing-to-do.
    let mode = if resume {
        match localpilot_localmind::ingest_status(cwd) {
            Ok(Some(job)) => {
                let has_index = localpilot_localmind::has_chunk_index(cwd);
                match localpilot_localmind::planned_run_mode(Some(&job), has_index) {
                    Some(mode) => mode,
                    None => {
                        state.apply(UiEvent::Notice(
                            "ingest job already completed; run /ingest refresh to update"
                                .to_string(),
                        ));
                        return Ok(());
                    }
                }
            }
            Ok(None) => {
                state.apply(UiEvent::Notice("no ingest job to resume".to_string()));
                return Ok(());
            }
            Err(error) => {
                state.apply(UiEvent::Notice(format!(
                    "ingest status unreadable: {error}"
                )));
                return Ok(());
            }
        }
    } else {
        requested_mode
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_task = cancel.clone();
    let (tx, mut progress_rx) = mpsc::unbounded_channel::<localpilot_localmind::IngestProgress>();
    let root = cwd.to_path_buf();
    let mut handle = tokio::task::spawn_blocking(move || {
        localpilot_localmind::ingest_run_with_progress(
            &root,
            &config,
            mode,
            &|| cancel_task.load(Ordering::Relaxed),
            &mut |stage| {
                let _ = tx.send(stage);
            },
        )
    });

    let mode_label = match mode {
        RunMode::Full => "full",
        RunMode::Refresh => "refresh",
    };
    state.busy = true;
    state.apply(UiEvent::Notice(format!(
        "ingesting project knowledge ({mode_label})…"
    )));
    let started = std::time::Instant::now();
    let mut total = 0_u64;
    let mut parse_bucket = 0_u64;

    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let outcome = loop {
        tokio::select! {
            biased;
            _ = tick.tick() => {
                state.spinner = state.spinner.wrapping_add(1);
                state.working_secs = started.elapsed().as_secs();
                drain_ingest_progress(state, &mut progress_rx, &mut total, &mut parse_bucket);
                // Ctrl-C requests a pause; other keys are ignored while ingesting.
                for _ in 0..64 {
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                    if let Event::Key(key) = event::read()? {
                        if is_key_action(key) && is_cancel(key) && !cancel.load(Ordering::Relaxed) {
                            cancel.store(true, Ordering::Relaxed);
                            state.apply(UiEvent::Notice("cancelling ingestion…".to_string()));
                        }
                    }
                }
                draw_ui(terminal, state)?;
            }
            joined = &mut handle => break joined,
        }
    };
    // Drain any milestones queued after the last tick so the final stages show.
    drain_ingest_progress(state, &mut progress_rx, &mut total, &mut parse_bucket);
    state.busy = false;

    match outcome {
        Ok(Ok(summary)) => {
            let interrupted =
                matches!(summary.job.status, JobStatus::Paused | JobStatus::Cancelled);
            let status = match summary.job.status {
                JobStatus::Completed => "completed",
                JobStatus::Paused => "paused",
                JobStatus::Cancelled => "cancelled",
                JobStatus::Failed => "failed",
                JobStatus::Running => "running",
                JobStatus::Queued => "queued",
            };
            let suffix = if interrupted {
                " — resume with /ingest resume"
            } else {
                ""
            };
            state.apply(UiEvent::Notice(format!(
                "ingestion {status}: {} file(s), {} chunk(s){suffix}",
                summary.job.completed_files, summary.chunks_written
            )));
        }
        Ok(Err(error)) => {
            state.apply(UiEvent::Notice(format!("ingestion failed: {error}")));
        }
        Err(error) => {
            state.apply(UiEvent::Notice(format!("ingestion task error: {error}")));
        }
    }
    draw_ui(terminal, state)?;
    Ok(())
}

/// Drain queued ingestion progress into notices. Milestone stages post once;
/// per-file `Parsing` ticks are throttled to quarter marks so a large walk does
/// not flood the transcript. `total`/`bucket` carry the throttle state across
/// calls.
fn drain_ingest_progress(
    state: &mut AppState,
    rx: &mut mpsc::UnboundedReceiver<localpilot_localmind::IngestProgress>,
    total: &mut u64,
    bucket: &mut u64,
) {
    use localpilot_localmind::IngestProgress;
    while let Ok(stage) = rx.try_recv() {
        match stage {
            IngestProgress::Discovering => {
                state.apply(UiEvent::Notice("ingest: discovering files…".to_string()));
            }
            IngestProgress::Discovered {
                candidates,
                skipped,
            } => {
                *total = candidates;
                state.apply(UiEvent::Notice(format!(
                    "ingest: {candidates} file(s) to parse ({skipped} skipped)"
                )));
            }
            IngestProgress::Parsing {
                completed,
                total: count,
            } => {
                *total = count;
                if count > 0 && completed > 0 {
                    let quarter = completed.saturating_mul(4) / count;
                    if quarter > *bucket {
                        *bucket = quarter;
                        state.apply(UiEvent::Notice(format!(
                            "ingest: parsed {completed}/{count} file(s)"
                        )));
                    }
                }
            }
            IngestProgress::Indexing => {
                state.apply(UiEvent::Notice(
                    "ingest: indexing project context…".to_string(),
                ));
            }
            IngestProgress::Writing => {
                state.apply(UiEvent::Notice("ingest: writing index…".to_string()));
            }
            // The caller posts the final summary line from the run result.
            IngestProgress::Completed { .. } => {}
        }
    }
}

fn apply_command_result(state: &mut AppState, output: Vec<u8>, result: anyhow::Result<()>) {
    apply_command_output(state, output);
    if let Err(error) = result {
        state.apply(UiEvent::Notice(format!("command failed: {error}")));
    }
}

fn apply_command_output(state: &mut AppState, output: Vec<u8>) {
    let text = String::from_utf8_lossy(&output);
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        state.apply(UiEvent::Notice(line.to_string()));
    }
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
    let mut paste_burst = PasteBurst::default();
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
                    let event = event::read()?;
                    let buffered_after = match event {
                        Event::Key(key) if is_key_action(key) => buffered_after_key(key)?,
                        _ => false,
                    };
                    pending = resolve_event(
                        state,
                        pending,
                        event,
                        cancel,
                        steer,
                        &mut paste_burst,
                        buffered_after,
                    );
                }
                // Commit a paste once its event stream has gone idle (the 50ms tick
                // re-checks). Time-based, so a gap between batches never commits a
                // half-paste.
                if let Some(text) = paste_burst.flush_if_idle(Instant::now()) {
                    insert_paste(state, text);
                }
                draw_ui(terminal, state)?;
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
    draw_ui(terminal, state)?;
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
    paste_burst: &mut PasteBurst,
    buffered_after: bool,
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
                } else if handle_paste_burst(state, paste_burst, key, buffered_after) {
                } else if slash_picker_captures(state, key) || file_picker_captures(state, key) {
                    if let Some(mapped) = map_key(key) {
                        handle_input(state, AppInput::Key(mapped));
                    }
                } else if is_newline(key, &state.input) {
                    state.insert_input_newline();
                } else if is_submit(key, &state.input) {
                    if state.input.trim_start().starts_with('/') {
                        state.apply(UiEvent::Notice(
                            "slash commands run when the current turn is idle".to_string(),
                        ));
                        return None;
                    }
                    // Submitting while a turn runs queues steering input,
                    // admitted at the next safe provider-turn boundary.
                    if let Some(steer) = steer {
                        if !state.input.trim().is_empty() {
                            let (shown, prompt) = state.take_input_for_submit();
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
            _ => {}
        }
        None
    }
}

fn insert_paste(state: &mut AppState, text: String) {
    // Normalize line endings so the row count and the expanded text are clean
    // whether the paste arrived as a bracketed event or a key burst.
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    if text.lines().count() >= 4 || text.len() > 400 {
        let placeholder = state.register_paste(text);
        state.insert_input(&placeholder);
    } else {
        state.insert_input(&text);
    }
}

fn buffered_after_key(key: KeyEvent) -> anyhow::Result<bool> {
    if !may_be_unbracketed_paste_key(key) {
        return Ok(false);
    }
    // A pasted character's successor is already on its way; give the terminal a
    // brief moment to deliver it so a burst is detected reliably (a poll of ZERO
    // races the OS/terminal parsing on Windows and misses it). Human typing has
    // far larger gaps, so this never mistakes typing for a paste. Newlines get a
    // touch longer for the CR/LF split.
    let timeout = if is_unbracketed_paste_newline_key(key) {
        Duration::from_millis(4)
    } else {
        Duration::from_millis(3)
    };
    Ok(event::poll(timeout)?)
}

/// Drive the paste-burst accumulator for one key. Returns `true` when the key was
/// consumed by the burst (the caller should do nothing else with it).
fn handle_paste_burst(
    state: &mut AppState,
    burst: &mut PasteBurst,
    key: KeyEvent,
    buffered_after: bool,
) -> bool {
    match burst.observe(key, buffered_after, Instant::now()) {
        PasteAction::Pass => false,
        PasteAction::Absorbed => true,
        PasteAction::Flush(text) => {
            insert_paste(state, text);
            true
        }
        PasteAction::FlushThenPass(text) => {
            insert_paste(state, text);
            false
        }
    }
}

fn map_key(key: KeyEvent) -> Option<Key> {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Key::CtrlC),
        KeyCode::Char(c) => Some(Key::Char(c)),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Tab => Some(Key::Tab),
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

fn slash_picker_captures(state: &AppState, key: KeyEvent) -> bool {
    state.slash_picker.is_some()
        && matches!(
            key.code,
            KeyCode::Enter
                | KeyCode::Char('\n' | '\r')
                | KeyCode::Tab
                | KeyCode::Esc
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Backspace
        )
}

fn file_picker_captures(state: &AppState, key: KeyEvent) -> bool {
    state.file_picker.is_some()
        && matches!(
            key.code,
            KeyCode::Enter
                | KeyCode::Char('\n' | '\r')
                | KeyCode::Tab
                | KeyCode::Esc
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Backspace
        )
}

/// Enumerate workspace files for the `@`-mention picker: relative, forward-slash
/// paths, respecting ignore files, sorted and capped.
fn workspace_files(root: &std::path::Path) -> Vec<String> {
    const MAX_FILES: usize = 10_000;
    let mut files = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .hidden(true)
        .require_git(false)
        .build()
    {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        files.push(rel.to_string_lossy().replace('\\', "/"));
        if files.len() >= MAX_FILES {
            break;
        }
    }
    files.sort();
    files
}

fn slash_picker_exact_submit(state: &AppState, key: KeyEvent) -> bool {
    if !key.modifiers.is_empty() || !matches!(key.code, KeyCode::Enter | KeyCode::Char('\n' | '\r'))
    {
        return false;
    }
    let Some(picker) = &state.slash_picker else {
        return false;
    };
    let Some(suggestion) = picker.items.get(picker.selected) else {
        return false;
    };
    state.input.trim() == format!("/{}", suggestion.name)
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
        RuntimeEvent::ToolStuck { name, count } => Some(UiEvent::Notice(format!(
            "tool `{name}` stuck after {count} failures — stopping and trying another way"
        ))),
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

fn compaction_mode(mode: localpilot_config::CompactionMode) -> localpilot_harness::CompactionMode {
    match mode {
        localpilot_config::CompactionMode::Deterministic => {
            localpilot_harness::CompactionMode::Deterministic
        }
        localpilot_config::CompactionMode::SmartWithFallback => {
            localpilot_harness::CompactionMode::SmartWithFallback
        }
    }
}

fn harness_compaction_mode_label(mode: localpilot_harness::CompactionMode) -> &'static str {
    match mode {
        localpilot_harness::CompactionMode::Deterministic => "deterministic",
        localpilot_harness::CompactionMode::SmartWithFallback => "smart_with_fallback",
    }
}

/// Render `text` into native scrollback above the inline viewport, sized to its
/// wrapped height at the current terminal width.
fn emit_block<B: Backend>(terminal: &mut Terminal<B>, text: Text<'static>) -> anyhow::Result<()> {
    let width = terminal.size()?.width;
    let height = (Paragraph::new(text.clone())
        .wrap(Wrap { trim: false })
        .line_count(width) as u16)
        .max(1);
    terminal.insert_before(height, move |buf| {
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .render(buf.area, buf);
    })?;
    Ok(())
}

/// Push any finished transcript items into native scrollback, once each, so they
/// flow into the terminal's own history and are never redrawn.
fn flush_scrollback<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
) -> anyhow::Result<()> {
    for item in state.drain_for_scrollback() {
        emit_block(terminal, history_block_text(&item))?;
    }
    Ok(())
}

/// Re-initialise the inline viewport at `height` — ratatui has no in-place
/// inline-viewport-height setter. The old region is cleared and the cursor parked
/// at its top first, so the new region reserves from the same baseline and leaves
/// no stale rows in scrollback. Called only on a terminal-dimension change (window
/// resize / height clamp), not per content (see [`LIVE_REGION_HEIGHT`]).
fn resize_viewport(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    height: u16,
) -> anyhow::Result<()> {
    let region = terminal.get_frame().area();
    let _ = terminal.clear();
    execute!(terminal.backend_mut(), MoveTo(region.x, region.y))?;
    *terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )?;
    Ok(())
}

/// Commit finished history to scrollback, size the live region to the current
/// state, then redraw it.
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
) -> anyhow::Result<()> {
    flush_scrollback(terminal, state)?;
    // Reserve a constant live-region band. Re-init the inline viewport only when
    // the terminal's own dimensions change (a window resize, or a height clamp on a
    // short window), never per content. The previous per-frame re-init dropped
    // freshly committed history from native scrollback before it scrolled
    // off-screen; holding the band fixed keeps every committed block in scrollback.
    let size = terminal.size()?;
    let want_height = LIVE_REGION_HEIGHT.min(size.height.max(1));
    let area = terminal.get_frame().area();
    if area.height != want_height || area.width != size.width {
        resize_viewport(terminal, want_height)?;
    }
    terminal.draw(|frame| render(frame, state))?;
    Ok(())
}

fn enter_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    // Stay in the main screen buffer (no alternate screen) and do not capture the
    // mouse, so native scrollback, selection, copy/paste, and scrollwheel keep
    // working. Bracketed paste is still enabled so large pastes arrive as one
    // event.
    execute!(stdout, EnableBracketedPaste)?;
    // Ask the terminal to report keys unambiguously (the kitty keyboard
    // protocol), so modified keys like Alt+Enter / Shift+Enter reach the app.
    // Pushed unconditionally: a terminal that doesn't support it ignores the
    // sequence, and the support query can false-negative. The flags are popped on
    // exit.
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
    // Clear the visible screen (not scrollback — that is the user's history) so
    // the launch banner starts on a clean surface.
    execute!(
        stdout,
        terminal::Clear(terminal::ClearType::All),
        MoveTo(0, 0)
    )?;
    // A bottom inline viewport, reserved at a fixed height (clamped to a short
    // window) and held there: finished output lives above it in native scrollback;
    // only this region is redrawn each frame.
    let rows = terminal::size()
        .map(|(_cols, rows)| rows)
        .unwrap_or(LIVE_REGION_HEIGHT);
    let terminal = Terminal::with_options(
        CrosstermBackend::new(stdout),
        TerminalOptions {
            viewport: Viewport::Inline(LIVE_REGION_HEIGHT.min(rows.max(1))),
        },
    )?;
    Ok(terminal)
}

/// Print the launch banner into scrollback, then a small fixed gap before the
/// composer (banner on top, a couple of blank rows, then the inline composer
/// directly below) — no full-screen padding.
fn launch_banner(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    banner: Text<'static>,
) -> anyhow::Result<()> {
    emit_block(terminal, banner)?;
    terminal.insert_before(BANNER_GAP_ROWS, |_buf| {})?;
    Ok(())
}

fn leave_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    // Clear the live region and land the cursor at its top so the shell prompt
    // resumes cleanly below the finished output — there is no alternate screen to
    // leave.
    let region = terminal.get_frame().area();
    let _ = terminal.clear();
    terminal::disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        MoveTo(region.x, region.y),
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Offline coverage for the scrollback-commit path. Driving the real
    //! [`flush_scrollback`]/[`emit_block`] over ratatui's `TestBackend` — which
    //! records a `scrollback` buffer as rows scroll off the top — lets us assert
    //! that every committed transcript block stays reachable (in scrollback or the
    //! visible buffer) without a live terminal. These pin the invariant that the
    //! interactive driver must keep: committed history is never silently dropped.

    use super::*;
    use localpilot_tui::TranscriptLine;
    use ratatui::backend::TestBackend;

    fn test_header() -> Header {
        Header {
            version: "0".into(),
            provider: "test".into(),
            model: "test-model".into(),
            workspace: "ws".into(),
            session_id: "session".into(),
            update: None,
        }
    }

    /// A small fixed inline viewport over a `TestBackend`, deliberately shorter
    /// than the backend so committed history has room to scroll above it. The
    /// height is a test literal, independent of the production [`LIVE_REGION_HEIGHT`].
    fn inline_terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        Terminal::with_options(
            TestBackend::new(width, height),
            TerminalOptions {
                viewport: Viewport::Inline(4),
            },
        )
        .expect("inline test terminal")
    }

    /// Symbols of the terminal's scrollback followed by its visible buffer — the
    /// full set of rows a user could reach by scrolling up.
    fn scrollback_and_buffer(terminal: &Terminal<TestBackend>) -> String {
        let backend = terminal.backend();
        let mut out = String::new();
        for buffer in [backend.scrollback(), backend.buffer()] {
            for cell in &buffer.content {
                out.push_str(cell.symbol());
            }
        }
        out
    }

    /// Push one assistant line and commit it the way the event loop does:
    /// flush finished transcript to scrollback, then redraw the live region.
    fn commit_line(terminal: &mut Terminal<TestBackend>, state: &mut AppState, text: &str) {
        state.transcript.push(TranscriptLine {
            speaker: "assistant".to_string(),
            text: text.to_string(),
        });
        flush_scrollback(terminal, state).expect("flush scrollback");
        terminal
            .draw(|frame| render(frame, state))
            .expect("draw live region");
    }

    #[test]
    fn committed_history_is_recoverable_from_scrollback_and_buffer() {
        let mut terminal = inline_terminal(40, 8);
        let mut state = AppState::new(test_header(), Mode::Agent, UiProfile::Default);
        for i in 0..50 {
            commit_line(&mut terminal, &mut state, &format!("history-marker-{i}"));
        }
        let reachable = scrollback_and_buffer(&terminal);
        for i in 0..50 {
            assert!(
                reachable.contains(&format!("history-marker-{i}")),
                "committed line history-marker-{i} is unreachable in scrollback+buffer"
            );
        }
    }

    #[test]
    fn committed_blocks_scroll_into_native_scrollback() {
        let mut terminal = inline_terminal(40, 6);
        let mut state = AppState::new(test_header(), Mode::Agent, UiProfile::Default);
        for i in 0..30 {
            commit_line(&mut terminal, &mut state, &format!("scrolled-{i}"));
        }
        // Far more committed lines than the screen holds, so the earliest must
        // have left the visible buffer for the terminal's own scrollback.
        assert!(
            terminal.backend().scrollback().area.height > 0,
            "no committed history reached native scrollback"
        );
        let scrollback: String = terminal
            .backend()
            .scrollback()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            scrollback.contains("scrolled-0"),
            "the earliest committed line never reached scrollback"
        );
    }

    #[test]
    fn history_survives_live_region_content_changes() {
        // The bug trigger was the live region changing height every time its
        // content changed. With a held, fixed-height viewport the content can
        // oscillate freely (streaming on/off, multi-line, idle) without losing any
        // committed history. This drives that oscillation against a fixed viewport.
        let mut terminal = inline_terminal(40, 8);
        let mut state = AppState::new(test_header(), Mode::Agent, UiProfile::Default);
        for i in 0..40 {
            state.streaming = match i % 3 {
                0 => String::new(),
                1 => "in progress".to_string(),
                _ => "in progress\nmore\nand more".to_string(),
            };
            commit_line(&mut terminal, &mut state, &format!("turn-{i}"));
        }
        state.streaming.clear();
        terminal
            .draw(|frame| render(frame, &state))
            .expect("final draw");
        let reachable = scrollback_and_buffer(&terminal);
        for i in 0..40 {
            assert!(
                reachable.contains(&format!("turn-{i}")),
                "turn-{i} was lost while the live-region content oscillated"
            );
        }
    }
}
