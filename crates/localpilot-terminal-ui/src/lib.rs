//! Backend-neutral full-screen terminal UI state and rendering.
//!
//! This crate owns application content, input state, layout, and hit maps. The
//! executable host owns terminal modes, raw events, clipboard access, and the
//! provider/runtime adapter.
#![forbid(unsafe_code)]

mod app;
mod editor;
mod layout;
mod presentation;
mod render;
mod sanitize;
mod text;
mod theme;
mod timeline;

pub use app::{
    AppCommand, AppModel, ColorSupport, DialogState, Focus, Header, InputAction, KeyboardSupport,
    PlanEntry, RecoveryState, RuntimeUpdate, StopState, TabId, TerminalCapabilities,
    TimelineNavigation, WorkState,
};
pub use editor::{Editor, EditorRow};
pub use layout::{FrameLayout, MINIMUM_HEIGHT, MINIMUM_WIDTH};
pub use render::{render, HitMap, ScrollbarGeometry, TabHit, TimelineRowHit};
pub use sanitize::sanitize_text;
pub use theme::{Theme, ThemeParseError, ThemeResolver, UiRole};
pub use timeline::{
    ActivityState, ContentPoint, ItemId, ItemKind, PinnedPrompt, Selection, SemanticRole,
    StyledRange, TextStyle, Timeline, TimelineItem, TimelineView, ViewportAnchor, VisualRow,
    VisualRowPart, VisualSpan,
};

/// The product name shown by the full-screen UI.
pub const APP_NAME: &str = "LocalPilot";
