//! The TUI view model.
//!
//! The TUI is UI-only: it owns layout, rendering, and input, never business
//! logic. The session runtime's events are mapped into [`UiEvent`]s by the
//! caller, keeping this crate decoupled from the provider/harness stack.

/// Operating mode shown in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Agent,
    Harness,
}

impl Mode {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Mode::Agent => "agent",
            Mode::Harness => "harness",
        }
    }
}

/// Permission profile shown in the UI. `bypass` is always surfaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Default,
    Relaxed,
    Bypass,
}

impl Profile {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Profile::Default => "default",
            Profile::Relaxed => "relaxed",
            Profile::Bypass => "BYPASS",
        }
    }
}

/// Header identity fields.
#[derive(Debug, Clone)]
pub struct Header {
    pub version: String,
    pub provider: String,
    pub model: String,
    pub workspace: String,
    pub session_id: String,
    /// A newer release tag, if one is available (shown in the header).
    pub update: Option<String>,
}

/// Always-visible footer stats.
#[derive(Debug, Clone, Default)]
pub struct FooterStats {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_per_sec: f64,
    pub context_used: usize,
    pub context_limit: usize,
    pub cost_usd: Option<f64>,
    pub quota_reset: Option<String>,
}

/// The optional thinking/reasoning side panel.
#[derive(Debug, Clone, Default)]
pub struct ThinkingPanel {
    pub visible: bool,
    pub text: String,
}

/// One task in the model's plan checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub title: String,
    pub status: String,
}

/// A pending tool-approval request.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool: String,
    pub target: String,
    pub risk_class: String,
}

/// A modal picker (model/provider selection).
#[derive(Debug, Clone)]
pub struct Picker {
    pub title: String,
    pub options: Vec<String>,
    pub selected: usize,
}

/// A large pasted block collapsed to a short placeholder in the input line. The
/// full content is restored before the prompt is sent to the model.
#[derive(Debug, Clone)]
pub struct Paste {
    pub placeholder: String,
    pub content: String,
}

/// The first-run gate asking whether the workspace folder is trusted. Until it
/// is answered the rest of the input is blocked.
#[derive(Debug, Clone)]
pub struct TrustPrompt {
    /// The folder being entered, shown in full so the user can verify it.
    pub path: String,
}

/// One transcript entry.
#[derive(Debug, Clone)]
pub struct TranscriptLine {
    pub speaker: String,
    pub text: String,
}

/// The full UI state.
#[derive(Debug, Clone)]
pub struct AppState {
    pub header: Header,
    pub transcript: Vec<TranscriptLine>,
    pub streaming: String,
    pub input: String,
    pub footer: FooterStats,
    pub thinking: ThinkingPanel,
    pub mode: Mode,
    pub profile: Profile,
    pub approval: Option<ApprovalRequest>,
    pub picker: Option<Picker>,
    /// A blocking first-run trust gate, shown until the folder is trusted.
    pub trust: Option<TrustPrompt>,
    /// Whether the workspace folder has been trusted this session.
    pub trusted: bool,
    /// Large pastes collapsed to placeholders, expanded back on submit.
    pub pastes: Vec<Paste>,
    pub search: Option<String>,
    /// The model's current task checklist (empty until it calls `update_plan`).
    pub plan: Vec<PlanItem>,
    pub should_quit: bool,
    /// Whether a turn is in flight (drives the working indicator).
    pub busy: bool,
    /// Animation frame for the working spinner, advanced by the host each tick.
    pub spinner: usize,
    /// Seconds elapsed in the in-flight turn, updated by the host each tick.
    pub working_secs: u64,
}

impl AppState {
    /// A new state with the given identity, an empty transcript, and defaults.
    #[must_use]
    pub fn new(header: Header, mode: Mode, profile: Profile) -> Self {
        Self {
            header,
            transcript: Vec::new(),
            streaming: String::new(),
            input: String::new(),
            footer: FooterStats::default(),
            thinking: ThinkingPanel::default(),
            mode,
            profile,
            approval: None,
            picker: None,
            trust: None,
            trusted: false,
            pastes: Vec::new(),
            search: None,
            plan: Vec::new(),
            should_quit: false,
            busy: false,
            spinner: 0,
            working_secs: 0,
        }
    }

    /// Collapse a pasted block to a short placeholder, stashing the full text to
    /// be restored on submit. Returns the placeholder to insert into the input.
    pub fn register_paste(&mut self, content: String) -> String {
        let lines = content.lines().count().max(1);
        let placeholder = format!("[pasted #{} · {lines} lines]", self.pastes.len() + 1);
        self.pastes.push(Paste {
            placeholder: placeholder.clone(),
            content,
        });
        placeholder
    }

    /// Restore any collapsed pastes in `text` to their full content.
    #[must_use]
    pub fn expand_pastes(&self, text: &str) -> String {
        let mut out = text.to_string();
        for paste in &self.pastes {
            out = out.replace(&paste.placeholder, &paste.content);
        }
        out
    }

    /// Take the current input, restoring collapsed pastes, and clear the set.
    pub fn take_input_expanded(&mut self) -> String {
        let raw = std::mem::take(&mut self.input);
        let expanded = self.expand_pastes(&raw);
        self.pastes.clear();
        expanded
    }

    /// Apply a mapped runtime/UI event to the state.
    pub fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::TextDelta(delta) => self.streaming.push_str(&delta),
            UiEvent::ReasoningDelta(delta) => self.thinking.text.push_str(&delta),
            UiEvent::TurnComplete => {
                if !self.streaming.is_empty() {
                    let text = std::mem::take(&mut self.streaming);
                    self.transcript.push(TranscriptLine {
                        speaker: "assistant".to_string(),
                        text,
                    });
                }
            }
            UiEvent::UserMessage(text) => self.transcript.push(TranscriptLine {
                speaker: "you".to_string(),
                text,
            }),
            UiEvent::Usage {
                tokens_in,
                tokens_out,
                tokens_per_sec,
            } => {
                self.footer.tokens_in = tokens_in;
                self.footer.tokens_out = tokens_out;
                self.footer.tokens_per_sec = tokens_per_sec;
            }
            UiEvent::ContextUsage {
                context_used,
                context_limit,
            } => {
                self.footer.context_used = context_used;
                self.footer.context_limit = context_limit;
            }
            UiEvent::QuotaPaused { reset } => self.footer.quota_reset = Some(reset),
            UiEvent::Notice(text) => self.transcript.push(TranscriptLine {
                speaker: "system".to_string(),
                text,
            }),
            UiEvent::PlanUpdated(plan) => self.plan = plan,
            UiEvent::ApprovalRequested(request) => self.approval = Some(request),
            UiEvent::ApprovalResolved => self.approval = None,
            UiEvent::ToggleThinking => self.thinking.visible = !self.thinking.visible,
            UiEvent::Quit => self.should_quit = true,
        }
    }
}

/// A UI-facing event, mapped from the session runtime by the caller.
#[derive(Debug, Clone)]
pub enum UiEvent {
    UserMessage(String),
    TextDelta(String),
    ReasoningDelta(String),
    Usage {
        tokens_in: u64,
        tokens_out: u64,
        tokens_per_sec: f64,
    },
    ContextUsage {
        context_used: usize,
        context_limit: usize,
    },
    TurnComplete,
    QuotaPaused {
        reset: String,
    },
    /// A system notice (warning or error) to show in the transcript.
    Notice(String),
    /// The model's task checklist changed.
    PlanUpdated(Vec<PlanItem>),
    ApprovalRequested(ApprovalRequest),
    ApprovalResolved,
    ToggleThinking,
    Quit,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState::new(
            Header {
                version: "0".into(),
                provider: "p".into(),
                model: "m".into(),
                workspace: "w".into(),
                session_id: "s".into(),
                update: None,
            },
            Mode::Agent,
            Profile::Default,
        )
    }

    #[test]
    fn a_registered_paste_round_trips_through_its_placeholder() {
        let mut state = state();
        let body = "line one\nline two\nline three\nline four".to_string();
        let placeholder = state.register_paste(body.clone());
        assert!(placeholder.contains("#1"));
        assert!(placeholder.contains("4 lines"));

        state.input = format!("see this {placeholder} please");
        let expanded = state.take_input_expanded();
        assert_eq!(expanded, format!("see this {body} please"));
        // The set is cleared once consumed, and the input is taken.
        assert!(state.pastes.is_empty());
        assert!(state.input.is_empty());
    }

    #[test]
    fn placeholders_are_numbered_per_paste() {
        let mut state = state();
        let first = state.register_paste("a\nb".into());
        let second = state.register_paste("c\nd".into());
        assert!(first.contains("#1"));
        assert!(second.contains("#2"));
    }
}
