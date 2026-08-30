//! Pins the terminal-ownership invariants for every child process spawned
//! while an interactive session may own the terminal: a child must never
//! inherit the console's stdin (it would consume the TUI's keystrokes,
//! including the Ctrl+C key event raw mode relies on) and must be isolated
//! from the interactive console (Windows: its own invisible console; Unix: a
//! non-foreground process group, so a direct `/dev/tty` read gets SIGTTIN).
//!
//! The behavioral tests (`sort` sees immediate EOF) prove the fix only when
//! the test runner itself has an interactive stdin — under CI the runner's
//! stdin is already closed, so a revert would still pass them. These source
//! checks are the deterministic regression fence.

const SHELL_SRC: &str = include_str!("../src/builtins_shell.rs");
const BACKGROUND_SRC: &str = include_str!("../src/builtins_background.rs");
const BUILTINS_SRC: &str = include_str!("../src/builtins.rs");

#[test]
fn run_shell_nulls_stdin_and_detaches_from_the_console() {
    assert!(
        SHELL_SRC.contains(".stdin(std::process::Stdio::null())"),
        "run_shell children must never inherit the interactive stdin"
    );
    assert!(
        SHELL_SRC.contains("creation_flags(CREATE_NO_WINDOW)"),
        "run_shell children must get their own invisible console on Windows"
    );
    assert!(
        SHELL_SRC.contains("process_group(0)"),
        "run_shell children must lead a non-foreground process group on Unix"
    );
}

#[test]
fn the_stream_editor_child_stays_off_the_interactive_console() {
    // replace_in_file pipes the payload through stdin (so stdin stays piped,
    // not null), but the child must still get its own invisible console on
    // Windows rather than the TUI's.
    assert!(
        BUILTINS_SRC.contains("creation_flags(CREATE_NO_WINDOW)"),
        "the stream-editor child must get its own invisible console on Windows"
    );
    assert!(
        BUILTINS_SRC.contains(".stdin(std::process::Stdio::piped())"),
        "the stream editor receives its payload over a piped stdin"
    );
}

#[test]
fn run_background_nulls_stdin_and_detaches_from_the_console() {
    assert!(
        BACKGROUND_SRC.contains(".stdin(std::process::Stdio::null())"),
        "background children must never inherit the interactive stdin"
    );
    assert!(
        BACKGROUND_SRC.contains("CREATE_NO_WINDOW"),
        "background children must get their own invisible console on Windows"
    );
    assert!(
        BACKGROUND_SRC.contains("process_group(0)"),
        "background children must lead a non-foreground process group on Unix"
    );
}
