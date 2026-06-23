//! Terminal UI for LocalPilot.
//!
//! A terminal-native REPL on `ratatui` (ADR-0006), rendered inline: finished
//! transcript items become [`history_block_text`] that the host pushes into the
//! terminal's native scrollback once, and [`render`] draws only the live region
//! (in-progress activity, the composer, and the status line). This crate owns
//! layout, rendering, and input — with a hand-rolled composer — and is decoupled
//! from the provider/harness stack, consuming a mapped [`UiEvent`] stream. Every
//! drawn surface uses ratatui's backend-agnostic widgets so it snapshot-tests
//! cleanly with a `TestBackend`.
#![forbid(unsafe_code)]

mod app;
mod render;
mod state;

pub use app::{
    handle_input, parse_slash, run, AppInput, BackgroundCommand, IngestAction, Key, SlashAction,
};
pub use render::{banner_text, history_block_text, live_region_height, render};
pub use state::{
    ActiveTool, AppState, ApprovalRequest, BackgroundProcess, FooterStats, Header, ImageAttachment,
    Mode, Paste, PlanItem, Profile, ThinkingPanel, TranscriptLine, TrustPrompt, UiEvent,
};

/// The product name shown in the UI.
pub const APP_NAME: &str = "LocalPilot";
