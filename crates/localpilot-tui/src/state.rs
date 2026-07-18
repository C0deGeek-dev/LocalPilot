//! The TUI view model.
//!
//! The TUI is UI-only: it owns layout, rendering, and input, never business
//! logic. The session runtime's events are mapped into [`UiEvent`]s by the
//! caller, keeping this crate decoupled from the provider/harness stack.

const MAX_INPUT_HISTORY: usize = 100;

/// Operating mode shown in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Agent,
    Harness,
    /// Research mode: a bare prompt is treated as a topic to research —
    /// local sources plus disclosed, allowlist-gated web per config
    /// (ADR-0076) — rather than a model turn.
    Research,
}

impl Mode {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Mode::Agent => "agent",
            Mode::Harness => "harness",
            Mode::Research => "research",
        }
    }
}

/// Permission profile shown in the UI. `bypass` and `unrestricted` are always
/// surfaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Default,
    Relaxed,
    Bypass,
    Unrestricted,
}

impl Profile {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Profile::Default => "default",
            Profile::Relaxed => "relaxed",
            Profile::Bypass => "BYPASS",
            Profile::Unrestricted => "UNRESTRICTED",
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
    /// The conversation's name, when the user has set one (`/name` / `/rename`).
    /// Shown in place of the raw id in the header and status line.
    pub session_name: Option<String>,
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
    /// The requested reasoning-effort level, when one is set.
    pub effort: Option<String>,
}

/// The optional thinking/reasoning side panel.
#[derive(Debug, Clone, Default)]
pub struct ThinkingPanel {
    pub visible: bool,
    pub text: String,
}

/// The optional "memories used this turn" inspector panel. The host renders the
/// inspector body (ids + provenance + epistemic status + contradictions +
/// staleness) and pushes it in; the TUI only displays the text.
#[derive(Debug, Clone, Default)]
pub struct MemoryPanel {
    pub visible: bool,
    pub body: String,
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

/// A large pasted block collapsed to a short placeholder in the input line. The
/// full content is restored before the prompt is sent to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paste {
    pub placeholder: String,
    pub content: String,
}

/// One recallable prompt: the visible text plus the paste mappings needed to
/// restore any collapsed placeholders when the prompt is submitted again.
/// Without the mappings a recalled prompt would send the literal placeholder
/// text (e.g. `[10 pasted rows #1]`) to the model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecallEntry {
    /// The visible prompt text, placeholders intact.
    pub text: String,
    /// The placeholder→content mappings the prompt was submitted with.
    pub pastes: Vec<Paste>,
}

impl RecallEntry {
    /// An entry with no paste mappings.
    #[must_use]
    pub fn text_only(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            pastes: Vec::new(),
        }
    }
}

/// A submitted prompt taken from the composer: the visible form for the
/// transcript and history, the expanded form for the model, and the paste
/// mappings the visible form depends on (for faithful recall later).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmittedInput {
    /// The input as typed, placeholders intact.
    pub shown: String,
    /// The input with pastes expanded — what the model receives.
    pub prompt: String,
    /// The mappings for placeholders that occur in `shown`.
    pub pastes: Vec<Paste>,
}

/// An image attached from the clipboard, shown as a short placeholder in the
/// input line and sent as a multimodal block when the prompt is submitted.
#[derive(Debug, Clone)]
pub struct ImageAttachment {
    /// The placeholder shown inline in the input (e.g. "[image #1 · PNG 12 KB]").
    pub placeholder: String,
    /// The image media type, e.g. "image/png".
    pub media_type: String,
    /// Base64-encoded image bytes.
    pub data: String,
}

/// The first-run gate asking whether the workspace folder is trusted. Until it
/// is answered the rest of the input is blocked.
#[derive(Debug, Clone)]
pub struct TrustPrompt {
    /// The folder being entered, shown in full so the user can verify it.
    pub path: String,
}

/// One command shown in the slash-command autocomplete popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashSuggestion {
    /// Command name without the leading slash (e.g. "clear").
    pub name: String,
    /// Short description shown beside the command.
    pub description: String,
}

/// Slash-command autocomplete popup state.
#[derive(Debug, Clone)]
pub struct SlashPicker {
    /// The raw slash command text the user typed (e.g. "/se").
    pub query: String,
    /// All matching commands for the current query.
    pub items: Vec<SlashSuggestion>,
    /// Index of the currently highlighted item.
    pub selected: usize,
}

/// One workspace file shown in the `@` file-mention autocomplete popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSuggestion {
    /// Workspace-relative path, forward-slash separated.
    pub path: String,
}

/// `@`-mention file autocomplete popup state.
#[derive(Debug, Clone)]
pub struct FilePicker {
    /// The text typed after the `@` (e.g. "b").
    pub query: String,
    /// All workspace files whose filename starts with `query`.
    pub items: Vec<FileSuggestion>,
    /// Index of the currently highlighted item.
    pub selected: usize,
}

/// One transcript entry.
#[derive(Debug, Clone)]
pub struct TranscriptLine {
    pub speaker: String,
    pub text: String,
}

/// A tool that is currently running, shown as a transient live indicator until
/// it finishes and its result line lands in scrollback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveTool {
    pub id: String,
    pub name: String,
}

/// A background process started this session (via `run_background`), surfaced in
/// the status line and the `/bg` command. The host pushes the current set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundProcess {
    /// The id the host uses to stop it (e.g. "bg-1").
    pub id: String,
    /// The command line that was started.
    pub command: String,
    /// Whether the process is still running.
    pub alive: bool,
}

/// The full UI state.
#[derive(Debug, Clone)]
pub struct AppState {
    pub header: Header,
    /// Finished transcript items awaiting emission to native scrollback. The host
    /// drains them each frame via [`AppState::drain_for_scrollback`]; they are
    /// emitted once and never redrawn.
    pub transcript: Vec<TranscriptLine>,
    pub streaming: String,
    pub input: String,
    /// UTF-8 byte offset where the next input edit occurs.
    pub input_cursor: usize,
    input_history: Vec<RecallEntry>,
    history_cursor: Option<usize>,
    history_draft: String,
    /// The paste mappings the draft depends on, saved and restored with it.
    history_draft_pastes: Vec<Paste>,
    /// Persisted prompts submitted in the current project, oldest-first; the seed
    /// for project-scoped recall.
    project_history: Vec<RecallEntry>,
    /// Persisted prompts submitted in every project, oldest-first; the seed for
    /// the view-all-projects recall scope.
    all_history: Vec<RecallEntry>,
    /// Whether recall currently draws from `all_history` rather than
    /// `project_history` (toggled by the view-all key).
    viewing_all_history: bool,
    pub footer: FooterStats,
    pub thinking: ThinkingPanel,
    /// The "memories used this turn" inspector panel.
    pub memory_panel: MemoryPanel,
    pub mode: Mode,
    pub profile: Profile,
    pub approval: Option<ApprovalRequest>,
    /// A blocking first-run trust gate, shown until the folder is trusted.
    pub trust: Option<TrustPrompt>,
    /// Whether the workspace folder has been trusted this session.
    pub trusted: bool,
    /// Large pastes collapsed to placeholders, expanded back on submit.
    pub pastes: Vec<Paste>,
    /// Session-monotonic paste counter: placeholder numbers never restart
    /// after a submit, so two different pastes in one session can never share
    /// a placeholder string.
    paste_seq: usize,
    /// Clipboard images attached to the next prompt, shown as placeholders.
    pub images: Vec<ImageAttachment>,
    /// Active slash-command autocomplete picker.
    pub slash_picker: Option<SlashPicker>,
    /// Active `@`-mention file autocomplete picker.
    pub file_picker: Option<FilePicker>,
    /// Workspace files offered by the `@` picker (relative, forward-slash). The
    /// host populates this; the picker filters it in memory.
    workspace_files: Vec<String>,
    /// The model's current task checklist (empty until it calls `update_plan`).
    pub plan: Vec<PlanItem>,
    /// Tools currently running, shown as transient live indicators.
    pub active_tools: Vec<ActiveTool>,
    /// Background processes started this session, shown in the status line and
    /// listed by `/bg`. The host refreshes this set.
    pub background: Vec<BackgroundProcess>,
    pub should_quit: bool,
    /// Whether a turn is in flight (drives the working indicator).
    pub busy: bool,
    /// Animation frame for the working spinner, advanced by the host each tick.
    pub spinner: usize,
    /// Seconds elapsed in the in-flight turn, updated by the host each tick.
    pub working_secs: u64,
    /// An incomplete trailing escape sequence held back from the visible text
    /// stream, completed by the next [`UiEvent::TextDelta`] (see
    /// [`scrub_streaming`]). Cleared when the stream ends or is discarded.
    text_escape_carry: String,
    /// The same holdback for the reasoning stream.
    reasoning_escape_carry: String,
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
            input_cursor: 0,
            input_history: Vec::new(),
            history_cursor: None,
            history_draft: String::new(),
            history_draft_pastes: Vec::new(),
            project_history: Vec::new(),
            all_history: Vec::new(),
            viewing_all_history: false,
            footer: FooterStats::default(),
            thinking: ThinkingPanel::default(),
            memory_panel: MemoryPanel::default(),
            mode,
            profile,
            approval: None,
            trust: None,
            trusted: false,
            pastes: Vec::new(),
            paste_seq: 0,
            images: Vec::new(),
            slash_picker: None,
            file_picker: None,
            workspace_files: Vec::new(),
            plan: Vec::new(),
            active_tools: Vec::new(),
            background: Vec::new(),
            should_quit: false,
            busy: false,
            spinner: 0,
            working_secs: 0,
            text_escape_carry: String::new(),
            reasoning_escape_carry: String::new(),
        }
    }

    /// Collapse a pasted block to a short placeholder, stashing the full text to
    /// be restored on submit. Returns the placeholder to insert into the input.
    pub fn register_paste(&mut self, content: String) -> String {
        let rows = content.split('\n').count().max(1);
        self.paste_seq += 1;
        let placeholder = format!("[{rows} pasted rows #{}]", self.paste_seq);
        self.pastes.push(Paste {
            placeholder: placeholder.clone(),
            content,
        });
        placeholder
    }

    /// Attach a clipboard image, stashing its data and inserting a short
    /// placeholder into the input line. `byte_len` is the decoded image size,
    /// used only to render a human-readable placeholder. Returns the placeholder.
    pub fn register_image(
        &mut self,
        media_type: impl Into<String>,
        data: impl Into<String>,
        byte_len: usize,
    ) -> String {
        let media_type = media_type.into();
        let label = media_type
            .rsplit('/')
            .next()
            .unwrap_or("image")
            .to_uppercase();
        let placeholder = format!(
            "[image #{} · {label} {}]",
            self.images.len() + 1,
            human_byte_size(byte_len)
        );
        self.images.push(ImageAttachment {
            placeholder: placeholder.clone(),
            media_type,
            data: data.into(),
        });
        placeholder
    }

    /// Take the attached images, clearing the set. Called alongside
    /// [`Self::take_input_for_submit`] when a prompt is sent.
    pub fn take_images(&mut self) -> Vec<ImageAttachment> {
        std::mem::take(&mut self.images)
    }

    /// Insert text at the current input cursor and advance past it.
    pub fn insert_input(&mut self, text: &str) {
        self.leave_history_navigation();
        self.normalize_input_cursor();
        self.input.insert_str(self.input_cursor, text);
        self.input_cursor += text.len();
    }

    /// Insert a newline at the cursor. At the end of the input, a trailing
    /// continuation marker and spaces are consumed first.
    pub fn insert_input_newline(&mut self) {
        self.leave_history_navigation();
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
        self.leave_history_navigation();
        self.normalize_input_cursor();
        if let Some((offset, _)) = self.input[..self.input_cursor].char_indices().next_back() {
            self.input.drain(offset..self.input_cursor);
            self.input_cursor = offset;
        }
    }

    /// Delete the character under the cursor.
    pub fn delete_input(&mut self) {
        self.leave_history_navigation();
        self.normalize_input_cursor();
        if let Some(ch) = self.input[self.input_cursor..].chars().next() {
            self.input
                .drain(self.input_cursor..self.input_cursor + ch.len_utf8());
        }
    }

    /// Clear the composer: empty the input, reset the cursor, leave any history
    /// navigation, and dismiss the slash/mention autocomplete overlays. Used by
    /// staged Ctrl+C so the first press wipes a pending prompt.
    pub fn clear_input(&mut self) {
        self.leave_history_navigation();
        self.input.clear();
        self.input_cursor = 0;
        self.close_slash_picker();
        self.close_file_picker();
    }

    /// Staged Ctrl+C. The first press clears a non-empty composer (dismissing any
    /// open autocomplete overlay as a side effect); once the composer is empty a
    /// press quits. Mirrors the shell convention: Ctrl+C abandons the current
    /// line before it exits the program.
    pub fn ctrl_c(&mut self) {
        if self.input.is_empty() {
            self.should_quit = true;
        } else {
            self.clear_input();
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

    /// Restore any collapsed pastes in `text` to their full content. Newest
    /// mapping wins if two pastes ever share a placeholder string (possible
    /// only across a recall of an old prompt, and then only when row count and
    /// number coincide).
    #[must_use]
    pub fn expand_pastes(&self, text: &str) -> String {
        let mut out = text.to_string();
        for paste in self.pastes.iter().rev() {
            out = out.replace(&paste.placeholder, &paste.content);
        }
        out
    }

    /// Take the current input, restoring collapsed pastes, and clear the set.
    pub fn take_input_expanded(&mut self) -> String {
        self.take_input_for_submit().prompt
    }

    /// Take the composer input for submission, recording the visible form and
    /// its paste mappings in prompt history so a later recall can restore the
    /// pasted content instead of replaying placeholder text.
    pub fn take_input_for_submit(&mut self) -> SubmittedInput {
        let raw = std::mem::take(&mut self.input);
        self.input_cursor = 0;
        let expanded = self.expand_pastes(&raw);
        // Keep only the mappings this prompt actually uses; a mapping left
        // over from an abandoned recall would otherwise ride along forever.
        let pastes: Vec<Paste> = self
            .pastes
            .drain(..)
            .filter(|paste| raw.contains(&paste.placeholder))
            .collect();
        self.record_input_history(&raw, &pastes);
        SubmittedInput {
            shown: raw,
            prompt: expanded,
            pastes,
        }
    }

    /// Whether the cursor is on the first logical input line.
    #[must_use]
    pub fn input_cursor_is_on_first_line(&self) -> bool {
        let cursor = self.normalized_input_cursor();
        !self.input[..cursor].contains('\n')
    }

    /// Whether the cursor is on the last logical input line.
    #[must_use]
    pub fn input_cursor_is_on_last_line(&self) -> bool {
        let cursor = self.normalized_input_cursor();
        !self.input[cursor..].contains('\n')
    }

    /// Replace the input with the previous submitted prompt, shell-style.
    pub fn recall_previous_input(&mut self) -> bool {
        if self.input_history.is_empty() {
            return false;
        }
        let index = match self.history_cursor {
            Some(index) => index.saturating_sub(1),
            None => {
                self.history_draft = self.input.clone();
                self.history_draft_pastes = self.pastes.clone();
                self.input_history.len() - 1
            }
        };
        self.set_history_input(index);
        true
    }

    /// Replace the input with the next submitted prompt, restoring the draft
    /// after the newest history entry.
    pub fn recall_next_input(&mut self) -> bool {
        let Some(index) = self.history_cursor else {
            return false;
        };
        if index + 1 < self.input_history.len() {
            self.set_history_input(index + 1);
        } else {
            self.input = std::mem::take(&mut self.history_draft);
            self.pastes = std::mem::take(&mut self.history_draft_pastes);
            self.input_cursor = self.input.len();
            self.history_cursor = None;
        }
        true
    }

    /// Seed recall from persisted history: `project` is the current project's
    /// prompts and `all` is every project's, both oldest-first. Recall starts
    /// scoped to the project; [`AppState::toggle_history_scope`] switches to all.
    /// This is the UI-only seam — the host loads and filters the store, this never
    /// touches the filesystem. Replaces any prior seed and resets the recall
    /// cursor; the in-session recall semantics are unchanged.
    pub fn seed_input_history(&mut self, project: Vec<RecallEntry>, all: Vec<RecallEntry>) {
        self.project_history = cap_history(project);
        self.all_history = cap_history(all);
        self.viewing_all_history = false;
        self.input_history = self.project_history.clone();
        self.history_cursor = None;
        self.history_draft.clear();
        self.history_draft_pastes.clear();
    }

    /// Toggle recall between this project's history and every project's. Returns
    /// whether the view now shows all projects. The active recall list is swapped
    /// and the cursor reset; navigation is otherwise unchanged.
    pub fn toggle_history_scope(&mut self) -> bool {
        self.viewing_all_history = !self.viewing_all_history;
        self.input_history = if self.viewing_all_history {
            self.all_history.clone()
        } else {
            self.project_history.clone()
        };
        self.history_cursor = None;
        self.history_draft.clear();
        self.history_draft_pastes.clear();
        self.viewing_all_history
    }

    fn record_input_history(&mut self, input: &str, pastes: &[Paste]) {
        self.history_cursor = None;
        self.history_draft.clear();
        self.history_draft_pastes.clear();
        if input.trim().is_empty() {
            return;
        }
        // Record into the active recall list and both scope seeds, so a later
        // view-all toggle still sees prompts submitted this session. Every
        // submission belongs to the current project, hence both seeds.
        let entry = RecallEntry {
            text: input.to_string(),
            pastes: pastes.to_vec(),
        };
        push_capped(&mut self.input_history, &entry);
        push_capped(&mut self.project_history, &entry);
        push_capped(&mut self.all_history, &entry);
    }

    fn set_history_input(&mut self, index: usize) {
        let entry = self.input_history[index].clone();
        self.input = entry.text;
        self.input_cursor = self.input.len();
        self.history_cursor = Some(index);
        // Restore the recalled prompt's paste mappings so its placeholders
        // expand to the original content on submit instead of being sent
        // verbatim (LocalHub#19).
        self.pastes = entry.pastes;
    }

    fn leave_history_navigation(&mut self) {
        if self.history_cursor.is_some() {
            self.history_cursor = None;
            self.history_draft.clear();
            self.history_draft_pastes.clear();
        }
    }

    /// Clear the visible conversation while preserving session identity,
    /// profile, mode, trust, and provider/model display.
    pub fn clear_conversation_view(&mut self) {
        self.transcript.clear();
        self.streaming.clear();
        self.text_escape_carry.clear();
        self.reasoning_escape_carry.clear();
        self.thinking.text.clear();
        self.plan.clear();
        self.active_tools.clear();
        self.approval = None;
        self.busy = false;
        self.spinner = 0;
        self.working_secs = 0;
        self.footer = FooterStats::default();
    }

    /// Take the finished transcript items, handing ownership to the host, which
    /// renders each above the inline viewport with `insert_before`. They flow into
    /// native scrollback once and are dropped here, so the buffer does not grow.
    pub fn drain_for_scrollback(&mut self) -> Vec<TranscriptLine> {
        std::mem::take(&mut self.transcript)
    }

    // --- Slash picker --------------------------------------------------------

    /// The slash commands offered by the autocomplete picker, each with a short
    /// description. This is the single source of truth for the picker; the names
    /// mirror the commands parsed by `parse_slash`.
    const SLASH_COMMANDS: &'static [(&'static str, &'static str)] = &[
        ("agent", "Switch to agent mode"),
        ("harness", "Switch to harness mode"),
        ("default", "Use the default permission profile"),
        ("relaxed", "Use the relaxed permission profile"),
        ("bypass", "Use the bypass permission profile"),
        (
            "unrestricted",
            "Approve everything, workspace boundary included — you take responsibility",
        ),
        ("think", "Toggle the reasoning panel"),
        ("effort", "Set reasoning effort: minimal|low|medium|high"),
        (
            "model",
            "Switch provider/model, or list them (/model [provider [model]])",
        ),
        ("new", "Start a fresh session"),
        ("fork", "Branch the conversation into a new session"),
        ("clone", "Copy the conversation into a new session"),
        ("tree", "Show the session event tree"),
        ("sessions", "List this workspace's sessions"),
        ("session", "Resume a session by id"),
        ("name", "Name this session (/name <text>)"),
        ("rename", "Rename this session (/rename <text>)"),
        ("continue", "Continue the previous session"),
        ("clear", "Clear the conversation view"),
        ("compact", "Summarize and compact the context"),
        ("compact_force", "Compact now, even if within the budget"),
        ("resume", "Continue a previous session"),
        ("harness-resume", "Resume harness plan work"),
        ("wait-resume", "Wait for quota, then resume"),
        ("ingest", "Manage workspace ingestion"),
        ("knowledge", "Query the knowledge base"),
        ("context", "Build a context bundle"),
        (
            "research",
            "Research a topic, local + web per config (/research [topic])",
        ),
        ("bg", "List background processes (/bg stop <id>|all)"),
        ("quit", "Exit LocalPilot"),
    ];

    /// Matching suggestions for `query` (e.g. "/se" or "/"). A query is matched on
    /// the command name after the leading slash; "/" (or an empty query) lists
    /// every command.
    #[must_use]
    fn slash_suggestions(query: &str) -> Vec<SlashSuggestion> {
        let prefix = query.strip_prefix('/').unwrap_or(query);
        Self::SLASH_COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(prefix))
            .map(|(name, description)| SlashSuggestion {
                name: (*name).to_string(),
                description: (*description).to_string(),
            })
            .collect()
    }

    /// Open the slash picker for `query` (e.g. "/se") and populate matching items.
    pub fn open_slash_picker(&mut self, query: String) {
        let items = Self::slash_suggestions(&query);
        self.slash_picker = Some(SlashPicker {
            query,
            items,
            selected: 0,
        });
    }

    /// Close the slash picker and clear its state.
    pub fn close_slash_picker(&mut self) {
        self.slash_picker = None;
    }

    /// Move the picker selection down one item, wrapping at the end.
    pub fn slash_picker_next(&mut self) {
        if let Some(picker) = &mut self.slash_picker {
            if !picker.items.is_empty() {
                picker.selected = (picker.selected + 1) % picker.items.len();
            }
        }
    }

    /// Move the picker selection up one item, wrapping at the start.
    pub fn slash_picker_prev(&mut self) {
        if let Some(picker) = &mut self.slash_picker {
            let len = picker.items.len();
            if len > 0 {
                picker.selected = (picker.selected + len - 1) % len;
            }
        }
    }

    /// Accept the highlighted command: replace the typed `/query` at the cursor
    /// with the full `/<name>` and close the picker. The user can then add
    /// arguments and submit with a second Enter.
    pub fn slash_picker_select(&mut self) {
        if let Some(picker) = self.slash_picker.take() {
            let Some(suggestion) = picker.items.get(picker.selected) else {
                return;
            };
            let command = format!("/{}", suggestion.name);
            // Replace from the slash up to the cursor with the full command.
            if let Some(slash_pos) = self.input[..self.input_cursor].rfind('/') {
                self.input.truncate(slash_pos);
                self.input.push_str(&command);
                self.input_cursor = slash_pos + command.len();
            } else {
                self.insert_input(&command);
            }
        }
    }

    /// Rebuild the picker items for a new `query`, keeping the picker open.
    pub fn slash_picker_update_query(&mut self, query: String) {
        let items = Self::slash_suggestions(&query);
        if let Some(picker) = &mut self.slash_picker {
            picker.query = query;
            picker.items = items;
            picker.selected = 0;
        }
    }

    /// Rebuild the picker from the current input, or close it once the input has
    /// left slash context. Called after each edit while the picker is open.
    pub fn refresh_or_close_slash_picker(&mut self) {
        if self.is_in_slash_context() {
            let cursor = self.normalized_input_cursor();
            self.slash_picker_update_query(self.input[..cursor].to_string());
        } else {
            self.close_slash_picker();
        }
    }

    /// Whether the input is still a slash-command prefix at the cursor: it begins
    /// with '/', the cursor is on the first line, and no whitespace has been typed
    /// yet (a space starts arguments and dismisses the picker).
    #[must_use]
    pub fn is_in_slash_context(&self) -> bool {
        if !self.input.starts_with('/') {
            return false;
        }
        let cursor = self.normalized_input_cursor();
        !self.input[..cursor].contains(char::is_whitespace)
    }

    // --- File mention picker -------------------------------------------------

    /// Replace the workspace file list offered by the `@` picker. Paths are
    /// workspace-relative and forward-slash separated.
    pub fn set_workspace_files(&mut self, files: Vec<String>) {
        self.workspace_files = files;
    }

    /// Cap on the number of files shown in the `@` popup at once.
    const MAX_FILE_SUGGESTIONS: usize = 50;

    /// The `@`-token immediately left of the cursor, as `(at_byte, query)`, when
    /// the input is in mention context. The `@` must start the input or follow
    /// whitespace (so `user@host` does not trigger), with no whitespace between
    /// it and the cursor.
    fn mention_query(&self) -> Option<(usize, &str)> {
        let cursor = self.normalized_input_cursor();
        let before = &self.input[..cursor];
        let at = before.rfind('@')?;
        let preceded_ok = at == 0
            || self.input[..at]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        if !preceded_ok {
            return None;
        }
        let query = &before[at + 1..];
        if query.contains(char::is_whitespace) {
            return None;
        }
        Some((at, query))
    }

    /// Whether the cursor sits inside an `@` file-mention token.
    #[must_use]
    pub fn is_in_mention_context(&self) -> bool {
        self.mention_query().is_some()
    }

    /// Files whose filename (basename) starts with `query`, case-insensitively.
    #[must_use]
    fn file_suggestions(&self, query: &str) -> Vec<FileSuggestion> {
        let needle = query.to_lowercase();
        self.workspace_files
            .iter()
            .filter(|path| {
                let name = path.rsplit('/').next().unwrap_or(path);
                name.to_lowercase().starts_with(&needle)
            })
            .take(Self::MAX_FILE_SUGGESTIONS)
            .map(|path| FileSuggestion { path: path.clone() })
            .collect()
    }

    /// Open the `@` picker for the mention token at the cursor.
    pub fn open_file_picker(&mut self) {
        let Some((_, query)) = self.mention_query() else {
            return;
        };
        let query = query.to_string();
        let items = self.file_suggestions(&query);
        self.file_picker = Some(FilePicker {
            query,
            items,
            selected: 0,
        });
    }

    /// Close the `@` picker and clear its state.
    pub fn close_file_picker(&mut self) {
        self.file_picker = None;
    }

    /// Move the `@` picker selection down one item, wrapping at the end.
    pub fn file_picker_next(&mut self) {
        if let Some(picker) = &mut self.file_picker {
            if !picker.items.is_empty() {
                picker.selected = (picker.selected + 1) % picker.items.len();
            }
        }
    }

    /// Move the `@` picker selection up one item, wrapping at the start.
    pub fn file_picker_prev(&mut self) {
        if let Some(picker) = &mut self.file_picker {
            let len = picker.items.len();
            if len > 0 {
                picker.selected = (picker.selected + len - 1) % len;
            }
        }
    }

    /// Rebuild the `@` picker from the current input, or close it once the input
    /// has left mention context. Called after each edit while the picker is open.
    pub fn refresh_or_close_file_picker(&mut self) {
        match self.mention_query() {
            Some((_, query)) => {
                let query = query.to_string();
                let items = self.file_suggestions(&query);
                if let Some(picker) = &mut self.file_picker {
                    picker.query = query;
                    picker.items = items;
                    picker.selected = 0;
                }
            }
            None => self.close_file_picker(),
        }
    }

    /// Accept the highlighted file: replace the `@<query>` token at the cursor
    /// with the bare relative path and a trailing space, then close the picker.
    pub fn file_picker_select(&mut self) {
        if let Some(picker) = self.file_picker.take() {
            let Some(suggestion) = picker.items.get(picker.selected) else {
                return;
            };
            let cursor = self.normalized_input_cursor();
            let Some((at, _)) = self.mention_query() else {
                return;
            };
            let insert = format!("{} ", suggestion.path);
            self.input.replace_range(at..cursor, &insert);
            self.input_cursor = at + insert.len();
        }
    }

    /// Apply a mapped runtime/UI event to the state.
    ///
    /// Every text payload is scrubbed of terminal-control bytes first: state
    /// text is rendered into the terminal verbatim (both the live region and
    /// the `insert_before` scrollback commits), so a stray ESC/C0 byte — a
    /// degenerating local model's delta, colored tool output, an ANSI-laden
    /// notice — would otherwise reach the terminal raw and can flip its modes
    /// (charset, wrapping, keyboard-protocol state) out from under the TUI.
    pub fn apply(&mut self, event: UiEvent) {
        let event = scrub_event(event);
        match event {
            UiEvent::TextDelta(delta) => {
                let delta = scrub_streaming(&mut self.text_escape_carry, delta);
                let delta = if self.streaming.is_empty() {
                    delta.trim_start_matches(['\r', '\n']).to_string()
                } else {
                    delta
                };
                if !delta.is_empty() {
                    self.streaming.push_str(&delta);
                }
            }
            UiEvent::ReasoningDelta(delta) => {
                let delta = scrub_streaming(&mut self.reasoning_escape_carry, delta);
                // Skip whitespace-only reasoning deltas so the thinking panel
                // does not fill with blank lines.
                if !delta.trim().is_empty() {
                    self.thinking.text.push_str(&delta);
                }
            }
            UiEvent::TurnComplete => {
                // The stream ended: an escape sequence still held back can
                // never complete, so it is dropped with the carry.
                self.text_escape_carry.clear();
                self.reasoning_escape_carry.clear();
                self.flush_streaming_assistant();
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
            UiEvent::Notice(text) => {
                self.flush_streaming_assistant();
                self.transcript.push(TranscriptLine {
                    speaker: "system".to_string(),
                    text,
                });
            }
            UiEvent::RecoveryNotice(text) => {
                // Drop the in-progress (bad) streamed text so the retry starts on a
                // fresh line instead of appending to the discarded output. Both
                // escape carries go with it — the retry is a fresh stream, and a
                // stale unterminated-sequence holdback would swallow its start.
                self.streaming.clear();
                self.text_escape_carry.clear();
                self.reasoning_escape_carry.clear();
                self.transcript.push(TranscriptLine {
                    speaker: "system".to_string(),
                    text,
                });
            }
            UiEvent::PlanUpdated(plan) => self.plan = plan,
            UiEvent::ToolStarted { id, name } => {
                // A running tool is a transient live indicator; only its finished
                // result line is committed to scrollback.
                self.flush_streaming_assistant();
                self.active_tools.push(ActiveTool { id, name });
            }
            UiEvent::ToolFinished {
                id,
                name,
                is_error,
                output,
            } => {
                self.active_tools.retain(|tool| tool.id != id);
                let status = if is_error { "error" } else { "ok" };
                let mut text = format!("{name} {status}");
                let summary = compact_tool_output(&output);
                if !summary.is_empty() {
                    text.push_str(": ");
                    text.push_str(&summary);
                }
                self.flush_streaming_assistant();
                self.transcript.push(TranscriptLine {
                    speaker: "tool".to_string(),
                    text,
                });
            }
            UiEvent::ApprovalRequested(request) => self.approval = Some(request),
            UiEvent::ApprovalResolved => self.approval = None,
            UiEvent::ToggleThinking => self.thinking.visible = !self.thinking.visible,
            UiEvent::ShowMemoryPanel(body) => {
                self.memory_panel.body = body;
                self.memory_panel.visible = true;
            }
            UiEvent::ToggleMemoryPanel => {
                self.memory_panel.visible = !self.memory_panel.visible;
            }
            UiEvent::BackgroundProcesses(processes) => self.background = processes,
            UiEvent::Quit => self.should_quit = true,
        }
    }

    fn flush_streaming_assistant(&mut self) {
        if self.streaming.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.streaming)
            .trim_end_matches(['\r', '\n'])
            .to_string();
        if !text.is_empty() {
            self.transcript.push(TranscriptLine {
                speaker: "assistant".to_string(),
                text,
            });
        }
    }
}

/// Push `input` onto a recall list, skipping a consecutive duplicate and keeping
/// the list bounded to the most recent [`MAX_INPUT_HISTORY`] entries.
/// A compact, human-readable byte size for an attachment placeholder.
fn human_byte_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} B")
    }
}

fn push_capped(history: &mut Vec<RecallEntry>, entry: &RecallEntry) {
    if history.last().is_some_and(|last| last.text == entry.text) {
        return;
    }
    history.push(entry.clone());
    if history.len() > MAX_INPUT_HISTORY {
        history.remove(0);
    }
}

/// Keep at most the most recent [`MAX_INPUT_HISTORY`] entries of a seed list.
fn cap_history(mut history: Vec<RecallEntry>) -> Vec<RecallEntry> {
    let start = history.len().saturating_sub(MAX_INPUT_HISTORY);
    if start > 0 {
        history.drain(0..start);
    }
    history
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
    ToolStarted {
        id: String,
        name: String,
    },
    ToolFinished {
        id: String,
        name: String,
        is_error: bool,
        output: String,
    },
    ApprovalRequested(ApprovalRequest),
    ApprovalResolved,
    ToggleThinking,
    /// Show the "memories used this turn" inspector panel with the given rendered
    /// body (the host computes it from the event log + LocalMind provenance).
    ShowMemoryPanel(String),
    /// Toggle the inspector panel's visibility.
    ToggleMemoryPanel,
    /// Replace the set of background processes shown in the status line.
    BackgroundProcesses(Vec<BackgroundProcess>),
    Quit,
}

/// The longest incomplete trailing escape sequence a streaming scrub holds
/// back for the next delta. Real sequences (SGR colors, cursor moves) are a
/// handful of bytes. Past this bound the unterminated sequence stops being
/// carried and is dropped to the end of the current payload — faithful OSC
/// semantics (a real terminal would swallow that content too) — so a
/// malformed "sequence" can never hold the visible stream hostage across
/// deltas.
const ESCAPE_CARRY_MAX: usize = 64;

/// Remove terminal-control bytes from one complete text payload. `\n` and
/// `\t` are legitimate layout; `\r\n`/`\r` are normalized to `\n` (a bare
/// `\r` written to the terminal overwrites the current row); every other C0
/// byte, DEL, and the C1 range is dropped — ESC in particular, since an
/// escape sequence reaching the terminal raw can flip charset/wrap/
/// keyboard-protocol modes the TUI depends on. Whole ANSI CSI/OSC sequences
/// are swallowed so colored text degrades to its plain content. Clean text
/// (the overwhelmingly common case) is returned unchanged without allocating.
///
/// Streaming deltas go through [`AppState`]'s carry-aware path instead, so a
/// sequence split across two deltas is still swallowed whole.
pub fn scrub_text(text: String) -> String {
    match scrub_core(&text) {
        None => text,
        Some((mut clean, tail)) => {
            // A complete payload has no next delta: a truncated trailing
            // escape is dropped, but a trailing bare `\r` is still a newline.
            if tail == "\r" {
                clean.push('\n');
            }
            clean
        }
    }
}

/// Scrub one streaming delta, holding an incomplete trailing escape sequence
/// (or a bare `\r` that may be the CR half of a split CRLF) in `carry` so the
/// next delta can complete it — otherwise a color code split across two
/// deltas would leak its printable tail (`[31m`) into the stream.
fn scrub_streaming(carry: &mut String, delta: String) -> String {
    let input = if carry.is_empty() {
        delta
    } else {
        let mut held = std::mem::take(carry);
        held.push_str(&delta);
        held
    };
    match scrub_core(&input) {
        // A non-empty carry always contains a dirty byte, so a `None` fast
        // path here implies the carry was empty and `input` is the raw delta.
        None => input,
        Some((clean, tail)) => {
            *carry = tail;
            clean
        }
    }
}

/// Core scrubber. Returns `None` when the input is already clean (no
/// allocation needed), otherwise the cleaned text plus a held-back tail: an
/// incomplete trailing escape sequence (bounded by [`ESCAPE_CARRY_MAX`]) or a
/// trailing bare `\r`, either of which a following streaming delta may
/// complete.
fn scrub_core(text: &str) -> Option<(String, String)> {
    // `is_ascii_control` covers 0x00..=0x1F and DEL (0x7F), so `\r` and ESC
    // are both "dirty"; `\n`/`\t` are the allowed exceptions. C1 controls
    // (U+0080..=U+009F) are dirty too: U+009B is a single-codepoint CSI that
    // xterm/VTE-family terminals execute even when it arrives as UTF-8 text.
    let dirty = |c: char| {
        (c.is_ascii_control() && c != '\n' && c != '\t') || ('\u{80}'..='\u{9f}').contains(&c)
    };
    if !text.chars().any(dirty) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut tail = String::new();
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '\r' => match chars.peek() {
                Some((_, '\n')) => {
                    chars.next();
                    out.push('\n');
                }
                Some(_) => out.push('\n'),
                // Possibly the CR half of a CRLF split across deltas.
                None => tail.push('\r'),
            },
            '\n' | '\t' => out.push(c),
            '\u{1b}' => {
                if !consume_escape_sequence(&mut chars) {
                    // The sequence runs off the end of this payload: hold it
                    // (bounded) so the next delta can complete it.
                    let rest = &text[at..];
                    if rest.len() <= ESCAPE_CARRY_MAX {
                        tail.push_str(rest);
                    }
                    break;
                }
            }
            c if c.is_ascii_control() || ('\u{80}'..='\u{9f}').contains(&c) => {}
            c => out.push(c),
        }
    }
    Some((out, tail))
}

/// Consume the remainder of an escape sequence whose ESC byte was just seen:
/// CSI (`ESC [ … final-byte`), OSC (`ESC ] … BEL` or `ESC ] … ESC \`), or a
/// two-character escape. Returns `false` when the sequence is cut off by
/// end-of-input (the caller may carry it into the next delta); never
/// over-consumes past a sequence terminator.
fn consume_escape_sequence(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) -> bool {
    match chars.peek().map(|&(_, c)| c) {
        Some('[') => {
            chars.next();
            for (_, c) in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    return true;
                }
            }
            false
        }
        Some(']') => {
            chars.next();
            while let Some((_, c)) = chars.next() {
                if c == '\u{7}' {
                    return true;
                }
                if c == '\u{1b}' {
                    if matches!(chars.peek(), Some((_, '\\'))) {
                        chars.next();
                    }
                    return true;
                }
            }
            false
        }
        Some(_) => {
            chars.next();
            true
        }
        None => false,
    }
}

/// Scrub every externally sourced text payload of a [`UiEvent`] before it can
/// enter rendered state. Enumerated per variant so a new text-bearing event
/// fails review here rather than silently bypassing the scrub.
fn scrub_event(event: UiEvent) -> UiEvent {
    match event {
        UiEvent::UserMessage(text) => UiEvent::UserMessage(scrub_text(text)),
        UiEvent::Notice(text) => UiEvent::Notice(scrub_text(text)),
        UiEvent::RecoveryNotice(text) => UiEvent::RecoveryNotice(scrub_text(text)),
        UiEvent::ShowMemoryPanel(body) => UiEvent::ShowMemoryPanel(scrub_text(body)),
        UiEvent::QuotaPaused { reset } => UiEvent::QuotaPaused {
            reset: scrub_text(reset),
        },
        UiEvent::PlanUpdated(plan) => UiEvent::PlanUpdated(
            plan.into_iter()
                .map(|item| PlanItem {
                    title: scrub_text(item.title),
                    status: scrub_text(item.status),
                })
                .collect(),
        ),
        UiEvent::ToolStarted { id, name } => UiEvent::ToolStarted {
            id,
            name: scrub_text(name),
        },
        UiEvent::ToolFinished {
            id,
            name,
            is_error,
            output,
        } => UiEvent::ToolFinished {
            id,
            name: scrub_text(name),
            is_error,
            output: scrub_text(output),
        },
        UiEvent::ApprovalRequested(request) => UiEvent::ApprovalRequested(ApprovalRequest {
            tool: scrub_text(request.tool),
            target: scrub_text(request.target),
            risk_class: scrub_text(request.risk_class),
        }),
        UiEvent::BackgroundProcesses(processes) => UiEvent::BackgroundProcesses(
            processes
                .into_iter()
                .map(|process| BackgroundProcess {
                    id: process.id,
                    command: scrub_text(process.command),
                    alive: process.alive,
                })
                .collect(),
        ),
        // The streaming deltas are scrubbed in their `apply` arms through the
        // per-stream escape carries, so a sequence split across two deltas is
        // still swallowed whole.
        event @ (UiEvent::TextDelta(_)
        | UiEvent::ReasoningDelta(_)
        | UiEvent::Usage { .. }
        | UiEvent::ContextUsage { .. }
        | UiEvent::TurnComplete
        | UiEvent::ApprovalResolved
        | UiEvent::ToggleThinking
        | UiEvent::ToggleMemoryPanel
        | UiEvent::Quit) => event,
    }
}

fn compact_tool_output(output: &str) -> String {
    const MAX_CHARS: usize = 96;

    let body = output
        .split_once("\noutput:\n")
        .map_or(output, |(_, body)| body);
    let mut summary = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if summary.chars().count() <= MAX_CHARS {
        return summary;
    }

    summary = summary.chars().take(MAX_CHARS - 3).collect();
    summary.push_str("...");
    summary
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
                session_name: None,
                update: None,
            },
            Mode::Agent,
            Profile::Default,
        )
    }

    #[test]
    fn seeding_scopes_recall_to_the_project_and_the_toggle_exposes_all() {
        let mut s = state();
        s.seed_input_history(
            vec![
                RecallEntry::text_only("proj-a"),
                RecallEntry::text_only("proj-b"),
            ],
            vec![
                RecallEntry::text_only("proj-a"),
                RecallEntry::text_only("other-1"),
                RecallEntry::text_only("proj-b"),
            ],
        );

        // Recall starts scoped to this project: Up walks only the project's seed.
        assert!(s.recall_previous_input());
        assert_eq!(s.input, "proj-b");
        assert!(s.recall_previous_input());
        assert_eq!(s.input, "proj-a");
        // No more project entries to recall past the oldest.
        assert!(s.recall_previous_input());
        assert_eq!(s.input, "proj-a");

        // Toggling to all projects exposes the entry the project filter excluded.
        s.input.clear();
        s.input_cursor = 0;
        assert!(s.toggle_history_scope());
        assert!(s.recall_previous_input());
        assert_eq!(s.input, "proj-b");
        assert!(s.recall_previous_input());
        assert_eq!(s.input, "other-1");

        // Toggling back returns to the project scope only.
        assert!(!s.toggle_history_scope());
        s.input.clear();
        s.input_cursor = 0;
        assert!(s.recall_previous_input());
        assert_eq!(s.input, "proj-b");
    }

    #[test]
    fn session_submissions_survive_a_view_all_toggle() {
        let mut s = state();
        s.seed_input_history(
            vec![RecallEntry::text_only("seeded")],
            vec![RecallEntry::text_only("seeded")],
        );
        s.input = "typed this session".to_string();
        s.input_cursor = s.input.len();
        let _ = s.take_input_for_submit();

        // The just-submitted prompt is recallable after switching scope twice.
        s.toggle_history_scope();
        s.toggle_history_scope();
        assert!(s.recall_previous_input());
        assert_eq!(s.input, "typed this session");
    }

    #[test]
    fn slash_picker_filters_to_real_commands() {
        let mut s = state();
        s.open_slash_picker("/se".to_string());
        let picker = s.slash_picker.as_ref().expect("picker open");
        let names: Vec<&str> = picker.items.iter().map(|i| i.name.as_str()).collect();
        // Preserves the table order and only keeps the "se" prefix.
        assert_eq!(names, ["sessions", "session"]);
        assert!(picker.items.iter().all(|i| !i.description.is_empty()));
    }

    #[test]
    fn slash_picker_offers_research() {
        let mut s = state();
        s.open_slash_picker("/re".to_string());
        let picker = s.slash_picker.as_ref().expect("picker open");
        let names: Vec<&str> = picker.items.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"research"), "research missing: {names:?}");
    }

    #[test]
    fn slash_picker_lists_every_command_for_a_bare_slash() {
        let mut s = state();
        s.open_slash_picker("/".to_string());
        let picker = s.slash_picker.as_ref().expect("picker open");
        assert_eq!(picker.items.len(), AppState::SLASH_COMMANDS.len());
    }

    #[test]
    fn slash_picker_select_inserts_the_full_command() {
        let mut s = state();
        s.input = "/se".to_string();
        s.input_cursor = s.input.len();
        s.open_slash_picker("/se".to_string());
        s.slash_picker_next(); // sessions -> session
        s.slash_picker_select();
        assert!(s.slash_picker.is_none());
        assert_eq!(s.input, "/session");
        assert_eq!(s.input_cursor, "/session".len());
    }

    #[test]
    fn slash_picker_prev_wraps_to_the_last_item() {
        let mut s = state();
        s.open_slash_picker("/".to_string());
        s.slash_picker_prev();
        let picker = s.slash_picker.as_ref().expect("picker open");
        assert_eq!(picker.selected, picker.items.len() - 1);
    }

    #[test]
    fn typing_a_space_leaves_slash_context_and_closes_the_picker() {
        let mut s = state();
        s.input = "/search".to_string();
        s.input_cursor = s.input.len();
        s.open_slash_picker("/search".to_string());
        s.insert_input(" ");
        s.refresh_or_close_slash_picker();
        assert!(s.slash_picker.is_none());
    }

    fn state_with_files() -> AppState {
        let mut s = state();
        // Sorted as the host injects it (sorted by full path).
        s.set_workspace_files(vec![
            "Backlog.md".to_string(),
            "README.md".to_string(),
            "docs/banner.txt".to_string(),
            "src/build.rs".to_string(),
            "src/main.rs".to_string(),
        ]);
        s
    }

    #[test]
    fn file_picker_filters_by_filename_prefix_case_insensitively() {
        let mut s = state_with_files();
        s.input = "@b".to_string();
        s.input_cursor = s.input.len();
        s.open_file_picker();
        let picker = s.file_picker.as_ref().expect("picker open");
        let paths: Vec<&str> = picker.items.iter().map(|i| i.path.as_str()).collect();
        // Basenames starting with "b" (any case): build.rs, Backlog.md, banner.txt.
        // README.md and main.rs are excluded (no leading "b" in the filename).
        assert_eq!(paths, ["Backlog.md", "docs/banner.txt", "src/build.rs"]);
    }

    #[test]
    fn file_picker_select_inserts_the_bare_path_and_keeps_the_suffix() {
        let mut s = state_with_files();
        s.input = "look at @ma now".to_string();
        s.input_cursor = "look at @ma".len();
        s.open_file_picker();
        s.file_picker_select();
        assert!(s.file_picker.is_none());
        assert_eq!(s.input, "look at src/main.rs  now");
        assert_eq!(s.input_cursor, "look at src/main.rs ".len());
    }

    #[test]
    fn file_picker_closes_when_whitespace_leaves_mention_context() {
        let mut s = state_with_files();
        s.input = "@b".to_string();
        s.input_cursor = s.input.len();
        s.open_file_picker();
        assert!(s.file_picker.is_some());
        s.insert_input(" ");
        s.refresh_or_close_file_picker();
        assert!(s.file_picker.is_none());
    }

    #[test]
    fn an_at_after_non_whitespace_is_not_mention_context() {
        let mut s = state_with_files();
        s.input = "user@b".to_string();
        s.input_cursor = s.input.len();
        assert!(!s.is_in_mention_context());
        s.open_file_picker();
        assert!(s.file_picker.is_none());
    }

    #[test]
    fn a_bare_at_lists_all_files() {
        let mut s = state_with_files();
        s.input = "@".to_string();
        s.input_cursor = s.input.len();
        s.open_file_picker();
        let picker = s.file_picker.as_ref().expect("picker open");
        assert_eq!(picker.items.len(), 5);
    }

    #[test]
    fn the_background_event_replaces_the_tracked_set() {
        let mut s = state();
        s.apply(UiEvent::BackgroundProcesses(vec![BackgroundProcess {
            id: "bg-1".to_string(),
            command: "npm run dev".to_string(),
            alive: true,
        }]));
        assert_eq!(s.background.len(), 1);
        assert_eq!(s.background[0].id, "bg-1");

        // A later push replaces the set rather than appending.
        s.apply(UiEvent::BackgroundProcesses(Vec::new()));
        assert!(s.background.is_empty());
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
    fn a_recalled_prompt_expands_its_paste_again_instead_of_the_placeholder() {
        // LocalHub#19: recalling a prompt containing a collapsed paste used to
        // send the literal placeholder text to the model.
        let mut state = state();
        let body = "row 1\nrow 2\nrow 3".to_string();
        let placeholder = state.register_paste(body.clone());
        state.input = format!("check {placeholder}");
        let first = state.take_input_for_submit();
        assert_eq!(first.prompt, format!("check {body}"));
        assert_eq!(first.pastes.len(), 1);

        // Recall the submitted prompt: the visible form stays compact...
        assert!(state.recall_previous_input());
        assert_eq!(state.input, format!("check {placeholder}"));
        // ...and submitting it again sends the pasted content, not the tag.
        let again = state.take_input_for_submit();
        assert_eq!(again.prompt, format!("check {body}"));
    }

    #[test]
    fn a_seeded_recall_entry_expands_its_persisted_paste() {
        // The cross-session path: the durable history carries the mapping and
        // the seed rehydrates it.
        let mut state = state();
        let entry = RecallEntry {
            text: "explain [2 pasted rows #1]".to_string(),
            pastes: vec![Paste {
                placeholder: "[2 pasted rows #1]".to_string(),
                content: "alpha\nbeta".to_string(),
            }],
        };
        state.seed_input_history(vec![entry.clone()], vec![entry]);

        assert!(state.recall_previous_input());
        assert_eq!(state.input, "explain [2 pasted rows #1]");
        let submitted = state.take_input_for_submit();
        assert_eq!(submitted.prompt, "explain alpha\nbeta");
    }

    #[test]
    fn paste_numbering_never_restarts_within_a_session() {
        // Placeholder numbers are session-monotonic so a recalled prompt's
        // placeholder can never collide with a fresh paste's.
        let mut state = state();
        let first = state.register_paste("a\nb".to_string());
        state.input = first.clone();
        let _ = state.take_input_for_submit();

        let second = state.register_paste("c\nd".to_string());
        assert!(first.contains("#1"));
        assert!(second.contains("#2"), "numbering continues: {second}");
    }

    #[test]
    fn browsing_history_and_returning_to_the_draft_keeps_its_pastes() {
        let mut state = state();
        // A submitted prompt to browse to.
        state.input = "earlier".to_string();
        let _ = state.take_input_for_submit();

        // A draft with a live paste, interrupted by history browsing.
        let placeholder = state.register_paste("x\ny".to_string());
        state.input = format!("draft {placeholder}");
        assert!(state.recall_previous_input());
        assert_eq!(state.input, "earlier");
        assert!(state.recall_next_input());

        // Back on the draft, the paste still expands.
        assert_eq!(state.input, format!("draft {placeholder}"));
        let submitted = state.take_input_for_submit();
        assert_eq!(submitted.prompt, "draft x\ny");
    }

    #[test]
    fn a_registered_image_yields_a_placeholder_and_is_drained_on_take() {
        let mut state = state();
        let placeholder = state.register_image("image/png", "aGVsbG8=", 2048);
        assert!(placeholder.contains("#1"));
        assert!(placeholder.contains("PNG"));
        assert!(placeholder.contains("2 KB"));
        assert_eq!(state.images.len(), 1);

        let taken = state.take_images();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].media_type, "image/png");
        assert_eq!(taken[0].data, "aGVsbG8=");
        assert!(state.images.is_empty());
    }

    #[test]
    fn control_bytes_never_reach_streamed_state() {
        // A degenerating local model can emit raw ESC/C0 bytes in its deltas;
        // rendered verbatim they would flip terminal modes under the TUI.
        let mut state = state();
        state.apply(UiEvent::TextDelta(
            "safe \u{1b}[31mred\u{1b}[0m text\u{7} bell\u{8} bs".to_string(),
        ));
        assert_eq!(state.streaming, "safe red text bell bs");
    }

    #[test]
    fn ansi_sequences_are_scrubbed_from_tool_output_lines() {
        let mut state = state();
        state.apply(UiEvent::ToolFinished {
            id: "call_1".to_string(),
            name: "run_shell".to_string(),
            is_error: true,
            output: "\u{1b}[1;31merror\u{1b}[0m: it broke".to_string(),
        });
        assert_eq!(
            state.transcript.last().unwrap().text,
            "run_shell error: error: it broke"
        );
    }

    #[test]
    fn osc_sequences_and_bare_carriage_returns_are_scrubbed_from_notices() {
        let mut state = state();
        state.apply(UiEvent::Notice(
            "\u{1b}]0;title\u{7}line one\rline two".to_string(),
        ));
        assert_eq!(state.transcript.last().unwrap().text, "line one\nline two");
    }

    #[test]
    fn c1_controls_are_scrubbed() {
        // U+009B is a one-codepoint CSI that VTE-family terminals execute even
        // from UTF-8 text; it must never reach rendered state.
        let mut state = state();
        state.apply(UiEvent::TextDelta("a\u{9b}31mb\u{85}c".to_string()));
        assert_eq!(state.streaming, "a31mbc");
    }

    #[test]
    fn an_ansi_sequence_split_across_deltas_leaves_no_residue() {
        // Realistic SSE shape: a color code arrives cut mid-sequence. The
        // carry holds the partial tail so the next delta completes it whole.
        let mut state = state();
        state.apply(UiEvent::TextDelta("red \u{1b}[3".to_string()));
        state.apply(UiEvent::TextDelta("1mtext\u{1b}[0m end".to_string()));
        assert_eq!(state.streaming, "red text end");
    }

    #[test]
    fn a_crlf_split_across_deltas_yields_one_newline() {
        let mut state = state();
        state.apply(UiEvent::TextDelta("line one\r".to_string()));
        state.apply(UiEvent::TextDelta("\nline two".to_string()));
        assert_eq!(state.streaming, "line one\nline two");
    }

    #[test]
    fn the_escape_carry_is_dropped_when_the_stream_ends() {
        let mut state = state();
        state.apply(UiEvent::TextDelta("answer\u{1b}[3".to_string()));
        state.apply(UiEvent::TurnComplete);
        assert_eq!(state.transcript.last().unwrap().text, "answer");

        // A fresh turn must not inherit the stale holdback.
        state.apply(UiEvent::TextDelta("next".to_string()));
        assert_eq!(state.streaming, "next");
    }

    #[test]
    fn a_runaway_incomplete_sequence_is_dropped_not_carried() {
        // An "OSC" that never terminates must not hold the stream hostage:
        // past the carry bound it is discarded and the stream continues.
        let mut state = state();
        let runaway = format!("start\u{1b}]0;{}", "t".repeat(200));
        state.apply(UiEvent::TextDelta(runaway));
        state.apply(UiEvent::TextDelta("visible".to_string()));
        assert_eq!(state.streaming, "startvisible");
    }

    #[test]
    fn clean_text_passes_the_scrub_unchanged() {
        let mut state = state();
        state.apply(UiEvent::TextDelta("plain\ttext\nwith café 😀".to_string()));
        assert_eq!(state.streaming, "plain\ttext\nwith café 😀");
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
    fn leading_blank_streaming_lines_are_dropped() {
        let mut state = state();
        state.apply(UiEvent::TextDelta("\n\nThe answer".to_string()));
        state.apply(UiEvent::TurnComplete);

        assert_eq!(state.transcript.len(), 1);
        assert_eq!(state.transcript[0].text, "The answer");
    }

    #[test]
    fn trailing_blank_streaming_lines_are_dropped_on_completion() {
        let mut state = state();
        state.apply(UiEvent::TextDelta("The answer\n\n".to_string()));
        state.apply(UiEvent::TurnComplete);

        assert_eq!(state.transcript.len(), 1);
        assert_eq!(state.transcript[0].text, "The answer");
    }

    #[test]
    fn tool_events_are_inserted_after_the_assistant_text_that_preceded_them() {
        let mut state = state();
        state.apply(UiEvent::TextDelta("First chunk\n\n".to_string()));
        state.apply(UiEvent::ToolStarted {
            id: "call_1".to_string(),
            name: "run_shell".to_string(),
        });
        state.apply(UiEvent::ToolFinished {
            id: "call_1".to_string(),
            name: "run_shell".to_string(),
            is_error: false,
            output: "tool: run_shell\nstatus: ok\noutput:\nok".to_string(),
        });
        state.apply(UiEvent::TextDelta("Second chunk".to_string()));
        state.apply(UiEvent::TurnComplete);

        assert_eq!(state.transcript.len(), 3);
        assert_eq!(state.transcript[0].speaker, "assistant");
        assert_eq!(state.transcript[0].text, "First chunk");
        assert_eq!(state.transcript[1].speaker, "tool");
        assert_eq!(state.transcript[1].text, "run_shell ok: ok");
        assert_eq!(state.transcript[2].speaker, "assistant");
        assert_eq!(state.transcript[2].text, "Second chunk");
    }

    #[test]
    fn tool_status_is_kept_to_one_compact_line() {
        let mut state = state();
        state.apply(UiEvent::ToolStarted {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
        });
        state.apply(UiEvent::ToolFinished {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            is_error: false,
            output: "tool: read_file\nstatus: ok\noutput:\nfirst line\n\nsecond line with detail"
                .to_string(),
        });

        assert_eq!(state.transcript.len(), 1);
        assert_eq!(state.transcript[0].speaker, "tool");
        assert_eq!(
            state.transcript[0].text,
            "read_file ok: first line second line with detail"
        );
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
