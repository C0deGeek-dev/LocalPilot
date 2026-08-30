# Embedding and Headless Drive

Three supported ways to drive LocalPilot without its own UI, in order of
preference:

1. **In-process embedding** — link the crates and own a `SessionRuntime`.
2. **`localpilot rpc`** — newline-delimited JSON over stdin/stdout for hosts
   in another language or process.
3. **`localpilot serve` + `localpilot connect`** — an **opt-in** local-IPC
   server that hosts sessions in one long-lived process, so several clients can
   attach to the same session at once (see below).

There is deliberately no HTTP server and no packaged product SDK: the library
surface below is the embedding contract, and the RPC protocol is its
process-boundary mirror.

This document uses “embedding” for hosting the agent runtime. The vector-model
server used by LocalMind has a separate lifecycle: session-bearing CLI commands
may start it through LocalBox, and LocalPilot/LocalMind share exact endpoint/PID
leases until the last client exits. User-started endpoints are never claimed or
stopped. See [LocalMind integration](localmind-integration.md#the-embedding-servers-lifecycle).

The third option is the **opt-in local-IPC server transport** (the
`localpilot-server` crate): a framed transport over a Unix domain socket or a
Windows named pipe, plus daemon lifecycle (detached spawn, a retry-connect ready
handshake, single-owner exclusivity), a registry that hosts many sessions in one
process, and a per-session host layer for multi-client fanout and out-of-band
control (below). It is still local-only — not an HTTP server and not a product
SDK — and reuses the same LF-delimited JSON framing as the stdio path. It is
strictly opt-in: it runs only when you invoke `serve`/`connect`, and the default
in-process `chat`/`ask`/`print`/`harness` path is byte-for-byte unchanged and
never starts it.

### Multi-client fanout and out-of-band control (server crate)

When several client connections attach to one session, the `localpilot-server`
`host` module (`SessionHost`) gives them a shared view and lets control reach a
running turn without waiting for it:

- **Fanout.** A `SessionHost` owns a *session-lifetime* `broadcast` sender (not
  one per turn). `subscribe()` hands each connection its own receiver; a driven
  turn streams every `RuntimeEvent` into that one sender, so all attached
  clients — including one that attaches mid-turn — see the same stream.
  `broadcast` tracks its own receivers: a dropped client prunes itself and never
  errors the driver, and a client that falls more than the channel capacity
  behind observes `RecvError::Lagged` and resynchronises instead of stalling the
  turn. `subscriber_count()` reports the live attach count.
- **Out-of-band control.** `drive(input)` locks the session for the turn and
  publishes that turn's `CancellationToken` into a short slot *before* awaiting
  it. `cancel()` reads that slot and cancels the token; `steer(text)` pushes onto
  the runtime's steer queue (extracted once at construction). Neither takes the
  session's async mutex, so both land while `drive` holds it — cancel stops the
  in-flight turn at its next cancellation check (a safe boundary or an executing
  tool, both raced against the token), and a steer is admitted at the next safe
  provider-turn boundary as a user message. `is_busy()` / `status()` read the
  same slot, so a status snapshot never blocks on a running turn. A small
  `control(Control::{Cancel, Steer, Status})` dispatch maps a decoded control
  request onto these methods, ready for a transport to route control frames
  through.

This is the library surface the `serve`/`connect` commands drive over the wire
(see [Running the opt-in server](#running-the-opt-in-server-serve--connect)).

### Attaching to a session (server crate)

The server transport is **one connection = one session**: a connection names
its session exactly once, then is bound to it. It does that with an *attach*
handshake rather than tagging every message with a session id. The shared RPC
envelope carries one additive command and one additive confirmation:

- **`attach`** (`ClientCommand::Attach { target }`) is the first thing a
  connection sends. Its `target` is internally tagged by `mode`:
  - `open_new` — open and bind a brand-new session;
  - `resume_id` — resume the session with `session_id` (a UUID);
  - `resume_name` — resume the session carrying `name`.
- **`attached`** (`ServerEvent::Attached { session_id, server_version }`)
  confirms the bind and reports the id the connection is now bound to.

```text
→ {"v":1,"id":"1","command":{"type":"attach","target":{"mode":"open_new"}}}
← {"v":1,"id":"1","event":{"type":"attached","session_id":"…","server_version":"2.6.0"}}

→ {"v":1,"command":{"type":"attach","target":{"mode":"resume_name","name":"nightly"}}}
← {"v":1,"event":{"type":"attached","session_id":"…","server_version":"2.6.0"}}
```

Server-side, `localpilot-server`'s `attach(target, &registry, &factory, &store)`
routes `open_new` → `open_new`, `resume_id` → `resume_by_id`, and `resume_name`
→ `resume_by_name`, returning the bound `SessionId` (the caller then builds a
`SessionHost` from `registry.get(id)`). An unknown id or name is a typed
`AttachError` (`UnknownId` / `UnknownName`) the transport renders as an
`error` event — never a panic. Resume-by-id is guarded against a never-seen id
so it cannot silently mint an empty session under a caller-chosen id.

**Additive evolution.** `server_version` (the server build, `SERVER_VERSION`)
is `#[serde(default)]` and skipped when empty, so a payload written by an older
peer that predates the field still deserializes — the field simply fills in
empty — and a default value never bloats the wire. This is the discipline for
every field added hereafter: default it and/or skip it when absent, so
forward/backward compatibility holds without a second version handshake. The
explicit `RPC_PROTOCOL_VERSION` still gates wire compatibility in `hello`; the
serde-default discipline *coexists* with it for per-field evolution rather than
replacing it. The `serve`/`connect` commands drive this handshake — see
[Running the opt-in server](#running-the-opt-in-server-serve--connect).

### Running the opt-in server (`serve` / `connect`)

The server is off unless you start it, and it is scoped to one workspace: the
endpoint address is derived from the workspace root, so two projects never
collide. Nothing about `chat`/`ask`/`print`/`harness` changes — those stay
in-process (ADR/D003).

Start a server for the current workspace (foreground, `Ctrl-C` to stop):

```console
$ localpilot serve                 # uses the workspace's default model/provider
$ localpilot serve --model <name> --provider <id> --bypass
```

`serve` acquires a single-owner lock first: if a server is already running for
this workspace it prints that and exits cleanly rather than double-serving.

Attach a client (plain text: stdin lines become prompts; session events stream
to stdout):

```console
$ localpilot connect               # opens a new session
$ localpilot connect --resume <id|name>   # resumes an existing session
$ localpilot connect --server      # start a server first if none is running
```

Several `connect` clients can attach to the **same** session at once — open one
with `connect`, note the session id it prints, and attach another with
`connect --resume <id>`. Every attached client sees the same event stream (the
turn's text, tool activity, and stop), and any of them can drive a turn, steer
it, cancel it, or read `status`. In the plain-text client, a permission ask is
answered by typing `/allow` or `/deny`; `Ctrl-C` cancels the running turn.

Under the hood each connection sends one `attach` (open-new / resume-by-id /
resume-by-name), the server confirms with `attached`, and the connection is
bound to that session for its lifetime — the exact handshake documented above.
Detaching a client (EOF, or a `shutdown` command) leaves the session running
for any other attached client.

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
  it while the turn runs; it is admitted at the next safe provider-turn boundary.
  `push(text)` queues a normal user steer; `push_interrupt(SoftInterrupt { .. })`
  queues a typed soft interrupt carrying a source (`user` / `system` /
  `background_task`) and an `urgent` flag. A non-user message is labelled so it
  does not read as user-typed input; an urgent interrupt is admitted between tool
  calls (skipping the rest of the batch). A steer arriving as a turn would finish
  keeps the turn going so the model sees it. Every injection is recorded as a
  `SoftInterruptInjected` event in the session log. (The system/background-task
  producer path is a library surface for future work; only user steering produces
  interrupts today.)

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
--resume <id-or-name>] [--no-approvals] [--idle-timeout <MINUTES>]` serves the same session runtime as an
[MCP](https://modelcontextprotocol.io) server (protocol revision 2025-06-18),
so an MCP client — an agent host like Claude Code or Codex — can drive and
steer a LocalPilot session through ordinary tool calls. Register it like any
stdio MCP server (`command = "localpilot"`, `args = ["mcp", "serve", …]`).

**The server gives up after four hours with no client message** (ADR-0173).
End of input already ends a server whose client exited; this covers the client
that is *abandoned* rather than closed, whose pipe stays open as long as the
process holding it exists — a stale host has kept a server (and the binary it
runs from) alive for days. Any client message resets the window and a running
turn produces events, so a long turn is never cut off. `--idle-timeout <MINUTES>`
overrides the default; `0` waits forever.

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

The session also learns from being driven: the client's corrections — steers,
cancellations, and denials — are recorded as `driver_intervention` events in
the durable session event log (with the client's self-reported identity from
`initialize`) and offered on disconnect as review-gated lesson candidates,
labelled with the driving client so they never masquerade as the session's
own retrospective. See
[localmind-integration.md](localmind-integration.md#driver-interventions-ride-the-same-bridge).

### Registering the server with an agent host

Claude Code — project `.mcp.json` (trust it when prompted; reload to pick up
changes):

```json
{
  "mcpServers": {
    "localpilot": {
      "command": "localpilot",
      "args": ["mcp", "serve"]
    }
  }
}
```

OpenAI Codex — `~/.codex/config.toml` (or a trusted project's
`.codex/config.toml`); `codex mcp add` does the same interactively:

```toml
[mcp_servers.localpilot]
command = "localpilot"
args = ["mcp", "serve"]
```

Add `--model`, `--permission`, `--resume <id-or-name>`, or `--no-approvals`
to `args` as needed. The `events` wait cap (20 s) sits well under common
client tool-call timeouts; if a host configures a shorter tool timeout (e.g.
Codex `tool_timeout_sec`), pass a smaller `wait_ms` when polling.

### Driving well

A driving host gets better results — and better lessons — by treating the
session as a colleague, not a puppet:

- **Let it work.** Poll `events` with a generous `wait_ms` and read what the
  session is doing before reacting; a `tool_stuck` or repeated-failure
  pattern is a signal, a single imperfect step usually is not.
- **Steer sparingly and specifically.** One concrete correction ("run the
  failing test before editing") beats a paragraph of guidance. Corrections
  become review-gated lesson candidates verbatim — a sharp steer today is a
  reusable lesson tomorrow; a vague one is review-queue noise.
- **Teach, don't do.** Prefer a steer that changes the session's approach
  over cancelling and dictating the answer; a cancellation carries almost no
  reusable signal.
- **Answer asks deliberately.** `reply_permission` carries exactly the
  authority a human at the prompt would have — deny anything you would not
  have typed yes to. For unattended watching, start with `--no-approvals`
  so every ask denies.
- **Carry the session id.** Take it from `hello`/`status` and resume by id
  (`--resume <id>`); a session with no turns yet is not visible to
  `--continue`. After a reconnect, resume polling from your last cursor —
  the feed retained the events, and a poll that never got its reply before a
  disconnect lost nothing.
