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

### Adapter size and the extraction trigger (ADR-0036)

`localpilot-localmind` has grown into a sizable host-side subsystem (the ingest
engine plus the chunk store, layered pack, cold-start primer, derived search
index, and model-callable tools). That is cohesive and tested today, so **no code
moves now**. But it is a recorded trigger, not an open-ended licence to grow:
**before the next major ingestion/knowledge capability lands here**, split it one
of two ways rather than adding to the adapter —

1. carve the derived **index/search/pack** primitives into a narrower LocalPilot
   crate, leaving the adapter as the contract/redaction/permission seam; or
2. move the host-neutral derived-context primitives **behind a LocalMind API**.

Either split must preserve the invariants above: host-owned filesystem walking +
single-authority redaction, disposable/rebuildable `.localmind/ingest/` derived
state, review-gated accepted-memory writes, and the one-way LocalPilot→LocalMind
dependency edge. See ADR-0036 for the full decision.

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

### Pin policy: pinned for releases, floating for dev builds

The submodule pins one exact LocalMind commit in the LocalPilot git index —
currently a tagged LocalMind release. A release build of LocalPilot (working
tree exactly on a clean version tag) always builds against that tested,
known-good commit; it is never a moving target. The pin is advanced
deliberately as part of cutting a LocalPilot release (check out the new
LocalMind tag under `external/localmind`, commit the updated gitlink), not
floated automatically.

A non-release LocalPilot build — the working tree is not exactly on a clean
`vX.Y.Z` tag, i.e. `LOCALPILOT_VERSION` would carry a `git describe` suffix
like `-N-g<hash>` or `-dirty` (see `crates/localpilot-cli/build.rs`) — is
treated as local development. `install/install.ps1` and `install/install.sh`
detect this and fetch + check out LocalMind's latest `origin/main` instead of
the pinned commit, so iterating on both repos together doesn't get stuck on a
stale snapshot. This only changes what the install script checks out locally;
it never rewrites the committed submodule gitlink.

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

The two halves are independent: a surface that does **not** close out still
*reads* accepted memory. In particular one-shot `localpilot print` injects relevant
accepted lessons into its turn (via `register_context_hook`, the same hook the
interactive and serve loops use) — it simply never writes learning candidates back.
So curated guardrails seeded into accepted memory reach even the one-shot author;
they do not require the learning runtimes. `print --self-review` adds an opt-in,
read-only repo-health pass after the run (advisory, on stderr; never edits or
commits).

- The agent can propose a durable lesson in-session with the `remember` tool: it
  enqueues a review candidate (permission-gated, project-local write) and never
  writes accepted memory directly — promotion stays a human, review-gated step
  (ADR-0011).
- `localpilot research` (and the `/research` mode, ADR-0060) produces memory
  candidates from its **supported, provenance-backed** findings. Each is redacted
  and enqueued through the same review path as a retrospective lesson (the
  `write_retrospective_lesson` queue, ADR-0037) at a low prior confidence — never
  written to accepted memory. Unsupported or unbacked findings never become
  candidates. `--no-memory` skips the enqueue entirely.
- Optionally, `localpilot research` also ingests the written report into
  LocalMind's **documentation index** (`doc_chunk`) so it is semantically
  searchable and appears in the LocalMind UI — reusing the same chunker as
  `localmind ingest docs` in-process. This is off by default (`[research]
  ingest_report`), best-effort (a failure warns, never fails the run), and
  idempotent; it is independent of the review-candidate enqueue above. Doc chunks
  are source matter, not accepted memory.
- The agent can list or inspect generated skill drafts read-only with the
  `skill_drafts` tool. Surfacing a draft never enables it; the disabled flag stays
  authoritative and activation stays a deliberate human step.
- `localpilot learning` exposes the rich LocalMind loop: `closeout`, `review`,
  `promote`, `search`, `skills`, `audit`, `freshness`, and `lifecycle`.
- `localpilot learning freshness` runs the proactive freshness pass: it flags
  stale / never-retrieved / version-sensitive accepted memory **for review** (by
  age, never-retrieved-after-a-grace, and a version-sensitive heuristic), across
  the project and global stores (`--scope project|global|both`). It is **dry-run
  by default** (`--apply` writes), bounded by a per-run cap (threshold/cap flags:
  `--max-age-days`, `--unused-grace-days`, `--version-sensitive-min-age-days`,
  `--max-flags`), and **never deletes** — it only routes to the existing review
  gate, so a flagged lesson is resolved with `learning review` (accept / supersede
  via edit) or `memory delete`. `localpilot learning lifecycle` lists the queues
  (flagged-for-review, never-retrieved, most-used, contradicted). Both support
  `--format human|json`. Usage counts that drive "never retrieved"/"most used" are
  bumped post-turn, off the retrieval read path.
- `localpilot learning revalidate` is the **opt-in, default-off** deeper check: it
  asks the configured local model whether version-sensitive lessons are still
  current and flags "no longer true" ones **for review** (never deletes). It is
  **network-touching and disclosed** (policy D007): a preview (no `--apply`) counts
  candidates **offline and contacts nothing**; only `--apply` contacts the model
  (egress disclosed on stderr). The offline `learning freshness` pass needs no
  model and is the default; the live re-validation run is opportunistic.
- **Review modes (`.localmind.toml` `[review] mode`).** The gate between a
  candidate lesson and durable memory has four modes, set in the LocalMind
  config, not `.localpilot.toml`:
  - `manual` (**default**) — every candidate waits for a human `learning review`
    decision. Nothing is promoted automatically.
  - `assisted` — candidates are annotated (quality, duplicates, contradictions)
    but still wait for a human.
  - `trusted` — a high-confidence candidate (≥ `trusted_threshold`) auto-accepts,
    and a high-confidence contradiction with a clear target auto-supersedes the
    contradicted memory; duplicates, low-confidence, and non-`general`-quality
    candidates still route to a human.
  - `automatic` — as `trusted`, applied on every closeout.

  `trusted`/`automatic` are an explicit opt-in: on those modes some accepted
  memory is written **without a per-candidate prompt** (still local-only,
  redacted, quality-gated, and never auto-deleting). Leave the default `manual`
  if you want to approve every write.
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
- Each ingested chunk is tagged with its file's programming language (via the
  same `language_for_extension` map accepted-memory tagging uses), and
  `knowledge_search` filters results to the workspace's dominant language —
  excluding off-language chunks while keeping language-neutral (`NULL`-tagged,
  e.g. docs) chunks eligible. A workspace with no dominant language detects no
  signal and applies no filter, so keyword retrieval is unchanged.
- When an embedding model is configured (the local CPU embed server) and
  reachable, `knowledge_search` is **hybrid**: the query is embedded and the
  cosine-nearest chunk vectors are blended into the keyword results, so a
  semantically-relevant chunk the keyword query missed is still recalled. Keyword
  (term-match) hits stay the **floor** — every keyword hit ranks above every
  vector-only hit, so a strong keyword hit always surfaces; cosine only
  sub-orders. With no embedding model, or when the endpoint is unreachable, the
  query embed is skipped and retrieval is byte-identical to the keyword-only
  ranking.
- Context compaction manages the active model projection only. It can emit a
  structured, source-grounded runtime digest and safe audit metadata, but it
  does not write accepted memory, create skill drafts, or enqueue LocalMind
  review items.

State is project-local under `.localmind/`. Durable memory is readable Markdown;
queue, audit, search index, and the code-structure graph live in SQLite.

**Machine-wide global memory (on by default).** Most memory is project-specific,
but LocalMind also keeps a **global** store shared across every project on the
machine — for cross-project knowledge like tool-use patterns, debugging recipes,
and durable user preferences ("the more you use it the smarter it gets"). It is
**on by default** (`allowed_scopes` defaults to `["project", "global_user"]`); a
project that wants project-only memory narrows it:

```toml
[learning]
enabled = true
allowed_scopes = ["project"]          # opt out of the machine-wide store
# global_memory_root = "/abs/path"    # optional; default is ~/.localmind/memory
```

The global store lives at the per-user home (`~/.localmind/memory`, resolved
cross-platform, overridable by `global_memory_root` or the `LOCALMIND_GLOBAL_ROOT`
env), with its own index, separate from any project. A conservative classifier
routes only clearly cross-project lessons there (project-specific knowledge stays
in the project store); promotion is still review-gated, and retrieval merges
project + global with **project precedence** (a project lesson overrides a global
one on conflict). `local_only` (the global store is same-machine, never remote).
See
[LocalMind on-disk-contract](https://github.com/C0deGeek-dev/LocalMind/blob/main/docs/on-disk-contract.md)
§Global-scope store and D-LM-0017.

## Store resolution

`localpilot learning` and `localpilot memory` resolve the store like `git`
resolves its repository root: starting from the current directory, they walk up to
the nearest ancestor that holds a store (`.localmind.toml` or the `.localmind/`
directory). So running a command from a project subdirectory answers from the
*project's* store instead of silently using — or creating — a different, empty one
beside the cwd. The resolved root is logged to stderr so the caller can see which
store answered.

- **`--workspace <path>`** pins the store root explicitly and skips the walk-up.
  Use it when running from outside the project (`localpilot learning --workspace
  /path/to/project search "query"`). It is accepted on both `learning` and
  `memory`, ahead of the subcommand.
- **A read never creates a store.** `learning search` and `memory search` are
  read-only: when no store is found they report it and write nothing. Their stdout
  stays script-stable — an empty `--json` result is still a valid empty array.
- **Three empty outcomes are distinguished** on stderr so a bare `no matches` is
  never ambiguous: (a) *no store found* at or above the cwd; (b) a store exists but
  holds *no accepted memory yet*; (c) a non-empty store whose memory the *query
  missed*.

## Loop-Outcome Lesson Writeback

When a human accepts or rejects a self-improvement patch proposal, the outcome is
written back as a durable lesson so the next loop run retrieves it and stops
repeating a mistake (LocalMind decision `D-LM-0014`). This reuses the **existing**
review-gated path — it builds no new store:

- A loop-outcome lesson carries `{ trigger, what, why, applies_to, outcome,
  provenance_ref }` and is enqueued as a `CandidateLesson` through the normal
  review queue. Promotion to accepted memory stays a human, review-gated step
  (ADR-0011); the patch decision is not auto-promotion of the lesson.
- A **rejected** outcome is a first-class negative signal — an `AntiPattern`
  candidate framed as a steer-away ("Avoid (rejected): …") that records what was
  proposed and why it was rejected — not the absence of a lesson. An accepted
  outcome is a `Process` lesson.
- The lesson carries provenance (an evidence ref to the change-provenance record)
  and its outcome (the category), so retrieval can weigh it and an audit can trace
  it.
- **Retrieval-on-next-run:** once a loop lesson is accepted, it is ordinary
  accepted memory — `localpilot self-review` pulls it (via `memory_list`) as a
  prior lesson, and a finding on a file the lesson names is surfaced as a
  recurring issue. A rejected proposal's anti-pattern steers the next run away.
- **Pollution guard / curation:** lessons are review-gated, so a rejected
  candidate never reaches accepted memory; a bad *accepted* lesson is curated with
  the existing `memory delete` (supersede) path. No special-case store.

The host surface is `localpilot_localmind::write_loop_lesson`; everything else
(review, promote, search, delete) is the existing LocalMind loop.

## Completion-Retrospective Lesson Bridge

The harness completion retrospective (ADR-0035) records advisory lessons to the
root `LESSONS.md` — a human-editable mirror. Those lessons are **also** offered to
the same review-gated queue, so a lesson can become accepted memory through human
review instead of living only in an un-gated file (ADR-0037).

- The host surface is `localpilot_localmind::write_retrospective_lesson`; the cli
  calls it for each retrospective lesson right after the run prints its summary.
- **Advisory and non-blocking.** A failed enqueue never breaks a finished run, and
  `LESSONS.md` stays the human-editable mirror — it is written by the retrospective
  before the offer runs and is not touched by the bridge.
- **A different shape from a loop-outcome lesson.** A retrospective lesson is a
  free-text advisory note, so it sets **no** accepted/rejected `outcome` and **no**
  change-provenance ref (it has neither). It is a `Process` candidate with a
  `completion_retrospective` evidence kind and a deliberately lower prior confidence
  (`0.4`, below the loop-outcome `0.75`) — an unverified self-observation entering
  review, not a human-confirmed patch outcome.
- **Queue-noise policy.** A too-short/sentinel lesson is skipped; duplicates are
  deduped by the review queue's canonical-hash (a repeated lesson bumps a seen-count
  rather than re-enqueuing). No custom dedup — the store already provides it.
- **Review-gated, never auto-accepted.** The candidate is `PromoteToMemory`;
  promotion to accepted memory stays a human step (ADR-0011), and a rejected
  candidate never reaches memory.

### Driver interventions ride the same bridge

When an external agent host drives a session over `localpilot mcp serve`
(see [embedding.md](embedding.md#mcp-over-stdio)), its **corrections** —
steer texts, turn cancellations, and permission denials — are captured as
`driver_intervention` events in the session event log and, on disconnect,
offered to the same review-gated queue through the same host surface
(`RetrospectiveLesson::driver_intervention`).

- **Honest provenance.** The queue entry is labelled `driver-intervention`
  (never `completion-retrospective`), and its evidence names the driving
  client from the MCP handshake — the reviewer always sees who actually said
  it. Same posture as research findings (see the decision log).
- **Corrections, not consent.** An *approval* is routine and stays
  event-log-only; only steers, cancels, and denials become candidates, capped
  per session so a noisy drive cannot flood review.
- Same gates as every candidate: quality bar, near-duplicate folding, human
  review before memory.

## Argument-Repair Feedback (opt-in)

When `[tools] repair_learning` is on (default off), a closed session's
argument-repair patterns are offered to the **same** review-gated queue, so a
human can learn "this model tends to send this tool's arguments in the wrong
shape." It wires the previously-unused `tool_use_candidate` producer onto the
review path — no new store.

- The host surface is `localpilot_localmind::enqueue_repair_signals`, called from
  `closeout_session` only when the flag is on.
- **Aggregate and redacted (reuse-only).** A signal is one `(model, tool)` pair
  with malformed-class labels and counts — derived from the redacted
  `tool_input_repaired` events. It stores **no** raw inputs, paths, or content; the
  candidate's evidence is redacted before persistence (the best-effort-redactor
  caveat in `redaction.rs` is acknowledged, which is why only labels/counts — never
  values — are ever carried).
- **Review-gated, low prior.** A `ToolUse` candidate at `0.4` confidence,
  `PromoteToMemory`; promotion to accepted memory stays a human step (ADR-0011), and
  the review queue's canonical-hash dedup keeps a repeated pattern from piling up.
- **No automatic rule cue.** A repair signal is *not* auto-promoted to an always-on
  rule cue — per-model cue sprawl is an open question. A human may promote an
  accepted candidate to a cue through the existing `register_rule_cues` path.

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
localpilot learning export --out pack.json [--scope project|global|both]  # signed memory bundle
localpilot learning import pack.json [--apply]                            # verify -> review-gated
localpilot memory inspect
localpilot memory delete <memory-id>
localpilot memory graph <symbol>
localpilot memory export graph.json   # NOTE: the code-graph snapshot, not the bundle
```

### Portable knowledge bundles (`learning export` / `learning import`)

`learning export` writes a **portable, signed** bundle of accepted memory — the
way to move knowledge across your own machines and share it. The round-trip lives
under `learning` (not `memory`) because `localpilot memory export` is the
code-graph snapshot. The bundle is built from the Markdown source of truth,
re-redacted on the way out, deterministic, and signed (Ed25519 over a SHA-256
digest) with a local keypair stored `0600` under `~/.localmind/keys/`.

`learning import` verifies the pack **fail-closed** and is **review-gated**:

- A tampered/forged/oversized/unknown-version pack is **rejected** and never
  reaches the store.
- A valid signature by an **unknown** key imports as *untrusted* (flagged for
  heavier review); your own or a trusted key is *trusted*.
- It is a **dry run by default** (writes nothing, reports counts); `--apply`
  enqueues the entries as **review candidates** — never straight into active
  memory. Each carries import provenance (origin author, trust class, digest).

**Trust UX (stated plainly in the CLI output and here):** *a verified author is
not verified content.* A signature attests integrity and authorship only;
imported memory is still reviewed before it is used. Trust is local — a keypair
plus a manual trust list, no PKI or network. See LocalMind
`docs/decisions.md` D-LM-0018 and `docs/on-disk-contract.md` §Signed bundle.

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

## Learning happy path

The end-to-end loop, from an empty store to a lesson that shapes a later turn:

```bash
# 1. (optional) bootstrap curated, author-reviewed lessons straight into memory
localpilot learning seed --file seed-packs/coding-lessons.json   # idempotent; audited

# 2. do real work, then turn the session into candidate lessons
localpilot print --allow-writes "…task…"
localpilot session list                       # find the session id
localpilot learning closeout --session <id>   # extract candidates -> review queue

# 3. human gate: review and promote
localpilot learning review list
localpilot learning review accept <item-id> --reviewer you
localpilot learning promote <item-id>         # -> durable accepted memory

# 4. retrieve / confirm reuse
localpilot learning search "topic"            # add --json for structured output
localpilot memory used                        # what shaped the latest turn (provenance)
```

Notes:

- **`learning seed` writes accepted memory directly** — the human gate moves to
  *authoring* time (the pack is curated and reviewed before it is committed), so
  seeding skips the per-session review queue. It is idempotent (body-level dedup)
  and records one audit row per lesson, so a seeded memory has the same
  `learning audit` provenance trail as a promoted one. Use `--dry-run` to validate
  a pack without writing. The shipped `seed-packs/coding-lessons.json` includes
  general code-authoring guardrails (propagate a subprocess exit code; drain child
  stdout/stderr without deadlocking; pass args as a list not a quoted string; guard
  a process launch; factor duplicated parse/format logic; don't claim a build/tests
  pass before running them) — once seeded, they ride into any turn that reads
  memory, including one-shot `print`.
- **`learning closeout` tolerates a reasoning model's output.** A local model
  commonly wraps its extraction JSON in a `<think>…</think>` block and a
  ```` ```json ```` code fence; closeout strips those before parsing and, on any
  parse failure, falls back to the deterministic extractor rather than aborting.
  A clean, successful session with no failure/correction signals may yield zero
  deterministic candidates — that is expected, not an error.
- **`learning search`** is keyword (FTS) by default and works offline; semantic
  ranking requires an `[inference]` embedding endpoint in `.localmind.toml`. Pass
  `--json` to consume results from a script or agent.
- **`localpilot models`** lists models only for providers that expose an OpenAI
  `GET /models` endpoint. A local model wired through the Anthropic-compatible
  no-think proxy (the LocalBox default) is not listable that way; set
  `[providers.local].model` so the configured model is explicit.
- **`localpilot print` always returns a readable terminal state.** A reader that
  closes stdout mid-stream (a closed pipe) is a clean stop, not a crash — `print`
  exits `141` (the SIGPIPE convention) so a wrapper can tell "the reader left" from
  a real failure. Set `[harness] turn_timeout_secs` to bound a long turn by
  wall-clock (off by default). Either way `print` ends with a one-line, parseable
  `handoff:` summary on stderr — stop reason, tool calls, files changed, and whether
  memory was written (always `false` for one-shot `print`, which reads memory but
  never closes out) — so a non-interactive caller always has a terminal state to act
  on. See ADR-0049.
