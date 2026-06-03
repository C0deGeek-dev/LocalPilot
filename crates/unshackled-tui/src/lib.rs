//! Terminal UI for Unshackled.
//!
//! A dense, terminal-native REPL on `ratatui` + `crossterm` + `tui-textarea`
//! (ADR-0006). This crate owns terminal layout, rendering, and input only; it is
//! decoupled from the provider/harness stack, consuming a mapped [`UiEvent`]
//! stream. A single [`render`] draws the whole UI from [`AppState`] so the layout
//! snapshot-tests cleanly with a `TestBackend`.
#![forbid(unsafe_code)]

mod app;
mod render;
mod state;

pub use app::{handle_input, parse_slash, run, AppInput, Key, SlashAction};
pub use render::render;
pub use state::{
    AppState, ApprovalRequest, FooterStats, Header, Mode, Paste, Picker, PlanItem, Profile,
    ThinkingPanel, TranscriptLine, TrustPrompt, UiEvent,
};

/// The product name shown in the UI.
pub const APP_NAME: &str = "Unshackled";
