//! A configured environment reaches a real spawned MCP server process.
//!
//! These drive an actual child process rather than asserting on the overlay
//! type, because the property under test is what the *operating system* hands
//! the child — inheritance, replacement by name, and the platform's own notion
//! of what "the same name" means. A green assertion on a `ServerEnvironment`
//! would prove none of that.
//!
//! The child is this same test binary re-invoked in probe mode (the marker
//! environment variable below), so the fixture needs no shell, no third-party
//! server, and no platform-specific helper program. It speaks just enough
//! JSON-RPC to answer one request: report the values of the variables it was
//! asked about. `StdioTransport` already skips non-JSON lines, so the test
//! harness's own stdout chatter is ignored by the client.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use localpilot_core::Secret;
use localpilot_mcp::{ResolvedEnvEntry, ServerEnvironment, StdioTransport, Transport};
use serde_json::{json, Value};

/// Switches `env_probe_child` into server mode.
///
/// Deliberately passed *only* through the spawned child's environment overlay,
/// never via `set_var` in the parent: environment writes are process-global, so a
/// parent that set this would make its own `env_probe_child` block forever
/// reading the test runner's stdin.
const CHILD_MARKER: &str = "LOCALPILOT_MCP_ENV_PROBE";

/// A variable the parent exports before spawning, to prove ordinary inheritance
/// still works alongside an overlay.
const INHERITED_ONLY: &str = "LOCALPILOT_TEST_INHERITED_ONLY";

/// A variable the parent exports *and* the overlay replaces.
const OVERRIDDEN: &str = "LOCALPILOT_TEST_OVERRIDDEN";

/// In probe mode, act as a minimal MCP server: answer each request with the
/// values of the environment variables named in `params.names`. Outside probe
/// mode this is an ordinary no-op test.
#[test]
fn env_probe_child() {
    if std::env::var(CHILD_MARKER).is_err() {
        return;
    }
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = request.get("id").cloned() else {
            continue; // a notification: nothing to answer
        };
        let names: Vec<String> = request
            .get("params")
            .and_then(|params| params.get("names"))
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let seen: BTreeMap<String, Option<String>> = names
            .into_iter()
            .map(|name| {
                let value = std::env::var(&name).ok();
                (name, value)
            })
            .collect();
        let response = json!({ "jsonrpc": "2.0", "id": id, "result": { "env": seen } });
        // Lead with a newline: the test harness writes `test <name> ... ` with no
        // trailing newline, so without this the response shares a line with that
        // prefix and the client — which parses whole lines — never sees valid JSON.
        let _ = writeln!(
            stdout,
            "
{response}"
        );
        let _ = stdout.flush();
    }
}

/// Spawn the probe child with `environment` overlaid and ask it what it sees.
async fn probe(
    mut entries: Vec<ResolvedEnvEntry>,
    names: &[&str],
) -> BTreeMap<String, Option<String>> {
    let exe = std::env::current_exe().unwrap();
    // The filter is positional and must precede `--exact`; the other order
    // silently matches nothing and the child exits without ever answering.
    let args: Vec<String> = [
        "env_probe_child",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ]
    .iter()
    .map(|arg| (*arg).to_string())
    .collect();

    // Carry the probe marker in the child's own overlay so the parent process
    // never sets it (see the constant's note).
    entries.push(entry(CHILD_MARKER, "1", false));
    let spawn_environment = ServerEnvironment::new(entries);

    let transport =
        StdioTransport::spawn(exe.to_str().unwrap(), &args, &spawn_environment).unwrap();

    let result = transport
        .call("probe", json!({ "names": names }))
        .await
        .unwrap();
    serde_json::from_value(result.get("env").cloned().unwrap_or(Value::Null)).unwrap()
}

fn entry(name: &str, value: &str, sensitive: bool) -> ResolvedEnvEntry {
    ResolvedEnvEntry {
        name: name.to_string(),
        value: Secret::new(value),
        sensitive,
    }
}

#[tokio::test]
async fn a_child_receives_every_configured_entry_form_and_keeps_its_inheritance() {
    std::env::set_var(INHERITED_ONLY, "inherited-value");
    std::env::set_var(OVERRIDDEN, "inherited-value");

    let seen = probe(
        vec![
            entry("LOCALPILOT_TEST_PLAIN", "plain-value", false),
            entry("LOCALPILOT_TEST_LITERAL", "sensitive-literal-value", true),
            entry(
                "LOCALPILOT_TEST_CREDENTIAL",
                "resolved-credential-value",
                true,
            ),
            entry(OVERRIDDEN, "overlay-value", false),
        ],
        &[
            "LOCALPILOT_TEST_PLAIN",
            "LOCALPILOT_TEST_LITERAL",
            "LOCALPILOT_TEST_CREDENTIAL",
            OVERRIDDEN,
            INHERITED_ONLY,
        ],
    )
    .await;

    // Every configured form arrives, whatever its sensitivity: the distinction
    // governs redaction, never delivery.
    assert_eq!(
        seen.get("LOCALPILOT_TEST_PLAIN"),
        Some(&Some("plain-value".to_string()))
    );
    assert_eq!(
        seen.get("LOCALPILOT_TEST_LITERAL"),
        Some(&Some("sensitive-literal-value".to_string()))
    );
    assert_eq!(
        seen.get("LOCALPILOT_TEST_CREDENTIAL"),
        Some(&Some("resolved-credential-value".to_string()))
    );
    // A configured entry replaces the inherited variable of the same name. The
    // spelling is identical on purpose: a case-differing pair would replace on
    // Windows and add a second variable on Linux/macOS, so configuration refuses
    // it and this assertion stays portable.
    assert_eq!(
        seen.get(OVERRIDDEN),
        Some(&Some("overlay-value".to_string()))
    );
    // An unrelated inherited variable survives: the overlay adds to the
    // environment, it does not replace it.
    assert_eq!(
        seen.get(INHERITED_ONLY),
        Some(&Some("inherited-value".to_string()))
    );
}

#[tokio::test]
async fn an_empty_overlay_leaves_inheritance_untouched() {
    std::env::set_var(INHERITED_ONLY, "inherited-value");

    let seen = probe(Vec::new(), &[INHERITED_ONLY]).await;

    // The shape every server configured before per-server environments existed
    // still has: plain inheritance, nothing added, nothing removed.
    assert_eq!(
        seen.get(INHERITED_ONLY),
        Some(&Some("inherited-value".to_string()))
    );
}
