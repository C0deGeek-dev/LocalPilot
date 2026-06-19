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
- Close-out runs on **every** deliberate, opted-in session-end path — the
  interactive REPL, each headless harness step, and the RPC/ACP serve loop — so
  autonomous runs learn too, not just the REPL. It is best-effort and non-fatal,
  and skips an empty session so opening and closing one leaves no artifacts.
  One-shot `localpilot print` deliberately does **not** close out, so a bare
  prompt never creates project files (ADR-0018).

### Which surfaces learn

"Learning" means closing out a session into LocalMind (extract candidate lessons,
enqueue for review). Retrieval / context injection is read-only and happens on
any surface that has accepted memory; only the surfaces below *write* learning
candidates. The `print`-mode code comment in `context_inject.rs` is the source of
truth this table mirrors.

| Surface | Closes out to LocalMind? |
|---|---|
| Interactive `chat` / REPL | Yes — on session end |
| Headless harness (`harness run`) | Yes — each step |
| RPC / ACP serve loop | Yes — on session end |
| `localpilot learning closeout` | Yes — explicit, on demand |
| One-shot `localpilot print` | No — read-only, never closes out (ADR-0018) |
| Bare `ask` / other one-shot prompts | No — unless an explicit `learning closeout` is run |

Close-out is best-effort and non-fatal, and an empty session writes nothing.

- The agent can propose a durable lesson in-session with the `remember` tool: it
  enqueues a review candidate (permission-gated, project-local write) and never
  writes accepted memory directly — promotion stays a human, review-gated step
  (ADR-0011).
- The agent can list or inspect generated skill drafts read-only with the
  `skill_drafts` tool. Surfacing a draft never enables it; the disabled flag stays
  authoritative and activation stays a deliberate human step.
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
- The turn's injected context and its **memories-used** audit (the
  `MemoriesUsed` event the `localpilot memory inspect` inspector renders) come
  from a *single* retrieval, so the audit lists exactly what was injected — never
  a memory ranked past the injected cap, nor a snippet truncated out of the
  block. Each injected block is recorded under its own layer: the repository
  primer as `primer`, ranked accepted memory as `memory`, and (legacy push-mode)
  ingested chunks as `ingest`.
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
| Step/commit completions (event log) | durable outcome anchors appended to the import as redacted facts |
| Failed-tool and quality-gate outcomes (event log) | pass/fail signal the extractor keys on, not re-parsed prose |
| Recovery diagnostics (event log) | frequent-failure candidate lessons |
| Agent-proposed lesson (`remember` tool) | a review candidate, enqueued in-session |
| Accepted memory | LocalMind retrieval and context injection |
| Skill drafts | LocalMind disabled `SKILL.md` draft emission, surfaced read-only via `skill_drafts` |

Close-out builds the import from the redacted transcript and then appends compact
structured signals from the session **event log** (failed tools, recovery
diagnostics, committed steps) so the deterministic extractor keys on the fact
LocalPilot already recorded rather than re-parsing prose. The deterministic text
path stays the baseline: when the event log has nothing notable, the import is the
transcript alone, unchanged. Only names, statuses, and short commit hashes are
appended — never raw payloads (ADR-0018).

All capture stays redacted-before-persistence and inside the permission boundary;
LocalMind never bypasses either.

## Extractor selection and the local inference default

LocalMind ships two extractors behind one `SessionExtractor` seam: a deterministic
one (heuristics over the redacted transcript) and a model-backed one that calls a
configured OpenAI-compatible endpoint. The host selects per the project's
`.localmind.toml`:

- `[inference]` configured with `features.extraction` on **and** a loopback
  endpoint → **model-backed** extractor, which falls back to deterministic
  automatically if the endpoint is unreachable or returns malformed output;
- `[inference]` configured against an **off-machine** endpoint → still
  **deterministic** unless off-machine learning egress is explicitly opted in
  (see below), so a session transcript never leaves the machine by default;
- otherwise → **deterministic** extractor.

**Local inference is wired by default, not by hand.** When a project is first
seen and its default LocalPilot provider points at a *loopback* endpoint (e.g. a
local LocalBox gateway at `127.0.0.1`), `initialize` writes a `.localmind.toml`
that already enables `[inference]` against that same local endpoint (the `/v1`
suffix is dropped — LocalMind appends the OpenAI path itself). So "local models do
the learning jobs" needs no extra config; if the gateway is down, extraction
degrades to the hardened deterministic baseline. The endpoint LocalMind sees is a
generic local URL — the engine stays host-neutral and learns nothing about
LocalBox internals (LocalMind decision D-LM-0002).

Per the ecosystem remote-egress policy, a **remote** provider is never wired into
learning automatically; pointing inference at a non-loopback endpoint is an
explicit, disclosed choice. Beyond config, the host enforces this at close-out:
model-backed extraction against an off-machine endpoint is reachable only when
the env opt-in `LOCALPILOT_LEARNING_ALLOW_REMOTE` is set (`1`/`true`). Without it
the off-machine endpoint is unreachable for learning and close-out falls back to
the deterministic extractor — the transcript stays local. When the opt-in is set,
each off-machine extraction writes an audit trail entry (endpoint host and model
only, never transcript content). The env-var form keeps this security-sensitive
egress switch out of any checked-in config that could travel with the project.

## Skill semantics (active-skill consumption contract)

A *skill* in this ecosystem is a **reviewable advisory prompt module**, not an
executable workflow (ecosystem decision: skills are advisory/active prompt
modules with no silent execution). The lifecycle and the host's consumption
contract:

1. **Draft → enabled → consumed.** LocalMind distills *disabled* skill drafts
   from accepted memory. A human enables a draft (`localpilot learning skills`),
   which makes it an *active* skill carrying provenance to its source memory.
2. **Host consumption is read-only and explicit.** Two model-callable tools
   surface skills, both read-only:
   - `skill_drafts` — lists/shows disabled drafts (suggestions to propose to the
     user);
   - `active_skills` — lists/shows enabled skills as advisory guidance the agent
     applies in its own reasoning.
   Each tool's only effect is a read inside the workspace
   (`Effect::ReadPath`). Reading a skill returns Markdown guidance with
   provenance; it never installs, enables, disables, or runs anything.
3. **No silent execution, no auto-install.** There is no path by which a skill
   is executed or activated automatically. Enabling, disabling, and retiring are
   deliberate human, review-gated steps. A skill body is *content the agent
   reads*, never an action the host takes.
4. **Budgeted.** A single skill body is bounded when surfaced, so pulling skill
   guidance stays lean alongside the always-on accepted-memory context.

This contract is what makes skills safe to surface by default: the worst case of
a wrong or stale skill is irrelevant guidance the agent can ignore, never an
unintended action.

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

### Layered retrieval contract (index → expand → fetch)

Retrieval is staged so a turn spends a small, bounded number of tokens to
*locate* the right knowledge before paying for any body:

- **Index** (`knowledge_search`) — the cheap layer: ranked locators (id +
  one-line summary + score).
- **Expand** (`knowledge_expand`) — cheap: the document neighbours (other chunks
  of the same file) around chosen ids, ids only.
- **Fetch** (`knowledge_fetch`) — the only expensive layer: full bodies for an
  explicit set of ids, and only those ids.

Each layer reports the token cost it spent, so the budget stays visible. The
budgeted packing path lays down cheap index summaries first and upgrades the
top entries to full bodies only while they fit a configurable budget; a tight
budget degrades gracefully to index-only and the packed total never exceeds the
budget. All three are read-only over the derived index and auto-allowed.

Two more agent tools close the learning loop in-session. `remember` lets the agent
propose a durable lesson, enqueuing a review candidate it never accepts itself.
`skill_drafts` lists or inspects generated skill drafts read-only without enabling
them. Both are registered on every session path; a tool-gated cue in the agent
system prompt mentions each only when it is present. Neither can write accepted
memory or activate a skill — review-gating stays sacred (ADR-0011, ADR-0018).

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
