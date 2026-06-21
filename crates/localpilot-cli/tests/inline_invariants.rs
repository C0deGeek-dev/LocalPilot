//! Pins the inline-rendering invariant: the interactive terminal driver stays in
//! the main screen buffer and never captures the mouse, so native scrollback,
//! selection, copy/paste, and scrollwheel keep working. A regression that re-adds
//! the alternate screen or default mouse capture fails one of these tests.
//!
//! The check is on the driver's source text (not a live terminal, which cannot be
//! exercised in CI), which is why the needles below are split or absent from this
//! file so they do not match themselves.

const DRIVER_SRC: &str = include_str!("../src/repl.rs");
const CARGO_MANIFEST: &str = include_str!("../Cargo.toml");

#[test]
fn ratatui_scrolling_regions_feature_stays_off() {
    // ratatui's `scrolling-regions` feature commits scrollback via DECSTBM margin
    // scrolls (`ESC[t;br` + `ESC[nS`). Windows Terminal does not preserve lines
    // scrolled out of a margin region into its scrollback buffer, so conversation
    // history above the live region is silently lost there. The default path
    // (`append_lines`, a `\n` at the bottom row) feeds scrollback on every
    // terminal. The TestBackend scrollback tests cannot catch this — TestBackend
    // routes region-scrolled lines into its scrollback buffer regardless — so this
    // manifest guard is the real regression fence. The quoted needle matches only
    // the TOML feature-array entry, not the prose in the surrounding comment.
    assert!(
        !CARGO_MANIFEST.contains("\"scrolling-regions\""),
        "ratatui's scrolling-regions feature breaks scrollback on Windows Terminal; keep it disabled"
    );
}

#[test]
fn the_driver_never_enters_the_alternate_screen() {
    assert!(
        !DRIVER_SRC.contains("EnterAlternateScreen"),
        "the inline driver must stay in the main screen buffer"
    );
    assert!(
        !DRIVER_SRC.contains("LeaveAlternateScreen"),
        "no alternate screen is entered, so none is left"
    );
}

#[test]
fn the_driver_never_enables_mouse_capture() {
    assert!(
        !DRIVER_SRC.contains("EnableMouseCapture"),
        "capturing the mouse would disable native selection and scrollwheel"
    );
}

#[test]
fn the_driver_builds_an_inline_viewport() {
    assert!(
        DRIVER_SRC.contains("Viewport::Inline"),
        "the live region is an inline viewport, not a fullscreen one"
    );
}
