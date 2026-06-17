# Architecture Decision Records

This file starts the decision log. Add new records at the top.

## ADR-0025: Ingested Chunks Live In An Indexed SQLite Store

Status: accepted. Builds on ADR-0013 (folder ingestion uses disposable
project-local artifacts) and ADR-0017 (retrieval context is a request-time
projection).

Folder ingestion persisted every derived chunk in a single `chunks.json` under
`.localmind/ingest/`. Every search and every refresh deserialized the whole file
into memory and scanned it linearly, so a large repo paid a full-RAM load and an
O(n) scan on each query — the opposite of the "lean on modest machines" goal.

Derived chunks now live in an embedded SQLite store at
`.localmind/ingest/chunks.sqlite` with an FTS5 virtual table, versioned by a
`PRAGMA user_version` stepper — the same pattern the accepted-memory store uses.
Search narrows to the matching rows through the FTS index (bounded by a
relevance-ordered limit), then recomputes the existing term-count +
path-name-boost score over just those rows, so ranking is unchanged while the
whole index is never loaded. Refresh updates only the paths that changed:
unchanged files are reused by `path:content_hash`, a changed file's prior rows
are kept as stale tombstones pointing at the new hash, and a vanished file's rows
are tombstoned with no successor. An existing `chunks.json` migrates into the
database on first open and is then removed; `ingest rebuild` recreates the store
from source. Only the large chunk index moved — the small manifest/job/review/
last-pack files stay JSON.

Reason:

- the persisted index exists to keep retrieval lean on modest machines; a
  full-RAM load plus linear scan on every query defeats that, and an indexed
  store fixes it without changing the chunk model or the ranking contract
- SQLite + FTS5 is already in the dependency tree and proven by the
  accepted-memory store, so the chunk store reuses a known-good, offline,
  extension-free pattern (rusqlite `bundled`) rather than inventing storage
- the store is derived and disposable (ADR-0013): migration is one-way and
  rebuild is always a valid fallback, so the change carries no durable-data risk

## ADR-0024: Session Store Has A Conservative Default Retention

Status: accepted. Builds on ADR-0011 (store convergence: the execution record)
and ADR-0012 (`.localpilot` is local, disposable, never committed).

The project-local `.localpilot/` state grew without bound: one transcript and
event-log pair per session, and one `tool-output/<id>.txt` snapshot per tool
call, none of it ever removed. A `RetentionPolicy` (`max_sessions`,
`max_age_days`; `0` = unbounded on that axis) now governs cleanup. `Store::prune`
removes the sessions outside the policy, trims the index, and sweeps any
tool-output snapshot no surviving session still references (a mark-and-sweep over
survivors' tool-call ids plus their `recovery-<id>` snapshot — no mtime
heuristics). It is exposed as `localpilot session prune [--keep] [--older-than]
[--dry-run]` and run best-effort at interactive chat startup.

A conservative cap is **on by default** (`[storage]`: 100 sessions, 90 days,
`auto_prune = true`) so the directory cannot grow forever without anyone opting
in. Both limits and the auto-prune are configurable, and `0`/`false` disable
them.

Reason:

- unbounded growth is a real disk and inspectability problem; a default cap fixes
  it for users who never touch config, the common case
- retention is the store's concern, so the policy and the mark-and-sweep live in
  `localpilot-store` behind one `prune` entry point rather than scattered deletes
- deletion of user history is sensitive (the privacy model treats inspect/delete
  as user controls), so cleanup is best-effort, silent, fully configurable, and
  has an explicit `--dry-run`; cache and provider metadata stay out of scope

## ADR-0023: Deterministic Result Verification In A Thin `localpilot-verify` Crate

Status: accepted. Builds on ADR-0010 (the runtime validates and controls) and
ADR-0001 (narrow crates, one-way dependencies).

The permission engine controls *whether* a tool may run; it does not check
*whether the call did what it claimed*. A separate stage closes that gap: after
a call executes, a `Verifier` judges it against its tool contract and returns a
`Verdict` of `Verified`, `Unverified`, or `Failed`. An effect a contract marks
`Unverifiable`, or one with no checkable postcondition, is `Unverified` — never
silently a success. The verdict is recorded durably (a `ToolVerified` event in
the execution log), and an opt-in gate refuses a final reply that claims an
action completed without a `Verified` call to support it.

This lives in a thin new crate, `localpilot-verify`, depending only on `core`,
`tools`, and `sandbox` — not on the harness — so verification is a stage the
harness composes, not a parallel control loop. A model-critic verifier is a
future drop-in behind the same `Verifier` trait; the deterministic verifier is
the default.

Reason:

- "no success claim without verified evidence" becomes a structural property of
  the loop, not a prompt convention — the gap ADR-0010 left between *controlling*
  an action and *confirming* it
- a deterministic-first stage keeps verification offline, testable, and free of a
  model in the hot path, with the model critic gated behind the same seam
- a narrow crate with a one-way dependency keeps the reliability contract from
  drifting into a second control plane; dropping the crate dependency returns the
  loop to its prior behaviour

## ADR-0022: The Final Alternate-Screen TUI Is Preserved As An Annotated Tag

Status: accepted. Supports ADR-0021.

Before the move to inline rendering, the last full alternate-screen terminal UI
— with mouse capture and the mouse-mode toggle — was frozen as an annotated,
immutable git tag, `legacy-altscreen-tui`, on the pristine pre-change release
commit. It is a keep-for-posterity restore point only and is not maintained
further.

To restore and run it from a clean checkout, the bundled LocalMind submodule
must be initialised first:

```text
git checkout legacy-altscreen-tui
git submodule update --init --recursive
cargo run -p localpilot --features tui -- chat
```

Reason:

- a clearly named, immutable, zero-maintenance restore point lets anyone recover
  the previous interface in one step without keeping dead code on the main line
- recording the exact restore command — including the submodule step, which a
  fresh checkout or worktree otherwise misses — makes the rollback reproducible

## ADR-0021: Inline Terminal Rendering, No Alternate Screen Or Mouse Capture

Status: accepted. Refines ADR-0006; the committed ratatui + crossterm stack is
unchanged and this record fixes how that stack is driven.

The interactive REPL renders inline in the terminal's main screen buffer rather
than taking over an alternate screen. Finished transcript items — user messages,
assistant turns, tool results, system notices — are written once into the
terminal's native scrollback with ratatui's `Terminal::insert_before`, and a
small bottom region (a `Viewport::Inline`) holds the only redrawn surface: the
in-progress activity, the composer, and the status line. The mouse is never
captured.

Consequences:

- Native scrollback, text selection, copy/paste, scrollwheel, and the terminal's
  own search work again, because the app neither switches screen buffers nor
  captures the mouse. The previous mouse-mode toggle is removed.
- History is append-only: a finished block is emitted once and never redrawn;
  only the bottom region repaints each frame.
- The inline region's height tracks the composer. Because the framework has no
  in-place inline-height setter, the terminal is re-initialised when the height
  changes. The `scrolling-regions` capability is enabled so inserting history
  uses the terminal's scroll regions instead of clearing the region each commit.
- Arbitrary full-screen layout — a sticky top bar, or split panes that survive
  scrolling — is given up. For a header-once, stream-output, input-at-bottom
  agent REPL this loses nothing that matters.

Reason:

- the alternate-screen renderer was large and unstable and it disabled the
  terminal features users expect; inline rendering is less code and restores them
- the target API is ratatui's public `Viewport::Inline` / `Terminal::insert_before`;
  behaviour was cross-checked against a local read-only behaviour reference, while
  the implementation, prompts, and tests are original to this repository
  (clean-room, ADR-0005)

## ADR-0020: Skills Are Read-Only Advisory Prompt Modules

Status: accepted. Builds on ADR-0011 (review-gating) and ADR-0013.

A "skill" surfaced to the host is a reviewable advisory prompt module, never an
executable workflow. The host exposes skills through read-only, model-callable
tools only — `skill_drafts` (disabled candidate workflows) and `active_skills`
(human-enabled skills, surfaced as guidance with provenance). Each tool's only
effect is a workspace read; reading a skill never installs, enables, disables, or
runs anything, and active skills are not auto-injected into always-on context.

Reason:

- a wrong or stale skill is then at worst irrelevant guidance the agent ignores,
  never an unintended action;
- enabling/disabling/retiring stay deliberate, review-gated human steps;
- it keeps the local-first, no-surprise posture and is safe to automate later.

The consumption contract is documented in `docs/localmind-integration.md`.

## ADR-0019: The Host Selects The Extractor From Inference Config, Defaulting To A Local Endpoint

Status: accepted. Realizes the learning loop's model path; complements ADR-0018.

Session closeout selects the extractor from the project's `.localmind.toml`: the
model-backed extractor when `[inference].features.extraction` is set, otherwise
the deterministic extractor. The model path falls back to deterministic when the
endpoint is unreachable or returns malformed output. On first use, when the
project's default provider points at a loopback endpoint, the adapter writes an
`[inference]` block targeting that same local endpoint (stripping the `/v1`
suffix LocalMind appends itself), so "local models do the learning jobs" needs no
manual plumbing.

Reason:

- the default learning experience may depend on a local model, with deterministic
  as a graceful, always-available fallback;
- detection is project-scoped, so behaviour does not depend on the host machine;
- a remote provider is never wired automatically — pointing inference at a
  non-loopback endpoint is an explicit, disclosed opt-in (ecosystem remote-egress
  policy). LocalMind stays host-neutral: it only ever sees a generic local
  endpoint (LocalMind decision D-LM-0002).

## ADR-0018: The Learning Write-Path Closes On Every Opted-In Session, Keyed On Structured Signals

Status: accepted. Complements ADR-0011 (the store split and review-gating) and
ADR-0016/0017 (the read path); this record fixes the *write/learn* path.

LocalMind was first-class on the read path but second-class on the learn path:
close-out ran only in the interactive REPL, and it flattened the transcript to
text and re-parsed prose. This record closes the loop.

- **Close-out runs on every deliberate, opted-in session-end path** — the
  interactive REPL, each headless harness step, and the RPC/ACP serve loop —
  through one shared best-effort, non-fatal helper. It skips an empty session, so
  opening and closing one leaves no artifacts. One-shot `localpilot print` is
  excluded, so a bare prompt never creates project files. The headless harness
  builds a fresh runtime per step, so per-step close-out is the natural granularity
  and captures step-level failure/fix/commit.
- **The import is keyed on structured signals, not re-parsed prose.** Close-out
  builds the import from the redacted transcript and then appends compact lines
  from the session **event log** — failed tools, recovery diagnostics, committed
  steps — so the deterministic extractor sees the fact LocalPilot already recorded.
  Only names, statuses, and short commit hashes are appended; never raw payloads.
  The deterministic text path stays the baseline: when the event log has nothing
  notable, the import is the transcript alone, unchanged. LocalMind-core's adapter
  contract is metadata-thin and is **not** changed.
- **In-session surfaces never bypass review-gating.** The `remember` tool lets the
  agent propose a durable lesson — it enqueues a review candidate (permission-gated,
  project-local write) and never writes accepted memory. The read-only
  `skill_drafts` tool lists or inspects generated drafts without enabling them; the
  disabled flag stays authoritative and activation stays a human step. Each tool
  has a tool-gated system-prompt cue that appears only when the tool is registered.

Consequences:

- Autonomous runs learn, not just the REPL: a headless or RPC session produces
  reviewable candidates enriched with execution outcomes.
- Close-out cannot regress the autonomous critical path: it is best-effort,
  non-blocking, off the turn path, and gated on the existing opt-in.
- Review-gating (ADR-0011) holds end to end: nothing on the write path writes
  accepted memory or activates a skill automatically; everything new produces a
  review candidate or a read-only suggestion.
- The change is a contained host-edge change (call sites plus the adapter import
  text); LocalMind-core stays host-neutral and unchanged.

Reason:

- The structured truth LocalPilot already records in the event log is the
  high-value, low-risk lever: enriching the import text the extractor consumes
  needs no engine change, while flattening to prose threw that truth away.
- Per-step close-out matches how the harness already builds sessions, so it adds
  no new lifecycle.
- All prompt, tool, and behavior text remains original to this repository
  (clean-room, ADR-0005 / docs/00-clean-room.md).

## ADR-0017: Retrieval Context Is A Request-Time Projection

Status: accepted. Refines ADR-0016 and ADR-0014 (pull-over-push and
runtime-only projection still hold; this record fixes *how* the per-turn seed
reaches the model).

Per-turn context-hook output — lean accepted project memory, plus ingest chunks
only under `[ingest] mode = "push"` — is computed once per turn and injected into
the outgoing `ModelRequest` adjacent to the leading system prompt. It is **never**
appended to `self.messages`, the durable transcript, or the event log. Its token
estimate is reserved from the compaction budget so the request still fits the
limit. The ingest knowledge base is reached on demand through the read-only
`knowledge_search` tool, which returns a ranked cross-source pack (ingest,
accepted memory, recent-session facts, code graph) via a compute-only path that
performs no write. On an interactive REPL the index is built in the background on
first use (trust-gated, off the turn path).

Consequences:

- `self.messages` equals the authored history equals the stored transcript again:
  the synthetic-message persistence invariant ("a resumed session reconstructs
  exactly the history the model received") holds without a retrieval exception.
- Re-derived retrieval cannot accumulate across turns, and folding it into the
  leading system run means it rides the wire as top-level `system`, not as a
  resent user message (on Anthropic, a non-leading system message maps to user).
- Because the injected block is no longer part of the compacted history, the
  compaction budget explicitly reserves its token estimate; reported context
  usage is the real request total.
- The evict-on-replace seed path (and its synthetic marker) are deleted — less
  code, fewer states.
- A present-but-unreadable ingest index is reported distinctly from a missing
  one, so corruption is visible rather than masked as "no knowledge"; a turn
  never breaks on a knowledge miss.

Reason:

- The interim ephemeral-but-in-`messages` seed (ADR-0016) softened the
  synthetic-persistence invariant and required eviction bookkeeping and a
  compaction cache that already counted it. Treating retrieval as a request-time
  projection — what it always was conceptually — is the correct model and removes
  that machinery.
- Keeping the pull tool read-only (compute-only pack) means a model can pull
  ranked project knowledge with no write or heavy side effect.
- All behavior, tool, and prompt text remain original to this repository
  (clean-room, ADR-0005 / docs/00-clean-room.md).

## ADR-0016: Project Knowledge Is Pulled On Demand, Not Pushed Every Turn

Status: accepted. Refines ADR-0014 and ADR-0015 (the runtime-only projection and
the ranked budget still hold; this record changes how the *ingest* source is
delivered).

Ingested folder knowledge is reached on demand through a read-only
`knowledge_search` tool, not auto-seeded into every turn. The only always-on
retrieval seed is accepted, review-gated memory, and it is contributed leanly:
bounded in size, re-derived each turn, and **replaced** rather than accumulated.
A new `[ingest] mode` selects behavior — `pull` (default) or `push` (legacy
auto-injection of ingest chunks), the latter kept only as an escape hatch.

The per-turn retrieval block is marked synthetic and is **not** written to the
durable transcript or event log: it is re-computable context, not authored
history.

Consequences:

- Retrieval no longer grows the context window with turn count. Previously the
  pre-turn context hook appended a fresh system message every turn, so
  re-derived retrieval accumulated; on the Anthropic wire a non-leading system
  message also maps to a resent user message, compounding the growth.
- The event-log → transcript projection and session close-out lesson extraction
  stay clean, because the ephemeral retrieval seed never enters them.
- Ingested knowledge is still ranked and budgeted when pulled (the tool wraps the
  deterministic read-only ingest search; the ADR-0015 allocator remains available
  via the pack path).
- A fresh project's knowledge base is empty until `localpilot ingest` runs; the
  tool returns a useful "not indexed yet" result rather than an error. The
  first-use auto-ingest was removed from the turn path so a heavy walk/chunk no
  longer stalls the first turn.
- `push` mode restores the prior always-on ingest injection without a rebuild.

Reason:

- Auto-seeding the lowest-trust, highest-volume source (ingest) on every turn was
  the dominant cause of context filling quickly, and it duplicated content the
  model rarely needed standing by.
- Pull keeps high-trust accepted memory passively available and lean while making
  bulk project knowledge reachable on demand at no standing context cost — the
  retrieval analogue of reading a file only when relevant.
- The behavior, tool, config, and prompt cue are original to this repository
  (clean-room, ADR-0005 / docs/00-clean-room.md).

## ADR-0015: Derived Context Sources Compete Under One Ranked Budget

Status: accepted

Derived knowledge that can be injected into a turn — accepted memory anchors,
recent session facts, ingest hits, code-graph neighbors, and explicit manual
pins — competes for one token budget instead of each source getting a fixed
slice. Selection is a deterministic two-phase allocation: a per-source reserve
phase (filled highest-precedence source first) guarantees a high-value entry
survives a flood from a noisier source, then a shared pool fills the remainder by
a composite rank.

The rank is composed from explicit, inspectable signals: raw relevance, a
source-quality weight (manual pin > accepted memory > recent session > ingest >
code graph), recency, a stale penalty, and a redundancy penalty that demotes the
second and later hits from the same file. Every candidate is recorded as either
selected or skipped with its reason and full signal breakdown.

Consequences:

- A context pack is auditable end to end: a reader can see why each entry was
  included and why a high-ranking near-miss was dropped.
- Runtime conversation context (the kept raw suffix and the compaction digest)
  is owned by the compaction layer (ADR-0014); the ranked budget governs the
  derived-knowledge layer. The two compose by precedence — system context and
  the current turn are hard, then the recent suffix and digest, then the ranked
  derived sources.
- Manual pins and accepted memory are protected by reserves so a lexical ingest
  flood cannot crowd out review-gated or user-chosen context.

Reason:

- Fixed per-source slices either waste budget or starve a strong signal; one
  ranked competition spends the budget where it is most useful while keeping
  trusted sources protected.
- OpenCode and Pi informed the layered-precedence and budget concepts; the
  ranking, signal set, reserve math, and data shapes are original to this
  repository (clean-room, ADR-0005 / docs/00-clean-room.md). No reference code
  or prompt text was copied.

## ADR-0014: Context Projection Is Runtime-Only And Audit-First

Status: accepted

Runtime compaction, derived ingest packs, accepted memory retrieval, and code
graph facts all contribute to the active model request, but they keep distinct
ownership and lifetime boundaries. Compaction rewrites only the active runtime
projection; it may persist source-grounded summary and attempt metadata in the
session event log, but it does not write accepted memory, skill drafts, review
items, or ingestion artifacts.

Consequences:

- Compaction cutover is completed-only: a candidate projection must pass
  pairing, budget, and digest validation before it becomes active.
- The deterministic compactor is the correctness baseline. Smart modes must
  report fallback reasons and leave a valid deterministic projection.
- Compaction audit events store mode, fallback reason, counts, estimates, and
  truncation metadata without raw dropped transcript dumps.
- Ingestion remains rebuildable `.localmind/ingest/` state, and accepted memory
  remains LocalMind review-gated state.

Reason:

- Treating runtime context as memory would silently teach LocalMind unreviewed
  facts and weaken ADR-0011.
- Provider output-limit and partial tool-call failures require atomic request
  projection, not in-place mutation of transcript history.
- Shared source hints and budget metadata make context decisions inspectable
  without leaking private plan state or raw oversized content.

## ADR-0013: Folder Ingestion Uses Disposable Project-Local Artifacts

Status: accepted

Project folder ingestion writes derived state under `.localmind/ingest/`:
manifests, redacted chunks, job state, skipped-file reports, review candidates,
and context packs. These artifacts are rebuildable from the trusted project
folder and may be deleted without touching accepted memory.

Accepted memory remains owned by LocalMind's reviewed memory path. Ingestion may
enqueue review candidates through LocalMind, but it must not write accepted
memory directly.

Consequences:

- `.localmind/ingest/` is disposable derived state. Rebuild and forget commands
  remove only ingestion artifacts.
- Persisted ingestion content is redacted by the LocalPilot redaction stack
  before it is written.
- The first implementation keeps deterministic JSON artifacts and Rust-side
  ranking. SQLite-backed search can be added later if the derived corpus needs
  FTS behavior, but that would remain rebuildable ingestion state.
- Context packs are persisted as the latest derived pack for inspection and
  staleness handling; they are not durable memory.

Reason:

- ADR-0011 already reserves `.localpilot/` for execution records and LocalMind
  for memory/learning. Folder ingestion is broad mechanical project knowledge,
  so it belongs beside LocalMind state but outside accepted memory.
- Keeping the v1 artifacts rebuildable avoids migration risk while the schema is
  still young.
- Review-queue promotion preserves the curated-memory boundary and gives users
  an explicit approval point before broad file observations become durable
  knowledge.

## ADR-0012: Project `.localpilot.toml` Is Local-Only, Never Committed

Status: accepted. Amends the "committed `.localpilot.toml`" wording in
ADR-0009.

The project-local `.localpilot.toml` is a machine-local file: it is listed in
`.gitignore` and is not committed. External launchers generate provider
config into it in the project directory (base URL, model, key env-var name),
and those values are inherently machine-local. The ratified quality gate
(`[[harness.checks]]`, ADR-0009) lives in the same file and is therefore also
local-only.

Consequences:

- The ratification trust boundary is the explicit user action that writes
  checks into the local file — not version control. Wording in
  [`docs/06`](06-harness-spec.md) and [`docs/07`](07-security-and-privacy.md)
  says "ratified into the project's local `.localpilot.toml`" rather than
  "committed".
- A fresh clone has no ratified gate; `gate propose` / `gate ratify` is the
  supported way to re-establish one. A team that wants a shared, reviewed
  gate definition can keep one in its own committed docs and ratify from it,
  but the harness never reads checks from a committed file.

Reason:

- committing the file would leak machine-local endpoints and invite config
  drift between what a launcher generates and what the repo pins
- one file with one clear lifecycle (generated/edited locally, ignored) beats
  splitting harness config across a committed and an ignored file
- ratification was always defined as the user's explicit act; tying trust to
  VCS state added nothing and contradicted the launcher workflow

## ADR-0011: Store Convergence — Execution Record vs Memory

Status: accepted

LocalPilot persists state in two stacks, which were growing toward overlap.
This record fixes the ownership boundary:

- **The LocalPilot store (`.localpilot/`) is the execution record, and only
  that**: transcripts, the durable session event log (tree-shaped, format-
  versioned), caches, tool-output snapshots, provider metadata, and recovery
  diagnostics. It never grows memory, lesson, retrieval, or review features.
- **LocalMind (`.localmind/`) is the only memory and learning backend**:
  session closeout, candidate lessons, the review queue, accepted memory,
  retrieval/context injection, skill drafts, and audit. New rich-learning
  behavior lands in LocalMind, never as a host-local memory implementation.
- **One redaction authority at the host boundary.** LocalPilot's redaction
  stack (`localpilot-config::redact`) is the canonical redactor: everything
  the host persists or hands to LocalMind is redacted by it first. LocalMind's
  import-time redaction remains as engine-internal defense in depth, not a
  second authority — divergence between the two pattern sets is resolved by
  updating the host stack.

Reason:

- two stores with drifting responsibilities and two redaction pattern sets is
  how secrets leak and how features get implemented twice
- the event log needs a single unambiguous home (the execution record) before
  later features (headless drive, hooks, subagents) build on it
- LocalMind is host-neutral and reusable; baking memory into the LocalPilot
  store would fork that capability

## ADR-0010: Reliability Contract for Unattended Operation

Status: accepted

LocalPilot's differentiator is unattended multi-step execution. That claim is
made testable by an explicit **reliability contract**: a small set of named
invariants the runtime guarantees on every exit path, each pinned by a named
test, split across the owning specs:

- Session-loop invariants (tool-result pairing on every exit path, no partial
  replies persisted, transcript fidelity) —
  [`docs/06`](06-harness-spec.md) §Reliability Contract.
- Permission invariants (no `run_shell` path weaker than the equivalent
  builtin, floor-aware allowlists that never lift destructive/privileged/
  unknown gating, wrapper commands never auto-allowed, approval prompts that
  state their target) — [`docs/07`](07-security-and-privacy.md) §Reliability
  Contract.

A change that breaks a contract-pinning test is a contract change: it requires
a superseding ADR, not a test edit. The bypass profile's scope is part of the
contract: bypass keeps the workspace boundary for path-bearing effects only;
shell commands are not path-contained, and the docs state this rather than
implying containment that does not exist.

Reason:

- the product's central claim ("every side effect passes a typed permission
  engine"; "safe to run unsupervised") was previously aspiration enforced
  only by convention — line-level review found exit paths and classification
  gaps that falsified it
- invariants stated in the spec and enforced by property tests survive
  refactors; workflow descriptions do not
- naming the tests in the spec makes the contract auditable: a reader can run
  the contract

## ADR-0009: Discovered Project Quality Gate

Status: accepted

The harness's single `test_command` is generalized into a quality gate: a set of
language-specific inspection checks — format, lint, test, dependency hygiene,
advisory audit, static analysis — drawn from the project's own toolchain rather
than hardcoded into the engine. Built-in toolchain profiles per stack declare
the default checks, how to interpret a check's findings, and which findings are
safely auto-fixable; a discovery step detects the stack, probes which tools are
actually available, and proposes a gate the user ratifies into committed
`.localpilot.toml`. The rule engine runs checks at a per-check cadence (fast
checks each step, full checks at phase boundaries) and acts on findings: safe
deterministic fixers are applied and re-run, remaining failures feed the
anti-sunk-cost loop (retry, bounded, then replan recorded in `DECISIONS.md`), and
dependency/audit findings block for a human decision. Discovered commands are
untrusted — discovery proposes, the user ratifies, and every check runs through
the same permission engine and sandbox as any other shell command.

Reason:

- replaces a single test hook with real per-language cleanup and inspection
  without baking tool lists into the engine
- keeps the engine stack-neutral: the abstraction is built in, the instances are
  discovered (the spirit of ADR-0002)
- makes findings actionable inside the loop instead of advisory, with bounded
  auto-fix and replan rather than runaway churn
- preserves the security model: discovered commands are ratified once and always
  mediated by the permission engine ([`docs/07`](07-security-and-privacy.md)),
  never auto-trusted
- per-check cadence keeps fast per-step feedback without paying full-suite cost
  on every step

## ADR-0008: Anthropic Messages API as the Second Provider

Status: accepted

A second, protocol-distinct provider adapter is added alongside the
OpenAI-compatible one: the Anthropic Messages API. It is implemented clean-room
from the public API reference, talks only to the documented official endpoint,
and exercises the provider trait's generality (top-level `system`,
`tool_use`/`tool_result` content blocks, a required `max_tokens`, and a typed
SSE stream).

Reason:

- satisfies the Stable requirement of at least two provider implementations
  ([`docs/09`](09-release-plan.md))
- proves the provider abstraction is not OpenAI-shaped by construction
- adds a major hosted model family without coupling the core to it (ADR-0002)

## ADR-0007: Windows, Linux, and macOS Are All Tier-1

Status: accepted

LocalPilot targets Windows, Linux, and macOS as equal first-class platforms. No
platform is a second-class port. Behavior parity is a release requirement, CI
builds and tests on all three, and installers ship for all three.

Reason:

- the target users run on all three platforms
- shell/filesystem security policy must be correct per-platform, not POSIX-only
- treating one OS as primary causes silent breakage on the others
- forces explicit Windows and POSIX command/path handling from the start

## ADR-0006: Ratatui as the TUI Framework

Status: accepted

The terminal UI is built on `ratatui` with the `crossterm` backend and
`tui-textarea` for input. This is a committed choice, not a recommendation.

Reason:

- `ratatui` is actively maintained and the de facto Rust TUI framework
- `crossterm` provides one terminal backend across Windows, Linux, and macOS,
  supporting the tier-1 platform commitment (ADR-0007)
- a single committed stack keeps rendering, layout, and snapshot tests uniform
- alternatives are out of scope unless a future ADR supersedes this one

## ADR-0005: Read-Only Local Behavior Reference

Status: accepted

A local working implementation may be inspected as a read-only behavior
reference while planning and implementing this Rust project.

The reference may be used to clarify expected workflows, command behavior,
configuration shape, user-facing edge cases, and high-level product
requirements. It must not be used as source material for copied, translated, or
mechanically ported code, prompts, tests, private endpoint behavior,
implementation structure, identifiers, UI copy, branding, or other prohibited
material.

Reason:

- preserves momentum while the Rust specs are still incomplete
- gives implementers a working behavior baseline for ambiguous flows
- keeps this repository independently authored and clean-room auditable
- makes provenance expectations explicit in planning and review

## ADR-0004: No Private Endpoint Adapters

Status: accepted

LocalPilot will not implement adapters for private, undocumented, or
consumer-product endpoints. Provider integrations must use official APIs, local
servers, or explicit user-owned custom endpoints.

Reason:

- reduces legal and account risk
- keeps provider contracts stable
- avoids brittle reverse-engineered behavior
- preserves trust in the project

## ADR-0003: Project Files Are Harness Source of Truth

Status: accepted

The harness treats `brief.md` and `PROGRESS.md` as authoritative. Transcripts
are helpful context but not authoritative state.

Reason:

- users can inspect and edit plans
- sessions can resume after crashes
- implementation remains auditable

## ADR-0002: Provider-Neutral Core

Status: accepted

The core crate must not depend on provider-specific APIs or payload shapes.

Reason:

- avoids coupling the product to one vendor
- makes local models first-class
- keeps tests independent of network access

## ADR-0001: Rust Workspace with Narrow Crates

Status: accepted

LocalPilot is split into narrow crates rather than one large binary crate.

Reason:

- clearer boundaries
- easier clean-room review
- smaller test surfaces
- easier future embedding
