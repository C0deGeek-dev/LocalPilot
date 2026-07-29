//! Backend-neutral full-screen terminal UI state and rendering.
//!
//! This crate owns application content, input state, layout, and hit maps. The
//! executable host owns terminal modes, raw events, clipboard access, and the
//! provider/runtime adapter.
#![forbid(unsafe_code)]

mod app;
mod editor;
mod render;
mod sanitize;
mod text;
mod timeline;

pub use app::{
    AppCommand, AppModel, ColorSupport, Focus, Header, InputAction, KeyboardSupport, PlanEntry,
    RecoveryState, RuntimeUpdate, StopState, TerminalCapabilities, WorkState,
};
pub use editor::{Editor, EditorRow};
pub use render::{render, HitMap};
pub use sanitize::sanitize_text;
pub use timeline::{
    ContentPoint, ItemId, ItemKind, Selection, Timeline, TimelineItem, TimelineView,
    ViewportAnchor, VisualRow,
};

/// The product name shown by the full-screen UI.
pub const APP_NAME: &str = "LocalPilot";
