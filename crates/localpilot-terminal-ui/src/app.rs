use std::collections::BTreeMap;

use crate::presentation::semantic_ranges;
use crate::{
    sanitize_text, ActivityState, Editor, ItemId, ItemKind, SemanticRole, StyledRange, TextStyle,
    Theme, Timeline,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TabId {
    Session,
    Plan,
    Activity,
    Settings,
}

impl TabId {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Session => "Session",
            Self::Plan => "Plan",
            Self::Activity => "Activity",
            Self::Settings => "Settings",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub version: String,
    pub provider: String,
    pub model: String,
    pub workspace: String,
    pub session_id: String,
    pub session_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSupport {
    Color,
    NoColor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardSupport {
    Basic,
    Enhanced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub color: ColorSupport,
    pub mouse_capture: bool,
    pub synchronized_output: bool,
    pub keyboard: KeyboardSupport,
    pub clipboard_write: bool,
}

impl Default for TerminalCapabilities {
    fn default() -> Self {
        Self {
            color: ColorSupport::Color,
            mouse_capture: false,
            synchronized_output: false,
            keyboard: KeyboardSupport::Basic,
            clipboard_write: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Composer,
    Timeline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkState {
    Idle,
    Busy { cancellation_requested: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    CancelOrExit,
    Insert(String),
    Backspace,
    Delete,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    Submit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppCommand {
    None,
    CancelWork,
    Copy(String),
    Exit,
    Submit(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryState {
    Healthy,
    Recovering,
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopState {
    Done,
    Cancelled,
    Degraded,
    ProviderError,
    BudgetExceeded,
    NoProgress,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeUpdate {
    Text(String),
    Reasoning(String),
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
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    ContextUsage {
        used: usize,
        limit: usize,
    },
    Warning(String),
    Plan(Vec<PlanEntry>),
    QuotaPaused(String),
    Recovery(RecoveryState),
    ToolStuck {
        name: String,
        count: u32,
    },
    Stopped(StopState),
}

#[derive(Debug, Clone)]
pub struct AppModel {
    pub header: Header,
    pub capabilities: TerminalCapabilities,
    pub theme: Theme,
    pub tabs: Vec<TabId>,
    pub active_tab: TabId,
    pub timeline: Timeline,
    pub editor: Editor,
    pub focus: Focus,
    pub work: WorkState,
    pub exit_armed: bool,
    pub exit_requested: bool,
    pub plan: Vec<PlanEntry>,
    pub usage: Option<(u64, u64)>,
    pub context_usage: Option<(usize, usize)>,
    active_assistant: Option<ItemId>,
    active_reasoning: Option<ItemId>,
    active_tools: BTreeMap<String, ItemId>,
}

impl AppModel {
    #[must_use]
    pub fn new(header: Header, capabilities: TerminalCapabilities) -> Self {
        let header = Header {
            version: sanitize_text(&header.version),
            provider: sanitize_text(&header.provider),
            model: sanitize_text(&header.model),
            workspace: sanitize_text(&header.workspace),
            session_id: sanitize_text(&header.session_id),
            session_name: header.session_name.map(|name| sanitize_text(&name)),
        };
        Self {
            header,
            capabilities,
            theme: Theme::Default,
            tabs: vec![TabId::Session],
            active_tab: TabId::Session,
            timeline: Timeline::new(),
            editor: Editor::default(),
            focus: Focus::Composer,
            work: WorkState::Idle,
            exit_armed: false,
            exit_requested: false,
            plan: Vec::new(),
            usage: None,
            context_usage: None,
            active_assistant: None,
            active_reasoning: None,
            active_tools: BTreeMap::new(),
        }
    }

    /// Installs only tabs backed by a real LocalPilot surface, preserving the
    /// caller's order and ignoring duplicates. Session is the safe fallback.
    pub fn set_tabs(&mut self, tabs: impl IntoIterator<Item = TabId>) {
        let mut unique = Vec::new();
        for tab in tabs {
            if !unique.contains(&tab) {
                unique.push(tab);
            }
        }
        if unique.is_empty() {
            unique.push(TabId::Session);
        }
        if !unique.contains(&self.active_tab) {
            self.active_tab = unique[0];
        }
        self.tabs = unique;
    }

    pub fn handle_input(&mut self, action: InputAction, editor_width: u16) -> AppCommand {
        if !matches!(action, InputAction::CancelOrExit) {
            self.exit_armed = false;
        }
        match action {
            InputAction::CancelOrExit => self.cancel_or_exit(),
            InputAction::Insert(text) if self.focus == Focus::Composer => {
                self.editor.insert(&text);
                AppCommand::None
            }
            InputAction::Backspace if self.focus == Focus::Composer => {
                self.editor.backspace();
                AppCommand::None
            }
            InputAction::Delete if self.focus == Focus::Composer => {
                self.editor.delete();
                AppCommand::None
            }
            InputAction::MoveLeft if self.focus == Focus::Composer => {
                self.editor.move_left();
                AppCommand::None
            }
            InputAction::MoveRight if self.focus == Focus::Composer => {
                self.editor.move_right();
                AppCommand::None
            }
            InputAction::MoveUp if self.focus == Focus::Composer => {
                self.editor.up_or_history(editor_width);
                AppCommand::None
            }
            InputAction::MoveDown if self.focus == Focus::Composer => {
                self.editor.down_or_history(editor_width);
                AppCommand::None
            }
            InputAction::Submit if self.focus == Focus::Composer => self
                .editor
                .submit()
                .map_or(AppCommand::None, AppCommand::Submit),
            InputAction::Insert(_)
            | InputAction::Backspace
            | InputAction::Delete
            | InputAction::MoveLeft
            | InputAction::MoveRight
            | InputAction::MoveUp
            | InputAction::MoveDown
            | InputAction::Submit => AppCommand::None,
        }
    }

    pub fn apply_runtime(&mut self, update: RuntimeUpdate) {
        match update {
            RuntimeUpdate::Text(text) => {
                if !self.append_active(self.active_assistant, &text) {
                    self.active_assistant = self.timeline.push(ItemKind::Assistant, text);
                }
            }
            RuntimeUpdate::Reasoning(text) => {
                if !self.append_active(self.active_reasoning, &text) {
                    self.active_reasoning = self.timeline.push(ItemKind::Reasoning, text);
                }
            }
            RuntimeUpdate::ToolStarted { id, name } => {
                if let Some(item) = self.timeline.push(ItemKind::Tool, name) {
                    let _ = self
                        .timeline
                        .set_activity(item, Some(ActivityState::Running));
                    self.active_tools.insert(id, item);
                }
            }
            RuntimeUpdate::ToolFinished {
                id,
                name,
                is_error,
                output,
            } => {
                let suffix = if is_error { " failed" } else { " completed" };
                let text = if output.is_empty() {
                    suffix.to_string()
                } else {
                    format!("{suffix}\n{output}")
                };
                if let Some(item) = self.active_tools.remove(&id) {
                    let _ = self.timeline.append_text(item, &text);
                    let activity = if is_error {
                        ActivityState::Error
                    } else {
                        ActivityState::Success
                    };
                    let _ = self.timeline.set_activity(item, Some(activity));
                    self.style_activity(item, activity);
                } else if let Some(item) =
                    self.timeline.push(ItemKind::Tool, format!("{name}{text}"))
                {
                    let activity = if is_error {
                        ActivityState::Error
                    } else {
                        ActivityState::Success
                    };
                    let _ = self.timeline.set_activity(item, Some(activity));
                    self.style_activity(item, activity);
                }
            }
            RuntimeUpdate::Usage {
                input_tokens,
                output_tokens,
            } => self.usage = Some((input_tokens, output_tokens)),
            RuntimeUpdate::ContextUsage { used, limit } => {
                self.context_usage = Some((used, limit));
            }
            RuntimeUpdate::Warning(message) | RuntimeUpdate::QuotaPaused(message) => {
                let _ = self.timeline.push(ItemKind::Notice, message);
            }
            RuntimeUpdate::Plan(plan) => {
                self.plan = plan
                    .into_iter()
                    .map(|entry| PlanEntry {
                        title: sanitize_text(&entry.title),
                        status: sanitize_text(&entry.status),
                    })
                    .collect();
            }
            RuntimeUpdate::Recovery(state) => {
                if state != RecoveryState::Healthy {
                    let _ = self
                        .timeline
                        .push(ItemKind::Notice, format!("recovery: {state:?}"));
                }
            }
            RuntimeUpdate::ToolStuck { name, count } => {
                let _ = self.timeline.push(
                    ItemKind::Notice,
                    format!("tool {name} stopped after {count} repeated failures"),
                );
            }
            RuntimeUpdate::Stopped(_) => {
                self.style_active_transcript();
                self.work = WorkState::Idle;
                self.active_assistant = None;
                self.active_reasoning = None;
                self.active_tools.clear();
            }
        }
    }

    pub fn begin_work(&mut self) {
        self.work = WorkState::Busy {
            cancellation_requested: false,
        };
        self.active_assistant = None;
        self.active_reasoning = None;
        self.active_tools.clear();
    }

    fn append_active(&mut self, active: Option<ItemId>, text: &str) -> bool {
        active.is_some_and(|id| self.timeline.append_text(id, text))
    }

    fn style_active_transcript(&mut self) {
        for id in [self.active_assistant, self.active_reasoning]
            .into_iter()
            .flatten()
        {
            let Some(item) = self.timeline.item(id) else {
                continue;
            };
            let styles = semantic_ranges(item.kind, &item.text);
            let _ = self.timeline.set_styles(id, styles);
        }
    }

    fn style_activity(&mut self, id: ItemId, activity: ActivityState) {
        let Some(end_byte) = self.timeline.item(id).map(|item| item.text.len()) else {
            return;
        };
        let role = match activity {
            ActivityState::Running => SemanticRole::Tool,
            ActivityState::Success => SemanticRole::Success,
            ActivityState::Error => SemanticRole::Error,
        };
        let styles = (end_byte > 0)
            .then_some(StyledRange {
                start_byte: 0,
                end_byte,
                style: TextStyle::new(role),
            })
            .into_iter()
            .collect();
        let _ = self.timeline.set_styles(id, styles);
    }

    fn cancel_or_exit(&mut self) -> AppCommand {
        if self.exit_armed {
            self.exit_requested = true;
            return AppCommand::Exit;
        }

        self.exit_armed = true;
        if let Some(text) = self.timeline.selected_text() {
            return AppCommand::Copy(text);
        }

        match self.work {
            WorkState::Idle => AppCommand::None,
            WorkState::Busy {
                cancellation_requested: false,
            } => {
                self.work = WorkState::Busy {
                    cancellation_requested: true,
                };
                AppCommand::CancelWork
            }
            WorkState::Busy {
                cancellation_requested: true,
            } => AppCommand::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> AppModel {
        AppModel::new(
            Header {
                version: "0".to_string(),
                provider: "test".to_string(),
                model: "test-model".to_string(),
                workspace: "workspace".to_string(),
                session_id: "session".to_string(),
                session_name: None,
            },
            TerminalCapabilities::default(),
        )
    }

    #[test]
    fn ctrl_c_arms_then_exits_when_idle_even_with_a_draft() {
        let mut app = model();
        app.editor.insert("unfinished draft");
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::None
        );
        assert!(app.exit_armed);
        assert!(!app.exit_requested);
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Exit
        );
        assert!(app.exit_requested);
    }

    #[test]
    fn ctrl_c_cancels_then_exits_while_cancellation_is_pending() {
        let mut app = model();
        app.begin_work();
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::CancelWork
        );
        assert_eq!(
            app.work,
            WorkState::Busy {
                cancellation_requested: true
            }
        );
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Exit
        );
    }

    #[test]
    fn ctrl_c_copies_a_selection_then_a_second_press_exits() {
        let mut app = model();
        let id = app
            .timeline
            .push(ItemKind::Assistant, "copy this")
            .expect("timeline id");
        app.timeline.start_selection(crate::ContentPoint {
            item_id: id,
            byte: 0,
        });
        app.timeline.extend_selection(crate::ContentPoint {
            item_id: id,
            byte: 4,
        });

        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Copy("copy".to_string())
        );
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Exit
        );
    }

    #[test]
    fn other_input_disarms_a_pending_exit() {
        let mut app = model();
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::None
        );
        assert_eq!(
            app.handle_input(InputAction::Insert("x".to_string()), 80),
            AppCommand::None
        );
        assert!(!app.exit_armed);
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::None
        );
    }

    #[test]
    fn streamed_text_keeps_one_stable_timeline_id() {
        let mut app = model();
        app.begin_work();
        app.apply_runtime(RuntimeUpdate::Text("hello ".to_string()));
        let id = app.timeline.items()[0].id;
        app.apply_runtime(RuntimeUpdate::Text("world".to_string()));
        assert_eq!(app.timeline.items().len(), 1);
        assert_eq!(app.timeline.items()[0].id, id);
        assert_eq!(app.timeline.items()[0].text, "hello world");
    }

    #[test]
    fn truthful_tab_configuration_preserves_order_and_removes_duplicates() {
        let mut app = model();
        app.set_tabs([TabId::Activity, TabId::Session, TabId::Activity]);
        assert_eq!(app.tabs, vec![TabId::Activity, TabId::Session]);
        assert_eq!(app.active_tab, TabId::Session);

        app.set_tabs([]);
        assert_eq!(app.tabs, vec![TabId::Session]);
        assert_eq!(app.active_tab, TabId::Session);
    }

    #[test]
    fn completed_output_gains_semantic_styles_and_tools_remain_compact() {
        let mut app = model();
        app.begin_work();
        app.apply_runtime(RuntimeUpdate::Text(
            "# Result\nUse `cargo test` and [docs](https://example.test).".to_string(),
        ));
        app.apply_runtime(RuntimeUpdate::ToolStarted {
            id: "tool-1".to_string(),
            name: "inspect".to_string(),
        });
        app.apply_runtime(RuntimeUpdate::ToolFinished {
            id: "tool-1".to_string(),
            name: "inspect".to_string(),
            is_error: false,
            output: "detail one\ndetail two".to_string(),
        });
        app.apply_runtime(RuntimeUpdate::Stopped(StopState::Done));

        let assistant = &app.timeline.items()[0];
        assert!(assistant
            .styles
            .iter()
            .any(|span| span.style.role == SemanticRole::Heading));
        assert!(assistant
            .styles
            .iter()
            .any(|span| span.style.role == SemanticRole::Code));
        let tool = &app.timeline.items()[1];
        assert_eq!(tool.activity, Some(ActivityState::Success));
        assert!(!tool.expanded);
        assert_eq!(app.timeline.rows(80)[2].text, "inspect completed");
    }
}
