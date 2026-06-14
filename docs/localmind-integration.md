# LocalMind Integration Contract

## Why

Learning (candidate lessons, review queues, memory promotion, retrieval, skill
generation and maintenance, audit, self-improvement) is a coherent capability
that should not be re-implemented inside every coding agent. LocalMind owns that
core as a standalone engine usable by native hosts and generic transcript
workflows.

LocalPilot is LocalMind's first native host. The LocalMind crates are bundled
into the LocalPilot binary through `localpilot-localmind`; users do not install
LocalMind separately.

## Ownership Boundary

The store split is fixed by ADR-0011: the LocalPilot store (`.localpilot/`) is
the execution record only (transcripts, the session event log, caches,
diagnostics); LocalMind (`.localmind/`) is the only memory/learning backend.
LocalPilot's redaction stack is the canonical redactor at the host boundary;
LocalMind's import redaction is defense in depth, not a second authority.

- **LocalMind core is host-neutral and must not depend on LocalPilot.** It owns
  session closeout, redaction-on-import, summarization, candidate-lesson
  extraction, the review queue, accepted-lesson persistence, Markdown-backed
  memory with a SQLite audit/search index, agent-ready context export, and
  `SKILL.md` draft emission.
- **LocalPilot owns the native host role.** It captures session evidence,
  enforces permissions and redaction before persistence, drives TUI/CLI
  surfaces, and adapts LocalPilot session records into LocalMind contracts.

## Bundling

LocalMind is vendored as a git submodule at `external/localmind` and excluded
from the LocalPilot workspace because it is its own workspace. The
`localpilot-localmind` adapter depends on `localmind-core` and `localmind-store`
by path.

```sh
git clone --recurse-submodules <repo>
git submodule update --init --recursive
```

CI checks out submodules recursively. The adapter is a one-way edge: LocalPilot
depends on LocalMind, never the reverse.

## Current Surfaces

- `localpilot-localmind::closeout_session` imports an LocalPilot transcript into
  LocalMind, extracts candidate lessons, and enqueues them for review.
- `localpilot learning` exposes the rich LocalMind loop: `closeout`, `review`,
  `promote`, `search`, `skills`, and `audit`.
- `localpilot memory` uses LocalMind accepted memory for status, inspect, search,
  delete, and context-injection disable.
- Agent turns contribute relevant accepted LocalMind memory as best-effort
  context. The block is lean (bounded) and re-derived each turn, and is injected
  into the outgoing request adjacent to the leading system prompt at build
  time — it is **not** appended to the message history, the transcript, or the
  event log, so it cannot accumulate and the stored transcript stays equal to the
  authored history (ADR-0017). Its token cost is reserved from the compaction
  budget so the request still fits the limit.
- Interactive sessions build the project ingest index in the background on first
  use (trust-gated, off the turn path), so `knowledge_search` has data without
  the first turn paying for a full walk; they close out into LocalMind on exit,
  then run one bounded, incremental code-graph reindex pass.
- `localpilot memory graph <symbol>` inspects a symbol's graph neighborhood,
  tests, and anchored lessons; `localpilot memory export <path> [--html]`
  writes a redacted, local-only snapshot of the graph (host redaction stack
  applied before write; no network).
- Promoting an accepted review item anchors the new memory to the code nodes
  its hints resolve to, so graph retrieval can surface it by structure.
- Folder ingestion writes rebuildable derived knowledge under
  `.localmind/ingest/`: manifests, redacted chunks, job state, review
  candidates, and task context packs. By default (`[ingest] mode = "pull"`)
  ingested knowledge is reached on demand through the read-only
  `knowledge_search` tool rather than seeded into every turn; the legacy
  `mode = "push"` restores always-on injection of high-ranking chunks. Ingested
  context is never accepted memory. Promotion from ingestion enqueues LocalMind
  review items first (ADR-0016).
- Context compaction manages the active model projection only. It can emit a
  structured, source-grounded runtime digest and safe audit metadata, but it
  does not write accepted memory, create skill drafts, or enqueue LocalMind
  review items.

State is project-local under `.localmind/`. Durable memory is readable Markdown;
queue, audit, search index, and the code-structure graph live in SQLite.

## Code Graph

LocalMind owns a code-structure knowledge graph (schema, tree-sitter ingestion,
persistence, traversal, ranked retrieval) populated from files the host feeds
it through the capture boundary; the engine never walks the filesystem itself.
The graph honours `.localmind.toml` `excluded_paths`, is offline and
deterministic (no model, no network in the pipeline), and joins code nodes to
accepted memory through anchor edges so retrieval traverses code and lessons
together. Reindexing is incremental and content-hash gated; removed sources
are superseded rather than deleted, so provenance and anchored knowledge
survive. The engine also exposes transport-agnostic MCP tool contracts
(`localmind-mcp`) for structural queries a host MCP server can mount.

## Signal Mapping

| LocalPilot signal | LocalMind use |
| --- | --- |
| Session transcript bundle | imported, redacted session for summarization |
| Tool events in transcript | evidence for lesson extraction |
| Code diffs and commits | future durable outcome anchors |
| Test output and quality gate results | future pass/fail signal attached to lessons |
| Recovery events | future frequent-failure candidate lessons |
| Accepted memory | LocalMind retrieval and context injection |
| Skill drafts | LocalMind disabled `SKILL.md` draft emission |

All capture stays redacted-before-persistence and inside the permission boundary;
LocalMind never bypasses either.

## Derived Context Metadata

Compaction digests, ingestion chunks, and task context packs use the same
vocabulary: goal, constraints, progress, decisions, next steps, critical
context, relevant files, command/failure outcomes, unresolved risks, and
stale/superseded facts. Derived records carry source hints, redaction status,
content hashes where available, token estimates, and inclusion or skip reasons.

The session event log stores compaction attempt metadata with mode, fallback
reason, dropped/kept counts, digest estimate, and truncation count. It does not
store raw dropped transcript content. Ingest refresh keeps superseded chunks
marked stale so retrieval can prefer newer evidence while audit remains
explainable.

## Commands

```sh
localpilot learning closeout --session <id>
localpilot learning review list
localpilot learning review accept <item-id>
localpilot learning promote <item-id>
localpilot learning search "<query>"
localpilot memory inspect
localpilot memory delete <memory-id>
localpilot memory graph <symbol>
localpilot memory export graph.json
```

The agent also has a read-only `knowledge_search` tool (registered on every
session path) that pulls a ranked cross-source pack on demand — ingested files,
accepted memory, recent-session facts, and code structure under one budget — via
a compute-only path that performs no write, so ingested project knowledge is
pulled when relevant instead of riding in every turn's context. A missing index
returns "not indexed yet"; a present-but-unreadable one is reported distinctly
("rebuild") rather than masked as empty.

New rich-learning behavior lands in LocalMind, not by expanding host-local memory
implementations.

## Folder Ingestion Roadmap Boundaries

The first ingestion implementation handles safe UTF-8 text-like files,
deterministic manifests, redacted chunks, lexical search, review candidates, and
task packs. These are local derived artifacts and can be rebuilt.

The following remain staged behind explicit user approval and review:

- rich extractors for PDF, DOCX, XLSX, images/OCR, archives, notebooks, and
  language-aware graph expansion beyond the current code graph;
- model-backed file, folder, and project summaries beyond deterministic review
  shells;
- external research/update flows. External facts must carry source citations,
  expiry/staleness metadata, and a review item before they can influence
  accepted memory.
