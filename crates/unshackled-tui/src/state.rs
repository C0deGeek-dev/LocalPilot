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
    /// Visual rows to keep above the latest transcript output.
    pub transcript_scroll: usize,
    pub input: String,
    /// UTF-8 byte offset where the next input edit occurs.
    pub input_cursor: usize,
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
            transcript_scroll: 0,
            input: String::new(),
            input_cursor: 0,
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
        let rows = content.split('\n').count().max(1);
        let placeholder = format!("[{rows} pasted rows #{}]", self.pastes.len() + 1);
        self.pastes.push(Paste {
            placeholder: placeholder.clone(),
            content,
        });
        placeholder
    }

    /// Insert text at the current input cursor and advance past it.
    pub fn insert_input(&mut self, text: &str) {
        self.normalize_input_cursor();
        self.input.insert_str(self.input_cursor, text);
        self.input_cursor += text.len();
    }

    /// Insert a newline at the cursor. At the end of the input, a trailing
    /// continuation marker and spaces are consumed first.
    pub fn insert_input_newline(&mut self) {
        self.normalize_input_cursor();
        if self.input_cursor == self.input.len() {
            let kept = self.input.trim_end_matches(' ').len();
            if self.input[..kept].ends_with('\\') {
                self.input.truncate(kept - 1);
                self.input_cursor = self.input.len();
            }
        }
        self.insert_input("\n");
    }

    /// Move the cursor one character left.
    pub fn move_input_left(&mut self) {
        self.normalize_input_cursor();
        if let Some((offset, _)) = self.input[..self.input_cursor].char_indices().next_back() {
            self.input_cursor = offset;
        }
    }

    /// Move the cursor one character right.
    pub fn move_input_right(&mut self) {
        self.normalize_input_cursor();
        if let Some(ch) = self.input[self.input_cursor..].chars().next() {
            self.input_cursor += ch.len_utf8();
        }
    }

    /// Move the cursor to the same character column on the previous logical line.
    pub fn move_input_up(&mut self) {
        self.normalize_input_cursor();
        let current_start = self.input[..self.input_cursor]
            .rfind('\n')
            .map_or(0, |offset| offset + 1);
        if current_start == 0 {
            return;
        }

        let column = self.input[current_start..self.input_cursor].chars().count();
        let previous_end = current_start - 1;
        let previous_start = self.input[..previous_end]
            .rfind('\n')
            .map_or(0, |offset| offset + 1);
        self.input_cursor = previous_start
            + byte_offset_at_column(&self.input[previous_start..previous_end], column);
    }

    /// Move the cursor to the same character column on the next logical line.
    pub fn move_input_down(&mut self) {
        self.normalize_input_cursor();
        let current_start = self.input[..self.input_cursor]
            .rfind('\n')
            .map_or(0, |offset| offset + 1);
        let column = self.input[current_start..self.input_cursor].chars().count();
        let Some(next_offset) = self.input[self.input_cursor..].find('\n') else {
            return;
        };
        let next_start = self.input_cursor + next_offset + 1;
        let next_end = self.input[next_start..]
            .find('\n')
            .map_or(self.input.len(), |offset| next_start + offset);
        self.input_cursor =
            next_start + byte_offset_at_column(&self.input[next_start..next_end], column);
    }

    /// Move the cursor to the start of its logical line.
    pub fn move_input_home(&mut self) {
        self.normalize_input_cursor();
        self.input_cursor = self.input[..self.input_cursor]
            .rfind('\n')
            .map_or(0, |offset| offset + 1);
    }

    /// Move the cursor to the end of its logical line.
    pub fn move_input_end(&mut self) {
        self.normalize_input_cursor();
        self.input_cursor = self.input[self.input_cursor..]
            .find('\n')
            .map_or(self.input.len(), |offset| self.input_cursor + offset);
    }

    /// Delete the character immediately before the cursor.
    pub fn backspace_input(&mut self) {
        self.normalize_input_cursor();
        if let Some((offset, _)) = self.input[..self.input_cursor].char_indices().next_back() {
            self.input.drain(offset..self.input_cursor);
            self.input_cursor = offset;
        }
    }

    /// Delete the character under the cursor.
    pub fn delete_input(&mut self) {
        self.normalize_input_cursor();
        if let Some(ch) = self.input[self.input_cursor..].chars().next() {
            self.input
                .drain(self.input_cursor..self.input_cursor + ch.len_utf8());
        }
    }

    /// A valid UTF-8 byte offset for rendering the input cursor.
    #[must_use]
    pub fn normalized_input_cursor(&self) -> usize {
        let mut cursor = self.input_cursor.min(self.input.len());
        while !self.input.is_char_boundary(cursor) {
            cursor = cursor.saturating_sub(1);
        }
        cursor
    }

    fn normalize_input_cursor(&mut self) {
        self.input_cursor = self.normalized_input_cursor();
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
        self.input_cursor = 0;
        let expanded = self.expand_pastes(&raw);
        self.pastes.clear();
        expanded
    }

    /// Scroll the transcript upward by rendered rows.
    pub fn scroll_transcript_up(&mut self, rows: usize) {
        let max_scroll = self.transcript_logical_rows().saturating_sub(1);
        self.transcript_scroll = self.transcript_scroll.saturating_add(rows).min(max_scroll);
    }

    /// Scroll the transcript downward by rendered rows.
    pub fn scroll_transcript_down(&mut self, rows: usize) {
        self.transcript_scroll = self.transcript_scroll.saturating_sub(rows);
    }

    /// Resume following the latest transcript output.
    pub fn scroll_transcript_to_bottom(&mut self) {
        self.transcript_scroll = 0;
    }

    fn transcript_logical_rows(&self) -> usize {
        let transcript_rows = self
            .transcript
            .iter()
            .map(|line| line.text.trim_start_matches('\n').split('\n').count())
            .sum::<usize>();
        let streaming_rows = if self.streaming.is_empty() {
            0
        } else {
            self.streaming.trim_start_matches('\n').split('\n').count()
        };
        transcript_rows + streaming_rows
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
            UiEvent::RecoveryNotice(text) => {
                // Drop the in-progress (bad) streamed text so the retry starts on a
                // fresh line instead of appending to the discarded output.
                self.streaming.clear();
                self.transcript.push(TranscriptLine {
                    speaker: "system".to_string(),
                    text,
                });
            }
            UiEvent::PlanUpdated(plan) => self.plan = plan,
            UiEvent::ApprovalRequested(request) => self.approval = Some(request),
            UiEvent::ApprovalResolved => self.approval = None,
            UiEvent::ToggleThinking => self.thinking.visible = !self.thinking.visible,
            UiEvent::Quit => self.should_quit = true,
        }
    }
}

fn byte_offset_at_column(line: &str, column: usize) -> usize {
    line.char_indices()
        .nth(column)
        .map_or(line.len(), |(offset, _)| offset)
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
    /// A recovery notice: posts a system line and discards the in-progress
    /// streamed text so a retry does not append to the bad output.
    RecoveryNotice(String),
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
        assert!(placeholder.contains("4 pasted rows"));

        state.input = format!("see this {placeholder} please");
        let expanded = state.take_input_expanded();
        assert_eq!(expanded, format!("see this {body} please"));
        // The set is cleared once consumed, and the input is taken.
        assert!(state.pastes.is_empty());
        assert!(state.input.is_empty());
    }

    #[test]
    fn a_recovery_notice_discards_the_in_progress_stream() {
        let mut state = state();
        state.streaming = "////////".to_string();
        state.apply(UiEvent::RecoveryNotice("recovering…".to_string()));
        // The bad partial output is dropped so the retry starts on a fresh line.
        assert!(state.streaming.is_empty());
        assert!(matches!(state.transcript.last(), Some(line) if line.speaker == "system"));
    }

    #[test]
    fn placeholders_are_numbered_per_paste() {
        let mut state = state();
        let first = state.register_paste("a\nb".into());
        let second = state.register_paste("c\nd".into());
        assert!(first.contains("2 pasted rows"));
        assert!(second.contains("2 pasted rows"));
        assert!(first.contains("#1"));
        assert!(second.contains("#2"));
    }

    #[test]
    fn input_edits_follow_the_cursor_on_utf8_boundaries() {
        let mut state = state();
        state.insert_input("aéz");
        state.move_input_left();
        state.move_input_left();
        state.insert_input("B");
        assert_eq!(state.input, "aBéz");

        state.delete_input();
        assert_eq!(state.input, "aBz");
        state.backspace_input();
        assert_eq!(state.input, "az");
    }

    #[test]
    fn newline_at_the_cursor_and_continuation_at_the_end_are_supported() {
        let mut state = state();
        state.insert_input("abcd");
        state.move_input_left();
        state.move_input_left();
        state.insert_input_newline();
        assert_eq!(state.input, "ab\ncd");
        assert_eq!(state.input_cursor, 3);

        state.input = "next \\  ".to_string();
        state.input_cursor = state.input.len();
        state.insert_input_newline();
        assert_eq!(state.input, "next \n");
    }

    #[test]
    fn vertical_input_movement_preserves_character_columns() {
        let mut state = state();
        state.input = "abé\nwxyz\nq".to_string();
        state.input_cursor = "abé".len();

        state.move_input_down();
        assert_eq!(&state.input[..state.input_cursor], "abé\nwxy");

        state.move_input_down();
        assert_eq!(&state.input[..state.input_cursor], "abé\nwxyz\nq");

        state.move_input_up();
        assert_eq!(&state.input[..state.input_cursor], "abé\nw");

        state.move_input_up();
        assert_eq!(&state.input[..state.input_cursor], "a");
    }
}
