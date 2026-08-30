//! Pins the terminal-ownership invariants for MCP server processes: an MCP
//! server lives for the whole session next to the interactive TUI, so it must
//! speak JSON-RPC over pipes (never the console's stdin) and be isolated from
//! the interactive console — Windows: its own invisible console (a shared one
//! would let it read `CONIN$` or re-cook the console mode); Unix: a
//! non-foreground process group, so a direct `/dev/tty` read gets SIGTTIN.
//!
//! Source-text checks, because a live console cannot be exercised in CI.

const TRANSPORT_SRC: &str = include_str!("../src/transport.rs");

#[test]
fn mcp_servers_speak_over_pipes_and_stay_off_the_interactive_console() {
    assert!(
        TRANSPORT_SRC.contains(".stdin(Stdio::piped())"),
        "MCP servers receive JSON-RPC over a piped stdin, never the console's"
    );
    assert!(
        TRANSPORT_SRC.contains("creation_flags(CREATE_NO_WINDOW)"),
        "MCP servers must get their own invisible console on Windows"
    );
    assert!(
        TRANSPORT_SRC.contains("process_group(0)"),
        "MCP servers must lead a non-foreground process group on Unix"
    );
}
