//! Backend-neutral full-screen terminal UI state and rendering.
//!
//! This crate owns application content, input state, layout, and hit maps. The
//! executable host owns terminal modes, raw events, clipboard access, and the
//! provider/runtime adapter.
//!
//! Pair presentation extends that boundary without creating a second terminal
//! application: one shared shell owns the composer, dialogs, workspace chrome,
//! and lifecycle, while each ordinary session has one cohesive projection of its
//! timeline and live state. `FrameLayout` remains the single geometry authority,
//! peer-tagged updates route only to their named projection, and drawing and
//! hit-testing consume the same per-pane rectangles. Ordinary single chat keeps
//! the same path when pair presentation is absent. The executable host remains
//! responsible for constructing and driving any runnable pair.
#![forbid(unsafe_code)]

mod app;
mod editor;
mod layout;
mod presentation;
mod projection;
mod render;
mod sanitize;
mod text;
mod theme;
mod timeline;

pub use app::{
    AppCommand, AppModel, ColorSupport, CompletionCommand, DialogState, DiffFile, DiffLine,
    DiffLineKind, Focus, Header, InputAction, KeyboardSupport, PairStatus, PairStatusCandidate,
    PlanEntry, QuestionAction, QuestionOption, QuestionResponse, RecoveryState, RuntimeUpdate,
    SessionEntry, SessionSelection, SettingEdit, SettingEntry, StopState, TabId, TakeoverKind,
    TakeoverNavigation, TerminalCapabilities, TimelineNavigation, TrustAction, UsageTotals,
    UserShellCommand, UserShellOutput, WorkState,
};
pub use editor::{Editor, EditorRow, ImageAttachment, PasteUnit, SubmittedInput};
pub use layout::{
    FrameLayout, PairTimelineLayout, TimelineLayout, TimelinePaneLayout, MINIMUM_HEIGHT,
    MINIMUM_WIDTH, PAIR_MINIMUM_HEIGHT, PAIR_WIDE_MINIMUM_WIDTH,
};
pub use projection::{PeerPane, SessionHeader};
pub use render::{
    render, CompletionHit, HitMap, QuestionHit, ScrollbarGeometry, TabHit, TakeoverHit, ThemeHit,
    TimelineHits, TimelinePaneHits, TimelineRowHit, TrustHit, TrustPathHit,
};
pub use sanitize::sanitize_text;
pub use theme::{Theme, ThemeParseError, ThemeResolver, UiRole};
pub use timeline::{
    ActivityState, ContentPoint, ItemId, ItemKind, PinnedPrompt, ResultTone, Selection,
    SemanticRole, StyledRange, TextStyle, Timeline, TimelineItem, TimelineView, ViewportAnchor,
    VisualRow, VisualRowPart, VisualSpan,
};

/// The product name shown by the full-screen UI.
pub const APP_NAME: &str = "LocalPilot";
