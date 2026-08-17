//! `localpilot chat` — interactive-session entry point and host-neutral helpers.
//!
//! [`run_chat`] is the one interactive-session initializer: it wires up the
//! provider/runtime after config load and launches the full-screen terminal
//! host. This module owns the host-neutral session helpers that host drives —
//! workspace/git status, background knowledge indexing, slash-command execution,
//! image capture, background commands, the self-improvement pump, and session
//! resume — while the full-screen application itself lives in
//! `localpilot-terminal-ui`.

use std::cell::{Cell, RefCell};
use std::io::{self};
use std::time::Instant;

use base64::Engine as _;
use localpilot_config::{CliOverrides, ConfigPaths};
use localpilot_core::{ContentBlock, TokenUsage};
use localpilot_harness::{SessionRuntime, SwitchError};
use localpilot_sandbox::{
    Approver, CommandClass, Effect, Interactivity, PermissionRequest, Profile,
};
use localpilot_slash::{BackgroundCommand, IngestAction, Mode, Profile as UiProfile};
use localpilot_store::Store;
use localpilot_tools::BackgroundProcesses;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::interactive_session::{
    resolved_image_support, ApprovalCall, InteractiveSessionBundle, InteractiveSessionSetup,
    TuiApprover,
};

pub(crate) struct ChatOutcome {
    pub(crate) succeeded: bool,
    pub(crate) presentation: Option<String>,
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
    run_chat_with(model, provider_id, profile, resume, false).await
}

/// [`run_chat`] with the incognito switch. When `incognito`, the session keeps
/// nothing on disk (in-memory store, prompt history off, no closeout/knowledge
/// index/reindex) and every file it creates is gated and reported at the end.
pub async fn run_chat_with(
    model: Option<&str>,
    provider_id: Option<&str>,
    profile: Profile,
    resume: Option<localpilot_core::SessionId>,
    incognito: bool,
) -> anyhow::Result<ChatOutcome> {
    let mut timer = StartupTimer::new();
    let cwd = std::env::current_dir()?;
    let config = localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())?;
    timer.mark("config load");

    // Best-effort retention so `.localpilot/` cannot grow without bound. Errors
    // are ignored — cleanup must never block starting a chat — and it runs before
    // the live region is drawn. Never for incognito: it must not touch the store.
    if config.storage.auto_prune && !incognito {
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
    let setup = InteractiveSessionSetup::resolve_with(cwd, config, profile, incognito).await?;
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

    let mut fullscreen_startup = resume
        .map(|session| {
            prepare_fullscreen_resume(&mut runtime, session)
                .unwrap_or_else(|notice| vec![crate::fullscreen::StartupItem::Notice(notice)])
        })
        .unwrap_or_default();
    if incognito {
        // Session-level informed consent: the acknowledgement for each file
        // creation is a decision, but the boundary is stated up front, including
        // the one thing the end report cannot cover.
        fullscreen_startup.insert(
            0,
            crate::fullscreen::StartupItem::Notice(
                "Incognito: nothing is saved to session memory or LocalMind, and every file \
                 this session creates needs your approval. Files a shell command writes outside \
                 the workspace cannot be tracked or listed. Run `/incognito off` to end and see \
                 what was created."
                    .to_string(),
            ),
        );
    }
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
    // Incognito never persists prompt history, and snapshots the workspace now so
    // the files it creates can be reported at the end. The snapshot lives in a
    // cell the host also drives: `/incognito`/`/incognito off` re-take and clear
    // it, so the exit report is correct however incognito was entered or left.
    let history =
        localpilot_store::PromptHistory::new(!incognito && config.history.persistence.is_enabled());
    let incognito_entry: RefCell<Option<crate::incognito::WorkspaceSnapshot>> =
        RefCell::new(incognito.then(|| crate::incognito::WorkspaceSnapshot::take(&cwd)));
    let deferred_selfimprove_reload = Cell::new(false);

    timer.mark("READY — entering full-screen TUI");
    let git = workspace_git_status(&cwd);
    // The one launch trust decision computed in `resolve`; the runtimes were
    // built from the same snapshot, so gate and runtime cannot diverge.
    let trust_required = setup.trust_required();
    let mut fullscreen_config = config.clone();
    let result = crate::fullscreen::run(
        localpilot_terminal_ui::Header {
            version: env!("LOCALPILOT_VERSION").to_string(),
            provider: selected_provider_id,
            model: model.to_string(),
            workspace: cwd.display().to_string(),
            branch: git.as_ref().map(|status| status.branch.clone()),
            workspace_dirty: git.as_ref().and_then(|status| status.dirty),
            mode: Mode::Agent,
            // A launch-time incognito badge rides the profile label so the header
            // always shows the session is non-persistent.
            profile: if incognito {
                format!("{} · incognito", ui_profile(profile).label())
            } else {
                ui_profile(profile).label().to_string()
            },
            session_id: runtime.session_id().to_string(),
            session_name: resumed_session_name,
        },
        fullscreen_startup,
        crate::fullscreen::HostContext {
            runtime: &mut runtime,
            approval_rx: &mut approval_rx,
            approval_tx: &approval_tx,
            question_rx: &mut question_rx,
            cwd: &cwd,
            history: &history,
            ingest: &config.ingest,
            config: &mut fullscreen_config,
            trust_required,
            deferred_selfimprove_reload: &deferred_selfimprove_reload,
            incognito_entry: &incognito_entry,
        },
    )
    .await;
    // A session still incognito at exit (launched with `--incognito`, or entered
    // via `/incognito` and never turned off) reports what it created and closes
    // out nothing. `/incognito off` already reported and cleared the entry cell,
    // so a session that ended normal reaches the else and closes out as usual.
    if runtime.is_incognito() {
        let before = incognito_entry.borrow_mut().take().unwrap_or_default();
        let after = crate::incognito::WorkspaceSnapshot::take(&cwd);
        let report = crate::incognito::IncognitoReport::assemble(
            &before,
            &after,
            runtime.incognito_ledger(),
        );
        eprint!("{}", report.render(&cwd));
    } else {
        crate::context_inject::close_out(&cwd, runtime.session_id());
    }
    let exit = result?;
    if deferred_selfimprove_reload.get() {
        crate::selfimprove_cmd::reload_after_chat(&cwd, &mut io::stdout())?;
    }
    Ok(ChatOutcome {
        succeeded: !exit.trust_denied,
        presentation: exit.presentation,
    })
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

const MAX_INTERACTIVE_COMMAND_LINES: usize = 1_000;
const MAX_INTERACTIVE_COMMAND_BYTES: usize = 128 * 1024;
const INTERACTIVE_TRUNCATION_MARKER: &str = "[output truncated for interactive display]";

/// The bounded form used by chat workflows that can emit patches or build logs.
/// The CLI subcommands keep their streaming output; only interactive projection is
/// capped so neither host can flood its timeline or retain unbounded text.
pub(crate) fn bounded_command_output_from_buffer(
    output: Vec<u8>,
    result: anyhow::Result<()>,
) -> CommandOutput {
    let normalized = command_output_from_buffer(output, result);
    let mut lines = Vec::new();
    let mut bytes = 0_usize;
    let mut truncated = false;
    for line in normalized.lines {
        let added = line.len().saturating_add(1);
        if lines.len() == MAX_INTERACTIVE_COMMAND_LINES
            || bytes.saturating_add(added) > MAX_INTERACTIVE_COMMAND_BYTES
        {
            truncated = true;
            break;
        }
        bytes = bytes.saturating_add(added);
        lines.push(line);
    }
    if truncated {
        let marker_bytes = INTERACTIVE_TRUNCATION_MARKER.len().saturating_add(1);
        while lines.len() >= MAX_INTERACTIVE_COMMAND_LINES
            || bytes.saturating_add(marker_bytes) > MAX_INTERACTIVE_COMMAND_BYTES
        {
            let Some(removed) = lines.pop() else {
                break;
            };
            bytes = bytes.saturating_sub(removed.len().saturating_add(1));
        }
        lines.push(INTERACTIVE_TRUNCATION_MARKER.to_string());
    }
    CommandOutput {
        lines,
        error: normalized.error,
    }
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

/// How many trailing conversation messages a resume replays into the transcript
/// view. The model's context is fully restored by `load_session` regardless;
/// this only bounds what is re-shown on screen. Matches the `/sessions`
/// listing's recent-10 convention.
const RESUME_REPLAY_MESSAGES: usize = 10;

/// Pick which resumed messages are re-shown: authored user/assistant text plus
/// bounded research results. Other synthetic runtime repairs remain hidden.
/// Keeps only the trailing `limit` and returns how many eligible messages were
/// elided along with the ones to show, oldest-first.
fn replay_selection(
    messages: Vec<localpilot_core::Message>,
    limit: usize,
) -> (usize, Vec<(localpilot_core::Role, String)>) {
    use localpilot_core::Role;
    let shown: Vec<(Role, String)> = messages
        .into_iter()
        .filter(|message| {
            !message.is_synthetic()
                || matches!(
                    message.metadata.synthetic.as_deref(),
                    Some(
                        localpilot_core::RESEARCH_TOPIC_ORIGIN
                            | localpilot_core::RESEARCH_RESULT_ORIGIN
                    )
                )
        })
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

/// Run an ingest subcommand (effectful — several actions mutate workspace state)
/// and return its raw output buffer + result — the presentation-neutral seam
/// consumed by the full-screen presenter.
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

pub(crate) enum SelfImprovePumpResult {
    Finished {
        outcome: crate::selfimprove_cmd::InteractiveOutcome,
        output: Vec<u8>,
        result: anyhow::Result<()>,
    },
    Declined(&'static str),
    Cancelled(Vec<u8>),
}

pub(crate) fn selfimprove_confirmation(
    step: crate::selfimprove_cmd::InteractiveStep,
    action: &localpilot_slash::SelfImproveAction,
    proposal_id: Option<&str>,
) -> Option<(String, String)> {
    match step {
        crate::selfimprove_cmd::InteractiveStep::Approve => {
            let localpilot_slash::SelfImproveAction::Approve { reviewer } = action else {
                return None;
            };
            Some((
                "selfimprove approve".to_string(),
                format!(
                    "approve displayed proposal `{}` as human reviewer `{reviewer}` and promote it onto the current branch",
                    proposal_id.unwrap_or("(unknown)")
                ),
            ))
        }
        crate::selfimprove_cmd::InteractiveStep::Reload => Some((
            "selfimprove reload".to_string(),
            "exit this chat and replace the running LocalPilot with the rebuilt binary".to_string(),
        )),
        _ => None,
    }
}

pub(crate) struct SelfImprovePumpRequest {
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) action: localpilot_slash::SelfImproveAction,
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) step: crate::selfimprove_cmd::InteractiveStep,
    pub(crate) confirmed_proposal_id: Option<String>,
    pub(crate) approval_tx: mpsc::UnboundedSender<ApprovalCall>,
    pub(crate) cancel: CancellationToken,
}

pub(crate) async fn execute_selfimprove_pump(
    request: SelfImprovePumpRequest,
) -> anyhow::Result<SelfImprovePumpResult> {
    let SelfImprovePumpRequest {
        cwd,
        action,
        model,
        provider,
        step,
        confirmed_proposal_id,
        approval_tx,
        cancel,
    } = request;
    if let Some((tool, detail)) =
        selfimprove_confirmation(step, &action, confirmed_proposal_id.as_deref())
    {
        let approver = TuiApprover::new(approval_tx);
        let request = PermissionRequest {
            tool,
            effect: Effect::RunCommand(CommandClass::Destructive),
            interactivity: Interactivity::Interactive,
            trusted: true,
            detail,
        };
        if !approver.approve(&request).await {
            return Ok(SelfImprovePumpResult::Declined(
                if step == crate::selfimprove_cmd::InteractiveStep::Reload {
                    "reload"
                } else {
                    "approval"
                },
            ));
        }
    }

    let mut output = Vec::new();
    let result = if step == crate::selfimprove_cmd::InteractiveStep::Build {
        crate::selfimprove_cmd::run_interactive(
            &cwd,
            &action,
            step,
            confirmed_proposal_id.as_deref(),
            &model,
            &provider,
            &mut output,
        )
        .await
    } else {
        let selected = {
            let run = crate::selfimprove_cmd::run_interactive(
                &cwd,
                &action,
                step,
                confirmed_proposal_id.as_deref(),
                &model,
                &provider,
                &mut output,
            );
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => Some(result),
                () = cancel.cancelled() => None,
            }
        };
        match selected {
            Some(result) => result,
            None => return Ok(SelfImprovePumpResult::Cancelled(output)),
        }
    };
    match result {
        Ok(outcome) => Ok(SelfImprovePumpResult::Finished {
            outcome,
            output,
            result: Ok(()),
        }),
        Err(error) => Ok(SelfImprovePumpResult::Finished {
            outcome: crate::selfimprove_cmd::InteractiveOutcome::Complete,
            output,
            result: Err(error),
        }),
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

/// The manual-compaction result notice for the full-screen host. `ContextUsage`
/// and the cancelled case are applied by the host around it.
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
    fn bounded_command_output_caps_lines_and_bytes_with_one_marker() {
        use std::fmt::Write as _;

        let mut many = String::new();
        for index in 0..MAX_INTERACTIVE_COMMAND_LINES + 50 {
            writeln!(&mut many, "line-{index}").unwrap();
        }
        let output = bounded_command_output_from_buffer(many.into_bytes(), Ok(()));
        assert_eq!(output.lines.len(), MAX_INTERACTIVE_COMMAND_LINES);
        assert_eq!(
            output.lines.last().map(String::as_str),
            Some(INTERACTIVE_TRUNCATION_MARKER)
        );

        let huge = vec![b'x'; MAX_INTERACTIVE_COMMAND_BYTES + 1];
        let output = bounded_command_output_from_buffer(huge, Ok(()));
        let displayed = output.lines.iter().map(String::len).sum::<usize>() + output.lines.len();
        assert!(displayed <= MAX_INTERACTIVE_COMMAND_BYTES);
        assert_eq!(
            output.lines.last().map(String::as_str),
            Some(INTERACTIVE_TRUNCATION_MARKER)
        );
    }

    #[test]
    fn selfimprove_approval_confirmation_names_the_exact_patch_and_reviewer() {
        let action = localpilot_slash::SelfImproveAction::Approve {
            reviewer: "David Smith".to_string(),
        };
        let (tool, detail) = selfimprove_confirmation(
            crate::selfimprove_cmd::InteractiveStep::Approve,
            &action,
            Some("selfimprove/finding-7"),
        )
        .expect("approval confirmation");
        assert_eq!(tool, "selfimprove approve");
        assert!(detail.contains("`selfimprove/finding-7`"));
        assert!(detail.contains("`David Smith`"));
        assert!(selfimprove_confirmation(
            crate::selfimprove_cmd::InteractiveStep::Gate,
            &localpilot_slash::SelfImproveAction::Next,
            None,
        )
        .is_none());
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
    fn resume_replay_keeps_a_bounded_research_result_but_not_other_synthetic_noise() {
        use localpilot_core::{Message, Role, RESEARCH_RESULT_ORIGIN, RESEARCH_TOPIC_ORIGIN};
        let messages = vec![
            Message::text(Role::User, "repair").into_synthetic("tool repair"),
            Message::text(Role::User, "Research this topic: retained topic")
                .into_synthetic(RESEARCH_TOPIC_ORIGIN),
            Message::text(Role::Assistant, "[F1] retained finding")
                .into_synthetic(RESEARCH_RESULT_ORIGIN),
        ];

        let (skipped, shown) = replay_selection(messages, 10);
        assert_eq!(skipped, 0);
        assert_eq!(
            shown,
            vec![
                (
                    Role::User,
                    "Research this topic: retained topic".to_string()
                ),
                (Role::Assistant, "[F1] retained finding".to_string()),
            ]
        );
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
