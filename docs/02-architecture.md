# Architecture

## System Shape

LocalPilot is a set of Rust crates with a thin CLI binary.

```text
CLI/TUI
  |
  v
Session Runtime
  |
  +-- Harness Orchestrator
  +-- Tool Runtime
  +-- Provider Runtime
  +-- Store
  +-- Permission Engine
  +-- Recovery Engine
  +-- LocalMind Adapter
  +-- Skills Engine
  +-- Quota Scheduler
```

The runtime owns conversation flow. The provider runtime owns model calls. The
tool runtime owns local effects. The harness orchestrator owns project workflow.

The session runtime runs in one of two operating modes. Agent mode is a direct
conversational loop with no rule engine. Harness mode wraps the same loop in the
rule engine, commit policy, and replan loop. Both modes share the tool runtime
and the permission engine. The permission engine is configurable from
least-privilege (default) up to a bypass (allow-all) launch mode; the operating
mode does not change which profile is active.

## Crate Responsibilities

### `localpilot-cli`

Owns:

- command parsing
- top-level dispatch
- process exit codes
- human-readable command output

Must not own:

- business logic
- provider payload construction
- tool execution policy

### `localpilot-core`

Owns:

- domain types
- provider-neutral message model
- content blocks
- session IDs
- shared error types

Must remain:

- free of HTTP clients
- free of terminal UI code
- free of provider-specific names except generic enum variants

### `localpilot-config`

Owns:

- config schema
- config layering
- env var mapping
- redaction helpers

Config precedence:

1. command-line flags
2. environment variables
3. project `.localpilot.toml`
4. user config
5. built-in defaults

### `localpilot-llm`

Owns:

- provider trait
- stream event model
- provider registry
- official provider implementations
- local provider implementations

Provider implementations must live behind the same trait.

Provider implementations also expose quota metadata when available:

- current limit class
- reset time
- retry-after duration
- whether automatic resume is safe
- provider-visible error code/category

### `localpilot-tools`

Owns:

- tool trait
- tool registry
- JSON schema generation
- dispatch
- builtin tools

Builtin v1 tools:

- `read_file`
- `write_file`
- `edit_file`
- `list_files`
- `search_text`
- `run_shell`
- `git_status`
- `git_commit`

### `localpilot-harness`

Owns:

- brief parser/renderer
- progress parser/renderer
- intake role
- planner role
- worker role
- rule engine
- retry/discard/replan loop

The harness may call tools through interfaces. It must not bypass permission
checks.

The harness coordinates with the quota scheduler. If a step pauses due to a
provider quota window, the current committed state and plan remain authoritative;
the scheduler only resumes the next safe turn.

### `localpilot-tui`

Owns:

- terminal layout
- message rendering
- keyboard input
- approval dialogs
- status lines
- footer stats
- optional thinking/reasoning panel

UI stack (chosen; see ADR-0006):

- `ratatui` — terminal UI framework
- `crossterm` — cross-platform terminal backend (Windows, Linux, macOS)
- `tui-textarea` — multi-line input widget

`ratatui` is the committed TUI framework, not a suggestion. Alternatives are out
of scope unless a future ADR supersedes ADR-0006.

### `localpilot-store`

Owns:

- transcript persistence
- session indexes
- file-backed cache
- attempt logs
- redaction before persistence
- skill manifests
- quota wait records
- retention: prunes sessions and orphaned tool-output under a `RetentionPolicy`
  (ADR-0024)

Storage must be inspectable plain files where possible.

### `localpilot-localmind`

Owns:

- adapter between LocalPilot session records and LocalMind contracts
- session closeout into LocalMind
- accepted-memory retrieval for context injection
- CLI-friendly wrappers around LocalMind review, memory, audit, and skill APIs
- host-owned context-injection controls

Must not own:

- a second durable memory implementation
- LocalMind core learning rules
- SQLite schema details beyond calling LocalMind APIs

Memory and learning must remain local-only by design.

### `localpilot-skills`

Owns:

- skill discovery
- skill execution metadata
- skill suggestion heuristics
- generated skill drafts
- skill permission manifests

Auto-generated skills are suggestions until the user reviews and accepts them.

### `localpilot-recovery`

Owns:

- bad-output detection
- repeated-token loop detection
- stream abort/retry ladder
- provider degradation state
- recovery diagnostics

Recovery must prefer stopping safely over continuing with corrupted context.

### `localpilot-patchgen`

Owns the write half of the self-improvement loop (ADR-0034):

- isolated-worktree proposal generation (never writes `main`)
- scope/path containment and minimal-diff checks
- the `ApprovalToken`-gated promotion path (single human-only constructor)
- the change-provenance record carried with each proposal

**As shipped:** this crate is built and tested but **not yet wired to a caller**
— it is intentionally staged, not live. It has no CLI, harness, or adapter
entry point yet; the planned entry is a confirm-gated `localpilot self-review
propose-patch` command that produces a worktree proposal and **stops at the
`ApprovalToken` gate**. Wiring is deferred until the next self-improvement phase
(see ADR-0034's as-shipped note). The gate stays correct by construction
regardless: no autonomous path constructs a token.

### `localpilot-quota`

Owns:

- provider quota window tracking
- reset timers
- wait/resume scheduling
- unattended-resume policy checks
- persistence of paused harness runs

### `localpilot-rpc`

Owns:

- the headless-drive wire protocol: newline-delimited JSON over stdio
  (versioned commands in, streamed session events out)
- the ACP (Agent Client Protocol) adapter over the same runtime
- permission asks over the wire: the engine decides, the client only answers;
  an unanswered ask is denied like non-interactive mode
- the byte-level LF framing contract shared by both stdio protocols

Must not own: any HTTP server, permission decisions, or a product SDK — the
supported embedding surface stays the in-process session runtime
([`docs/embedding.md`](embedding.md)).

### `localpilot-sandbox`

Owns:

- permission rules
- permission profiles (default, relaxed, bypass)
- workspace path policy
- command risk classification
- platform sandbox integration

V1 should implement conservative policy without relying on OS sandboxing:

- never write outside allowed workspace roots without approval
- never delete recursively without explicit approval
- never run network commands without approval unless allowlisted
- never read secret-like files without approval

The default profile enforces these. The relaxed profile auto-approves a
user-defined allowlist. The bypass profile is a launch mode that disables
prompting entirely, like running fully localpilot, and is never the default.

### `localpilot-mcp`

Owns:

- MCP client protocol
- server lifecycle
- tool discovery
- resource reads
- permission integration

MCP is in scope for v1.

Remote agents, a web UI surface, and multi-repo orchestration are planned as
separate tracks after v1. They reuse the same session runtime rather than forking
it.

## Runtime Flow

### Normal Chat Turn

1. User submits message.
2. Runtime builds provider-neutral messages.
3. Tool registry exposes allowed tool schemas.
4. Provider streams response events.
5. Recovery engine watches for bad-output patterns.
6. Tool calls are routed through permission checks.
7. Tool results are appended to the conversation.
8. Loop continues until provider emits final answer.
9. Store persists transcript.

### Harness Resume

1. Load config.
2. Load `brief.md`.
3. Load `PROGRESS.md`.
4. Validate repo state.
5. Select next incomplete step.
6. Build worker prompt from the step and current state.
7. Run agent loop with tools.
8. Pause if provider quota requires waiting.
9. Run post-step rules.
10. Run tests if configured.
11. Commit if rules pass.
12. Mark step done and commit progress update.
13. Stop, continue, or schedule quota-reset resume based on mode.

## Data Model

### Messages

Messages are provider-neutral:

- role
- content blocks
- metadata

Provider adapters translate messages to the provider's official API format.
Reasoning/thinking blocks that a provider requires for continuity are stored as
message content, including signatures or provider metadata when needed, so the
next request can replay them through the adapter.

### Tool Calls

Tool calls are normalized:

- id
- tool name
- JSON input
- result text
- error flag

Provider adapters translate between provider tool-call formats and this model.

### Session State

Session state is split:

- durable transcript
- volatile runtime state
- project files
- provider metadata

Project files are authoritative for harness work. The transcript is supporting
context, not source of truth.

## Error Handling

Errors must be typed at crate boundaries:

- config errors
- provider errors
- tool errors
- permission errors
- harness validation errors
- store errors

The CLI converts errors to:

- short user message
- optional debug detail behind `--verbose`
- stable non-zero exit code

## Observability

Use `tracing`.

Default behavior:

- no remote telemetry
- local debug logs only when enabled
- redact tokens and secrets by default

Log levels:

- `error`: failed operation
- `warn`: recoverable risk or degraded mode
- `info`: major lifecycle events
- `debug`: payload metadata, never raw secrets
- `trace`: local-only deep diagnostics
