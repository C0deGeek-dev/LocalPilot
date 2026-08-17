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

The opt-in `localpilot pair` interface composes two independent Agent-mode
session runtimes over one workspace; it does not add a third operating mode.
An exact-two topology provides bound one-to-one messaging, a finite typed
convergence driver schedules one model turn at a time, and one CLI-owned
full-screen host projects both sessions and their attributed user interactions.

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

### `localpilot-llm` (+ `-core`, `-openai`, `-anthropic`)

The provider layer is split across four crates so editing one adapter recompiles
only that adapter, not the whole layer or its downstream dependents:

- **`localpilot-llm-core`** — the shared contract: the provider trait, the stream
  event model, request/response shapes, the error taxonomy, auth, and header
  parsing. Depends on no adapter, so there is no dependency cycle.
- **`localpilot-llm-openai`** / **`localpilot-llm-anthropic`** — one hand-written
  adapter each, depending only on `-core`.
- **`localpilot-llm`** — the umbrella: the provider registry (the seam that wires
  the adapters), model discovery, vision resolution, and the test `FakeProvider`.
  It re-exports the whole public surface, so `harness`/`cli`/`rpc`/`quota` import
  everything as `localpilot_llm::…` unchanged.

Owns (across the four crates):

- provider trait
- stream event model
- provider registry
- official provider implementations
- local provider implementations
- a shared, poison-recovering reasoning-effort handle, seeded from session
  config and snapshotted immediately before each provider request

Provider implementations must live behind the same trait, each in its own adapter
crate depending on `-core`. Editing an adapter re-checks its ~1.5k-line crate in
isolation (the sibling adapter and `-core` are untouched); a full `--workspace`
build still recompiles the downstream spine through the umbrella.

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

### `localpilot-agents`

Owns the **data** half of declarative subagents:

- parsing and validating a subagent definition (a YAML file, not compiled in)
- discovering definitions with the same precedence users know from skills
- resolving a definition's tool list into the child's actual grants by
  intersecting it with the parent's — a subagent's authority is always a subset
  of the caller's

Must not own: execution. Running a child session needs the harness (which depends
on this crate), so containment is structural — a subagent is a bounded child
session with its own context window, prompt, and always-narrower tool set.
Subagents are not skills: a skill is text the model may read and grants nothing; a
subagent is an execution with authority. The two share no loader, registry, or
file format.

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

### Terminal UI crates

`localpilot-terminal-ui` is the new authoritative, backend-neutral full-screen
chat model. It owns:

- stable-ID timeline, content-coordinate viewport and selection state
- width-indexed visible-row virtualization, pinned prompts, compact activity,
  and held/new-output state
- grapheme/display-width editor geometry and input routing
- bounded question-editor geometry: free-text answers reuse the shared wrap and
  caret projection, grow within the modal, then expose a proportional vertical
  scrollbar without changing the answer source of truth
- lifecycle/focus state, including cancel/exit intent
- one responsive frame layout/hit map and one semantic theme resolver across
  default, dim, high-contrast, colorblind, and no-color rendering

Tool activity uses the same retained-text/visible-projection split as the rest
of the timeline. `AppModel` converts provider-neutral lifecycle events into an
original sanitized headline plus the bounded envelope-free result; runtime and
store payloads do not change. Typed tool presentation metadata records source
and retained line/byte counts, terminal bounding, and headline metadata byte
boundaries beside that retention decision. `Timeline::row_ranges` remains the sole cell-aware
geometry authority. Running, successful, and cancelled collapsed tools project
the first four wrapped visual rows. Failed tools project the first two and last
six retained wrapped rows, with the skipped count and tail boundary carried as
projection metadata over the original stable item ID and byte-coordinate
space. Visible-only copy walks those source segments and inserts an omission
marker only when a selection crosses their discontinuity. Expansion
uses the complete retained rows, and search expands before anchoring a match
that was outside the compact projection. Disclosure derives its hidden-row count
from those same width-indexed ranges. The renderer only supplies semantic
headline/body/diff styles, first/intermediate/last prefixes, and a synthetic
wide-only metadata gap whose hit mapping resolves back to the original byte
boundary. The tail ellipsis is likewise a prefix over the mapped source row,
not synthetic timeline text; it does not
reparse provider events or create a second tool-card model.

Tool focus is one optional stable `ItemId` owned by `Timeline`, projected onto
every visual row of that item and rendered with shape plus semantic focus cues.
The Crossterm host maps F7/F8, Enter, Escape, and prefix clicks into typed
`ToolAction`s, but `AppModel` is the single geometry-aware activation seam.
Focus movement reveals a headline in content coordinates; toggling snapshots
the viewport start and restores it after reflow, clamped once against the new
bottom. Any ordinary composer action releases tool focus before normal input
routing. `TimelineDensity` is defined by `localpilot-config` and reused by the
terminal model, settings projection, and host. `compact` reproduces the shipped
Tool-to-Tool geometry, `comfortable` owns only the optional spacer between
adjacent tools, and the separate Tool-to-Assistant/Reasoning spacer is an
invariant rather than a density preference.

Optional successful-run grouping is another projection over the same raw item
vector, never a storage transform. `Timeline` detects runs of at least three
consecutive successful `Tool` items and gives the head entry a typed synthetic
summary row; collapsed members retain index identity but contribute zero visual
height. A `TimelineFocusTarget` distinguishes original tools from group
summaries, so both travel through the existing geometry-aware `ToolAction`
seam. Expanded groups project the summary followed by each original tool.
Collapsed-member anchors resolve to the summary, search expands the containing
group before resolving original bytes, selection treats the summary as
non-source and inserts a counted group marker when it crosses hidden members,
and the exit transcript continues to walk raw items. The config default is off,
so no migration or provider/store change exists.

Assistant progress hierarchy is metadata over an ordinary `Assistant` item.
Every streamed assistant segment starts as `AssistantPresentation::Answer`; a
subsequent `ToolStarted` update is the only proof point that changes the open
segment to `Progress` before its pointer is retired under ADR-0141. Both states
keep the same item kind, text, styles, stable ID, byte coordinates, wrap width,
and transcript/export label. The renderer alone maps Answer to the filled accent
dot and Progress to a muted hollow dot plus an explicit screen-reader label.
Consequently the retroactive cue change cannot move an anchor, selection, or
row, and the last assistant segment remains Answer when the turn stops.

The default resolver owns the application canvas, raised prompt/composer
surface, prompt text, muted text, focus edge, scrollbar, and tab roles. Prompt
and composer bands are filled surfaces assembled from terminal cells; they are
not long accent-colored rules. Pinned prompts use the same three-row projection
as prompts in the conversation flow so scrolling cannot change their visual
identity.

It depends on Ratatui's backend-neutral APIs but not Crossterm, the harness,
providers, the store, or the CLI. `localpilot-cli` owns the Crossterm alternate-
buffer lifecycle, raw event and clipboard adapters, and maps the existing
provider-neutral runtime/approval/cancellation streams into terminal UI actions.
Its async event pump mirrors the established inline runtime seam: each turn owns
one broadcast receiver and cancellation token, approvals are deny-safe, and
terminal input is drained in bounded batches. During active operations one
short-lived reader thread preserves Crossterm's poll/read affinity and wakes the
async pump immediately; the independent 50 ms tick drives progress and activity
frames, and missed ticks are skipped rather than replayed. The reader is stopped
and its channel drained before completion projection so a boundary Enter cannot
be lost. The Windows unbracketed-paste probe is non-blocking for ordinary keys.
Once `Event::Paste` proves bracketed-paste support, the legacy heuristic retires
for the session; until then an embedded Enter is paste content only when a run
is under way and more input is already queued behind it, judged from the input
queue and the 150 ms continuation window rather than from how long the loop
took to process the run (ADR-0157). Flushing staged text to the composer does
not end that window, so a chunked paste is classified once; a final Enter with
nothing queued behind it remains the normal submit.
The backend-neutral projection stores only a sanitized operation label and a
monotonic start instant; render derives heartbeat frames and elapsed text, so no
second timer task or mutable animation counter can drift from work lifecycle
(ADR-0148). Prompts submitted during a turn
are visible stable timeline items marked pending. Escape promotes the leading
contiguous plain-text prompts into urgent soft interrupts, preserving FIFO order;
the open provider stream is dropped and the same runtime turn restarts with the
new user direction. Its incomplete assistant segment remains visible as
interrupted feedback but is not persisted as model history. Shell operations and
image prompts are ordering barriers: when one is at the queue head, Escape
hard-cancels the current work and leaves every follow-up in its original order
instead of steering a later prompt past it. Ctrl+C is a staged path: selection
copy first; otherwise atomically stash and clear a nonempty composer; then cancel
active work on an empty composer; then exit on the next consecutive press. Other
input disarms exit. Runtime output is inserted before later pending operations,
so visible and provider transcript order agree. A newly opened assistant or
reasoning item strips only leading CR/LF provider framing and drops an all-
whitespace opener; later deltas append unchanged, including leading newlines.

For `localpilot pair`, the same backend-neutral model holds two cohesive session
projections inside one shared application shell. At widths of 61 columns or
more, the timeline is split into labelled peer panes; below 61 columns, only the
active peer is shown at full width and F6 switches peers. Each projection keeps
its own timeline, viewport, search, selection, runtime status, and usage, while
the composer and dialogs are shared and explicitly target or identify a peer.
`FrameLayout` remains the sole drawing and hit-test authority, so an off-screen
peer has no stale pointer targets and resize does not move state between peers.

The inline chat host and its `localpilot-tui` crate have been retired
(ADR-0154), superseding the inline main-screen-buffer rendering of
ADR-0021/ADR-0039: `localpilot chat` resolves only to the full-screen
application, which owns:

- terminal layout
- message rendering
- keyboard input
- approval dialogs
- the question widget (`ask_user` and the intake guidance gate both drive it)
- status lines
- footer stats
- optional thinking/reasoning panel

UI stack (chosen; see ADR-0006):

- `ratatui` — terminal UI framework
- `crossterm` — cross-platform terminal backend (Windows, Linux, macOS)
- a hand-rolled multi-line composer (no third-party input widget), so cursor,
  wrapping, history, and paste behaviour are owned and testable

The host uses an alternate buffer, full-frame rendering, captured mouse input,
and application-owned content selection (ADR-0107). `ratatui` is the committed
TUI framework, not a suggestion. Alternatives are out of scope unless a future
ADR supersedes ADR-0006.

### `localpilot-slash`

The single source of truth for the slash-command surface across the full-screen
picker and pair picker. Owns:

- the parser (`parse_slash_for` and its helpers, plus
  `Mode`/`Profile`/`SlashAction` and the argument shapes) — lookup-first typed
  dispatch, host-aware: the full-screen and pair hosts route the five shared
  takeover commands (`help`/`theme`/`settings`/`diff`/`search`) to real actions,
  while the pair-only `/abort` stays external to every host. Full-screen alone
  adds the CLI-injected, engine-neutral `/localmind` workspace tab (ADR-0152); the
  presentation crates remain free of LocalMind dependencies. The full-screen
  host also runs `/compact`, the long-running `/ingest` runs, `/research`, and the
  `/harness-resume` / `/wait-resume` resume commands on its
  operation pump (a UI-agnostic progress lane surfaces ingest milestones without the
  operation and the pump both mutating the model). The full-screen host owns a typed
  live session mode (`localpilot_slash::Mode`) — bare `/research` enters a persistent
  research mode — and captures a per-prompt `PromptKind` at enqueue time so a
  mid-queue mode switch cannot retroactively reinterpret an already-queued prompt.
  Interactive `/research` shares one prepared config snapshot between the shown egress
  disclosure and the run, so what is disclosed is exactly what the run may reach. The
  completion boundary projects the full report into one redacted, 4-KiB finding/source
  index and asks `SessionRuntime` to append its topic/result exchange through the normal
  transcript and event-log authority. The full-screen host renders that same stored
  result as assistant content; resume admits only this named synthetic origin and keeps
  other runtime repair messages hidden (ADR-0149). The
  resume commands enter `Mode::Harness` at dispatch and snapshot the live model,
  provider, permission profile, and a retained single-host session-trust grant then
  (never launch-time), running an inner runtime whose approvals reach the host through a
  cloned `approval_tx` — the only new field on the host context
- one globally-ordered `SLASH_SPELLINGS` table: each spelling maps to a semantic
  `SlashCommand` id and carries an `ArgSpec`, a `StrayArgs` policy, and a
  per-host `Option<&str>` description
- `specs_for(Host)`, which projects the table into any one host's catalog in
  global order, so no host keeps a private list (ADR-0144)
- `SlashAction::runs_live(Host)`, the single active-turn policy: the full-screen
  host runs profile, background, effort, and reasoning-visibility controls plus
  its safe takeovers live; pair remains unchanged

Must remain:

- dependency-free (no other workspace crate, no third-party crate) so both
  `localpilot-cli` and `localpilot-terminal-ui` can depend on it without a cycle
- free of rendering and I/O — it defines *what* commands exist and whether a
  host may run them live, not *how* a host services them

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
- the per-file declaration adapter and the `search_definitions` tool (ADR-0104):
  the shared code-intelligence grammars are reached from here because this is the
  only crate holding both sides, while the file walk, ignore handling, and
  workspace scoping stay host-side per ADR-0036

Must not own:

- a second durable memory implementation
- LocalMind core learning rules
- SQLite schema details beyond calling LocalMind APIs
- a second parsing stack, grammar registry, or language table

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

### `localpilot-dist`

Owns the **on-disk contract** for what is installed, which version runs, and how a
new one lands — the reuse base the self-dev store and the updater share:

- the version-per-directory cache (every version in its own directory, so
  switching is a rename and rollback is free — the only layout that behaves the
  same on Windows, where a running executable cannot be replaced in place)
- the install marker (its presence makes a version resolvable) and SHA-256
  verification recorded at install, not re-checked on the hot path
- resolution order (newest / pinned / rolled-back) and the pin/rollback state
- a `PATH`-visible `bin/` refreshed from the resolver on every change, by copy
  (a symlink needs a privilege on Windows; a copy works everywhere), replaced
  rename-then-copy so a running executable can be moved aside and swept later

Must not own: the download. It deliberately reaches no network — it is the small,
testable on-disk half the networked updater commits into.

### `localpilot-selfdev`

Owns the primitives for building LocalPilot from its own source and swapping
onto the result:

- `SourceState` — a content fingerprint of the working tree (commit hash,
  status, diff, and untracked file *contents*) reduced to one stable
  `version_label`, so "which bytes" is answerable and not just "which commit"
- an isolated build (own cargo profile, own target directory, a job count that
  leaves a core for the running session) that passes the source identity to the
  build script as environment rather than making it watch `.git`
- an immutable version store (`versions/<label>/`, copy-in, never overwritten)
  and marker-file channel pointers, so a running process is never launched from a
  path a later build can overwrite
- a publish gauntlet that refuses to promote a stale or broken build (identity +
  freshness + a real RPC handshake)
- the reload primitives: a durable, idempotent, non-consuming continuation intent,
  and a one-seam relaunch (`exec` on Unix, spawn-then-exit on Windows) that swaps
  onto the new binary and lets the session continue itself on the far side

Must not own: the decision to reload. This crate makes each step safe to take;
whether to take it is the caller's policy (the opt-in self-dev surface, off by
default).

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

### `localpilot-selfimprove`

Owns the **thin orchestrator** that wires the three stage crates above into one
human-gated loop, without merging them (ADR-0138). It sequences their existing
entrypoints in a fixed order and surfaces the loop state; it holds only
sequencing and state, no stage logic:

```text
review ─▶ propose ─▶ [ human ApprovalToken gate ] ─▶ promote ─▶ build+gauntlet ─▶ reload
Found  ─▶ Proposed ─▶            (hard stop)        ─▶ Approved ─▶    Built     ─▶ Reloaded
```

- a `contract` of five linear states, each bound to the *existing* entrypoint
  that produces it, with the Proposed→Approved crossing representable **only**
  with an `ApprovalToken` — so "no autonomous advance past the gate" is a
  property of the types, not a convention
- an `Orchestrator` that advances one step at a time, persists the loop state
  under `.localpilot/selfimprove/` (git-ignored, resume-safe across processes),
  and after approval builds the **approved, merged tree** — never the patchgen
  proposal worktree
- a `SelfDevStage` seam whose real implementation delegates to
  `localpilot-selfdev` (`build_gauntlet_promote`, `relaunch`) and reuses the
  existing rollback circuit breaker; a failed self-dev advance drives that
  breaker, and the orchestrator adds no rollback logic of its own

The stages stay separate because they have different blast radii: source
mutation with a human-merge gate (`patchgen`) versus a compiled-binary lifecycle
with a build gauntlet and rollback breaker (`selfdev`). Merging them would couple
two unrelated concerns and blur the human gate (ADR-0138).

Must not own: minting the `ApprovalToken` (only an explicit human approval path
does), and any autonomous build→reload path — the unattended loop stays deferred
(ADR-0128). It is surfaced by `localpilot selfimprove status` / `next` and by the
two interactive hosts through `/selfimprove`. Both surfaces call the same
orchestrator and persisted state. Chat lists multiple findings before requiring
an explicit rank, displays the persisted proposal before a reviewer-bound
approval confirmation, advances build/reload one requested step at a time, and
performs a confirmed reload only after restoring terminal modes (ADR-0151).

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

### `localpilot-taskgraph`

A pure task-graph engine: the plan several workers agree on, plus every rule that
keeps that plan coherent while they mutate it concurrently. It is a **leaf crate
with no LocalPilot dependencies** — no I/O, no sessions, no models, no tools — so
a whole plan can be driven from seed to settled by a deterministic simulator with
no live agents attached. Wiring it to real workers is the server's job, never
this crate's.

Owns:

- the graph: tasks and review gates, edges stored only as "what this waits on"
  (dependents are derived, so the two directions cannot disagree), and a plan
  `version` that increments on every accepted mutation. A task may carry an
  optional **model** — an opaque `Option<String>` (`None` = the worker session's
  default) the crate never interprets; whoever turns a node into a real worker
  resolves it. It defaults on deserialize, so a plan snapshotted before the field
  existed still loads.
- validated mutations — `seed` (idempotent under a caller-supplied key),
  `expand_node` (a task becomes a *join* over its children rather than being
  replaced, so nothing downstream is rewired), `complete_node`, and
  `inject_from_gate` (a review raises findings by adding work and then
  re-reviews) — plus the supervisor operations `fail_node`, `abandon_node`, and
  `salvage_assignment`
- the four invariants every mutation satisfies: ownership (only a task's owner
  or its current assignee may change it), acyclicity (checked before an edge is
  written, and for a batch of new tasks before any of them is created),
  terminality (a finished task never changes; rework enters as new tasks), and
  honest completion (a completion carries a typed `HandoffArtifact`; in deep mode
  it must also state what was *not* checked, and a gate must say how it reviewed
  and cite something)
- derived scheduling: `ready_nodes` (deterministic, id-ordered — the same plan
  yields the same frontier everywhere), the third state `Blocked` for a task
  whose upstream ended badly, `cascade_blocked` to settle a stranded tail rather
  than hang on it, and `assemble_input`, which hydrates a task's upstream
  handoffs into its prompt so a worker reads what earlier tasks found instead of
  re-deriving it
- a deterministic simulator (`sim`): no clock, no randomness, no task scheduling
  — each round takes the ready frontier in id order and resolves it in dispatch
  order, so a plan that misbehaves here is the engine's fault and a plan that
  only misbehaves live is not

Must not own: spawning, transport, persistence, prompts, or anything that knows
what a worker *is*. `PlanMode::Deep` decides how strict the rules are; it does
not decide who runs them.

### `localpilot-server`

The opt-in, single-machine local server behind `serve`/`connect`: a
cross-platform framed local-IPC transport, the daemon lifecycle around it, and a
process-local registry that hosts many `SessionRuntime`s at once for multiple
attached clients. It is strictly opt-in — the default in-process
`chat`/`ask`/`print`/`harness` path never touches it (D003).

Owns:

- a deterministic per-workspace endpoint scheme: a Unix domain socket under the
  runtime dir (`$XDG_RUNTIME_DIR`/`$TMPDIR`/`/tmp`, `sun_path`-length checked)
  or a Windows named pipe, keyed by a short stable hash of the canonical
  workspace root, overridable by `LOCALPILOT_SERVER_SOCKET`
- a uniform `Listener`/`Conn`/`connect` transport surface, identical across
  platforms (`UnixListener`/`UnixStream` on Unix; `named_pipe` server/client on
  Windows, with the create-next-instance-before-accept pattern), framed with
  `localpilot-rpc`'s LF-delimited NDJSON codec reused as-is
- daemon lifecycle: detached spawn of the current executable (new process group
  on Unix; `DETACHED_PROCESS | CREATE_NO_WINDOW` on Windows; null stdio), a
  bounded retry-connect ready handshake, and single-owner exclusivity
- one-owner exclusivity with stale-endpoint reaping: an atomic exclusive-create
  lock file next to the socket on Unix, the first-pipe-instance flag on Windows;
  a failed acquire probes for a live daemon and either reuses it or reaps a
  stale socket/lock and retries
- a session registry keyed by `SessionId` (a structural `RwLock` over the map
  plus a per-session async `Mutex` held for the whole of a turn), a per-session
  `SessionHost` (multi-client event fanout over a session-lifetime broadcast,
  plus lock-free out-of-band cancel/steer), and the connection-scoped attach seam
  (open-new / resume-by-id / resume-by-name → the bound session id)

#### Swarm state (opt-in)

Beside the session registry — never inside it — sits the swarm layer. A session
is a session whether or not it is collaborating, so nothing here is on the path a
single-agent turn takes.

- **Scoping.** A swarm is identified by the *repository*, not the path: every
  git worktree of one repo resolves to one swarm, so a worker spawned into a
  worktree joins the coordinator's swarm rather than founding an invisible second
  one. Resolved by reading git's own on-disk contract — `.git` as a directory, or
  as a `gitdir:` pointer file whose target names its `commondir` — with no `git`
  subprocess, since a swarm id is needed on every spawn. Outside a repository the
  canonical directory path is used, so non-git workspaces work rather than
  erroring. `LOCALPILOT_SWARM_ID` overrides the whole resolution.
- **Membership.** Members are keyed by `SessionId` and carry a status and role.
  A hierarchical swarm uses coordinator/worker roles and **one** structural edge
  per worker: who it reports back to. Children, ancestry, and subtrees are all
  derived by walking that edge — a stored child list would be a second copy of
  the same fact and would disagree the first time a member departed. A pair uses
  exactly two `Peer` roles and no hierarchy edges. The two topology kinds are
  mutually exclusive. A reverse index maps a session back to its swarm, because
  a tool call knows only its own session id.
- **Caps and admission.** Two bounds: a *lifetime* member cap (which counts
  departed members, so a coordinator that keeps replacing failed work is still
  stopped) and a *concurrency* budget on running members. Both are checked and
  the slot taken under one write lock, as a **reservation**: a spawn reserves,
  builds the worker, then confirms or releases. Checking a cap and inserting
  afterwards would let a burst of concurrent spawns all read the same count and
  all proceed. Idempotency keys are answered from the reservation table as well
  as the member table, so a retry whose first attempt is still building is told
  so rather than starting a second worker.
- **Workers.** A swarm worker is an *ordinary hosted session with a swarm edge*
  — not a second process, not a second loop, and not a special case anywhere on
  the session path, so the registry, the `SessionHost`, cancel/steer, and the
  reaper all work on it unchanged. Building the session is behind a
  `WorkerFactory` the host supplies, because narrowing tools to the spawner's,
  attributing permission asks to the spawner's approver, and resolving a provider
  all need wiring the server crate does not have. The production factory lives in
  the CLI (`swarm_cmd`), where a provider and a model actually exist: it builds
  each worker through the same `SessionSetup` recipe `serve`/`rpc` use — an
  ordinary headless session — on the model the spawn asked for.
- **Per-worker model.** A spawn may name a model, and each worker runs on the one
  its plan node asked for (`None` = the session default). Two gates keep this
  honest, one cheap and early, one exact and late. *Before* the build, the host
  checks a **configured provider serves that model**; a model none advertises is
  refused (`ProviderUnavailable`) rather than quietly built on a default — the
  slot it reserved is released. *After* the build, if the session landed on a
  different model than asked, the spawn is **refused** (`ModelMismatch`): running
  anyway produces work that reads normally and never says the wrong model
  produced it. A multi-provider config routes each advertised model to its own
  provider, so different workers can run on models served by different providers.
- **Flow-back.** When a worker's turn ends, its final assistant text is bounded
  (the same 4 KiB the in-process delegation path uses), recorded on its
  membership, and injected into whoever it reports back to as a
  `BackgroundTask`-sourced soft interrupt — labelled, so a coordinator can tell a
  worker's report from something its user typed. If the spawner is no longer
  hosted the report is still recorded, because a re-elected coordinator will need
  it.
- **The plan.** A swarm's `TaskPlan` (see `localpilot-taskgraph`) is read,
  mutated, and stored under one write lock rather than by
  read → change → write-back, which would leave a gap in the middle of exactly
  the state that cannot afford one.
- **Running a plan.** `localpilot swarm run <plan>` is the production entrypoint:
  it resolves the workspace's providers once (the shared `SessionSetup` recipe),
  builds the host over the CLI's production `WorkerFactory`, adopts a coordinator
  as the swarm root, seeds a plan read from a JSON file (objective, mode, and
  nodes — each with an optional per-node model), and runs it to completion. This
  is the only user surface that spawns workers; the model-callable `swarm` tool
  stays messaging-only, so a mid-turn agent cannot fan out on its own (that is
  autonomous-loop territory, out of scope). A worker per ready task, each on the
  model its node asked for, refilling as workers finish. The fan-out is bounded
  at the admission seam — `--concurrency` caps how many workers (and so how many
  models) run at once, the RAM/VRAM bound; `--max-agents` is the lifetime runaway
  guard — resource containment (`SwarmLimits`), never a token or spend budget.

#### Pair collaboration (opt-in)

`localpilot pair <task>` adopts two fresh sessions atomically into the exact-two
topology. Both receive the same original task and workspace, but each retains an
independent runtime, history, tool registry, permission engine, approval and
question channels, provider, and model. Only cloneable provider/MCP capability
sources and the resolved agent set are shared. Pair admission rejects hierarchy
operations and a hierarchical swarm rejects peer admission.

The convergence core accepts only versioned typed `propose`, `revise`, and
`agree` envelopes. It owns the canonical candidate, monotonic revision, digest,
and per-peer agreement; prose and peer-claimed identity cannot change protocol
state. The driver serializes all model turns, sends the prior envelope directly
to the other peer as a non-waking system notification, and stops with one typed
outcome at agreement, round/slot/token bounds, abort, failure, protocol error,
or no progress.

The CLI constructs both sessions through the ordinary interactive-session
recipe, binds the real `SessionHost` and sender-scoped messaging view to each
endpoint, and owns the one Crossterm event loop. Runtime events, approvals, and
questions retain their peer identity through the host; user steering targets one
named host and remains user-sourced. Cooperative shutdown aborts the driver,
cancels and awaits both sessions, fails outstanding asks closed, restores the
terminal, and never applies or commits the candidate automatically.

Design constraint: **safe-only lifecycle primitives.** No `unsafe`, no
`libc`/`nix`, no `flock`/`setsid`/`kill` — only safe `std` + `tokio`
(`process_group`, `creation_flags`, `create_new`, `PermissionsExt`). The
transport is opt-in and sits alongside — never replacing — the stdio embedding
surface.

#### Multi-session RAM model

One `serve` process hosts many sessions cheaply because the heavy, immutable
inputs are a **shared pool**, resolved once at start-up and cloned (an `Arc`
bump) into every session, while only the light, mutable per-session state is
built per session:

- **Shared, one per server** (captured in the CLI's `SessionSetup`, injected
  into each `factory.create()`): the provider stack (`Arc<dyn ModelProvider>`)
  and the connected MCP pool — the spawned MCP server subprocesses and their
  transports are launched **once** and held for the life of the setup. Each
  session projects a *fresh* `ToolRegistry` from that setup, but the registry
  only references the one pool's MCP clients (`Arc<dyn Transport>` clones) — MCP
  servers are never re-spawned per session. See [`mcp.md`](mcp.md).
- **Per session, mutable** (isolated so no state bleeds between sessions): the
  `SessionRuntime` itself with its transcript, compaction cache, and
  `SessionConfig`, a fresh permission engine and workspace read-roots, and a
  fresh wire approver. A turn on one session can never appear in another.

The measured cost of an extra session is therefore only its mutable state — on
the order of tens of KiB of resident memory, not the megabytes a fresh provider
or MCP pool would add.

**Reaping.** A periodic reaper keeps the resident set bounded by removing
sessions no client needs any more. A session is reaped when either its last
client detached at least a grace period ago, or it has been idle past a timeout
— but **never** while a turn is in flight: busy-safety is the per-session mutex
itself (a running turn holds it for the whole turn), so the reaper only closes a
session it can `try_lock`, and it persists the event log
(`SessionRuntime::close` records `SessionClosed`) **before** removing the session
from the registry and host map. The scan holds the host-map lock — the same lock
an attach takes — so no client can bind a session between the decision to reap it
and its removal. On clean shutdown the reaper stops and every remaining session
is persisted and dropped before the endpoint is released.

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

Host-derived research topics and results use `Role::User`/`Role::Assistant` with
the stable synthetic origins `research topic` and `research result`. Those
origins distinguish the derived prompt and projected evidence from
human/provider-authored prose without creating a second persistence path or
changing the stored event schema. Unlike repair notices, both are intentionally
model-visible and replay-visible (ADR-0149).

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
