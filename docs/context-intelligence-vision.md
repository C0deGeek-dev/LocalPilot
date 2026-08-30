# Context Intelligence Vision

## Purpose

LocalPilot should treat context as a managed resource, not as an append-only
chat transcript. Long sessions, tool-heavy work, project ingest, accepted
memory, code graph facts, and model summaries all compete for the same provider
context window. The system should preserve what matters, remove repetition,
bound large artifacts, explain what it selected, and never turn transient
runtime context into durable memory without review.

This document describes the target behavior for smart compaction, smarter
ingest, LocalMind data quality, and retrieval. It is intentionally
implementation-checkable: after the feature ships, every section should map to
code, tests, docs, or an explicit non-goal.

> **Scope and authority.** This doc owns LocalPilot's *host-side* context
> behaviour (compaction, ingest, retrieval into the prompt). For the memory and
> learning engine itself — session capture, lesson extraction, the review queue,
> accepted-memory promotion, skills, and the graph layer — the **LocalMind
> `vision.md`** is authoritative. Where the two overlap, defer to LocalMind
> `vision.md` for engine semantics and keep this doc to how LocalPilot consumes
> them.

## Core Model

Context has layers with different trust and lifetimes:

| Layer | Owner | Lifetime | Notes |
| --- | --- | --- | --- |
| System and policy context | LocalPilot | active request | Highest priority; not summarized away. |
| Current user turn | LocalPilot | active request | Preserved raw. |
| Recent session suffix | LocalPilot | active request | Preserved raw within budget. |
| Compacted runtime digest | LocalPilot | active session | Derived from older transcript; fallback is deterministic. |
| Tool-output snapshots | LocalPilot | execution record | Redacted before persistence; model sees bounded previews. |
| Ingest chunks and packs | LocalMind storage via LocalPilot host | rebuildable derived state | Local project knowledge, not accepted memory. Pulled on demand via `knowledge_search` by default, not seeded every turn (ADR-0016). |
| Accepted memory | LocalMind | durable reviewed memory | Review-gated and auditable. |
| Code graph facts | LocalMind | derived indexed state | Host feeds sources; graph links code and accepted memory. |

The active model request is a projection from these layers. The projection is
budgeted, inspectable, and rebuildable from source records.

## Non-Negotiable Invariants

- Compaction is atomic: failed, canceled, empty, malformed, output-limited, or
  over-budget smart compaction must not mutate active projected history.
- Deterministic compaction remains the correctness baseline.
- Tool-call/tool-result pairing is preserved across every compaction and
  retrieval path.
- Provider-native reasoning, encrypted/signature-like artifacts, partial
  tool-argument fragments, and cache-bound provider IDs are not preserved across
  a rewritten prefix unless the provider contract explicitly supports it.
- No raw media, base64 payloads, or full giant tool outputs are embedded in
  summarizer prompts, compacted summaries, or context packs.
- LocalPilot redacts before persistence/export at the host boundary.
- LocalMind accepted memory is written only through review/promotion.
- Ingest state under `.localmind/ingest/` is derived and disposable.
- Every model-backed summary or extraction output is parsed, validated,
  budgeted, and source-grounded before use.
- A provider context-overflow response before durable assistant/tool side
  effects may trigger one safe shrink-and-retry. A repeated overflow is
  terminal.

## Smart Runtime Compaction

Smart compaction should produce a structured digest, not a vague prose summary.
The digest should include:

- current goal;
- constraints and user preferences;
- completed progress;
- in-progress or blocked work;
- key decisions and stale/superseded decisions;
- next steps;
- critical context;
- relevant files and file operations;
- commands run and outcomes;
- repeated failures collapsed into a single useful note;
- source hints for claims.

The compactor should first normalize older history into provider-safe groups,
then produce deterministic digest data, then optionally ask a model to improve
that digest. The model call must have no tools, bounded input, bounded output,
timeout/cancellation support, strict schema validation, and deterministic
fallback.

Recent raw messages remain more valuable than rewritten history. The system
should keep a recent suffix within budget and summarize older content.

## Smarter Ingest

Ingest should evolve from "redacted chunks plus lexical search" into derived
context packs that are useful inside a model request.

Target behavior:

- classify, redact, chunk, hash, and version source files;
- collapse repeated boilerplate and generated content;
- track stale/superseded chunks by content hash and source path;
- summarize chunks or clusters into source-grounded digest records;
- preserve line ranges, source hashes, token estimates, and redaction status;
- explain why a pack entry was included, skipped, truncated, or marked stale;
- support rebuild/forget without touching accepted memory;
- keep rich extraction for PDFs, DOCX, XLSX, images/OCR, archives, notebooks,
  and external research behind explicit config and review.

Context packs should combine task query relevance, accepted memory anchors,
code graph neighbors, recent session facts, and ingest hits under one budget.

## LocalMind Data Quality

LocalMind should use the same context-quality rules for session closeout,
candidate extraction, accepted-memory updates, and context export.

Target behavior:

- deterministic extraction remains available without inference;
- model-backed extraction returns strict structured data only;
- summaries and candidates cite evidence: transcript ranges, tool events, file
  diffs, command output, ingest chunks, or code graph facts;
- candidate ids are stable enough for dedupe;
- candidates can be marked valid, low-confidence, duplicate, conflicting,
  malformed, or missing evidence;
- accepted-memory updates are review items: merge, supersede, split, ignore, or
  promote;
- repeated commands, user corrections, failure/resolution pairs, stale
  decisions, and conflicting prior memory are tested as first-class cases.

LocalMind remains host-neutral. LocalPilot-specific runtime details belong in
adapter metadata, not LocalMind core dependencies.

## Retrieval And Budgeting

The final request builder should select context through explicit budgets:

- provider context window;
- reserved output buffer;
- keep-recent raw suffix floor;
- per-source caps for digest, accepted memory, ingest, code graph, and tool
  previews;
- hard priority for current turn and system context.

Ranking should consider relevance, recency, source quality, accepted-memory
priority, exact file match, graph proximity, confidence, stale penalty, and
redundancy penalty.

The user should be able to inspect:

- selected context entries;
- skipped near-misses;
- token estimates;
- truncation reasons;
- fallback reasons;
- source ids and hashes;
- stale or superseded markers.

## Reference Implementations

OpenCode and Pi are useful implementation references. In the local copies,
their root licenses are MIT. Their strongest reusable ideas are:

- context epochs and safe provider-turn boundaries;
- completed-only compaction cutover;
- overflow-triggered retry before durable side effects;
- structured summary templates;
- media placeholders and bounded tool previews;
- split-turn compaction;
- valid cut-point selection;
- repeated-summary update semantics;
- file-operation tracking;
- extension cancellation/fallback behavior.

Unshackled has useful behavioral ideas: microcompacting large tool results,
API-round grouping, prompt-too-long retries, compact boundaries, stripping
media/static context, and post-compact cleanup. The local copy does not appear
to have a clean license grant, so it should remain behavior-only unless
provenance is resolved.

## Expected User Experience

The user should notice fewer context-limit failures, less repetition, and
better continuity after long sessions. A compacted session should retain the
actual task state: what was decided, what failed, what changed, which files
matter, and what should happen next.

When the system uses smarter context, it should say enough to be debuggable:
mode used, whether fallback happened, final token estimate, dropped/kept
counts, and where to inspect the context pack or compaction metadata.

## Implementation Checklist

- Output-limit streaming cannot produce a secondary partial tool-argument parse
  failure.
- Compaction cutover is completed-only and tested.
- Deterministic digest has the structured sections listed above.
- Smart summarizer has strict schema validation and deterministic fallback.
- Split-turn compaction is tested.
- Media and large tool outputs become bounded previews/placeholders.
- Ingest packs carry provenance, hashes, token estimates, stale state, and
  inclusion reasons.
- LocalMind candidates carry evidence and validation status.
- Accepted memory cannot be written from compaction or ingest without review.
- Retrieval has source budgets and inspectable selection metadata.
- Repeated failures and repeated commands collapse into useful aggregate notes.
- Stale decisions are removed or marked stale on repeated compaction.
- Provider overflow triggers at most one safe retry before terminal failure.
- Tests cover Windows path behavior where ingest or provenance uses paths.
- Public docs explain the data lifecycle and privacy boundaries.

## Implementation Status (2026-06)

Shipped and tested:

- Output-limit streaming hardening and completed-only compaction cutover.
- Deterministic structured digest, plus an optional model-backed summarizer with
  strict schema validation, self-checks, output-cap clamping, and deterministic
  fallback on every failure mode.
- Split-turn compaction, media/base64 stripping to placeholders, and bounded
  tool-output previews.
- Provider-overflow safe retry (one shrink-and-retry, second overflow terminal).
- Ingest packs with provenance, hashes, token estimates, stale state, and
  inclusion/skip reasons.
- A ranked cross-source budget allocator (accepted memory, recent session,
  ingest, code graph, manual pins) with relevance, source-quality, recency,
  exact-file-match, confidence, graph-proximity, stale-penalty, and
  redundancy-penalty signals, each recorded per entry for inspection (ADR-0015).
- Pack building gathers candidates from ingest hits, accepted-memory anchors,
  code-graph neighbors of task-relevant symbols, and the most recent session
  summary's key points, all competing under one budget.
- The `knowledge pack` command surfaces the active selection: per-source
  reserves, included entries with rank score and reason, and skipped
  near-misses with reasons.
- Project knowledge is pulled on demand: a read-only `knowledge_search` tool
  returns a ranked cross-source pack (ingest, accepted memory, recent-session
  facts, code graph) via a compute-only path that performs no write, so ingested
  chunks are no longer seeded into every turn. `[ingest] mode` (default `pull`,
  legacy `push`) selects the behavior (ADR-0016).
- The per-turn accepted-memory block is a request-time projection: computed once
  per turn and injected into the outgoing request adjacent to the leading system
  prompt, never written to history/transcript/event-log, with its token cost
  reserved from the compaction budget. So `messages == transcript`, retrieval
  cannot accumulate, and it rides the wire as top-level `system` (ADR-0017). The
  interactive REPL builds the ingest index in the background on first use
  (trust-gated, off the turn path); a corrupt index is reported distinctly.
- LocalMind candidates carry evidence and validation status, and memory-update
  suggestions (merge, supersede, split, ignore, promote) are review items, never
  direct writes.
- The learning write-path is closed: close-out runs on every opted-in
  session-end path (REPL, each headless harness step, the RPC serve loop; one-shot
  `print` excluded), and the import is enriched with redacted structured signals
  from the event log so extraction keys on fact, with the deterministic text path
  intact as the baseline. The agent can enqueue a review candidate in-session
  (`remember`) and surface generated skill drafts read-only (`skill_drafts`).
  Nothing on this path writes accepted memory or activates a skill — review-gating
  holds (ADR-0018).

Deliberate gaps (intentionally deferred):

- The recent-raw-suffix and compaction-digest layers are owned by the
  compaction runtime and compose with the ranked allocator by precedence; they
  are not merged into a single allocator call.

