//! Crossterm host for the backend-neutral full-screen chat model.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::future::Future;
use std::io::{self, Read, Stdout, Write};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    self, BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use localpilot_core::ContentBlock;
use localpilot_harness::{
    ModelHealth, RuntimeEvent, SessionRuntime, SoftInterrupt, SteerQueue, StopReason,
};
use localpilot_store::SessionIndexEntry;
use localpilot_terminal_ui::{
    render, sanitize_text, AppCommand, AppModel, ColorSupport, CompletionCommand, ContentPoint,
    DiffFile, DiffLine, DiffLineKind, Header, HitMap, InputAction, ItemId, ItemKind,
    KeyboardSupport, PairStatus, PairStatusCandidate, PeerPane, PlanEntry,
    QuestionOption as UiQuestionOption, QuestionResponse, RecoveryState, ResultTone, RuntimeUpdate,
    SessionEntry, SessionHeader, SessionSelection, SettingEdit, SettingEntry, StopState,
    SubmittedInput, TakeoverNavigation, TerminalCapabilities, Theme, Timeline, TimelineNavigation,
    TimelinePaneHits, UsageTotals, UserShellCommand, UserShellOutput, VisualRowPart,
};
use localpilot_terminal_ui::{QuestionAction, TrustAction};
use localpilot_tools::{ToolOutputPresentation, UserAnswer, UserQuestion};
use localpilot_tui::{parse_slash_for, SlashAction};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::interactive_session::{resolved_image_support, ApprovalCall, PairPeer, QuestionCall};
use crate::key_input::{
    is_cancel, is_clipboard_image_key, is_key_action, is_unbracketed_paste_newline_key,
    may_be_unbracketed_paste_key, PasteAction, PasteBurst,
};
#[cfg(test)]
use crate::pair_run::PairResultCandidate;
use crate::pair_run::{
    InteractivePairRun, PairAsk, PairAskAnswer, PairAskAnswerError, PairAskId, PairAskKind,
    PairAskRequest, PairPumpEvent, PairResultSnapshot, PairRunState, PairRunStatus,
    PairTerminalStatus, PreparedPairRun,
};
use crate::repl::{switch_model_target, ClipboardImageRead};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const WHEEL_SCROLL_ROWS: isize = 3;
const CHAT_THEME_ENV: &str = "LOCALPILOT_CHAT_THEME";
const CHAT_COPY_ON_SELECT_ENV: &str = "LOCALPILOT_CHAT_COPY_ON_SELECT";
const CHAT_MOUSE_ENV: &str = "LOCALPILOT_CHAT_MOUSE";
const CHAT_SCREEN_READER_ENV: &str = "LOCALPILOT_CHAT_SCREEN_READER";
const CHAT_EDITOR_ENV: &str = "LOCALPILOT_EDITOR";
const MAX_EXTERNAL_EDITOR_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DIFF_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SESSION_CHOOSER_ROWS: usize = 100;
static TERMINAL_MODES_ACTIVE: AtomicBool = AtomicBool::new(false);
static MOUSE_CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);
static KEYBOARD_FLAGS_PUSHED: AtomicBool = AtomicBool::new(false);
static LOCAL_UTC_OFFSET: OnceLock<time::UtcOffset> = OnceLock::new();

pub(crate) fn capture_local_utc_offset() {
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let _ = LOCAL_UTC_OFFSET.set(offset);
}

pub(crate) struct HostContext<'a> {
    pub(crate) runtime: &'a mut SessionRuntime,
    pub(crate) approval_rx: &'a mut mpsc::UnboundedReceiver<ApprovalCall>,
    pub(crate) question_rx: &'a mut mpsc::UnboundedReceiver<QuestionCall>,
    pub(crate) cwd: &'a Path,
    pub(crate) history: &'a localpilot_store::PromptHistory,
    pub(crate) ingest: &'a localpilot_config::IngestConfig,
    pub(crate) config: &'a localpilot_config::Config,
    pub(crate) trust_required: bool,
}

pub(crate) struct PairHostContext<'a> {
    pub(crate) cwd: &'a Path,
    pub(crate) history: &'a localpilot_store::PromptHistory,
    pub(crate) ingest: &'a localpilot_config::IngestConfig,
    pub(crate) config: &'a localpilot_config::Config,
    pub(crate) trust_required: bool,
}

pub(crate) struct PairRestoredExit {
    pub(crate) converged: bool,
    pub(crate) trust_denied: bool,
}

/// A model `ask_user` call being answered one question at a time.
struct PendingQuestions {
    questions: Vec<UserQuestion>,
    index: usize,
    answers: Vec<UserAnswer>,
    reply: oneshot::Sender<Vec<UserAnswer>>,
}

impl PendingQuestions {
    fn show_current(&self, app: &mut AppModel) {
        let question = &self.questions[self.index];
        app.request_question(
            question.header.clone(),
            question.question.clone(),
            question.options.iter().map(|option| UiQuestionOption {
                label: option.label.clone(),
                description: option.description.clone(),
            }),
            question.multi_select,
            self.index + 1,
            self.questions.len(),
        );
    }

    fn advance(&mut self, app: &mut AppModel, answer: UserAnswer) -> bool {
        self.answers.push(answer);
        self.index += 1;
        if self.index < self.questions.len() {
            self.show_current(app);
            false
        } else {
            true
        }
    }

    fn finish(mut self) {
        while self.answers.len() < self.questions.len() {
            self.answers.push(UserAnswer::Dismissed);
        }
        let _ = self.reply.send(self.answers);
    }
}

/// One peer question call being presented one question at a time.
struct PendingPairQuestions {
    id: PairAskId,
    peer: PairPeer,
    questions: Vec<UserQuestion>,
    index: usize,
    answers: Vec<UserAnswer>,
}

enum PairQuestionAdvance {
    Pending,
    Complete,
    Failed,
}

impl PendingPairQuestions {
    fn show_current(&self, app: &mut AppModel) -> bool {
        let question = &self.questions[self.index];
        app.request_question_for(
            pair_pane(self.peer),
            question.header.clone(),
            question.question.clone(),
            question.options.iter().map(|option| UiQuestionOption {
                label: option.label.clone(),
                description: option.description.clone(),
            }),
            question.multi_select,
            self.index + 1,
            self.questions.len(),
        )
    }

    fn advance(&mut self, app: &mut AppModel, answer: UserAnswer) -> PairQuestionAdvance {
        self.answers.push(answer);
        self.index += 1;
        app.clear_dialog();
        if self.index >= self.questions.len() {
            PairQuestionAdvance::Complete
        } else if self.show_current(app) {
            PairQuestionAdvance::Pending
        } else {
            PairQuestionAdvance::Failed
        }
    }

    fn finish(mut self) -> (PairAskId, Vec<UserAnswer>) {
        while self.answers.len() < self.questions.len() {
            self.answers.push(UserAnswer::Dismissed);
        }
        (self.id, self.answers)
    }
}

enum PairDialog {
    Approval { id: PairAskId },
    Questions(PendingPairQuestions),
}

enum PairHostAction {
    None,
    Steer {
        peer: PairPeer,
        text: String,
    },
    Answer {
        id: PairAskId,
        answer: PairAskAnswer,
        abort: bool,
        exit: bool,
    },
    Abort {
        exit: bool,
    },
    Exit,
}

/// Pure translation between the terminal model and the typed collaboration
/// pump. Runtime ownership and terminal I/O remain in the outer loop.
struct PairTerminalAdapter {
    dialog: Option<PairDialog>,
    pump_open: bool,
    terminal: Option<PairTerminalStatus>,
    pending_steers: [VecDeque<ItemId>; 2],
}

impl PairTerminalAdapter {
    fn new() -> Self {
        Self {
            dialog: None,
            pump_open: true,
            terminal: None,
            pending_steers: [VecDeque::new(), VecDeque::new()],
        }
    }

    fn queue_steer(&mut self, peer: PairPeer, item_id: ItemId) {
        self.pending_steers[peer_index(peer)].push_back(item_id);
    }

    fn activate_steer(&mut self, app: &mut AppModel, peer: PairPeer) {
        if let Some(item_id) = self.pending_steers[peer_index(peer)].pop_front() {
            let _ = app.activate_prompt_for(pair_pane(peer), item_id);
        }
    }

    fn reject_latest_steer(&mut self, app: &mut AppModel, peer: PairPeer) {
        if let Some(item_id) = self.pending_steers[peer_index(peer)].pop_back() {
            let _ = app.activate_prompt_for(pair_pane(peer), item_id);
        }
        apply_pair_warning(
            app,
            peer,
            "steering was not delivered because the collaboration was no longer accepting input",
        );
    }

    fn settle_pending_steers(&mut self, app: &mut AppModel) {
        for peer in [PairPeer::A, PairPeer::B] {
            let pending = &mut self.pending_steers[peer_index(peer)];
            let count = pending.len();
            while let Some(item_id) = pending.pop_front() {
                let _ = app.activate_prompt_for(pair_pane(peer), item_id);
            }
            if count > 0 {
                apply_pair_warning(
                    app,
                    peer,
                    format!(
                        "{count} steering message{} not delivered before the collaboration ended",
                        if count == 1 { " was" } else { "s were" }
                    ),
                );
            }
        }
    }

    fn clear_dialog(&mut self, app: &mut AppModel) {
        self.dialog = None;
        app.clear_dialog();
    }

    fn mark_pump_closed(&mut self) {
        self.pump_open = false;
    }

    fn apply_pump_event(&mut self, app: &mut AppModel, event: PairPumpEvent) -> PairHostAction {
        match event {
            PairPumpEvent::Runtime { peer, event } => {
                if matches!(
                    &event,
                    RuntimeEvent::SoftInterruptInjected { source, .. } if source == "user"
                ) {
                    self.activate_steer(app, peer);
                }
                let _ = app.apply_runtime_for(pair_pane(peer), map_runtime_event(event));
            }
            PairPumpEvent::Ask(ask) => return self.install_ask(app, ask),
            PairPumpEvent::Progress(status) => apply_pair_status(app, &status),
            PairPumpEvent::RuntimeLagged { peer, skipped } => {
                apply_pair_warning(
                    app,
                    peer,
                    format!("runtime updates lagged; {skipped} update(s) were skipped"),
                );
            }
            PairPumpEvent::RuntimeClosed { peer } => {
                self.clear_dialog(app);
                apply_pair_warning(app, peer, "runtime event stream closed unexpectedly");
            }
            PairPumpEvent::AskChannelClosed { peer, kind } => {
                self.clear_dialog(app);
                apply_pair_warning(
                    app,
                    peer,
                    format!(
                        "{} request channel closed unexpectedly",
                        pair_ask_kind(kind)
                    ),
                );
            }
            PairPumpEvent::InvariantViolation { detail } => {
                self.clear_dialog(app);
                apply_pair_warning(app, PairPeer::A, format!("collaboration stopped: {detail}"));
                apply_pair_warning(app, PairPeer::B, format!("collaboration stopped: {detail}"));
            }
            PairPumpEvent::Finished { status, result } => {
                self.clear_dialog(app);
                self.record_terminal(app, status, &result);
            }
            PairPumpEvent::DriverFailed { status, result } => {
                // The self-contained result card already carries the failure detail
                // and must remain the final terminal presentation, so no second
                // notice is appended after it.
                self.clear_dialog(app);
                self.record_terminal(app, status, &result);
            }
        }
        PairHostAction::None
    }

    fn install_ask(&mut self, app: &mut AppModel, ask: PairAsk) -> PairHostAction {
        let PairAsk { id, peer, request } = ask;
        if self.dialog.is_some() {
            self.clear_dialog(app);
            apply_pair_warning(
                app,
                peer,
                "a second user request arrived while one was visible",
            );
            return rejected_pair_ask(id, request);
        }
        match request {
            PairAskRequest::Approval(request) => {
                if app.request_approval_for(
                    pair_pane(peer),
                    request.tool,
                    request.target,
                    request.risk_class,
                ) {
                    self.dialog = Some(PairDialog::Approval { id });
                    PairHostAction::None
                } else {
                    apply_pair_warning(app, peer, "the approval dialog could not be displayed");
                    PairHostAction::Answer {
                        id,
                        answer: PairAskAnswer::Approval(false),
                        abort: true,
                        exit: false,
                    }
                }
            }
            PairAskRequest::Questions(questions) => {
                if questions.is_empty() {
                    return PairHostAction::Answer {
                        id,
                        answer: PairAskAnswer::Questions(Vec::new()),
                        abort: false,
                        exit: false,
                    };
                }
                let pending = PendingPairQuestions {
                    id,
                    peer,
                    questions,
                    index: 0,
                    answers: Vec::new(),
                };
                if pending.show_current(app) {
                    self.dialog = Some(PairDialog::Questions(pending));
                    PairHostAction::None
                } else {
                    apply_pair_warning(app, peer, "the question dialog could not be displayed");
                    let (id, answers) = pending.finish();
                    PairHostAction::Answer {
                        id,
                        answer: PairAskAnswer::Questions(answers),
                        abort: true,
                        exit: false,
                    }
                }
            }
        }
    }

    fn record_terminal(
        &mut self,
        app: &mut AppModel,
        status: PairRunStatus,
        result: &PairResultSnapshot,
    ) {
        if let PairRunState::Finished(terminal) = status.state {
            self.terminal = Some(terminal);
            // Ordered terminal handling: settle steering, quiesce both projections'
            // work, apply the final status, then leave exactly one retained result
            // card per timeline.
            self.settle_pending_steers(app);
            let _ = app.apply_runtime_for(
                PeerPane::A,
                RuntimeUpdate::Stopped(pair_terminal_stop_state(terminal)),
            );
            let _ = app.apply_runtime_for(
                PeerPane::B,
                RuntimeUpdate::Stopped(pair_terminal_stop_state(terminal)),
            );
        }
        apply_pair_status(app, &status);
        let tone = result_tone(result.reason);
        for peer in [PairPeer::A, PairPeer::B] {
            let _ = app.append_result_for(pair_pane(peer), render_pair_result(result, peer), tone);
        }
    }
}

/// The honesty tone a terminal reason renders with: only a genuine convergence is a
/// success; a bounded or aborted run is incomplete; everything else is an error.
const fn result_tone(reason: PairTerminalStatus) -> ResultTone {
    match reason {
        PairTerminalStatus::Converged => ResultTone::Success,
        PairTerminalStatus::CapReached | PairTerminalStatus::Aborted => ResultTone::Incomplete,
        PairTerminalStatus::ProtocolError
        | PairTerminalStatus::TimedOut
        | PairTerminalStatus::PeerFailed
        | PairTerminalStatus::ProviderError
        | PairTerminalStatus::BudgetExceeded
        | PairTerminalStatus::NoProgress
        | PairTerminalStatus::DriverFailed
        | PairTerminalStatus::Unknown => ResultTone::Error,
    }
}

/// The retained, inspect-and-copy-only card for one peer: the shared outcome and
/// candidate, then only this peer's raw response. Candidate lines are duplicated on
/// both cards so either the wide split or a later narrow single pane is
/// self-contained.
fn render_pair_result(result: &PairResultSnapshot, peer: PairPeer) -> String {
    let mut lines = vec![result_headline(result)];
    match &result.candidate {
        Some(candidate) => {
            lines.push(format!(
                "Candidate: revision {} (digest {})",
                candidate.revision, candidate.digest
            ));
            lines.push(format!("Artifact: {}", candidate.artifact));
        }
        None => lines.push("Candidate: none was applied.".to_string()),
    }
    let raw = result.raw[peer_index(peer)].as_deref();
    match raw {
        Some(raw) => lines.push(format!(
            "Peer {}'s latest response: {raw}",
            pair_peer_label(peer)
        )),
        None => lines.push(format!(
            "Peer {} produced no response to inspect.",
            pair_peer_label(peer)
        )),
    }
    lines.push("Inspect/copy only; no files or version control were changed.".to_string());
    lines.join("\n")
}

/// The one-line, factual outcome headline for a retained result.
fn result_headline(result: &PairResultSnapshot) -> String {
    let detail = || {
        result
            .detail
            .clone()
            .unwrap_or_else(|| "no detail".to_string())
    };
    match result.reason {
        PairTerminalStatus::Converged => match &result.candidate {
            Some(candidate) => format!("Converged at revision {}.", candidate.revision),
            None => "Converged.".to_string(),
        },
        PairTerminalStatus::CapReached => format!(
            "Round cap reached after {}; no convergence.",
            counted_rounds(result.completed_rounds)
        ),
        PairTerminalStatus::Aborted => "Aborted before convergence.".to_string(),
        PairTerminalStatus::TimedOut => "Timed out before convergence.".to_string(),
        PairTerminalStatus::BudgetExceeded => "Budget exceeded before convergence.".to_string(),
        PairTerminalStatus::NoProgress => {
            "Stopped with no progress before convergence.".to_string()
        }
        PairTerminalStatus::ProtocolError => {
            format!("Protocol error before convergence: {}", detail())
        }
        PairTerminalStatus::PeerFailed => format!("A peer failed before convergence: {}", detail()),
        PairTerminalStatus::ProviderError => {
            format!("Provider error before convergence: {}", detail())
        }
        PairTerminalStatus::DriverFailed => format!("The driver failed: {}", detail()),
        PairTerminalStatus::Unknown => "Finished without convergence.".to_string(),
    }
}

const fn pair_peer_label(peer: PairPeer) -> &'static str {
    match peer {
        PairPeer::A => "A",
        PairPeer::B => "B",
    }
}

/// A round count with grammatical agreement: `1 round`, otherwise `N rounds`.
fn counted_rounds(rounds: u32) -> String {
    format!("{rounds} round{}", if rounds == 1 { "" } else { "s" })
}

fn rejected_pair_ask(id: PairAskId, request: PairAskRequest) -> PairHostAction {
    let answer = match request {
        PairAskRequest::Approval(_) => PairAskAnswer::Approval(false),
        PairAskRequest::Questions(questions) => {
            PairAskAnswer::Questions(vec![UserAnswer::Dismissed; questions.len()])
        }
    };
    PairHostAction::Answer {
        id,
        answer,
        abort: true,
        exit: false,
    }
}

/// Bounded, already-selected state projected into the first full-screen frame.
/// This is deliberately narrower than a runtime event: restored user messages
/// are view state, not newly-executed model events.
pub(crate) enum StartupItem {
    User(String),
    Assistant(String),
    Notice(String),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
    },
    ContextUsage {
        used: usize,
        limit: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopExit {
    Normal,
    TrustDenied,
}

/// A terminal-session result can only be constructed after full-screen modes
/// have been restored. Its presentation is therefore safe for the caller to
/// write into the shell's main buffer.
pub(crate) struct RestoredExit {
    pub(crate) trust_denied: bool,
    pub(crate) presentation: Option<String>,
}

struct ExitDraft {
    trust_denied: bool,
    presentation: Option<String>,
}

impl ExitDraft {
    fn after_restore(self) -> RestoredExit {
        RestoredExit {
            trust_denied: self.trust_denied,
            presentation: self.presentation,
        }
    }
}

fn restore_exit_with(exit: ExitDraft, restore: impl FnOnce()) -> RestoredExit {
    restore();
    exit.after_restore()
}

#[derive(Clone, PartialEq, Eq)]
struct QueuedPrompt {
    text: String,
    attachments: Vec<ContentBlock>,
    item_id: ItemId,
}

#[derive(Clone, PartialEq, Eq)]
struct QueuedShell {
    command: UserShellCommand,
    item_id: ItemId,
}

impl std::fmt::Debug for QueuedShell {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueuedShell")
            .field("command", &self.command)
            .field("item_id", &self.item_id)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
enum QueuedOperation {
    Prompt(QueuedPrompt),
    Shell(QueuedShell),
}

impl QueuedOperation {
    fn item_id(&self) -> ItemId {
        match self {
            Self::Prompt(prompt) => prompt.item_id,
            Self::Shell(shell) => shell.item_id,
        }
    }

    #[cfg(test)]
    fn prompt(&self) -> &QueuedPrompt {
        match self {
            Self::Prompt(prompt) => prompt,
            Self::Shell(_) => panic!("expected queued prompt"),
        }
    }

    #[cfg(test)]
    fn shell(&self) -> &QueuedShell {
        match self {
            Self::Shell(shell) => shell,
            Self::Prompt(_) => panic!("expected queued shell"),
        }
    }
}

impl std::fmt::Debug for QueuedOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Prompt(prompt) => formatter.debug_tuple("Prompt").field(prompt).finish(),
            Self::Shell(shell) => formatter.debug_tuple("Shell").field(shell).finish(),
        }
    }
}

impl std::fmt::Debug for QueuedPrompt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueuedPrompt")
            .field(
                "text",
                &format_args!("<{} bytes redacted>", self.text.len()),
            )
            .field(
                "attachments",
                &format_args!("<{} redacted>", self.attachments.len()),
            )
            .field("item_id", &self.item_id)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageCapabilitySnapshot {
    provider_id: String,
    vision_capable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectionGesture {
    peer: Option<PeerPane>,
    leading: ContentPoint,
    trailing: ContentPoint,
    origin_column: u16,
    origin_row: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollbarTarget {
    Takeover,
    Timeline(Option<PeerPane>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollbarGesture {
    target: ScrollbarTarget,
    grab: u16,
}

#[derive(Debug, Default)]
struct MouseState {
    selection: Option<SelectionGesture>,
    selection_pointer: Option<(u16, u16)>,
    scrollbar: Option<ScrollbarGesture>,
}

impl MouseState {
    fn reset_gesture(&mut self) {
        self.selection = None;
        self.selection_pointer = None;
        self.scrollbar = None;
    }
}

struct WorkspaceFileIndex {
    receiver: std_mpsc::Receiver<Vec<String>>,
    finished: bool,
}

impl WorkspaceFileIndex {
    fn start(root: PathBuf) -> Self {
        let (sender, receiver) = std_mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("localpilot-workspace-files".to_string())
            .spawn(move || {
                let _ = sender.send(crate::repl::workspace_files(&root));
            });
        Self {
            receiver,
            finished: false,
        }
    }

    fn refresh(&mut self, app: &mut AppModel) {
        if self.finished {
            return;
        }
        match self.receiver.try_recv() {
            Ok(files) => {
                app.set_workspace_files(files);
                self.finished = true;
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                app.set_workspace_files(Vec::new());
                self.finished = true;
            }
            Err(std_mpsc::TryRecvError::Empty) => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RoutedEvent {
    Unhandled,
    Handled,
    Copy(String),
    PasteClipboard,
}

#[derive(Clone, PartialEq, Eq)]
enum TrustEventOutcome {
    Pending,
    Copy(String),
    ContinueSession,
    Remember,
    Exit,
    Deny,
}

impl std::fmt::Debug for TrustEventOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => formatter.write_str("Pending"),
            Self::Copy(text) => formatter
                .debug_tuple("Copy")
                .field(&format_args!("<{} bytes redacted>", text.len()))
                .finish(),
            Self::ContinueSession => formatter.write_str("ContinueSession"),
            Self::Remember => formatter.write_str("Remember"),
            Self::Exit => formatter.write_str("Exit"),
            Self::Deny => formatter.write_str("Deny"),
        }
    }
}

pub(crate) async fn run(
    header: Header,
    startup_items: impl IntoIterator<Item = StartupItem>,
    context: HostContext<'_>,
) -> Result<RestoredExit> {
    let started = Instant::now();
    install_panic_restore_hook();
    let mouse_capture = std::env::var(CHAT_MOUSE_ENV)
        .ok()
        .as_deref()
        .and_then(parse_bool_setting)
        .unwrap_or(true);
    let (mut modes, capabilities) = TerminalModes::enter(mouse_capture)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialize full-screen terminal")?;
    terminal.clear().context("clear full-screen terminal")?;
    let mut app = AppModel::new(header, capabilities);
    app.set_command_catalog(fullscreen_command_catalog());
    app.set_command_values(
        "model",
        fullscreen_model_values(context.config, context.runtime.active_provider_id()),
    );
    apply_host_preferences(&mut app);
    for item in startup_items {
        apply_startup_item(&mut app, item);
    }
    if context.trust_required {
        app.require_workspace_trust(context.cwd.display().to_string());
    }
    // Seat an immediately useful frame before reading even the bounded global
    // history store. Workspace scans stay out of this startup seam entirely.
    let _ = draw_synchronized(&mut terminal, &app)?;
    let mut workspace_index = WorkspaceFileIndex::start(context.cwd.to_path_buf());
    if !context.trust_required {
        crate::repl::start_session_knowledge_index(context.cwd, context.ingest);
    }
    let history_entries = context.history.load();
    app.seed_history(
        localpilot_store::project_entries(&history_entries, context.cwd)
            .iter()
            .map(expand_history_entry)
            .collect(),
    );
    let exit_cwd = context.cwd.to_path_buf();
    let result = run_event_loop(
        &mut terminal,
        &mut modes,
        &mut app,
        context,
        &mut workspace_index,
    )
    .await;
    let exit_draft = result.as_ref().ok().map(|exit| match exit {
        LoopExit::Normal => ExitDraft {
            trust_denied: false,
            presentation: Some(exit_presentation(
                &app,
                &exit_cwd,
                started.elapsed(),
                app.print_transcript_on_exit(),
            )),
        },
        LoopExit::TrustDenied => ExitDraft {
            trust_denied: true,
            presentation: None,
        },
    });
    let _ = terminal.show_cursor();
    drop(terminal);
    if let Err(error) = result {
        modes.restore();
        return Err(error);
    }
    let exit_draft = exit_draft.context("full-screen exit outcome was not captured")?;
    Ok(restore_exit_with(exit_draft, || modes.restore()))
}

/// Own the full-screen lifetime for an exact-two collaboration. Every async
/// host close happens only after the terminal has been restored.
pub(crate) async fn run_pair(
    primary: Header,
    secondary: SessionHeader,
    prepared: PreparedPairRun,
    context: PairHostContext<'_>,
) -> Result<PairRestoredExit> {
    install_panic_restore_hook();
    let mouse_capture = std::env::var(CHAT_MOUSE_ENV)
        .ok()
        .as_deref()
        .and_then(parse_bool_setting)
        .unwrap_or(true);
    let (mut modes, capabilities) = match TerminalModes::enter(mouse_capture) {
        Ok(entered) => entered,
        Err(error) => {
            prepared.into_host().close().await;
            return Err(error);
        }
    };
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = match Terminal::new(backend).context("initialize collaboration terminal") {
        Ok(terminal) => terminal,
        Err(error) => {
            modes.restore();
            prepared.into_host().close().await;
            return Err(error);
        }
    };
    if let Err(error) = terminal.clear().context("clear collaboration terminal") {
        restore_pair_terminal(terminal, &mut modes);
        prepared.into_host().close().await;
        return Err(error);
    }

    let mut app = AppModel::new_pair(primary, secondary, capabilities);
    app.set_command_catalog(pair_command_catalog());
    apply_host_preferences(&mut app);
    apply_pair_status(&mut app, &prepared.status());
    if context.trust_required {
        app.require_workspace_trust(context.cwd.display().to_string());
    }
    if let Err(error) = draw_synchronized(&mut terminal, &app) {
        restore_pair_terminal(terminal, &mut modes);
        prepared.into_host().close().await;
        return Err(error);
    }

    let mut workspace_index = WorkspaceFileIndex::start(context.cwd.to_path_buf());
    let history_entries = context.history.load();
    app.seed_history(
        localpilot_store::project_entries(&history_entries, context.cwd)
            .iter()
            .map(expand_history_entry)
            .collect(),
    );
    if context.trust_required {
        let trust = run_pair_trust_loop(
            &mut terminal,
            &mut app,
            context.cwd,
            context.ingest,
            &mut workspace_index,
        );
        match trust {
            Ok(PairTrustOutcome::Accepted) => {}
            Ok(PairTrustOutcome::Exit) => {
                restore_pair_terminal(terminal, &mut modes);
                prepared.into_host().close().await;
                return Ok(PairRestoredExit {
                    converged: false,
                    trust_denied: false,
                });
            }
            Ok(PairTrustOutcome::Denied) => {
                restore_pair_terminal(terminal, &mut modes);
                prepared.into_host().close().await;
                return Ok(PairRestoredExit {
                    converged: false,
                    trust_denied: true,
                });
            }
            Err(error) => {
                restore_pair_terminal(terminal, &mut modes);
                prepared.into_host().close().await;
                return Err(error);
            }
        }
    } else {
        crate::repl::start_session_knowledge_index(context.cwd, context.ingest);
    }

    let _ = app.begin_work_for(PeerPane::A);
    let _ = app.begin_work_for(PeerPane::B);
    let mut run = prepared.spawn();
    let mut adapter = PairTerminalAdapter::new();
    let loop_result = run_pair_event_loop(
        &mut terminal,
        &mut app,
        &mut run,
        &mut adapter,
        context,
        &mut workspace_index,
    )
    .await;
    adapter.clear_dialog(&mut app);
    if loop_result.is_err() || run.is_driver_live() {
        run.abort_and_cancel();
    }
    restore_pair_terminal(terminal, &mut modes);
    let completion = run.shutdown().await;
    match loop_result {
        Ok(()) => Ok(PairRestoredExit {
            converged: completion.terminal_status() == PairTerminalStatus::Converged,
            trust_denied: false,
        }),
        Err(error) => Err(error),
    }
}

fn restore_pair_terminal(
    mut terminal: Terminal<CrosstermBackend<Stdout>>,
    modes: &mut TerminalModes,
) {
    let _ = terminal.show_cursor();
    drop(terminal);
    modes.restore();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairTrustOutcome {
    Accepted,
    Exit,
    Denied,
}

fn run_pair_trust_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppModel,
    cwd: &Path,
    ingest: &localpilot_config::IngestConfig,
    workspace_index: &mut WorkspaceFileIndex,
) -> Result<PairTrustOutcome> {
    let mut mouse_state = MouseState::default();
    let mut paste_burst = PasteBurst::default();
    while app.workspace_trust_pending() {
        workspace_index.refresh(app);
        let hit_map = draw_synchronized(terminal, app)?;
        if !event::poll(EVENT_POLL_INTERVAL).context("poll collaboration trust input")? {
            advance_mouse_selection(app, &hit_map, &mouse_state);
            continue;
        }
        let next = event::read().context("read collaboration trust input")?;
        mouse_state.reset_gesture();
        if let Event::Key(key) = &next {
            if is_key_action(*key) {
                let buffered_after = buffered_after_fullscreen_key(*key)
                    .context("poll after collaboration trust paste key")?;
                if handle_dialog_paste_burst(app, &mut paste_burst, *key, buffered_after, false) {
                    continue;
                }
            }
        }
        match handle_trust_event(app, next, &hit_map) {
            TrustEventOutcome::Pending => {}
            TrustEventOutcome::Copy(text) => copy_to_clipboard(app, text),
            TrustEventOutcome::ContinueSession => {
                accept_workspace_trust(app, cwd, false, crate::trust::remember);
                crate::repl::start_session_knowledge_index(cwd, ingest);
            }
            TrustEventOutcome::Remember => {
                accept_workspace_trust(app, cwd, true, crate::trust::remember);
                crate::repl::start_session_knowledge_index(cwd, ingest);
            }
            TrustEventOutcome::Exit => return Ok(PairTrustOutcome::Exit),
            TrustEventOutcome::Deny => return Ok(PairTrustOutcome::Denied),
        }
    }
    Ok(PairTrustOutcome::Accepted)
}

fn apply_startup_item(app: &mut AppModel, item: StartupItem) {
    match item {
        StartupItem::User(text) => {
            let _ = app.append_prompt(text, None, false);
        }
        StartupItem::Assistant(text) => {
            app.apply_runtime(RuntimeUpdate::Text(text));
            app.apply_runtime(RuntimeUpdate::Stopped(StopState::Done));
        }
        StartupItem::Notice(text) => app.apply_runtime(RuntimeUpdate::Notice(text)),
        StartupItem::Usage {
            input_tokens,
            output_tokens,
            cached_input_tokens,
        } => app.apply_runtime(RuntimeUpdate::Usage {
            input_tokens,
            output_tokens,
            cached_input_tokens,
        }),
        StartupItem::ContextUsage { used, limit } => {
            app.apply_runtime(RuntimeUpdate::ContextUsage { used, limit });
        }
    }
}

fn exit_presentation(app: &AppModel, cwd: &Path, elapsed: Duration, print: bool) -> String {
    let mut sections = Vec::new();
    if print {
        let transcript = visible_transcript(app);
        if !transcript.is_empty() {
            sections.push(transcript);
        }
    }

    let mut summary = vec![
        "LocalPilot".to_string(),
        format!("Session: {}", format_elapsed(elapsed)),
    ];
    if let Some(usage) = app.active_usage() {
        summary.push(format!(
            "Tokens: {} input · {} output",
            format_count(usage.input_tokens),
            format_count(usage.output_tokens)
        ));
        if usage.cached_input_tokens > 0 {
            summary.push(format!(
                "Prompt cache: {} input tokens read",
                format_count(usage.cached_input_tokens)
            ));
        }
    }
    if let Some(status) = crate::repl::workspace_git_status(cwd) {
        let state = match status.dirty {
            Some(true) => " · uncommitted changes",
            Some(false) => " · clean",
            None => "",
        };
        summary.push(format!("Workspace: {}{state}", status.branch));
    }
    summary.push(format!(
        "Resume: localpilot chat --resume {}",
        app.active_session_id()
    ));
    sections.push(summary.join("\n"));
    format!("{}\n", sections.join("\n\n"))
}

fn visible_transcript(app: &AppModel) -> String {
    app.active_timeline()
        .items()
        .iter()
        .filter_map(|item| {
            let label = match item.kind {
                ItemKind::User => "You",
                ItemKind::Assistant => "LocalPilot",
                ItemKind::Reasoning => return None,
                ItemKind::Tool => "Tool",
                ItemKind::Question => "Question",
                ItemKind::Shell => "Shell",
                ItemKind::Notice => "Notice",
                ItemKind::Result => "Result",
            };
            let visible = if item.kind == ItemKind::Tool && !item.expanded {
                item.text.lines().next().unwrap_or_default()
            } else {
                &item.text
            };
            let visible = sanitize_text(visible);
            (!visible.trim().is_empty()).then(|| format!("{label}\n{visible}"))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(character);
    }
    formatted
}

// Both full-screen pickers are generated from the one authoritative
// `localpilot-slash` catalog table (per-host descriptions in global order), so
// the inline, full-screen, and pair catalogs cannot drift.
fn fullscreen_command_catalog() -> Vec<CompletionCommand> {
    host_command_catalog(localpilot_tui::Host::Fullscreen)
}

fn pair_command_catalog() -> Vec<CompletionCommand> {
    host_command_catalog(localpilot_tui::Host::Pair)
}

fn host_command_catalog(host: localpilot_tui::Host) -> Vec<CompletionCommand> {
    localpilot_tui::specs_for(host)
        .into_iter()
        .map(|(name, description)| CompletionCommand {
            name: name.to_string(),
            description: description.to_string(),
        })
        .collect()
}

const fn pair_pane(peer: PairPeer) -> PeerPane {
    match peer {
        PairPeer::A => PeerPane::A,
        PairPeer::B => PeerPane::B,
    }
}

const fn pair_peer(peer: PeerPane) -> PairPeer {
    match peer {
        PeerPane::A => PairPeer::A,
        PeerPane::B => PairPeer::B,
    }
}

const fn peer_index(peer: PairPeer) -> usize {
    match peer {
        PairPeer::A => 0,
        PairPeer::B => 1,
    }
}

const fn pair_ask_kind(kind: PairAskKind) -> &'static str {
    match kind {
        PairAskKind::Approval => "approval",
        PairAskKind::Questions => "question",
    }
}

fn pair_terminal_label(status: PairTerminalStatus) -> &'static str {
    match status {
        PairTerminalStatus::Converged => "Converged",
        PairTerminalStatus::CapReached => "Round cap reached",
        PairTerminalStatus::ProtocolError => "Protocol error",
        PairTerminalStatus::Aborted => "Aborted",
        PairTerminalStatus::TimedOut => "Timed out",
        PairTerminalStatus::PeerFailed => "Peer failed",
        PairTerminalStatus::ProviderError => "Provider error",
        PairTerminalStatus::BudgetExceeded => "Budget exceeded",
        PairTerminalStatus::NoProgress => "No progress",
        PairTerminalStatus::DriverFailed => "Driver failed",
        PairTerminalStatus::Unknown => "Finished",
    }
}

const fn pair_terminal_stop_state(status: PairTerminalStatus) -> StopState {
    match status {
        // Only a genuine convergence is a success; a bounded round-cap run settled
        // without converging.
        PairTerminalStatus::Converged => StopState::Done,
        PairTerminalStatus::CapReached => StopState::Quiesced,
        PairTerminalStatus::Aborted => StopState::Cancelled,
        PairTerminalStatus::TimedOut => StopState::TimedOut,
        PairTerminalStatus::ProviderError => StopState::ProviderError,
        PairTerminalStatus::BudgetExceeded => StopState::BudgetExceeded,
        PairTerminalStatus::NoProgress => StopState::NoProgress,
        PairTerminalStatus::ProtocolError
        | PairTerminalStatus::PeerFailed
        | PairTerminalStatus::DriverFailed
        | PairTerminalStatus::Unknown => StopState::Degraded,
    }
}

fn apply_pair_status(app: &mut AppModel, status: &PairRunStatus) {
    let terminal = match status.state {
        PairRunState::Running => None,
        PairRunState::Finished(terminal) => Some(pair_terminal_label(terminal).to_string()),
    };
    let _ = app.set_pair_status(PairStatus {
        completed_rounds: status.completed_rounds,
        max_rounds: status.max_rounds,
        scheduled: status.scheduled.map(pair_pane),
        candidate: status
            .candidate
            .as_ref()
            .map(|candidate| PairStatusCandidate {
                revision: candidate.revision,
                full_digest: candidate.full_digest.clone(),
            }),
        agreements: status.agreements,
        repairing: status.repairing.map(pair_pane),
        terminal,
    });
}

fn apply_pair_warning(app: &mut AppModel, peer: PairPeer, warning: impl Into<String>) {
    let _ = app.apply_runtime_for(pair_pane(peer), RuntimeUpdate::Warning(warning.into()));
}

fn apply_pair_notice(app: &mut AppModel, notice: impl Into<String>) {
    let Some(peer) = app.active_pair_pane() else {
        return;
    };
    let _ = app.apply_runtime_for(peer, RuntimeUpdate::Notice(notice.into()));
}

fn sorted_session_entries(mut sessions: Vec<SessionIndexEntry>) -> Vec<SessionIndexEntry> {
    sessions.sort_by(|a, b| {
        b.updated_unix
            .cmp(&a.updated_unix)
            .then_with(|| a.id.to_string().cmp(&b.id.to_string()))
    });
    sessions
}

fn latest_other_session(
    sessions: Vec<SessionIndexEntry>,
    current: localpilot_core::SessionId,
) -> Option<localpilot_core::SessionId> {
    sorted_session_entries(sessions)
        .into_iter()
        .find(|entry| entry.id != current)
        .map(|entry| entry.id)
}

fn format_session_updated_at(updated_unix: u64, offset: time::UtcOffset) -> Option<String> {
    let timestamp = i64::try_from(updated_unix).ok()?;
    let updated = time::OffsetDateTime::from_unix_timestamp(timestamp)
        .ok()?
        .to_offset(offset);
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        updated.year(),
        u8::from(updated.month()),
        updated.day(),
        updated.hour(),
        updated.minute()
    ))
}

fn format_session_updated(updated_unix: u64) -> Option<String> {
    format_session_updated_at(
        updated_unix,
        LOCAL_UTC_OFFSET
            .get()
            .copied()
            .unwrap_or(time::UtcOffset::UTC),
    )
}

fn fullscreen_session_entries(
    sessions: Vec<SessionIndexEntry>,
    current: localpilot_core::SessionId,
) -> Vec<SessionEntry> {
    let sessions = sorted_session_entries(sessions);
    let indexed_current = sessions.iter().find(|entry| entry.id == current).cloned();
    let mut sessions = sessions
        .into_iter()
        .take(MAX_SESSION_CHOOSER_ROWS)
        .collect::<Vec<_>>();
    if sessions.iter().all(|entry| entry.id != current) {
        let current_entry = indexed_current.unwrap_or(SessionIndexEntry {
            id: current,
            message_count: 0,
            created_unix: 0,
            updated_unix: 0,
            name: None,
        });
        if sessions.len() == MAX_SESSION_CHOOSER_ROWS {
            sessions.pop();
        }
        sessions.push(current_entry);
    }
    sessions
        .into_iter()
        .map(|entry| SessionEntry {
            selector: entry.id.to_string(),
            name: entry.name,
            message_count: entry.message_count,
            updated: (entry.updated_unix > 0)
                .then(|| format_session_updated(entry.updated_unix))
                .flatten(),
            current: entry.id == current,
        })
        .collect()
}

fn session_name(runtime: &SessionRuntime, session: localpilot_core::SessionId) -> Option<String> {
    runtime
        .store()
        .list_sessions()
        .ok()?
        .into_iter()
        .find(|entry| entry.id == session)
        .and_then(|entry| entry.name)
}

fn sanitized_session_name(name: &str) -> String {
    sanitize_text(name).replace(['\n', '\t'], " ")
}

fn apply_fullscreen_resume(
    app: &mut AppModel,
    session: localpilot_core::SessionId,
    name: Option<String>,
    result: Result<Vec<StartupItem>, String>,
) {
    match result {
        Ok(startup) => {
            app.clear_stashed_draft();
            app.clear_conversation();
            app.set_active_session_id(session.to_string());
            app.set_active_session_name(name.map(|name| sanitized_session_name(&name)));
            for item in startup {
                apply_startup_item(app, item);
            }
        }
        Err(notice) => app.apply_runtime(RuntimeUpdate::Notice(notice)),
    }
}

fn load_fullscreen_session(
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    session: localpilot_core::SessionId,
) {
    let result = crate::repl::prepare_fullscreen_resume(runtime, session);
    let name = result
        .as_ref()
        .ok()
        .and_then(|_| session_name(runtime, session));
    apply_fullscreen_resume(app, session, name, result);
}

fn activate_session_selection(
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    selection: &SessionSelection,
) {
    match crate::session_cmd::resolve_session_ref_in_store(runtime.store(), selection.as_str()) {
        Ok(session) => load_fullscreen_session(app, runtime, session),
        Err(error) => app.apply_runtime(RuntimeUpdate::Notice(error.to_string())),
    }
}

/// Load the working-tree diff, optionally keeping only files whose path contains
/// `filter` (trimmed, ASCII-case-insensitive substring; an empty filter keeps
/// all files), then open the diff takeover. Filtering never mutates file
/// contents. On load failure, open the "Diff unavailable" notice.
fn open_workspace_diff(app: &mut AppModel, cwd: &Path, filter: Option<&str>) {
    match load_workspace_diff(cwd) {
        Ok(files) => app.open_diff(filter_diff_files(files, filter)),
        Err(error) => {
            app.open_diff([DiffFile {
                status: "!".to_string(),
                path: "Diff unavailable".to_string(),
                additions: 0,
                deletions: 0,
                lines: vec![DiffLine {
                    old_line: None,
                    new_line: None,
                    kind: DiffLineKind::Metadata,
                    text: error.to_string(),
                }],
            }]);
        }
    }
}

/// Keep only diff files whose path contains `filter` (trimmed,
/// ASCII-case-insensitive substring). An absent or empty filter keeps them all.
/// File contents are never mutated — only the file list is narrowed.
fn filter_diff_files(mut files: Vec<DiffFile>, filter: Option<&str>) -> Vec<DiffFile> {
    if let Some(needle) = filter.map(|value| value.trim().to_ascii_lowercase()) {
        if !needle.is_empty() {
            files.retain(|file| file.path.to_ascii_lowercase().contains(&needle));
        }
    }
    files
}

/// Apply a full-screen takeover action (help/theme/settings/diff/search) on the
/// idle default host. Factored out of the `execute_fullscreen_slash` match so the
/// idle dispatch seam is unit-testable without constructing a session runtime —
/// production and tests drive this same function. Only the five takeover actions
/// reach it (the grouped call site guarantees that); anything else is a no-op.
fn open_fullscreen_takeover(
    app: &mut AppModel,
    config: &localpilot_config::Config,
    cwd: &Path,
    action: SlashAction,
) {
    match action {
        SlashAction::Help => app.open_help(),
        SlashAction::Theme(None) => app.open_theme_picker(),
        SlashAction::Theme(Some(value)) => match value.parse::<Theme>() {
            Ok(theme) => app.apply_theme(theme),
            Err(error) => app.apply_runtime(RuntimeUpdate::Warning(error.to_string())),
        },
        SlashAction::Settings(None) => {
            let settings = fullscreen_settings(app, config);
            app.open_settings(settings);
        }
        SlashAction::Settings(Some(query)) => {
            let settings = fullscreen_settings(app, config);
            app.open_settings_with_query(settings, &query);
        }
        SlashAction::Diff(filter) => open_workspace_diff(app, cwd, filter.as_deref()),
        SlashAction::Search(query) => app.open_timeline_search(query.unwrap_or_default()),
        _ => {}
    }
}

fn load_workspace_diff(cwd: &Path) -> Result<Vec<DiffFile>> {
    let primary = read_git_diff(cwd, true)?;
    let bytes = if primary.0.success() {
        primary.1
    } else {
        let fallback = read_git_diff(cwd, false)?;
        if !fallback.0.success() {
            return Ok(Vec::new());
        }
        fallback.1
    };
    Ok(parse_unified_diff(&String::from_utf8_lossy(&bytes)))
}

fn read_git_diff(cwd: &Path, against_head: bool) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    let mut command = std::process::Command::new("git");
    command.current_dir(cwd).args([
        "-c",
        "core.quotepath=false",
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--unified=3",
    ]);
    if against_head {
        command.arg("HEAD");
    }
    let mut child = command
        .arg("--")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start git diff")?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .context("capture git diff output")?
        .take(MAX_DIFF_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read git diff output")?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_DIFF_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("workspace diff exceeds 8 MiB");
    }
    let status = child.wait().context("wait for git diff")?;
    Ok((status, bytes))
}

fn parse_unified_diff(input: &str) -> Vec<DiffFile> {
    let mut files = Vec::new();
    let mut current: Option<DiffFile> = None;
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    for raw in input.lines() {
        if let Some(header) = raw.strip_prefix("diff --git ") {
            if let Some(file) = current.take() {
                files.push(file);
            }
            let path = split_git_path_fields(header)
                .last()
                .map_or_else(|| header.to_string(), |path| strip_git_prefix(path, "b/"));
            current = Some(DiffFile {
                status: "M".to_string(),
                path,
                additions: 0,
                deletions: 0,
                lines: Vec::new(),
            });
            old_line = 0;
            new_line = 0;
            continue;
        }
        let Some(file) = current.as_mut() else {
            continue;
        };
        if raw.starts_with("new file mode ") {
            file.status = "A".to_string();
            continue;
        }
        if raw.starts_with("deleted file mode ") {
            file.status = "D".to_string();
            continue;
        }
        if let Some(path) = raw.strip_prefix("rename to ") {
            file.status = "R".to_string();
            file.path = decode_git_path(path);
            continue;
        }
        if let Some(path) = raw.strip_prefix("copy to ") {
            file.status = "C".to_string();
            file.path = decode_git_path(path);
            continue;
        }
        if let Some(path) = raw.strip_prefix("+++ ") {
            if path != "/dev/null" {
                file.path = strip_git_prefix(&decode_git_path(path), "b/");
            }
            continue;
        }
        if raw.starts_with("--- ") || raw.starts_with("index ") {
            continue;
        }
        if raw.starts_with("@@") {
            if let Some((old, new)) = parse_hunk_starts(raw) {
                old_line = old;
                new_line = new;
            }
            file.lines.push(DiffLine {
                old_line: None,
                new_line: None,
                kind: DiffLineKind::Hunk,
                text: raw.to_string(),
            });
            continue;
        }
        let (kind, old, new, text) = if let Some(text) = raw.strip_prefix('+') {
            let line = new_line;
            new_line = new_line.saturating_add(1);
            file.additions = file.additions.saturating_add(1);
            (DiffLineKind::Addition, None, Some(line), text)
        } else if let Some(text) = raw.strip_prefix('-') {
            let line = old_line;
            old_line = old_line.saturating_add(1);
            file.deletions = file.deletions.saturating_add(1);
            (DiffLineKind::Deletion, Some(line), None, text)
        } else if let Some(text) = raw.strip_prefix(' ') {
            let old = old_line;
            let new = new_line;
            old_line = old_line.saturating_add(1);
            new_line = new_line.saturating_add(1);
            (DiffLineKind::Context, Some(old), Some(new), text)
        } else {
            (DiffLineKind::Metadata, None, None, raw)
        };
        file.lines.push(DiffLine {
            old_line: old,
            new_line: new,
            kind,
            text: text.to_string(),
        });
    }
    if let Some(file) = current {
        files.push(file);
    }
    files
}

fn split_git_path_fields(input: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in input.chars() {
        if quoted {
            if escaped {
                current.push('\\');
                current.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            } else {
                current.push(character);
            }
        } else if character == '"' && current.is_empty() {
            quoted = true;
        } else if character.is_whitespace() {
            if !current.is_empty() {
                fields.push(decode_git_quoted(&current));
                current.clear();
            }
        } else {
            current.push(character);
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        fields.push(decode_git_quoted(&current));
    }
    fields
}

fn decode_git_path(input: &str) -> String {
    let trimmed = input.trim();
    if let Some(body) = trimmed
        .strip_prefix('"')
        .and_then(|body| body.strip_suffix('"'))
    {
        decode_git_quoted(body)
    } else {
        trimmed.to_string()
    }
}

fn decode_git_quoted(input: &str) -> String {
    let input = input.as_bytes();
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] != b'\\' || index + 1 >= input.len() {
            output.push(input[index]);
            index += 1;
            continue;
        }
        index += 1;
        match input[index] {
            b'a' => output.push(0x07),
            b'b' => output.push(0x08),
            b't' => output.push(b'\t'),
            b'n' => output.push(b'\n'),
            b'v' => output.push(0x0b),
            b'f' => output.push(0x0c),
            b'r' => output.push(b'\r'),
            digit @ b'0'..=b'7' => {
                let mut value = digit - b'0';
                let mut digits = 1;
                while digits < 3
                    && index + 1 < input.len()
                    && matches!(input[index + 1], b'0'..=b'7')
                {
                    index += 1;
                    value = value.saturating_mul(8).saturating_add(input[index] - b'0');
                    digits += 1;
                }
                output.push(value);
            }
            escaped => output.push(escaped),
        }
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn strip_git_prefix(path: &str, prefix: &str) -> String {
    path.strip_prefix(prefix).unwrap_or(path).to_string()
}

fn parse_hunk_starts(header: &str) -> Option<(u32, u32)> {
    let mut fields = header.split_whitespace();
    (fields.next()? == "@@").then_some(())?;
    let old = fields
        .next()?
        .strip_prefix('-')?
        .split(',')
        .next()?
        .parse()
        .ok()?;
    let new = fields
        .next()?
        .strip_prefix('+')?
        .split(',')
        .next()?
        .parse()
        .ok()?;
    Some((old, new))
}

fn fullscreen_settings(app: &AppModel, config: &localpilot_config::Config) -> Vec<SettingEntry> {
    let enabled = |value| if value { "On" } else { "Off" }.to_string();
    vec![
        SettingEntry {
            section: "Input".to_string(),
            name: "Mouse reporting".to_string(),
            value: enabled(app.capabilities.mouse_capture),
            description: format!(
                "Set {CHAT_MOUSE_ENV}=false before launch for keyboard-only input."
            ),
            edit: None,
            is_default: true,
        },
        SettingEntry {
            section: "Input".to_string(),
            name: "Copy on selection".to_string(),
            value: enabled(app.copy_on_select()),
            description: format!(
                "Set {CHAT_COPY_ON_SELECT_ENV}=true to copy immediately after a drag selection."
            ),
            edit: Some(SettingEdit::CopyOnSelect),
            is_default: app.copy_on_select_is_default(),
        },
        SettingEntry {
            section: "Input".to_string(),
            name: "Clipboard".to_string(),
            value: if app.capabilities.clipboard_write {
                "Available"
            } else {
                "Unavailable"
            }
            .to_string(),
            description: "Ctrl+C or timeline right-click copies; composer right-click pastes text when the platform clipboard is available."
                .to_string(),
            edit: None,
            is_default: true,
        },
        SettingEntry {
            section: "Input".to_string(),
            name: "Keyboard protocol".to_string(),
            value: match app.capabilities.keyboard {
                KeyboardSupport::Basic => "Basic",
                KeyboardSupport::Enhanced => "Enhanced",
            }
            .to_string(),
            description: "Enhanced reporting distinguishes more modified key combinations."
                .to_string(),
            edit: None,
            is_default: true,
        },
        SettingEntry {
            section: "Input".to_string(),
            name: "Prompt history".to_string(),
            value: enabled(config.history.persistence.is_enabled()),
            description: "Prompt recall follows the resolved LocalPilot history configuration."
                .to_string(),
            edit: None,
            is_default: true,
        },
        SettingEntry {
            section: "Input".to_string(),
            name: "Compact paste".to_string(),
            value: "On".to_string(),
            description: "Multiline pastes stay atomic and compact until submission.".to_string(),
            edit: None,
            is_default: true,
        },
        SettingEntry {
            section: "Accessibility".to_string(),
            name: "Screen reader".to_string(),
            value: enabled(app.capabilities.screen_reader),
            description: format!(
                "Set {CHAT_SCREEN_READER_ENV}=true for a role-labeled full-screen projection."
            ),
            edit: None,
            is_default: true,
        },
        SettingEntry {
            section: "Navigation".to_string(),
            name: "Tabs".to_string(),
            value: app
                .tabs
                .iter()
                .map(|tab| tab.label())
                .collect::<Vec<_>>()
                .join(", "),
            description: "Only tabs backed by an active LocalPilot surface are shown.".to_string(),
            edit: None,
            is_default: true,
        },
        SettingEntry {
            section: "Appearance".to_string(),
            name: "Color mode".to_string(),
            value: app.theme.display_name().to_string(),
            description: format!(
                "Use /theme to preview modes or set {CHAT_THEME_ENV} before launch."
            ),
            edit: Some(SettingEdit::Theme),
            is_default: app.theme_is_default(),
        },
        SettingEntry {
            section: "Maintenance".to_string(),
            name: "Updates".to_string(),
            value: "Manual".to_string(),
            description: "Use the LocalPilot update command to check and apply releases."
                .to_string(),
            edit: None,
            is_default: true,
        },
        SettingEntry {
            section: "Session".to_string(),
            name: "Provider".to_string(),
            value: app.active_provider().to_string(),
            description: "The provider currently serving this conversation.".to_string(),
            edit: None,
            is_default: true,
        },
        SettingEntry {
            section: "Session".to_string(),
            name: "Model".to_string(),
            value: app.active_model().to_string(),
            description: "Use /model to choose from configured LocalPilot providers.".to_string(),
            edit: None,
            is_default: true,
        },
        SettingEntry {
            section: "Session".to_string(),
            name: "Mode and profile".to_string(),
            value: format!("{} · {}", app.shared_mode(), app.shared_profile()),
            description: "The active LocalPilot execution mode and permission profile.".to_string(),
            edit: None,
            is_default: true,
        },
    ]
}

fn fullscreen_model_values(
    config: &localpilot_config::Config,
    active_provider: &str,
) -> Vec<CompletionCommand> {
    config
        .providers
        .iter()
        .map(|(id, provider)| {
            let active = if id == active_provider {
                "current · "
            } else {
                ""
            };
            let model = provider.model.as_deref().unwrap_or("provider default");
            CompletionCommand {
                name: id.clone(),
                description: format!("{active}{} · {model}", provider.kind),
            }
        })
        .collect()
}

fn image_content_blocks(images: Vec<localpilot_terminal_ui::ImageAttachment>) -> Vec<ContentBlock> {
    images
        .into_iter()
        .map(|image| ContentBlock::image(image.media_type, image.data))
        .collect()
}

fn apply_host_preferences(app: &mut AppModel) {
    apply_theme_preference(app, std::env::var_os(CHAT_THEME_ENV));
    if let Some(value) = std::env::var_os(CHAT_COPY_ON_SELECT_ENV) {
        match value.into_string() {
            Ok(value) => match parse_bool_setting(&value) {
                Some(enabled) => app.set_copy_on_select(enabled),
                None => app.apply_runtime(RuntimeUpdate::Warning(format!(
                    "{CHAT_COPY_ON_SELECT_ENV} must be true, false, 1, or 0; using false"
                ))),
            },
            Err(_) => app.apply_runtime(RuntimeUpdate::Warning(format!(
                "{CHAT_COPY_ON_SELECT_ENV} must be true, false, 1, or 0; using false"
            ))),
        }
    }
    if std::env::var_os(CHAT_MOUSE_ENV).is_some()
        && std::env::var(CHAT_MOUSE_ENV)
            .ok()
            .as_deref()
            .and_then(parse_bool_setting)
            .is_none()
    {
        app.apply_runtime(RuntimeUpdate::Warning(format!(
            "{CHAT_MOUSE_ENV} must be true, false, 1, or 0; using true"
        )));
    }
    if std::env::var_os(CHAT_SCREEN_READER_ENV).is_some()
        && std::env::var(CHAT_SCREEN_READER_ENV)
            .ok()
            .as_deref()
            .and_then(parse_bool_setting)
            .is_none()
    {
        app.apply_runtime(RuntimeUpdate::Warning(format!(
            "{CHAT_SCREEN_READER_ENV} must be true, false, 1, or 0; using false"
        )));
    }
    app.capture_setting_defaults();
}

fn apply_theme_preference(app: &mut AppModel, value: Option<OsString>) {
    if let Some(value) = value {
        match value.into_string() {
            Ok(value) => match value.parse::<Theme>() {
                Ok(theme) => app.theme = theme,
                Err(error) => app.apply_runtime(RuntimeUpdate::Warning(error.to_string())),
            },
            Err(_) => app.apply_runtime(RuntimeUpdate::Warning(format!(
                "{CHAT_THEME_ENV} contains non-Unicode text; using the default theme"
            ))),
        }
    }
}

fn parse_bool_setting(value: &str) -> Option<bool> {
    if value.eq_ignore_ascii_case("true") || value == "1" {
        Some(true)
    } else if value.eq_ignore_ascii_case("false") || value == "0" {
        Some(false)
    } else {
        None
    }
}

/// The composer-ownership guard shared by every image path: emits the reason
/// notice and returns `true` when an attach must not proceed.
fn image_attach_blocked(app: &mut AppModel) -> bool {
    if let Some(block) = app.image_attach_block() {
        app.apply_runtime(RuntimeUpdate::Warning(block.message().to_string()));
        true
    } else {
        false
    }
}

async fn attach_clipboard_image_idle(
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    config: &localpilot_config::Config,
) {
    if image_attach_blocked(app) {
        return;
    }
    let provider_id = runtime.active_provider_id().to_string();
    if !runtime.active_accepts_images() {
        let resolved = resolved_image_support(config, Some(&provider_id)).await;
        runtime.set_image_support_override(resolved);
    }
    let capability = ImageCapabilitySnapshot {
        provider_id,
        vision_capable: runtime.active_accepts_images(),
    };
    attach_clipboard_image_with_capability(app, &capability);
}

fn attach_clipboard_image_with_capability(
    app: &mut AppModel,
    capability: &ImageCapabilitySnapshot,
) {
    if image_attach_blocked(app) {
        return;
    }
    if !capability.vision_capable {
        app.apply_runtime(RuntimeUpdate::Warning(
            crate::repl::image_unsupported_notice(&capability.provider_id),
        ));
        return;
    }
    let image = match crate::repl::read_clipboard_image() {
        Ok(ClipboardImageRead::Missing) => {
            app.apply_runtime(RuntimeUpdate::Warning(
                "no image or image file on the clipboard".to_string(),
            ));
            return;
        }
        Ok(ClipboardImageRead::Image(image)) => image,
        Err(message) => {
            app.apply_runtime(RuntimeUpdate::Warning(message));
            return;
        }
    };
    let notice = image.attach_notice();
    let crate::repl::CapturedClipboardImage {
        media_type,
        data,
        byte_len,
        ..
    } = image;
    attach_prepared_image(app, media_type, data, byte_len, notice);
}

/// Hand a prepared image to the composer, emitting the success notice or, if the
/// composer declined it after the pre-checks (a race with an overlay/dialog), the
/// defensive fallback so the paste is never silently dropped.
fn attach_prepared_image(
    app: &mut AppModel,
    media_type: &'static str,
    data: String,
    byte_len: usize,
    success_notice: String,
) {
    if app.attach_image(media_type, data, byte_len).is_some() {
        app.apply_runtime(RuntimeUpdate::Warning(success_notice));
    } else {
        app.apply_runtime(RuntimeUpdate::Warning(
            "couldn't attach the image.".to_string(),
        ));
    }
}

async fn attach_image_path_idle(
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    config: &localpilot_config::Config,
    path: &std::path::Path,
) {
    if image_attach_blocked(app) {
        return;
    }
    let provider_id = runtime.active_provider_id().to_string();
    if !runtime.active_accepts_images() {
        let resolved = resolved_image_support(config, Some(&provider_id)).await;
        runtime.set_image_support_override(resolved);
    }
    let capability = ImageCapabilitySnapshot {
        provider_id,
        vision_capable: runtime.active_accepts_images(),
    };
    attach_image_path_with_capability(app, &capability, path);
}

fn attach_image_path_with_capability(
    app: &mut AppModel,
    capability: &ImageCapabilitySnapshot,
    path: &std::path::Path,
) {
    if image_attach_blocked(app) {
        return;
    }
    if !capability.vision_capable {
        app.apply_runtime(RuntimeUpdate::Warning(
            crate::repl::image_unsupported_notice(&capability.provider_id),
        ));
        return;
    }
    match crate::repl::load_image_file(path) {
        Ok(loaded) => {
            let notice = format!("attached image {}", loaded.file_name);
            attach_prepared_image(app, loaded.media_type, loaded.data, loaded.byte_len, notice);
        }
        Err(crate::repl::ImageLoadError::TooLarge) => {
            app.apply_runtime(RuntimeUpdate::Warning(
                "that image is too large to attach.".to_string(),
            ));
        }
        Err(crate::repl::ImageLoadError::Unsupported) => {
            app.apply_runtime(RuntimeUpdate::Warning(
                "that file isn't a supported image (PNG, JPEG, WebP, or GIF).".to_string(),
            ));
        }
        Err(crate::repl::ImageLoadError::Unreadable(message)) => {
            app.apply_runtime(RuntimeUpdate::Warning(format!(
                "couldn't read the image file: {message}"
            )));
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct EditorCommand {
    program: OsString,
    args: Vec<OsString>,
}

impl std::fmt::Debug for EditorCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EditorCommand")
            .field("program", &self.program)
            .field("args", &format_args!("<{} redacted>", self.args.len()))
            .finish()
    }
}

fn resolve_editor_command() -> Result<EditorCommand> {
    resolve_editor_command_with(|name| std::env::var_os(name))
        .map_err(anyhow::Error::msg)
        .context("resolve external editor")
}

fn resolve_editor_command_with(
    mut lookup: impl FnMut(&str) -> Option<OsString>,
) -> std::result::Result<EditorCommand, String> {
    for name in [CHAT_EDITOR_ENV, "VISUAL", "EDITOR"] {
        let Some(value) = lookup(name) else {
            continue;
        };
        let value = value
            .into_string()
            .map_err(|_| format!("{name} contains non-Unicode text"))?;
        let mut parts = split_editor_command(&value)?;
        let Some(program) = parts.first().cloned() else {
            return Err(format!("{name} is empty"));
        };
        parts.remove(0);
        return Ok(EditorCommand {
            program,
            args: parts,
        });
    }
    Ok(EditorCommand {
        program: if cfg!(windows) {
            OsString::from("notepad.exe")
        } else {
            OsString::from("vi")
        },
        args: Vec::new(),
    })
}

fn split_editor_command(value: &str) -> std::result::Result<Vec<OsString>, String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut token_started = false;
    for character in value.trim().chars() {
        match (quote, character) {
            (Some(open), current_quote) if open == current_quote => {
                quote = None;
                token_started = true;
            }
            (None, '"' | '\'') => {
                quote = Some(character);
                token_started = true;
            }
            (None, whitespace) if whitespace.is_whitespace() => {
                if token_started {
                    parts.push(OsString::from(std::mem::take(&mut current)));
                    token_started = false;
                }
            }
            _ => {
                current.push(character);
                token_started = true;
            }
        }
    }
    if quote.is_some() {
        return Err("external editor command has an unterminated quote".to_string());
    }
    if token_started {
        parts.push(OsString::from(current));
    }
    Ok(parts)
}

trait SuspensibleModes {
    type Capabilities;

    fn leave(&mut self);
    fn reenter(&mut self) -> Result<Self::Capabilities>;
}

struct ModeSuspension<'a, M: SuspensibleModes> {
    modes: &'a mut M,
    reentry_attempted: bool,
}

impl<'a, M: SuspensibleModes> ModeSuspension<'a, M> {
    fn new(modes: &'a mut M) -> Self {
        modes.leave();
        Self {
            modes,
            reentry_attempted: false,
        }
    }

    fn resume(mut self) -> Result<M::Capabilities> {
        self.reentry_attempted = true;
        self.modes.reenter()
    }
}

impl<M: SuspensibleModes> Drop for ModeSuspension<'_, M> {
    fn drop(&mut self) {
        // An early non-panic return still restores the application. During a
        // panic, remaining in the already-restored plain terminal is safer; the
        // panic hook cannot run a second time after unwinding re-enters modes.
        if !self.reentry_attempted && !std::thread::panicking() {
            let _ = self.modes.reenter();
        }
    }
}

async fn with_modes_suspended<M, F, T>(modes: &mut M, operation: F) -> Result<(T, M::Capabilities)>
where
    M: SuspensibleModes,
    F: Future<Output = T>,
{
    let suspension = ModeSuspension::new(modes);
    let output = operation.await;
    let capabilities = suspension.resume()?;
    Ok((output, capabilities))
}

async fn launch_external_editor(command: &EditorCommand, path: &Path) -> Result<()> {
    let status = tokio::process::Command::new(&command.program)
        .args(&command.args)
        .arg(path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("start editor {}", command.program.to_string_lossy()))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("external editor exited with {status}"))
    }
}

fn read_external_edit(path: &Path) -> Result<String> {
    let size = std::fs::metadata(path)
        .context("inspect edited prompt")?
        .len();
    if size > MAX_EXTERNAL_EDITOR_BYTES {
        anyhow::bail!("edited prompt exceeds the 8 MiB limit");
    }
    let bytes = std::fs::read(path).context("read edited prompt")?;
    String::from_utf8(bytes).context("edited prompt is not valid UTF-8")
}

async fn edit_composer_externally(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    modes: &mut TerminalModes,
    app: &mut AppModel,
) -> Result<()> {
    let Some(draft) = app.external_edit_text().map(str::to_owned) else {
        return Ok(());
    };
    let prepared = (|| -> Result<_> {
        let directory = tempfile::Builder::new()
            .prefix("localpilot-edit-")
            .tempdir()
            .context("create external-editor directory")?;
        let path = directory.path().join("LOCALPILOT_PROMPT.md");
        std::fs::write(&path, draft).context("write external-editor draft")?;
        let command = resolve_editor_command()?;
        Ok((directory, path, command))
    })();
    let (directory, path, command) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            app.finish_external_edit(None);
            app.apply_runtime(RuntimeUpdate::Warning(format!(
                "external editor could not start: {error}"
            )));
            return Ok(());
        }
    };

    let _ = terminal.show_cursor();
    let operation = async {
        let mut stdout = io::stdout();
        writeln!(
            stdout,
            "Editing prompt; close the editor to return to LocalPilot…"
        )
        .context("write external-editor handoff")?;
        stdout.flush().context("flush external-editor handoff")?;
        launch_external_editor(&command, &path).await?;
        read_external_edit(&path)
    };
    let (edited, capabilities) = with_modes_suspended(modes, operation).await?;
    app.capabilities = capabilities;
    drop(directory);
    match edited {
        Ok(edited) => app.finish_external_edit(Some(edited)),
        Err(error) => {
            app.finish_external_edit(None);
            app.apply_runtime(RuntimeUpdate::Warning(format!(
                "external editor kept the original draft: {error}"
            )));
        }
    }
    terminal.clear().context("clear after external editor")?;
    let _ = draw_synchronized(terminal, app)?;
    Ok(())
}

fn prepare_prompt_operation(
    app: &mut AppModel,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    submitted: SubmittedInput,
    pending: bool,
) -> Option<QueuedOperation> {
    let item_id = app.append_prompt(
        submitted.display.clone(),
        Some(local_prompt_time()),
        pending,
    )?;
    persist_prompt(app, history, cwd, &submitted);
    let attachments = image_content_blocks(submitted.images);
    if !attachments.is_empty() {
        app.apply_runtime(RuntimeUpdate::Warning(format!(
            "sending {} image(s) with this prompt",
            attachments.len()
        )));
    }
    Some(QueuedOperation::Prompt(QueuedPrompt {
        text: submitted.prompt,
        attachments,
        item_id,
    }))
}

fn prepare_shell_operation(
    app: &mut AppModel,
    command: UserShellCommand,
    pending: bool,
) -> Option<QueuedOperation> {
    let item_id = app.append_shell(&command, pending)?;
    Some(QueuedOperation::Shell(QueuedShell { command, item_id }))
}

async fn execute_fullscreen_slash(
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    config: &localpilot_config::Config,
    cwd: &Path,
    submitted: SubmittedInput,
) -> bool {
    if !submitted.images.is_empty() {
        app.apply_runtime(RuntimeUpdate::Notice(
            "image attachments were ignored for the slash command".to_string(),
        ));
    }
    let Some(action) = parse_slash_for(localpilot_tui::Host::Fullscreen, &submitted.prompt) else {
        app.apply_runtime(RuntimeUpdate::Warning(
            "invalid slash command input".to_string(),
        ));
        return false;
    };
    match action {
        SlashAction::Model {
            provider: Some(provider),
            model,
        } => {
            let report = switch_model_target(runtime, config, &provider, model).await;
            app.set_active_provider_model(report.provider, report.model);
            for notice in report.notices {
                app.apply_runtime(RuntimeUpdate::Notice(notice));
            }
        }
        SlashAction::Model { provider: None, .. } => {
            let values = fullscreen_model_values(config, runtime.active_provider_id());
            if values.is_empty() {
                app.apply_runtime(RuntimeUpdate::Notice(
                    "no providers are configured".to_string(),
                ));
            } else {
                app.apply_runtime(RuntimeUpdate::Notice(
                    "type /model <provider> or choose one from the completion list".to_string(),
                ));
            }
        }
        SlashAction::Clear => {
            runtime.clear_conversation();
            app.clear_conversation();
            let (used, limit) = runtime.context_usage();
            app.apply_runtime(RuntimeUpdate::ContextUsage { used, limit });
            app.apply_runtime(RuntimeUpdate::Notice("conversation cleared".to_string()));
        }
        SlashAction::Sessions => match runtime.store().list_sessions() {
            Ok(sessions) => {
                let entries = fullscreen_session_entries(sessions, runtime.session_id());
                app.open_sessions(entries);
            }
            Err(error) => app.apply_runtime(RuntimeUpdate::Warning(format!(
                "session index unreadable: {error}"
            ))),
        },
        SlashAction::LoadSession(reference) => {
            match crate::session_cmd::resolve_session_ref_in_store(runtime.store(), &reference) {
                Ok(session) => load_fullscreen_session(app, runtime, session),
                Err(error) => app.apply_runtime(RuntimeUpdate::Notice(error.to_string())),
            }
        }
        SlashAction::ContinueSession(reference) => {
            let target = match reference {
                Some(reference) => {
                    crate::session_cmd::resolve_session_ref_in_store(runtime.store(), &reference)
                        .map(Some)
                }
                None => runtime
                    .store()
                    .list_sessions()
                    .map(|sessions| latest_other_session(sessions, runtime.session_id()))
                    .map_err(anyhow::Error::from),
            };
            match target {
                Ok(Some(session)) => load_fullscreen_session(app, runtime, session),
                Ok(None) => app.apply_runtime(RuntimeUpdate::Notice(
                    "no previous session in this workspace".to_string(),
                )),
                Err(error) => app.apply_runtime(RuntimeUpdate::Notice(error.to_string())),
            }
        }
        SlashAction::NameSession(name) => {
            let session = runtime.session_id();
            match runtime.store().set_session_name(session, &name) {
                Ok(()) => {
                    let name = name.trim();
                    app.set_active_session_name(Some(sanitized_session_name(name)));
                    app.apply_runtime(RuntimeUpdate::Notice(format!(
                        "named this session \"{}\"",
                        sanitized_session_name(name)
                    )));
                }
                Err(error) => app.apply_runtime(RuntimeUpdate::Notice(format!(
                    "could not name session: {error}"
                ))),
            }
        }
        SlashAction::NewSession => {
            runtime.start_new_session();
            app.clear_stashed_draft();
            app.clear_conversation();
            app.set_active_session_id(runtime.session_id().to_string());
            app.set_active_session_name(None);
            let (used, limit) = runtime.context_usage();
            app.apply_runtime(RuntimeUpdate::ContextUsage { used, limit });
            app.apply_runtime(RuntimeUpdate::Notice(format!(
                "started new session {}",
                runtime.session_id()
            )));
        }
        action @ (SlashAction::Fork | SlashAction::CloneSession) => {
            let mark_fork = matches!(action, SlashAction::Fork);
            match runtime.fork_session(mark_fork) {
                Ok(id) => {
                    app.clear_stashed_draft();
                    app.set_active_session_id(id.to_string());
                    app.set_active_session_name(None);
                    let verb = if mark_fork { "forked" } else { "cloned" };
                    app.apply_runtime(RuntimeUpdate::Notice(format!("{verb} into session {id}")));
                }
                Err(error) => app.apply_runtime(RuntimeUpdate::Warning(format!(
                    "session branch failed: {error}"
                ))),
            }
        }
        SlashAction::Exit { print_transcript } => {
            app.request_exit(print_transcript);
            return true;
        }
        SlashAction::Invalid { command, reason } => {
            app.apply_runtime(RuntimeUpdate::Warning(format!(
                "invalid /{command}: {reason}"
            )));
        }
        SlashAction::LocalBoxAdopt => {
            match crate::localbox::detect().await {
                crate::localbox::LocalBoxState::Running { endpoint, model } => {
                    // The user explicitly typed `/localbox adopt`, so the command
                    // itself is the consent for this workspace config write; the
                    // permission engine can still hard-deny it under a Deny policy.
                    let engine = localpilot_sandbox::PermissionEngine::new(
                        crate::models_cmd::profile(config),
                        Vec::new(),
                    );
                    let path = localpilot_config::project_config_path(cwd);
                    let request = localpilot_sandbox::PermissionRequest {
                        tool: "localbox adopt".to_string(),
                        effect: localpilot_sandbox::Effect::WritePath {
                            inside_workspace: true,
                            overwrite: path.exists(),
                            secret_like: false,
                        },
                        interactivity: localpilot_sandbox::Interactivity::Interactive,
                        trusted: true,
                        detail: path.display().to_string(),
                    };
                    if matches!(engine.decide(&request), localpilot_sandbox::Decision::Deny) {
                        app.apply_runtime(RuntimeUpdate::Notice(
                            "adopt denied by the permission policy".to_string(),
                        ));
                    } else {
                        match crate::localbox::write_local_provider(
                            &path,
                            &endpoint,
                            model.as_deref(),
                        ) {
                            Ok(()) => app.apply_runtime(RuntimeUpdate::Notice(format!(
                                "adopted LocalBox at {endpoint} — wrote [providers.local]; it applies on the next launch"
                            ))),
                            Err(error) => app.apply_runtime(RuntimeUpdate::Warning(format!(
                                "adopt failed: {error}"
                            ))),
                        }
                    }
                }
                crate::localbox::LocalBoxState::InstalledNotRunning => {
                    app.apply_runtime(RuntimeUpdate::Notice(
                        "no running LocalBox server — run `localbox serve <model>` (or `localpilot localbox adopt --serve <model>`) first".to_string(),
                    ));
                }
                crate::localbox::LocalBoxState::NotInstalled => {
                    app.apply_runtime(RuntimeUpdate::Notice(
                        "LocalBox is not installed (no `localbox` on PATH)".to_string(),
                    ));
                }
            }
        }
        action @ (SlashAction::Help
        | SlashAction::Theme(_)
        | SlashAction::Settings(_)
        | SlashAction::Diff(_)
        | SlashAction::Search(_)) => open_fullscreen_takeover(app, config, cwd, action),
        SlashAction::Unknown(command) => {
            app.apply_runtime(RuntimeUpdate::Warning(format!(
                "unknown slash command: /{command}"
            )));
        }
        // The commands the full-screen host does not dispatch yet. Listed
        // explicitly (not a wildcard) so a new `SlashAction` variant that no one
        // has taught this host about is a compile error, not a silent deferral.
        SlashAction::SetMode(_)
        | SlashAction::SetProfile(_)
        | SlashAction::ToggleThinking
        | SlashAction::SetEffort(_)
        | SlashAction::Tree
        | SlashAction::Compact { .. }
        | SlashAction::HarnessResume
        | SlashAction::WaitResume
        | SlashAction::Ingest(_)
        | SlashAction::Knowledge(_)
        | SlashAction::ContextBuild(_)
        | SlashAction::Research(_)
        | SlashAction::Agents(_)
        | SlashAction::Skills(_)
        | SlashAction::Background(_) => {
            let command = submitted
                .prompt
                .trim()
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("command");
            app.apply_runtime(RuntimeUpdate::Notice(format!(
                "/{command} is not available in full-screen chat yet"
            )));
        }
    }
    false
}

enum PairLoopReady {
    Pump(Option<PairPumpEvent>),
    Tick,
}

async fn run_pair_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppModel,
    run: &mut InteractivePairRun,
    adapter: &mut PairTerminalAdapter,
    context: PairHostContext<'_>,
    workspace_index: &mut WorkspaceFileIndex,
) -> Result<()> {
    let mut mouse_state = MouseState::default();
    let mut paste_burst = PasteBurst::default();
    let mut tick = tokio::time::interval(EVENT_POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let ready = tokio::select! {
            event = run.next(), if adapter.pump_open => PairLoopReady::Pump(event),
            _ = tick.tick() => PairLoopReady::Tick,
        };
        match ready {
            PairLoopReady::Pump(Some(event)) => {
                let action = adapter.apply_pump_event(app, event);
                if execute_pair_host_action(run, adapter, app, action) {
                    return Ok(());
                }
            }
            PairLoopReady::Pump(None) => adapter.mark_pump_closed(),
            PairLoopReady::Tick => {
                workspace_index.refresh(app);
                let mut hit_map = draw_synchronized(terminal, app)?;
                if let Some(text) = paste_burst.flush_if_idle(Instant::now()) {
                    let action = if matches!(&adapter.dialog, Some(PairDialog::Questions(_))) {
                        let resolution = app.handle_question_input(InputAction::Paste(text));
                        resolve_pair_question_action(app, resolution, adapter)
                    } else if adapter.dialog.is_none() {
                        let _ = app.handle_input(InputAction::Paste(text), hit_map.editor_width);
                        PairHostAction::None
                    } else {
                        PairHostAction::None
                    };
                    if execute_pair_host_action(run, adapter, app, action) {
                        return Ok(());
                    }
                }
                for _ in 0..64 {
                    if !event::poll(Duration::ZERO).context("poll collaboration input")? {
                        break;
                    }
                    let next = event::read().context("read collaboration input")?;
                    if let Event::Key(key) = &next {
                        if is_key_action(*key) {
                            let buffered_after = buffered_after_fullscreen_key(*key)
                                .context("poll after collaboration paste key")?;
                            let consumed = if adapter.dialog.is_some() {
                                handle_dialog_paste_burst(
                                    app,
                                    &mut paste_burst,
                                    *key,
                                    buffered_after,
                                    matches!(&adapter.dialog, Some(PairDialog::Questions(_))),
                                )
                            } else {
                                handle_fullscreen_paste_burst(
                                    app,
                                    &mut paste_burst,
                                    *key,
                                    buffered_after,
                                    hit_map.editor_width,
                                )
                            };
                            if consumed {
                                continue;
                            }
                        }
                    }
                    let geometry_event = matches!(next, Event::Mouse(_) | Event::Resize(_, _));
                    let action = handle_pair_terminal_event(
                        app,
                        next,
                        adapter,
                        &hit_map,
                        &mut mouse_state,
                        context.config,
                        context.cwd,
                        context.history,
                    );
                    if execute_pair_host_action(run, adapter, app, action) {
                        return Ok(());
                    }
                    if geometry_event {
                        hit_map = draw_synchronized(terminal, app)?;
                    }
                }
                advance_mouse_selection(app, &hit_map, &mouse_state);
                let _ = draw_synchronized(terminal, app)?;
            }
        }
    }
}

/// Every user-facing abort routes through here: reflect the pending cancellation on
/// both busy panes, then request the sole supervisor abort. No terminal is faked; the
/// real terminal state arrives later from the driver report and its retained card.
fn abort_pair(app: &mut AppModel, run: &mut InteractivePairRun) {
    let _ = app.request_pair_cancellation();
    run.abort_and_cancel();
}

fn execute_pair_host_action(
    run: &mut InteractivePairRun,
    adapter: &mut PairTerminalAdapter,
    app: &mut AppModel,
    action: PairHostAction,
) -> bool {
    match action {
        PairHostAction::None => false,
        PairHostAction::Steer { peer, text } => {
            if !run.steer(peer, text) {
                adapter.reject_latest_steer(app, peer);
            }
            false
        }
        PairHostAction::Answer {
            id,
            answer,
            abort,
            exit,
        } => {
            if let Err(error) = run.answer_ask(id, answer) {
                match error {
                    PairAskAnswerError::RequesterGone { .. } => {
                        apply_pair_notice(app, error.to_string());
                    }
                    PairAskAnswerError::NoActive { .. }
                    | PairAskAnswerError::Stale { .. }
                    | PairAskAnswerError::WrongKind { .. }
                    | PairAskAnswerError::WrongQuestionCount { .. } => {
                        apply_pair_warning(app, PairPeer::A, error.to_string());
                        apply_pair_warning(app, PairPeer::B, error.to_string());
                        adapter.clear_dialog(app);
                        abort_pair(app, run);
                    }
                }
            }
            if abort {
                adapter.clear_dialog(app);
                abort_pair(app, run);
            }
            exit
        }
        PairHostAction::Abort { exit } => {
            adapter.clear_dialog(app);
            abort_pair(app, run);
            exit
        }
        PairHostAction::Exit => {
            adapter.clear_dialog(app);
            if run.is_driver_live() {
                run.abort_and_cancel();
            }
            true
        }
    }
}

fn prepare_pair_steer(
    app: &mut AppModel,
    adapter: &mut PairTerminalAdapter,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    submitted: SubmittedInput,
) -> PairHostAction {
    if !submitted.images.is_empty() {
        apply_pair_notice(
            app,
            format!(
                "This submission was not sent: peer steering accepts text only and contained {} image{}.",
                submitted.images.len(),
                if submitted.images.len() == 1 { "" } else { "s" }
            ),
        );
        return PairHostAction::None;
    }
    let Some(pane) = app.active_pair_pane() else {
        return PairHostAction::None;
    };
    let peer = pair_peer(pane);
    let Some(item_id) =
        app.append_prompt(submitted.display.clone(), Some(local_prompt_time()), true)
    else {
        apply_pair_warning(app, peer, "steering could not be queued for display");
        return PairHostAction::None;
    };
    persist_prompt(app, history, cwd, &submitted);
    adapter.queue_steer(peer, item_id);
    PairHostAction::Steer {
        peer,
        text: submitted.prompt,
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_pair_terminal_event(
    app: &mut AppModel,
    event: Event,
    adapter: &mut PairTerminalAdapter,
    hit_map: &HitMap,
    mouse_state: &mut MouseState,
    config: &localpilot_config::Config,
    cwd: &Path,
    history: &localpilot_store::PromptHistory,
) -> PairHostAction {
    match &adapter.dialog {
        Some(PairDialog::Approval { .. }) => {
            return handle_pair_approval_event(app, event, adapter)
        }
        Some(PairDialog::Questions(_)) => {
            return handle_pair_question_event(app, event, adapter, hit_map)
        }
        None => {}
    }

    match route_pointer_or_navigation(app, &event, hit_map, mouse_state) {
        RoutedEvent::Handled => return PairHostAction::None,
        RoutedEvent::Copy(text) => {
            copy_to_clipboard(app, text);
            return PairHostAction::None;
        }
        RoutedEvent::PasteClipboard => {
            paste_text_from_clipboard(app, hit_map.editor_width);
            return PairHostAction::None;
        }
        RoutedEvent::Unhandled => {}
    }
    match event {
        Event::Key(key) if is_key_action(key) => {
            if is_clipboard_image_key(key) {
                apply_pair_notice(
                    app,
                    "Clipboard images are not available in this collaboration yet.",
                );
                return PairHostAction::None;
            }
            let Some(action) = map_key(key) else {
                return PairHostAction::None;
            };
            match app.handle_input(action, hit_map.editor_width) {
                AppCommand::Exit => PairHostAction::Exit,
                AppCommand::CancelWork => PairHostAction::Abort { exit: false },
                AppCommand::Copy(text) => {
                    copy_to_clipboard(app, text);
                    PairHostAction::None
                }
                AppCommand::OpenExternalEditor => {
                    apply_pair_notice(
                        app,
                        "The external editor is not available during a collaboration.",
                    );
                    PairHostAction::None
                }
                AppCommand::RunSlash(submitted) => execute_pair_slash(app, config, cwd, submitted),
                AppCommand::Submit(submitted) => {
                    if adapter.terminal.is_some() {
                        apply_pair_notice(
                            app,
                            "This collaboration has finished; start a new run to send another prompt.",
                        );
                        PairHostAction::None
                    } else {
                        prepare_pair_steer(app, adapter, history, cwd, submitted)
                    }
                }
                AppCommand::RunShell(_) => {
                    apply_pair_notice(
                        app,
                        "Shell commands are not available during a collaboration.",
                    );
                    PairHostAction::None
                }
                AppCommand::NavigateTakeover(navigation) => {
                    apply_takeover_navigation(app, navigation, hit_map);
                    PairHostAction::None
                }
                AppCommand::NavigateTimeline(navigation) => {
                    apply_timeline_navigation(app, navigation, hit_map);
                    PairHostAction::None
                }
                AppCommand::ActivateSession(_) => {
                    apply_pair_notice(app, "Sessions cannot be changed during a collaboration.");
                    PairHostAction::None
                }
                AppCommand::None => PairHostAction::None,
            }
        }
        Event::Paste(text) => {
            if text.trim().is_empty() {
                apply_pair_notice(
                    app,
                    "Clipboard images are not available in this collaboration yet.",
                );
            } else {
                let _ = app.handle_input(InputAction::Paste(text), hit_map.editor_width);
            }
            PairHostAction::None
        }
        Event::FocusGained
        | Event::FocusLost
        | Event::Resize(_, _)
        | Event::Mouse(_)
        | Event::Key(_) => PairHostAction::None,
    }
}

fn handle_pair_approval_event(
    app: &mut AppModel,
    event: Event,
    adapter: &mut PairTerminalAdapter,
) -> PairHostAction {
    let Event::Key(key) = event else {
        return PairHostAction::None;
    };
    if !is_key_action(key) {
        return PairHostAction::None;
    }
    let (answer, abort, exit) = if is_cancel(key) {
        match app.handle_input(InputAction::CancelOrExit, 1) {
            AppCommand::Copy(text) => {
                copy_to_clipboard(app, text);
                return PairHostAction::None;
            }
            AppCommand::Exit => (false, true, true),
            AppCommand::CancelWork => (false, true, false),
            _ => return PairHostAction::None,
        }
    } else {
        match key.code {
            KeyCode::Char('y' | 'Y') => (true, false, false),
            KeyCode::Enter if !app.capabilities.screen_reader => (true, false, false),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => (false, false, false),
            _ => return PairHostAction::None,
        }
    };
    let Some(PairDialog::Approval { id }) = adapter.dialog.take() else {
        return PairHostAction::Abort { exit: false };
    };
    app.clear_dialog();
    PairHostAction::Answer {
        id,
        answer: PairAskAnswer::Approval(answer),
        abort,
        exit,
    }
}

fn handle_pair_question_event(
    app: &mut AppModel,
    event: Event,
    adapter: &mut PairTerminalAdapter,
    hit_map: &HitMap,
) -> PairHostAction {
    match event {
        Event::Mouse(mouse) => {
            app.disarm_exit();
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    if let Some(timeline) = active_timeline_hits(app, hit_map) {
                        app.active_timeline_mut().scroll_by(
                            -WHEEL_SCROLL_ROWS,
                            timeline.wrap_width,
                            timeline.timeline.height,
                        );
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(timeline) = active_timeline_hits(app, hit_map) {
                        app.active_timeline_mut().scroll_by(
                            WHEEL_SCROLL_ROWS,
                            timeline.wrap_width,
                            timeline.timeline.height,
                        );
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(hit) = hit_map
                        .question_rows
                        .iter()
                        .find(|hit| rect_contains(hit.area, mouse.column, mouse.row))
                    {
                        app.select_question_option(hit.index);
                    }
                }
                _ => {}
            }
            PairHostAction::None
        }
        Event::Paste(text) => {
            let resolution = app.handle_question_input(InputAction::Paste(text));
            resolve_pair_question_action(app, resolution, adapter)
        }
        Event::Key(key) if is_key_action(key) => {
            if is_cancel(key) {
                let command = app.handle_input(InputAction::CancelOrExit, hit_map.editor_width);
                return match command {
                    AppCommand::Copy(text) => {
                        copy_to_clipboard(app, text);
                        PairHostAction::None
                    }
                    AppCommand::Exit => dismiss_pair_questions(app, adapter, true, true),
                    AppCommand::CancelWork => dismiss_pair_questions(app, adapter, true, false),
                    _ => PairHostAction::None,
                };
            }
            app.disarm_exit();
            let Some(action) = map_key(key) else {
                return PairHostAction::None;
            };
            if let InputAction::NavigateTimeline(navigation) = action {
                apply_timeline_navigation(app, navigation, hit_map);
                return PairHostAction::None;
            }
            let resolution = app.handle_question_input(action);
            resolve_pair_question_action(app, resolution, adapter)
        }
        Event::FocusGained | Event::FocusLost | Event::Resize(_, _) | Event::Key(_) => {
            PairHostAction::None
        }
    }
}

fn resolve_pair_question_action(
    app: &mut AppModel,
    action: QuestionAction,
    adapter: &mut PairTerminalAdapter,
) -> PairHostAction {
    match action {
        QuestionAction::None => PairHostAction::None,
        QuestionAction::Cancel => dismiss_pair_questions(app, adapter, false, false),
        QuestionAction::Submit(response) => {
            let answer = match response {
                QuestionResponse::Selected(labels) => UserAnswer::Selected(labels),
                QuestionResponse::Other(text) => UserAnswer::Other(text),
            };
            let advance = match adapter.dialog.as_mut() {
                Some(PairDialog::Questions(pending)) => pending.advance(app, answer),
                _ => return PairHostAction::Abort { exit: false },
            };
            match advance {
                PairQuestionAdvance::Pending => PairHostAction::None,
                PairQuestionAdvance::Complete => {
                    let Some(PairDialog::Questions(pending)) = adapter.dialog.take() else {
                        return PairHostAction::Abort { exit: false };
                    };
                    let (id, answers) = pending.finish();
                    PairHostAction::Answer {
                        id,
                        answer: PairAskAnswer::Questions(answers),
                        abort: false,
                        exit: false,
                    }
                }
                PairQuestionAdvance::Failed => {
                    apply_pair_notice(app, "The next question could not be displayed.");
                    dismiss_pair_questions(app, adapter, true, false)
                }
            }
        }
    }
}

fn dismiss_pair_questions(
    app: &mut AppModel,
    adapter: &mut PairTerminalAdapter,
    abort: bool,
    exit: bool,
) -> PairHostAction {
    let Some(PairDialog::Questions(pending)) = adapter.dialog.take() else {
        return PairHostAction::Abort { exit };
    };
    app.clear_dialog();
    let (id, answers) = pending.finish();
    PairHostAction::Answer {
        id,
        answer: PairAskAnswer::Questions(answers),
        abort,
        exit,
    }
}

fn execute_pair_slash(
    app: &mut AppModel,
    config: &localpilot_config::Config,
    cwd: &Path,
    submitted: SubmittedInput,
) -> PairHostAction {
    if !submitted.images.is_empty() {
        apply_pair_notice(app, "Image attachments were ignored for the slash command.");
    }
    // Match `/abort` as an exact first token so `/abortive` stays an unknown command
    // rather than a mis-typed abort.
    if submitted.prompt.split_whitespace().next() == Some("/abort") {
        if submitted.prompt.split_whitespace().nth(1).is_none() {
            // Maps to the sole existing abort action; no second cancellation path.
            return PairHostAction::Abort { exit: false };
        }
        apply_pair_warning(
            app,
            PairPeer::A,
            "usage: /abort takes no arguments; it stops the collaboration and both peers",
        );
        return PairHostAction::None;
    }
    match parse_slash_for(localpilot_tui::Host::Pair, &submitted.prompt) {
        Some(SlashAction::Exit { print_transcript }) => {
            app.request_exit(print_transcript);
            PairHostAction::Exit
        }
        Some(SlashAction::Invalid { command, reason }) => {
            apply_pair_warning(app, PairPeer::A, format!("invalid /{command}: {reason}"));
            PairHostAction::None
        }
        Some(SlashAction::Help) => {
            app.open_help();
            PairHostAction::None
        }
        Some(SlashAction::Theme(None)) => {
            app.open_theme_picker();
            PairHostAction::None
        }
        Some(SlashAction::Theme(Some(value))) => {
            match value.parse::<Theme>() {
                Ok(theme) => app.apply_theme(theme),
                Err(error) => apply_pair_warning(app, PairPeer::A, error.to_string()),
            }
            PairHostAction::None
        }
        Some(SlashAction::Settings(None)) => {
            app.open_settings(fullscreen_settings(app, config));
            PairHostAction::None
        }
        Some(SlashAction::Settings(Some(query))) => {
            let settings = fullscreen_settings(app, config);
            app.open_settings_with_query(settings, &query);
            PairHostAction::None
        }
        Some(SlashAction::Diff(filter)) => {
            open_workspace_diff(app, cwd, filter.as_deref());
            PairHostAction::None
        }
        Some(SlashAction::Search(query)) => {
            app.open_timeline_search(query.unwrap_or_default());
            PairHostAction::None
        }
        Some(SlashAction::Unknown(command)) => {
            apply_pair_warning(
                app,
                PairPeer::A,
                format!("unknown slash command: /{command}"),
            );
            PairHostAction::None
        }
        Some(_) => {
            let command = submitted
                .prompt
                .trim()
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("command");
            apply_pair_notice(
                app,
                format!("/{command} is not available during a collaboration."),
            );
            PairHostAction::None
        }
        None => {
            apply_pair_warning(app, PairPeer::A, "invalid slash command input");
            PairHostAction::None
        }
    }
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    modes: &mut TerminalModes,
    app: &mut AppModel,
    context: HostContext<'_>,
    workspace_index: &mut WorkspaceFileIndex,
) -> Result<LoopExit> {
    let HostContext {
        runtime,
        approval_rx,
        question_rx,
        cwd,
        history,
        ingest,
        config,
        trust_required: _,
    } = context;
    let mut queue = VecDeque::new();
    let mut mouse_state = MouseState::default();
    let mut paste_burst = PasteBurst::default();
    while !app.exit_requested {
        workspace_index.refresh(app);
        let hit_map = draw_synchronized(terminal, app)?;
        if !event::poll(EVENT_POLL_INTERVAL).context("poll full-screen terminal event")? {
            if let Some(text) = paste_burst.flush_if_idle(Instant::now()) {
                if !app.workspace_trust_pending() {
                    let _ = app.handle_input(InputAction::Paste(text), hit_map.editor_width);
                }
            }
            advance_mouse_selection(app, &hit_map, &mouse_state);
            continue;
        }
        let next = event::read().context("read full-screen terminal event")?;
        if app.workspace_trust_pending() {
            mouse_state.reset_gesture();
            if let Event::Key(key) = &next {
                if is_key_action(*key) {
                    let buffered_after = buffered_after_fullscreen_key(*key)
                        .context("poll after workspace-trust paste key")?;
                    if handle_dialog_paste_burst(app, &mut paste_burst, *key, buffered_after, false)
                    {
                        continue;
                    }
                }
            }
            match handle_trust_event(app, next, &hit_map) {
                TrustEventOutcome::Pending => {}
                TrustEventOutcome::Copy(text) => copy_to_clipboard(app, text),
                TrustEventOutcome::ContinueSession => {
                    accept_workspace_trust(app, cwd, false, crate::trust::remember);
                    crate::repl::start_session_knowledge_index(cwd, ingest);
                }
                TrustEventOutcome::Remember => {
                    accept_workspace_trust(app, cwd, true, crate::trust::remember);
                    crate::repl::start_session_knowledge_index(cwd, ingest);
                }
                TrustEventOutcome::Exit => break,
                TrustEventOutcome::Deny => return Ok(LoopExit::TrustDenied),
            }
            continue;
        }
        if let Event::Key(key) = &next {
            if is_key_action(*key) {
                let buffered_after = buffered_after_fullscreen_key(*key)
                    .context("poll after full-screen paste key")?;
                if handle_fullscreen_paste_burst(
                    app,
                    &mut paste_burst,
                    *key,
                    buffered_after,
                    hit_map.editor_width,
                ) {
                    continue;
                }
            }
        }
        match route_pointer_or_navigation(app, &next, &hit_map, &mut mouse_state) {
            RoutedEvent::Handled => continue,
            RoutedEvent::Copy(text) => {
                copy_to_clipboard(app, text);
                continue;
            }
            RoutedEvent::PasteClipboard => {
                paste_text_from_clipboard(app, hit_map.editor_width);
                continue;
            }
            RoutedEvent::Unhandled => {}
        }
        match next {
            Event::Key(key) if is_key_action(key) => {
                if is_clipboard_image_key(key) {
                    attach_clipboard_image_idle(app, runtime, config).await;
                    continue;
                }
                let Some(action) = map_key(key) else {
                    continue;
                };
                match app.handle_input(action, hit_map.editor_width) {
                    AppCommand::Exit => break,
                    AppCommand::Copy(text) => copy_to_clipboard(app, text),
                    AppCommand::OpenExternalEditor => {
                        edit_composer_externally(terminal, modes, app).await?;
                    }
                    AppCommand::RunSlash(submitted) => {
                        if execute_fullscreen_slash(app, runtime, config, cwd, submitted).await {
                            break;
                        }
                    }
                    AppCommand::Submit(submitted) => {
                        let Some(operation) =
                            prepare_prompt_operation(app, history, cwd, submitted, false)
                        else {
                            continue;
                        };
                        if drive_operation_chain(
                            terminal,
                            app,
                            runtime,
                            approval_rx,
                            question_rx,
                            operation,
                            &mut queue,
                            history,
                            cwd,
                            &mut mouse_state,
                            &mut paste_burst,
                            workspace_index,
                        )
                        .await?
                        {
                            break;
                        }
                    }
                    AppCommand::RunShell(command) => {
                        let Some(operation) = prepare_shell_operation(app, command, false) else {
                            continue;
                        };
                        if drive_operation_chain(
                            terminal,
                            app,
                            runtime,
                            approval_rx,
                            question_rx,
                            operation,
                            &mut queue,
                            history,
                            cwd,
                            &mut mouse_state,
                            &mut paste_burst,
                            workspace_index,
                        )
                        .await?
                        {
                            break;
                        }
                    }
                    AppCommand::NavigateTakeover(navigation) => {
                        apply_takeover_navigation(app, navigation, &hit_map);
                    }
                    AppCommand::NavigateTimeline(navigation) => {
                        apply_timeline_navigation(app, navigation, &hit_map);
                    }
                    AppCommand::ActivateSession(selection) => {
                        activate_session_selection(app, runtime, &selection);
                    }
                    AppCommand::None | AppCommand::CancelWork => {}
                }
            }
            Event::Paste(text) => {
                if text.trim().is_empty() {
                    attach_clipboard_image_idle(app, runtime, config).await;
                } else if let Some(path) = crate::repl::recognized_image_candidate_path(&text) {
                    attach_image_path_idle(app, runtime, config, &path).await;
                } else {
                    let _ = app.handle_input(InputAction::Paste(text), hit_map.editor_width);
                }
            }
            Event::FocusGained
            | Event::FocusLost
            | Event::Resize(_, _)
            | Event::Mouse(_)
            | Event::Key(_) => {}
        }
    }
    if let Some(usage) = crate::repl::stored_session_usage(runtime.store(), runtime.session_id()) {
        app.set_active_usage(Some(UsageTotals {
            input_tokens: usage.effective_input_tokens(),
            output_tokens: usage.output_tokens,
            cached_input_tokens: usage.cache_read_input_tokens,
        }));
    }
    Ok(LoopExit::Normal)
}

#[allow(clippy::too_many_arguments)]
async fn drive_operation_chain(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>,
    question_rx: &mut mpsc::UnboundedReceiver<QuestionCall>,
    first: QueuedOperation,
    queue: &mut VecDeque<QueuedOperation>,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    mouse_state: &mut MouseState,
    paste_burst: &mut PasteBurst,
    workspace_index: &mut WorkspaceFileIndex,
) -> Result<bool> {
    let mut current = Some(first);
    while let Some(operation) = current {
        let next_item = queue.front().map(QueuedOperation::item_id);
        match operation {
            QueuedOperation::Prompt(prompt) => {
                let _ = app.activate_prompt(prompt.item_id);
                app.begin_work_before(next_item);
                if drive_turn(
                    terminal,
                    app,
                    runtime,
                    approval_rx,
                    question_rx,
                    &prompt.text,
                    &prompt.attachments,
                    queue,
                    history,
                    cwd,
                    mouse_state,
                    paste_burst,
                    workspace_index,
                )
                .await?
                {
                    discard_queued_operations(queue);
                    return Ok(true);
                }
            }
            QueuedOperation::Shell(shell) => {
                app.begin_work_before(next_item);
                let _ = app.activate_shell(shell.item_id);
                if drive_shell(
                    terminal,
                    app,
                    runtime,
                    approval_rx,
                    shell,
                    queue,
                    history,
                    cwd,
                    mouse_state,
                    paste_burst,
                    workspace_index,
                )
                .await?
                {
                    discard_queued_operations(queue);
                    return Ok(true);
                }
            }
        }
        current = queue.pop_front();
    }
    Ok(false)
}

fn discard_queued_operations(queue: &mut VecDeque<QueuedOperation>) {
    queue.clear();
}

#[allow(clippy::too_many_arguments)] // the live terminal pump threads these owners
async fn drive_shell(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>,
    shell: QueuedShell,
    queue: &mut VecDeque<QueuedOperation>,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    mouse_state: &mut MouseState,
    paste_burst: &mut PasteBurst,
    workspace_index: &mut WorkspaceFileIndex,
) -> Result<bool> {
    let cancel = CancellationToken::new();
    let image_capability = ImageCapabilitySnapshot {
        provider_id: runtime.active_provider_id().to_string(),
        vision_capable: runtime.active_accepts_images(),
    };
    let mut pending: Option<oneshot::Sender<bool>> = None;
    let outcome = {
        let operation =
            runtime.run_user_shell_command_detailed(shell.command.as_str(), &cancel, false);
        tokio::pin!(operation);
        let mut tick = tokio::time::interval(EVENT_POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        async {
            loop {
        tokio::select! {
            biased;
            _ = tick.tick() => {
                workspace_index.refresh(app);
                let mut hit_map = draw_synchronized(terminal, app)?;
                if let Some(text) = paste_burst.flush_if_idle(Instant::now()) {
                    if pending.is_none() {
                        let _ = app.handle_input(InputAction::Paste(text), hit_map.editor_width);
                    }
                }
                for _ in 0..64 {
                    if !event::poll(Duration::ZERO).context("poll full-screen shell input")? {
                        break;
                    }
                    let next = event::read().context("read full-screen shell input")?;
                    if let Event::Key(key) = &next {
                        if is_key_action(*key) {
                            let buffered_after = buffered_after_fullscreen_key(*key)
                                .context("poll after active shell paste key")?;
                            let consumed = if pending.is_some() {
                                handle_dialog_paste_burst(
                                    app,
                                    paste_burst,
                                    *key,
                                    buffered_after,
                                    false,
                                )
                            } else {
                                handle_fullscreen_paste_burst(
                                    app,
                                    paste_burst,
                                    *key,
                                    buffered_after,
                                    hit_map.editor_width,
                                )
                            };
                            if consumed {
                                continue;
                            }
                        }
                    }
                    let geometry_event = matches!(next, Event::Mouse(_) | Event::Resize(_, _));
                    let exit = if pending.is_some() {
                        handle_approval_event(app, next, &mut pending, &cancel)
                    } else {
                        match route_pointer_or_navigation(app, &next, &hit_map, mouse_state) {
                            RoutedEvent::Handled => false,
                            RoutedEvent::Copy(text) => {
                                copy_to_clipboard(app, text);
                                false
                            }
                            RoutedEvent::PasteClipboard => {
                                paste_text_from_clipboard(app, hit_map.editor_width);
                                false
                            }
                            RoutedEvent::Unhandled => handle_turn_event(
                                app,
                                next,
                                &cancel,
                                &hit_map,
                                queue,
                                history,
                                cwd,
                                &image_capability,
                            ),
                        }
                    };
                    if exit {
                        cancel.cancel();
                        return Ok(true);
                    }
                    if geometry_event {
                        hit_map = draw_synchronized(terminal, app)?;
                    }
                }
                advance_mouse_selection(app, &hit_map, mouse_state);
                let _ = draw_synchronized(terminal, app)?;
            }
            result = &mut operation => {
                let output = result.presentation.map_or_else(
                    || {
                        UserShellOutput::diagnostic(
                            result.result.is_error(),
                            present_shell_diagnostic(&result.result.output),
                        )
                    },
                    |presentation| match presentation {
                        ToolOutputPresentation::Shell(captured) => UserShellOutput::captured(
                            captured.exit_code,
                            &captured.stdout,
                            &captured.stderr,
                        ),
                    },
                );
                let _ = app.finish_shell(shell.item_id, &shell.command, &output);
                app.apply_runtime(RuntimeUpdate::Stopped(StopState::Done));
                let _ = draw_synchronized(terminal, app)?;
                return Ok(false);
            }
            Some(call) = approval_rx.recv(), if pending.is_none() => {
                mouse_state.reset_gesture();
                if let Some(text) = paste_burst.flush_pending() {
                    let _ = app.handle_input(InputAction::Paste(text), 1);
                }
                app.request_approval(
                    call.request.tool,
                    call.request.target,
                    call.request.risk_class,
                );
                pending = Some(call.reply);
            }
        }
            }
        }
        .await
    };
    deny_pending(app, &mut pending);
    deny_buffered_approvals(approval_rx);
    outcome
}

fn accept_workspace_trust(
    app: &mut AppModel,
    cwd: &Path,
    remember: bool,
    persist: impl FnOnce(&Path),
) {
    if remember {
        persist(cwd);
    }
    app.clear_dialog();
}

fn handle_trust_event(app: &mut AppModel, event: Event, hit_map: &HitMap) -> TrustEventOutcome {
    match event {
        Event::Mouse(mouse) => {
            app.disarm_exit();
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    let _ = app.handle_trust_input(InputAction::MoveUp);
                }
                MouseEventKind::ScrollDown => {
                    let _ = app.handle_trust_input(InputAction::MoveDown);
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(path) = hit_map
                        .trust_path
                        .as_ref()
                        .filter(|path| rect_contains(path.area, mouse.column, mouse.row))
                    {
                        app.start_trust_path_selection(
                            path.text().to_string(),
                            path.byte_for_column(mouse.column, false),
                        );
                    } else if let Some(hit) = hit_map
                        .trust_rows
                        .iter()
                        .find(|hit| rect_contains(hit.area, mouse.column, mouse.row))
                    {
                        app.select_trust_option(hit.index);
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                    if app.trust_path_selection_active() =>
                {
                    if let Some(path) = &hit_map.trust_path {
                        app.extend_trust_path_selection(
                            path.text(),
                            path.byte_for_column(mouse.column, true),
                        );
                    }
                }
                _ => {}
            }
            TrustEventOutcome::Pending
        }
        Event::Paste(_) => {
            app.disarm_exit();
            TrustEventOutcome::Pending
        }
        Event::Key(key) if is_key_action(key) => {
            if is_cancel(key) {
                return match app.handle_input(InputAction::CancelOrExit, hit_map.editor_width) {
                    AppCommand::Copy(text) => TrustEventOutcome::Copy(text),
                    AppCommand::Exit => TrustEventOutcome::Exit,
                    _ => TrustEventOutcome::Pending,
                };
            }
            app.disarm_exit();
            let Some(action) = map_key(key) else {
                return TrustEventOutcome::Pending;
            };
            match app.handle_trust_input(action) {
                TrustAction::None => TrustEventOutcome::Pending,
                TrustAction::ContinueSession => TrustEventOutcome::ContinueSession,
                TrustAction::Remember => TrustEventOutcome::Remember,
                TrustAction::Deny => TrustEventOutcome::Deny,
            }
        }
        _ => TrustEventOutcome::Pending,
    }
}

#[allow(clippy::too_many_arguments)] // the live terminal pump threads these owners
async fn drive_turn(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>,
    question_rx: &mut mpsc::UnboundedReceiver<QuestionCall>,
    prompt: &str,
    attachments: &[ContentBlock],
    queue: &mut VecDeque<QueuedOperation>,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    mouse_state: &mut MouseState,
    paste_burst: &mut PasteBurst,
    workspace_index: &mut WorkspaceFileIndex,
) -> Result<bool> {
    let (events, mut rx) = broadcast::channel::<RuntimeEvent>(1024);
    let cancel = CancellationToken::new();
    // Snapshot immediately before the turn borrows the runtime. Mid-turn image
    // paste can then use the exact active provider without racing a model switch
    // or attempting a second mutable borrow for capability discovery.
    let image_capability = ImageCapabilitySnapshot {
        provider_id: runtime.active_provider_id().to_string(),
        vision_capable: runtime.active_accepts_images(),
    };
    let mut pending: Option<oneshot::Sender<bool>> = None;
    let mut pending_questions: Option<PendingQuestions> = None;
    let mut pending_steer_items = VecDeque::new();
    let steer = runtime.steer_queue();
    let outcome = {
        let operation = runtime.run_turn_with_attachments(prompt, attachments, &events, &cancel);
        tokio::pin!(operation);
        let mut tick = tokio::time::interval(EVENT_POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        async {
            loop {
        tokio::select! {
            biased;
            _ = tick.tick() => {
                workspace_index.refresh(app);
                let mut hit_map = draw_synchronized(terminal, app)?;
                if let Some(text) = paste_burst.flush_if_idle(Instant::now()) {
                    if pending.is_none() && pending_questions.is_none() {
                        let _ = app.handle_input(InputAction::Paste(text), hit_map.editor_width);
                    } else if pending_questions.is_some() {
                        let _ = app.handle_question_input(InputAction::Paste(text));
                    }
                }
                for _ in 0..64 {
                    if !event::poll(Duration::ZERO).context("poll full-screen turn input")? {
                        break;
                    }
                    let next = event::read().context("read full-screen turn input")?;
                    if let Event::Key(key) = &next {
                        if is_key_action(*key) {
                            let buffered_after = buffered_after_fullscreen_key(*key)
                                .context("poll after active full-screen paste key")?;
                            let consumed = if pending.is_some() || pending_questions.is_some() {
                                handle_dialog_paste_burst(
                                    app,
                                    paste_burst,
                                    *key,
                                    buffered_after,
                                    pending_questions.is_some(),
                                )
                            } else {
                                handle_fullscreen_paste_burst(
                                    app,
                                    paste_burst,
                                    *key,
                                    buffered_after,
                                    hit_map.editor_width,
                                )
                            };
                            if consumed {
                                continue;
                            }
                        }
                    }
                    let geometry_event = matches!(next, Event::Mouse(_) | Event::Resize(_, _));
                    let exit = if pending_questions.is_some() {
                        handle_question_event(
                            app,
                            next,
                            &mut pending_questions,
                            &cancel,
                            &hit_map,
                        )
                    } else if pending.is_some() {
                        handle_approval_event(app, next, &mut pending, &cancel)
                    } else {
                        match route_pointer_or_navigation(app, &next, &hit_map, mouse_state) {
                            RoutedEvent::Handled => false,
                            RoutedEvent::Copy(text) => {
                                copy_to_clipboard(app, text);
                                false
                            }
                            RoutedEvent::PasteClipboard => {
                                paste_text_from_clipboard(app, hit_map.editor_width);
                                false
                            }
                            RoutedEvent::Unhandled => handle_turn_event_with_steering(
                                app,
                                next,
                                &cancel,
                                &hit_map,
                                queue,
                                history,
                                cwd,
                                &image_capability,
                                &steer,
                                &mut pending_steer_items,
                            ),
                        }
                    };
                    if exit {
                        cancel.cancel();
                        return Ok(true);
                    }
                    if geometry_event {
                        hit_map = draw_synchronized(terminal, app)?;
                    }
                }
                advance_mouse_selection(app, &hit_map, mouse_state);
                let _ = draw_synchronized(terminal, app)?;
            }
            reason = &mut operation => {
                drain_runtime_events(app, &mut rx, &mut pending_steer_items);
                app.apply_runtime(map_runtime_event(RuntimeEvent::Stopped(reason)));
                let _ = draw_synchronized(terminal, app)?;
                return Ok(false);
            }
            Some(call) = approval_rx.recv(), if pending.is_none() && pending_questions.is_none() => {
                mouse_state.reset_gesture();
                if let Some(text) = paste_burst.flush_pending() {
                    let _ = app.handle_input(InputAction::Paste(text), 1);
                }
                app.request_approval(
                    call.request.tool,
                    call.request.target,
                    call.request.risk_class,
                );
                pending = Some(call.reply);
            }
            received = rx.recv() => {
                match received {
                    Ok(event) => apply_runtime_event(app, event, &mut pending_steer_items),
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {}
                }
            }
            Some(call) = question_rx.recv(), if pending.is_none() && pending_questions.is_none() => {
                mouse_state.reset_gesture();
                if let Some(text) = paste_burst.flush_pending() {
                    let _ = app.handle_input(InputAction::Paste(text), 1);
                }
                if call.questions.is_empty() {
                    let _ = call.reply.send(Vec::new());
                } else {
                    let questions = PendingQuestions {
                        questions: call.questions,
                        index: 0,
                        answers: Vec::new(),
                        reply: call.reply,
                    };
                    questions.show_current(app);
                    pending_questions = Some(questions);
                }
            }
        }
            }
        }
        .await
    };
    deny_pending(app, &mut pending);
    deny_buffered_approvals(approval_rx);
    dismiss_pending_questions(app, &mut pending_questions);
    dismiss_buffered_questions(question_rx);
    outcome
}

#[allow(clippy::too_many_arguments)] // the live input router threads each state owner explicitly
fn handle_turn_event_impl(
    app: &mut AppModel,
    event: Event,
    cancel: &CancellationToken,
    hit_map: &HitMap,
    queue: &mut VecDeque<QueuedOperation>,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    image_capability: &ImageCapabilitySnapshot,
    mut steering: Option<(&SteerQueue, &mut VecDeque<ItemId>)>,
) -> bool {
    match event {
        Event::Key(key) if is_key_action(key) => {
            if is_enqueue_key(key) {
                if let AppCommand::Submit(submitted) =
                    app.handle_input(InputAction::Submit, hit_map.editor_width)
                {
                    if let Some(operation) =
                        prepare_prompt_operation(app, history, cwd, submitted, true)
                    {
                        queue.push_back(operation);
                    }
                }
                return false;
            }
            if is_clipboard_image_key(key) {
                attach_clipboard_image_with_capability(app, image_capability);
                return false;
            }
            let Some(action) = map_key(key) else {
                return false;
            };
            match app.handle_input(action, hit_map.editor_width) {
                AppCommand::Exit => {
                    cancel.cancel();
                    true
                }
                AppCommand::CancelWork => {
                    let promoted = if key.code == KeyCode::Esc {
                        steering.as_mut().map_or(0, |(steer, pending_items)| {
                            promote_queued_prompts_to_urgent_steering(
                                app,
                                queue,
                                steer,
                                pending_items,
                            )
                        })
                    } else {
                        0
                    };
                    if promoted > 0 {
                        app.clear_cancellation_request();
                    } else {
                        cancel.cancel();
                    }
                    false
                }
                AppCommand::Copy(text) => {
                    copy_to_clipboard(app, text);
                    false
                }
                AppCommand::RunSlash(submitted) => {
                    match parse_slash_for(localpilot_tui::Host::Fullscreen, &submitted.prompt) {
                        Some(SlashAction::Exit { print_transcript }) => {
                            app.request_exit(print_transcript);
                            cancel.cancel();
                            true
                        }
                        // The contained takeovers stay reachable mid-operation,
                        // matching the behaviour before they became parsed actions.
                        Some(SlashAction::Help) => {
                            app.open_help();
                            false
                        }
                        Some(SlashAction::Theme(None)) => {
                            app.open_theme_picker();
                            false
                        }
                        Some(SlashAction::Theme(Some(value))) => {
                            match value.parse::<Theme>() {
                                Ok(theme) => app.apply_theme(theme),
                                Err(error) => {
                                    app.apply_runtime(RuntimeUpdate::Warning(error.to_string()))
                                }
                            }
                            false
                        }
                        Some(SlashAction::Search(query)) => {
                            app.open_timeline_search(query.unwrap_or_default());
                            false
                        }
                        // Everything else — including the idle-only settings/diff
                        // takeovers — waits for the operation to finish.
                        _ => {
                            app.apply_runtime(RuntimeUpdate::Notice(
                                "slash commands run when the current operation is idle".to_string(),
                            ));
                            false
                        }
                    }
                }
                AppCommand::Submit(submitted) => {
                    if let Some(operation) =
                        prepare_prompt_operation(app, history, cwd, submitted, true)
                    {
                        queue.push_back(operation);
                    }
                    false
                }
                AppCommand::RunShell(command) => {
                    if let Some(operation) = prepare_shell_operation(app, command, true) {
                        queue.push_back(operation);
                    }
                    false
                }
                AppCommand::NavigateTakeover(navigation) => {
                    apply_takeover_navigation(app, navigation, hit_map);
                    false
                }
                AppCommand::NavigateTimeline(navigation) => {
                    apply_timeline_navigation(app, navigation, hit_map);
                    false
                }
                AppCommand::ActivateSession(_) => {
                    app.apply_runtime(RuntimeUpdate::Notice(
                        "sessions can be changed when the current operation is idle".to_string(),
                    ));
                    false
                }
                AppCommand::None | AppCommand::OpenExternalEditor => false,
            }
        }
        Event::Paste(text) => {
            if text.trim().is_empty() {
                attach_clipboard_image_with_capability(app, image_capability);
            } else if let Some(path) = crate::repl::recognized_image_candidate_path(&text) {
                attach_image_path_with_capability(app, image_capability, &path);
            } else {
                let _ = app.handle_input(InputAction::Paste(text), hit_map.editor_width);
            }
            false
        }
        Event::FocusGained
        | Event::FocusLost
        | Event::Mouse(_)
        | Event::Resize(_, _)
        | Event::Key(_) => false,
    }
}

#[allow(clippy::too_many_arguments)] // the non-steering adapter preserves the shared router inputs
fn handle_turn_event(
    app: &mut AppModel,
    event: Event,
    cancel: &CancellationToken,
    hit_map: &HitMap,
    queue: &mut VecDeque<QueuedOperation>,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    image_capability: &ImageCapabilitySnapshot,
) -> bool {
    handle_turn_event_impl(
        app,
        event,
        cancel,
        hit_map,
        queue,
        history,
        cwd,
        image_capability,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn handle_turn_event_with_steering(
    app: &mut AppModel,
    event: Event,
    cancel: &CancellationToken,
    hit_map: &HitMap,
    queue: &mut VecDeque<QueuedOperation>,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    image_capability: &ImageCapabilitySnapshot,
    steer: &SteerQueue,
    pending_steer_items: &mut VecDeque<ItemId>,
) -> bool {
    handle_turn_event_impl(
        app,
        event,
        cancel,
        hit_map,
        queue,
        history,
        cwd,
        image_capability,
        Some((steer, pending_steer_items)),
    )
}

fn promote_queued_prompts_to_urgent_steering(
    app: &mut AppModel,
    queue: &mut VecDeque<QueuedOperation>,
    steer: &SteerQueue,
    pending_steer_items: &mut VecDeque<ItemId>,
) -> usize {
    let mut promoted = 0;
    while queue.front().is_some_and(|operation| {
        matches!(operation, QueuedOperation::Prompt(prompt) if prompt.attachments.is_empty())
    }) {
        let Some(QueuedOperation::Prompt(prompt)) = queue.pop_front() else {
            break;
        };
        pending_steer_items.push_back(prompt.item_id);
        steer.push_interrupt(SoftInterrupt {
            content: prompt.text,
            source: localpilot_harness::SoftInterruptSource::User,
            urgent: true,
        });
        promoted += 1;
    }
    if promoted > 0 {
        app.apply_runtime(RuntimeUpdate::Notice(format!(
            "steering with {promoted} queued prompt{}",
            if promoted == 1 { "" } else { "s" }
        )));
    }
    promoted
}

fn route_pointer_or_navigation(
    app: &mut AppModel,
    event: &Event,
    hit_map: &HitMap,
    mouse_state: &mut MouseState,
) -> RoutedEvent {
    match event {
        Event::Mouse(mouse) => handle_mouse_event(app, *mouse, hit_map, mouse_state),
        Event::FocusLost => {
            mouse_state.reset_gesture();
            RoutedEvent::Handled
        }
        Event::Key(key) if is_key_action(*key) => {
            let Some(InputAction::NavigateTimeline(navigation)) = map_key(*key) else {
                return RoutedEvent::Unhandled;
            };
            let command = app.handle_input(
                InputAction::NavigateTimeline(navigation),
                hit_map.editor_width,
            );
            match command {
                AppCommand::NavigateTimeline(navigation) => {
                    apply_timeline_navigation(app, navigation, hit_map)
                }
                AppCommand::NavigateTakeover(navigation) => {
                    apply_takeover_navigation(app, navigation, hit_map)
                }
                _ => {}
            }
            RoutedEvent::Handled
        }
        Event::FocusGained | Event::Paste(_) | Event::Resize(_, _) | Event::Key(_) => {
            RoutedEvent::Unhandled
        }
    }
}

fn active_timeline_hits<'a>(app: &AppModel, hit_map: &'a HitMap) -> Option<&'a TimelinePaneHits> {
    hit_map.timelines.as_ref()?.active(app.active_pair_pane())
}

fn timeline_hits_for_peer(hit_map: &HitMap, peer: Option<PeerPane>) -> Option<&TimelinePaneHits> {
    match peer {
        Some(peer) => hit_map.timelines.as_ref()?.for_peer(peer),
        None => hit_map.timelines.as_ref()?.active(None),
    }
}

fn timeline_for_peer(app: &AppModel, peer: Option<PeerPane>) -> Option<&Timeline> {
    match peer {
        Some(peer) => app.timeline_for(peer),
        None if !app.is_pair() => Some(app.active_timeline()),
        None => None,
    }
}

fn timeline_for_peer_mut(app: &mut AppModel, peer: Option<PeerPane>) -> Option<&mut Timeline> {
    match peer {
        Some(peer) => app.timeline_for_mut(peer),
        None if !app.is_pair() => Some(app.active_timeline_mut()),
        None => None,
    }
}

fn pointer_timeline_hits(hit_map: &HitMap, column: u16, row: u16) -> Option<&TimelinePaneHits> {
    hit_map.timelines.as_ref()?.at(column, row)
}

fn handle_mouse_event(
    app: &mut AppModel,
    mouse: MouseEvent,
    hit_map: &HitMap,
    mouse_state: &mut MouseState,
) -> RoutedEvent {
    if app.has_theme_picker() {
        app.disarm_exit();
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                let _ = app.handle_input(InputAction::MoveUp, hit_map.editor_width);
            }
            MouseEventKind::ScrollDown => {
                let _ = app.handle_input(InputAction::MoveDown, hit_map.editor_width);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = hit_map
                    .theme_rows
                    .iter()
                    .find(|hit| rect_contains(hit.area, mouse.column, mouse.row))
                {
                    app.select_theme(hit.index);
                }
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Moved
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {}
        }
        return RoutedEvent::Handled;
    }
    if !matches!(
        mouse.kind,
        MouseEventKind::Moved | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) && app.dismiss_quick_help()
    {
        app.disarm_exit();
        mouse_state.reset_gesture();
        return RoutedEvent::Handled;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            app.disarm_exit();
            if hit_map.takeover {
                app.scroll_takeover_by(
                    -WHEEL_SCROLL_ROWS,
                    hit_map.takeover_scrollbar.total_rows,
                    hit_map.takeover_scrollbar.viewport_rows,
                );
            } else if app.is_pair() {
                if let Some(timeline) = pointer_timeline_hits(hit_map, mouse.column, mouse.row) {
                    if let Some(target) = timeline_for_peer_mut(app, timeline.peer) {
                        target.scroll_by(
                            -WHEEL_SCROLL_ROWS,
                            timeline.wrap_width,
                            timeline.timeline.height,
                        );
                    }
                }
            } else if let Some(timeline) = active_timeline_hits(app, hit_map) {
                app.active_timeline_mut().scroll_by(
                    -WHEEL_SCROLL_ROWS,
                    timeline.wrap_width,
                    timeline.timeline.height,
                );
            }
            RoutedEvent::Handled
        }
        MouseEventKind::ScrollDown => {
            app.disarm_exit();
            if hit_map.takeover {
                app.scroll_takeover_by(
                    WHEEL_SCROLL_ROWS,
                    hit_map.takeover_scrollbar.total_rows,
                    hit_map.takeover_scrollbar.viewport_rows,
                );
            } else if app.is_pair() {
                if let Some(timeline) = pointer_timeline_hits(hit_map, mouse.column, mouse.row) {
                    if let Some(target) = timeline_for_peer_mut(app, timeline.peer) {
                        target.scroll_by(
                            WHEEL_SCROLL_ROWS,
                            timeline.wrap_width,
                            timeline.timeline.height,
                        );
                    }
                }
            } else if let Some(timeline) = active_timeline_hits(app, hit_map) {
                app.active_timeline_mut().scroll_by(
                    WHEEL_SCROLL_ROWS,
                    timeline.wrap_width,
                    timeline.timeline.height,
                );
            }
            RoutedEvent::Handled
        }
        MouseEventKind::Down(MouseButton::Left) => {
            app.disarm_exit();
            mouse_state.reset_gesture();

            let clicked_timeline = (!hit_map.takeover)
                .then(|| pointer_timeline_hits(hit_map, mouse.column, mouse.row))
                .flatten();
            if let Some(peer) = clicked_timeline.and_then(|timeline| timeline.peer) {
                let _ = app.select_pair_pane(peer);
            }
            let input_timeline = if app.is_pair() {
                clicked_timeline
            } else {
                active_timeline_hits(app, hit_map)
            };
            let scrollbar = if hit_map.takeover {
                Some((ScrollbarTarget::Takeover, &hit_map.takeover_scrollbar))
            } else {
                input_timeline.map(|timeline| {
                    (
                        ScrollbarTarget::Timeline(timeline.peer),
                        &timeline.scrollbar,
                    )
                })
            };
            if let Some((target, scrollbar)) = scrollbar {
                if rect_contains(scrollbar.track, mouse.column, mouse.row) {
                    if let Some(thumb) = scrollbar.thumb {
                        if rect_contains(thumb, mouse.column, mouse.row) {
                            mouse_state.scrollbar = Some(ScrollbarGesture {
                                target,
                                grab: mouse.row.saturating_sub(thumb.y),
                            });
                        } else {
                            let viewport_height = if hit_map.takeover {
                                usize::from(hit_map.takeover_content.height.max(1))
                            } else {
                                input_timeline.map_or(1, |timeline| {
                                    usize::from(timeline.timeline.height.max(1))
                                })
                            };
                            let delta = isize::try_from(viewport_height).unwrap_or(isize::MAX);
                            let delta = if mouse.row < thumb.y { -delta } else { delta };
                            if hit_map.takeover {
                                app.scroll_takeover_by(
                                    delta,
                                    scrollbar.total_rows,
                                    scrollbar.viewport_rows,
                                );
                            } else if let Some(timeline) = input_timeline {
                                if let Some(target) = timeline_for_peer_mut(app, timeline.peer) {
                                    target.scroll_by(
                                        delta,
                                        timeline.wrap_width,
                                        timeline.timeline.height,
                                    );
                                }
                            }
                        }
                    }
                    return RoutedEvent::Handled;
                }
            }

            if hit_map.takeover {
                if let Some(hit) = hit_map
                    .takeover_file_rows
                    .iter()
                    .find(|hit| rect_contains(hit.area, mouse.column, mouse.row))
                {
                    app.select_diff_file(hit.index);
                } else if let Some(hit) = hit_map
                    .takeover_rows
                    .iter()
                    .find(|hit| rect_contains(hit.area, mouse.column, mouse.row))
                {
                    app.select_takeover_row(hit.index);
                }
                return RoutedEvent::Handled;
            }

            if let Some(tab) = hit_map
                .tabs
                .iter()
                .find(|tab| rect_contains(tab.area, mouse.column, mouse.row))
            {
                app.active_tab = tab.tab;
                app.active_timeline_mut().clear_selection();
                return RoutedEvent::Handled;
            }

            if input_timeline.is_some_and(|timeline| {
                timeline
                    .label
                    .is_some_and(|label| rect_contains(label, mouse.column, mouse.row))
            }) {
                return RoutedEvent::Handled;
            }

            if let Some(timeline) = input_timeline {
                if let Some(hit) = timeline.rows.iter().find(|hit| {
                    hit.y == mouse.row
                        && mouse.column >= timeline.timeline.x
                        && mouse.column < hit.content_x
                        && matches!(hit.row.part, VisualRowPart::Content { first: true, .. })
                }) {
                    if let Some(target) = timeline_for_peer_mut(app, timeline.peer) {
                        if target.toggle_expandable(hit.row.item_id) {
                            target.clear_selection();
                            return RoutedEvent::Handled;
                        }
                    }
                }
            }

            if rect_contains(hit_map.composer, mouse.column, mouse.row) {
                if app.has_input_overlay() {
                    return RoutedEvent::Handled;
                }
                let visual_row = hit_map
                    .composer_scroll
                    .saturating_add(usize::from(mouse.row.saturating_sub(hit_map.composer.y)));
                app.editor.set_cursor_from_visual(
                    visual_row,
                    mouse.column.saturating_sub(hit_map.composer.x),
                    hit_map.editor_width,
                );
                app.active_timeline_mut().clear_selection();
                return RoutedEvent::Handled;
            }

            if app.is_pair() && input_timeline.is_none() {
                return RoutedEvent::Handled;
            }

            if let Some((leading, trailing)) = selection_points(
                hit_map,
                input_timeline.and_then(|timeline| timeline.peer),
                mouse.column,
                mouse.row,
            ) {
                let peer = input_timeline.and_then(|timeline| timeline.peer);
                if let Some(target) = timeline_for_peer_mut(app, peer) {
                    target.start_selection(leading);
                }
                mouse_state.selection = Some(SelectionGesture {
                    peer,
                    leading,
                    trailing,
                    origin_column: mouse.column,
                    origin_row: mouse.row,
                });
                mouse_state.selection_pointer = Some((mouse.column, mouse.row));
            } else if let Some(peer) = input_timeline.map(|timeline| timeline.peer) {
                if let Some(target) = timeline_for_peer_mut(app, peer) {
                    target.clear_selection();
                }
            }
            RoutedEvent::Handled
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            app.disarm_exit();
            if let Some(gesture) = mouse_state.scrollbar {
                let thumb_top = mouse.row.saturating_sub(gesture.grab);
                match gesture.target {
                    ScrollbarTarget::Takeover => {
                        if let Some(start) = hit_map
                            .takeover_scrollbar
                            .content_start_for_thumb_top(thumb_top)
                        {
                            app.scroll_takeover_to(
                                start,
                                hit_map.takeover_scrollbar.total_rows,
                                hit_map.takeover_scrollbar.viewport_rows,
                            );
                        }
                    }
                    ScrollbarTarget::Timeline(peer) => {
                        if let Some(timeline) = timeline_hits_for_peer(hit_map, peer) {
                            if let Some(start) =
                                timeline.scrollbar.content_start_for_thumb_top(thumb_top)
                            {
                                if let Some(target) = timeline_for_peer_mut(app, peer) {
                                    target.scroll_to_row(
                                        start,
                                        timeline.wrap_width,
                                        timeline.timeline.height,
                                    );
                                }
                            }
                        }
                    }
                }
                return RoutedEvent::Handled;
            }
            if hit_map.takeover {
                return RoutedEvent::Handled;
            }
            if mouse_state.selection.is_some() {
                mouse_state.selection_pointer = Some((mouse.column, mouse.row));
                advance_mouse_selection(app, hit_map, mouse_state);
            }
            RoutedEvent::Handled
        }
        MouseEventKind::Up(MouseButton::Left) => {
            app.disarm_exit();
            if mouse_state.scrollbar.is_some() {
                mouse_state.reset_gesture();
                return RoutedEvent::Handled;
            }
            let selecting = mouse_state.selection;
            if let Some(gesture) = selecting {
                if (mouse.row, mouse.column) == (gesture.origin_row, gesture.origin_column) {
                    if let Some(target) = timeline_for_peer_mut(app, gesture.peer) {
                        target.clear_selection();
                    }
                } else {
                    extend_mouse_selection(app, hit_map, mouse_state, mouse.column, mouse.row);
                }
            }
            mouse_state.reset_gesture();
            if selecting.is_some() && app.copy_on_select() {
                selecting
                    .and_then(|gesture| timeline_for_peer(app, gesture.peer))
                    .and_then(Timeline::selected_text)
                    .map_or(RoutedEvent::Handled, RoutedEvent::Copy)
            } else {
                RoutedEvent::Handled
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            app.disarm_exit();
            if hit_map.takeover {
                return RoutedEvent::Handled;
            }
            if rect_contains(hit_map.composer, mouse.column, mouse.row) {
                return RoutedEvent::PasteClipboard;
            }
            if let Some(timeline) = pointer_timeline_hits(hit_map, mouse.column, mouse.row)
                .filter(|timeline| rect_contains(timeline.timeline, mouse.column, mouse.row))
            {
                return timeline_for_peer(app, timeline.peer)
                    .and_then(Timeline::selected_text)
                    .map_or(RoutedEvent::Handled, RoutedEvent::Copy);
            }
            RoutedEvent::Handled
        }
        MouseEventKind::Moved => {
            mouse_state.reset_gesture();
            RoutedEvent::Handled
        }
        MouseEventKind::Down(_)
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => {
            if hit_map.takeover {
                RoutedEvent::Handled
            } else {
                RoutedEvent::Unhandled
            }
        }
    }
}

fn advance_mouse_selection(app: &mut AppModel, hit_map: &HitMap, mouse_state: &MouseState) {
    let Some((column, row)) = mouse_state.selection_pointer else {
        return;
    };
    let Some(gesture) = mouse_state.selection else {
        return;
    };
    let Some(timeline) = timeline_hits_for_peer(hit_map, gesture.peer) else {
        return;
    };
    if row < timeline.timeline.y {
        if let Some(target) = timeline_for_peer_mut(app, gesture.peer) {
            target.scroll_by(-1, timeline.wrap_width, timeline.timeline.height);
        }
    } else if row >= timeline.timeline.bottom() {
        if let Some(target) = timeline_for_peer_mut(app, gesture.peer) {
            target.scroll_by(1, timeline.wrap_width, timeline.timeline.height);
        }
    }
    extend_mouse_selection(app, hit_map, mouse_state, column, row);
}

fn extend_mouse_selection(
    app: &mut AppModel,
    hit_map: &HitMap,
    mouse_state: &MouseState,
    column: u16,
    row: u16,
) {
    let Some(gesture) = mouse_state.selection else {
        return;
    };
    let Some((leading, trailing)) = selection_points_nearest(hit_map, gesture.peer, column, row)
    else {
        return;
    };
    let Some(target) = timeline_for_peer_mut(app, gesture.peer) else {
        return;
    };
    if (row, column) >= (gesture.origin_row, gesture.origin_column) {
        target.start_selection(gesture.leading);
        target.extend_selection(trailing);
    } else {
        target.start_selection(gesture.trailing);
        target.extend_selection(leading);
    }
}

fn selection_points(
    hit_map: &HitMap,
    peer: Option<PeerPane>,
    column: u16,
    row: u16,
) -> Option<(ContentPoint, ContentPoint)> {
    let hit = timeline_hits_for_peer(hit_map, peer)?
        .rows
        .iter()
        .find(|hit| hit.y == row)?;
    Some((
        hit.point_for_column(column, false),
        hit.point_for_column(column, true),
    ))
}

fn selection_points_nearest(
    hit_map: &HitMap,
    peer: Option<PeerPane>,
    column: u16,
    row: u16,
) -> Option<(ContentPoint, ContentPoint)> {
    selection_points(hit_map, peer, column, row).or_else(|| {
        let hit = timeline_hits_for_peer(hit_map, peer)?
            .rows
            .iter()
            .min_by_key(|hit| hit.y.abs_diff(row))?;
        Some((
            hit.point_for_column(column, false),
            hit.point_for_column(column, true),
        ))
    })
}

fn apply_timeline_navigation(app: &mut AppModel, navigation: TimelineNavigation, hit_map: &HitMap) {
    if hit_map.takeover {
        let navigation = match navigation {
            TimelineNavigation::PageUp => TakeoverNavigation::PageUp,
            TimelineNavigation::PageDown => TakeoverNavigation::PageDown,
        };
        apply_takeover_navigation(app, navigation, hit_map);
        return;
    }
    let Some(timeline) = active_timeline_hits(app, hit_map) else {
        return;
    };
    let page = isize::try_from(timeline.timeline.height.max(1)).unwrap_or(isize::MAX);
    match navigation {
        TimelineNavigation::PageUp => app.active_timeline_mut().scroll_by(
            -page,
            timeline.wrap_width,
            timeline.timeline.height,
        ),
        TimelineNavigation::PageDown => {
            app.active_timeline_mut()
                .scroll_by(page, timeline.wrap_width, timeline.timeline.height)
        }
    }
}

fn apply_takeover_navigation(app: &mut AppModel, navigation: TakeoverNavigation, hit_map: &HitMap) {
    let page =
        isize::try_from(hit_map.takeover_scrollbar.viewport_rows.max(1)).unwrap_or(isize::MAX);
    let delta = match navigation {
        TakeoverNavigation::LineUp => -1,
        TakeoverNavigation::LineDown => 1,
        TakeoverNavigation::PageUp => -page,
        TakeoverNavigation::PageDown => page,
    };
    app.scroll_takeover_by(
        delta,
        hit_map.takeover_scrollbar.total_rows,
        hit_map.takeover_scrollbar.viewport_rows,
    );
}

fn rect_contains(rect: ratatui::layout::Rect, column: u16, row: u16) -> bool {
    column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
}

fn persist_prompt(
    app: &mut AppModel,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    submitted: &SubmittedInput,
) {
    let pastes = submitted
        .pastes
        .iter()
        .map(|paste| localpilot_store::HistoryPaste {
            placeholder: paste.placeholder.clone(),
            content: paste.content.clone(),
        })
        .collect::<Vec<_>>();
    if let Err(error) = history.append(&submitted.shown, &pastes, cwd) {
        app.apply_runtime(RuntimeUpdate::Warning(format!(
            "prompt history could not be saved: {error}"
        )));
    }
}

fn expand_history_entry(entry: &localpilot_store::HistoryEntry) -> String {
    let mut expanded = String::with_capacity(entry.text.len());
    let mut copied = 0;
    for paste in &entry.pastes {
        let Some(relative) = entry.text[copied..].find(&paste.placeholder) else {
            continue;
        };
        let start = copied + relative;
        let end = start + paste.placeholder.len();
        expanded.push_str(&entry.text[copied..start]);
        expanded.push_str(&paste.content);
        copied = end;
    }
    expanded.push_str(&entry.text[copied..]);
    expanded
}

fn handle_approval_event(
    app: &mut AppModel,
    event: Event,
    pending: &mut Option<oneshot::Sender<bool>>,
    cancel: &CancellationToken,
) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    if !is_key_action(key) {
        return false;
    }
    if is_cancel(key) {
        let command = app.handle_input(InputAction::CancelOrExit, 1);
        return match command {
            AppCommand::Copy(text) => {
                copy_to_clipboard(app, text);
                false
            }
            AppCommand::Exit => {
                deny_pending(app, pending);
                cancel.cancel();
                true
            }
            AppCommand::CancelWork => {
                deny_pending(app, pending);
                cancel.cancel();
                false
            }
            _ => false,
        };
    }
    let answer = match key.code {
        KeyCode::Char('y' | 'Y') => Some(true),
        KeyCode::Enter if !app.capabilities.screen_reader => Some(true),
        KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
        _ => None,
    };
    if let Some(answer) = answer {
        if let Some(reply) = pending.take() {
            let _ = reply.send(answer);
        }
        app.clear_dialog();
    }
    false
}

fn handle_question_event(
    app: &mut AppModel,
    event: Event,
    pending: &mut Option<PendingQuestions>,
    cancel: &CancellationToken,
    hit_map: &HitMap,
) -> bool {
    match event {
        Event::Mouse(mouse) => {
            app.disarm_exit();
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    if let Some(timeline) = active_timeline_hits(app, hit_map) {
                        app.active_timeline_mut().scroll_by(
                            -WHEEL_SCROLL_ROWS,
                            timeline.wrap_width,
                            timeline.timeline.height,
                        );
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(timeline) = active_timeline_hits(app, hit_map) {
                        app.active_timeline_mut().scroll_by(
                            WHEEL_SCROLL_ROWS,
                            timeline.wrap_width,
                            timeline.timeline.height,
                        );
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(hit) = hit_map
                        .question_rows
                        .iter()
                        .find(|hit| rect_contains(hit.area, mouse.column, mouse.row))
                    {
                        app.select_question_option(hit.index);
                    }
                }
                _ => {}
            }
            false
        }
        Event::Paste(text) => {
            app.disarm_exit();
            let resolution = app.handle_question_input(InputAction::Paste(text));
            resolve_question_action(app, resolution, pending)
        }
        Event::Key(key) if is_key_action(key) => {
            if is_cancel(key) {
                return match app.handle_input(InputAction::CancelOrExit, hit_map.editor_width) {
                    AppCommand::Exit => {
                        dismiss_pending_questions(app, pending);
                        cancel.cancel();
                        true
                    }
                    AppCommand::CancelWork => {
                        dismiss_pending_questions(app, pending);
                        cancel.cancel();
                        false
                    }
                    AppCommand::Copy(text) => {
                        copy_to_clipboard(app, text);
                        false
                    }
                    _ => false,
                };
            }
            app.disarm_exit();
            let Some(action) = map_key(key) else {
                return false;
            };
            if let InputAction::NavigateTimeline(navigation) = action {
                apply_timeline_navigation(app, navigation, hit_map);
                return false;
            }
            let resolution = app.handle_question_input(action);
            resolve_question_action(app, resolution, pending)
        }
        Event::FocusGained | Event::FocusLost | Event::Resize(_, _) | Event::Key(_) => false,
    }
}

fn resolve_question_action(
    app: &mut AppModel,
    action: QuestionAction,
    pending: &mut Option<PendingQuestions>,
) -> bool {
    match action {
        QuestionAction::None => false,
        QuestionAction::Submit(response) => {
            let answer = match response {
                QuestionResponse::Selected(labels) => UserAnswer::Selected(labels),
                QuestionResponse::Other(text) => UserAnswer::Other(text),
            };
            let finished = pending
                .as_mut()
                .is_some_and(|questions| questions.advance(app, answer));
            if finished {
                if let Some(questions) = pending.take() {
                    questions.finish();
                }
                app.clear_dialog();
            }
            false
        }
        QuestionAction::Cancel => {
            dismiss_pending_questions(app, pending);
            false
        }
    }
}

fn dismiss_pending_questions(app: &mut AppModel, pending: &mut Option<PendingQuestions>) {
    if let Some(questions) = pending.take() {
        questions.finish();
    }
    app.clear_dialog();
}

fn deny_pending(app: &mut AppModel, pending: &mut Option<oneshot::Sender<bool>>) {
    if let Some(reply) = pending.take() {
        let _ = reply.send(false);
    }
    app.clear_dialog();
}

fn deny_buffered_approvals(approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>) {
    while let Ok(call) = approval_rx.try_recv() {
        let _ = call.reply.send(false);
    }
}

fn dismiss_buffered_questions(question_rx: &mut mpsc::UnboundedReceiver<QuestionCall>) {
    while let Ok(call) = question_rx.try_recv() {
        let _ = call
            .reply
            .send(vec![UserAnswer::Dismissed; call.questions.len()]);
    }
}

fn present_shell_diagnostic(output: &str) -> &str {
    output
        .split_once("\noutput:\n")
        .map_or(output, |(_, body)| body)
        .trim()
}

fn apply_runtime_event(
    app: &mut AppModel,
    event: RuntimeEvent,
    pending_steer_items: &mut VecDeque<ItemId>,
) {
    if matches!(
        &event,
        RuntimeEvent::SoftInterruptInjected { source, .. } if source == "user"
    ) {
        if let Some(item_id) = pending_steer_items.pop_front() {
            let _ = app.activate_prompt(item_id);
        }
    }
    app.apply_runtime(map_runtime_event(event));
}

fn drain_runtime_events(
    app: &mut AppModel,
    rx: &mut broadcast::Receiver<RuntimeEvent>,
    pending_steer_items: &mut VecDeque<ItemId>,
) {
    loop {
        match rx.try_recv() {
            Ok(event) => apply_runtime_event(app, event, pending_steer_items),
            Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                break
            }
        }
    }
}

fn local_prompt_time() -> String {
    let offset = LOCAL_UTC_OFFSET
        .get()
        .copied()
        .unwrap_or(time::UtcOffset::UTC);
    let now = time::OffsetDateTime::now_utc().to_offset(offset);
    format_prompt_time(now)
}

fn format_prompt_time(now: time::OffsetDateTime) -> String {
    format!("{:02}:{:02}", now.hour(), now.minute())
}

fn copy_to_clipboard(app: &mut AppModel, text: String) {
    let result = arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text));
    if let Err(error) = result {
        app.apply_runtime(RuntimeUpdate::Warning(format!(
            "clipboard copy unavailable: {error}"
        )));
    }
}

fn paste_text_from_clipboard(app: &mut AppModel, editor_width: u16) {
    let text = arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text());
    if let Ok(text) = text {
        apply_clipboard_text(app, editor_width, text);
    }
}

fn apply_clipboard_text(app: &mut AppModel, editor_width: u16, text: String) {
    if !text.is_empty() {
        let _ = app.handle_input(InputAction::Paste(text), editor_width);
    }
}

fn draw_synchronized(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &AppModel,
) -> Result<localpilot_terminal_ui::HitMap> {
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)
        .context("begin synchronized full-screen update")?;
    let mut hit_map = None;
    let draw_result = terminal
        .draw(|frame| hit_map = Some(render(frame, app)))
        .map(|_| ());
    let end_result = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    draw_result.context("draw full-screen frame")?;
    end_result.context("end synchronized full-screen update")?;
    hit_map.context("full-screen render did not produce a hit map")
}

fn map_key(key: KeyEvent) -> Option<InputAction> {
    if is_cancel(key) {
        return Some(InputAction::CancelOrExit);
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::F(6) if !ctrl && !alt && !shift => Some(InputAction::CyclePeer),
        KeyCode::PageUp => Some(InputAction::NavigateTimeline(TimelineNavigation::PageUp)),
        KeyCode::PageDown => Some(InputAction::NavigateTimeline(TimelineNavigation::PageDown)),
        KeyCode::Home if ctrl && !alt => Some(InputAction::MoveTextStart),
        KeyCode::End if ctrl && !alt => Some(InputAction::MoveTextEnd),
        KeyCode::Home if !ctrl && !alt => Some(InputAction::MoveVisualStart),
        KeyCode::End if !ctrl && !alt => Some(InputAction::MoveVisualEnd),
        KeyCode::Left if alt && !ctrl => Some(InputAction::MoveWordLeft),
        KeyCode::Right if alt && !ctrl => Some(InputAction::MoveWordRight),
        KeyCode::Char('a') if ctrl && !alt => Some(InputAction::MoveLineStart),
        KeyCode::Char('b') if ctrl && !alt => Some(InputAction::MoveLeft),
        KeyCode::Char('e') if ctrl && !alt => Some(InputAction::MoveLineEnd),
        KeyCode::Char('f') if ctrl && !alt => Some(InputAction::ForwardCharOrSearch),
        KeyCode::Char('g') if ctrl && !alt => Some(InputAction::OpenExternalEditor),
        KeyCode::Char('h') if ctrl && !alt => Some(InputAction::Backspace),
        KeyCode::Char('j') if ctrl && !alt => Some(InputAction::Insert("\n".to_string())),
        KeyCode::Char('k') if ctrl && !alt => Some(InputAction::DeleteToLineEnd),
        KeyCode::Char('r') if ctrl && !alt => Some(InputAction::OpenReverseHistory),
        KeyCode::Char('s') if ctrl && !alt => Some(InputAction::StashOrPop),
        KeyCode::Char('u') if ctrl && !alt => Some(InputAction::DeleteToLineStart),
        KeyCode::Char('w') if ctrl && !alt => Some(InputAction::DeleteWordLeft),
        KeyCode::Char('y') if ctrl && !alt => Some(InputAction::AcceptCompletion),
        KeyCode::Char(character) if !ctrl && !alt => {
            Some(InputAction::Insert(character.to_string()))
        }
        KeyCode::Enter if alt || shift => Some(InputAction::Insert("\n".to_string())),
        KeyCode::Enter => Some(InputAction::Submit),
        KeyCode::Tab => Some(InputAction::AcceptCompletion),
        KeyCode::Esc => Some(InputAction::Escape),
        KeyCode::Backspace => Some(InputAction::Backspace),
        KeyCode::Delete => Some(InputAction::Delete),
        KeyCode::Left => Some(InputAction::MoveLeft),
        KeyCode::Right => Some(InputAction::MoveRight),
        KeyCode::Up => Some(InputAction::MoveUp),
        KeyCode::Down => Some(InputAction::MoveDown),
        _ => None,
    }
}

fn is_enqueue_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('q')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
}

fn buffered_after_fullscreen_key(key: KeyEvent) -> io::Result<bool> {
    if !may_be_unbracketed_paste_key(key) {
        return Ok(false);
    }
    let timeout = if is_unbracketed_paste_newline_key(key) {
        Duration::from_millis(4)
    } else {
        Duration::from_millis(3)
    };
    event::poll(timeout)
}

/// Normalize Windows terminals that deliver paste as a rapid key-record burst.
/// Returns `true` when the current key was consumed as paste content; a
/// FlushThenPass inserts the accumulated paste and lets the command key continue.
fn handle_fullscreen_paste_burst(
    app: &mut AppModel,
    burst: &mut PasteBurst,
    key: KeyEvent,
    buffered_after: bool,
    editor_width: u16,
) -> bool {
    match burst.observe(key, buffered_after, Instant::now()) {
        PasteAction::Pass => false,
        PasteAction::Absorbed => true,
        PasteAction::Flush(text) => {
            let _ = app.handle_input(InputAction::Paste(text), editor_width);
            true
        }
        PasteAction::FlushThenPass(text) => {
            let _ = app.handle_input(InputAction::Paste(text), editor_width);
            false
        }
    }
}

/// Apply the same burst detector while a dialog owns keyboard focus. Approval
/// and trust dialogs have no text field, so pasted text is discarded. The
/// question dialog accepts it only while its explicit Other editor is active.
fn handle_dialog_paste_burst(
    app: &mut AppModel,
    burst: &mut PasteBurst,
    key: KeyEvent,
    buffered_after: bool,
    question_dialog: bool,
) -> bool {
    let route_text = |app: &mut AppModel, text: String| {
        if question_dialog {
            // The model itself ignores text until the explicit Other editor is
            // active, matching the bracketed Event::Paste path.
            let _ = app.handle_question_input(InputAction::Paste(text));
        }
    };
    match burst.observe(key, buffered_after, Instant::now()) {
        PasteAction::Pass => false,
        PasteAction::Absorbed => true,
        PasteAction::Flush(text) => {
            route_text(app, text);
            true
        }
        PasteAction::FlushThenPass(text) => {
            route_text(app, text);
            false
        }
    }
}

pub(crate) fn map_runtime_event(event: RuntimeEvent) -> RuntimeUpdate {
    match event {
        RuntimeEvent::Text(text) => RuntimeUpdate::Text(text),
        RuntimeEvent::Reasoning(text) => RuntimeUpdate::Reasoning(text),
        RuntimeEvent::ToolStarted { id, name, detail } => {
            RuntimeUpdate::ToolStarted { id, name, detail }
        }
        RuntimeEvent::ToolFinished {
            id,
            name,
            is_error,
            cancelled,
            output,
            duration_ms,
        } => RuntimeUpdate::ToolFinished {
            id,
            name,
            is_error,
            cancelled,
            output,
            duration_ms,
        },
        RuntimeEvent::Usage(usage) => RuntimeUpdate::Usage {
            input_tokens: usage.effective_input_tokens(),
            output_tokens: usage.output_tokens,
            cached_input_tokens: usage.cache_read_input_tokens,
        },
        RuntimeEvent::ContextUsage { used, limit } => RuntimeUpdate::ContextUsage { used, limit },
        RuntimeEvent::Warning(message) => RuntimeUpdate::Warning(message),
        RuntimeEvent::Plan(steps) => RuntimeUpdate::Plan(
            steps
                .into_iter()
                .map(|step| PlanEntry {
                    title: step.title,
                    status: step.status,
                })
                .collect(),
        ),
        RuntimeEvent::QuotaPaused { reset } => RuntimeUpdate::QuotaPaused(reset),
        RuntimeEvent::Recovery { health } => RuntimeUpdate::Recovery(match health {
            ModelHealth::Healthy => RecoveryState::Healthy,
            ModelHealth::Recovering => RecoveryState::Recovering,
            ModelHealth::Degraded => RecoveryState::Degraded,
        }),
        RuntimeEvent::ToolStuck { name, count } => RuntimeUpdate::ToolStuck { name, count },
        RuntimeEvent::FilesTouched(_) => RuntimeUpdate::FilesTouched,
        RuntimeEvent::SoftInterruptInjected { .. } => RuntimeUpdate::SoftInterruptInjected,
        RuntimeEvent::Stopped(reason) => RuntimeUpdate::Stopped(match reason {
            StopReason::Done => StopState::Done,
            StopReason::Cancelled => StopState::Cancelled,
            StopReason::Degraded => StopState::Degraded,
            StopReason::ProviderError => StopState::ProviderError,
            StopReason::BudgetExceeded => StopState::BudgetExceeded,
            StopReason::NoProgress => StopState::NoProgress,
            StopReason::TimedOut => StopState::TimedOut,
            StopReason::Quiesced => StopState::Quiesced,
        }),
    }
}

struct TerminalModes {
    active: bool,
    mouse_capture: bool,
}

impl TerminalModes {
    fn enter(mouse_capture: bool) -> Result<(Self, TerminalCapabilities)> {
        terminal::enable_raw_mode().context("enable raw terminal mode")?;
        TERMINAL_MODES_ACTIVE.store(true, Ordering::Release);
        MOUSE_CAPTURE_ACTIVE.store(mouse_capture, Ordering::Release);
        let mut guard = Self {
            active: true,
            mouse_capture,
        };
        let mut stdout = io::stdout();
        if let Err(error) = write_required_modes(&mut stdout, mouse_capture) {
            guard.restore();
            return Err(error).context("enter full-screen terminal modes");
        }
        let enhanced = write_keyboard_enhancement(&mut stdout).is_ok();
        KEYBOARD_FLAGS_PUSHED.store(enhanced, Ordering::Release);
        let clipboard_write = arboard::Clipboard::new().is_ok();
        let capabilities = TerminalCapabilities {
            color: if std::env::var_os("NO_COLOR").is_some() {
                ColorSupport::NoColor
            } else {
                ColorSupport::Color
            },
            mouse_capture,
            synchronized_output: true,
            keyboard: if enhanced {
                KeyboardSupport::Enhanced
            } else {
                KeyboardSupport::Basic
            },
            clipboard_write,
            screen_reader: std::env::var(CHAT_SCREEN_READER_ENV)
                .ok()
                .as_deref()
                .and_then(parse_bool_setting)
                .unwrap_or(false),
        };
        Ok((guard, capabilities))
    }

    fn restore(&mut self) {
        if !self.active {
            return;
        }
        restore_terminal_modes();
        self.active = false;
    }
}

impl SuspensibleModes for TerminalModes {
    type Capabilities = TerminalCapabilities;

    fn leave(&mut self) {
        self.restore();
    }

    fn reenter(&mut self) -> Result<Self::Capabilities> {
        let (replacement, capabilities) = Self::enter(self.mouse_capture)?;
        *self = replacement;
        Ok(capabilities)
    }
}

impl Drop for TerminalModes {
    fn drop(&mut self) {
        self.restore();
    }
}

fn write_required_modes(writer: &mut impl Write, mouse_capture: bool) -> io::Result<()> {
    execute!(writer, EnterAlternateScreen)?;
    if mouse_capture {
        execute!(writer, EnableMouseCapture)?;
    }
    execute!(writer, EnableBracketedPaste, Hide)
}

fn write_keyboard_enhancement(writer: &mut impl Write) -> io::Result<()> {
    execute!(
        writer,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
        )
    )
}

fn write_restore_modes(
    writer: &mut impl Write,
    keyboard_flags_pushed: bool,
    mouse_capture: bool,
) -> io::Result<()> {
    if keyboard_flags_pushed {
        // Keyboard enhancement is opportunistic. Its legacy Windows command
        // may be unsupported, but that must never prevent required cleanup.
        let _ = execute!(writer, PopKeyboardEnhancementFlags);
    }
    execute!(writer, Show, DisableBracketedPaste)?;
    if mouse_capture {
        execute!(writer, DisableMouseCapture)?;
    }
    execute!(writer, LeaveAlternateScreen)
}

fn restore_terminal_modes() {
    if !TERMINAL_MODES_ACTIVE.swap(false, Ordering::AcqRel) {
        return;
    }
    let keyboard_flags_pushed = KEYBOARD_FLAGS_PUSHED.swap(false, Ordering::AcqRel);
    let mouse_capture = MOUSE_CAPTURE_ACTIVE.swap(false, Ordering::AcqRel);
    let _ = write_restore_modes(&mut io::stdout(), keyboard_flags_pushed, mouse_capture);
    let _ = terminal::disable_raw_mode();
}

fn install_panic_restore_hook() {
    let driver = std::thread::current().id();
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == driver {
            restore_terminal_modes();
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, MouseEvent};
    use localpilot_config::ProviderConfig;
    use localpilot_core::TokenUsage;
    use localpilot_llm::{FakeProvider, ModelProvider, ProviderRegistry};
    use localpilot_sandbox::Profile;
    use localpilot_server::swarm::PairBounds;
    use localpilot_terminal_ui::{ItemKind, ViewportAnchor, WorkState};
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::interactive_session::{
        InteractivePairHost, InteractivePeerSelection, InteractiveSessionSetup,
    };

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn draw_hit_map(app: &AppModel, width: u16, height: u16) -> HitMap {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, app)))
            .expect("draw hit map");
        hit_map.expect("hit map")
    }

    fn event_hit_map() -> HitMap {
        draw_hit_map(&app(), 80, 24)
    }

    fn single_hits(hit_map: &HitMap) -> &TimelinePaneHits {
        hit_map
            .timelines
            .as_ref()
            .and_then(|timelines| timelines.active(None))
            .expect("single timeline hits")
    }

    fn app() -> AppModel {
        AppModel::new(
            Header {
                version: "0".to_string(),
                provider: "fixture".to_string(),
                model: "fixture-model".to_string(),
                workspace: "fixture-workspace".to_string(),
                branch: Some("fixture-branch".to_string()),
                workspace_dirty: Some(true),
                mode: "agent".to_string(),
                profile: "default".to_string(),
                session_id: "fixture-session".to_string(),
                session_name: None,
            },
            TerminalCapabilities::default(),
        )
    }

    fn pair_app() -> AppModel {
        pair_app_with_workspace("fixture-workspace")
    }

    fn pair_app_with_workspace(workspace: &str) -> AppModel {
        AppModel::new_pair(
            Header {
                version: "0".to_string(),
                provider: "provider-a".to_string(),
                model: "model-a".to_string(),
                workspace: workspace.to_string(),
                branch: Some("fixture-branch".to_string()),
                workspace_dirty: Some(true),
                mode: "agent".to_string(),
                profile: "default".to_string(),
                session_id: "session-a".to_string(),
                session_name: Some("Alpha".to_string()),
            },
            localpilot_terminal_ui::SessionHeader {
                provider: "provider-b".to_string(),
                model: "model-b".to_string(),
                session_id: "session-b".to_string(),
                session_name: Some("Beta".to_string()),
            },
            TerminalCapabilities::default(),
        )
    }

    fn pair_question(label: &str) -> UserQuestion {
        UserQuestion {
            header: Some(label.to_string()),
            question: format!("Choose {label}"),
            options: vec![localpilot_tools::QuestionOption {
                label: label.to_string(),
                description: None,
            }],
            multi_select: false,
        }
    }

    #[tokio::test]
    async fn real_host_pump_drives_the_production_adapter_and_test_backend() {
        const A_PROPOSAL: &str = r#"{"v":1,"action":"propose","artifact":"alpha"}"#;
        const B_PROPOSAL: &str = r#"{"v":1,"action":"propose","artifact":"beta"}"#;
        let provider = |id: &str, response: &str| {
            let seed = FakeProvider::new();
            let mut declaration = seed.declaration().clone();
            declaration.id = id.to_string();
            declaration.display_name = id.to_string();
            Arc::new(
                FakeProvider::new()
                    .with_declaration(declaration)
                    .text(response),
            )
        };
        let first = provider("first", A_PROPOSAL);
        let second = provider("second", B_PROPOSAL);
        let providers = HashMap::from([
            ("first".to_string(), first.clone() as Arc<dyn ModelProvider>),
            (
                "second".to_string(),
                second.clone() as Arc<dyn ModelProvider>,
            ),
        ]);
        let models = HashMap::from([
            ("first".to_string(), "model-a".to_string()),
            ("second".to_string(), "model-b".to_string()),
        ]);
        let mut config = localpilot_config::Config::default();
        config.provider.default = "first".to_string();
        config
            .providers
            .insert("first".to_string(), ProviderConfig::default());
        config
            .providers
            .insert("second".to_string(), ProviderConfig::default());
        let directory = tempfile::tempdir().unwrap();
        let setup = InteractiveSessionSetup::for_test(
            directory.path().to_path_buf(),
            config,
            Profile::Default,
            ProviderRegistry::from_providers(providers, models, "first"),
        );
        let host = InteractivePairHost::prepare(
            &setup,
            "compare both proposals",
            InteractivePeerSelection {
                provider_id: "first",
                model: "model-a",
            },
            InteractivePeerSelection {
                provider_id: "second",
                model: "model-b",
            },
        )
        .await
        .unwrap();
        let mut run = PreparedPairRun::new(
            host,
            PairBounds {
                max_rounds: 1,
                slot_timeout: Duration::from_secs(5),
                slot_token_budget: 0,
            },
        )
        .unwrap()
        .spawn();
        let mut app = pair_app();
        let _ = app.begin_work_for(PeerPane::A);
        let _ = app.begin_work_for(PeerPane::B);
        let mut adapter = PairTerminalAdapter::new();

        tokio::time::timeout(Duration::from_secs(10), async {
            while adapter.terminal.is_none() {
                let event = run.next().await.expect("event before terminal status");
                let action = adapter.apply_pump_event(&mut app, event);
                assert!(matches!(action, PairHostAction::None));
                let hits = draw_hit_map(&app, 120, 30);
                assert!(hits.timelines.is_some());
            }
        })
        .await
        .expect("real pair host completed");

        let completion = run.shutdown().await;
        assert!(matches!(
            completion.terminal_status(),
            PairTerminalStatus::CapReached | PairTerminalStatus::Converged
        ));
        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));
        assert!(app
            .timeline_for(PeerPane::A)
            .unwrap()
            .items()
            .iter()
            .any(|item| item.text.contains("alpha")));
        assert!(app
            .timeline_for(PeerPane::B)
            .unwrap()
            .items()
            .iter()
            .any(|item| item.text.contains("beta")));
        assert!(!first.requests().is_empty());
        assert!(!second.requests().is_empty());
    }

    async fn pending_pair_run() -> (tempfile::TempDir, InteractivePairRun) {
        let provider = |id: &str| {
            let seed = FakeProvider::new();
            let mut declaration = seed.declaration().clone();
            declaration.id = id.to_string();
            declaration.display_name = id.to_string();
            Arc::new(FakeProvider::new().with_declaration(declaration).text("{}"))
                as Arc<dyn ModelProvider>
        };
        let providers = HashMap::from([
            ("first".to_string(), provider("first")),
            ("second".to_string(), provider("second")),
        ]);
        let models = HashMap::from([
            ("first".to_string(), "model-a".to_string()),
            ("second".to_string(), "model-b".to_string()),
        ]);
        let mut config = localpilot_config::Config::default();
        config.provider.default = "first".to_string();
        config
            .providers
            .insert("first".to_string(), ProviderConfig::default());
        config
            .providers
            .insert("second".to_string(), ProviderConfig::default());
        let directory = tempfile::tempdir().unwrap();
        let setup = InteractiveSessionSetup::for_test(
            directory.path().to_path_buf(),
            config,
            Profile::Default,
            ProviderRegistry::from_providers(providers, models, "first"),
        );
        let host = InteractivePairHost::prepare(
            &setup,
            "compare both proposals",
            InteractivePeerSelection {
                provider_id: "first",
                model: "model-a",
            },
            InteractivePeerSelection {
                provider_id: "second",
                model: "model-b",
            },
        )
        .await
        .unwrap();
        let run = PreparedPairRun::new(
            host,
            PairBounds {
                max_rounds: 1,
                slot_timeout: Duration::from_secs(5),
                slot_token_budget: 0,
            },
        )
        .unwrap()
        .spawn();
        (directory, run)
    }

    fn assert_both_panes_cancelling(app: &mut AppModel) {
        for pane in [PeerPane::A, PeerPane::B] {
            let _ = app.select_pair_pane(pane);
            assert_eq!(
                app.active_work(),
                localpilot_terminal_ui::WorkState::Busy {
                    cancellation_requested: true
                },
                "pane {pane:?} reflects the requested cancellation",
            );
        }
    }

    #[tokio::test]
    async fn the_abort_action_marks_both_busy_panes_cancelling_before_terminal() {
        let (_dir, mut run) = pending_pair_run().await;
        let mut app = pair_app();
        let mut adapter = PairTerminalAdapter::new();
        assert!(app.begin_work_for(PeerPane::A));
        assert!(app.begin_work_for(PeerPane::B));

        // The abort action reflects the pending cancellation on BOTH busy panes
        // through the real host-action seam, and does not fake terminal completion.
        let exit = execute_pair_host_action(
            &mut run,
            &mut adapter,
            &mut app,
            PairHostAction::Abort { exit: false },
        );
        assert!(!exit);
        assert!(
            adapter.terminal.is_none(),
            "the abort does not fake a terminal report"
        );
        assert_both_panes_cancelling(&mut app);

        let completion = run.shutdown().await;
        assert_eq!(completion.terminal_status(), PairTerminalStatus::Aborted);
    }

    #[tokio::test]
    async fn a_modal_abort_answer_also_marks_both_panes_cancelling() {
        let (_dir, mut run) = pending_pair_run().await;
        let mut app = pair_app();
        let mut adapter = PairTerminalAdapter::new();
        assert!(app.begin_work_for(PeerPane::A));
        assert!(app.begin_work_for(PeerPane::B));

        // A modal Ctrl+C answers with abort=true (here with no active request, taking
        // the stale/invariant aborting path); it must also cancel both panes.
        let exit = execute_pair_host_action(
            &mut run,
            &mut adapter,
            &mut app,
            PairHostAction::Answer {
                id: PairAskId::fixture(1),
                answer: PairAskAnswer::Approval(false),
                abort: true,
                exit: false,
            },
        );
        assert!(!exit);
        assert_both_panes_cancelling(&mut app);

        let completion = run.shutdown().await;
        assert_eq!(completion.terminal_status(), PairTerminalStatus::Aborted);
    }

    #[test]
    fn pair_terminal_adapter_routes_peer_updates_status_and_retained_completion() {
        let mut app = pair_app();
        let mut adapter = PairTerminalAdapter::new();
        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));

        assert!(matches!(
            adapter.apply_pump_event(
                &mut app,
                PairPumpEvent::Runtime {
                    peer: PairPeer::B,
                    event: RuntimeEvent::Text("beta stream".to_string()),
                },
            ),
            PairHostAction::None
        ));
        assert!(matches!(
            adapter.apply_pump_event(
                &mut app,
                PairPumpEvent::Runtime {
                    peer: PairPeer::A,
                    event: RuntimeEvent::Text("alpha stream".to_string()),
                },
            ),
            PairHostAction::None
        ));
        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));
        assert!(app
            .timeline_for(PeerPane::A)
            .unwrap()
            .items()
            .iter()
            .any(|item| item.text.contains("alpha stream")));
        assert!(app
            .timeline_for(PeerPane::B)
            .unwrap()
            .items()
            .iter()
            .any(|item| item.text.contains("beta stream")));

        let running = PairRunStatus {
            state: PairRunState::Running,
            completed_rounds: 1,
            max_rounds: 3,
            scheduled: Some(PairPeer::B),
            candidate: None,
            agreements: [false, false],
            repairing: None,
        };
        let _ = adapter.apply_pump_event(&mut app, PairPumpEvent::Progress(running));
        assert_eq!(
            app.pair_status(),
            Some(&PairStatus {
                completed_rounds: 1,
                max_rounds: 3,
                scheduled: Some(PeerPane::B),
                candidate: None,
                agreements: [false, false],
                repairing: None,
                terminal: None,
            })
        );
        let hits = draw_hit_map(&app, 120, 30);
        assert!(
            hits.timelines.is_some(),
            "pair status must remain renderable"
        );

        let finished = PairRunStatus {
            state: PairRunState::Finished(PairTerminalStatus::Converged),
            completed_rounds: 2,
            max_rounds: 3,
            scheduled: None,
            candidate: None,
            agreements: [false, false],
            repairing: None,
        };
        let _ = adapter.apply_pump_event(
            &mut app,
            PairPumpEvent::Finished {
                status: finished,
                result: Box::new(PairResultSnapshot::for_reason(
                    PairTerminalStatus::Converged,
                )),
            },
        );
        assert_eq!(adapter.terminal, Some(PairTerminalStatus::Converged));
        assert_eq!(
            app.pair_status()
                .and_then(|status| status.terminal.as_deref()),
            Some("Converged")
        );
    }

    #[test]
    fn every_terminal_reason_maps_to_an_honest_result_tone() {
        for (reason, tone) in [
            (PairTerminalStatus::Converged, ResultTone::Success),
            (PairTerminalStatus::CapReached, ResultTone::Incomplete),
            (PairTerminalStatus::Aborted, ResultTone::Incomplete),
            (PairTerminalStatus::ProtocolError, ResultTone::Error),
            (PairTerminalStatus::TimedOut, ResultTone::Error),
            (PairTerminalStatus::PeerFailed, ResultTone::Error),
            (PairTerminalStatus::ProviderError, ResultTone::Error),
            (PairTerminalStatus::BudgetExceeded, ResultTone::Error),
            (PairTerminalStatus::NoProgress, ResultTone::Error),
            (PairTerminalStatus::DriverFailed, ResultTone::Error),
            (PairTerminalStatus::Unknown, ResultTone::Error),
        ] {
            assert_eq!(result_tone(reason), tone, "tone for {reason:?}");
            // Only a convergence may claim success.
            if reason != PairTerminalStatus::Converged {
                assert_ne!(result_tone(reason), ResultTone::Success, "{reason:?}");
            }
        }
    }

    #[test]
    fn every_terminal_reason_renders_its_factual_headline() {
        let cases: [(PairTerminalStatus, Option<&str>, &str); 11] = [
            (PairTerminalStatus::Converged, None, "Converged."),
            (
                PairTerminalStatus::CapReached,
                None,
                "Round cap reached after 0 rounds; no convergence.",
            ),
            (
                PairTerminalStatus::Aborted,
                None,
                "Aborted before convergence.",
            ),
            (
                PairTerminalStatus::TimedOut,
                None,
                "Timed out before convergence.",
            ),
            (
                PairTerminalStatus::BudgetExceeded,
                None,
                "Budget exceeded before convergence.",
            ),
            (
                PairTerminalStatus::NoProgress,
                None,
                "Stopped with no progress before convergence.",
            ),
            (
                PairTerminalStatus::ProtocolError,
                Some("bad frame"),
                "Protocol error before convergence: bad frame",
            ),
            (
                PairTerminalStatus::PeerFailed,
                Some("peer down"),
                "A peer failed before convergence: peer down",
            ),
            (
                PairTerminalStatus::ProviderError,
                Some("429"),
                "Provider error before convergence: 429",
            ),
            (
                PairTerminalStatus::DriverFailed,
                Some("panicked"),
                "The driver failed: panicked",
            ),
            (
                PairTerminalStatus::Unknown,
                None,
                "Finished without convergence.",
            ),
        ];
        for (reason, detail, expected) in cases {
            let snapshot = PairResultSnapshot {
                reason,
                detail: detail.map(str::to_string),
                completed_rounds: 0,
                candidate: None,
                raw: [None, None],
            };
            let card = render_pair_result(&snapshot, PairPeer::A);
            assert!(card.contains(expected), "reason {reason:?} card: {card}");
            assert!(
                card.contains("Inspect/copy only; no files or version control were changed."),
                "footer missing for {reason:?}"
            );
        }
        // A convergence with a candidate names its revision.
        let converged = PairResultSnapshot {
            reason: PairTerminalStatus::Converged,
            detail: None,
            completed_rounds: 4,
            candidate: Some(PairResultCandidate {
                revision: 7,
                digest: "d".to_string(),
                artifact: "a".to_string(),
            }),
            raw: [None, None],
        };
        assert!(render_pair_result(&converged, PairPeer::A).contains("Converged at revision 7."));

        // The round-cap count agrees grammatically: exactly one round is singular.
        let one_round = PairResultSnapshot {
            completed_rounds: 1,
            ..PairResultSnapshot::for_reason(PairTerminalStatus::CapReached)
        };
        assert!(render_pair_result(&one_round, PairPeer::A)
            .contains("Round cap reached after 1 round; no convergence."));
    }

    #[test]
    fn result_cards_duplicate_the_candidate_but_keep_raw_peer_local() {
        let result = PairResultSnapshot {
            reason: PairTerminalStatus::Converged,
            detail: None,
            completed_rounds: 3,
            candidate: Some(PairResultCandidate {
                revision: 2,
                digest: "deadbeefcafe".to_string(),
                artifact: "shared artifact".to_string(),
            }),
            raw: [Some("ALPHA-RAW".to_string()), Some("BETA-RAW".to_string())],
        };
        let a = render_pair_result(&result, PairPeer::A);
        let b = render_pair_result(&result, PairPeer::B);
        assert!(
            a.contains("ALPHA-RAW") && !a.contains("BETA-RAW"),
            "A card: {a}"
        );
        assert!(
            b.contains("BETA-RAW") && !b.contains("ALPHA-RAW"),
            "B card: {b}"
        );
        for card in [&a, &b] {
            assert!(card.contains("Converged at revision 2."));
            assert!(card.contains("digest deadbeefcafe"));
            assert!(card.contains("shared artifact"));
            assert!(
                card.contains("Inspect/copy only; no files or version control were changed."),
                "footer missing: {card}"
            );
        }

        // Missing candidate and a peer with no response read as explicit unavailables,
        // and the round-cap headline names how many rounds ran.
        let cap = PairResultSnapshot {
            completed_rounds: 5,
            ..PairResultSnapshot::for_reason(PairTerminalStatus::CapReached)
        };
        let card = render_pair_result(&cap, PairPeer::A);
        assert!(card.contains("Round cap reached after 5 rounds; no convergence."));
        assert!(card.contains("Candidate: none was applied."));
        assert!(card.contains("Peer A produced no response"));
    }

    #[test]
    fn terminal_leaves_one_toned_result_row_in_each_pane() {
        let mut app = pair_app();
        let mut adapter = PairTerminalAdapter::new();
        let result = PairResultSnapshot {
            reason: PairTerminalStatus::CapReached,
            detail: None,
            completed_rounds: 1,
            candidate: Some(PairResultCandidate {
                revision: 1,
                digest: "abc123".to_string(),
                artifact: "draft".to_string(),
            }),
            raw: [
                Some("ALPHA-CARD".to_string()),
                Some("BETA-CARD".to_string()),
            ],
        };
        let _ = adapter.apply_pump_event(
            &mut app,
            PairPumpEvent::Finished {
                status: PairRunStatus {
                    state: PairRunState::Finished(PairTerminalStatus::CapReached),
                    completed_rounds: 1,
                    max_rounds: 1,
                    scheduled: None,
                    candidate: None,
                    agreements: [false, false],
                    repairing: None,
                },
                result: Box::new(result),
            },
        );
        for (pane, mine, theirs) in [
            (PeerPane::A, "ALPHA-CARD", "BETA-CARD"),
            (PeerPane::B, "BETA-CARD", "ALPHA-CARD"),
        ] {
            let timeline = app.timeline_for(pane).expect("peer timeline");
            let cards: Vec<_> = timeline
                .items()
                .iter()
                .filter(|item| item.kind == ItemKind::Result)
                .collect();
            assert_eq!(cards.len(), 1, "exactly one result card per pane");
            let card = cards[0];
            assert_eq!(card.tone, Some(ResultTone::Incomplete));
            assert!(card.text.contains(mine) && !card.text.contains(theirs));
            assert!(card
                .text
                .contains("Round cap reached after 1 round; no convergence."));
        }
    }

    #[test]
    fn abort_slash_maps_to_the_sole_abort_action_and_rejects_arguments() {
        let mut app = pair_app();
        let config = localpilot_config::Config::default();
        let cwd = Path::new(".");
        let submit = |prompt: &str| SubmittedInput {
            shown: prompt.to_string(),
            display: prompt.to_string(),
            prompt: prompt.to_string(),
            pastes: Vec::new(),
            images: Vec::new(),
        };
        // Bare `/abort` maps to the one existing abort action.
        assert!(matches!(
            execute_pair_slash(&mut app, &config, cwd, submit("/abort")),
            PairHostAction::Abort { exit: false }
        ));
        // Arguments are rejected with an explicit usage notice and no action.
        assert!(matches!(
            execute_pair_slash(&mut app, &config, cwd, submit("/abort now")),
            PairHostAction::None
        ));
        assert!(app
            .timeline_for(PeerPane::A)
            .expect("A timeline")
            .items()
            .iter()
            .any(|item| item.text.contains("usage: /abort")));

        // A different command that merely shares the prefix is NOT a mistyped `/abort`:
        // it neither aborts nor prints the abort usage.
        let mut other = pair_app();
        assert!(!matches!(
            execute_pair_slash(&mut other, &config, cwd, submit("/abortive")),
            PairHostAction::Abort { .. }
        ));
        assert!(other
            .timeline_for(PeerPane::A)
            .expect("A timeline")
            .items()
            .iter()
            .all(|item| !item.text.contains("usage: /abort")));
    }

    #[test]
    fn pair_routes_all_five_takeovers_and_never_reports_unknown() {
        let config = localpilot_config::Config::default();
        let cwd = Path::new(".");
        let submit = |prompt: &str| SubmittedInput {
            shown: prompt.to_string(),
            display: prompt.to_string(),
            prompt: prompt.to_string(),
            pastes: Vec::new(),
            images: Vec::new(),
        };
        let unknown = |app: &AppModel| {
            app.timeline_for(PeerPane::A)
                .map(|pane| {
                    pane.items()
                        .iter()
                        .any(|item| item.text.contains("unknown slash command"))
                })
                .unwrap_or(false)
        };

        // `/help` opens the help takeover.
        let mut app = pair_app();
        assert!(matches!(
            execute_pair_slash(&mut app, &config, cwd, submit("/help")),
            PairHostAction::None
        ));
        assert!(app.has_takeover());
        assert!(!unknown(&app));

        // `/theme dim` applies a valid theme directly; a bad name warns, no panic.
        let mut app = pair_app();
        let _ = execute_pair_slash(&mut app, &config, cwd, submit("/theme dim"));
        assert_eq!(app.theme, Theme::Dim);
        let mut app = pair_app();
        let _ = execute_pair_slash(&mut app, &config, cwd, submit("/theme nonesuch"));
        // The warning surfaces the parser's real reason, including the accepted
        // values — not a hand-rolled "unknown theme" string.
        assert!(app
            .timeline_for(PeerPane::A)
            .expect("A timeline")
            .items()
            .iter()
            .any(|item| item.text.contains("expected default")));
        assert!(!unknown(&app));

        // `/settings <query>` opens settings with the filter prefilled.
        let mut app = pair_app();
        let _ = execute_pair_slash(&mut app, &config, cwd, submit("/settings mouse"));
        assert!(app.has_takeover());
        assert!(!unknown(&app));

        // `/diff <path>` and `/search <query>` route without an unknown notice.
        let mut app = pair_app();
        let _ = execute_pair_slash(&mut app, &config, cwd, submit("/diff src"));
        assert!(!unknown(&app));
        let mut app = pair_app();
        let _ = execute_pair_slash(&mut app, &config, cwd, submit("/search foo"));
        assert!(!unknown(&app));
    }

    #[test]
    fn idle_fullscreen_takeovers_open_via_the_shared_seam() {
        // Drives the exact function the idle full-screen branch uses for the five
        // takeovers, without constructing a session runtime.
        let config = localpilot_config::Config::default();
        let cwd = Path::new(".");

        let mut help = app();
        open_fullscreen_takeover(&mut help, &config, cwd, SlashAction::Help);
        assert!(help.has_takeover());

        let mut picker = app();
        open_fullscreen_takeover(&mut picker, &config, cwd, SlashAction::Theme(None));
        assert!(picker.has_theme_picker());

        let mut good = app();
        open_fullscreen_takeover(
            &mut good,
            &config,
            cwd,
            SlashAction::Theme(Some("dim".to_string())),
        );
        assert_eq!(good.theme, Theme::Dim);
        assert!(!good.has_theme_picker());

        let mut bad = app();
        let before = bad.theme;
        open_fullscreen_takeover(
            &mut bad,
            &config,
            cwd,
            SlashAction::Theme(Some("nonesuch".to_string())),
        );
        assert_eq!(
            bad.theme, before,
            "an invalid theme leaves the theme unchanged"
        );
        assert!(
            !bad.has_theme_picker(),
            "an invalid theme never opens the picker"
        );
        assert!(
            bad.active_timeline()
                .items()
                .iter()
                .any(|item| item.text.contains("expected default")),
            "the warning surfaces the parser's accepted-values reason",
        );

        let mut settings = app();
        open_fullscreen_takeover(
            &mut settings,
            &config,
            cwd,
            SlashAction::Settings(Some("mouse".to_string())),
        );
        assert!(settings.has_takeover());

        let mut diff = app();
        open_fullscreen_takeover(&mut diff, &config, cwd, SlashAction::Diff(None));
        assert!(diff.has_takeover());

        let mut search = app();
        open_fullscreen_takeover(
            &mut search,
            &config,
            cwd,
            SlashAction::Search(Some("q".to_string())),
        );
        // Search opens the timeline-search input overlay, not a takeover/picker.
        assert!(search.has_input_overlay());
        assert!(!search.has_takeover());
        assert!(!search.has_theme_picker());
    }

    #[test]
    fn takeovers_dispatch_correctly_during_an_active_turn() {
        let history = localpilot_store::PromptHistory::with_store(None);
        let cwd = Path::new(".");
        // Submit a slash command while a turn is running and return the app state.
        let submit_during_work = |prompt: &str| {
            let mut app = app();
            app.begin_work();
            let cancel = CancellationToken::new();
            let mut queue = VecDeque::new();
            let _ = app.handle_input(InputAction::Insert(prompt.to_string()), 80);
            let exit = handle_turn_event(
                &mut app,
                Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
                &cancel,
                &event_hit_map(),
                &mut queue,
                &history,
                cwd,
                &image_capability(false),
            );
            assert!(!exit, "{prompt} must not exit the turn");
            app
        };
        let idle_notice = |app: &AppModel| {
            app.active_timeline()
                .items()
                .iter()
                .any(|item| item.text.contains("run when the current operation is idle"))
        };

        // help/theme/search stay reachable mid-operation.
        assert!(submit_during_work("/help").has_takeover());
        assert_eq!(submit_during_work("/theme dim").theme, Theme::Dim);
        let searched = submit_during_work("/search foo");
        assert!(searched.has_input_overlay());
        assert!(!searched.has_takeover());

        // an invalid theme mid-turn surfaces the real parser reason, no picker.
        let bad = submit_during_work("/theme nonesuch");
        assert!(!bad.has_theme_picker());
        assert!(bad
            .active_timeline()
            .items()
            .iter()
            .any(|item| item.text.contains("expected default")));

        // settings/diff are idle-only: they emit the idle notice, never a takeover.
        let settings = submit_during_work("/settings");
        assert!(!settings.has_takeover());
        assert!(idle_notice(&settings));
        let diff = submit_during_work("/diff");
        assert!(!diff.has_takeover());
        assert!(idle_notice(&diff));
    }

    #[test]
    fn diff_filter_is_trimmed_case_insensitive_substring_over_paths() {
        let file = |path: &str| DiffFile {
            status: "M".to_string(),
            path: path.to_string(),
            additions: 0,
            deletions: 0,
            lines: Vec::new(),
        };
        let files = vec![
            file("src/main.rs"),
            file("crates/localpilot-cli/SRC/lib.rs"),
            file("docs/readme.md"),
        ];
        // Empty / absent filter keeps all files.
        assert_eq!(filter_diff_files(files.clone(), None).len(), 3);
        assert_eq!(filter_diff_files(files.clone(), Some("   ")).len(), 3);
        // Case-insensitive, trimmed substring on the path.
        let src = filter_diff_files(files.clone(), Some("  SrC  "));
        assert_eq!(src.len(), 2);
        assert!(src
            .iter()
            .all(|f| f.path.to_ascii_lowercase().contains("src")));
        // No match yields an empty list (the caller opens the empty diff state).
        assert!(filter_diff_files(files, Some("no-such-path")).is_empty());
    }

    #[test]
    fn an_aborted_terminal_settles_steers_and_leaves_one_incomplete_card_per_pane() {
        let mut app = pair_app();
        let mut adapter = PairTerminalAdapter::new();
        // Both peers busy; the abort request marks both cancelling without faking a
        // terminal, then a pending steer is queued.
        assert!(app.begin_work_for(PeerPane::A));
        assert!(app.begin_work_for(PeerPane::B));
        assert!(app.request_pair_cancellation());
        let steer = app
            .append_prompt("late steer", None, true)
            .expect("steer row");
        adapter.queue_steer(PairPeer::A, steer);

        let _ = adapter.apply_pump_event(
            &mut app,
            PairPumpEvent::Finished {
                status: PairRunStatus {
                    state: PairRunState::Finished(PairTerminalStatus::Aborted),
                    completed_rounds: 0,
                    max_rounds: 1,
                    scheduled: None,
                    candidate: None,
                    agreements: [false, false],
                    repairing: None,
                },
                result: Box::new(PairResultSnapshot::for_reason(PairTerminalStatus::Aborted)),
            },
        );

        // The pending steer settled at the terminal.
        assert!(
            !app.timeline_for(PeerPane::A)
                .expect("A timeline")
                .item(steer)
                .expect("steer row")
                .pending
        );
        // Each pane ends with exactly one Incomplete Aborted result card.
        for pane in [PeerPane::A, PeerPane::B] {
            let cards: Vec<_> = app
                .timeline_for(pane)
                .expect("pane timeline")
                .items()
                .iter()
                .filter(|item| item.kind == ItemKind::Result)
                .collect();
            assert_eq!(cards.len(), 1, "exactly one result card per pane");
            assert_eq!(cards[0].tone, Some(ResultTone::Incomplete));
            assert!(cards[0].text.contains("Aborted before convergence."));
        }
    }

    #[test]
    fn building_the_result_presentation_never_touches_version_control() {
        // The retained result is inspect/copy-only. Rendering it into both panes of an
        // app rooted at a freshly initialized, clean git workspace must not create,
        // stage, or commit anything: the porcelain is empty before and after.
        let repo = tempfile::tempdir().expect("temporary git workspace");
        let root = repo.path().display().to_string();
        // Address the repo with `-C <path>` so the process working directory is never
        // changed — keeping the proof scoped and parallel-test safe.
        let git = |args: &[&str]| {
            let mut full = vec!["-C", root.as_str()];
            for arg in args {
                full.push(arg);
            }
            std::process::Command::new("git")
                .args(&full)
                .output()
                .expect("run git")
        };
        assert!(git(&["init", "--initial-branch=main"]).status.success());
        let porcelain = || {
            String::from_utf8(git(&["status", "--porcelain=v1", "--untracked-files=all"]).stdout)
                .expect("utf8 porcelain")
        };
        let before = porcelain();
        assert!(
            before.is_empty(),
            "precondition: the initialized repo starts clean, got {before:?}"
        );

        let mut app = pair_app_with_workspace(&root);
        let mut adapter = PairTerminalAdapter::new();
        let result = PairResultSnapshot {
            reason: PairTerminalStatus::Converged,
            detail: None,
            completed_rounds: 2,
            candidate: Some(PairResultCandidate {
                revision: 2,
                digest: "digest".to_string(),
                artifact: "artifact".to_string(),
            }),
            raw: [Some("A-RAW".to_string()), Some("B-RAW".to_string())],
        };
        let _ = adapter.apply_pump_event(
            &mut app,
            PairPumpEvent::Finished {
                status: PairRunStatus {
                    state: PairRunState::Finished(PairTerminalStatus::Converged),
                    completed_rounds: 2,
                    max_rounds: 3,
                    scheduled: None,
                    candidate: None,
                    agreements: [true, true],
                    repairing: None,
                },
                result: Box::new(result),
            },
        );
        // The result cards exist in memory, and version control is untouched.
        assert!(app
            .timeline_for(PeerPane::A)
            .expect("A timeline")
            .items()
            .iter()
            .any(|item| item.kind == ItemKind::Result));
        let after = porcelain();
        assert_eq!(before, after, "result presentation changed version control");
        assert!(
            after.is_empty(),
            "the workspace remains clean after the result: {after:?}"
        );
    }

    #[test]
    fn pair_terminal_adapter_settles_every_undelivered_peer_steer() {
        let mut app = pair_app();
        let mut adapter = PairTerminalAdapter::new();
        let a = app
            .append_prompt("alpha steer", None, true)
            .expect("A prompt");
        adapter.queue_steer(PairPeer::A, a);
        assert!(app.select_pair_pane(PeerPane::B));
        let b = app
            .append_prompt("beta steer", None, true)
            .expect("B prompt");
        adapter.queue_steer(PairPeer::B, b);
        assert!(app.select_pair_pane(PeerPane::A));

        let _ = adapter.apply_pump_event(
            &mut app,
            PairPumpEvent::Finished {
                status: PairRunStatus {
                    state: PairRunState::Finished(PairTerminalStatus::CapReached),
                    completed_rounds: 1,
                    max_rounds: 1,
                    scheduled: None,
                    candidate: None,
                    agreements: [false, false],
                    repairing: None,
                },
                result: Box::new(PairResultSnapshot::for_reason(
                    PairTerminalStatus::CapReached,
                )),
            },
        );

        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));
        for (peer, item) in [(PeerPane::A, a), (PeerPane::B, b)] {
            let timeline = app.timeline_for(peer).expect("peer timeline");
            assert!(!timeline.item(item).expect("settled prompt").pending);
            assert!(timeline
                .items()
                .iter()
                .any(|entry| entry.text.contains("1 steering message was not delivered")));
        }
    }

    #[test]
    fn pair_terminal_adapter_rejects_the_latest_steer_on_its_origin_peer() {
        let mut app = pair_app();
        let mut adapter = PairTerminalAdapter::new();
        assert!(app.select_pair_pane(PeerPane::B));
        let item = app
            .append_prompt("late beta steer", None, true)
            .expect("B prompt");
        adapter.queue_steer(PairPeer::B, item);
        assert!(app.select_pair_pane(PeerPane::A));

        adapter.reject_latest_steer(&mut app, PairPeer::B);

        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));
        let beta = app.timeline_for(PeerPane::B).expect("B timeline");
        assert!(!beta.item(item).expect("rejected prompt remains").pending);
        assert!(beta.items().iter().any(|entry| entry.text.contains(
            "steering was not delivered because the collaboration was no longer accepting input"
        )));
        assert!(!app
            .timeline_for(PeerPane::A)
            .expect("A timeline")
            .items()
            .iter()
            .any(|entry| entry.text.contains("steering was not delivered")));
    }

    #[test]
    fn pair_terminal_adapter_attributes_and_steps_one_exact_question_vector() {
        let mut app = pair_app();
        let mut adapter = PairTerminalAdapter::new();
        let id = PairAskId::fixture(7);
        let install = adapter.apply_pump_event(
            &mut app,
            PairPumpEvent::Ask(PairAsk {
                id,
                peer: PairPeer::B,
                request: PairAskRequest::Questions(vec![
                    pair_question("first"),
                    pair_question("second"),
                ]),
            }),
        );
        assert!(matches!(install, PairHostAction::None));
        assert_eq!(app.active_pair_pane(), Some(PeerPane::B));

        let first = resolve_pair_question_action(
            &mut app,
            QuestionAction::Submit(QuestionResponse::Selected(vec!["first".to_string()])),
            &mut adapter,
        );
        assert!(matches!(first, PairHostAction::None));
        let second = resolve_pair_question_action(
            &mut app,
            QuestionAction::Submit(QuestionResponse::Other("custom".to_string())),
            &mut adapter,
        );
        match second {
            PairHostAction::Answer {
                id: answer_id,
                answer: PairAskAnswer::Questions(answers),
                abort,
                exit,
            } => {
                assert_eq!(answer_id, id);
                assert_eq!(
                    answers,
                    vec![
                        UserAnswer::Selected(vec!["first".to_string()]),
                        UserAnswer::Other("custom".to_string()),
                    ]
                );
                assert!(!abort);
                assert!(!exit);
            }
            _ => panic!("expected one complete question answer"),
        }
        assert!(adapter.dialog.is_none());
    }

    #[test]
    fn pair_focus_targets_steering_and_image_submissions_are_not_sent() {
        let mut app = pair_app();
        let mut adapter = PairTerminalAdapter::new();
        let hit_map = draw_hit_map(&app, 100, 28);
        let mut mouse_state = MouseState::default();
        let config = localpilot_config::Config::default();
        let cwd = Path::new(".");
        let history = localpilot_store::PromptHistory::with_store(None);

        let action = handle_pair_terminal_event(
            &mut app,
            Event::Key(press(KeyCode::F(6), KeyModifiers::NONE)),
            &mut adapter,
            &hit_map,
            &mut mouse_state,
            &config,
            cwd,
            &history,
        );
        assert!(matches!(action, PairHostAction::None));
        assert_eq!(app.active_pair_pane(), Some(PeerPane::B));

        let _ = app.handle_input(InputAction::Insert("follow up".to_string()), 80);
        let action = handle_pair_terminal_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &mut adapter,
            &hit_map,
            &mut mouse_state,
            &config,
            cwd,
            &history,
        );
        assert!(matches!(
            action,
            PairHostAction::Steer {
                peer: PairPeer::B,
                text,
            } if text == "follow up"
        ));
        let b_item = app
            .timeline_for(PeerPane::B)
            .unwrap()
            .items()
            .iter()
            .find(|item| item.kind == ItemKind::User)
            .expect("queued B steer");
        assert!(b_item.pending);
        let b_item_id = b_item.id;

        let _ = adapter.apply_pump_event(
            &mut app,
            PairPumpEvent::Runtime {
                peer: PairPeer::A,
                event: RuntimeEvent::SoftInterruptInjected {
                    point: "between_calls".to_string(),
                    source: "system".to_string(),
                },
            },
        );
        assert!(
            app.timeline_for(PeerPane::B)
                .unwrap()
                .item(b_item_id)
                .expect("B item remains")
                .pending
        );
        assert!(app.select_pair_pane(PeerPane::A));
        let _ = adapter.apply_pump_event(
            &mut app,
            PairPumpEvent::Runtime {
                peer: PairPeer::B,
                event: RuntimeEvent::SoftInterruptInjected {
                    point: "after_tools".to_string(),
                    source: "user".to_string(),
                },
            },
        );
        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));
        assert!(
            !app.timeline_for(PeerPane::B)
                .unwrap()
                .item(b_item_id)
                .expect("B item activated")
                .pending
        );

        let user_rows = app
            .timeline_for(PeerPane::A)
            .unwrap()
            .items()
            .iter()
            .filter(|item| item.kind == ItemKind::User)
            .count();
        let _ = app
            .attach_image("image/png", "opaque", 6)
            .expect("attach image");
        let action = handle_pair_terminal_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &mut adapter,
            &hit_map,
            &mut mouse_state,
            &config,
            cwd,
            &history,
        );
        assert!(matches!(action, PairHostAction::None));
        assert_eq!(
            app.timeline_for(PeerPane::A)
                .unwrap()
                .items()
                .iter()
                .filter(|item| item.kind == ItemKind::User)
                .count(),
            user_rows
        );
        assert!(app
            .timeline_for(PeerPane::A)
            .unwrap()
            .items()
            .iter()
            .any(|item| item.text.contains("submission was not sent")));

        let action = handle_pair_terminal_event(
            &mut app,
            Event::Key(press(KeyCode::Char('v'), KeyModifiers::CONTROL)),
            &mut adapter,
            &hit_map,
            &mut mouse_state,
            &config,
            cwd,
            &history,
        );
        assert!(matches!(action, PairHostAction::None));
        assert!(app
            .timeline_for(PeerPane::A)
            .unwrap()
            .items()
            .iter()
            .any(|item| item.text.contains("Clipboard images are not available")));
    }

    fn fill_pair_timelines(app: &mut AppModel, rows: usize) {
        for number in 0..rows {
            let _ = app.timeline_for_mut(PeerPane::A).and_then(|timeline| {
                timeline.push(ItemKind::Assistant, format!("alpha response {number:03}"))
            });
            let _ = app.timeline_for_mut(PeerPane::B).and_then(|timeline| {
                timeline.push(ItemKind::Assistant, format!("beta response {number:03}"))
            });
        }
    }

    #[test]
    fn theme_host_preference_is_opt_in_and_invalid_values_keep_the_default() {
        let mut unset = app();
        apply_theme_preference(&mut unset, None);
        assert_eq!(unset.theme, Theme::Default);

        let mut terminal = app();
        apply_theme_preference(&mut terminal, Some(OsString::from("terminal")));
        assert_eq!(terminal.theme, Theme::Terminal);

        let mut invalid = app();
        apply_theme_preference(&mut invalid, Some(OsString::from("brand-theme")));
        assert_eq!(invalid.theme, Theme::Default);
        assert!(invalid.active_timeline().items().iter().any(|item| item
            .text
            .contains("unknown terminal chat theme \"brand-theme\"")));
    }

    #[test]
    fn settings_projection_names_observed_facts_and_only_real_session_edits() {
        let mut app = app();
        app.capture_setting_defaults();
        let settings = fullscreen_settings(&app, &localpilot_config::Config::default());
        let names = settings
            .iter()
            .map(|setting| setting.name.as_str())
            .collect::<Vec<_>>();

        for expected in [
            "Mouse reporting",
            "Copy on selection",
            "Screen reader",
            "Tabs",
            "Updates",
            "Prompt history",
            "Compact paste",
        ] {
            assert!(names.contains(&expected), "missing {expected}");
        }
        assert_eq!(
            settings
                .iter()
                .filter_map(|setting| setting.edit)
                .collect::<Vec<_>>(),
            [SettingEdit::CopyOnSelect, SettingEdit::Theme]
        );
    }

    fn image_capability(vision_capable: bool) -> ImageCapabilitySnapshot {
        ImageCapabilitySnapshot {
            provider_id: "fixture".to_string(),
            vision_capable,
        }
    }

    fn timeline_has_notice(app: &AppModel, needle: &str) -> bool {
        app.active_timeline()
            .items()
            .iter()
            .any(|item| item.text.contains(needle))
    }

    #[test]
    fn attach_image_path_resolves_capability_before_reading_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let png = dir.path().join("photo.png");
        std::fs::write(
            &png,
            [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3],
        )
        .expect("write png");
        let missing = dir.path().join("gone.png");

        // (a) capable + a real PNG file -> attaches, notice names the file.
        let mut capable = app();
        attach_image_path_with_capability(&mut capable, &image_capability(true), &png);
        assert!(timeline_has_notice(&capable, "attached image "));

        // (b) not capable + a missing path -> the two-lever capability notice, and
        // no file read happens (the path never existed, yet there is no read error).
        let mut text_only = app();
        attach_image_path_with_capability(&mut text_only, &image_capability(false), &missing);
        assert!(timeline_has_notice(&text_only, "supports_vision"));
        assert!(timeline_has_notice(&text_only, "vision_probe"));
        assert!(!timeline_has_notice(
            &text_only,
            "couldn't read the image file"
        ));

        // (c) capable + a missing path -> the read-error notice.
        let mut unreadable = app();
        attach_image_path_with_capability(&mut unreadable, &image_capability(true), &missing);
        assert!(timeline_has_notice(
            &unreadable,
            "couldn't read the image file"
        ));
    }

    #[test]
    fn attach_prepared_image_notifies_when_the_composer_declines() {
        let mut app = app();
        // A help takeover owns input, so attach_image returns None.
        app.open_help();
        attach_prepared_image(
            &mut app,
            "image/png",
            "DATA".to_string(),
            4,
            "attached 1×1 image".to_string(),
        );
        assert!(timeline_has_notice(&app, "couldn't attach the image."));
        assert!(!timeline_has_notice(&app, "attached 1×1 image"));
    }

    #[test]
    fn ctrl_c_maps_to_contextual_interrupt_handling() {
        assert_eq!(
            map_key(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(InputAction::CancelOrExit)
        );
    }

    #[test]
    fn enter_submits_and_escape_maps_to_work_interrupt() {
        assert_eq!(
            map_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Some(InputAction::Submit)
        );
        assert_eq!(
            map_key(press(KeyCode::Esc, KeyModifiers::NONE)),
            Some(InputAction::Escape)
        );
        assert_eq!(
            map_key(press(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(InputAction::Insert("\n".to_string()))
        );
        assert_eq!(
            map_key(press(KeyCode::F(6), KeyModifiers::NONE)),
            Some(InputAction::CyclePeer)
        );
        assert_eq!(map_key(press(KeyCode::F(6), KeyModifiers::SHIFT)), None);
    }

    #[test]
    fn wheel_and_page_navigation_hold_idle_and_busy_timelines() {
        let mut app = app();
        for number in 0..100 {
            let _ = app
                .active_timeline_mut()
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        let hit_map = draw_hit_map(&app, 80, 24);
        let timeline = single_hits(&hit_map);
        let mut mouse_state = MouseState::default();
        app.exit_armed = true;
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::ScrollUp,
                    timeline.timeline.x,
                    timeline.timeline.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(matches!(
            app.active_timeline().viewport,
            ViewportAnchor::Held(_) | ViewportAnchor::Top
        ));
        assert!(!app.exit_armed);

        app.active_timeline_mut().follow_bottom();
        app.begin_work();
        let busy_hit_map = draw_hit_map(&app, 80, 24);
        let busy_timeline = single_hits(&busy_hit_map);
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::ScrollUp,
                    busy_timeline.timeline.x,
                    busy_timeline.timeline.y,
                )),
                &busy_hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(matches!(
            app.active_timeline().viewport,
            ViewportAnchor::Held(_) | ViewportAnchor::Top
        ));

        app.active_timeline_mut().follow_bottom();
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Key(press(KeyCode::PageUp, KeyModifiers::NONE)),
                &busy_hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(matches!(
            app.active_timeline().viewport,
            ViewportAnchor::Held(_) | ViewportAnchor::Top
        ));
        let held = app.active_timeline().viewport;
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Key(press(KeyCode::Home, KeyModifiers::CONTROL)),
                &busy_hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Unhandled
        );
        assert_eq!(app.active_timeline().viewport, held);
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Key(press(KeyCode::End, KeyModifiers::CONTROL)),
                &busy_hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Unhandled
        );
        assert_eq!(app.active_timeline().viewport, held);
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Key(press(KeyCode::Home, KeyModifiers::NONE)),
                &busy_hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Unhandled
        );
    }

    #[test]
    fn single_pointer_routing_keeps_outside_wheel_and_click_behavior() {
        let mut app = app();
        for number in 0..40 {
            let _ = app
                .active_timeline_mut()
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        let first = app.active_timeline().items()[0].id;
        app.active_timeline_mut().start_selection(ContentPoint {
            item_id: first,
            byte: 0,
        });
        app.active_timeline_mut().extend_selection(ContentPoint {
            item_id: first,
            byte: 4,
        });
        let hit_map = draw_hit_map(&app, 80, 24);
        let mut mouse_state = MouseState::default();
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::ScrollUp,
                    hit_map.composer.x,
                    hit_map.composer.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_ne!(app.active_timeline().viewport, ViewportAnchor::FollowBottom);

        let footer = hit_map.frame.expect("frame").footer;
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    footer.x,
                    footer.y
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(app.active_timeline().selection.is_none());
    }

    #[test]
    fn pair_pointer_focus_wheel_copy_and_miss_routing_are_peer_exact() {
        let mut app = pair_app();
        fill_pair_timelines(&mut app, 50);
        let b_item = app.timeline_for(PeerPane::B).expect("B").items()[0].id;
        let b = app.timeline_for_mut(PeerPane::B).expect("B");
        b.start_selection(ContentPoint {
            item_id: b_item,
            byte: 0,
        });
        b.extend_selection(ContentPoint {
            item_id: b_item,
            byte: 4,
        });

        let hit_map = draw_hit_map(&app, 100, 24);
        let timelines = hit_map.timelines.as_ref().expect("pair timelines");
        let b_hits = timelines.for_peer(PeerPane::B).expect("B hits");
        let b_label = b_hits.label.expect("B label");
        let divider = timelines.divider().expect("divider");
        let mut mouse_state = MouseState::default();

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    b_label.x,
                    b_label.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(app.active_pair_pane(), Some(PeerPane::B));
        assert_eq!(
            app.timeline_for(PeerPane::B)
                .and_then(Timeline::selected_text)
                .as_deref(),
            Some("beta")
        );

        assert!(app.select_pair_pane(PeerPane::A));
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::ScrollUp,
                    b_hits.timeline.x,
                    b_hits.timeline.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));
        assert_eq!(
            app.timeline_for(PeerPane::A).expect("A").viewport,
            ViewportAnchor::FollowBottom
        );
        assert_ne!(
            app.timeline_for(PeerPane::B).expect("B").viewport,
            ViewportAnchor::FollowBottom
        );
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Right),
                    b_hits.timeline.x,
                    b_hits.timeline.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Copy("beta".to_string())
        );
        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));

        let a_before = app.timeline_for(PeerPane::A).expect("A").viewport;
        let b_before = app.timeline_for(PeerPane::B).expect("B").viewport;
        for (column, row) in [
            (divider.x, divider.y),
            (
                hit_map.frame.expect("frame").footer.x,
                hit_map.frame.expect("frame").footer.y,
            ),
        ] {
            assert_eq!(
                route_pointer_or_navigation(
                    &mut app,
                    &Event::Mouse(mouse(MouseEventKind::Down(MouseButton::Left), column, row,)),
                    &hit_map,
                    &mut mouse_state,
                ),
                RoutedEvent::Handled
            );
        }
        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));
        assert_eq!(app.timeline_for(PeerPane::A).expect("A").viewport, a_before);
        assert_eq!(app.timeline_for(PeerPane::B).expect("B").viewport, b_before);
        assert!(app
            .timeline_for(PeerPane::A)
            .expect("A")
            .selection
            .is_none());
        assert!(app
            .timeline_for(PeerPane::B)
            .expect("B")
            .selection
            .is_some());

        let hit = b_hits.rows.first().expect("B row");
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    hit.content_x,
                    hit.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(app.active_pair_pane(), Some(PeerPane::B));
        assert_eq!(
            mouse_state.selection.expect("selection").peer,
            Some(PeerPane::B)
        );
    }

    #[test]
    fn pair_selection_and_scrollbar_drags_remain_on_their_origin_peer() {
        let mut app = pair_app();
        fill_pair_timelines(&mut app, 120);
        app.set_copy_on_select(true);
        let hit_map = draw_hit_map(&app, 100, 24);
        let timelines = hit_map.timelines.as_ref().expect("pair timelines");
        let a_hits = timelines.for_peer(PeerPane::A).expect("A hits");
        let b_hits = timelines.for_peer(PeerPane::B).expect("B hits");
        let start = b_hits.rows.first().expect("B start row");
        let end = b_hits.rows.get(1).unwrap_or(start);
        let mut mouse_state = MouseState::default();

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    start.content_x.saturating_add(3),
                    start.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(
            mouse_state.selection.expect("gesture").peer,
            Some(PeerPane::B)
        );
        assert!(app.select_pair_pane(PeerPane::A));
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Drag(MouseButton::Left),
                    a_hits.timeline.x,
                    end.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        let expected_copy = app
            .timeline_for(PeerPane::B)
            .and_then(Timeline::selected_text)
            .expect("B selection after drag");
        let copied = route_pointer_or_navigation(
            &mut app,
            &Event::Mouse(mouse(
                MouseEventKind::Up(MouseButton::Left),
                a_hits.timeline.x,
                end.y,
            )),
            &hit_map,
            &mut mouse_state,
        );
        assert_eq!(copied, RoutedEvent::Copy(expected_copy));
        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));
        assert!(app
            .timeline_for(PeerPane::A)
            .expect("A")
            .selection
            .is_none());
        assert!(app
            .timeline_for(PeerPane::B)
            .expect("B")
            .selection
            .is_some());

        app.timeline_for_mut(PeerPane::A)
            .expect("A")
            .follow_bottom();
        app.timeline_for_mut(PeerPane::B)
            .expect("B")
            .follow_bottom();
        let hit_map = draw_hit_map(&app, 100, 24);
        let timelines = hit_map.timelines.as_ref().expect("pair timelines");
        let a_hits = timelines.for_peer(PeerPane::A).expect("A hits");
        let b_hits = timelines.for_peer(PeerPane::B).expect("B hits");
        let thumb = b_hits.scrollbar.thumb.expect("B thumb");
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    thumb.x,
                    thumb.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(
            mouse_state.scrollbar.expect("scrollbar").target,
            ScrollbarTarget::Timeline(Some(PeerPane::B))
        );
        assert!(app.select_pair_pane(PeerPane::A));
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Drag(MouseButton::Left),
                    a_hits.scrollbar.track.x,
                    b_hits.scrollbar.track.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(
            app.timeline_for(PeerPane::A).expect("A").viewport,
            ViewportAnchor::FollowBottom
        );
        assert_eq!(
            app.timeline_for(PeerPane::B).expect("B").viewport,
            ViewportAnchor::Top
        );
        let _ = route_pointer_or_navigation(
            &mut app,
            &Event::Mouse(mouse(
                MouseEventKind::Up(MouseButton::Left),
                a_hits.scrollbar.track.x,
                b_hits.scrollbar.track.y,
            )),
            &hit_map,
            &mut mouse_state,
        );

        app.timeline_for_mut(PeerPane::B)
            .expect("B")
            .follow_bottom();
        let hit_map = draw_hit_map(&app, 100, 24);
        let b_hits = hit_map
            .timelines
            .as_ref()
            .and_then(|timelines| timelines.for_peer(PeerPane::B))
            .expect("B hits");
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    b_hits.scrollbar.track.x,
                    b_hits.scrollbar.track.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(app.active_pair_pane(), Some(PeerPane::B));
        assert_ne!(
            app.timeline_for(PeerPane::B).expect("B").viewport,
            ViewportAnchor::FollowBottom
        );
    }

    #[test]
    fn narrow_pair_wheel_scrolls_only_the_visible_peer() {
        let mut app = pair_app();
        fill_pair_timelines(&mut app, 50);
        let hit_map = draw_hit_map(&app, 60, 24);
        let visible = hit_map
            .timelines
            .as_ref()
            .and_then(|timelines| timelines.active(Some(PeerPane::A)))
            .expect("visible A");
        let mut mouse_state = MouseState::default();
        let _ = route_pointer_or_navigation(
            &mut app,
            &Event::Mouse(mouse(
                MouseEventKind::ScrollUp,
                visible.timeline.x,
                visible.timeline.y,
            )),
            &hit_map,
            &mut mouse_state,
        );
        let a_viewport = app.timeline_for(PeerPane::A).expect("A").viewport;
        assert_ne!(a_viewport, ViewportAnchor::FollowBottom);
        assert_eq!(
            app.timeline_for(PeerPane::B).expect("B").viewport,
            ViewportAnchor::FollowBottom
        );

        assert_eq!(
            app.handle_input(InputAction::CyclePeer, hit_map.editor_width),
            AppCommand::None
        );
        let hit_map = draw_hit_map(&app, 60, 24);
        let visible = hit_map
            .timelines
            .as_ref()
            .and_then(|timelines| timelines.active(Some(PeerPane::B)))
            .expect("visible B");
        let _ = route_pointer_or_navigation(
            &mut app,
            &Event::Mouse(mouse(
                MouseEventKind::ScrollUp,
                visible.timeline.x,
                visible.timeline.y,
            )),
            &hit_map,
            &mut mouse_state,
        );
        assert_eq!(
            app.timeline_for(PeerPane::A).expect("A").viewport,
            a_viewport
        );
        assert_ne!(
            app.timeline_for(PeerPane::B).expect("B").viewport,
            ViewportAnchor::FollowBottom
        );
    }

    #[test]
    fn composer_navigation_and_shell_editing_shortcuts_map_semantically() {
        let cases = [
            (
                KeyCode::Home,
                KeyModifiers::NONE,
                InputAction::MoveVisualStart,
            ),
            (KeyCode::End, KeyModifiers::NONE, InputAction::MoveVisualEnd),
            (
                KeyCode::Home,
                KeyModifiers::CONTROL,
                InputAction::MoveTextStart,
            ),
            (
                KeyCode::End,
                KeyModifiers::CONTROL,
                InputAction::MoveTextEnd,
            ),
            (KeyCode::Left, KeyModifiers::ALT, InputAction::MoveWordLeft),
            (
                KeyCode::Right,
                KeyModifiers::ALT,
                InputAction::MoveWordRight,
            ),
            (
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
                InputAction::MoveLineStart,
            ),
            (
                KeyCode::Char('b'),
                KeyModifiers::CONTROL,
                InputAction::MoveLeft,
            ),
            (
                KeyCode::Char('e'),
                KeyModifiers::CONTROL,
                InputAction::MoveLineEnd,
            ),
            (
                KeyCode::Char('f'),
                KeyModifiers::CONTROL,
                InputAction::ForwardCharOrSearch,
            ),
            (
                KeyCode::Char('g'),
                KeyModifiers::CONTROL,
                InputAction::OpenExternalEditor,
            ),
            (
                KeyCode::Char('h'),
                KeyModifiers::CONTROL,
                InputAction::Backspace,
            ),
            (
                KeyCode::Char('k'),
                KeyModifiers::CONTROL,
                InputAction::DeleteToLineEnd,
            ),
            (
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
                InputAction::DeleteToLineStart,
            ),
            (
                KeyCode::Char('w'),
                KeyModifiers::CONTROL,
                InputAction::DeleteWordLeft,
            ),
            (
                KeyCode::Char('j'),
                KeyModifiers::CONTROL,
                InputAction::Insert("\n".to_string()),
            ),
            (
                KeyCode::Char('r'),
                KeyModifiers::CONTROL,
                InputAction::OpenReverseHistory,
            ),
            (
                KeyCode::Char('s'),
                KeyModifiers::CONTROL,
                InputAction::StashOrPop,
            ),
            (
                KeyCode::Char('y'),
                KeyModifiers::CONTROL,
                InputAction::AcceptCompletion,
            ),
            (
                KeyCode::Tab,
                KeyModifiers::NONE,
                InputAction::AcceptCompletion,
            ),
        ];
        for (code, modifiers, expected) in cases {
            assert_eq!(map_key(press(code, modifiers)), Some(expected));
        }
    }

    #[test]
    fn fullscreen_catalog_matches_the_shared_spec_table() {
        // The full-screen picker is generated from the shared table: 19 rows in
        // global order (14 dispatched + 5 takeovers), byte-for-byte, and never a
        // deferred inline-only row.
        let full_screen: Vec<(String, String)> = fullscreen_command_catalog()
            .into_iter()
            .map(|command| (command.name, command.description))
            .collect();
        let expected_full_screen: &[(&str, &str)] = &[
            (
                "model",
                "Switch provider/model, or list them (/model [provider [model]])",
            ),
            (
                "localbox",
                "Adopt a running LocalBox server into your config (/localbox adopt)",
            ),
            ("new", "Start a fresh session"),
            ("fork", "Branch the conversation into a new session"),
            ("clone", "Copy the conversation into a new session"),
            ("sessions", "List this workspace's sessions"),
            ("session", "Resume a session by id"),
            ("name", "Name this session (/name <text>)"),
            ("rename", "Rename this session (/rename <text>)"),
            ("continue", "Continue the previous session"),
            ("clear", "Clear the conversation view"),
            ("resume", "Continue a previous session"),
            ("exit", "Exit LocalPilot (/exit [print])"),
            ("quit", "Exit LocalPilot"),
            ("search", "Search messages in this session"),
            ("help", "Open keyboard and command help"),
            ("theme", "Preview terminal color modes"),
            ("settings", "Inspect terminal chat settings"),
            ("diff", "Review tracked workspace changes"),
        ];
        assert_eq!(full_screen.len(), 19);
        for (got, want) in full_screen.iter().zip(expected_full_screen.iter()) {
            assert_eq!((got.0.as_str(), got.1.as_str()), *want);
        }
        for deferred in [
            "agent", "harness", "compact", "research", "skills", "bg", "tree",
        ] {
            assert!(!full_screen.iter().any(|(name, _)| name == deferred));
        }

        // The pair picker: 8 rows with pair-specific copy for search/settings and
        // the permanent pair-only `abort`.
        let pair: Vec<(String, String)> = pair_command_catalog()
            .into_iter()
            .map(|command| (command.name, command.description))
            .collect();
        let expected_pair: &[(&str, &str)] = &[
            ("exit", "Exit LocalPilot (/exit [print])"),
            ("quit", "Exit LocalPilot"),
            ("search", "Search messages for the selected peer"),
            ("help", "Open keyboard and command help"),
            ("theme", "Preview terminal color modes"),
            ("settings", "Inspect terminal settings"),
            ("diff", "Review tracked workspace changes"),
            ("abort", "Stop the collaboration and both peers"),
        ];
        assert_eq!(pair.len(), 8);
        for (got, want) in pair.iter().zip(expected_pair.iter()) {
            assert_eq!((got.0.as_str(), got.1.as_str()), *want);
        }
    }

    #[test]
    fn session_projection_is_recent_first_bounded_and_keeps_the_current_row() {
        let current = localpilot_core::SessionId::new();
        let mut indexed = (1..=MAX_SESSION_CHOOSER_ROWS)
            .map(|updated_unix| SessionIndexEntry {
                id: localpilot_core::SessionId::new(),
                message_count: updated_unix,
                created_unix: updated_unix as u64,
                updated_unix: updated_unix as u64,
                name: None,
            })
            .collect::<Vec<_>>();
        let newest = indexed.last().expect("newest fixture").id.to_string();
        indexed.push(SessionIndexEntry {
            id: current,
            message_count: 4,
            created_unix: 0,
            updated_unix: 0,
            name: Some("PLANTED_SESSION_NAME".to_string()),
        });

        let projected = fullscreen_session_entries(indexed, current);
        assert_eq!(projected.len(), MAX_SESSION_CHOOSER_ROWS);
        assert_eq!(projected[0].selector, newest);
        assert!(projected.last().expect("current row").current);
        assert_eq!(projected.iter().filter(|entry| entry.current).count(), 1);
        assert!(!format!("{projected:?}").contains("PLANTED_SESSION_NAME"));

        let absent_current = localpilot_core::SessionId::new();
        let projected = fullscreen_session_entries(Vec::new(), absent_current);
        assert_eq!(projected.len(), 1);
        assert!(projected[0].current);
        assert_eq!(projected[0].updated, None);
    }

    #[test]
    fn session_dates_and_bare_continue_selection_are_deterministic() {
        assert_eq!(
            format_session_updated_at(0, time::UtcOffset::UTC).as_deref(),
            Some("1970-01-01 00:00")
        );
        assert!(format_session_updated_at(u64::MAX, time::UtcOffset::UTC).is_none());

        let current = localpilot_core::SessionId::new();
        let older = localpilot_core::SessionId::new();
        let newest_other = localpilot_core::SessionId::new();
        let entry = |id, updated_unix| SessionIndexEntry {
            id,
            message_count: 0,
            created_unix: updated_unix,
            updated_unix,
            name: None,
        };
        assert_eq!(
            latest_other_session(
                vec![entry(older, 1), entry(current, 9), entry(newest_other, 5),],
                current,
            ),
            Some(newest_other)
        );
        assert_eq!(latest_other_session(vec![entry(current, 9)], current), None);
    }

    #[test]
    fn session_row_click_focuses_without_activating() {
        let mut app = app();
        let selected = localpilot_core::SessionId::new().to_string();
        app.open_sessions([
            SessionEntry {
                selector: localpilot_core::SessionId::new().to_string(),
                name: Some("Current".to_string()),
                message_count: 2,
                updated: None,
                current: true,
            },
            SessionEntry {
                selector: selected.clone(),
                name: Some("Previous".to_string()),
                message_count: 8,
                updated: None,
                current: false,
            },
        ]);
        let hit_map = draw_hit_map(&app, 80, 20);
        let second = hit_map.takeover_rows[1];
        let mut mouse_state = MouseState::default();

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    second.area.x,
                    second.area.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(app.has_takeover());
        let AppCommand::ActivateSession(selection) =
            app.handle_input(InputAction::Submit, hit_map.editor_width)
        else {
            panic!("Enter should activate the focused row");
        };
        assert_eq!(selection.as_str(), selected);
    }

    #[test]
    fn resume_failure_preserves_view_and_success_replaces_the_projection() {
        let mut app = app();
        app.editor.insert("session-local stash");
        let _ = app.handle_input(InputAction::StashOrPop, 80);
        let original = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "existing conversation")
            .expect("timeline item");
        let original_session = app.active_session_id().to_string();
        let target = localpilot_core::SessionId::new();

        apply_fullscreen_resume(
            &mut app,
            target,
            Some("unused name".to_string()),
            Err("resume failed: planted failure".to_string()),
        );
        assert_eq!(app.active_session_id(), original_session);
        assert!(app.has_stashed_draft());
        assert!(app.active_timeline().item(original).is_some());
        assert!(app
            .active_timeline()
            .items()
            .iter()
            .any(|item| item.text.contains("planted failure")));

        apply_fullscreen_resume(
            &mut app,
            target,
            Some("new\u{1b}[2J name\nsecond".to_string()),
            Ok(vec![
                StartupItem::User("restored prompt".to_string()),
                StartupItem::Assistant("restored answer".to_string()),
                StartupItem::Usage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cached_input_tokens: 0,
                },
                StartupItem::ContextUsage {
                    used: 20,
                    limit: 100,
                },
            ]),
        );
        assert_eq!(app.active_session_id(), target.to_string());
        assert!(!app.has_stashed_draft());
        assert_eq!(app.active_session_name(), Some("new name second"));
        assert!(!app
            .active_timeline()
            .items()
            .iter()
            .any(|item| item.text == "existing conversation"));
        assert_eq!(
            app.active_usage(),
            Some(UsageTotals {
                input_tokens: 11,
                output_tokens: 7,
                cached_input_tokens: 0,
            })
        );
        assert!(app
            .active_timeline()
            .items()
            .iter()
            .any(|item| item.text == "restored prompt"));
        assert!(app
            .active_timeline()
            .items()
            .iter()
            .any(|item| item.text == "restored answer"));
    }

    #[test]
    fn ctrl_q_is_reserved_for_active_prompt_enqueue() {
        assert!(is_enqueue_key(press(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_enqueue_key(press(
            KeyCode::Char('q'),
            KeyModifiers::NONE
        )));
        assert_eq!(
            map_key(press(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            None,
            "Ctrl+Q is inert outside the active-turn host path"
        );
    }

    #[test]
    fn exit_print_uses_only_sanitized_visible_timeline_content() {
        let mut app = app();
        let _ = app.append_prompt("hello\x1b[2Jworld", None, false);
        app.apply_runtime(RuntimeUpdate::Reasoning(
            "HIDDEN_REASONING_SECRET".to_string(),
        ));
        app.apply_runtime(RuntimeUpdate::Text("visible answer".to_string()));
        app.apply_runtime(RuntimeUpdate::Stopped(StopState::Done));
        app.apply_runtime(RuntimeUpdate::ToolStarted {
            id: "tool-1".to_string(),
            name: "read_file".to_string(),
            detail: "COLLAPSED_DETAIL_SECRET".to_string(),
        });
        app.apply_runtime(RuntimeUpdate::ToolFinished {
            id: "tool-1".to_string(),
            name: "read_file".to_string(),
            is_error: false,
            cancelled: false,
            output: "COLLAPSED_OUTPUT_SECRET".to_string(),
            duration_ms: 20,
        });
        app.apply_runtime(RuntimeUpdate::Usage {
            input_tokens: 1_234,
            output_tokens: 56,
            cached_input_tokens: 0,
        });

        let directory = tempfile::tempdir().expect("tempdir");
        let output = exit_presentation(&app, directory.path(), Duration::from_secs(62), true);
        assert!(output.contains("You\nhelloworld"));
        assert!(output.contains("LocalPilot\nvisible answer"));
        assert!(output.contains("Tokens: 1,234 input · 56 output"));
        assert!(output.contains("Resume: localpilot chat --resume fixture-session"));
        assert!(!output.contains("HIDDEN_REASONING_SECRET"));
        assert!(!output.contains("COLLAPSED_DETAIL_SECRET"));
        assert!(!output.contains("COLLAPSED_OUTPUT_SECRET"));
        assert!(!output.contains('\x1b'));

        let summary = exit_presentation(&app, directory.path(), Duration::ZERO, false);
        assert!(!summary.contains("hello"));
        assert!(!summary.contains("visible answer"));
    }

    #[test]
    fn restored_exit_witness_is_created_after_restore() {
        let mut order = Vec::new();
        let exit = restore_exit_with(
            ExitDraft {
                trust_denied: false,
                presentation: Some("summary".to_string()),
            },
            || order.push("restore"),
        );
        if exit.presentation.is_some() {
            order.push("write summary");
        }
        assert_eq!(order, ["restore", "write summary"]);
    }

    #[test]
    fn external_editor_resolution_honors_precedence_quotes_and_redacts_arguments() {
        let command = resolve_editor_command_with(|name| match name {
            CHAT_EDITOR_ENV => Some(OsString::from(
                "\"C:\\Program Files\\Editor\\editor.exe\" --wait --token SECRET_ARG",
            )),
            "VISUAL" => Some(OsString::from("ignored-visual")),
            "EDITOR" => Some(OsString::from("ignored-editor")),
            _ => None,
        })
        .expect("editor command");
        assert_eq!(
            command.program,
            OsString::from("C:\\Program Files\\Editor\\editor.exe")
        );
        assert_eq!(
            command.args,
            [
                OsString::from("--wait"),
                OsString::from("--token"),
                OsString::from("SECRET_ARG")
            ]
        );
        assert!(!format!("{command:?}").contains("SECRET_ARG"));
        assert!(split_editor_command("\"unterminated").is_err());
    }

    #[test]
    fn external_editor_readback_rejects_invalid_utf8_and_oversize_files() {
        let directory = tempfile::tempdir().expect("editor fixture");
        let path = directory.path().join("LOCALPILOT_PROMPT.md");
        std::fs::write(&path, b"edited draft").expect("valid fixture");
        assert_eq!(
            read_external_edit(&path).expect("valid UTF-8 fixture"),
            "edited draft"
        );

        std::fs::write(&path, [0xff, 0xfe]).expect("invalid fixture");
        assert!(read_external_edit(&path)
            .expect_err("invalid UTF-8")
            .to_string()
            .contains("not valid UTF-8"));

        let file = std::fs::File::create(&path).expect("oversize fixture");
        file.set_len(MAX_EXTERNAL_EDITOR_BYTES + 1)
            .expect("sparse oversize fixture");
        assert!(read_external_edit(&path)
            .expect_err("oversize file")
            .to_string()
            .contains("8 MiB"));
    }

    #[derive(Default)]
    struct FakeModes {
        events: Vec<&'static str>,
    }

    impl SuspensibleModes for FakeModes {
        type Capabilities = &'static str;

        fn leave(&mut self) {
            self.events.push("leave");
        }

        fn reenter(&mut self) -> Result<Self::Capabilities> {
            self.events.push("reenter");
            Ok("capabilities")
        }
    }

    #[tokio::test]
    async fn suspended_operation_reenters_after_success_and_operation_error() {
        let mut success_modes = FakeModes::default();
        let (value, capabilities) = with_modes_suspended(&mut success_modes, async { 42 })
            .await
            .expect("successful round trip");
        assert_eq!(value, 42);
        assert_eq!(capabilities, "capabilities");
        assert_eq!(success_modes.events, ["leave", "reenter"]);

        let mut failed_operation_modes = FakeModes::default();
        let (operation, _) = with_modes_suspended(&mut failed_operation_modes, async {
            Err::<(), _>("injected spawn failure")
        })
        .await
        .expect("terminal re-entry still succeeds");
        assert_eq!(operation, Err("injected spawn failure"));
        assert_eq!(failed_operation_modes.events, ["leave", "reenter"]);
    }

    #[test]
    fn suspended_operation_leaves_a_plain_terminal_during_panic_unwind() {
        let mut modes = FakeModes::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _suspension = ModeSuspension::new(&mut modes);
            panic!("injected editor panic");
        }));
        assert!(result.is_err());
        assert_eq!(modes.events, ["leave"]);
    }

    #[test]
    fn mouse_drag_selects_graphemes_and_copy_on_release_persists() {
        let mut app = app();
        app.set_copy_on_select(true);
        let _ = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "alpha 界 beta");
        let hit_map = draw_hit_map(&app, 80, 24);
        let hit = single_hits(&hit_map)
            .rows
            .iter()
            .find(|hit| hit.row.kind == ItemKind::Assistant)
            .expect("assistant row hit");
        let start_column = hit.content_x;
        let end_column = hit.content_x + 6;
        let row = hit.y;
        let mut mouse_state = MouseState::default();

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    start_column,
                    row
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Drag(MouseButton::Left),
                    end_column,
                    row
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(
            app.active_timeline().selected_text().as_deref(),
            Some("alpha 界")
        );
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Up(MouseButton::Left),
                    end_column,
                    row
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Copy("alpha 界".to_string())
        );
        assert_eq!(
            app.active_timeline().selected_text().as_deref(),
            Some("alpha 界")
        );
        assert!(mouse_state.selection.is_none());

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    start_column,
                    row
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Up(MouseButton::Left),
                    start_column,
                    row
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(app.active_timeline().selected_text().is_none());
    }

    #[test]
    fn default_selection_waits_for_explicit_right_click_copy() {
        let mut app = app();
        let _ = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "copy explicitly");
        assert!(!app.copy_on_select());
        let hit_map = draw_hit_map(&app, 80, 24);
        let hit = single_hits(&hit_map)
            .rows
            .iter()
            .find(|hit| hit.row.kind == ItemKind::Assistant)
            .expect("assistant row hit");
        let start = hit.content_x;
        let end = hit.content_x.saturating_add(3);
        let mut mouse_state = MouseState::default();
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
        ] {
            let column = if matches!(kind, MouseEventKind::Down(_)) {
                start
            } else {
                end
            };
            assert_eq!(
                route_pointer_or_navigation(
                    &mut app,
                    &Event::Mouse(mouse(kind, column, hit.y)),
                    &hit_map,
                    &mut mouse_state,
                ),
                RoutedEvent::Handled
            );
        }
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(MouseEventKind::Up(MouseButton::Left), end, hit.y)),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(
            app.active_timeline().selected_text().as_deref(),
            Some("copy")
        );
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(MouseEventKind::Down(MouseButton::Right), end, hit.y,)),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Copy("copy".to_string())
        );
    }

    #[test]
    fn right_click_routes_timeline_copy_composer_paste_and_empty_timeline_inert() {
        let mut app = app();
        let _ = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "copy explicitly");
        let hit_map = draw_hit_map(&app, 80, 24);
        let hit = single_hits(&hit_map)
            .rows
            .iter()
            .find(|hit| hit.row.kind == ItemKind::Assistant)
            .expect("assistant row hit");
        let mut mouse_state = MouseState::default();
        app.active_timeline_mut()
            .start_selection(hit.point_for_column(hit.content_x, false));
        app.active_timeline_mut()
            .extend_selection(hit.point_for_column(hit.content_x + 4, false));

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Right),
                    hit.content_x,
                    hit.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Copy("copy".to_string())
        );
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Right),
                    hit_map.composer.x,
                    hit_map.composer.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::PasteClipboard
        );

        app.active_timeline_mut().clear_selection();
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Right),
                    hit.content_x,
                    hit.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
    }

    #[test]
    fn right_click_clipboard_text_uses_the_atomic_idle_and_busy_paste_path() {
        let text = (1..=12)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        for busy in [false, true] {
            let mut app = app();
            if busy {
                app.begin_work();
            }
            apply_clipboard_text(&mut app, 80, text.clone());
            assert_eq!(app.editor.text(), "[Paste #1 - 12 lines]");
            assert!(app.active_timeline().items().is_empty());

            apply_clipboard_text(&mut app, 80, String::new());
            assert_eq!(app.editor.text(), "[Paste #1 - 12 lines]");
        }
    }

    #[test]
    fn lost_mouse_release_self_heals_on_focus_loss_or_unpressed_motion() {
        let mut app = app();
        let _ = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "select me");
        let hit_map = draw_hit_map(&app, 80, 24);
        let hit = single_hits(&hit_map)
            .rows
            .iter()
            .find(|hit| hit.row.kind == ItemKind::Assistant)
            .expect("assistant row hit");
        let mut mouse_state = MouseState::default();

        for recovery in [
            Event::FocusLost,
            Event::Mouse(mouse(MouseEventKind::Moved, 0, 0)),
        ] {
            let _ = route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    hit.content_x,
                    hit.y,
                )),
                &hit_map,
                &mut mouse_state,
            );
            assert!(mouse_state.selection_pointer.is_some());
            assert_eq!(
                route_pointer_or_navigation(&mut app, &recovery, &hit_map, &mut mouse_state),
                RoutedEvent::Handled
            );
            assert!(mouse_state.selection.is_none());
            assert!(mouse_state.selection_pointer.is_none());
        }
    }

    #[test]
    fn quick_help_wheel_scrolls_the_timeline_without_consuming_the_first_step() {
        let mut app = app();
        for number in 0..80 {
            let _ = app
                .active_timeline_mut()
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        let _ = app.handle_input(InputAction::Insert("?".to_string()), 76);
        let hit_map = draw_hit_map(&app, 80, 24);
        let mut mouse_state = MouseState::default();

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(MouseEventKind::ScrollUp, 10, 8)),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(app.dismiss_quick_help());
        assert!(matches!(
            app.active_timeline().viewport,
            localpilot_terminal_ui::ViewportAnchor::Held(_)
        ));
    }

    #[test]
    fn held_edge_selection_continues_to_autoscroll_without_new_mouse_events() {
        let mut app = app();
        for number in 0..120 {
            let _ = app
                .active_timeline_mut()
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        let hit_map = draw_hit_map(&app, 80, 24);
        let origin = single_hits(&hit_map)
            .rows
            .last()
            .expect("visible timeline row");
        let mut mouse_state = MouseState::default();
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    origin.content_x,
                    origin.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Drag(MouseButton::Left),
                    origin.content_x,
                    single_hits(&hit_map).timeline.y.saturating_sub(1),
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );

        let after_drag = draw_hit_map(&app, 80, 24);
        let start_after_drag = single_hits(&after_drag).scrollbar.start;
        advance_mouse_selection(&mut app, &after_drag, &mouse_state);
        let after_stationary_tick = draw_hit_map(&app, 80, 24);

        assert!(single_hits(&after_stationary_tick).scrollbar.start < start_after_drag);
        assert!(app.active_timeline().selected_text().is_some());
    }

    #[test]
    fn activity_prefix_click_toggles_details_without_starting_selection() {
        let mut app = app();
        app.apply_runtime(RuntimeUpdate::ToolStarted {
            id: "tool-1".to_string(),
            name: "inspect".to_string(),
            detail: String::new(),
        });
        app.apply_runtime(RuntimeUpdate::ToolFinished {
            id: "tool-1".to_string(),
            name: "inspect".to_string(),
            is_error: false,
            cancelled: false,
            output: "detail one\ndetail two".to_string(),
            duration_ms: 25,
        });
        let tool = app
            .active_timeline()
            .items()
            .iter()
            .find(|item| item.kind == ItemKind::Tool)
            .expect("tool item")
            .id;
        assert!(
            !app.active_timeline()
                .item(tool)
                .expect("tool item")
                .expanded
        );
        let hit_map = draw_hit_map(&app, 80, 24);
        let hit = single_hits(&hit_map)
            .rows
            .iter()
            .find(|hit| hit.row.item_id == tool)
            .expect("tool row hit");
        let mut mouse_state = MouseState::default();

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    single_hits(&hit_map).timeline.x,
                    hit.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(
            app.active_timeline()
                .item(tool)
                .expect("tool item")
                .expanded
        );
        assert!(app.active_timeline().selection.is_none());
    }

    #[test]
    fn scrollbar_thumb_drag_and_track_click_reanchor_timeline() {
        let mut app = app();
        for number in 0..120 {
            let _ = app
                .active_timeline_mut()
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        let hit_map = draw_hit_map(&app, 80, 24);
        let thumb = single_hits(&hit_map)
            .scrollbar
            .thumb
            .expect("scrollbar thumb");
        let mut mouse_state = MouseState::default();

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    thumb.x,
                    thumb.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(
            mouse_state.scrollbar,
            Some(ScrollbarGesture {
                target: ScrollbarTarget::Timeline(None),
                grab: 0,
            })
        );
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Drag(MouseButton::Left),
                    thumb.x,
                    single_hits(&hit_map).scrollbar.track.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(app.active_timeline().viewport, ViewportAnchor::Top);
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Up(MouseButton::Left),
                    thumb.x,
                    single_hits(&hit_map).scrollbar.track.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(mouse_state.scrollbar, None);

        app.active_timeline_mut().follow_bottom();
        let hit_map = draw_hit_map(&app, 80, 24);
        let thumb = single_hits(&hit_map).scrollbar.thumb.expect("bottom thumb");
        let click_y = thumb.y.saturating_sub(1);
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    thumb.x,
                    click_y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(matches!(
            app.active_timeline().viewport,
            ViewportAnchor::Held(_) | ViewportAnchor::Top
        ));
    }

    #[test]
    fn help_takeover_contains_mouse_input_and_scrolls_its_own_view() {
        let mut app = app();
        app.set_command_catalog(fullscreen_command_catalog());
        app.open_help();
        let mut hit_map = draw_hit_map(&app, 80, 20);
        assert!(hit_map.takeover);
        assert_eq!(hit_map.takeover_scrollbar.start, 0);
        let mut mouse_state = MouseState::default();

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(MouseEventKind::ScrollDown, 20, 8)),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        hit_map = draw_hit_map(&app, 80, 20);
        assert!(hit_map.takeover_scrollbar.start > 0);

        let thumb = hit_map
            .takeover_scrollbar
            .thumb
            .expect("help scrollbar thumb");
        let track_bottom = hit_map.takeover_scrollbar.track.bottom().saturating_sub(1);
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    thumb.x,
                    thumb.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Drag(MouseButton::Left),
                    thumb.x,
                    track_bottom,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        hit_map = draw_hit_map(&app, 80, 20);
        assert_eq!(
            hit_map.takeover_scrollbar.start,
            hit_map
                .takeover_scrollbar
                .total_rows
                .saturating_sub(hit_map.takeover_scrollbar.viewport_rows)
        );
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(MouseEventKind::Down(MouseButton::Right), 20, 8)),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
    }

    #[test]
    fn theme_picker_mouse_focus_previews_without_touching_the_timeline() {
        let mut app = app();
        let timeline_item = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "underlying conversation")
            .expect("timeline item");
        app.open_theme_picker();
        let hit_map = draw_hit_map(&app, 80, 24);
        assert_eq!(hit_map.theme_rows.len(), Theme::ALL.len());
        let dim_index = Theme::ALL
            .iter()
            .position(|theme| *theme == Theme::Dim)
            .expect("dim theme");
        let dim = hit_map.theme_rows[dim_index];
        let mut mouse_state = MouseState::default();

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    dim.area.x,
                    dim.area.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(app.theme, Theme::Dim);
        assert!(app.active_timeline().item(timeline_item).is_some());
        assert!(app.has_theme_picker());
        let _ = app.handle_input(InputAction::Escape, hit_map.editor_width);
        assert_eq!(app.theme, Theme::Default);
        assert!(!app.has_theme_picker());
    }

    fn feed_unbracketed_multiline_paste(app: &mut AppModel) {
        let mut burst = PasteBurst::default();
        let keys = [
            KeyCode::Char('a'),
            KeyCode::Enter,
            KeyCode::Char('b'),
            KeyCode::Enter,
            KeyCode::Char('c'),
            KeyCode::Enter,
            KeyCode::Char('d'),
        ];
        for (index, code) in keys.into_iter().enumerate() {
            let consumed = handle_fullscreen_paste_burst(
                app,
                &mut burst,
                press(code, KeyModifiers::NONE),
                index + 1 < keys.len(),
                76,
            );
            assert!(consumed, "paste key {index} escaped into submit routing");
        }
        assert!(!burst.has_pending());
    }

    #[test]
    fn fullscreen_multiline_key_burst_is_one_paste_and_never_submits_lines() {
        let mut app = app();
        feed_unbracketed_multiline_paste(&mut app);

        assert!(app.active_timeline().items().is_empty());
        let AppCommand::Submit(submitted) = app.handle_input(InputAction::Submit, 76) else {
            panic!("whole paste should submit once");
        };
        assert_eq!(submitted.prompt, "a\nb\nc\nd");
        assert_eq!(submitted.pastes.len(), 1);
    }

    #[test]
    fn dialog_paste_bursts_are_inert_except_in_question_other_editor() {
        let mut approval = app();
        approval.request_approval("write_file", "fixture", "write");
        let mut burst = PasteBurst::default();
        assert!(handle_dialog_paste_burst(
            &mut approval,
            &mut burst,
            press(KeyCode::Char('y'), KeyModifiers::NONE),
            true,
            false,
        ));
        assert!(handle_dialog_paste_burst(
            &mut approval,
            &mut burst,
            press(KeyCode::Enter, KeyModifiers::NONE),
            false,
            false,
        ));
        assert!(
            approval.dialog.is_some(),
            "pasted y/newline resolved approval"
        );

        let mut trust = app();
        trust.require_workspace_trust("fixture");
        let mut burst = PasteBurst::default();
        assert!(handle_dialog_paste_burst(
            &mut trust,
            &mut burst,
            press(KeyCode::Char('1'), KeyModifiers::NONE),
            true,
            false,
        ));
        assert!(handle_dialog_paste_burst(
            &mut trust,
            &mut burst,
            press(KeyCode::Enter, KeyModifiers::NONE),
            false,
            false,
        ));
        assert!(
            trust.workspace_trust_pending(),
            "pasted choice resolved trust"
        );

        let mut question = app();
        question.request_question(
            None,
            "Explain",
            std::iter::empty::<UiQuestionOption>(),
            false,
            1,
            1,
        );
        assert_eq!(
            question.handle_question_input(InputAction::Submit),
            QuestionAction::None
        );
        let mut burst = PasteBurst::default();
        for (index, code) in [KeyCode::Char('x'), KeyCode::Enter, KeyCode::Char('y')]
            .into_iter()
            .enumerate()
        {
            assert!(handle_dialog_paste_burst(
                &mut question,
                &mut burst,
                press(code, KeyModifiers::NONE),
                index < 2,
                true,
            ));
        }
        assert_eq!(
            question.handle_question_input(InputAction::Submit),
            QuestionAction::Submit(QuestionResponse::Other("x y".to_string()))
        );
    }

    #[test]
    fn ctrl_q_enqueues_one_atomic_paste_without_cancelling_active_work() {
        let mut app = app();
        app.begin_work();
        feed_unbracketed_multiline_paste(&mut app);
        let cancel = CancellationToken::new();
        let history = localpilot_store::PromptHistory::with_store(None);
        let mut queue = VecDeque::new();

        assert!(!handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            Path::new("fixture"),
            &image_capability(false),
        ));

        assert_eq!(queue.len(), 1);
        assert_eq!(queue.front().expect("queued").prompt().text, "a\nb\nc\nd");
        assert!(!cancel.is_cancelled());
        assert!(!app.exit_requested);
    }

    #[test]
    fn active_turn_queues_typeahead_and_escape_promotes_fifo_steering() {
        let mut app = app();
        app.begin_work();
        app.editor.insert("next prompt");
        let cancel = CancellationToken::new();
        let history = localpilot_store::PromptHistory::with_store(None);
        let cwd = Path::new("fixture");
        let mut queue = VecDeque::new();
        let steer = SteerQueue::default();
        let mut pending_steer_items = VecDeque::new();

        assert!(!handle_turn_event_with_steering(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            cwd,
            &image_capability(false),
            &steer,
            &mut pending_steer_items,
        ));
        assert!(app.editor.text().is_empty());
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.front().expect("queued").prompt().text, "next prompt");
        assert!(
            app.active_timeline()
                .item(queue.front().expect("queued").item_id())
                .expect("queued item")
                .pending
        );
        assert!(!cancel.is_cancelled());

        app.editor.insert("third prompt");
        assert!(!handle_turn_event_with_steering(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            cwd,
            &image_capability(false),
            &steer,
            &mut pending_steer_items,
        ));
        assert_eq!(
            queue
                .iter()
                .map(|queued| queued.prompt().text.as_str())
                .collect::<Vec<_>>(),
            vec!["next prompt", "third prompt"]
        );

        let queued_ids = queue
            .iter()
            .map(QueuedOperation::item_id)
            .collect::<Vec<_>>();
        assert!(!handle_turn_event_with_steering(
            &mut app,
            Event::Key(press(KeyCode::Esc, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            cwd,
            &image_capability(false),
            &steer,
            &mut pending_steer_items,
        ));
        assert!(!cancel.is_cancelled());
        assert!(queue.is_empty());
        assert!(!steer.is_empty());
        assert_eq!(
            pending_steer_items.iter().copied().collect::<Vec<_>>(),
            queued_ids
        );
        assert_eq!(
            app.active_work(),
            WorkState::Busy {
                cancellation_requested: false
            }
        );

        for item_id in queued_ids {
            apply_runtime_event(
                &mut app,
                RuntimeEvent::SoftInterruptInjected {
                    point: "during_stream".to_string(),
                    source: "user".to_string(),
                },
                &mut pending_steer_items,
            );
            assert!(
                !app.active_timeline()
                    .item(item_id)
                    .expect("steered row")
                    .pending
            );
        }
        assert!(pending_steer_items.is_empty());
    }

    #[test]
    fn active_turn_queues_shell_then_prompt_in_one_serial_order() {
        let temp = tempfile::tempdir().expect("temp history");
        let history = localpilot_store::PromptHistory::with_store(Some(
            temp.path().join("prompt-history.jsonl"),
        ));
        let mut app = app();
        app.begin_work();
        let cancel = CancellationToken::new();
        let mut queue = VecDeque::new();

        let _ = app.handle_input(
            InputAction::Insert("!echo SHELL_QUEUE_SECRET".to_string()),
            80,
        );
        assert!(!handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
        ));
        app.editor.insert("ordinary queued prompt");
        assert!(!handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
        ));

        assert_eq!(queue.len(), 2);
        let shell = queue.front().expect("queued shell").shell();
        let prompt = queue.back().expect("queued prompt").prompt();
        assert_eq!(shell.command.as_str(), "echo SHELL_QUEUE_SECRET");
        assert_eq!(prompt.text, "ordinary queued prompt");
        assert!(!format!("{queue:?}").contains("SHELL_QUEUE_SECRET"));
        let ordered_ids = app
            .active_timeline()
            .items()
            .iter()
            .filter(|item| item.id == shell.item_id || item.id == prompt.item_id)
            .map(|item| item.id)
            .collect::<Vec<_>>();
        assert_eq!(ordered_ids, vec![shell.item_id, prompt.item_id]);
        assert_eq!(
            app.active_timeline()
                .item(shell.item_id)
                .expect("shell row")
                .kind,
            ItemKind::Shell
        );
        assert!(
            app.active_timeline()
                .item(prompt.item_id)
                .expect("prompt row")
                .pending
        );
        let stored = history.load();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].text, "ordinary queued prompt");
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn escape_does_not_reorder_a_shell_before_a_queued_prompt() {
        let temp = tempfile::tempdir().expect("temp history");
        let history = localpilot_store::PromptHistory::with_store(None);
        let mut app = app();
        app.begin_work();
        let cancel = CancellationToken::new();
        let mut queue = VecDeque::new();
        let steer = SteerQueue::default();
        let mut pending_steer_items = VecDeque::new();

        let _ = app.handle_input(InputAction::Insert("!echo first".to_string()), 80);
        assert!(!handle_turn_event_with_steering(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
            &steer,
            &mut pending_steer_items,
        ));
        app.editor.insert("second prompt");
        assert!(!handle_turn_event_with_steering(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
            &steer,
            &mut pending_steer_items,
        ));

        assert!(!handle_turn_event_with_steering(
            &mut app,
            Event::Key(press(KeyCode::Esc, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
            &steer,
            &mut pending_steer_items,
        ));

        assert!(cancel.is_cancelled());
        assert_eq!(queue.len(), 2);
        assert!(matches!(queue.front(), Some(QueuedOperation::Shell(_))));
        assert!(matches!(queue.back(), Some(QueuedOperation::Prompt(_))));
        assert!(steer.is_empty());
        assert!(pending_steer_items.is_empty());
    }

    #[test]
    fn escape_promotes_only_the_prompt_prefix_before_a_shell() {
        let temp = tempfile::tempdir().expect("temp history");
        let history = localpilot_store::PromptHistory::with_store(None);
        let mut app = app();
        app.begin_work();
        let cancel = CancellationToken::new();
        let mut queue = VecDeque::new();
        let steer = SteerQueue::default();
        let mut pending_steer_items = VecDeque::new();

        app.editor.insert("first prompt");
        assert!(!handle_turn_event_with_steering(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
            &steer,
            &mut pending_steer_items,
        ));
        let prompt_id = queue.front().expect("queued prompt").item_id();
        let _ = app.handle_input(InputAction::Insert("!echo second".to_string()), 80);
        assert!(!handle_turn_event_with_steering(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
            &steer,
            &mut pending_steer_items,
        ));

        assert!(!handle_turn_event_with_steering(
            &mut app,
            Event::Key(press(KeyCode::Esc, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
            &steer,
            &mut pending_steer_items,
        ));

        assert!(!cancel.is_cancelled());
        assert_eq!(queue.len(), 1);
        assert!(matches!(queue.front(), Some(QueuedOperation::Shell(_))));
        assert!(!steer.is_empty());
        assert_eq!(pending_steer_items, VecDeque::from([prompt_id]));
    }

    #[test]
    fn active_exit_cancels_work_and_discards_queued_operations() {
        let history = localpilot_store::PromptHistory::with_store(None);
        let mut app = app();
        app.begin_work();
        let cancel = CancellationToken::new();
        let mut queue = VecDeque::new();

        app.editor.insert("queued prompt that must not run");
        assert!(!handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            Path::new("fixture"),
            &image_capability(false),
        ));
        assert_eq!(queue.len(), 1);

        app.editor.insert("/exit print");
        let exit = handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            Path::new("fixture"),
            &image_capability(false),
        );
        assert!(exit);
        assert!(cancel.is_cancelled());
        assert!(app.print_transcript_on_exit());

        if exit {
            discard_queued_operations(&mut queue);
        }
        assert!(queue.is_empty());
    }

    #[test]
    fn resume_startup_projection_restores_messages_usage_and_context() {
        let mut app = app();
        for item in [
            StartupItem::User("prior prompt".to_string()),
            StartupItem::Assistant("prior answer".to_string()),
            StartupItem::Usage {
                input_tokens: 100,
                output_tokens: 25,
                cached_input_tokens: 0,
            },
            StartupItem::ContextUsage {
                used: 300,
                limit: 4_000,
            },
            StartupItem::Notice("resume ready".to_string()),
        ] {
            apply_startup_item(&mut app, item);
        }

        assert_eq!(app.active_timeline().items()[0].kind, ItemKind::User);
        assert_eq!(app.active_timeline().items()[0].text, "prior prompt");
        assert_eq!(app.active_timeline().items()[1].kind, ItemKind::Assistant);
        assert_eq!(app.active_timeline().items()[1].text, "prior answer");
        assert_eq!(
            app.active_usage(),
            Some(UsageTotals {
                input_tokens: 100,
                output_tokens: 25,
                cached_input_tokens: 0,
            })
        );
        assert_eq!(app.active_context_usage(), Some((300, 4_000)));
        assert_eq!(
            app.active_timeline().items().last().expect("notice").kind,
            ItemKind::Notice
        );
    }

    #[test]
    fn active_turn_never_queues_a_slash_command_as_provider_input() {
        let temp = tempfile::tempdir().expect("temp history");
        let history = localpilot_store::PromptHistory::with_store(Some(
            temp.path().join("prompt-history.jsonl"),
        ));
        let mut app = app();
        app.begin_work();
        let cancel = CancellationToken::new();
        let mut queue = VecDeque::new();

        let _ = app.handle_input(InputAction::Insert("/clear".to_string()), 80);
        assert!(!handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
        ));

        assert!(queue.is_empty());
        assert!(!cancel.is_cancelled());
        assert!(history.load().is_empty());
        assert!(app.active_timeline().items().iter().any(|item| {
            item.kind == ItemKind::Notice
                && item.text.contains("when the current operation is idle")
        }));
    }

    #[test]
    fn configured_providers_become_truthful_model_picker_values() {
        let mut config = localpilot_config::Config::default();
        config.providers.insert(
            "local".to_string(),
            localpilot_config::ProviderConfig {
                kind: "openai_compatible".to_string(),
                model: Some("fixture-model".to_string()),
                ..Default::default()
            },
        );
        config.providers.insert(
            "remote".to_string(),
            localpilot_config::ProviderConfig {
                kind: "anthropic".to_string(),
                ..Default::default()
            },
        );

        let values = fullscreen_model_values(&config, "local");
        assert_eq!(
            values
                .iter()
                .map(|value| value.name.as_str())
                .collect::<Vec<_>>(),
            vec!["local", "remote"]
        );
        assert!(values[0].description.contains("current"));
        assert!(values[0].description.contains("fixture-model"));
        assert!(!values[1].description.contains("current"));
        assert!(values[1].description.contains("provider default"));
    }

    #[test]
    fn shell_diagnostic_strips_only_the_registry_envelope() {
        assert_eq!(
            present_shell_diagnostic(
                "tool: run_shell\nstatus: error\noutput:\npermission denied for run_shell"
            ),
            "permission denied for run_shell"
        );
        assert_eq!(
            present_shell_diagnostic("cancelled by user"),
            "cancelled by user"
        );
    }

    #[test]
    fn buffered_approvals_are_all_denied_at_a_driver_boundary() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut replies = Vec::new();
        for number in 0..3 {
            let (reply, answer) = oneshot::channel();
            sender
                .send(ApprovalCall {
                    request: localpilot_tui::ApprovalRequest {
                        tool: format!("tool-{number}"),
                        target: "fixture".to_string(),
                        risk_class: "test".to_string(),
                    },
                    reply,
                })
                .expect("queue approval");
            replies.push(answer);
        }
        deny_buffered_approvals(&mut receiver);
        assert!(replies
            .iter_mut()
            .all(|answer| answer.try_recv() == Ok(false)));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn active_turn_paste_stays_compact_until_submit_then_queues_and_persists_raw() {
        let payload = (1..=12)
            .map(|line| format!("line {line} 界"))
            .collect::<Vec<_>>()
            .join("\n");
        let temp = tempfile::tempdir().expect("temp history");
        let history = localpilot_store::PromptHistory::with_store(Some(
            temp.path().join("prompt-history.jsonl"),
        ));
        let mut app = app();
        app.begin_work();
        let cancel = CancellationToken::new();
        let mut queue = VecDeque::new();

        assert!(!handle_turn_event(
            &mut app,
            Event::Paste(payload.clone()),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
        ));
        assert_eq!(app.editor.text(), "[Paste #1 - 12 lines]");
        assert!(queue.is_empty());

        assert!(!handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
        ));
        assert_eq!(queue.front().expect("queued paste").prompt().text, payload);
        let entry = history.load().pop().expect("stored paste");
        assert_eq!(entry.text, "[Paste #1 - 12 lines]");
        assert_eq!(entry.pastes.len(), 1);
        assert_eq!(expand_history_entry(&entry), payload);
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn queued_image_prompts_keep_isolated_blocks_and_persist_no_image_bytes() {
        let temp = tempfile::tempdir().expect("temp history");
        let history = localpilot_store::PromptHistory::with_store(Some(
            temp.path().join("prompt-history.jsonl"),
        ));
        let mut app = app();
        app.begin_work();
        let cancel = CancellationToken::new();
        let mut queue = VecDeque::new();

        for (number, secret) in [(1, "IMAGE_SECRET_ONE"), (2, "IMAGE_SECRET_TWO")] {
            app.editor.insert(&format!("inspect {number} "));
            let placeholder = app
                .attach_image("image/png", secret, number * 1024)
                .expect("attach fixture image");
            assert!(!handle_turn_event(
                &mut app,
                Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
                &cancel,
                &event_hit_map(),
                &mut queue,
                &history,
                temp.path(),
                &image_capability(true),
            ));
            let queued = queue.back().expect("queued image prompt");
            let queued = queued.prompt();
            assert_eq!(queued.text, format!("inspect {number}"));
            assert_eq!(queued.attachments.len(), 1);
            let ContentBlock::Image { data, .. } = &queued.attachments[0] else {
                panic!("image content block");
            };
            assert_eq!(data, secret);
            let timeline_prompt = app
                .active_timeline()
                .item(queued.item_id)
                .expect("timeline prompt");
            assert!(timeline_prompt.text.contains(&placeholder));
        }

        assert_eq!(queue.len(), 2);
        assert!(!format!("{queue:?}").contains("IMAGE_SECRET"));
        assert_eq!(
            app.active_timeline()
                .items()
                .iter()
                .filter(|item| item.text == "sending 1 image(s) with this prompt")
                .count(),
            2
        );
        let stored = std::fs::read_to_string(temp.path().join("prompt-history.jsonl"))
            .expect("stored history");
        assert!(!stored.contains("IMAGE_SECRET_ONE"));
        assert!(!stored.contains("IMAGE_SECRET_TWO"));
        let entries = history.load();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.pastes.is_empty()));
        assert!(entries.iter().all(|entry| entry.text.contains("[image #")));
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn workspace_file_index_refreshes_an_open_mention_without_blocking_input() {
        let (sender, receiver) = std_mpsc::channel();
        let mut index = WorkspaceFileIndex {
            receiver,
            finished: false,
        };
        let mut app = app();
        let _ = app.handle_input(InputAction::Insert("@sam".to_string()), 80);
        assert!(app.has_input_overlay());

        sender
            .send(vec!["src/sample.rs".to_string()])
            .expect("workspace result");
        index.refresh(&mut app);
        assert!(index.finished);
        assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
        assert_eq!(app.editor.text(), "@src/sample.rs ");
    }

    #[test]
    fn first_frame_is_drawn_before_the_fullscreen_workspace_scan_starts() {
        let source = include_str!("fullscreen.rs");
        let first_frame = source
            .find("let _ = draw_synchronized(&mut terminal, &app)?;")
            .expect("first frame");
        let index_start = source
            .find("WorkspaceFileIndex::start(context.cwd.to_path_buf())")
            .expect("async workspace index start");
        assert!(first_frame < index_start);
    }

    #[test]
    fn active_turn_snapshots_image_capability_before_runtime_borrow() {
        let source = include_str!("fullscreen.rs");
        let snapshot = source
            .find("let image_capability = ImageCapabilitySnapshot")
            .expect("capability snapshot");
        let turn = source
            .find("let operation = runtime.run_turn_with_attachments")
            .expect("attachment turn");
        assert!(snapshot < turn);

        let mut app = app();
        attach_clipboard_image_with_capability(&mut app, &image_capability(false));
        assert!(app.active_timeline().items().iter().any(|item| item
            .text
            .contains("current model is not known to accept images")));
    }

    #[test]
    fn approval_denial_resolves_reply_and_clears_dialog() {
        let mut app = app();
        app.begin_work();
        app.request_approval("write_file", "fixture", "write");
        let (reply, mut answer) = oneshot::channel();
        let mut pending = Some(reply);
        let cancel = CancellationToken::new();

        assert!(!handle_approval_event(
            &mut app,
            Event::Key(press(KeyCode::Char('n'), KeyModifiers::NONE)),
            &mut pending,
            &cancel,
        ));
        assert_eq!(answer.try_recv(), Ok(false));
        assert!(app.dialog.is_none());
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn screen_reader_approval_has_no_enter_default_and_exposes_a_real_deny_key() {
        let mut app = app();
        app.capabilities.screen_reader = true;
        app.begin_work();
        app.request_approval("write_file", "fixture", "write");
        let (reply, mut answer) = oneshot::channel();
        let mut pending = Some(reply);
        let cancel = CancellationToken::new();

        assert!(!handle_approval_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &mut pending,
            &cancel,
        ));
        assert!(matches!(
            answer.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(app.dialog.is_some());

        assert!(!handle_approval_event(
            &mut app,
            Event::Key(press(KeyCode::Char('n'), KeyModifiers::NONE)),
            &mut pending,
            &cancel,
        ));
        assert_eq!(answer.try_recv(), Ok(false));
        assert!(app.dialog.is_none());
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn selected_text_keeps_first_ctrl_c_copy_precedence_during_approval() {
        let mut app = app();
        let item = app
            .active_timeline_mut()
            .push(localpilot_terminal_ui::ItemKind::Assistant, "copy me")
            .expect("timeline item");
        app.active_timeline_mut().start_selection(ContentPoint {
            item_id: item,
            byte: 0,
        });
        app.active_timeline_mut().extend_selection(ContentPoint {
            item_id: item,
            byte: 4,
        });
        app.begin_work();
        app.request_approval("write_file", "fixture", "write");
        let (reply, mut answer) = oneshot::channel();
        let mut pending = Some(reply);
        let cancel = CancellationToken::new();

        assert!(!handle_approval_event(
            &mut app,
            Event::Key(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            &mut pending,
            &cancel,
        ));
        assert!(matches!(
            answer.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(app.dialog.is_some());
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn question_mouse_focus_and_enter_resolve_reply_without_cancelling_work() {
        let mut app = app();
        app.begin_work();
        let questions = vec![UserQuestion {
            header: None,
            question: "Pick one".to_string(),
            options: vec![
                localpilot_tools::QuestionOption {
                    label: "Red".to_string(),
                    description: None,
                },
                localpilot_tools::QuestionOption {
                    label: "Blue".to_string(),
                    description: None,
                },
            ],
            multi_select: false,
        }];
        let (reply, mut answer) = oneshot::channel();
        let mut pending = Some(PendingQuestions {
            questions,
            index: 0,
            answers: Vec::new(),
            reply,
        });
        pending
            .as_ref()
            .expect("pending questions")
            .show_current(&mut app);
        let hit_map = draw_hit_map(&app, 120, 30);
        let cancel = CancellationToken::new();

        let blue = hit_map.question_rows[1].area;
        assert!(!handle_question_event(
            &mut app,
            Event::Mouse(mouse(
                MouseEventKind::Down(MouseButton::Left),
                blue.x,
                blue.y,
            )),
            &mut pending,
            &cancel,
            &hit_map,
        ));
        assert!(!handle_question_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &mut pending,
            &cancel,
            &hit_map,
        ));
        assert_eq!(
            answer.try_recv(),
            Ok(vec![UserAnswer::Selected(vec!["Blue".to_string()])])
        );
        assert!(app.dialog.is_none());
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn buffered_questions_are_cancelled_at_a_driver_boundary() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut answers = Vec::new();
        for _ in 0..3 {
            let (reply, answer) = oneshot::channel();
            sender
                .send(QuestionCall {
                    questions: vec![UserQuestion {
                        header: None,
                        question: "fixture".to_string(),
                        options: vec![
                            localpilot_tools::QuestionOption {
                                label: "A".to_string(),
                                description: None,
                            },
                            localpilot_tools::QuestionOption {
                                label: "B".to_string(),
                                description: None,
                            },
                        ],
                        multi_select: false,
                    }],
                    reply,
                })
                .expect("queue question");
            answers.push(answer);
        }

        dismiss_buffered_questions(&mut receiver);
        for mut answer in answers {
            assert_eq!(answer.try_recv(), Ok(vec![UserAnswer::Dismissed]));
        }
    }

    #[test]
    fn question_sets_advance_in_order_and_dismiss_only_the_unanswered_tail() {
        let questions = vec![
            UserQuestion {
                header: Some("First".to_string()),
                question: "Choose first".to_string(),
                options: vec![
                    localpilot_tools::QuestionOption {
                        label: "A".to_string(),
                        description: None,
                    },
                    localpilot_tools::QuestionOption {
                        label: "B".to_string(),
                        description: None,
                    },
                ],
                multi_select: false,
            },
            UserQuestion {
                header: Some("Second".to_string()),
                question: "Choose second".to_string(),
                options: vec![
                    localpilot_tools::QuestionOption {
                        label: "C".to_string(),
                        description: None,
                    },
                    localpilot_tools::QuestionOption {
                        label: "D".to_string(),
                        description: None,
                    },
                ],
                multi_select: false,
            },
        ];
        let (reply, mut answers) = oneshot::channel();
        let mut pending = Some(PendingQuestions {
            questions,
            index: 0,
            answers: Vec::new(),
            reply,
        });
        let mut app = app();
        pending
            .as_ref()
            .expect("pending questions")
            .show_current(&mut app);

        assert!(!resolve_question_action(
            &mut app,
            QuestionAction::Submit(QuestionResponse::Selected(vec!["B".to_string()])),
            &mut pending,
        ));
        assert!(matches!(
            answers.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(app.dialog.is_some(), "the second question remains visible");

        assert!(!resolve_question_action(
            &mut app,
            QuestionAction::Cancel,
            &mut pending,
        ));
        assert_eq!(
            answers.try_recv(),
            Ok(vec![
                UserAnswer::Selected(vec!["B".to_string()]),
                UserAnswer::Dismissed,
            ])
        );
        assert!(app.dialog.is_none());
    }

    #[test]
    fn trust_dialog_preserves_double_ctrl_c_exit_contract() {
        let mut app = app();
        app.require_workspace_trust("fixture");
        let hit_map = draw_hit_map(&app, 80, 24);
        let ctrl_c = || Event::Key(press(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert_eq!(
            handle_trust_event(&mut app, ctrl_c(), &hit_map),
            TrustEventOutcome::Pending
        );
        assert!(app.workspace_trust_pending());
        assert_eq!(
            handle_trust_event(
                &mut app,
                Event::Key(press(KeyCode::Char('x'), KeyModifiers::NONE)),
                &hit_map,
            ),
            TrustEventOutcome::Pending
        );
        assert_eq!(
            handle_trust_event(&mut app, ctrl_c(), &hit_map),
            TrustEventOutcome::Pending
        );
        assert_eq!(
            handle_trust_event(&mut app, ctrl_c(), &hit_map),
            TrustEventOutcome::Exit
        );
    }

    #[test]
    fn trust_dialog_routes_session_remember_deny_and_mouse_focus_without_io() {
        let mut app = app();
        app.require_workspace_trust("fixture");
        let hit_map = draw_hit_map(&app, 120, 30);

        assert_eq!(
            handle_trust_event(
                &mut app,
                Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
                &hit_map,
            ),
            TrustEventOutcome::ContinueSession
        );
        assert!(app.workspace_trust_pending());

        assert_eq!(
            handle_trust_event(
                &mut app,
                Event::Key(press(KeyCode::Down, KeyModifiers::NONE)),
                &hit_map,
            ),
            TrustEventOutcome::Pending
        );
        assert_eq!(
            handle_trust_event(
                &mut app,
                Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
                &hit_map,
            ),
            TrustEventOutcome::Remember
        );

        let deny = hit_map.trust_rows[2].area;
        assert_eq!(
            handle_trust_event(
                &mut app,
                Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    deny.x,
                    deny.y,
                )),
                &hit_map,
            ),
            TrustEventOutcome::Pending
        );
        assert_eq!(
            handle_trust_event(
                &mut app,
                Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
                &hit_map,
            ),
            TrustEventOutcome::Deny
        );

        app.select_trust_option(0);
        assert_eq!(
            handle_trust_event(
                &mut app,
                Event::Key(press(KeyCode::Esc, KeyModifiers::NONE)),
                &hit_map,
            ),
            TrustEventOutcome::Deny
        );
    }

    #[test]
    fn session_trust_does_not_write_but_remember_uses_an_isolated_store() {
        let temp = tempfile::tempdir().expect("temp trust root");
        let cwd = temp.path().join("workspace");
        std::fs::create_dir(&cwd).expect("workspace fixture");
        let store = temp.path().join("trusted-folders.txt");
        let persist = |path: &Path| crate::trust::remember_in_test_store(path, &store);

        let mut app = app();
        app.require_workspace_trust(cwd.display().to_string());
        accept_workspace_trust(&mut app, &cwd, false, persist);
        assert!(
            !store.exists(),
            "session-only trust must not touch the store"
        );
        assert!(!app.workspace_trust_pending());

        app.require_workspace_trust(cwd.display().to_string());
        accept_workspace_trust(&mut app, &cwd, true, persist);
        let persisted = std::fs::read_to_string(&store).expect("isolated trust store");
        assert_eq!(
            persisted.trim(),
            cwd.canonicalize().expect("canonical cwd").to_string_lossy()
        );
        assert!(!app.workspace_trust_pending());
    }

    #[test]
    fn selected_text_keeps_ctrl_c_copy_precedence_during_trust() {
        let mut app = app();
        app.require_workspace_trust("fixture");
        let hit_map = draw_hit_map(&app, 80, 24);
        let path = hit_map.trust_path.as_ref().expect("path hit").area;
        assert_eq!(
            handle_trust_event(
                &mut app,
                Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    path.x,
                    path.y,
                )),
                &hit_map,
            ),
            TrustEventOutcome::Pending
        );
        assert_eq!(
            handle_trust_event(
                &mut app,
                Event::Mouse(mouse(
                    MouseEventKind::Drag(MouseButton::Left),
                    path.x + 4,
                    path.y,
                )),
                &hit_map,
            ),
            TrustEventOutcome::Pending
        );
        let ctrl_c = || Event::Key(press(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert_eq!(
            handle_trust_event(&mut app, ctrl_c(), &hit_map),
            TrustEventOutcome::Copy("fixt".to_string())
        );
        assert!(app.workspace_trust_pending());
        assert_eq!(
            handle_trust_event(&mut app, ctrl_c(), &hit_map),
            TrustEventOutcome::Exit
        );
    }

    #[test]
    fn prompt_timestamp_is_local_hh_mm_shape() {
        let value = local_prompt_time();
        assert_eq!(value.len(), 5);
        assert_eq!(value.as_bytes()[2], b':');
        assert!(value[..2].parse::<u8>().is_ok_and(|hour| hour < 24));
        assert!(value[3..].parse::<u8>().is_ok_and(|minute| minute < 60));
    }

    #[test]
    fn prompt_timestamp_formats_the_precomputed_local_offset() {
        let offset = time::UtcOffset::from_hms(2, 30, 0).expect("offset");
        let local = time::OffsetDateTime::UNIX_EPOCH.to_offset(offset);
        assert_eq!(format_prompt_time(local), "02:30");
    }

    #[test]
    fn runtime_events_map_without_provider_or_view_state() {
        assert_eq!(
            map_runtime_event(RuntimeEvent::Usage(TokenUsage {
                input_tokens: 12,
                output_tokens: 34,
                cache_creation_input_tokens: 5,
                cache_read_input_tokens: 6,
            })),
            RuntimeUpdate::Usage {
                input_tokens: 23,
                output_tokens: 34,
                cached_input_tokens: 6,
            }
        );
        assert_eq!(
            map_runtime_event(RuntimeEvent::Stopped(StopReason::Cancelled)),
            RuntimeUpdate::Stopped(StopState::Cancelled)
        );
        assert_eq!(
            map_runtime_event(RuntimeEvent::FilesTouched(Vec::new())),
            RuntimeUpdate::FilesTouched
        );
        assert_eq!(
            map_runtime_event(RuntimeEvent::Stopped(StopReason::Quiesced)),
            RuntimeUpdate::Stopped(StopState::Quiesced)
        );
    }

    #[test]
    fn terminal_mode_commands_enter_and_restore_in_safe_order() {
        let mut entered = Vec::new();
        write_required_modes(&mut entered, true).expect("required mode bytes");
        let text = String::from_utf8(entered).expect("ANSI is UTF-8");
        let alternate = text.find("?1049h").expect("alternate enter");
        let paste = text.find("?2004h").expect("paste enable");
        assert!(alternate < paste);

        // Crossterm uses the Windows console input API for mouse capture, so
        // there is deliberately no mouse escape sequence in this byte buffer.
        // On ANSI backends the sequence remains observable and ordered.
        #[cfg(not(windows))]
        {
            let mouse = text.find("?1000h").expect("mouse enable");
            assert!(alternate < mouse && mouse < paste);
        }

        let mut mouse_free = Vec::new();
        write_required_modes(&mut mouse_free, false).expect("mouse-free mode bytes");
        #[cfg(not(windows))]
        assert!(!String::from_utf8(mouse_free)
            .expect("ANSI is UTF-8")
            .contains("?1000h"));

        let mut restored = Vec::new();
        write_restore_modes(&mut restored, true, true).expect("restore mode bytes");
        let text = String::from_utf8(restored).expect("ANSI is UTF-8");
        let paste = text.find("?2004l").expect("paste disable");
        let alternate = text.find("?1049l").expect("alternate leave");
        assert!(paste < alternate);

        #[cfg(not(windows))]
        {
            let keyboard = text.find("<1u").expect("keyboard pop");
            let mouse = text.find("?1000l").expect("mouse disable");
            assert!(keyboard < paste && paste < mouse && mouse < alternate);
        }

        let mut mouse_free_restore = Vec::new();
        write_restore_modes(&mut mouse_free_restore, false, false)
            .expect("mouse-free restore bytes");
        #[cfg(not(windows))]
        assert!(!String::from_utf8(mouse_free_restore)
            .expect("ANSI is UTF-8")
            .contains("?1000l"));
    }

    #[test]
    fn boolean_host_settings_accept_only_documented_values() {
        for value in ["true", "TRUE", "1"] {
            assert_eq!(parse_bool_setting(value), Some(true));
        }
        for value in ["false", "FALSE", "0"] {
            assert_eq!(parse_bool_setting(value), Some(false));
        }
        for value in ["", "yes", "2"] {
            assert_eq!(parse_bool_setting(value), None);
        }
    }

    #[test]
    fn unified_diff_parser_preserves_files_counts_kinds_and_line_numbers() {
        let files = parse_unified_diff(
            "diff --git a/src/one.rs b/src/one.rs\n\
             index 111..222 100644\n\
             --- a/src/one.rs\n\
             +++ b/src/one.rs\n\
             @@ -2,2 +2,2 @@\n\
             \x20keep\n\
             -old\n\
             +new\n\
             diff --git a/new.txt b/new.txt\n\
             new file mode 100644\n\
             --- /dev/null\n\
             +++ b/new.txt\n\
             @@ -0,0 +1 @@\n\
             +hello\n",
        );

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/one.rs");
        assert_eq!((files[0].additions, files[0].deletions), (1, 1));
        assert_eq!(files[0].lines[1].old_line, Some(2));
        assert_eq!(files[0].lines[1].new_line, Some(2));
        assert_eq!(files[0].lines[2].kind, DiffLineKind::Deletion);
        assert_eq!(files[0].lines[2].old_line, Some(3));
        assert_eq!(files[0].lines[3].kind, DiffLineKind::Addition);
        assert_eq!(files[0].lines[3].new_line, Some(3));
        assert_eq!(files[1].status, "A");
        assert_eq!(files[1].path, "new.txt");
        assert_eq!(files[1].additions, 1);
    }

    #[test]
    fn unified_diff_parser_decodes_quoted_paths_without_running_diff_drivers() {
        let files = parse_unified_diff(
            "diff --git \"a/src/file name.rs\" \"b/src/file name.rs\"\n\
             --- \"a/src/file name.rs\"\n\
             +++ \"b/src/file name.rs\"\n\
             @@ -1 +1 @@\n\
             -old\n\
             +new\n\
             diff --git a/old.rs b/new.rs\n\
             similarity index 100%\n\
             rename from old.rs\n\
             rename to \"folder/new\\tname.rs\"\n",
        );

        assert_eq!(files[0].path, "src/file name.rs");
        assert_eq!(files[1].status, "R");
        assert_eq!(files[1].path, "folder/new\tname.rs");
        assert_eq!(
            split_git_path_fields("\"a/src/file name.rs\" \"b/src/file name.rs\""),
            ["a/src/file name.rs", "b/src/file name.rs"]
        );
    }

    #[test]
    fn runtime_event_replay_follows_bottom_or_preserves_a_held_content_anchor() {
        use std::fmt::Write as _;

        let mut seed = app();
        seed.begin_work();
        let mut seed_text = String::new();
        for number in 0..80 {
            writeln!(&mut seed_text, "seed {number:03}").expect("write fixture text");
        }
        seed.apply_runtime(RuntimeUpdate::Text(seed_text));
        seed.apply_runtime(RuntimeUpdate::Stopped(StopState::Done));

        let script = || {
            vec![
                RuntimeEvent::Text("stream 001\n".to_string()),
                RuntimeEvent::Text("stream 002\nSTREAM_TAIL".to_string()),
                RuntimeEvent::Usage(TokenUsage {
                    input_tokens: 12,
                    output_tokens: 34,
                    ..TokenUsage::default()
                }),
                RuntimeEvent::ToolStarted {
                    id: "fixture-tool".to_string(),
                    name: "inspect".to_string(),
                    detail: String::new(),
                },
                RuntimeEvent::ToolFinished {
                    id: "fixture-tool".to_string(),
                    name: "inspect".to_string(),
                    is_error: false,
                    cancelled: false,
                    output: "detail one\ndetail two".to_string(),
                    duration_ms: 25,
                },
                RuntimeEvent::Stopped(StopReason::Done),
            ]
        };

        let mut following = seed.clone();
        following.begin_work();
        for event in script() {
            following.apply_runtime(map_runtime_event(event));
        }
        let bottom = following.active_timeline().view(40, 8);
        assert!(bottom.rows.iter().any(|row| row.text == "STREAM_TAIL"));
        assert!(bottom
            .rows
            .iter()
            .any(|row| row.text == "inspect completed · 2 lines · 25 ms"));
        assert_eq!(
            following.active_usage(),
            Some(UsageTotals {
                input_tokens: 12,
                output_tokens: 34,
                cached_input_tokens: 0,
            })
        );
        assert_eq!(following.active_work(), WorkState::Idle);

        let mut held = seed;
        held.active_timeline_mut().scroll_by(-12, 40, 8);
        let ViewportAnchor::Held(anchor) = held.active_timeline().viewport else {
            panic!("seed must be held away from bottom");
        };
        held.begin_work();
        for event in script() {
            held.apply_runtime(map_runtime_event(event));
        }
        let held_view = held.active_timeline().view(31, 6);
        assert_eq!(
            held_view.rows.first().map(|row| row.item_id),
            Some(anchor.item_id)
        );
        assert_eq!(
            held.active_timeline().viewport,
            ViewportAnchor::Held(anchor)
        );
    }
}
