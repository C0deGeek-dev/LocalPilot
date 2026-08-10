//! Pins config parity across the session entry points: the interactive chat
//! builder (`interactive_session.rs`), the headless session runner
//! (`session_cmd.rs`), and the shared builder that the wire client (`rpc_cmd.rs`)
//! and the opt-in server (`serve`/`connect`) both construct through
//! (`server_cmd.rs`). A config key
//! wired into one but not the others silently no-ops on the missing paths —
//! exactly how `verify_before_done` was once honored in `session` while chat
//! and rpc ignored it. The check is on source text (the constructions are
//! private and need live providers/terminals to build), the repo's sanctioned
//! deterministic fence for what CI cannot observe directly.
//!
//! Note: `rpc_cmd.rs` no longer builds a `SessionConfig` itself — it delegates
//! to `server_cmd::SessionSetup::build`, the single recipe it shares with the
//! server factory — so the wire path's parity is pinned there. `server_cmd`
//! reads the same keys behind a `self.config` prefix; the check tolerates
//! either prefix so it holds across all three constructions.

const INTERACTIVE_SRC: &str = include_str!("../src/interactive_session.rs");
const SESSION_SRC: &str = include_str!("../src/session_cmd.rs");
const SERVER_SRC: &str = include_str!("../src/server_cmd.rs");

/// Config keys every entry point's `SessionConfig` must thread identically.
const PARITY_KEYS: &[&str] = &[
    "verify_before_done: config.harness.verify_before_done",
    "verify_command: config.harness.verify_command.clone()",
    "rules: config.harness.rules.clone()",
    "enforce_claim_gate: config.harness.claim_gate.is_enabled()",
    "tool_marker_enabled: config.tools.marker",
    "enforce_readable_errors: config.tools.readable_errors",
    "repair_mode: config.tools.repair",
    "elide_seen_reads: config.tools.elide_seen_reads",
];

/// Whether `source` threads `key`, accepting either a bare `config.` prefix
/// (the direct chat/session constructions) or the shared builder's
/// `self.config.` prefix (`server_cmd.rs`).
fn wires(source: &str, key: &str) -> bool {
    source.contains(key) || source.contains(&key.replace("config.", "self.config."))
}

#[test]
fn chat_session_and_the_shared_builder_thread_the_same_harness_config_keys() {
    for (name, source) in [
        ("interactive_session.rs (chat)", INTERACTIVE_SRC),
        ("session_cmd.rs (session)", SESSION_SRC),
        ("server_cmd.rs (rpc + serve)", SERVER_SRC),
    ] {
        for key in PARITY_KEYS {
            assert!(
                wires(source, key),
                "{name} does not wire `{key}` into its SessionConfig — a config \
                 key honored on one entry point must be honored on all of them"
            );
        }
    }
}
