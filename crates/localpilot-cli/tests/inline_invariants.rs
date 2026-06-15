//! Pins the inline-rendering invariant: the interactive terminal driver stays in
//! the main screen buffer and never captures the mouse, so native scrollback,
//! selection, copy/paste, and scrollwheel keep working. A regression that re-adds
//! the alternate screen or default mouse capture fails one of these tests.
//!
//! The check is on the driver's source text (not a live terminal, which cannot be
//! exercised in CI), which is why the needles below are split or absent from this
//! file so they do not match themselves.

const DRIVER_SRC: &str = include_str!("../src/repl.rs");

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
