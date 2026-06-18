//! Pure helpers for the "inspect a named target before launching your own
//! server" discipline.
//!
//! A task prompt often names an existing thing the agent can reach — a local
//! URL, a running service, a `host:port`. The agent should *probe that target*
//! before assuming it must stand up its own competing server. These helpers
//! supply the two evidence-grounded facts that decision needs:
//!
//! 1. [`extract_targets`] pulls the **local serveable** targets out of a prompt
//!    (loopback hosts, or any `host:port` with an explicit port), ignoring bare
//!    external hostnames that are only references.
//! 2. [`target_probed`] / [`any_target_unprobed`] read the session
//!    [`EvidenceLedger`] to tell whether such a target was actually probed this
//!    session — by a `fetch` tool call or a probe shell command (`curl`, `wget`,
//!    `Invoke-WebRequest`, …) whose arguments hit the target.
//!
//! Both are pure (no IO) and grounded in *evidence*, not the model's claim that
//! it "already checked" — the same doctrine as `precondition::read_this_session`,
//! which clears a `RequiresPriorRead` only when a prior `read_file` is in the
//! ledger. Keeping them pure lets the rule compose them and lets each fact be
//! unit-tested in isolation.

use localpilot_tools::string_arg;
use serde_json::Value;

use crate::evidence::{CallOutcome, EvidenceLedger};

/// The canonical host every loopback spelling collapses to, so `localhost:8080`
/// and `127.0.0.1:8080` compare equal.
const LOOPBACK_CANONICAL: &str = "localhost";

/// The loopback host spellings recognised on every platform.
const LOOPBACK_FORMS: &[&str] = &["localhost", "127.0.0.1", "0.0.0.0", "::1", "[::1]"];

/// Probe-command leading tokens recognised across Windows/Linux/macOS. A shell
/// command whose program is one of these is treated as a liveness/probe call.
const PROBE_COMMANDS: &[&str] = &[
    "curl",
    "wget",
    "invoke-webrequest",
    "invoke-restmethod",
    "iwr",
    "irm",
    "test-netconnection",
    "tnc",
    "nc",
    "httpie",
    "http",
    "xh",
];

/// Contiguous token signatures that mark a command as starting a local HTTP
/// server, matched anywhere in the (lowercased) command line. Curated and
/// extensible — an unrecognised launcher is a documented miss, not a bug.
const LAUNCH_SIGNATURES: &[&[&str]] = &[
    &["http.server"],         // python[3] -m http.server
    &["http-server"],         // npx http-server
    &["live-server"],         // npx live-server
    &["php", "-s"],           // php -S host:port
    &["npx", "serve"],        // npx serve
    &["run", "dev"],          // npm/pnpm/yarn run dev
    &["run", "start"],        // npm/pnpm/yarn run start
    &["yarn", "dev"],         // yarn dev
    &["yarn", "start"],       // yarn start
    &["pnpm", "dev"],         // pnpm dev
    &["pnpm", "start"],       // pnpm start
    &["-run", "-e", "httpd"], // ruby -run -e httpd
];

/// Program basenames that are local-server launchers when they lead a command.
const LAUNCH_PROGRAMS: &[&str] = &["vite", "caddy", "serve", "http-server", "live-server"];

/// Entry-file basenames whose creation counts as scaffolding a competing
/// frontend instead of inspecting an existing target.
const COMPETING_ENTRY_FILES: &[&str] = &["index.html", "index.htm"];

/// A local, serveable network target named in a task prompt: a host with an
/// optional port. Loopback hosts are normalized to [`LOOPBACK_CANONICAL`] so the
/// common spellings of localhost compare equal; the port is kept verbatim and
/// stays significant (`localhost:8080` != `localhost:9000`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalTarget {
    host: String,
    port: Option<u16>,
}

impl LocalTarget {
    /// Whether this target's canonical host is the loopback host.
    fn is_loopback(&self) -> bool {
        self.host == LOOPBACK_CANONICAL
    }

    /// The host spellings a command/url string may use to reference this target:
    /// for a loopback target, every loopback form; otherwise the host itself.
    fn host_forms(&self) -> Vec<&str> {
        if self.is_loopback() {
            LOOPBACK_FORMS.to_vec()
        } else {
            vec![self.host.as_str()]
        }
    }
}

/// Extract the distinct local serveable targets named in `prompt`.
///
/// Parses `http(s)://…` URLs and bare `host:port` tokens, normalizes loopback
/// hosts, and keeps a candidate only when it is locally serveable per the plan's
/// scoping: a loopback host (with or without a port), or any `host:port` with an
/// explicit port. A bare external hostname without a port is a reference, not a
/// serveable target, and is dropped.
pub(crate) fn extract_targets(prompt: &str) -> Vec<LocalTarget> {
    let mut targets: Vec<LocalTarget> = Vec::new();
    for raw in prompt.split(|c: char| c.is_whitespace()) {
        let Some(authority) = authority_of(raw) else {
            continue;
        };
        let Some(target) = parse_authority(authority) else {
            continue;
        };
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets
}

/// The host[:port] authority of one whitespace token, after stripping wrapping
/// punctuation and any URL scheme/path. Returns `None` for a token that cannot
/// carry an authority.
fn authority_of(raw: &str) -> Option<&str> {
    let token = raw.trim_matches(|c: char| {
        !c.is_ascii_alphanumeric() && !matches!(c, '.' | ':' | '/' | '[' | ']' | '-' | '_')
    });
    // After a scheme, the authority runs to the next path/query/fragment char.
    let after_scheme = match token.split_once("://") {
        Some((_scheme, rest)) => rest,
        None => token,
    };
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    if authority.is_empty() {
        None
    } else {
        Some(authority)
    }
}

/// Parse a `host[:port]` authority into a kept [`LocalTarget`], or `None` when it
/// is not a local serveable target (an external host without a port, a token
/// that is not host-shaped such as a bare number or a clock time).
fn parse_authority(authority: &str) -> Option<LocalTarget> {
    let (host_raw, port) = split_host_port(authority)?;
    let host = normalize_host(host_raw)?;
    let loopback = host == LOOPBACK_CANONICAL;
    // Keep only local serveable shapes: a loopback host (port optional), or any
    // host with an explicit port. Drop a bare external hostname reference.
    if loopback || port.is_some() {
        Some(LocalTarget { host, port })
    } else {
        None
    }
}

/// Split an authority into its host and optional port, handling bracketed IPv6
/// (`[::1]:8080`). Returns `None` when a present port does not parse as a port.
fn split_host_port(authority: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6: `[host]` or `[host]:port`.
        let (host, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => Some(p.parse::<u16>().ok()?),
            None if tail.is_empty() => None,
            None => return None,
        };
        // Re-bracket so the loopback form `[::1]` is recognised.
        return Some((bracketed(host), port));
    }
    match authority.rsplit_once(':') {
        // A colon that is part of an unbracketed IPv6 address (more than one
        // colon) is not a port separator; only `::1` is a recognised such host.
        Some((host, _)) if host.contains(':') => Some((authority, None)),
        Some((host, port)) => Some((host, Some(port.parse::<u16>().ok()?))),
        None => Some((authority, None)),
    }
}

/// `::1` re-bracketed to `[::1]`; any other host returned unchanged.
fn bracketed(host: &str) -> &str {
    if host == "::1" {
        "[::1]"
    } else {
        host
    }
}

/// Canonicalize a host: every loopback form maps to [`LOOPBACK_CANONICAL`]; a
/// host that is not host-shaped (no letter, not a dotted IPv4, not loopback)
/// returns `None` so clock times and bare numbers are not mistaken for targets.
fn normalize_host(host: &str) -> Option<String> {
    let lower = host.to_ascii_lowercase();
    if LOOPBACK_FORMS.contains(&lower.as_str()) {
        return Some(LOOPBACK_CANONICAL.to_string());
    }
    if is_host_shaped(&lower) {
        Some(lower)
    } else {
        None
    }
}

/// Whether a (lowercased) host looks like a hostname or IPv4 address: it carries
/// a letter, or it is a dotted run of digits. A bare number like `12` is not.
fn is_host_shaped(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    let has_letter = host.bytes().any(|b| b.is_ascii_lowercase());
    let dotted_numeric = host.contains('.')
        && host
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    has_letter || dotted_numeric
}

/// Whether `text` (a url or command line) references `target`: it contains the
/// `host:port` for one of the target's loopback-equivalent host forms, or — for a
/// loopback target named without a port — one of those host forms as a token.
pub(crate) fn references_target(text: &str, target: &LocalTarget) -> bool {
    let lower = text.to_ascii_lowercase();
    match target.port {
        Some(port) => target
            .host_forms()
            .iter()
            .any(|host| lower.contains(&format!("{host}:{port}"))),
        None => target
            .host_forms()
            .iter()
            .any(|host| lower.contains(&host.to_ascii_lowercase())),
    }
}

/// Reconstruct a single command line from a `run_shell` tool input, which is
/// either `{ "command": "<line>" }` or `{ "program": "p", "args": [...] }`.
pub(crate) fn shell_command_line(input: &Value) -> Option<String> {
    if let Some(command) = string_arg(input, "command") {
        return Some(command.to_string());
    }
    let program = string_arg(input, "program")?;
    let mut line = program.to_string();
    if let Some(args) = input.get("args").and_then(Value::as_array) {
        for arg in args.iter().filter_map(Value::as_str) {
            line.push(' ');
            line.push_str(arg);
        }
    }
    Some(line)
}

/// Whether a command line's program is a recognised liveness/probe command.
pub(crate) fn is_probe_command(command: &str) -> bool {
    first_token(command)
        .map(|program| {
            let program = program_basename(program).to_ascii_lowercase();
            PROBE_COMMANDS.contains(&program.as_str())
        })
        .unwrap_or(false)
}

/// The first whitespace-delimited token of a command line.
fn first_token(command: &str) -> Option<&str> {
    command.split_whitespace().next()
}

/// A program's basename without a directory prefix or an executable suffix, so
/// `/usr/bin/curl` and `curl.exe` both reduce to `curl`.
fn program_basename(program: &str) -> &str {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    name.strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"))
        .unwrap_or(name)
}

/// Whether a command line starts a local HTTP server: its program is a known
/// launcher, or it carries a known launch signature. Table-driven over
/// [`LAUNCH_PROGRAMS`]/[`LAUNCH_SIGNATURES`]; case-insensitive.
pub(crate) fn is_launch_command(command: &str) -> bool {
    let tokens: Vec<String> = command
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    let Some(first) = tokens.first() else {
        return false;
    };
    if LAUNCH_PROGRAMS.contains(&program_basename(first)) {
        return true;
    }
    LAUNCH_SIGNATURES
        .iter()
        .any(|signature| contains_window(&tokens, signature))
}

/// Whether `needle` appears as a contiguous run of tokens in `haystack`.
fn contains_window(haystack: &[String], needle: &[&str]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.iter().zip(needle).all(|(have, want)| have == want))
}

/// Whether a `write_file`/`apply_patch` call **creates** a competing entry file
/// (an `index.html`-family page) rather than inspecting an existing target.
pub(crate) fn is_scaffold_write(tool: &str, input: &Value) -> bool {
    match tool {
        "write_file" => string_arg(input, "path").is_some_and(is_competing_entry_path),
        "apply_patch" => input
            .get("operations")
            .and_then(Value::as_array)
            .is_some_and(|operations| operations.iter().any(creates_competing_entry)),
        _ => false,
    }
}

/// Whether one `apply_patch` operation is a `create` of a competing entry file.
fn creates_competing_entry(operation: &Value) -> bool {
    operation.get("action").and_then(Value::as_str) == Some("create")
        && operation
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(is_competing_entry_path)
}

/// Whether a path's basename is a competing entry file.
fn is_competing_entry_path(path: &str) -> bool {
    let base = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    COMPETING_ENTRY_FILES.contains(&base.as_str())
}

/// Whether a *successful* call this session probed `target`: a `fetch` whose url
/// references it, or a `run_shell` probe command whose line references it.
pub(crate) fn target_probed(target: &LocalTarget, ledger: &EvidenceLedger) -> bool {
    ledger.calls().iter().any(|call| {
        if call.outcome != CallOutcome::Ok {
            return false;
        }
        match call.name.as_str() {
            "fetch" => {
                string_arg(&call.input, "url").is_some_and(|url| references_target(url, target))
            }
            "run_shell" => shell_command_line(&call.input)
                .is_some_and(|line| is_probe_command(&line) && references_target(&line, target)),
            _ => false,
        }
    })
}

/// Whether at least one named target has **not** been probed this session. The
/// session passes this as the rule's `named_local_target_unprobed` signal.
pub(crate) fn any_target_unprobed(targets: &[LocalTarget], ledger: &EvidenceLedger) -> bool {
    targets.iter().any(|target| !target_probed(target, ledger))
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_core::{ContentBlock, EventId, Message, Role, ToolCall, ToolResult};
    use localpilot_store::{
        MessageOrigin, SessionEvent, SessionEventKind, SESSION_EVENT_FORMAT_VERSION,
    };

    fn target(host: &str, port: Option<u16>) -> LocalTarget {
        LocalTarget {
            host: host.to_string(),
            port,
        }
    }

    // --- extraction (01.1) ---

    #[test]
    fn extracts_localhost_url_with_port() {
        let targets = extract_targets("Test the app at http://localhost:8080/health please");
        assert_eq!(targets, vec![target("localhost", Some(8080))]);
    }

    #[test]
    fn ignores_bare_external_url() {
        // An external hostname with no port is a reference, not a serveable target.
        let targets = extract_targets("Match the design at https://example.com exactly");
        assert!(targets.is_empty(), "got {targets:?}");
    }

    #[test]
    fn keeps_external_host_with_explicit_port() {
        let targets = extract_targets("the api lives at api.internal:9000");
        assert_eq!(targets, vec![target("api.internal", Some(9000))]);
    }

    #[test]
    fn keeps_loopback_ip_and_bracketed_ipv6() {
        assert_eq!(
            extract_targets("serve on 127.0.0.1:3000"),
            vec![target("localhost", Some(3000))]
        );
        assert_eq!(
            extract_targets("bound to http://[::1]:5173/"),
            vec![target("localhost", Some(5173))]
        );
    }

    #[test]
    fn does_not_mistake_a_clock_time_or_bare_number_for_a_target() {
        assert!(extract_targets("the meeting is at 12:34 today").is_empty());
        assert!(extract_targets("see section 8080 of the spec").is_empty());
    }

    #[test]
    fn deduplicates_loopback_spellings() {
        let targets = extract_targets("hit localhost:8000 then 127.0.0.1:8000 again");
        assert_eq!(targets, vec![target("localhost", Some(8000))]);
    }

    // --- matcher / loopback equivalence + port sensitivity (01.2) ---

    #[test]
    fn matcher_treats_loopback_spellings_as_equal() {
        let t = target("localhost", Some(8080));
        assert!(references_target("curl http://127.0.0.1:8080/", &t));
        assert!(references_target("GET http://localhost:8080", &t));
        assert!(references_target("fetch http://[::1]:8080/x", &t));
    }

    #[test]
    fn matcher_is_port_sensitive() {
        let t = target("localhost", Some(8080));
        assert!(!references_target("curl http://localhost:9000/", &t));
    }

    // --- launch detection (02.1) ---

    #[test]
    fn detects_local_server_launch_commands_across_ecosystems() {
        for command in [
            "python -m http.server",
            "python3 -m http.server 8000",
            "php -S localhost:8000",
            "npx serve",
            "npx http-server -p 8080",
            "npm run dev",
            "pnpm dev",
            "yarn start",
            "vite",
            "caddy run",
            "serve -s build",
            "ruby -run -e httpd . -p 8000",
            "/usr/local/bin/vite --host",
        ] {
            assert!(
                is_launch_command(command),
                "should detect launch: {command}"
            );
        }
    }

    #[test]
    fn benign_commands_are_not_launches() {
        for command in ["ls -la", "cargo build", "git status", "echo serve the page"] {
            assert!(!is_launch_command(command), "false launch: {command}");
        }
    }

    // --- scaffold detection (02.2) ---

    #[test]
    fn detects_competing_entry_file_scaffold() {
        assert!(is_scaffold_write(
            "write_file",
            &serde_json::json!({ "path": "index.html", "content": "<html>" })
        ));
        assert!(is_scaffold_write(
            "write_file",
            &serde_json::json!({ "path": "public/index.html", "content": "<html>" })
        ));
        assert!(is_scaffold_write(
            "apply_patch",
            &serde_json::json!({ "operations": [
                { "action": "create", "path": "site/index.html", "content": "<html>" }
            ] })
        ));
    }

    #[test]
    fn non_entry_writes_are_not_scaffolds() {
        assert!(!is_scaffold_write(
            "write_file",
            &serde_json::json!({ "path": "src/main.rs", "content": "fn main() {}" })
        ));
        // Updating an existing entry file is not a fresh competing scaffold.
        assert!(!is_scaffold_write(
            "apply_patch",
            &serde_json::json!({ "operations": [
                { "action": "update", "path": "index.html", "hunks": [] }
            ] })
        ));
    }

    // --- probe-evidence read (01.3) ---

    fn event(kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            v: SESSION_EVENT_FORMAT_VERSION,
            id: EventId::new(),
            parent_id: None,
            at_unix: 0,
            kind,
        }
    }

    /// A ledger holding one successful tool call `name(input)`.
    fn ledger_with_ok_call(name: &str, input: Value) -> EvidenceLedger {
        let call = ToolCall::new("p1".into(), name, input);
        let invoke = event(SessionEventKind::Message {
            message: Message::new(Role::Assistant, vec![ContentBlock::ToolUse(call)]),
            origin: MessageOrigin::Assistant,
        });
        let result = event(SessionEventKind::Message {
            message: Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult(ToolResult::success(
                    "p1".into(),
                    "200 OK",
                ))],
            ),
            origin: MessageOrigin::ToolResult,
        });
        EvidenceLedger::project(&[invoke, result])
    }

    #[test]
    fn probe_via_fetch_tool_satisfies() {
        let t = target("localhost", Some(8080));
        let ledger = ledger_with_ok_call(
            "fetch",
            serde_json::json!({ "url": "http://localhost:8080/" }),
        );
        assert!(target_probed(&t, &ledger));
        assert!(!any_target_unprobed(std::slice::from_ref(&t), &ledger));
    }

    #[test]
    fn probe_via_curl_command_satisfies() {
        let t = target("localhost", Some(8080));
        let ledger = ledger_with_ok_call(
            "run_shell",
            serde_json::json!({ "command": "curl http://127.0.0.1:8080/health" }),
        );
        assert!(target_probed(&t, &ledger));
    }

    #[test]
    fn probe_via_direct_form_run_shell_satisfies() {
        let t = target("localhost", Some(8080));
        let ledger = ledger_with_ok_call(
            "run_shell",
            serde_json::json!({ "program": "curl", "args": ["http://localhost:8080"] }),
        );
        assert!(target_probed(&t, &ledger));
    }

    #[test]
    fn no_probe_is_unsatisfied() {
        let t = target("localhost", Some(8080));
        // A non-probe command that mentions the target does not count as a probe.
        let ledger = ledger_with_ok_call(
            "run_shell",
            serde_json::json!({ "command": "echo localhost:8080" }),
        );
        assert!(!target_probed(&t, &ledger));
        assert!(any_target_unprobed(std::slice::from_ref(&t), &ledger));
    }

    #[test]
    fn a_failed_probe_does_not_satisfy() {
        let t = target("localhost", Some(8080));
        let call = ToolCall::new(
            "p1".into(),
            "fetch",
            serde_json::json!({ "url": "http://localhost:8080/" }),
        );
        let invoke = event(SessionEventKind::Message {
            message: Message::new(Role::Assistant, vec![ContentBlock::ToolUse(call)]),
            origin: MessageOrigin::Assistant,
        });
        let result = event(SessionEventKind::Message {
            message: Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult(ToolResult::error(
                    "p1".into(),
                    "connection refused",
                ))],
            ),
            origin: MessageOrigin::ToolResult,
        });
        let ledger = EvidenceLedger::project(&[invoke, result]);
        assert!(!target_probed(&t, &ledger));
    }
}
