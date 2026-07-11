# Embedding and Headless Drive

Two supported ways to drive LocalPilot without its own UI, in order of
preference:

1. **In-process embedding** — link the crates and own a `SessionRuntime`.
2. **`localpilot rpc`** — newline-delimited JSON over stdin/stdout for hosts
   in another language or process.

There is deliberately no HTTP server and no packaged product SDK: the library
surface below is the embedding contract, and the RPC protocol is its
process-boundary mirror.

## In-process embedding

The supported library API is the `SessionRuntime` in `localpilot-harness`,
composed from the same crates the CLI uses. A minimal host:

```rust,no_run
use std::sync::Arc;

use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

async fn host(provider: Arc<dyn localpilot_llm::ModelProvider>) -> anyhow::Result<()> {
    let root = std::env::current_dir()?;
    let mut runtime = SessionRuntime::new(
        provider,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        // Replace with your own `Approver` to prompt your user.
        Box::new(ScriptedApprover::new(Vec::new())),
        Store::open(&root),
        Workspace::new(&root)?,
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    );

    let (events, mut rx) = broadcast::channel::<RuntimeEvent>(1024);
    let cancel = CancellationToken::new();
    let turn = runtime.run_turn("summarize this repo", &events, &cancel);
    tokio::pin!(turn);
    loop {
        tokio::select! {
            _ = &mut turn => break,
            Ok(event) = rx.recv() => {
                if let RuntimeEvent::Text(delta) = event {
                    print!("{delta}");
                }
            }
        }
    }
    Ok(())
}
```

What the host owns:

- the **provider** (build one from config with `ProviderRegistry`, or
  implement `ModelProvider` yourself),
- the **approver** — every ask-class permission decision flows through your
  `Approver` implementation; the engine's verdicts cannot be bypassed,
- **cancellation** via the `CancellationToken`,
- **steering**: clone `runtime.steer_queue()` before a turn and push text into
  it while the turn runs; it is admitted at the next safe provider-turn
  boundary.

What the runtime guarantees is the reliability contract in
[`docs/06`](06-harness-spec.md) and [`docs/07`](07-security-and-privacy.md):
tool pairing on every exit path, permission mediation for every side effect,
redaction before persistence, and a durable session event log under
`.localpilot/`.

### Stability caveats

- The crates are pre-1.0: APIs may change between minor versions. Pin exact
  versions and read the changelog before bumping.
- `SessionRuntime::new` takes its collaborators positionally; expect this
  constructor to grow a builder before 1.0.
- The session event-log format and the RPC protocol are explicitly versioned;
  the in-process Rust API is not — the compiler is the migration tool.

## RPC over stdio

`localpilot rpc [--model …] [--provider …] [--permission …]` serves one client
on stdin/stdout. One JSON object per LF line in each direction; every record
carries the protocol version (`"v": 1`).

A session lives as long as the process. To pick an earlier session back up,
`--continue` opens the most recent session in the workspace and
`--resume <id-or-name>` opens a specific one (same flags as `chat`; the two
are mutually exclusive). The `hello` reply reports the resumed session's id.
The conversation is rebuilt from the session's durable event log; the current
permission profile and trust state apply — nothing resumes with stale
elevated permissions.

Commands in: `hello`, `prompt` (with a `disposition` of `immediate`, `steer`,
or `follow_up`), `cancel`, `permission_reply`, `status`, `shutdown`. Events
out mirror the runtime's session events (`text_delta`, `tool_started`,
`tool_finished`, `usage`, `context_usage`, `stopped`, …) plus
`permission_ask`/`status`/`error`.

```text
→ {"v":1,"id":"1","command":{"type":"hello"}}
← {"v":1,"id":"1","event":{"type":"hello","protocol_version":1,"session_id":"…","model":"…"}}
→ {"v":1,"command":{"type":"prompt","text":"run the tests"}}
← {"v":1,"event":{"type":"permission_ask","ask_id":"ask-…","tool":"run_shell","detail":"cargo test","risk":"run a command"}}
→ {"v":1,"command":{"type":"permission_reply","ask_id":"ask-…","allow":true}}
← {"v":1,"event":{"type":"text_delta","text":"All tests pass."}}
← {"v":1,"event":{"type":"stopped","reason":"done"}}
```

Permission semantics over the wire: the decision logic stays in the
permission engine; the client only renders the ask. An unanswered ask — a
disconnected or silent client — is **denied**, exactly like non-interactive
mode. `status` exposes the session, the active profile, outstanding asks, and
the next incomplete harness step.

Framing contract: records are split on LF only; a trailing CR before the LF
is tolerated; Unicode line separators (U+2028/U+2029) inside a record never
split it.

## MCP over stdio

`localpilot mcp serve [--model …] [--provider …] [--permission …] [--continue |
--resume <id-or-name>] [--no-approvals]` serves the same session runtime as an
[MCP](https://modelcontextprotocol.io) server (protocol revision 2025-06-18),
so an MCP client — an agent host like Claude Code or Codex — can drive and
steer a LocalPilot session through ordinary tool calls. Register it like any
stdio MCP server (`command = "localpilot"`, `args = ["mcp", "serve", …]`).

The tools: `prompt` submits input (starts a turn when idle; while a turn runs,
`disposition: "steer"` injects guidance at the next safe boundary and
`"follow_up"` queues the next turn), `cancel` aborts the running turn,
`status` reports session/model/profile/busy/pending asks, `transcript` returns
the redacted transcript tail, and `reply_permission` answers a pending
permission ask.

MCP is request/response, so events are pull-based: every session event gets a
monotonic sequence number in a bounded feed, and `events` returns the page
after your cursor — pass `wait_ms` (server-capped at 20000) to wait for the
first new event instead of busy-polling. The feed reports a `dropped` count
when a lagging client overflowed it; nothing is ever dropped silently.

Permission semantics are identical to the other adapters: the engine decides,
the client only answers the asks it is shown, and an unanswered ask is denied.
`--no-approvals` withholds the `reply_permission` tool entirely — the client
can watch and steer but every ask denies (watch-and-steer mode). Asks that a
profile resolves without asking are never surfaced at all.
