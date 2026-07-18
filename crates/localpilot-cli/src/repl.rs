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
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use crossterm::cursor::MoveTo;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{execute, terminal};
use localpilot_config::{CliOverrides, ConfigPaths};
use localpilot_core::ContentBlock;
use localpilot_harness::{ModelHealth, RuntimeEvent, SessionConfig, SessionRuntime, SwitchError};
use localpilot_llm::ProviderRegistry;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{
    Approver, Effect, Interactivity, PermissionEngine, PermissionEngineHandle, PermissionRequest,
    Profile,
};
use localpilot_store::Store;
use localpilot_tools::BackgroundProcesses;
use localpilot_tui::{
    banner_text, blocking_prompt_height, handle_input, history_block_text, parse_slash, render,
    AppInput, AppState, ApprovalRequest, BackgroundCommand, BackgroundProcess, Header,
    ImageAttachment, IngestAction, Key, Mode, PlanItem, Profile as UiProfile, SlashAction,
    TrustPrompt, UiEvent,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::key_input::{
    is_cancel, is_clipboard_image_key, is_key_action, is_newline, is_submit,
    is_unbracketed_paste_newline_key, may_be_unbracketed_paste_key, PasteAction, PasteBurst,
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
    /// The durable prompt-history store; submitted prompts are appended here.
    history: &'a localpilot_store::PromptHistory,
    /// Loaded config, used to re-resolve the active provider's vision capability
    /// when the user pastes an image (config wins, else a best-effort probe).
    config: &'a localpilot_config::Config,
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
    let target_kind = match request.effect {
        Effect::ReadPath { .. } | Effect::WritePath { .. } => "path",
        Effect::RunCommand(_) => "command",
        Effect::Network => "network",
    };
    let risk_class = request.effect.risk_label();
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
/// Opt-in startup timing. With `LOCALPILOT_TIME_STARTUP=1` in the environment,
/// each init step prints its own and the cumulative duration to stderr before the
/// live region is drawn — to diagnose a slow startup (e.g. MCP server spawning).
/// A no-op (zero cost, no output) when the variable is unset.
struct StartupTimer {
    on: bool,
    start: Instant,
    last: Instant,
}

impl StartupTimer {
    fn new() -> Self {
        let start = Instant::now();
        Self {
            on: std::env::var_os("LOCALPILOT_TIME_STARTUP").is_some(),
            start,
            last: start,
        }
    }

    fn mark(&mut self, label: &str) {
        if !self.on {
            return;
        }
        let now = Instant::now();
        eprintln!(
            "[startup] {label:<26} +{:>6} ms   (total {} ms)",
            now.duration_since(self.last).as_millis(),
            now.duration_since(self.start).as_millis(),
        );
        self.last = now;
    }
}

pub async fn run_chat(
    model: Option<&str>,
    provider_id: Option<&str>,
    profile: Profile,
    resume: Option<localpilot_core::SessionId>,
) -> anyhow::Result<()> {
    let mut timer = StartupTimer::new();
    let cwd = std::env::current_dir()?;
    let config = localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())?;
    timer.mark("config load");

    // Best-effort retention so `.localpilot/` cannot grow without bound. Errors
    // are ignored — cleanup must never block starting a chat — and it runs before
    // the live region is drawn.
    if config.storage.auto_prune {
        let policy = crate::session_cmd::retention_policy(&config.storage, None, None);
        if !policy.is_unbounded() {
            let _ = Store::open(&cwd).prune(policy, crate::session_cmd::now_unix(), false);
        }
    }

    timer.mark("store prune");
    let model = model
        .map(str::to_string)
        .or_else(|| config.resolve_model(provider_id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no model: pass --model, or set a default in .localpilot.toml \
                 ([providers.<id>] model = \"...\")"
            )
        })?;
    // Build every configured provider once and keep a shared handle, so `/model`
    // can re-point the live session at another configured provider without
    // rebuilding or re-authenticating it.
    let provider_registry = std::sync::Arc::new(ProviderRegistry::from_config(&config)?);
    let provider = match provider_id {
        Some(id) => provider_registry.get(id),
        None => provider_registry.default_provider(),
    }
    .cloned()
    .ok_or_else(|| anyhow::anyhow!("no provider is configured"))?;

    // The real context window: per-provider config first, then best-effort
    // discovery from the local server's model listing. Failure means falling
    // back to the configured global budget, never an error.
    timer.mark("provider registry");
    let mut context_window = provider.declaration().max_context_tokens;
    if context_window.is_none() {
        context_window = discovered_window(&config, provider_id, &model).await;
    }
    timer.mark("context-window discovery");

    // Ask-gated actions suspend the turn and prompt in the TUI; the user's
    // y/n answer flows back through this channel to the permission engine.
    let (approval_tx, mut approval_rx) = mpsc::unbounded_channel::<ApprovalCall>();
    let mut registry = crate::mcp::McpTools::load(&config).await.registry();
    let broker = crate::mcp::install_broker(&config.tools, &mut registry);
    timer.mark("mcp servers + tools");
    // Interactive session: apply the built-in safety rails so an unconfigured
    // project still bounds a runaway tool loop (ADR-0055). The interactive profile
    // uses a higher tool-call ceiling and no default wall-clock — a long
    // interactive turn is legitimate and the user can cancel it. Explicit
    // `[harness]` values win inside `resolved_rails`.
    let rails = config.harness.resolved_rails(true);
    let mut runtime = SessionRuntime::new(
        provider,
        registry,
        PermissionEngine::new(profile, Vec::new()),
        Box::new(TuiApprover {
            tx: approval_tx.clone(),
        }),
        Store::open(&cwd),
        crate::session_cmd::workspace_with_read_roots(&cwd, &config)?,
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            model: model.to_string(),
            interactivity: Interactivity::Interactive,
            trusted: matches!(profile, Profile::Bypass | Profile::Unrestricted),
            context_token_limit: localpilot_harness::effective_context_limit(
                context_window,
                config.harness.context_token_limit,
            ),
            compaction_mode: compaction_mode(config.compaction.mode),
            summarizer_tuning: localpilot_harness::SummarizerTuning::from_config(
                &config.compaction,
            ),
            tool_call_budget: rails.tool_call_budget,
            tool_call_budget_max: rails.tool_call_budget_max,
            tool_budget_explicit: rails.budget_explicit,
            rules: config.harness.rules.clone(),
            enforce_claim_gate: config.harness.claim_gate.is_enabled(),
            tool_marker_enabled: config.tools.marker,
            enforce_readable_errors: config.tools.readable_errors,
            repair_mode: config.tools.repair,
            turn_timeout: rails.turn_timeout_secs.map(std::time::Duration::from_secs),
            verify_before_done: config.harness.verify_before_done,
            verify_command: config.harness.verify_command.clone(),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    timer.mark("runtime (store + workspace)");
    runtime.set_broker(broker);
    // Hand the runtime the built provider map so `/model` switches are a lookup.
    runtime.set_registry(provider_registry);
    // Best-effort: resolve the active provider's image-input capability (config
    // wins, else a read-only `/props` probe) so the image-attach preflight honours
    // an undeclared-but-vision-capable local server. Default-off probe (a declared
    // provider is not probed); failure leaves the provider's declaration as the gate.
    runtime.set_image_support_override(resolved_image_support(&config, provider_id).await);
    timer.mark("vision /props probe");
    localpilot_harness::register_project_analysis_context(
        &cwd,
        config.context.project_analysis,
        config.docs.lookup_policy,
        &mut runtime,
    );
    localpilot_harness::register_project_instructions_context(
        &cwd,
        config.context.inject_instructions,
        config.context.instruction_char_budget,
        &mut runtime,
    );
    // Relevant accepted LocalMind memory is contributed per turn through the
    // context-hook fabric; ingested folder knowledge is pulled on demand via the
    // knowledge_search tool rather than seeded here.
    localpilot_localmind::register_context_hook(&cwd, &mut runtime);
    timer.mark("context hooks");

    let header = Header {
        version: env!("LOCALPILOT_VERSION").to_string(),
        provider: provider_id.unwrap_or(&config.provider.default).to_string(),
        model: model.to_string(),
        workspace: cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| cwd.display().to_string()),
        session_id: runtime.session_id().to_string(),
        session_name: None,
        // Remote-sourced (a release tag via the GitHub API) and rendered into
        // the banner without passing the state scrub — strip control bytes so
        // a garbled or hostile tag can never reach the terminal raw.
        update: crate::update::cached_notice(&cwd).await.map(|notice| {
            notice
                .chars()
                .filter(|c| !c.is_ascii_control() && !('\u{80}'..='\u{9f}').contains(c))
                .collect()
        }),
    };
    timer.mark("update check");
    let mut state = AppState::new(header, Mode::Agent, ui_profile(profile));
    // Ask once per folder before doing anything in it; trust is remembered across
    // sessions. Already-trusted folders (and bypass/unrestricted, which are
    // explicit) skip it.
    if !matches!(profile, Profile::Bypass | Profile::Unrestricted)
        && !crate::trust::is_trusted(&cwd)
    {
        state.trust = Some(TrustPrompt {
            path: cwd.display().to_string(),
        });
    } else {
        state.trusted = true;
    }
    // Seed the `@`-mention file list; refreshed after each turn (files may change).
    state.set_workspace_files(workspace_files(&cwd));
    timer.mark("workspace file walk");

    // Boot straight into a session the CLI asked for (`--resume <id|name>` or
    // `--continue`). Context is rebuilt from the event log and the transcript tail
    // is replayed into the view, exactly as the in-session `/resume` does. The
    // reference was already resolved to an id by `resolve_resume`.
    if let Some(session) = resume {
        load_session_id(&mut state, &mut runtime, session);
    }

    // Seed prompt recall from the durable global history so Up/Down survives a
    // restart, scoped to this project (Ctrl-T views all projects). The store
    // honours the `[history] persistence` opt-out; when off it loads nothing and
    // appends nothing. A read never fails the session — the load is tolerant.
    let history = localpilot_store::PromptHistory::new(config.history.persistence.is_enabled());
    let history_entries = history.load();
    state.seed_input_history(
        recall_entries(localpilot_store::project_entries(&history_entries, &cwd)),
        recall_entries(history_entries),
    );
    timer.mark("prompt history load");

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
                if let Err(error) =
                    localpilot_localmind::ingest_run(&ingest_root, &ingest_config, mode)
                {
                    // A failed background index build makes knowledge_search return
                    // nothing or stale results all session, indistinguishable from
                    // "no matching knowledge" — surface it so it is diagnosable.
                    tracing::warn!(
                        target: "localpilot::ingest",
                        %error,
                        "background project-knowledge index build failed; knowledge_search may return no or stale results this session"
                    );
                }
            });
        }
    }

    timer.mark("knowledge index (mode check)");
    timer.mark("READY — entering TUI");
    install_terminal_restore_panic_hook();
    let mut terminal = enter_terminal()?;
    // Print the launch banner once and seat the live region at the screen
    // bottom. A banner failure must still fall through to `leave_terminal` —
    // an early `?` here would leave the shell in raw mode.
    let result = match launch_banner(&mut terminal, banner_text(&state.header)) {
        Ok(()) => {
            event_loop(
                &mut terminal,
                &mut state,
                &mut runtime,
                &mut approval_rx,
                CommandHost {
                    approval_tx,
                    cwd: &cwd,
                    model: &model,
                    provider_id,
                    history: &history,
                    config: &config,
                },
            )
            .await
        }
        Err(error) => Err(error),
    };
    leave_terminal(&mut terminal)?;
    // Learn from the finished session. This is best-effort so terminal teardown
    // is never held hostage by the learning subsystem. The id is read *after*
    // the event loop: `/new`, `/continue`, and `/fork` re-point the runtime
    // mid-run, and a close-out against the id captured at startup would check
    // the abandoned (often empty) session for lessons instead of the one the
    // user actually worked in.
    crate::context_inject::close_out(&cwd, runtime.session_id());
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
                    } else if is_clipboard_image_key(key) {
                        attach_clipboard_image(state, runtime, &host, false).await;
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
                // ones to a placeholder so the input line stays readable. A paste
                // that carries no usable text may be a terminal routing Ctrl+V of a
                // clipboard image through paste, so probe the clipboard for one.
                Event::Paste(text) if state.trust.is_none() => {
                    if text.trim().is_empty() {
                        attach_clipboard_image(state, runtime, &host, true).await;
                    } else {
                        insert_paste(state, text);
                    }
                }
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

/// Convert persisted history entries into the TUI's recall shape, carrying
/// each prompt's paste mappings so a recalled placeholder can expand again.
fn recall_entries(
    entries: Vec<localpilot_store::HistoryEntry>,
) -> Vec<localpilot_tui::RecallEntry> {
    entries
        .into_iter()
        .map(|entry| localpilot_tui::RecallEntry {
            text: entry.text,
            pastes: entry
                .pastes
                .into_iter()
                .map(|paste| localpilot_tui::Paste {
                    placeholder: paste.placeholder,
                    content: paste.content,
                })
                .collect(),
        })
        .collect()
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
    let submitted = state.take_input_for_submit();
    let images = state.take_images();
    let (shown, prompt) = (submitted.shown, submitted.prompt);
    if prompt.trim().is_empty() && images.is_empty() {
        return Ok(());
    }
    // Persist the visible prompt to the durable history — with its paste
    // mappings, so a recalled prompt can restore the pasted content instead of
    // replaying placeholder text (LocalHub#19). Best-effort: a write failure
    // surfaces as a notice and never blocks the turn or breaks the session;
    // the no-op opt-out is honoured inside.
    let history_pastes: Vec<localpilot_store::HistoryPaste> = submitted
        .pastes
        .iter()
        .map(|paste| localpilot_store::HistoryPaste {
            placeholder: paste.placeholder.clone(),
            content: paste.content.clone(),
        })
        .collect();
    if let Err(error) = host.history.append(&shown, &history_pastes, host.cwd) {
        state.apply(UiEvent::Notice(format!(
            "could not save prompt history: {error}"
        )));
    }
    let result = if let Some(action) = parse_slash(&prompt) {
        // A slash command takes no image attachments; the captured set is dropped.
        run_slash(terminal, state, runtime, approval_rx, host, action).await
    } else {
        // The image placeholders are stand-ins for the attachment blocks, so strip
        // them from the text the model receives while leaving `shown` intact.
        let model_prompt = strip_image_placeholders(&prompt, &images);
        let attachments: Vec<ContentBlock> = images
            .iter()
            .map(|image| ContentBlock::image(&image.media_type, &image.data))
            .collect();
        state.apply(UiEvent::UserMessage(shown));
        if !attachments.is_empty() {
            state.apply(UiEvent::Notice(format!(
                "sending {} image(s) with this prompt",
                attachments.len()
            )));
        }
        if state.mode == Mode::Research {
            // In research mode a bare prompt is a topic to research (web per
            // config, ADR-0076), not a model turn.
            run_research_prompt(terminal, state, approval_rx, host, &model_prompt).await
        } else {
            state.busy = true;
            let outcome = run_turn(
                terminal,
                state,
                runtime,
                approval_rx,
                &model_prompt,
                &attachments,
            )
            .await;
            state.busy = false;
            // The turn may have created or removed files; refresh the @-mention list.
            state.set_workspace_files(workspace_files(host.cwd));
            outcome
        }
    };
    // A turn may have started a background process and a `/bg`/`/new` may have
    // changed the set; keep the status-line indicator current either way.
    refresh_background(state, runtime.background_registry());
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
            state.header.session_name = None;
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
                    // The branch is a distinct session and inherits no name.
                    state.header.session_name = None;
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
                    let name = entry
                        .name
                        .as_deref()
                        .map(|n| format!(" \"{n}\""))
                        .unwrap_or_default();
                    state.apply(UiEvent::Notice(format!(
                        "{}{name} — {} message(s){current}",
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
        SlashAction::NameSession(name) => {
            let id = runtime.session_id();
            match runtime.store().set_session_name(id, &name) {
                Ok(()) => {
                    state.header.session_name = Some(name.clone());
                    state.apply(UiEvent::Notice(format!("named this session \"{name}\"")));
                }
                Err(error) => {
                    state.apply(UiEvent::Notice(format!("could not name session: {error}")));
                }
            }
        }
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
            // Smart compaction may call the summarizer model, which can take
            // up to the provider timeout against a wedged server. Drive it
            // through the event pump so the UI stays live and Ctrl+C cancels
            // (the summarizer future is dropped; the conversation is only
            // mutated on completion, so a cancel leaves it unchanged).
            let (_events, mut rx) = broadcast::channel::<RuntimeEvent>(4);
            let cancel = CancellationToken::new();
            state.busy = true;
            let operation = async {
                Ok(tokio::select! {
                    summary = async {
                        if force {
                            runtime.compact_conversation_force().await
                        } else {
                            runtime.compact_conversation().await
                        }
                    } => Some(summary),
                    () = cancel.cancelled() => None,
                })
            };
            let summary = drive_runtime_operation(
                terminal,
                state,
                approval_rx,
                &mut rx,
                &cancel,
                std::time::Instant::now(),
                None,
                None,
                None,
                operation,
            )
            .await;
            state.busy = false;
            let Some(summary) = summary? else {
                state.apply(UiEvent::Notice("compaction cancelled".to_string()));
                return Ok(());
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
        SlashAction::Model { provider, model } => {
            run_model_command(state, runtime, host.cwd, provider, model).await;
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
        SlashAction::Research(topic) => match topic {
            // A one-shot `/research <topic>` runs immediately and leaves the
            // current mode unchanged.
            Some(topic) => {
                state.apply(UiEvent::UserMessage(format!("/research {topic}")));
                run_research_prompt(terminal, state, approval_rx, host, &topic).await?;
            }
            // A bare `/research` enters persistent research mode. The notice
            // reflects the configured egress state (ADR-0076) rather than a
            // fixed claim.
            None => {
                state.mode = Mode::Research;
                state.apply(UiEvent::Notice(crate::research::research_mode_notice(
                    host.cwd,
                )));
            }
        },
        SlashAction::Background(command) => {
            apply_background_command(state, runtime.background_registry(), command)
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

/// Drive the `/model` command: with no provider, list the configured providers
/// and their available models; otherwise re-point the live session at the named
/// provider (and model). All outcomes — success, the no-default-model warning, an
/// unknown provider, or a refused mid-turn switch — surface as plain notices; the
/// command never panics or degrades the session.
async fn run_model_command(
    state: &mut AppState,
    runtime: &mut SessionRuntime,
    cwd: &std::path::Path,
    provider: Option<String>,
    model: Option<String>,
) {
    let config =
        match localpilot_config::load(&ConfigPaths::standard(cwd), &CliOverrides::default()) {
            Ok(config) => config,
            Err(error) => {
                state.apply(UiEvent::Notice(format!(
                    "/model: cannot read config: {error}"
                )));
                return;
            }
        };
    match provider {
        None => list_models(state, runtime, &config).await,
        Some(provider_id) => switch_model(state, runtime, &config, &provider_id, model).await,
    }
}

/// List configured providers and the models each reports, marking the active one.
/// Discovery failure is non-fatal: the provider's configured model is shown with a
/// note instead.
async fn list_models(
    state: &mut AppState,
    runtime: &SessionRuntime,
    config: &localpilot_config::Config,
) {
    if config.providers.is_empty() {
        state.apply(UiEvent::Notice(
            "no providers configured (see .localpilot.toml)".to_string(),
        ));
        return;
    }
    let active_provider = runtime.active_provider_id().to_string();
    let active_model = runtime.active_model().to_string();
    state.apply(UiEvent::Notice(
        "providers (current marked *, switch with /model <provider> [model]):".to_string(),
    ));
    for (id, entry) in &config.providers {
        let marker = if *id == active_provider { "*" } else { " " };
        state.apply(UiEvent::Notice(format!("{marker} {id} ({})", entry.kind)));
        let Some(base_url) = crate::models_cmd::listing_base_url(entry) else {
            let configured = entry.model.as_deref().unwrap_or("(none)");
            state.apply(UiEvent::Notice(format!(
                "    configured model: {configured}"
            )));
            continue;
        };
        match crate::models_cmd::discover_models_for_provider(config, id, &base_url).await {
            Ok(models) if !models.is_empty() => {
                for model in models {
                    let active = if *id == active_provider && model.id == active_model {
                        " (active)"
                    } else {
                        ""
                    };
                    state.apply(UiEvent::Notice(format!("    {}{active}", model.id)));
                }
            }
            Ok(_) => state.apply(UiEvent::Notice("    (no models loaded)".to_string())),
            Err(error) => {
                let configured = entry.model.as_deref().unwrap_or("(none)");
                state.apply(UiEvent::Notice(format!(
                    "    unreachable ({error}); configured model: {configured}"
                )));
            }
        }
    }
}

/// Switch the active provider (and optionally model). Reports the new target and
/// any warning; leaves the session unchanged on a typed error.
async fn switch_model(
    state: &mut AppState,
    runtime: &mut SessionRuntime,
    config: &localpilot_config::Config,
    provider_id: &str,
    model: Option<String>,
) {
    let outcome = match runtime.set_active_provider(provider_id) {
        Ok(outcome) => outcome,
        Err(SwitchError::UnknownProvider(id)) => {
            state.apply(UiEvent::Notice(format!(
                "/model: provider '{id}' is not configured — try /model to list"
            )));
            return;
        }
        Err(SwitchError::TurnInFlight) => {
            state.apply(UiEvent::Notice(
                "/model: a turn is in progress; switch once it finishes".to_string(),
            ));
            return;
        }
    };
    // The provider's no-default-model warning surfaces before any model override.
    if let Some(warning) = &outcome.warning {
        state.apply(UiEvent::Notice(format!("/model: {warning}")));
    }
    // An explicit model overrides the provider default; validate it best-effort.
    if let Some(model) = model {
        if let Err(error) = runtime.set_active_model(&model) {
            state.apply(UiEvent::Notice(format!("/model: {error}")));
            return;
        }
        warn_unknown_model(state, config, provider_id, &model).await;
    }
    state.header.provider = runtime.active_provider_id().to_string();
    state.header.model = runtime.active_model().to_string();
    // The active provider changed, so re-resolve its image-input capability for the
    // attach preflight (config wins, else a best-effort probe of the new server).
    runtime.set_image_support_override(resolved_image_support(config, Some(provider_id)).await);
    state.apply(UiEvent::Notice(format!(
        "switched to provider '{}' · model '{}'",
        runtime.active_provider_id(),
        runtime.active_model()
    )));
}

/// Best-effort model-id check: when the provider exposes a model listing and the
/// requested model is absent, warn (never fail — the id may be valid but unlisted,
/// or discovery may be offline).
async fn warn_unknown_model(
    state: &mut AppState,
    config: &localpilot_config::Config,
    provider_id: &str,
    model: &str,
) {
    let Some(entry) = config.providers.get(provider_id) else {
        return;
    };
    let Some(base_url) = crate::models_cmd::listing_base_url(entry) else {
        return;
    };
    if let Ok(models) =
        crate::models_cmd::discover_models_for_provider(config, provider_id, &base_url).await
    {
        if !models.is_empty() && !models.iter().any(|m| m.id == model) {
            state.apply(UiEvent::Notice(format!(
                "/model: '{model}' is not in {provider_id}'s model list; using it anyway"
            )));
        }
    }
}

/// List or stop the session's background processes, posting the result as
/// notices. Stopping is synchronous, so it runs directly off the input loop.
/// Whether `action` is safe to run while a turn is in flight. These touch only
/// UI state or the interior-mutable background registry, never the borrowed
/// runtime, so they can execute from the mid-turn key handler.
fn is_live_slash(action: &SlashAction) -> bool {
    matches!(
        action,
        SlashAction::ToggleThinking | SlashAction::Background(_) | SlashAction::SetProfile(_)
    )
}

/// Run an allowlisted slash command mid-turn. Only the variants accepted by
/// [`is_live_slash`] are handled here; anything else is a no-op.
fn run_live_slash(
    state: &mut AppState,
    background: Option<&Arc<BackgroundProcesses>>,
    permissions: Option<&PermissionEngineHandle>,
    action: SlashAction,
) {
    match action {
        SlashAction::ToggleThinking => state.thinking.visible = !state.thinking.visible,
        SlashAction::Background(command) => match background {
            Some(processes) => {
                apply_background_command(state, processes, command);
                refresh_background(state, processes);
            }
            None => state.apply(UiEvent::Notice(
                "background controls are unavailable right now".to_string(),
            )),
        },
        // A profile switch only reconfigures this side's permission engine, so
        // it need not wait for the model: the runtime snapshots the shared
        // handle per tool call, and the swap governs the very next call.
        SlashAction::SetProfile(profile) => match permissions {
            Some(handle) => {
                handle.set(PermissionEngine::new(sandbox_profile(profile), Vec::new()));
                state.profile = profile;
                state.apply(UiEvent::Notice(format!(
                    "permission profile: {} (in force from the next tool call)",
                    profile.label()
                )));
            }
            None => state.apply(UiEvent::Notice(
                "profile changes are unavailable during this operation".to_string(),
            )),
        },
        _ => {}
    }
}

fn apply_background_command(
    state: &mut AppState,
    processes: &BackgroundProcesses,
    command: BackgroundCommand,
) {
    match command {
        BackgroundCommand::List => {
            let listed = processes.list();
            if listed.is_empty() {
                state.apply(UiEvent::Notice("no background processes".to_string()));
            } else {
                state.apply(UiEvent::Notice(
                    "background processes (stop with /bg stop <id> or /bg stop all):".to_string(),
                ));
                for process in listed {
                    let status = if process.alive { "running" } else { "exited" };
                    state.apply(UiEvent::Notice(format!(
                        "  {} [{}] {}s · {}",
                        process.id, status, process.age_secs, process.command
                    )));
                }
            }
        }
        BackgroundCommand::Stop(id) => {
            if processes.stop_now(&id) {
                state.apply(UiEvent::Notice(format!("stopped background process {id}")));
            } else {
                state.apply(UiEvent::Notice(format!("no background process {id}")));
            }
        }
        BackgroundCommand::StopAll => {
            let count = processes.list().len();
            processes.kill_all();
            state.apply(UiEvent::Notice(format!(
                "stopped {count} background process(es)"
            )));
        }
    }
}

/// Push the current background-process set into the UI so the status-line
/// indicator and `/bg` listing stay in sync after a turn or a `/bg` command.
fn refresh_background(state: &mut AppState, processes: &BackgroundProcesses) {
    let processes = processes
        .list()
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

/// How many trailing conversation messages a resume replays into the transcript
/// view. The model's context is fully restored by `load_session` regardless;
/// this only bounds what is re-shown on screen. Matches the `/sessions`
/// listing's recent-10 convention.
const RESUME_REPLAY_MESSAGES: usize = 10;

fn load_session_id(
    state: &mut AppState,
    runtime: &mut SessionRuntime,
    session: localpilot_core::SessionId,
) {
    match runtime.load_session(session) {
        Ok(report) => {
            state.clear_conversation_view();
            if report.skipped_lines > 0 {
                state.apply(UiEvent::Notice(format!(
                    "recovered session log: skipped {} damaged event line(s); the remaining events are intact",
                    report.skipped_lines
                )));
            }
            state.header.session_id = session.to_string();
            // Surface the conversation's name (if any) in the header on resume.
            state.header.session_name = runtime
                .store()
                .list_sessions()
                .ok()
                .and_then(|sessions| sessions.into_iter().find(|e| e.id == session))
                .and_then(|entry| entry.name);
            replay_recent_transcript(state, runtime, session);
            state.apply(UiEvent::Notice(format!(
                "resumed session {session}; current profile and trust apply"
            )));
        }
        Err(error) => {
            state.apply(UiEvent::Notice(format!("resume failed: {error}")));
        }
    }
}

/// Re-show the tail of a resumed session's conversation so the user sees what
/// they are continuing, not an empty screen. View-only: the runtime already
/// holds the full restored history. User and assistant text messages only
/// (tool traffic and runtime-synthesized repairs would be noise), routed
/// through `state.apply` so the normal transcript invariants and scrubbing
/// hold. Best-effort — an unreadable transcript degrades to the resume notice.
fn replay_recent_transcript(
    state: &mut AppState,
    runtime: &SessionRuntime,
    session: localpilot_core::SessionId,
) {
    use localpilot_core::Role;
    let Ok(messages) = runtime.store().read_transcript(session) else {
        return;
    };
    let (skipped, shown) = replay_selection(messages, RESUME_REPLAY_MESSAGES);
    if skipped > 0 {
        state.apply(UiEvent::Notice(format!(
            "… {skipped} earlier message(s) not shown (context fully restored)"
        )));
    }
    for (role, text) in shown {
        match role {
            Role::User => state.apply(UiEvent::UserMessage(text)),
            _ => {
                state.apply(UiEvent::TextDelta(text));
                state.apply(UiEvent::TurnComplete);
            }
        }
    }
}

/// Pick which resumed messages are re-shown: authored (non-synthetic) user and
/// assistant text, keeping only the trailing `limit`. Returns how many eligible
/// messages were elided along with the ones to show, oldest-first.
fn replay_selection(
    messages: Vec<localpilot_core::Message>,
    limit: usize,
) -> (usize, Vec<(localpilot_core::Role, String)>) {
    use localpilot_core::Role;
    let shown: Vec<(Role, String)> = messages
        .into_iter()
        .filter(|message| !message.is_synthetic())
        .filter_map(|message| {
            if !matches!(message.role, Role::User | Role::Assistant) {
                return None;
            }
            let text = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then_some((message.role, text))
        })
        .collect();
    let skipped = shown.len().saturating_sub(limit);
    (skipped, shown.into_iter().skip(skipped).collect())
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

/// Run a research pass for `topic` and post its output to the transcript.
/// Web research follows the same config defaults as the subcommand (on unless
/// `[research.web].enabled = false`), with the egress disclosure landing in
/// the transcript before any request. The pass calls the model provider —
/// potentially several sequential requests, each bounded only by the provider
/// timeout — so it is driven through the event pump: the UI stays live and
/// Ctrl+C cancels (dropping the in-flight research future).
async fn run_research_prompt(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>,
    host: &CommandHost<'_>,
    topic: &str,
) -> anyhow::Result<()> {
    let options = match crate::research::options_from_config(host.cwd, true, true)? {
        Some(options) => options,
        None => {
            state.apply(UiEvent::Notice(
                "research is disabled ([research].enabled = false)".to_string(),
            ));
            return Ok(());
        }
    };
    let (_events, mut rx) = broadcast::channel::<RuntimeEvent>(4);
    let cancel = CancellationToken::new();
    let cwd = host.cwd;
    state.busy = true;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let run_stop = std::sync::Arc::clone(&stop);
    let operation = async {
        let mut output = Vec::new();
        // The pinned future borrows `output`; the block scopes that borrow so
        // `output` can move into the return value once the run has finished.
        let result = {
            let run = crate::research::run_interactive_research(
                cwd,
                topic,
                &options,
                run_stop,
                &mut output,
            );
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => Some(result),
                () = cancel.cancelled() => {
                    // Ctrl+C asks the loop to stop at its next question boundary
                    // and waits for the partial report — coverage-so-far beats
                    // nothing on a long run.
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    Some(run.await)
                }
            }
        };
        Ok((output, result))
    };
    let outcome = drive_runtime_operation(
        terminal,
        state,
        approval_rx,
        &mut rx,
        &cancel,
        std::time::Instant::now(),
        None,
        None,
        None,
        operation,
    )
    .await;
    state.busy = false;
    let (output, result) = outcome?;
    match result {
        Some(result) => apply_command_result(state, output, result),
        None => state.apply(UiEvent::Notice("research cancelled".to_string())),
    }
    Ok(())
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

    // The harness resume builds its own inner runtime with the profile
    // captured above, so a mid-run profile swap has nothing to apply to —
    // profile slash commands keep the idle-only notice here.
    let summary = drive_runtime_operation(
        terminal,
        state,
        approval_rx,
        &mut rx,
        &cancel,
        started,
        None,
        None,
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
    attachments: &[ContentBlock],
) -> anyhow::Result<()> {
    let (events, mut rx) = broadcast::channel::<RuntimeEvent>(1024);
    let cancel = CancellationToken::new();
    let started = std::time::Instant::now();
    // Input submitted while the turn runs becomes steering: admitted at the
    // next safe provider-turn boundary instead of being swallowed.
    let steer = runtime.steer_queue();
    // A clonable Arc, so `/bg` can run mid-turn without touching the runtime the
    // turn future has mutably borrowed.
    let background = runtime.background_handle();
    // Same pattern for the permission engine, so `/unrestricted` (and the other
    // profile commands) apply while the model is still generating.
    let permissions = runtime.permission_engine_handle();
    let turn = async {
        let _ = runtime
            .run_turn_with_attachments(prompt, attachments, &events, &cancel)
            .await;
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
        Some(&background),
        Some(&permissions),
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
    background: Option<&Arc<BackgroundProcesses>>,
    permissions: Option<&PermissionEngineHandle>,
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
                        background,
                        permissions,
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
#[allow(clippy::too_many_arguments)] // the mid-turn event handler threads these
fn resolve_event(
    state: &mut AppState,
    pending: Option<oneshot::Sender<bool>>,
    event: Event,
    cancel: &CancellationToken,
    steer: Option<&localpilot_harness::SteerQueue>,
    background: Option<&Arc<BackgroundProcesses>>,
    permissions: Option<&PermissionEngineHandle>,
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
                        match parse_slash(&state.input) {
                            Some(action) if is_live_slash(&action) => {
                                // Clear the input line, then run the allowlisted
                                // command against UI state / the shared handle.
                                let _ = state.take_input_for_submit();
                                run_live_slash(state, background, permissions, action);
                            }
                            _ => state.apply(UiEvent::Notice(
                                "slash commands run when the current turn is idle".to_string(),
                            )),
                        }
                        return None;
                    }
                    // Submitting while a turn runs queues steering input,
                    // admitted at the next safe provider-turn boundary.
                    if let Some(steer) = steer {
                        if !state.input.trim().is_empty() {
                            let submitted = state.take_input_for_submit();
                            steer.push(submitted.prompt);
                            state.apply(UiEvent::UserMessage(submitted.shown));
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
    // Route the paste through the same scrub the transcript uses: line
    // endings normalized, control bytes dropped, and whole ANSI sequences
    // swallowed (e.g. colors copied out of another terminal), so nothing
    // control-ish reaches the composer render or the model.
    let text = localpilot_tui::scrub_text(text);
    if text.lines().count() >= 4 || text.len() > 400 {
        let placeholder = state.register_paste(text);
        state.insert_input(&placeholder);
    } else {
        state.insert_input(&text);
    }
}

/// The largest base64 payload we attach, keeping a single image comfortably under
/// provider request limits (~5 MB encoded ≈ ~3.7 MB of image bytes).
const MAX_IMAGE_BASE64_BYTES: usize = 5 * 1024 * 1024;

/// Read an image from the OS clipboard and attach it to the next prompt as a
/// placeholder. Best effort: an unsupported model, an absent image, or an
/// encode/oversize failure surfaces as a notice and never disturbs the session.
/// `quiet_when_absent` suppresses the "no image" notice for the empty-paste
/// fallback path, where a normal text paste simply had nothing to insert.
async fn attach_clipboard_image(
    state: &mut AppState,
    runtime: &mut SessionRuntime,
    host: &CommandHost<'_>,
    quiet_when_absent: bool,
) {
    if !runtime.active_accepts_images() {
        // An explicit paste is a strong signal the user wants images. The
        // capability may be unresolved (probe was off at startup, or the server
        // came up afterwards), so re-resolve it once — config wins, else a
        // best-effort `/props` probe — before deciding.
        let resolved = resolved_image_support(host.config, host.provider_id).await;
        runtime.set_image_support_override(resolved);
    }
    if !runtime.active_accepts_images() {
        // Still not known to accept images: refuse rather than send one blind to
        // a text-only model, and name both levers that enable it.
        state.apply(UiEvent::Notice(format!(
            "the current model is not known to accept images. To paste images, set \
             `supports_vision = true` for provider '{}' in .localpilot.toml, or enable \
             `[discovery] vision_probe = true` to auto-detect a local vision server.",
            runtime.active_provider_id()
        )));
        return;
    }
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(error) => {
            state.apply(UiEvent::Notice(format!("clipboard unavailable: {error}")));
            return;
        }
    };
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(error) => {
            if clipboard_error_is_missing_image(&error) {
                // Genuinely nothing to paste (e.g. an empty text paste): stay
                // quiet on the empty-paste probe path, but still tell a
                // deliberate Ctrl+V there was no image.
                if !quiet_when_absent {
                    state.apply(UiEvent::Notice("no image on the clipboard".to_string()));
                }
            } else {
                // A real read failure is never swallowed — this is the "nothing
                // happened, no message" case users hit when a paste-routed image
                // fails to decode.
                state.apply(UiEvent::Notice(format!(
                    "couldn't read the clipboard image: {error}"
                )));
            }
            return;
        }
    };
    let width = image.width;
    let height = image.height;
    let png = match encode_png(&image) {
        Ok(png) => png,
        Err(message) => {
            state.apply(UiEvent::Notice(message));
            return;
        }
    };
    let data = base64::engine::general_purpose::STANDARD.encode(&png);
    if data.len() > MAX_IMAGE_BASE64_BYTES {
        state.apply(UiEvent::Notice(
            "clipboard image is too large to attach".to_string(),
        ));
        return;
    }
    let placeholder = state.register_image("image/png", data, png.len());
    state.insert_input(&placeholder);
    state.apply(UiEvent::Notice(format!("attached {width}×{height} image")));
}

/// Whether a clipboard read error means "there is simply no image on the
/// clipboard" (benign — nothing to paste) rather than a real read/decode failure
/// that must always be surfaced to the user.
fn clipboard_error_is_missing_image(error: &arboard::Error) -> bool {
    matches!(error, arboard::Error::ContentNotAvailable)
}

/// Encode arboard's raw RGBA clipboard pixels to PNG bytes.
fn encode_png(image: &arboard::ImageData) -> Result<Vec<u8>, String> {
    use image::{ExtendedColorType, ImageEncoder};
    let width = u32::try_from(image.width).map_err(|_| "image width too large".to_string())?;
    let height = u32::try_from(image.height).map_err(|_| "image height too large".to_string())?;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&image.bytes, width, height, ExtendedColorType::Rgba8)
        .map_err(|error| format!("could not encode image: {error}"))?;
    Ok(out)
}

/// Remove the inline image placeholders from `prompt`; the attachment blocks carry
/// the images, so the model receives clean text.
fn strip_image_placeholders(prompt: &str, images: &[ImageAttachment]) -> String {
    let mut out = prompt.to_string();
    for image in images {
        out = out.replace(&image.placeholder, "");
    }
    out.trim().to_string()
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
        KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Key::CtrlT),
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

/// Diagnostic: with `LOCALPILOT_DEBUG_STREAM=<file>` set, append each raw stream
/// event to that file with the text shown escaped (`{:?}`, so `\n`, `<think>`,
/// and blank runs are visible). Used to find what actually produces "empty lines"
/// in a reply. A no-op when the variable is unset.
fn debug_stream_log(kind: &str, text: &str) {
    if let Some(path) = std::env::var_os("LOCALPILOT_DEBUG_STREAM") {
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "[{kind}] {text:?}");
        }
    }
}

fn map_event(event: RuntimeEvent, elapsed_secs: f64) -> Option<UiEvent> {
    match event {
        RuntimeEvent::Text(text) => {
            debug_stream_log("text", &text);
            Some(UiEvent::TextDelta(text))
        }
        RuntimeEvent::Reasoning(text) => {
            debug_stream_log("reasoning", &text);
            Some(UiEvent::ReasoningDelta(text))
        }
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
        Profile::Unrestricted => UiProfile::Unrestricted,
    }
}

fn sandbox_profile(profile: UiProfile) -> Profile {
    match profile {
        UiProfile::Default => Profile::Default,
        UiProfile::Relaxed => Profile::Relaxed,
        UiProfile::Bypass => Profile::Bypass,
        UiProfile::Unrestricted => Profile::Unrestricted,
    }
}

/// Best-effort context window for `model` from the provider's own model
/// listing, when the provider speaks the OpenAI-compatible protocol and a base
/// URL is known. Silent on failure: discovery is metadata, not a gate.
/// The probe-resolved image-input capability for the active provider — config
/// `supports_vision` wins, else a best-effort read-only `/props` probe, else
/// false — recorded on the runtime so the image-attach preflight honours an
/// undeclared-but-vision-capable local server. Returns `None` (leave the
/// declaration as the sole gate) when no such provider is configured. The probe
/// runs only when `[discovery] vision_probe` is on **and** config did not already
/// declare the capability (a declaration wins, so no probe is needed).
async fn resolved_image_support(
    config: &localpilot_config::Config,
    provider_id: Option<&str>,
) -> Option<bool> {
    let id = provider_id.unwrap_or(&config.provider.default);
    let entry = config.providers.get(id)?;
    let declared = entry.supports_vision;
    let probed = if declared.is_none() && config.discovery.vision_probe {
        match crate::models_cmd::listing_base_url(entry) {
            Some(base_url) => {
                crate::models_cmd::probe_vision_for_provider(config, id, &base_url).await
            }
            None => None,
        }
    } else {
        None
    };
    Some(localpilot_llm::resolve_vision(declared, probed))
}

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
    let base_url = crate::models_cmd::listing_base_url(entry)?;
    let models = crate::models_cmd::discover_models_for_provider(config, id, &base_url)
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
    // A modal blocking prompt (the first-run trust gate, a tool approval) needs
    // more rows than the fixed streaming band so its last line — the [y]/[n]
    // choice — is never clipped below the viewport. Grow to fit it, clamped to
    // the window; every other state keeps the fixed band, so streaming still
    // never resizes the viewport per token.
    let base = LIVE_REGION_HEIGHT.min(size.height.max(1));
    let want_height = blocking_prompt_height(state, size.width)
        .map_or(base, |needed| needed.clamp(base, size.height.max(1)));
    let area = terminal.get_frame().area();
    if area.height != want_height || area.width != size.width {
        resize_viewport(terminal, want_height)?;
    }
    terminal.draw(|frame| render(frame, state))?;
    Ok(())
}

/// Restore the terminal before a panic message prints. A panic under the
/// event loop unwinds past `leave_terminal`, which would leave the user's
/// shell in raw mode with the kitty keyboard flags and bracketed paste still
/// enabled — and print the panic message staircased into the raw-mode screen.
/// The hook undoes the `enter_terminal` state first, then defers to the
/// previous hook.
///
/// Restore runs only when the *driver thread* panics. The event loop is the
/// root future of `Runtime::block_on`, polled on the thread that installed
/// this hook — a panic there is fatal to the session, so restoring is right.
/// A panic on any other thread is a tokio task panic the runtime catches
/// (surfacing as a `JoinError`) while the session keeps running; restoring
/// then would itself break raw-mode input under the live TUI. Installed once,
/// just before raw mode is enabled; every restore operation is a harmless
/// no-op on a terminal that was already restored normally.
fn install_terminal_restore_panic_hook() {
    let driver = std::thread::current().id();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == driver {
            let mut stdout = io::stdout();
            let _ = execute!(
                stdout,
                PopKeyboardEnhancementFlags,
                DisableBracketedPaste,
                crossterm::cursor::Show
            );
            let _ = terminal::disable_raw_mode();
        }
        previous(info);
    }));
}

fn enter_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    terminal::enable_raw_mode()?;
    // Raw mode is on from here: an error in the rest of the setup must not
    // leave the shell raw on the early-return path.
    match enter_terminal_inner() {
        Ok(terminal) => Ok(terminal),
        Err(error) => {
            let _ = terminal::disable_raw_mode();
            Err(error)
        }
    }
}

fn enter_terminal_inner() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    let mut stdout = io::stdout();
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

    #[test]
    fn a_missing_clipboard_image_is_benign_but_a_read_failure_is_surfaced() {
        // "No image on the clipboard" is the quiet, benign case on the
        // empty-paste probe path...
        assert!(clipboard_error_is_missing_image(
            &arboard::Error::ContentNotAvailable
        ));
        // ...but any other error is a real read failure that must be reported,
        // so an image paste never fails silently with no message.
        assert!(!clipboard_error_is_missing_image(
            &arboard::Error::Unknown {
                description: "decode failed".to_string(),
            }
        ));
    }

    fn test_header() -> Header {
        Header {
            version: "0".into(),
            provider: "test".into(),
            model: "test-model".into(),
            workspace: "ws".into(),
            session_id: "session".into(),
            session_name: None,
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
    fn profile_slash_commands_apply_mid_turn_through_the_shared_handle() {
        // A profile switch only reconfigures this side's permission engine, so
        // it is allowlisted for mid-turn execution...
        let action = SlashAction::SetProfile(UiProfile::Unrestricted);
        assert!(is_live_slash(&action));

        // ...and applying it swaps the shared engine (what the runtime
        // snapshots on the next tool call) and the footer profile together.
        let mut state = AppState::new(test_header(), Mode::Agent, UiProfile::Default);
        let handle =
            PermissionEngineHandle::new(PermissionEngine::new(Profile::Default, Vec::new()));
        run_live_slash(&mut state, None, Some(&handle), action);
        assert_eq!(handle.profile(), Profile::Unrestricted);
        assert_eq!(state.profile, UiProfile::Unrestricted);

        // A drive with no handle (compaction, research, harness resume — the
        // last runs its own inner runtime) degrades to a notice and changes
        // neither side.
        let mut state = AppState::new(test_header(), Mode::Agent, UiProfile::Default);
        run_live_slash(
            &mut state,
            None,
            None,
            SlashAction::SetProfile(UiProfile::Bypass),
        );
        assert_eq!(state.profile, UiProfile::Default);
    }

    #[test]
    fn history_persistence_none_disables_the_store_end_to_end() {
        // The config opt-out (`[history] persistence = "none"`) must produce a
        // store that neither reads nor writes: a submit-shaped append is a no-op
        // and load returns nothing, so a full open→submit cycle persists nothing.
        use localpilot_config::HistoryPersistence;
        let off = localpilot_store::PromptHistory::new(HistoryPersistence::None.is_enabled());
        assert!(!off.is_enabled());
        off.append("a prompt with a secret", std::path::Path::new("."))
            .expect("disabled append never errors");
        assert!(off.load().is_empty());
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

    #[test]
    fn resume_replay_keeps_the_conversation_tail_and_skips_noise() {
        use localpilot_core::{Message, Role};
        // Tool traffic, synthetic repairs, and system prompts are noise; only
        // authored user/assistant text is re-shown, bounded to the trailing N.
        let mut messages = vec![
            Message::text(Role::System, "setup prompt"),
            Message::text(Role::User, "repair").into_synthetic("tool repair"),
            Message::text(Role::Tool, "tool result"),
        ];
        for i in 0..6 {
            messages.push(Message::text(Role::User, format!("q{i}")));
            messages.push(Message::text(Role::Assistant, format!("a{i}")));
        }

        let (skipped, shown) = replay_selection(messages, 10);
        assert_eq!(skipped, 2, "12 eligible messages, limit 10");
        assert_eq!(shown.len(), 10);
        assert_eq!(
            shown.first().unwrap().1,
            "q1",
            "oldest shown is the tail start"
        );
        assert_eq!(shown.last().unwrap().1, "a5", "newest message is kept");
        assert!(shown
            .iter()
            .all(|(role, _)| matches!(role, Role::User | Role::Assistant)));
    }

    #[test]
    fn live_slash_allowlist_admits_only_bg_and_think() {
        // The mid-turn key handler runs only commands that touch UI state or the
        // shared background registry — never the borrowed runtime. Everything else
        // must stay queued behind the "run when idle" notice.
        for input in [
            "/bg",
            "/bg list",
            "/bg stop bg-1",
            "/bg stop all",
            "/think",
            "/thinking",
        ] {
            let action = parse_slash(input).expect("parses to an action");
            assert!(
                is_live_slash(&action),
                "{input} should be allowed while a turn is in flight"
            );
        }
        for input in ["/model", "/new", "/clear", "/compact", "/fork", "/quit"] {
            let action = parse_slash(input).expect("parses to an action");
            assert!(
                !is_live_slash(&action),
                "{input} must wait for the turn to finish"
            );
        }
    }
}
