//! `localpilot chat` — the interactive terminal REPL.
//!
//! [`run_chat`] is the one interactive-session initializer and selects its
//! terminal host only after provider/runtime setup. Full-screen chat is the
//! default; the established inline driver remains an explicit temporary
//! rollback while its terminal matrix is completed. Its rendering and input
//! logic remain unit-tested in `localpilot-tui`; the authoritative full-screen
//! application lives in `localpilot-terminal-ui`.

use std::future::Future;
use std::io::{self, Stdout};
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
use localpilot_core::{ContentBlock, TokenUsage};
use localpilot_harness::{ModelHealth, RuntimeEvent, SessionRuntime, SwitchError};
use localpilot_sandbox::{
    Approver, Decision, Effect, Interactivity, PermissionEngine, PermissionEngineHandle,
    PermissionRequest, Profile,
};
use localpilot_store::Store;
use localpilot_tools::{BackgroundProcesses, UserAnswer, UserQuestion};
use localpilot_tui::{
    banner_text, blocking_prompt_height, handle_input, history_block_text, parse_slash, render,
    AppInput, AppState, BackgroundCommand, BackgroundProcess, Header, ImageAttachment,
    IngestAction, Key, Mode, PlanItem, Profile as UiProfile, QuestionPrompt, SlashAction,
    TrustPrompt, UiEvent,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::interactive_session::{
    resolved_image_support, ApprovalCall, InteractiveSessionBundle, InteractiveSessionSetup,
    QuestionCall, TuiApprover,
};
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

const CHAT_UI_ENV: &str = "LOCALPILOT_CHAT_UI";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatUi {
    Inline,
    Fullscreen,
}

pub(crate) struct ChatOutcome {
    pub(crate) succeeded: bool,
    pub(crate) presentation: Option<String>,
}

impl ChatOutcome {
    const fn success() -> Self {
        Self {
            succeeded: true,
            presentation: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceGitStatus {
    pub(crate) branch: String,
    pub(crate) dirty: Option<bool>,
}

pub(crate) fn workspace_git_status(root: &std::path::Path) -> Option<WorkspaceGitStatus> {
    let branch =
        crate::harness_cmd::git_line(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .or_else(|| {
                crate::harness_cmd::git_line(root, &["rev-parse", "--short", "HEAD"])
                    .map(|commit| format!("detached@{commit}"))
            })?;
    let dirty = crate::harness_cmd::git_line(
        root,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    )
    .map(|status| !status.is_empty());
    Some(WorkspaceGitStatus { branch, dirty })
}

fn selected_chat_ui(value: Option<&std::ffi::OsStr>) -> anyhow::Result<ChatUi> {
    match value.and_then(std::ffi::OsStr::to_str) {
        None | Some("") | Some("fullscreen") => Ok(ChatUi::Fullscreen),
        Some("inline") => Ok(ChatUi::Inline),
        Some(value) => Err(anyhow::anyhow!(
            "invalid {CHAT_UI_ENV} value `{value}`; expected `inline` or `fullscreen`"
        )),
    }
}

/// A question set part-way through being answered.
struct PendingQuestions {
    questions: Vec<UserQuestion>,
    /// Which question is on screen.
    index: usize,
    /// Answers collected so far, one per question already resolved.
    answers: Vec<UserAnswer>,
    reply: oneshot::Sender<Vec<UserAnswer>>,
}

impl PendingQuestions {
    /// The view model for the question currently on screen.
    fn view(&self) -> QuestionPrompt {
        let question = &self.questions[self.index];
        QuestionPrompt {
            header: question.header.clone(),
            question: question.question.clone(),
            options: question
                .options
                .iter()
                .map(|option| (option.label.clone(), option.description.clone()))
                .collect(),
            selected: 0,
            checked: vec![false; question.options.len()],
            multi_select: question.multi_select,
            other: None,
            index: self.index + 1,
            total: self.questions.len(),
        }
    }

    /// Record `answer` and advance. Returns the next question's view model, or
    /// `None` once every question has been answered.
    fn advance(&mut self, answer: UserAnswer) -> Option<QuestionPrompt> {
        self.answers.push(answer);
        self.index += 1;
        (self.index < self.questions.len()).then(|| self.view())
    }

    /// Answer whatever is left as dismissed and send the result.
    fn finish(mut self) {
        while self.answers.len() < self.questions.len() {
            self.answers.push(UserAnswer::Dismissed);
        }
        let _ = self.reply.send(self.answers);
    }
}

/// The channels the turn uses to reach the user: approvals and questions. They
/// travel together because every helper that can run a turn has to be able to
/// service both while it waits.
struct UserChannels {
    approvals: mpsc::UnboundedReceiver<ApprovalCall>,
    questions: mpsc::UnboundedReceiver<QuestionCall>,
}

/// What the event loop is waiting for the user to answer. Named to avoid the
/// `Poll::Pending` the `select!` expansion brings into scope.
enum PendingAsk {
    /// A tool-approval decision.
    Approval(oneshot::Sender<bool>),
    /// A set of `ask_user` questions.
    Questions(PendingQuestions),
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

pub(crate) fn start_session_knowledge_index(
    cwd: &std::path::Path,
    config: &localpilot_config::IngestConfig,
) {
    let Some(mode) = localpilot_localmind::session_open_mode(cwd, config) else {
        return;
    };
    let ingest_root = cwd.to_path_buf();
    let ingest_config = config.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(error) = localpilot_localmind::ingest_run(&ingest_root, &ingest_config, mode) {
            tracing::warn!(
                target: "localpilot::ingest",
                %error,
                "background project-knowledge index build failed; knowledge_search may return no or stale results this session"
            );
        }
    });
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

/// The "no usable model" startup error, enriched with an actionable pointer when
/// LocalBox is available. With no LocalBox (the common case) it is the exact
/// legacy message, so existing behaviour is unchanged.
fn no_model_error(offer: crate::localbox::ModelOffer) -> anyhow::Error {
    let base =
        "no model: pass --model, or set a default in .localpilot.toml ([providers.<id>] model = \"...\")";
    match crate::localbox::offer_message(&offer) {
        None => anyhow::anyhow!("{base}"),
        Some(pointer) => anyhow::anyhow!("{base}\n  {pointer}"),
    }
}

pub async fn run_chat(
    model: Option<&str>,
    provider_id: Option<&str>,
    profile: Profile,
    resume: Option<localpilot_core::SessionId>,
) -> anyhow::Result<ChatOutcome> {
    let mut timer = StartupTimer::new();
    let cwd = std::env::current_dir()?;
    let chat_ui = selected_chat_ui(std::env::var_os(CHAT_UI_ENV).as_deref())?;
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
    let model = match model
        .map(str::to_string)
        .or_else(|| config.resolve_model(provider_id))
    {
        Some(model) => model,
        None => {
            // No usable model configured. If LocalBox is available, enrich the
            // error with an actionable pointer instead of only the bare notice;
            // when it is absent the message is byte-for-byte the legacy one.
            let offer = crate::localbox::offer_for(false, crate::localbox::detect().await);
            return Err(no_model_error(offer));
        }
    };
    let selected_provider_id = provider_id.unwrap_or(&config.provider.default).to_string();
    let setup = InteractiveSessionSetup::resolve(cwd, config, profile).await?;
    timer.mark("provider registry + mcp servers + tools");
    let InteractiveSessionBundle {
        mut runtime,
        approval_tx,
        approvals: mut approval_rx,
        questions: mut question_rx,
    } = setup.build(&selected_provider_id, &model).await?;
    timer.mark("runtime + discovery + context hooks");
    // Keep the setup alive for the whole chat: it owns the shared MCP transports
    // used by the runtime's per-session tool registry.
    let cwd = setup.cwd().to_path_buf();
    let config = setup.config();

    let fullscreen_startup = if chat_ui == ChatUi::Fullscreen {
        resume
            .map(|session| {
                prepare_fullscreen_resume(&mut runtime, session)
                    .unwrap_or_else(|notice| vec![crate::fullscreen::StartupItem::Notice(notice)])
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let resumed_session_name = runtime
        .store()
        .list_sessions()
        .ok()
        .and_then(|sessions| {
            sessions
                .into_iter()
                .find(|entry| entry.id == runtime.session_id())
        })
        .and_then(|entry| entry.name);
    let header = Header {
        version: env!("LOCALPILOT_VERSION").to_string(),
        provider: selected_provider_id,
        model: model.to_string(),
        workspace: cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| cwd.display().to_string()),
        session_id: runtime.session_id().to_string(),
        session_name: resumed_session_name,
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
    let history = localpilot_store::PromptHistory::new(config.history.persistence.is_enabled());
    // The full-screen model loads its bounded prompt history only after drawing
    // a first frame and has no consumer yet for the inline host's eager
    // `@`-mention file list or knowledge-index startup. Enter before those
    // synchronous workspace walks;
    // large directories (notably a user's home directory) must never look like
    // a hung, blank launch. The shared provider/runtime initialization above is
    // still authoritative for both hosts.
    if chat_ui == ChatUi::Fullscreen {
        timer.mark("READY — entering full-screen TUI");
        let git = workspace_git_status(&cwd);
        let trust_required = crate::trust::prompt_required(profile, &cwd);
        let result = crate::fullscreen::run(
            localpilot_terminal_ui::Header {
                version: header.version.clone(),
                provider: header.provider.clone(),
                model: header.model.clone(),
                workspace: cwd.display().to_string(),
                branch: git.as_ref().map(|status| status.branch.clone()),
                workspace_dirty: git.as_ref().and_then(|status| status.dirty),
                mode: Mode::Agent,
                profile: ui_profile(profile).label().to_string(),
                session_id: header.session_id.clone(),
                session_name: header.session_name.clone(),
            },
            fullscreen_startup,
            crate::fullscreen::HostContext {
                runtime: &mut runtime,
                approval_rx: &mut approval_rx,
                question_rx: &mut question_rx,
                cwd: &cwd,
                history: &history,
                ingest: &config.ingest,
                config,
                trust_required,
            },
        )
        .await;
        crate::context_inject::close_out(&cwd, runtime.session_id());
        return result.map(|exit| ChatOutcome {
            succeeded: !exit.trust_denied,
            presentation: exit.presentation,
        });
    }

    let mut prompts = UserChannels {
        approvals: approval_rx,
        questions: question_rx,
    };

    let mut state = AppState::new(header, Mode::Agent, ui_profile(profile));
    // Ask once per folder before doing anything in it; trust is remembered across
    // sessions. Already-trusted folders (and bypass/unrestricted, which are
    // explicit) skip it.
    if crate::trust::prompt_required(profile, &cwd) {
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
        start_session_knowledge_index(&cwd, &config.ingest);
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
                &mut prompts,
                CommandHost {
                    approval_tx,
                    cwd: &cwd,
                    model: &model,
                    provider_id,
                    history: &history,
                    config,
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
    result?;
    Ok(ChatOutcome::success())
}

pub(crate) fn prepare_fullscreen_resume(
    runtime: &mut SessionRuntime,
    session: localpilot_core::SessionId,
) -> Result<Vec<crate::fullscreen::StartupItem>, String> {
    use crate::fullscreen::StartupItem;
    use localpilot_core::Role;

    let mut startup = Vec::new();
    match runtime.load_session(session) {
        Ok(report) => {
            if report.skipped_lines > 0 {
                startup.push(StartupItem::Notice(format!(
                    "recovered session log: skipped {} damaged event line(s); the remaining events are intact",
                    report.skipped_lines
                )));
            }
            if let Ok(messages) = runtime.store().read_transcript(session) {
                let (skipped, shown) = replay_selection(messages, RESUME_REPLAY_MESSAGES);
                if skipped > 0 {
                    startup.push(StartupItem::Notice(format!(
                        "… {skipped} earlier message(s) not shown (context fully restored)"
                    )));
                }
                startup.extend(shown.into_iter().map(|(role, text)| match role {
                    Role::User => StartupItem::User(text),
                    _ => StartupItem::Assistant(text),
                }));
            }
            if let Some(usage) = stored_session_usage(runtime.store(), session) {
                startup.push(StartupItem::Usage {
                    input_tokens: usage.effective_input_tokens(),
                    output_tokens: usage.output_tokens,
                    cached_input_tokens: usage.cache_read_input_tokens,
                });
            }
            let (used, limit) = runtime.context_usage();
            startup.push(StartupItem::ContextUsage { used, limit });
            startup.push(StartupItem::Notice(format!(
                "resumed session {session}; current profile and trust apply"
            )));
        }
        Err(error) => return Err(format!("resume failed: {error}")),
    }
    Ok(startup)
}

pub(crate) fn stored_session_usage(
    store: &Store,
    session: localpilot_core::SessionId,
) -> Option<TokenUsage> {
    let events = store.read_events(session).ok()?;
    let mut usage = None::<TokenUsage>;
    for event in events {
        if let localpilot_store::SessionEventKind::UsageReported {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
        } = event.kind
        {
            usage
                .get_or_insert_with(TokenUsage::default)
                .accumulate(TokenUsage {
                    input_tokens,
                    output_tokens,
                    cache_creation_input_tokens,
                    cache_read_input_tokens,
                });
        }
    }
    usage
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    runtime: &mut SessionRuntime,
    prompts: &mut UserChannels,
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
                        attach_clipboard_image(state, runtime, &host).await;
                    } else if handle_paste_burst(state, &mut paste_burst, key, buffered_after) {
                    } else if slash_picker_exact_submit(state, key) {
                        state.close_slash_picker();
                        submit_current_input(terminal, state, runtime, prompts, &host).await?;
                        submitted = true;
                    } else if slash_picker_captures(state, key) || file_picker_captures(state, key)
                    {
                        if let Some(mapped) = map_key(key) {
                            handle_input(state, AppInput::Key(mapped));
                        }
                    } else if is_newline(key, &state.input) {
                        state.insert_input_newline();
                    } else if is_submit(key, &state.input) {
                        submit_current_input(terminal, state, runtime, prompts, &host).await?;
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
                        attach_clipboard_image(state, runtime, &host).await;
                    } else if let Some(path) = recognized_image_candidate_path(&text) {
                        attach_image_path(state, runtime, &host, &path).await;
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
    prompts: &mut UserChannels,
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
        run_slash(terminal, state, runtime, prompts, host, action).await
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
            run_research_prompt(terminal, state, prompts, host, &model_prompt).await
        } else {
            state.busy = true;
            let outcome = run_turn(
                terminal,
                state,
                runtime,
                prompts,
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
    prompts: &mut UserChannels,
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
                prompts,
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
            state.apply(UiEvent::Notice(compact_result_notice(&summary, force)));
        }
        SlashAction::HarnessResume => {
            state.mode = Mode::Harness;
            state.apply(UiEvent::Notice("running harness resume".to_string()));
            run_harness_command(terminal, state, prompts, host, false).await?;
        }
        SlashAction::WaitResume => {
            state.mode = Mode::Harness;
            state.apply(UiEvent::Notice("checking paused harness run".to_string()));
            run_harness_command(terminal, state, prompts, host, true).await?;
        }
        SlashAction::Model { provider, model } => {
            run_model_command(state, runtime, host.cwd, provider, model).await;
        }
        SlashAction::LocalBoxAdopt => {
            run_localbox_adopt(terminal, state, prompts, host).await?;
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
                run_research_prompt(terminal, state, prompts, host, &topic).await?;
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
        SlashAction::Agents(raw) => {
            let mut output = Vec::new();
            let result = run_agents_slash(host.cwd, &raw, &mut output);
            apply_command_result(state, output, result);
        }
        SlashAction::Skills(raw) => {
            let mut output = Vec::new();
            let result = run_skills_slash(host.cwd, &raw, &mut output).await;
            apply_command_result(state, output, result);
        }
        SlashAction::Background(command) => {
            apply_background_command(state, runtime.background_registry(), command)
        }
        SlashAction::Exit { .. } => state.should_quit = true,
        SlashAction::Invalid { command, reason } => {
            state.apply(UiEvent::Notice(format!("invalid /{command}: {reason}")));
        }
        // The full-screen/pair takeovers are never produced by the inline parser
        // (`parse_slash`), so they are unreachable in this host; the explicit arm
        // keeps the match exhaustive without a wildcard.
        SlashAction::Help
        | SlashAction::Theme(_)
        | SlashAction::Settings(_)
        | SlashAction::Diff(_)
        | SlashAction::Search(_) => {}
        SlashAction::Unknown(command) => {
            state.apply(UiEvent::Notice(format!(
                "unknown slash command: /{command}"
            )));
        }
    }
    Ok(())
}

/// Execute a `/skills …` slash command through the shared `skills` command
/// surface, so the interactive form parses and behaves exactly like
/// `localpilot skills …` (LocalHub#40). Because the TUI owns stdin, a mutation is
/// treated as non-interactive: its impact is disclosed and, without `--yes`, it is
/// refused rather than run unattended. A parse error surfaces clap's usage text as
/// a notice instead of aborting the REPL.
/// `/agents` — the same read-only surface as `localpilot agents`, so the TUI and
/// the CLI cannot drift into two different answers about which agents exist.
pub(crate) fn run_agents_slash(
    cwd: &std::path::Path,
    raw: &str,
    out: &mut dyn std::io::Write,
) -> anyhow::Result<()> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    match tokens.as_slice() {
        [] | ["list"] => crate::agents_cmd::list(cwd, false, out),
        ["show", name] => crate::agents_cmd::show(cwd, name, false, out),
        ["show"] => {
            writeln!(out, "usage: /agents show <name>")?;
            Ok(())
        }
        _ => {
            writeln!(out, "usage: /agents [list | show <name>]")?;
            Ok(())
        }
    }
}

pub(crate) async fn run_skills_slash(
    cwd: &std::path::Path,
    raw: &str,
    out: &mut dyn std::io::Write,
) -> anyhow::Result<()> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    if tokens.is_empty() {
        writeln!(
            out,
            "usage: /skills <list|show|available|install|delete|repo|research> … — see \
             `localpilot skills --help`"
        )?;
        return Ok(());
    }
    match <crate::SkillsSlash as clap::Parser>::try_parse_from(tokens) {
        Ok(parsed) => match parsed.command {
            // Discovery is async (bounded web search); the rest is synchronous.
            crate::ProjectSkillsCommand::Research {
                query,
                global,
                no_web,
            } => {
                let _ = crate::skill_discovery::run_skill_research(
                    cwd,
                    &query.join(" "),
                    global,
                    !no_web,
                    out,
                )
                .await?;
                Ok(())
            }
            other => {
                let _ = crate::skills_cmd::run(other, cwd, false, out)?;
                Ok(())
            }
        },
        Err(err) => {
            writeln!(out, "{err}")?;
            Ok(())
        }
    }
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
        // Same LocalBox pointer the startup path shows, so `/model` on an empty
        // config points the user at a detected local server instead of dead-ending.
        let detected = crate::localbox::detect().await;
        if let Some(pointer) =
            crate::localbox::offer_message(&crate::localbox::offer_for(false, detected.clone()))
        {
            state.apply(UiEvent::Notice(pointer));
        }
        if matches!(detected, crate::localbox::LocalBoxState::Running { .. }) {
            state.apply(UiEvent::Notice(
                "run `/localbox adopt` to add it and use it on the next launch".to_string(),
            ));
        }
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
    let report = switch_model_target(runtime, config, provider_id, model).await;
    state.header.provider = report.provider;
    state.header.model = report.model;
    for notice in report.notices {
        state.apply(UiEvent::Notice(notice));
    }
}

pub(crate) struct ModelSwitchReport {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) notices: Vec<String>,
}

/// Re-point the shared session runtime and report UI-neutral outcomes so both
/// interactive terminal hosts use one provider/model switching authority.
pub(crate) async fn switch_model_target(
    runtime: &mut SessionRuntime,
    config: &localpilot_config::Config,
    provider_id: &str,
    model: Option<String>,
) -> ModelSwitchReport {
    let mut notices = Vec::new();
    let outcome = match runtime.set_active_provider(provider_id) {
        Ok(outcome) => outcome,
        Err(SwitchError::UnknownProvider(id)) => {
            notices.push(format!(
                "/model: provider '{id}' is not configured — try /model to list"
            ));
            // Don't dead-end when LocalBox is available: a local model is one
            // `/localbox adopt` away. Point at the dedicated command rather than
            // overloading `/model` with the start logic.
            match crate::localbox::detect().await {
                crate::localbox::LocalBoxState::Running { .. } => notices.push(
                    "a LocalBox server is running — `/localbox adopt` adds it as the `local` provider"
                        .to_string(),
                ),
                crate::localbox::LocalBoxState::InstalledNotRunning => notices.push(
                    "LocalBox is installed — start a model with `localbox serve <model>` (or `localpilot localbox adopt --serve <model>`), then `/localbox adopt`"
                        .to_string(),
                ),
                crate::localbox::LocalBoxState::NotInstalled => {}
            }
            return model_switch_report(runtime, notices);
        }
        Err(SwitchError::TurnInFlight) => {
            notices.push("/model: a turn is in progress; switch once it finishes".to_string());
            return model_switch_report(runtime, notices);
        }
    };
    // The provider's no-default-model warning surfaces before any model override.
    if let Some(warning) = &outcome.warning {
        notices.push(format!("/model: {warning}"));
    }
    // An explicit model overrides the provider default; validate it best-effort.
    if let Some(model) = model {
        if let Err(error) = runtime.set_active_model(&model) {
            notices.push(format!("/model: {error}"));
            return model_switch_report(runtime, notices);
        }
        if let Some(warning) = unknown_model_warning(config, provider_id, &model).await {
            notices.push(warning);
        }
    }
    // The active provider changed, so re-resolve its image-input capability for the
    // attach preflight (config wins, else a best-effort probe of the new server).
    runtime.set_image_support_override(resolved_image_support(config, Some(provider_id)).await);
    notices.push(format!(
        "switched to provider '{}' · model '{}'",
        runtime.active_provider_id(),
        runtime.active_model()
    ));
    model_switch_report(runtime, notices)
}

fn model_switch_report(runtime: &SessionRuntime, notices: Vec<String>) -> ModelSwitchReport {
    ModelSwitchReport {
        provider: runtime.active_provider_id().to_string(),
        model: runtime.active_model().to_string(),
        notices,
    }
}

/// Best-effort model-id check: when the provider exposes a model listing and the
/// requested model is absent, warn (never fail — the id may be valid but unlisted,
/// or discovery may be offline).
async fn unknown_model_warning(
    config: &localpilot_config::Config,
    provider_id: &str,
    model: &str,
) -> Option<String> {
    let entry = config.providers.get(provider_id)?;
    let base_url = crate::models_cmd::listing_base_url(entry)?;
    if let Ok(models) =
        crate::models_cmd::discover_models_for_provider(config, provider_id, &base_url).await
    {
        if !models.is_empty() && !models.iter().any(|m| m.id == model) {
            return Some(format!(
                "/model: '{model}' is not in {provider_id}'s model list; using it anyway"
            ));
        }
    }
    None
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

/// Run a `/bg` command (effectful: `stop`/`stop all` mutate the registry) and
/// The one UI-neutral execution result shared by every synchronous command:
/// its (partial) output lines plus an optional exact failure text. The inline
/// host adapts it byte/item-equivalently ([`apply_command_output_result`]); the
/// full-screen host converts it into a bounded `CommandReport`. The commands
/// themselves stay effectful — this is only the output-conversion seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandOutput {
    pub lines: Vec<String>,
    pub error: Option<String>,
}

/// Normalize a buffer + result into a [`CommandOutput`]: nonblank lines in order,
/// and the generic `command failed: …` text on `Err` (tree supplies its own
/// distinct error instead of using this).
pub(crate) fn command_output_from_buffer(
    output: Vec<u8>,
    result: anyhow::Result<()>,
) -> CommandOutput {
    let text = String::from_utf8_lossy(&output);
    let lines = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect();
    let error = result.err().map(|error| format!("command failed: {error}"));
    CommandOutput { lines, error }
}

/// The background-command producer as a [`CommandOutput`] (it never fails).
pub(crate) fn background_command_output(
    processes: &BackgroundProcesses,
    command: BackgroundCommand,
) -> CommandOutput {
    CommandOutput {
        lines: background_command_lines(processes, command),
        error: None,
    }
}

/// The inline adapter: push each output line as a Notice in order, then the exact
/// failure text where present — byte/item-equivalent to the pre-seam behaviour.
fn apply_command_output_result(state: &mut AppState, output: CommandOutput) {
    for line in output.lines {
        state.apply(UiEvent::Notice(line));
    }
    if let Some(error) = output.error {
        state.apply(UiEvent::Notice(error));
    }
}

pub(crate) fn background_command_lines(
    processes: &BackgroundProcesses,
    command: BackgroundCommand,
) -> Vec<String> {
    match command {
        BackgroundCommand::List => {
            let listed = processes.list();
            if listed.is_empty() {
                vec!["no background processes".to_string()]
            } else {
                let mut lines = vec![
                    "background processes (stop with /bg stop <id> or /bg stop all):".to_string(),
                ];
                for process in listed {
                    let status = if process.alive { "running" } else { "exited" };
                    lines.push(format!(
                        "  {} [{}] {}s · {}",
                        process.id, status, process.age_secs, process.command
                    ));
                }
                lines
            }
        }
        BackgroundCommand::Stop(id) => {
            if processes.stop_now(&id) {
                vec![format!("stopped background process {id}")]
            } else {
                vec![format!("no background process {id}")]
            }
        }
        BackgroundCommand::StopAll => {
            let count = processes.list().len();
            processes.kill_all();
            vec![format!("stopped {count} background process(es)")]
        }
    }
}

fn apply_background_command(
    state: &mut AppState,
    processes: &BackgroundProcesses,
    command: BackgroundCommand,
) {
    apply_command_output_result(state, background_command_output(processes, command));
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
    match crate::session_cmd::resolve_session_ref_in_store(runtime.store(), id) {
        Ok(session) => load_session_id(state, runtime, session),
        Err(error) => state.apply(UiEvent::Notice(error.to_string())),
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
    let (output, result) = ingest_slash_output(cwd, action);
    apply_command_result(state, output, result);
}

/// Run an ingest subcommand (effectful — several actions mutate workspace state)
/// and return its raw output buffer + result — the presentation-neutral seam
/// shared by the inline host and the full-screen presenter.
pub(crate) fn ingest_slash_output(
    cwd: &std::path::Path,
    action: IngestAction,
) -> (Vec<u8>, anyhow::Result<()>) {
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
    (output, result)
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // Preflight runs before any Busy transition: an early exit posts one notice and
    // returns without entering Busy or starting a walk. Shared with the full-screen
    // host so the decisions and copy cannot drift.
    let (config, mode, start_notice) =
        match crate::ingest_progress::ingest_preflight(cwd, requested_mode, resume) {
            crate::ingest_progress::IngestPreflight::EarlyExit(notice) => {
                state.apply(UiEvent::Notice(notice));
                return Ok(());
            }
            crate::ingest_progress::IngestPreflight::Proceed(prepared) => {
                (prepared.config, prepared.mode, prepared.start_notice)
            }
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

    state.busy = true;
    state.apply(UiEvent::Notice(start_notice));
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

    state.apply(UiEvent::Notice(
        crate::ingest_progress::ingest_result_notice(outcome),
    ));
    draw_ui(terminal, state)?;
    Ok(())
}

/// Drain queued ingestion progress into inline notices through the host-shared
/// throttle. `total`/`bucket` carry the quarter-mark state across calls.
fn drain_ingest_progress(
    state: &mut AppState,
    rx: &mut mpsc::UnboundedReceiver<localpilot_localmind::IngestProgress>,
    total: &mut u64,
    bucket: &mut u64,
) {
    crate::ingest_progress::drain_ingest_progress_with(rx, total, bucket, |message| {
        state.apply(UiEvent::Notice(message));
    });
}

fn apply_command_result(state: &mut AppState, output: Vec<u8>, result: anyhow::Result<()>) {
    apply_command_output_result(state, command_output_from_buffer(output, result));
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
    prompts: &mut UserChannels,
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
        prompts,
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

async fn run_harness_command(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    prompts: &mut UserChannels,
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
            approver: move || Box::new(TuiApprover::new(tx.clone())) as Box<dyn Approver>,
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
        terminal, state, prompts, &mut rx, &cancel, started, None, None, None, operation,
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

/// `/model adopt`: adopt a running LocalBox server into `.localpilot.toml` from
/// inside the session. Detection is a cheap read; the config write is gated
/// through the permission engine, and an `Ask` raises the standard in-session
/// approval prompt — driven by [`drive_runtime_operation`] so the prompt is
/// serviced without deadlocking the input loop (the same pattern turns use). The
/// written provider applies on the next launch; this never silently writes.
async fn run_localbox_adopt(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    prompts: &mut UserChannels,
    host: &CommandHost<'_>,
) -> anyhow::Result<()> {
    let (endpoint, model) = match crate::localbox::detect().await {
        crate::localbox::LocalBoxState::Running { endpoint, model } => (endpoint, model),
        crate::localbox::LocalBoxState::InstalledNotRunning => {
            state.apply(UiEvent::Notice(
                "no running LocalBox server found — run `localbox serve <model>` first".to_string(),
            ));
            return Ok(());
        }
        crate::localbox::LocalBoxState::NotInstalled => {
            state.apply(UiEvent::Notice(
                "LocalBox is not installed (no `localbox` on PATH)".to_string(),
            ));
            return Ok(());
        }
    };

    let path = localpilot_config::project_config_path(host.cwd);
    let overwrite = path.exists();
    let profile = sandbox_profile(state.profile);
    let trusted = state.trusted;
    let tx = host.approval_tx.clone();
    let (_events, mut rx) = broadcast::channel::<RuntimeEvent>(16);
    let cancel = CancellationToken::new();
    let started = std::time::Instant::now();
    let write_endpoint = endpoint.clone();
    let write_path = path.clone();
    state.busy = true;

    let operation = async move {
        let engine = PermissionEngine::new(profile, Vec::new());
        let request = PermissionRequest {
            tool: "localbox adopt".to_string(),
            effect: Effect::WritePath {
                inside_workspace: true,
                overwrite,
                secret_like: false,
            },
            interactivity: Interactivity::Interactive,
            trusted,
            detail: write_path.display().to_string(),
        };
        let approved = match engine.decide(&request) {
            Decision::Allow => true,
            // An `Ask` raises the standard approval prompt; the driving loop
            // renders it and feeds the answer back, so this await never deadlocks.
            Decision::Ask => TuiApprover::new(tx.clone()).approve(&request).await,
            Decision::Deny => false,
        };
        if !approved {
            return Ok::<Option<String>, anyhow::Error>(None);
        }
        crate::localbox::write_local_provider(&write_path, &write_endpoint, model.as_deref())?;
        Ok(Some(write_endpoint))
    };

    let outcome = drive_runtime_operation(
        terminal, state, prompts, &mut rx, &cancel, started, None, None, None, operation,
    )
    .await;
    state.busy = false;
    match outcome? {
        Some(endpoint) => state.apply(UiEvent::Notice(format!(
            "adopted LocalBox at {endpoint} — wrote [providers.local]; it applies on the next `localpilot` launch"
        ))),
        None => state.apply(UiEvent::Notice(
            "adopt declined — no config written".to_string(),
        )),
    }
    Ok(())
}

async fn run_turn(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    runtime: &mut SessionRuntime,
    prompts: &mut UserChannels,
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
        prompts,
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
    prompts: &mut UserChannels,
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

    // What the user has been asked and has not yet answered.
    let mut pending: Option<PendingAsk> = None;
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
            Some(call) = prompts.approvals.recv() => {
                state.apply(UiEvent::ApprovalRequested(call.request));
                pending = Some(PendingAsk::Approval(call.reply));
            }
            Some(call) = prompts.questions.recv() => {
                let questions = PendingQuestions {
                    questions: call.questions,
                    index: 0,
                    answers: Vec::new(),
                    reply: call.reply,
                };
                state.apply(UiEvent::QuestionAsked(questions.view()));
                pending = Some(PendingAsk::Questions(questions));
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

/// Apply one terminal event while a question is open.
///
/// Navigation and typing go through the shared widget in `localpilot-tui`, so
/// the deterministic loop and the REPL cannot drift. Only the two keys that
/// *end* a question are handled here, because the answer has to be read out of
/// the widget before it is cleared — the same division as the approval gate,
/// where the UI shows the prompt and the REPL owns the reply.
fn resolve_question_event(
    state: &mut AppState,
    mut questions: PendingQuestions,
    event: Event,
    cancel: &CancellationToken,
) -> Option<PendingAsk> {
    let Event::Key(key) = event else {
        return Some(PendingAsk::Questions(questions));
    };
    if !is_key_action(key) {
        return Some(PendingAsk::Questions(questions));
    }
    if is_cancel(key) {
        // Cancelling the turn cancels the question: answer what is left as
        // dismissed so the waiting tool call resolves rather than hanging.
        state.apply(UiEvent::QuestionResolved);
        questions.finish();
        cancel.cancel();
        return None;
    }

    let answer = match key.code {
        KeyCode::Enter => {
            let prompt = state.question.as_ref()?;
            if prompt.on_other_row() && prompt.other.is_none() {
                // The first Enter on the free-text row opens text entry.
                None
            } else if let Some(text) = prompt.other.as_ref() {
                let text = text.trim();
                Some(if text.is_empty() {
                    UserAnswer::Dismissed
                } else {
                    UserAnswer::Other(text.to_string())
                })
            } else {
                Some(UserAnswer::Selected(prompt.chosen()))
            }
        }
        KeyCode::Esc => {
            let prompt = state.question.as_ref()?;
            // In text entry, Esc backs out to the list; on the list it skips.
            prompt.other.is_none().then_some(UserAnswer::Dismissed)
        }
        _ => None,
    };

    if let Some(mapped) = map_key(key) {
        handle_input(state, AppInput::Key(mapped));
    }
    let Some(answer) = answer else {
        return Some(PendingAsk::Questions(questions));
    };
    match questions.advance(answer) {
        Some(next) => {
            state.apply(UiEvent::QuestionAsked(next));
            Some(PendingAsk::Questions(questions))
        }
        None => {
            state.apply(UiEvent::QuestionResolved);
            questions.finish();
            None
        }
    }
}

/// Apply a terminal event received mid-turn. Approval dialogs capture their
/// decision keys; otherwise Ctrl-C cancels while ordinary editing and paste
/// events continue updating the next prompt.
#[allow(clippy::too_many_arguments)] // the mid-turn event handler threads these
fn resolve_event(
    state: &mut AppState,
    pending: Option<PendingAsk>,
    event: Event,
    cancel: &CancellationToken,
    steer: Option<&localpilot_harness::SteerQueue>,
    background: Option<&Arc<BackgroundProcesses>>,
    permissions: Option<&PermissionEngineHandle>,
    paste_burst: &mut PasteBurst,
    buffered_after: bool,
) -> Option<PendingAsk> {
    if let Some(PendingAsk::Approval(reply)) = pending {
        let Event::Key(key) = event else {
            return Some(PendingAsk::Approval(reply));
        };
        if !is_key_action(key) {
            return Some(PendingAsk::Approval(reply));
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
            None => Some(PendingAsk::Approval(reply)),
        }
    } else if let Some(questions) = match pending {
        Some(PendingAsk::Questions(questions)) => Some(questions),
        _ => None,
    } {
        resolve_question_event(state, questions, event, cancel)
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

/// Where a captured image came from, so a success notice can name bitmap
/// dimensions or a file name without fabricating one from the other.
pub(crate) enum ImageSource {
    Bitmap { width: usize, height: usize },
    File { name: String },
}

pub(crate) struct CapturedClipboardImage {
    pub(crate) media_type: &'static str,
    pub(crate) data: String,
    pub(crate) byte_len: usize,
    pub(crate) source: ImageSource,
}

impl CapturedClipboardImage {
    /// The user-facing "attached …" notice for this capture.
    pub(crate) fn attach_notice(&self) -> String {
        match &self.source {
            ImageSource::Bitmap { width, height } => format!("attached {width}×{height} image"),
            ImageSource::File { name } => format!("attached image {name}"),
        }
    }
}

impl std::fmt::Debug for CapturedClipboardImage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = formatter.debug_struct("CapturedClipboardImage");
        debug
            .field("media_type", &self.media_type)
            .field(
                "data",
                &format_args!("<{} bytes redacted>", self.data.len()),
            )
            .field("byte_len", &self.byte_len);
        match &self.source {
            ImageSource::Bitmap { width, height } => {
                debug.field("width", width).field("height", height);
            }
            ImageSource::File { name } => {
                debug.field("file", name);
            }
        }
        debug.finish()
    }
}

pub(crate) enum ClipboardImageRead {
    Missing,
    Image(CapturedClipboardImage),
}

/// Image extensions the pasted-path recognizer will intercept as a candidate.
/// This is only a conservative syntactic gate; the media type is always decided
/// from magic bytes, so a mis-named file is still rejected by content.
const SUPPORTED_IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "gif"];

/// Identify a provider-safe image type from its leading magic bytes. Returns
/// `None` for anything that is not PNG, JPEG, WebP, or GIF — including a file
/// with an image extension but non-image content.
pub(crate) fn image_media_type_from_magic(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The cardinality of a clipboard file list. The selector classifies count only;
/// whether the single file is a readable, supported, in-budget image is decided
/// by `load_image_file`, the single content authority.
pub(crate) enum FileListPick {
    None,
    One(std::path::PathBuf),
    Multiple,
}

pub(crate) fn pick_clipboard_image_file(paths: &[std::path::PathBuf]) -> FileListPick {
    match paths {
        [] => FileListPick::None,
        [single] => FileListPick::One(single.clone()),
        _ => FileListPick::Multiple,
    }
}

#[derive(Debug)]
pub(crate) enum ImageLoadError {
    TooLarge,
    Unsupported,
    Unreadable(String),
}

pub(crate) struct LoadedImage {
    pub(crate) media_type: &'static str,
    pub(crate) data: String,
    pub(crate) byte_len: usize,
    pub(crate) file_name: String,
}

/// Whether a raw byte length would base64-encode within the attach ceiling,
/// computed overflow-safely so a preflight never allocates for an oversize file.
fn encoded_base64_len_within_ceiling(raw_len: u64) -> bool {
    match raw_len
        .checked_add(2)
        .map(|padded| padded / 3)
        .and_then(|groups| groups.checked_mul(4))
    {
        Some(encoded) => encoded <= MAX_IMAGE_BASE64_BYTES as u64,
        None => false,
    }
}

/// Read an already-encoded image file and prepare it for attachment. The single
/// content authority: it decides unreadable / unsupported / oversize. The size
/// is preflighted from metadata before the bytes are read.
pub(crate) fn load_image_file(path: &std::path::Path) -> Result<LoadedImage, ImageLoadError> {
    let metadata =
        std::fs::metadata(path).map_err(|error| ImageLoadError::Unreadable(error.to_string()))?;
    if !metadata.is_file() {
        return Err(ImageLoadError::Unreadable("not a regular file".to_string()));
    }
    if !encoded_base64_len_within_ceiling(metadata.len()) {
        return Err(ImageLoadError::TooLarge);
    }
    let bytes =
        std::fs::read(path).map_err(|error| ImageLoadError::Unreadable(error.to_string()))?;
    let media_type = image_media_type_from_magic(&bytes).ok_or(ImageLoadError::Unsupported)?;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    if data.len() > MAX_IMAGE_BASE64_BYTES {
        return Err(ImageLoadError::TooLarge);
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_string());
    Ok(LoadedImage {
        media_type,
        data,
        byte_len: bytes.len(),
        file_name,
    })
}

fn strip_one_quote_pair(text: &str) -> &str {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &text[1..text.len() - 1];
        }
    }
    text
}

/// Recognize a paste that is unambiguously a single existing image-file path:
/// one line, an optional matching quote pair, a supported image extension, and
/// an existing regular file (checked with metadata only — no content read, so
/// capability can be resolved before any bytes are touched). Anything else
/// returns `None` and stays ordinary composer text.
pub(crate) fn recognized_image_candidate_path(text: &str) -> Option<std::path::PathBuf> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let unquoted = strip_one_quote_pair(trimmed);
    if unquoted.is_empty() {
        return None;
    }
    let path = std::path::Path::new(unquoted);
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    if !SUPPORTED_IMAGE_EXTENSIONS.contains(&extension.as_str()) {
        return None;
    }
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Some(path.to_path_buf()),
        _ => None,
    }
}

pub(crate) fn image_unsupported_notice(provider_id: &str) -> String {
    format!(
        "the current model is not known to accept images. To paste images, set \
         `supports_vision = true` for provider '{provider_id}' in .localpilot.toml, or enable \
         `[discovery] vision_probe = true` to auto-detect a local vision server."
    )
}

pub(crate) fn read_clipboard_image() -> Result<ClipboardImageRead, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| format!("clipboard unavailable: {error}"))?;
    // Compute the bitmap read first so its borrow is released before the
    // clipboard is moved into the file-list closure.
    let image_result = clipboard.get_image();
    read_clipboard_image_with(image_result, move || clipboard.get().file_list())
}

/// The clipboard-decision seam, injectable so it can be tested without a real
/// clipboard. A missing bitmap falls back to a copied image file; a file-list
/// read error is surfaced, never swallowed into a benign "missing".
fn read_clipboard_image_with(
    image_result: Result<arboard::ImageData<'_>, arboard::Error>,
    file_list: impl FnOnce() -> Result<Vec<std::path::PathBuf>, arboard::Error>,
) -> Result<ClipboardImageRead, String> {
    match image_result {
        Ok(image) => {
            let width = image.width;
            let height = image.height;
            let png = encode_png(&image)?;
            let data = base64::engine::general_purpose::STANDARD.encode(&png);
            validate_image_base64_size(data.len())?;
            Ok(ClipboardImageRead::Image(CapturedClipboardImage {
                media_type: "image/png",
                data,
                byte_len: png.len(),
                source: ImageSource::Bitmap { width, height },
            }))
        }
        Err(error) if clipboard_error_is_missing_image(&error) => {
            // No bitmap on the clipboard — a copied image file (Windows CF_HDROP,
            // macOS file URLs, Linux X11/XWayland URI lists) is the other shape.
            match file_list() {
                Ok(files) => match pick_clipboard_image_file(&files) {
                    FileListPick::None => Ok(ClipboardImageRead::Missing),
                    FileListPick::Multiple => Err(
                        "multiple files are on the clipboard — copy a single image file."
                            .to_string(),
                    ),
                    FileListPick::One(path) => match load_image_file(&path) {
                        Ok(loaded) => Ok(ClipboardImageRead::Image(CapturedClipboardImage {
                            media_type: loaded.media_type,
                            data: loaded.data,
                            byte_len: loaded.byte_len,
                            source: ImageSource::File {
                                name: loaded.file_name,
                            },
                        })),
                        Err(ImageLoadError::TooLarge) => {
                            Err("that image is too large to attach.".to_string())
                        }
                        Err(ImageLoadError::Unsupported) => Err(
                            "the clipboard file isn't a supported image (PNG, JPEG, WebP, or GIF)."
                                .to_string(),
                        ),
                        Err(ImageLoadError::Unreadable(message)) => {
                            Err(format!("couldn't read the image file: {message}"))
                        }
                    },
                },
                // "No copied file" is the only benign file-list outcome; every
                // other read/backend error must surface, not silently vanish.
                Err(error) if clipboard_error_is_missing_image(&error) => {
                    Ok(ClipboardImageRead::Missing)
                }
                Err(error) => Err(format!("couldn't read the clipboard file list: {error}")),
            }
        }
        Err(error) => Err(format!("couldn't read the clipboard image: {error}")),
    }
}

fn validate_image_base64_size(encoded_len: usize) -> Result<(), String> {
    if encoded_len > MAX_IMAGE_BASE64_BYTES {
        Err("that image is too large to attach.".to_string())
    } else {
        Ok(())
    }
}

/// Resolve the image capability once, refusing (with the two-lever notice) when
/// the active model is not known to accept images. Returns whether to proceed.
async fn ensure_image_capability(
    state: &mut AppState,
    runtime: &mut SessionRuntime,
    host: &CommandHost<'_>,
) -> bool {
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
        state.apply(UiEvent::Notice(image_unsupported_notice(
            runtime.active_provider_id(),
        )));
        return false;
    }
    true
}

/// Read an image from the OS clipboard (a bitmap or a single copied image file)
/// and attach it to the next prompt as a placeholder. Best effort: an
/// unsupported model, an absent image, or a read/oversize failure always
/// surfaces a notice and never disturbs the session.
async fn attach_clipboard_image(
    state: &mut AppState,
    runtime: &mut SessionRuntime,
    host: &CommandHost<'_>,
) {
    if !ensure_image_capability(state, runtime, host).await {
        return;
    }
    let image = match read_clipboard_image() {
        Ok(ClipboardImageRead::Missing) => {
            state.apply(UiEvent::Notice(
                "no image or image file on the clipboard".to_string(),
            ));
            return;
        }
        Ok(ClipboardImageRead::Image(image)) => image,
        Err(message) => {
            state.apply(UiEvent::Notice(message));
            return;
        }
    };
    let notice = image.attach_notice();
    let placeholder = state.register_image(image.media_type, image.data, image.byte_len);
    state.insert_input(&placeholder);
    state.apply(UiEvent::Notice(notice));
}

/// Attach an image identified by a pasted/dropped file path. Capability is
/// resolved before any file bytes are read, so a text-only model gets the
/// capability notice without touching the file, and a mis-named file is rejected
/// by content afterwards.
async fn attach_image_path(
    state: &mut AppState,
    runtime: &mut SessionRuntime,
    host: &CommandHost<'_>,
    path: &std::path::Path,
) {
    if !ensure_image_capability(state, runtime, host).await {
        return;
    }
    match load_image_file(path) {
        Ok(loaded) => {
            let notice = format!("attached image {}", loaded.file_name);
            let placeholder = state.register_image(loaded.media_type, loaded.data, loaded.byte_len);
            state.insert_input(&placeholder);
            state.apply(UiEvent::Notice(notice));
        }
        Err(ImageLoadError::TooLarge) => {
            state.apply(UiEvent::Notice(
                "that image is too large to attach.".to_string(),
            ));
        }
        Err(ImageLoadError::Unsupported) => {
            state.apply(UiEvent::Notice(
                "that file isn't a supported image (PNG, JPEG, WebP, or GIF).".to_string(),
            ));
        }
        Err(ImageLoadError::Unreadable(message)) => {
            state.apply(UiEvent::Notice(format!(
                "couldn't read the image file: {message}"
            )));
        }
    }
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
pub(crate) fn workspace_files(root: &std::path::Path) -> Vec<String> {
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
        RuntimeEvent::ToolStarted { id, name, .. } => Some(UiEvent::ToolStarted { id, name }),
        RuntimeEvent::ToolFinished {
            id,
            name,
            is_error,
            output,
            ..
        } => Some(UiEvent::ToolFinished {
            id,
            name,
            is_error,
            output,
        }),
        RuntimeEvent::Usage(usage) => Some(UiEvent::Usage {
            // The whole prompt the model saw, cached prefix included, so a cache
            // hit does not appear to shrink the input.
            tokens_in: usage.effective_input_tokens(),
            tokens_out: usage.output_tokens,
            tokens_per_sec: if elapsed_secs > 0.0 {
                usage.output_tokens as f64 / elapsed_secs
            } else {
                0.0
            },
            cached_in: usage.cache_read_input_tokens,
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
        // A clean completion settles the plan panel: whatever the model left
        // non-done is no longer live work (LocalHub#20). Abnormal stops
        // (cancel, timeout, degraded, budget) keep the truthful unfinished
        // view untouched.
        RuntimeEvent::Stopped(localpilot_harness::StopReason::Done) => Some(UiEvent::PlanSettled),
        _ => None,
    }
}

/// Render the session's durable event log as an indented tree of lifecycle
/// landmarks: opens, turns, steps, branch closures, and forks.
pub(crate) fn render_session_tree(events: &[localpilot_store::SessionEvent]) -> Vec<String> {
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

pub(crate) fn ui_profile(profile: Profile) -> UiProfile {
    match profile {
        Profile::Default => UiProfile::Default,
        Profile::Relaxed => UiProfile::Relaxed,
        Profile::Bypass => UiProfile::Bypass,
        Profile::Unrestricted => UiProfile::Unrestricted,
    }
}

pub(crate) fn sandbox_profile(profile: UiProfile) -> Profile {
    match profile {
        UiProfile::Default => Profile::Default,
        UiProfile::Relaxed => Profile::Relaxed,
        UiProfile::Bypass => Profile::Bypass,
        UiProfile::Unrestricted => Profile::Unrestricted,
    }
}

/// The manual-compaction result notice, shared by the inline and full-screen
/// hosts so their copy cannot drift. `ContextUsage` and the cancelled case are
/// applied by each host around it.
pub(crate) fn compact_result_notice(
    summary: &localpilot_harness::ManualCompaction,
    force: bool,
) -> String {
    if summary.compacted {
        let fallback = summary
            .fallback_reason
            .as_ref()
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

    #[test]
    fn command_output_from_buffer_keeps_partial_lines_then_the_exact_error() {
        // The shared UI-neutral result for a buffer-producing command with partial
        // output plus a failure: nonblank lines in order, then the exact
        // `command failed: …` text. The inline adapter pushes exactly these as
        // Notices (one per line, then the error), so the buffer inline contract is
        // byte/item-equivalent to the pre-seam behaviour.
        let buffer = b"first line\n\n   \nsecond line\n".to_vec();
        let out = command_output_from_buffer(buffer, Err(anyhow::anyhow!("boom")));
        assert_eq!(
            out.lines,
            vec!["first line".to_string(), "second line".to_string()],
        );
        assert_eq!(out.error, Some("command failed: boom".to_string()));
        // A success carries no error line.
        let ok = command_output_from_buffer(b"only line\n".to_vec(), Ok(()));
        assert_eq!(ok.lines, vec!["only line".to_string()]);
        assert_eq!(ok.error, None);
    }

    #[test]
    fn background_command_lines_match_the_inline_notice_strings() {
        // The presentation-neutral producer returns exactly the strings the inline
        // host pushes as Notices (byte/item-equivalent), one per line, in order.
        let procs = BackgroundProcesses::new();
        assert_eq!(
            background_command_lines(&procs, BackgroundCommand::List),
            vec!["no background processes".to_string()],
        );
        assert_eq!(
            background_command_lines(&procs, BackgroundCommand::Stop("x".to_string())),
            vec!["no background process x".to_string()],
        );
    }
    use localpilot_tui::TranscriptLine;

    #[test]
    fn full_screen_git_status_is_best_effort_and_truthful() {
        let directory = tempfile::tempdir().expect("temporary git workspace");
        let init = std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(directory.path())
            .output()
            .expect("run git init");
        assert!(init.status.success());
        std::fs::write(directory.path().join("draft.txt"), "draft")
            .expect("write untracked fixture");

        let status = workspace_git_status(directory.path()).expect("git status");
        assert_eq!(status.branch, "main");
        assert_eq!(status.dirty, Some(true));
        assert_eq!(
            workspace_git_status(directory.path().join("missing").as_path()),
            None
        );
    }
    use ratatui::backend::TestBackend;

    #[test]
    fn full_screen_host_is_default_with_an_explicit_legacy_rollback() {
        use std::ffi::OsStr;

        assert_eq!(
            selected_chat_ui(None).expect("default host"),
            ChatUi::Fullscreen
        );
        assert_eq!(
            selected_chat_ui(Some(OsStr::new(""))).expect("empty host override"),
            ChatUi::Fullscreen
        );
        assert_eq!(
            selected_chat_ui(Some(OsStr::new("inline"))).expect("inline host"),
            ChatUi::Inline
        );
        assert_eq!(
            selected_chat_ui(Some(OsStr::new("fullscreen"))).expect("full-screen host"),
            ChatUi::Fullscreen
        );
        assert!(selected_chat_ui(Some(OsStr::new("unknown"))).is_err());
    }

    #[test]
    fn full_screen_first_frame_precedes_inline_workspace_enumeration() {
        let source = include_str!("repl.rs");
        let full_screen_entry = source
            .find("if chat_ui == ChatUi::Fullscreen {")
            .expect("full-screen branch");
        let inline_file_walk = source
            .find("state.set_workspace_files(workspace_files(&cwd));")
            .expect("inline workspace file walk");

        assert!(
            full_screen_entry < inline_file_walk,
            "the full-screen host must draw before the inline @-mention scan"
        );
    }

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

    #[test]
    fn clipboard_image_base64_limit_accepts_boundary_and_rejects_oversize() {
        assert_eq!(validate_image_base64_size(MAX_IMAGE_BASE64_BYTES), Ok(()));
        assert_eq!(
            validate_image_base64_size(MAX_IMAGE_BASE64_BYTES + 1),
            Err("that image is too large to attach.".to_string())
        );
    }

    #[test]
    fn captured_clipboard_debug_never_contains_base64() {
        let image = CapturedClipboardImage {
            media_type: "image/png",
            data: "SECRET_CLIPBOARD_BASE64".to_string(),
            byte_len: 12,
            source: ImageSource::Bitmap {
                width: 2,
                height: 3,
            },
        };
        let debug = format!("{image:?}");
        assert!(!debug.contains("SECRET_CLIPBOARD_BASE64"));
        assert!(debug.contains("23 bytes redacted"));
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
        off.append("a prompt with a secret", &[], std::path::Path::new("."))
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
    fn stored_session_usage_sums_every_reported_request() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = Store::open(directory.path());
        let session = localpilot_core::SessionId::new();
        for (input_tokens, output_tokens) in [(100, 20), (7, 3)] {
            store
                .append_event(
                    session,
                    None,
                    localpilot_store::SessionEventKind::UsageReported {
                        input_tokens,
                        output_tokens,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    },
                )
                .expect("append usage");
        }
        assert_eq!(
            stored_session_usage(&store, session),
            Some(TokenUsage {
                input_tokens: 107,
                output_tokens: 23,
                ..TokenUsage::default()
            })
        );
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

    fn ui_state() -> AppState {
        AppState::new(
            Header {
                version: "0.1.0".to_string(),
                provider: "local".to_string(),
                model: "test-model".to_string(),
                workspace: "demo".to_string(),
                session_id: "ab12cd34".to_string(),
                session_name: None,
                update: None,
            },
            Mode::Agent,
            UiProfile::Default,
        )
    }

    fn one_question() -> Vec<UserQuestion> {
        vec![UserQuestion {
            header: Some("Storage".to_string()),
            question: "Which store?".to_string(),
            options: vec![
                localpilot_tools::QuestionOption {
                    label: "SQLite".to_string(),
                    description: None,
                },
                localpilot_tools::QuestionOption {
                    label: "Postgres".to_string(),
                    description: None,
                },
            ],
            multi_select: false,
        }]
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::NONE,
        ))
    }

    fn open_questions(
        state: &mut AppState,
        questions: Vec<UserQuestion>,
    ) -> (PendingAsk, oneshot::Receiver<Vec<UserAnswer>>) {
        let (reply, answer) = oneshot::channel();
        let pending = PendingQuestions {
            questions,
            index: 0,
            answers: Vec::new(),
            reply,
        };
        state.apply(UiEvent::QuestionAsked(pending.view()));
        (PendingAsk::Questions(pending), answer)
    }

    #[tokio::test]
    async fn answering_a_question_replies_on_the_channel_and_clears_the_state() {
        let mut state = ui_state();
        let (pending, answer) = open_questions(&mut state, one_question());
        let cancel = CancellationToken::new();

        // Move to the second option, then confirm.
        let pending = resolve_question_event(
            &mut state,
            unwrap_questions(pending),
            key(KeyCode::Down),
            &cancel,
        );
        let pending = resolve_question_event(
            &mut state,
            unwrap_questions(pending.expect("still open")),
            key(KeyCode::Enter),
            &cancel,
        );
        assert!(pending.is_none(), "the question is resolved");
        assert!(state.question.is_none(), "and cleared from the UI");
        assert_eq!(
            answer.await.unwrap(),
            vec![UserAnswer::Selected(vec!["Postgres".to_string()])]
        );
        assert!(!cancel.is_cancelled(), "answering never cancels the turn");
    }

    #[tokio::test]
    async fn esc_on_the_list_dismisses_without_inventing_an_answer() {
        let mut state = ui_state();
        let (pending, answer) = open_questions(&mut state, one_question());
        let cancel = CancellationToken::new();
        let pending = resolve_question_event(
            &mut state,
            unwrap_questions(pending),
            key(KeyCode::Esc),
            &cancel,
        );
        assert!(pending.is_none());
        assert_eq!(answer.await.unwrap(), vec![UserAnswer::Dismissed]);
    }

    #[tokio::test]
    async fn ctrl_c_during_a_question_cancels_the_turn_and_still_answers_the_call() {
        let mut state = ui_state();
        let (pending, answer) = open_questions(&mut state, one_question());
        let cancel = CancellationToken::new();
        let ctrl_c = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        let pending =
            resolve_question_event(&mut state, unwrap_questions(pending), ctrl_c, &cancel);
        assert!(pending.is_none());
        assert!(cancel.is_cancelled(), "Ctrl-C cancels the turn");
        // The waiting tool call still resolves rather than hanging.
        assert_eq!(answer.await.unwrap(), vec![UserAnswer::Dismissed]);
    }

    /// Unwrap the question arm of a pending ask, for tests that only drive that
    /// path.
    fn unwrap_questions(pending: PendingAsk) -> PendingQuestions {
        match pending {
            PendingAsk::Questions(questions) => questions,
            PendingAsk::Approval(_) => panic!("expected a pending question"),
        }
    }

    #[test]
    fn image_media_type_is_detected_from_magic_bytes_not_extension() {
        assert_eq!(
            image_media_type_from_magic(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0]),
            Some("image/png")
        );
        assert_eq!(
            image_media_type_from_magic(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(
            image_media_type_from_magic(b"GIF89a...."),
            Some("image/gif")
        );
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBPmore");
        assert_eq!(image_media_type_from_magic(&webp), Some("image/webp"));
        // Content that is not a supported image — including a truncated header —
        // is `None`, so extension spoofing cannot pass.
        assert_eq!(image_media_type_from_magic(b"this is just text"), None);
        assert_eq!(image_media_type_from_magic(&[0xFF, 0xD8]), None);
    }

    #[test]
    fn clipboard_file_list_pick_is_cardinality_only() {
        assert!(matches!(pick_clipboard_image_file(&[]), FileListPick::None));
        assert!(matches!(
            pick_clipboard_image_file(&[std::path::PathBuf::from("a.png")]),
            FileListPick::One(_)
        ));
        assert!(matches!(
            pick_clipboard_image_file(&[
                std::path::PathBuf::from("a.png"),
                std::path::PathBuf::from("b.txt"),
            ]),
            FileListPick::Multiple
        ));
    }

    #[test]
    fn load_image_file_is_the_single_content_authority() {
        let dir = tempfile::tempdir().expect("temp dir");
        let png = dir.path().join("ok.png");
        std::fs::write(
            &png,
            [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3],
        )
        .expect("write png");
        let loaded = load_image_file(&png).expect("png loads");
        assert_eq!(loaded.media_type, "image/png");
        assert_eq!(loaded.file_name, "ok.png");

        // A .png whose bytes are not an image is rejected by content.
        let fake = dir.path().join("fake.png");
        std::fs::write(&fake, b"not an image at all").expect("write fake");
        assert!(matches!(
            load_image_file(&fake),
            Err(ImageLoadError::Unsupported)
        ));

        // A missing file is unreadable.
        assert!(matches!(
            load_image_file(&dir.path().join("ghost.png")),
            Err(ImageLoadError::Unreadable(_))
        ));
    }

    #[test]
    fn oversize_is_rejected_by_the_metadata_preflight_without_overflow() {
        assert!(encoded_base64_len_within_ceiling(0));
        assert!(encoded_base64_len_within_ceiling(1024));
        // ~4 MiB raw -> ~5.3 MB base64, over the 5 MB ceiling.
        assert!(!encoded_base64_len_within_ceiling(4 * 1024 * 1024));
        // Would overflow u64 in the * 4 step; treated as too large, not a panic.
        assert!(!encoded_base64_len_within_ceiling(u64::MAX));
    }

    #[test]
    fn recognized_paths_intercept_only_existing_image_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let png = dir.path().join("pic.png");
        std::fs::write(&png, [0x89, 0x50, 0x4E, 0x47]).expect("write png");
        let bare = png.to_string_lossy().into_owned();
        assert_eq!(recognized_image_candidate_path(&bare), Some(png.clone()));
        assert_eq!(
            recognized_image_candidate_path(&format!("\"{bare}\"")),
            Some(png.clone())
        );

        // Ordinary prose and a non-image extension stay text.
        assert_eq!(
            recognized_image_candidate_path("just a sentence mentioning png files"),
            None
        );
        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, b"x").expect("write txt");
        assert_eq!(
            recognized_image_candidate_path(&txt.to_string_lossy()),
            None
        );
        // A non-existent image path is not intercepted.
        assert_eq!(
            recognized_image_candidate_path(&dir.path().join("ghost.png").to_string_lossy()),
            None
        );
    }

    #[test]
    fn attach_notice_names_dimensions_or_a_file_without_fabrication() {
        let bitmap = CapturedClipboardImage {
            media_type: "image/png",
            data: String::new(),
            byte_len: 0,
            source: ImageSource::Bitmap {
                width: 10,
                height: 20,
            },
        };
        assert_eq!(bitmap.attach_notice(), "attached 10×20 image");
        let file = CapturedClipboardImage {
            media_type: "image/png",
            data: String::new(),
            byte_len: 0,
            source: ImageSource::File {
                name: "cat.png".to_string(),
            },
        };
        assert_eq!(file.attach_notice(), "attached image cat.png");
    }

    #[test]
    fn clipboard_read_falls_back_to_files_and_never_swallows_a_read_error() {
        use arboard::Error;

        // (a) no bitmap and no copied file (both ContentNotAvailable) -> Missing.
        let read = read_clipboard_image_with(Err(Error::ContentNotAvailable), || {
            Err(Error::ContentNotAvailable)
        });
        assert!(matches!(read, Ok(ClipboardImageRead::Missing)));

        // (b) no bitmap, one copied PNG file -> a File capture.
        let dir = tempfile::tempdir().expect("temp dir");
        let png = dir.path().join("shot.png");
        std::fs::write(&png, [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 9]).expect("write");
        let png_for_closure = png.clone();
        let read = read_clipboard_image_with(Err(Error::ContentNotAvailable), move || {
            Ok(vec![png_for_closure])
        });
        match read {
            Ok(ClipboardImageRead::Image(image)) => {
                assert_eq!(image.media_type, "image/png");
                assert_eq!(image.attach_notice(), "attached image shot.png");
            }
            _ => panic!("expected a file image capture"),
        }

        // (c) no bitmap, multiple copied files -> the multiple-files error.
        let read = read_clipboard_image_with(Err(Error::ContentNotAvailable), || {
            Ok(vec![
                std::path::PathBuf::from("a.png"),
                std::path::PathBuf::from("b.png"),
            ])
        });
        assert_eq!(
            read.err().as_deref(),
            Some("multiple files are on the clipboard — copy a single image file.")
        );

        // (d) a real file-list read error is surfaced, not swallowed into Missing.
        let read = read_clipboard_image_with(Err(Error::ContentNotAvailable), || {
            Err(Error::ClipboardNotSupported)
        });
        assert!(read
            .err()
            .expect("expected a surfaced error")
            .starts_with("couldn't read the clipboard file list:"));
    }
}
