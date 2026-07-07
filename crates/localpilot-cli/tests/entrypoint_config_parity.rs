//! Pins config parity across the three session entry points: the interactive
//! chat driver (`repl.rs`), the wire client (`rpc_cmd.rs`), and the headless
//! session runner (`session_cmd.rs`) build their runtimes in three separate
//! functions, and a config key wired into one but not the others silently
//! no-ops on the missing paths — exactly how `verify_before_done` was honored
//! in `session` while chat and rpc ignored it. The check is on source text
//! (the three constructions are private and need live providers/terminals to
//! build), the repo's sanctioned deterministic fence for what CI cannot
//! observe directly.

const REPL_SRC: &str = include_str!("../src/repl.rs");
const RPC_SRC: &str = include_str!("../src/rpc_cmd.rs");
const SESSION_SRC: &str = include_str!("../src/session_cmd.rs");

/// Config keys every entry point's `SessionConfig` must thread identically.
const PARITY_KEYS: &[&str] = &[
    "verify_before_done: config.harness.verify_before_done",
    "verify_command: config.harness.verify_command.clone()",
    "rules: config.harness.rules.clone()",
    "enforce_claim_gate: config.harness.claim_gate.is_enabled()",
    "tool_marker_enabled: config.tools.marker",
    "enforce_readable_errors: config.tools.readable_errors",
    "repair_mode: config.tools.repair",
];

#[test]
fn chat_rpc_and_session_thread_the_same_harness_config_keys() {
    for (name, source) in [
        ("repl.rs (chat)", REPL_SRC),
        ("rpc_cmd.rs (rpc)", RPC_SRC),
        ("session_cmd.rs (session)", SESSION_SRC),
    ] {
        for key in PARITY_KEYS {
            assert!(
                source.contains(key),
                "{name} does not wire `{key}` into its SessionConfig — a config \
                 key honored on one entry point must be honored on all three"
            );
        }
    }
}
