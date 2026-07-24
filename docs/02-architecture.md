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

The benchmark-facing eval primitives (the scorecard wire contract, discipline
metrics, blinded judge core, ablation scoring, gated check execution, and
verify-command detection) live in the shared `localx-eval-core` crate
(rev-pinned git dependency); the harness supplies the host-bound adapters —
session-trace derivation, the live judge model call, and the permission-engine
command gate.

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
- a hand-rolled multi-line composer (no third-party input widget), so cursor,
  wrapping, history, and paste behaviour are owned and testable

Rendering is inline in the terminal's main screen buffer, not an alternate screen
(ADR-0021): finished transcript blocks are written once into native scrollback, and
a fixed-height bottom band holds the only redrawn surface (ADR-0039).

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

### `localpilot-research`

Owns the **host-neutral** research loop (ADR-0060):

- the `Source`/`Synthesizer` traits and the bounded `run_research` loop
  (decompose → gather → adversarial cross-check → synthesise)
- the value types (`Provenance`/`Evidence`/`Finding`/`ResearchReport`), the
  Markdown report renderer, and the review-candidate spec
- the **pure** web-egress policy gate (`WebAccess`/`FetchDecision`/`host_allowed`/
  `AuditEntry`) — it decides whether a fetch is permitted and how it is recorded,
  but parses no URLs and performs no I/O

Must not own:

- any filesystem, network, or model dependency — the concrete sources
  (knowledge/memory/web), the model-backed synthesizer, URL parsing, the report
  writer, and the candidate enqueue live in `localpilot-cli`

Keeping the loop here (not in `localpilot-localmind`) holds the adapter boundary
(ADR-0036) and lets the security-sensitive gate be unit-tested with fakes.

It also owns the **host-neutral render contract** (ADR-0095): the render-signal
detector (`render_signal`), the `Renderer`/`RenderGate` traits, and the render
value/outcome types. The detector and traits are always compiled; the concrete
browser implementation lives in the optional `localpilot-render` crate.

### `localpilot-render`

Owns the **optional** browser-rendering fallback for research (ADR-0095), pulled
in by `localpilot-cli` only under the `render-browser` feature:

- an original, dependency-light Chrome DevTools Protocol client over a local
  WebSocket (`tokio-tungstenite`), and headless-browser discovery/launch with an
  ephemeral cookie-less profile — no browser is bundled or downloaded
- `ChromiumRenderer`, which implements `localpilot-research`'s `Renderer`:
  bounded navigate/settle/extract, CDP `Fetch`-domain request interception that
  gates every browser request through the caller's `RenderGate`, and
  same-origin/`srcdoc` frame extraction

Must not own the allowlist policy or audit: it consults the `RenderGate` the
binding layer implements over `WebAccess`, so there is one egress boundary, not
two. A build without the feature links no browser stack.

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

### `localpilot-selfreview`

Owns the read-only front of the human-gated self-improvement loop
(ADR-0034/0047/0053): the `observe → detect → propose` stages that scan a
repository for advisory health findings (drift, leftover markers, stale
decision indexes, incomplete plan rows, broken doc links, heuristic missing
tests), fold in model-emitted harness-friction findings, and rank everything
into one advisory report — plus the pure finding→draft-spec mapping for the
outward emitter.

Must not own: any write or publish path — it writes nothing; the
patch-generating half lives in `localpilot-patchgen` and publication is
`ApprovalToken`-gated in the CLI. Prior lessons are injected by the host, so
the crate carries no memory dependency.

### `localpilot-verify`

Owns deterministic verification of executed tool calls against their
contracts: after a call runs, a `Verifier` turns the recorded result into a
`Verdict` (`Verified`/`Unverified`/`Failed`) so the loop can refuse a
"success" claim no postcondition supports. Deterministic-first; an effect a
contract marks unverifiable is recorded as unverified, never as success.

Must not own: command execution or permissioning — it judges outcomes the
runtime observed. (Distinct from the `verify_before_done` finalize gate,
which reuses the quality-gate `CheckRunner`.)

**As shipped:** this crate is the write half of the self-improvement loop and is
**wired** — reached only through the confirm-gated `localpilot self-review
propose-patch` / `promote` / `discard` commands. `propose-patch` has a model
author a minimal, scope-confined edit for a ranked finding into an isolated
worktree and **stops at the `ApprovalToken` gate**; `promote` applies it onto
`main` only when an explicit human `--approve` mints the token (fast-forward
only, never pushes); `discard` drops the worktree/branch. A proposal persists
across invocations via its on-disk worktree plus its provenance record, so a
human reviews the diff between proposing and promoting. The gate stays correct
by construction: the sole `ApprovalToken` constructor is the explicit-human
`--approve` path, so no autonomous path constructs a token (see ADR-0034's
as-shipped note).

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
