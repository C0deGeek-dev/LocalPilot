# Changelog

Notable changes per release. As of 1.0.0 the public CLI/config/provider surface
is SemVer-stable; the configuration schema stability policy is in
[docs/configuration.md](docs/configuration.md).

## Unreleased

- **Vision (image input) is a resolved per-provider capability (ADR-0061).**
  LocalPilot no longer assumes every local OpenAI-compatible server is text-only.
  A model's vision support resolves in precedence **config > probe > false**: a new
  per-provider `supports_vision` flag (user-set, or auto-written by LocalBox when it
  loads a multimodal projector) wins; otherwise a best-effort, **read-only** probe
  of a local llama.cpp server's documented `GET /props` `modalities.vision` (no
  model inference; toggleable via `[discovery] vision_probe`, default on; an
  unreachable/signal-less server is treated as unknown, never a false claim);
  otherwise text-only. The OpenAI adapter's image-input gate becomes "official API
  **or** vision resolved true", so an undeclared provider is byte-identical to
  before. `doctor` reports the declared capability and `localpilot models` the full
  resolved capability and its source; the interactive image-attach preflight now
  refuses with actionable guidance (how to declare `supports_vision`) instead of
  sending an image blind. No `GET /v1/models` augmentation and no active trial-image
  probe. See `docs/04-provider-contract.md` §Vision and `docs/configuration.md`.

- **New `/research` mode and `localpilot research` subcommand (ADR-0060).** A
  bounded research loop decomposes a topic into sub-questions, gathers evidence
  across local sources (ingested knowledge + accepted memory), cross-checks each
  finding against its evidence, and produces both a redacted Markdown report and
  **review-gated** memory candidates (never written to accepted memory). It is
  reachable interactively (`/research <topic>` one-shot; bare `/research` enters a
  persistent research mode) and headlessly (`localpilot research <topic>`, with
  `--no-report`/`--no-memory`). When a provider and model are configured the model
  decomposes the topic; synthesis stays grounded in gathered evidence so a finding
  is always backed. The loop lives in a new host-neutral `localpilot-research`
  crate. **Web research is off by default** and reachable only via the headless
  `localpilot research --web` opt-in, which prints an egress disclosure, fetches
  only allowlisted domains (others are skipped and logged), sends only the redacted
  sub-question, and audits every request; `[research.web] enabled = false` is the
  kill switch. Configure under `[research]`; see `docs/configuration.md` and
  `docs/07-security-and-privacy.md`.

- **Outcome-aware down-weight wired to the uplift eval (ADR-0046/ADR-0059).** The
  engine's reasoned route-to-review flag was built but never wired to an outcome
  signal. It is now wired to the uplift A/B eval (not a live turn — one turn is too
  weak a signal): when an arm that injected a set of lessons under-performs its
  control, those lessons are routed to review (never deleted) for a human to
  re-judge, joined by the per-turn `memories_used` audit. Off by default
  (`[memory] outcome_downweight`); only `memory`-layer ids are eligible; reversible.

- **Semantic relevance gate at memory injection (ADR-0059).** Accepted-memory
  injection was gated only by keyword bm25 score (unnormalized, not portably
  tightenable), so a same-language but off-topic lesson could inject into an
  unrelated task and mislead the model (the negative transfer seen in the v1.1.0
  sweep). The injection layer now embeds the prompt once per turn and scores each
  keyword candidate by normalized cosine over the stored vectors, gating any hit
  below `[memory] injection_min_cosine` (default `0.6`; `0.0` disables). Because
  cosine is normalized it ships **default-on**, but it is **best-effort**: with no
  embedding endpoint (or an unembedded lesson) the hit carries no cosine and is
  injected exactly as on the keyword path — a no-embed run is byte-identical. The
  keyword search stays the candidate floor; cosine only re-filters. Reuses the
  engine's `embed_query` + global-aware `vector_search`. See
  `docs/configuration.md`.

## v1.1.0 - 2026-06-29

Coordinated LocalX release.

- **`localpilot eval` verifies the build before finishing, by default.** The
  verify-before-done gate is now **on by default for `eval`** (opt out with
  `eval --no-verify`, which reproduces the prior behaviour byte-for-byte), so a
  benchmark measures compiled+tested solves instead of code the model never
  built. Interactive and `print` turns are unchanged (the `[harness]
  verify_before_done` config default stays `false`). Stack detection gains a
  C++ branch: a workspace with C++ sources at the root (a CMake project or a
  bare exercism layout) is compile-checked with an artifact-free
  `g++ -std=c++17 -I. -fsyntax-only <sources>` — catching "it never compiled"
  without writing build artifacts into the captured diff. When the gate is on
  but no target is detected, a warning makes the un-verified finalize visible.
  The gate runs in the workspace's de-verbatim cwd (see above), so its build
  command no longer ran in a fallback directory on Windows. The legacy
  `--verify` flag is accepted but redundant.
- **Edit tools tolerate indentation drift and guide a failed edit.** `edit_file`,
  `multi_edit`, and `apply_patch` now share one anchored matcher: an exact unique
  match first, then a single leading-indentation-tolerant rung that applies only
  on a *unique* block whose indentation differs by one consistent whitespace
  prefix (re-indenting the replacement to the file), then a guiding error — the
  match count for an ambiguous edit, or the nearest existing line plus a re-read
  hint for a not-found one — instead of a bare "old_text was not found". An empty
  or identical-to-`new_text` `old_text` is rejected. Matching stays anchored,
  never fuzzy (no best-guess location); CRLF handling and `multi_edit`/
  `apply_patch` atomicity are unchanged. This cuts the "model gives up and
  rewrites the whole file" failure when its `old_text` indentation is slightly off.
- **The Windows shell prefers PowerShell 7 (`pwsh`), so `&&` chains work.** A
  `run_shell` `command` string runs through `pwsh` when it is on PATH, falling
  back to `powershell.exe` (Windows PowerShell 5.1) otherwise. `pwsh` supports
  the `&&`/`||` chain operators that 5.1 lacks, so a chained command
  (`cargo build && cargo test`) runs as written instead of erroring — which is
  what taught the learning corpus junk "PowerShell doesn't support `&&`" lessons.
  Detection is cached; it is *prefer*, not *require* (a host without `pwsh` still
  works with `;`). A timed-out command's whole process tree is killed
  (`taskkill /T /F`; a process-group `kill` on Unix), confirmed by test, so a
  hung build's grandchildren (`make`→`cc1`, `gradle`→daemon) never orphan.
- **Child processes run in the workspace on Windows, not a fallback directory.**
  The sandbox canonicalizes the workspace root to a verbatim extended-length path
  (`\\?\…`); handed to a child process as its working directory, a launched shell
  could not use it (cmd fell back to `C:\Windows`, PowerShell resolved relative
  paths against a broken `$PWD`), so every model-issued build/test command ran
  *outside* the workspace and failed. The shell, git, background, and
  verify-before-done child processes now spawn in a de-verbatim equivalent of the
  same directory (`Workspace::process_dir`, via `dunce`), while the verbatim
  containment root and its `starts_with` boundary are unchanged. Windows/Linux/
  macOS parity; no behaviour change off Windows.
- **Ingest keyword retrieval ranks by FTS bm25, and short query terms match whole
  tokens.** `knowledge_search`'s keyword tier now ranks by the FTS index's own
  **bm25** score (IDF-weighted, so a common token like `and` ranks far below a
  rare one), with the file-path column weighted above the body — replacing the old
  flat term-count + substring path bonus. Query terms of 3+ characters still match
  as prefixes (`pars` → `parser`); shorter terms match a whole token exactly, so
  `an` no longer matches `and` (and `do` no longer matches `docker`). This is a
  deliberate ranking change (ADR-0057, refining ADR-0025) — it reorders some
  results by design; the hybrid keyword-floor/vector blend shape is unchanged.

- **`knowledge_search` is hybrid keyword+vector retrieval when embeddings are
  configured.** With an embedding model set (and reachable), the query is embedded
  and the cosine-nearest chunk vectors are blended into the keyword results, so a
  semantically-relevant chunk the keyword query missed is recalled. Keyword
  (term-match) hits stay the **floor** — a keyword hit always ranks above a
  vector-only hit, so a strong keyword hit always surfaces; cosine only
  sub-orders. With no embedding model, or when the endpoint is unreachable, the
  result is **byte-identical** to the prior keyword-only ranking (a bounded vector
  window keeps the pass cheap).

- **Ingested chunks are embedded on ingest (best-effort, opt-in) into a chunk
  vector index.** When an embedding model is configured (the same
  `[inference]` embedding gate accepted-memory embedding uses — the local CPU
  embed server), each ingested chunk is embedded into a new rebuildable
  `ingest_chunk_vectors` table (schema v4, mirroring the accepted-memory
  `vector_index` shape). It is **best-effort**: an unchanged chunk is not
  re-embedded (content-fingerprinted), a down/unconfigured endpoint writes no
  vectors and never fails ingest, and chunk vectors are dropped with their chunks.
  With no embedding model configured this is a no-op, so ingest stays exactly the
  keyword path. `ingest run`/`refresh` report `embedded: N of M chunks` when
  embeddings are active. New `[ingest] embed_chunks` (default `true`) opts out of
  the per-chunk ingest embedding cost while keeping accepted-memory embeddings.

- **Ingested folder knowledge is language-tagged and `knowledge_search` filters
  to the workspace language.** Each ingested chunk now records its file's
  programming language (reusing LocalMind's `language_for_extension` map — the
  same one accepted-memory tagging uses), and `knowledge_search` filters hits to
  the workspace's dominant language (via `detect_workspace_language`), excluding
  off-language chunks while keeping language-neutral (`NULL`-tagged, e.g. docs)
  chunks eligible. A docs-only or mixed workspace detects no dominant language and
  applies no filter, so keyword retrieval stays byte-identical to before. The
  chunk store migrates additively (schema v3, nullable `language` column;
  pre-existing chunks read as untagged until re-ingested).

- **Accepted memory now has a proactive lifecycle: usage tracking + a freshness
  pass + an operator surface.** A memory's hit count is bumped when it is injected
  into a turn (best-effort, post-turn, off the retrieval path), so dead weight and
  high-value lessons are both visible. New `localpilot learning freshness` flags
  stale / never-retrieved / version-sensitive accepted memory **for review** — by
  age, never-retrieved-after-a-grace, and a version-sensitive heuristic, across the
  project and global stores (`--scope project|global|both`); it is **dry-run by
  default** (`--apply` writes), bounded by a per-run cap, and **never deletes** — a
  flagged lesson is resolved through the existing `learning review` / `memory
  delete` path. `localpilot learning lifecycle` lists the queues (flagged,
  never-retrieved, most-used, contradicted). Both honour `--format human|json`.

- **Optional source re-validation (`localpilot learning revalidate`, opt-in,
  default-off).** Asks the configured local model whether version-sensitive
  accepted lessons are still current and flags "no longer true" ones **for
  review** — never deletes. It is **network-touching and disclosed**: a preview
  (no `--apply`) counts candidates **offline and contacts nothing**; only
  `--apply` contacts the model (egress is disclosed on stderr). The offline
  `learning freshness` pass needs no model and stays the default; this deeper
  check is opportunistic.

- **`edit_file`/`multi_edit`/`apply_patch` match across CRLF/LF line endings.**
  The edit tools matched `old_text` against the raw file bytes, so a model that
  emits `old_text` with `\n` could not edit a CRLF-stored file — every attempt
  failed "old_text was not found", pushing the model to give up and rewrite the
  whole file (and to keep re-learning that workaround as a lesson). Matching now
  runs on a line-ending-normalized form; the file's original CRLF/LF style is
  preserved on write.

- **Injected memory's language filter now also catches idiom-named lessons.** A
  lesson learned in a language but named only by idiom (a Go `sort.Strings`
  pattern) is tagged with the session's language at promotion (LocalMind), so the
  workspace-language injection filter excludes it from other languages instead of
  leaking it as noise.

- **Injected memory is filtered by the workspace language.** The session's
  dominant language (a bounded, cached scan at session start) is pushed into
  accepted-memory retrieval, so a lesson clearly about another language is
  excluded inside LocalMind's query (schema v7) rather than retrieved and
  dropped afterward — a Python idiom no longer lands in a Rust task and wastes
  the injection budget. A lesson that names no single language stays eligible
  everywhere. Opt out with `[memory] injection_language_filter = false`. The
  extension→language table now lives in LocalMind, shared with the stored lesson
  tag, so the workspace signal and the tag cannot drift.

- **Learning is on by default (`localpilot eval` stays clean-room).** LocalMind
  learning now defaults **on** (D-LM-0019), so interactive and agentic runs
  accumulate reviewed, machine-wide memory out of the box — `local_only`, review-
  gated (candidates, never auto-active), opt out with `[learning] enabled = false`.
  Capability measurement is unaffected: **`localpilot eval` neither reads nor
  writes accumulated memory by default** (clean-room), and a new **`eval --learn`**
  flag opts a run into closing the session out into LocalMind (review-gated lesson
  candidates, scope-routed to the global store) — for turning a benchmark or
  scripted run into a learning corpus without contaminating a measurement arm.

- **Portable signed knowledge bundles (`learning export` / `learning import`).**
  Accepted memory can be exported to a portable, signed bundle and imported on
  another machine or from someone else. `learning export --out pack.json [--scope
  project|global|both]` writes a deterministic, re-redacted, Ed25519-signed pack;
  `learning import pack.json [--apply]` verifies it **fail-closed** (a tampered or
  unknown-version pack is rejected and never stored), classifies trust
  (trusted/untrusted by signing key), and is **review-gated** — a dry run by
  default, `--apply` enqueues entries as review candidates with import provenance,
  never straight into active memory. The CLI states plainly that *a verified
  author is not verified content*. Trust is local (a keypair + manual trust list,
  no PKI). The round-trip lives under `learning` because `memory export` is the
  code-graph snapshot. See `docs/localmind-integration.md`.

- **Machine-wide global memory (on by default, via LocalMind).** A **global**
  store shared across every project on the machine is now on by default, so
  cross-project knowledge (tool-use patterns, debugging recipes, durable user
  preferences) accumulates and "the more you use it the smarter it gets" fires
  across projects. The store lives under `~/.localmind/memory` (overridable by
  `global_memory_root` or `LOCALMIND_GLOBAL_ROOT`); a conservative classifier
  routes only clearly cross-project lessons there, promotion stays review-gated,
  and retrieval merges project + global with project precedence. `local_only`
  (same-machine, never remote). A project that wants project-only memory sets
  `allowed_scopes = ["project"]`. See
  [docs/localmind-integration.md](docs/localmind-integration.md) and LocalMind
  D-LM-0017.

- **Project instruction files are injected directly, every turn (default-on).**
  `CLAUDE.md`/`AGENTS.md` previously reached the model only through the
  review-gated learning store, so a fresh checkout's instructions might never be
  seen. LocalPilot now injects the merged instruction document **directly into
  the turn context every turn** — ungated and independent of learning — bounded by
  `[context] instruction_char_budget` (8000 chars, truncate-with-marker over
  budget) and redacted first. Discovery gains two conventions: a first-class
  **`Navigator.md`** (LocalPilot's own, highest precedence) and
  **`.github/copilot-instructions.md`** (lowest), alongside `CLAUDE.md`/`AGENTS.md`;
  within a tier they order by kind (`Navigator` > `CLAUDE` > `AGENTS` >
  copilot). Opt out with `[context] inject_instructions = false`. The ingest path
  is unchanged (still review-gated). See ADR-0056.

- **Built-in loop safety rails — default-behaviour change.** A fresh project with
  no `[harness]` budget/timeout used to run an **unbounded** loop that a weak
  model could spin to an external SIGKILL with no scorecard. The loop now applies
  a conservative built-in bound when the config leaves a rail unset (an explicit
  `[harness]` value always wins): a headless run (`eval`/`print`/`harness` step)
  self-bounds to **200** tool calls and **600 s**; an interactive session bounds a
  runaway at **500** tool calls with no default wall-clock (a long interactive
  turn is legitimate and cancellable). This is a safety default, not a feature
  lever — an unbounded loop is a defect — so it ships on; tune or lift it with
  explicit `tool_call_budget`/`turn_timeout_secs`. The verify gate now also stops
  a turn with `NoProgress` (not a clean `Done`) when its build never goes green
  within the re-entry cap, tying the no-progress signal to the build result. The
  built-in default fills only the hard ceiling (no soft start), so the cost
  controller's no-progress branch is inert under it; the always-on degenerate-loop
  guard (ADR-0052: repeated/cyclic calls or a run of consecutive failures) now
  stays active for the built-in default and only defers to the controller when an
  operator sets an **explicit** budget — so a spinning or failing loop stops early
  on `NoProgress` instead of burning the whole ceiling. See
  [docs/06-harness-spec.md](docs/06-harness-spec.md) §Built-In Safety Rails and
  ADR-0055 (refining ADR-0029/0052).

- **Verify-before-done gate (`[harness] verify_before_done`, default-off).** A
  solve loop ends when the model stops calling tools, which let a turn "finish"
  code it never built — the largest avoidable cause of compiled-language losses.
  When enabled, a turn that would finalize with no tool call first runs a
  build/test verification; on failure the diagnostics are fed back and the loop
  continues instead of declaring success. The command is detected from the
  workspace stack (`cargo test`, `go test ./...`, `npm test`, `python -m pytest`,
  `mvn`/`gradle test`, `make`) or set explicitly with `[harness] verify_command`.
  It reuses the permission-gated quality-gate runner (no second command engine or
  retry loop) and is bounded by the budget/timeout rails plus a fixed re-entry
  cap. `localpilot eval --verify` / `--verify-command <cmd>` enables it for one
  run so a benchmark arm can measure its lift. Off by default (a feature lever);
  see [docs/06-harness-spec.md](docs/06-harness-spec.md) §Verify-Before-Done Gate
  and ADR-0054.

## v1.0.0 - 2026-06-24

Coordinated LocalX 1.0 release. First stable: the CLI, configuration, and
provider contract are now under SemVer. Validated on real local models,
including a cross-model sweep (lesson-injection uplift holds on a second model;
the grammar tool-call lever ships opt-in, default-off — no validity headroom on
either model measured).

- **Google Cloud Vertex AI Gemini via ADC.** Added `kind = "google-vertex-openai"`
  with `auth = "google_adc"` for projects that require Application Default
  Credentials instead of API keys. LocalPilot derives the documented Vertex
  OpenAI-compatible base URL from `google_project` + `google_location`, reads a
  gcloud `authorized_user` ADC file (`google_adc_path`, `GOOGLE_APPLICATION_CREDENTIALS`,
  or the gcloud default), mints short-lived OAuth bearer tokens in-process, and
  uses the same auth path for chat, `localpilot models`, and `/model`.
  `doctor` reports only `google_adc` / `google_adc_file`, never ADC JSON or
  minted tokens.
  Gemini tool calls now also preserve and replay the OpenAI-compatible
  `extra_content.google.thought_signature` metadata, avoiding Vertex/Gemini
  `Function call is missing a thought_signature` errors on multi-step tool use.

- **Outward self-improvement drafts (`self-review propose-issue`/`propose-pr`/
  `emit-draft`, default-off).** The self-improvement loop can now author a **draft**
  issue/PR from a ranked self-review finding and — only with an explicit human
  `--approve` — publish it as a **draft** to an allowlisted repo via the `gh` CLI.
  It is human-gated by construction: the same value-typed approval token that
  promotes a patch is required to publish, and the autonomous loop can never mint
  one (it can propose but not publish). The surface is off by default
  (`[self_improvement] enabled` + an `outward_targets` allowlist, both required and
  fail-closed); publication is draft-only (never ready/merge), dry-run by default
  (`emit-draft` without `--approve` prints the `gh` plan and publishes nothing),
  redacted, and writes drafts to the git-ignored `.localpilot/outward/` store for
  inspection before any publish. `drafts list`/`show`/`discard` inspect them.
  (ADR-0053, extends ADR-0034.)

- **`fetch` fails fast on a stalled connect.** The network tool now sets a connect
  timeout (bounded under the request timeout) so a hung TCP/TLS connect errors
  quickly instead of blocking the agent loop for the full request window.

- **Always-on degenerate-loop guard.** A turn can no longer spin unbounded when the
  tool-call budget is off. Even with the budget disabled, the loop now stops with
  `NoProgress` if the no-progress detector trips (a repeated or cyclic successful
  call set) or a run of consecutive *failing* calls exceeds a fixed conservative
  limit — the denied/failing spin the detector never saw (it is fed only by
  successful calls), which had let a weak local model loop for thousands of
  messages. A productive turn is never cut, and when the budget is configured the
  existing controller still owns the no-progress stop. "Budget off" still means no
  *cost* ceiling. (ADR-0052.)

- **Opt-in argument-repair feedback to LocalMind (`[tools] repair_learning`, default
  off).** At session close, the session's argument-repair patterns are offered to
  LocalMind's existing review-gated queue as aggregate, redacted candidates (which
  model needed which repair on which tool). Reuse-only: it stores no raw
  inputs/paths/content, writes no accepted memory, and adds no new store — a human
  promotes a candidate or it expires in review. A repair signal is never auto-promoted
  to an always-on rule cue.

- **Opt-in, conservative tool-argument repair (`[tools] repair`, default off).** A
  validator-first stage that, when enabled, repairs a *shape-invalid* tool call
  (a bare string where an array of strings is expected, a stringified array/object
  of the right item type, or a markdown autolink in a path field) on **only** the
  fields the validator flagged, re-validates, and either runs the repaired call —
  with a model-visible note saying what changed — or falls back to the readable
  error. It is gated by the tool's safety contract: a destructive, external-write,
  irreversible, or MCP tool, and any content/command field, is **never** repaired
  (`run_shell`, `apply_patch`, `git_commit`, `git_restore`, `fetch` get a readable
  error, never a silent rewrite). Repair changes arguments, never authority — the
  permission engine runs on the repaired input. `warn` applies and logs each repair
  loudly; `off` (the default) reproduces the prior behaviour exactly. Every repair
  and every high-risk refusal is a redacted session event. (The git contracts
  `git_restore`/`git_commit`/`apply_patch` are reclassified to their honest
  side-effect class so this gate is provable from the contract alone; this is
  advisory metadata only — the permission path and prompts are unchanged.)

- **Schema-aware tool-input validation errors and a dormant validity metric, lit up.**
  When a tool call's arguments are well-formed JSON but do not match the tool's
  schema, the model now receives a concise, schema-aware message — the offending
  field, the expected shape, and a valid example drawn from the tool's contract —
  instead of the raw deserializer string, so it can self-correct on the next turn
  (the validator-first / retry-with-error pattern). On by default; set
  `[tools] readable_errors = false` to restore the raw message (the rollback). The
  raw detail is always retained in the logs/telemetry. Independently, the
  previously dormant tool-input validity metric is now lit up: each tool call is
  validated against its schema and recorded as a redacted `tool_input_valid` /
  `tool_input_invalid` session event (classified by malformed-argument shape, never
  carrying a raw value), and the `eval` scorecard reports `schema_valid_rate`. This
  is measurement plus a message improvement — dispatch behaviour is unchanged.

- **`doctor` and `models` are agent-consumable (ADR-0048 `--format`, extended).**
  `doctor` gains `--format human|json` (`--json` alias; JSON by default off a
  terminal): the JSON adds the resolved **binary path**, the `git describe`
  **version**, the **provider kind/base URL/model/context window**, the **memory
  store root**, and a list of **capability tokens** — enough for a wrapper to
  detect a stale PATH binary vs the repo build (drift detection is the caller's
  job) and to feature-detect a surface (e.g. the `--workspace` flag) instead of
  guessing from the version. `models` no longer prompts then silently skips
  non-interactively: it gains `--format human|json`, a `--yes` flag, and a clear
  terminal state — under no-TTY (or `--yes`) it never blocks on a prompt, reports
  `approval_required` rather than skipping, and **exits non-zero** when an endpoint
  is unreachable or approval was required without `--yes`. The credential is still
  reported as a source label only, never the value.

- **`print` survives a closed reader and bounds a long turn.** A dogfood `print
  --allow-writes` run hung for minutes, then panicked with `failed printing to
  stdout: The pipe is being closed` when its reader closed stdout. Two fixes: the
  streamed-answer writes are now checked — a closed reader (`BrokenPipe`, or the
  Windows `ERROR_BROKEN_PIPE`/`ERROR_NO_DATA` codes) is a clean stop that cancels
  the turn and exits `141` (the SIGPIPE convention) instead of the process panic;
  and a new optional `[harness] turn_timeout_secs` bounds a turn by wall-clock, so
  a long or stuck turn stops with a terminal state rather than hanging. Either way
  `print` now emits a one-line, machine-readable `handoff:` summary on stderr —
  stop reason, tool calls, files changed, and whether memory was written — so a
  non-interactive caller always reads a terminal state. The timeout is unset by
  default (no behaviour change); set it to opt a turn into the bound.

- **Code-authoring guardrails in `seed-packs/coding-lessons.json` + an opt-in
  `print --self-review`.** The curated coding pack gains six general, model-actionable
  lessons distilled from a dogfood run where the local author wrote compilable code that
  skipped unspecified rigor: propagate a subprocess child's exit code (and surface its
  stderr); drain child stdout/stderr concurrently (and don't claim concurrency you didn't
  write); pass process args as a list, not a quoted string; guard a process launch like a
  missing argument; factor duplicated parse/format logic into one helper; and don't claim
  a build or tests pass before running them. Because one-shot `localpilot print` *reads*
  accepted memory (it injects lessons; it just never closes out), seeding these reaches the
  author with no new wiring. `print --self-review` adds an opt-in, read-only repo-health
  pass after a run (advisory, on stderr; never edits or commits), and `print --help` now
  states the reads-memory-but-does-not-learn contract.

- **Discoverable structured output for `learning search` / `memory search` (ADR-0048).**
  Adding `--json` was not enough — a dogfood run showed both the operator and the local
  model missed it and tab-parsed the human table. Now the format is resolved from context:
  when stdout is **not a terminal** (piped or redirected) the commands emit a JSON array by
  default; a real terminal still gets the human table plus a one-line stderr hint pointing
  at the structured form. A uniform `--format human|json` overrides either way (`--json`
  kept as an alias) — `--format human` forces the table even when piped. `memory search`
  gains the same JSON output as `learning search`. Stdout stays script-stable; the hint and
  diagnostics ride on stderr.

- **Workspace-aware LocalMind store resolution.** `localpilot learning` and
  `localpilot memory` now resolve the store like `git` resolves its repo root —
  walking up from the current directory to the nearest ancestor holding
  `.localmind` — so running from a project subdirectory answers from the project's
  store instead of silently using or creating a different, empty one. The resolved
  root is logged to stderr. A new `--workspace <path>` flag pins the root
  explicitly (skipping the walk-up). `learning search` / `memory search` are now
  read-only (a search never creates a store) and distinguish three empty outcomes
  on stderr — no store found, an empty store, and a non-empty store the query
  missed — so a bare `no matches` is no longer ambiguous. Stdout stays
  script-stable (an empty `--json` result is still a valid empty array).

- **`learning search --json`.** Accepted-memory search can emit a JSON array (id, score,
  path, snippet, category) for agent consumption, alongside the default human-readable
  text. Empty results are a valid empty array.

- **`doctor` reports a truthful version after a same-branch rebuild.** The embedded
  `git describe` version is captured by `build.rs`, which previously only re-ran when
  `.git/HEAD` changed — but a commit on the current branch advances the branch ref, not
  HEAD, so the reported version went stale after a pull + rebuild. The build script now
  also retriggers on the resolved branch ref and `packed-refs`.
- **`localpilot init` no longer writes a dangling default provider.** The starter
  `.localpilot.toml` shipped `default = "local"` with `[providers.local]` commented out,
  so the first `ask`/`print`/`chat` failed to resolve a provider. The `default` line is
  now commented alongside the provider block, with guidance to uncomment both once a
  provider is configured.
- **`localpilot models` explains an empty result.** When the only configured providers
  speak a protocol with no `GET /models` listing (e.g. `anthropic`), the command names
  them and explains the served model is whatever the local server has loaded, rather than
  printing a blanket "no providers ... configured".

- **`learning seed` now records an audit row per lesson.** Seeding writes accepted
  memory directly (the human gate moves to authoring time), but previously left no
  trace in `learning audit`. Each seeded lesson now writes an audit event (actor
  `seed`, subject = memory id, metadata naming the source and category), so a seeded
  memory has the same provenance trail as a promoted one. A dry run still writes
  nothing.

- **Advisory whole-repo teardown sweep at completion.** When `[harness]
  teardown_sweep` is enabled, the harness runs a read-only cleanup-audit pass at
  the completion seam alongside the retrospective — surfacing dead/abandoned code,
  duplicate/parallel logic, over-engineering, redundant data access, and doc/test
  drift as ranked advisory findings (each with a category, confidence, risk,
  recommended action, and the hidden-usage channels ruled out). It extends the
  existing `localpilot-selfreview` scanner (no second scanner), leans on
  `cargo machete`/`clippy`/`cargo deny` for tool-owned categories rather than
  re-deriving them, and is advisory by construction: it never blocks completion,
  edits code, or commits. Off by default; the same pass is available on demand via
  `localpilot self-review --cleanup`. See ADR-0047 and docs/06-harness-spec.md.

- **Promote a curated lesson to an always-on rule cue.** A seed lesson tagged
  `rule-cue` is injected every turn as terse, always-present guidance (independent
  of prompt relevance) — a weak model acts on a short always-on rule better than
  on a retrieved paragraph. Advisory, not an enforced harness rule (ADR-0027); the
  cue is excluded from the relevance block so it is never injected twice. Opt-in;
  default unchanged. See ADR-0046.
- **Outcome-aware down-weighting routes a lesson to review.** `flag_unhelpful_lesson`
  flags a lesson the uplift eval found unhelpful for human re-review (it stays
  active and is never auto-deleted), reusing the engine's reasoned route-to-review
  flag. See ADR-0046.
- **Accepted-memory injection tuning (`[memory]`).** A new config section makes
  always-on memory injection earn its context cost, with every default preserving
  the prior behaviour: `injection_min_score` (gate out weak matches so they don't
  fill the per-turn budget), `injection_context_aware` (scale the injected char
  budget toward the model's context window — a small model gets less),
  `injection_char_budget` (the budget / ceiling), and `injection_skip_categories`
  (skip a category a rule already enforces, so injection adds signal not
  redundancy). Additive and opt-in; default-off pending the uplift eval. See
  ADR-0045 and docs/configuration.md.
- **Selectable constraint encoding (`constraint_mode`).** A provider can now
  choose how a tool-call constraint is encoded: `response_format` (default — the
  OpenAI structured-output wrapper, unchanged) or `json_schema` (a documented
  llama.cpp server extension that sends the schema as a top-level `json_schema`
  field the server compiles to a grammar). Use `json_schema` for a local server,
  such as a turboquant `llama-server` build, that rejects the `response_format`
  wrapper — so the constraint engages the server's grammar instead of falling
  back to native tool-calling. Opt-in per provider (`[providers.<id>.options]
  constraint_mode = "json_schema"`); default and fallback are unchanged. See
  ADR-0044 and docs/04-provider-contract.md. **Live finding (2026-06-22):** on a
  turboquant `q3635ba3bapex` server the `json_schema` field still `400`s on the
  model's `<think>` prefix (same as `response_format`); only a raw GBNF `grammar`
  field engages there — so a third encoding, `constraint_mode = "grammar"`, was
  added: it emits a top-level GBNF `grammar` (a valid-tool-call grammar built from
  the tool names, JSON sub-grammar authored from the JSON spec). Live-verified to
  engage (`200`, valid constrained tool call after `<think>`). Per-argument schema
  constraint (a json-schema→GBNF converter) remains a follow-up. All three
  encodings are opt-in, default `response_format`; default-off pending a
  discipline eval.
- **Constrained decoding is disabled after a server rejects it.** A local
  OpenAI-compatible server that declares constrained decoding but returns a
  client error on the schema-constrained request now has the constraint dropped
  for the rest of the session after the first rejection, instead of re-sending
  it (and logging a fallback warning) every turn. Native tool-calling is the
  fallback, unchanged.
- **Curated best-practice seed packs.** `seed-packs/` ships opt-in coding and
  research lesson packs plus long-form references; seed them with `localpilot
  learning seed --file` or `localpilot ingest run`. Nothing is auto-loaded.
- **Seed curated lessons + re-enable memory injection.** `localpilot learning
  seed --file <pack.json>` writes a curated, author-reviewed set of best-practice
  lessons straight into LocalMind accepted memory (idempotent — re-seeding skips
  lessons already present; `--dry-run` validates without writing). `localpilot
  memory enable` clears the injection-disable flag that `memory disable` sets, so
  a lesson-on/off comparison is scriptable. See ADR-0043.
- **Switch provider/model mid-conversation with `/model`.** In the `chat` REPL,
  `/model` lists the configured providers and their models; `/model <provider>`
  or `/model <provider> <model>` re-points the active session — for example start
  on a local model and continue the same conversation on Anthropic or OpenAI. The
  switch selects an already-built provider (no rebuild, no re-auth), takes effect
  at the next turn boundary, and keeps the full transcript. Listing reuses the
  `GET /models` discovery and degrades gracefully offline. See ADR-0041.
- **Store API keys with `localpilot login` (bring-your-own-key).** `localpilot
  login anthropic|openai` deep-links to the provider's key page, takes a pasted
  key, validates it with one minimal request (`--no-verify` skips), and stores it
  in the OS keychain (Windows Credential Manager) or a `0600` per-user file
  (macOS/Linux); `localpilot logout <provider>` removes it. A stored key needs no
  environment variable: resolution is keychain → file → `api_key_env` → config.
  `localpilot doctor` now reports each provider's credential *source* (keychain /
  file / env / not set), never the secret. Bring-your-own-key only — no "sign in
  with Claude/ChatGPT" and no subscription credentials (ADR-0042). The keychain
  backend is the opt-in `keychain` build feature.
- **Prompt history survives a restart, scoped to the project.** The `chat`
  composer's Up/Down recall is now seeded from a durable store, so a new session
  starts with your past prompts instead of an empty history. The store is one
  global append-only file (`prompt-history.jsonl`) under the per-user directory
  beside `config.toml`, with each prompt tagged by the directory it was typed in;
  recall shows **only the current project's** prompts by default, and **Ctrl-T**
  toggles a view of every project's. It is on by default and fully opt-out via
  `[history] persistence = "none"` (no read, no write). Prompts are stored raw so
  recall is faithful, protected by mode `0600` on unix (the per-user directory ACL
  on Windows) and a bounded size; see ADR-0040 and
  `docs/07-security-and-privacy.md` (§Prompt History At Rest).
- **Gated `self-review propose-patch` write loop.** The write half of the
  self-improvement loop (ADR-0034) is now wired: `localpilot self-review
  propose-patch --finding <rank> --model <model>` asks a model to author a minimal,
  scope-confined fix for a ranked finding into an isolated git worktree and stops;
  `localpilot self-review promote --id <id> --reviewer <you> --approve` applies it
  to the main branch (the `--approve` flag is the explicit human act that mints the
  approval token — without it promotion is refused; fast-forward only, never
  pushes); `localpilot self-review discard --id <id>` drops the proposal. A proposal
  persists across invocations, so review can happen between propose and promote. The
  agent never mints the token, never merges, and never pushes — the gate is structural.
- **Scroll-up history no longer loses the start of a conversation.** In the
  `chat` REPL the inline live region used to be torn down and re-created every time
  its height changed (composer, activity tail, pickers). Early in a session that
  dropped freshly committed transcript blocks before they had scrolled into the
  terminal's native scrollback, leaving a hole in scroll-up history — the
  conversation's start gone while pre-launch shell output survived. The live region
  is now a fixed-height band, re-initialised only on a terminal resize, so every
  committed block stays in scrollback. Trade-off: a small constant gap above the
  composer when idle (tunable via `LIVE_REGION_HEIGHT`). See ADR-0039.
- **A large file write no longer degrades the session.** When a local model
  cannot emit a big file-write tool call as one well-formed payload, the harness
  used to re-prompt blindly and degrade without ever writing the file. It now
  detects the failed write specifically (a typed `MalformedToolArguments`
  provider signal carrying the tool name) and steers the model to write the file
  in pieces — the first section with `write_file`, each remaining section with a
  new **`append_file`** builtin (atomic, newline-preserving, binary-refusing) —
  recovering the write within the existing repair budget. The recovery ladder's
  input-shrink actions, previously computed but never applied, now compact
  history on a repeated bad turn. See ADR-0038 and `docs/06-harness-spec.md`
  ("Bad-output recovery").
- **Ingestion shows a live progress loader.** In the `chat` REPL the walking
  ingest actions (`/ingest run`, `/ingest refresh`, `/ingest resume`) no longer
  block silently: a working spinner runs while stage notices report discovering,
  files-to-parse, parsed *N*/*total* (throttled), indexing, and writing, ending
  in an `ingestion completed: … file(s), … chunk(s)` summary. `Ctrl-C` pauses an
  in-flight run — the chunks already written are kept, so `/ingest resume`
  continues instead of restarting — and failures surface as a notice rather than
  leaving the UI stuck. The non-interactive `localpilot ingest run`/`refresh`
  also print stage banners. Backed by a new `ingest_run_with_progress` engine
  entry point (the old `run` is a no-op-callback shim, so behaviour is
  unchanged). Docs corrected to match: `docs/01-product-spec.md` drops the
  never-shipped `/search` command and fixes `/resume` (it reopens the previous
  session; the harness workflows are `/harness-resume` / `/wait-resume`), and the
  wiki How-To/Troubleshooting pages show real `ingest`/`knowledge` subcommands.
- **Plan mode carries planning judgment.** The planner now prefers steps that
  extend or reuse the existing code named in the repository summary over adding
  parallel code, and must cover every acceptance criterion in the brief. `brief.md`
  gains an optional `## Risks & Rollback` section (absent in older briefs,
  round-trips losslessly), and the per-step worker prompt asks the model to update
  the matching documentation in the same step as a behaviour change. When a run
  finishes its last step, an **advisory** completion retrospective reviews the work
  against the brief (unmet criteria, scope drift, test-quality) and appends durable
  lessons to a new root `LESSONS.md`; it reports only — it never blocks completion,
  edits code, or commits. See [docs/06-harness-spec.md](docs/06-harness-spec.md)
  §Completion Retrospective and ADR-0035.
- **Completion-retrospective lessons are offered to review.** Each lesson the
  completion retrospective records is now *also* offered to LocalMind's review-gated
  queue as a candidate, so a human can promote it to memory instead of it living only
  in the un-gated `LESSONS.md` (which stays the human-editable mirror). Advisory and
  non-blocking — a failed enqueue never breaks a finished run — and a candidate
  reaches memory only after human review. See
  [docs/localmind-integration.md](docs/localmind-integration.md) and ADR-0037.
- **Measured session-friction findings (self-review).** `localpilot self-review`
  gained a third, deterministic findings source: a captured run's capability
  scorecard `process` block is projected into the same ranked findings stream with
  no model in the loop (`--process-file <scorecard.json>`). Redundant tool calls, a
  budget-exceeded/no-progress stop, an edit before any observation, a done-claim
  with no test run, and a mid-task failure each surface as a friction finding; a
  clean run yields none. This is the auto-captured counterpart to the existing
  model-reported audit-prompt friction. See
  [docs/12-feature-specs.md](docs/12-feature-specs.md) §Self-Review.
- **Loop-outcome lesson writeback (self-improvement loop learning arc).** When a
  human accepts or rejects a patch proposal, the outcome is written back as a
  durable lesson through the existing review-gated LocalMind path (no new store):
  an accepted outcome becomes a process lesson, a rejected one a first-class
  negative-signal anti-pattern ("Avoid (rejected): …") carrying the
  change-provenance reference. Once accepted, the lesson is retrieved by
  `localpilot self-review` on the next run, so the loop stops repeating a mistake;
  a bad lesson is curated through the existing `memory delete`/review-reject
  paths. See [docs/localmind-integration.md](docs/localmind-integration.md)
  §Loop-Outcome Lesson Writeback (LocalMind decision D-LM-0014).
- **Human-gated patch generation (self-improvement loop write half).** A new
  crate turns an approved finding into a minimal change inside an isolated git
  worktree on its own branch (never the main working tree), scope-bound to the
  files the finding named, carrying a change-provenance record
  (prompt/model/tools/test-evidence/rationale/risks/rollback/lessons). The only
  operation that writes outside the worktree — promoting the change onto the main
  branch — requires an approval token a human-confirmation path mints; the agent
  never self-merges, promotion fast-forwards only and never pushes, and rollback
  is to drop the worktree. The git surface runs fixed subcommands as argv (no
  shell, no network). See [docs/12-feature-specs.md](docs/12-feature-specs.md)
  §Human-Approved Patch Generation and
  [docs/07-security-and-privacy.md](docs/07-security-and-privacy.md).
- **`localpilot self-review` (read-only repo-health scan).** A new subcommand
  walks the workspace and emits a ranked, advisory findings report — leftover
  `TODO`/`FIXME` markers, a decision index (registry) lagging the decision log,
  incomplete plan rows, broken doc links, and an opt-in missing-test heuristic —
  plus model-emitted harness-friction findings (`--audit-prompt` /
  `--friction-file`). Findings rank by severity × confidence; prior accepted
  lessons inform the scan. It writes nothing. `--json` emits the machine-readable
  report (`localpilot-selfreview-v1`). See
  [docs/12-feature-specs.md](docs/12-feature-specs.md) §Self-Review.
- **Project context files (`CLAUDE.md` / `AGENTS.md`).** LocalPilot now discovers
  project instruction files at the workspace root, in nested directories, and at
  a per-user global location (`~/.localpilot/`), resolves their `@`-import
  directives (cycle-detected and depth-bounded), and merges them by precedence
  (repo-root > nested > global) into one ordered context document. Folder
  ingestion captures the merged document as first-class derived knowledge under a
  synthetic `<project-context>` path, so `knowledge_search` can surface project
  conventions and constraints on demand. See
  [docs/configuration.md](docs/configuration.md) §Project context files.
- **Background processes.** A new `run_background` tool runs a long-running
  command — a dev server like `npm run dev` or `bun run index.ts`, or a watcher —
  detached from the turn: it confirms the process stayed up past a short grace
  period, captures its startup output, and tracks it so later turns can `list`,
  read `logs`, or `stop` it. The registry is session-scoped and in-memory; every
  child is killed when the session closes (no cross-invocation daemons).
  `run_shell` now recognizes a dev-server/watcher command and points at
  `run_background` instead of blocking until its timeout, and `bun`/`deno` are
  recognized by the command classifier. The interactive UI pins a running-process
  indicator to the bottom-right status corner, and a new `/bg` command lists them
  (`/bg`), stops one (`/bg stop <id>`), or stops all (`/bg stop all`).
- **Capability scorecard.** The golden-task evals now emit a machine-readable
  JSON scorecard per task run, widening the previous pass/fail line into three
  measured layers — `results` (pass/fail, regression-safety, partial credit),
  `quality` (diff size, vs-gold ratio, format/lint/type-check clean, complexity,
  tests-added), and `process` (tool-call count, redundant calls,
  reproduce-before-fix, test-before-done, retrieval utilization, exit reason,
  recovery) — read deterministically from the captured diff and the session event
  trace. A reported `speed` block (wall time, tokens) is a guardrail, never the
  headline. The one-line discipline scorecard is unchanged. See
  [docs/08-testing.md](docs/08-testing.md) §Golden-Task Evals.
- **Per-turn tool-call budget is now opt-in (behavior change).** The
  `[harness] tool_call_budget` / `tool_call_budget_max` keys default to **unset**,
  so a turn runs unbounded unless an operator configures a budget — previously
  both defaulted to a fixed `50`. Setting either key enables enforcement (a single
  configured bound serves as both the soft start and the hard ceiling); with the
  budget off, neither the cost ceiling nor the no-progress stop fires.
- **First-party capability corpus.** Added an original, clean-room corpus of
  small buggy tasks (each with its own failing→passing test) under
  `crates/localpilot-harness/tests/corpus/`, plus an in-repo runner that drives
  the harness loop headless against each task, emits the scorecard, and grades by
  building and running the task's own test in isolation. Includes a git-history
  extraction helper that surfaces fix-commit candidates as reviewable fixture
  stubs. Offline-deterministic by default; a live model path is gated behind
  `LOCALPILOT_LIVE_TESTS`.
- **LLM-as-judge quality rubric.** Added an original, blinded, calibrated
  LLM-as-judge that scores the quality dimensions static signals cannot see
  (readability, idiomatic style, abstraction fit, latent-bug risk) into the
  scorecard's optional `judge` block. Single-solution scoring is blind by
  construction; comparative judging randomizes solution order and maps the verdict
  back; a prompt-addressed cache makes scoring offline-deterministic; and
  `cohens_kappa` reports agreement against a human-labelled sample. See
  [docs/08-testing.md](docs/08-testing.md) §LLM-as-judge quality rubric.
- **Judge ranking self-test.** Added a cheap, per-run trust gate complementing
  calibration: the judge must score each authored `better` fixture strictly above
  its `worse` pair (`ranking_selftest_offline`, `RANKING_FIXTURES`) or scoring is
  refused (`score_offline_gated` → `JudgeError::Untrustworthy`, naming the failed
  fixture) rather than emitting a believed-but-wrong number. Runs offline with no
  model (the CI gate); `ranking_selftest_live` is the opportunistic live variant.
- **Ablation, attribution, and composite scoring.** Added an ablation arm matrix
  (`baseline`, `full`, and one arm per harness feature turned off, model pinned),
  per-feature attribution that maps each feature to the process signal it should
  move and flags a feature that is on but inert, and a composite score where
  correctness gates first and passers rank by quality + process + regression-safety
  (speed stays a reported guardrail). All deterministic and offline-testable, with
  an original clean-room set of adversarial tasks.
- **`localpilot eval` command.** A new headless subcommand runs the agent on one
  problem in the workspace and emits the capability scorecard (JSON) to stdout —
  the solver entry point an external benchmark runner drives. It runs the same
  harness a real session uses, captures the produced diff + the session trace,
  optionally grades with `--test <cmd>` (or leaves `results` for an external
  grader), and records `--arm`/`--task`/`--gold-diff` on the card. Only the JSON
  reaches stdout, so the line is pipe-safe.

## v0.3.0-beta.3 - 2026-06-18

Coordinated LocalX beta release.

- **Release hygiene.** Stamped every crate's `Cargo.toml` package version at
  `0.3.0-beta.3` and advanced the `external/localmind` submodule pin to the
  matching beta.3 LocalMind commit. The coordinated cut had moved the top-level
  `VERSION` but left the Rust packages and the embedded LocalMind a train behind.
- **RPC robustness.** The stdio line framer now caps an unterminated record
  (default 16 MiB) and returns a framing error instead of buffering without
  bound, so a peer that never sends a newline cannot exhaust memory.
- **Memory inspector accuracy.** The per-turn "memories used" record (shown by
  `localpilot memory inspect`) is now derived from the same single retrieval that
  builds the injected context, so it lists exactly what was injected — no longer
  over-reporting memories ranked past the injected cap, and now including the
  repository primer (`primer` layer) and push-mode ingested chunks (`ingest`
  layer) that were previously omitted. Each turn does one memory search instead of
  two.
- **Security (command classification).** An inline Windows shell command —
  `cmd /c …`, `powershell`/`pwsh -Command …`, `-EncodedCommand`, `-File` — is now
  treated as opaque and classified `unknown` (gated), exactly like `bash -c`,
  instead of being substring-classified. This closes a path where
  `cmd /c "echo data > file"` was auto-allowed as a read while the shell performed
  the write. Independently, an argument with an output redirection (`>`/`>>`) can
  no longer be classified `read-only`. The classifier fails toward a prompt, never
  a silent allow (ADR-0032).
- **Security (shell secret reads).** A read-only shell command (`cat`/`type`/
  `head`) whose path argument is secret-like (`.env`, `*.pem`, `~/.ssh/…`,
  `.aws/credentials`, …) or resolves outside the workspace now prompts, instead of
  being auto-allowed to read the file into model context. Ordinary in-workspace
  reads are unaffected (ADR-0032).
- The **no-unsupported-claim gate** is now reachable through configuration:
  `[harness] claim_gate = "warn"` (default `"off"`) flags a completed-action
  claim in the final reply that no verified tool call this turn supports. Matching
  is now **per claim** — a verified action no longer excuses a different,
  unverified one — and a verified shell command (opaque) backs any category while
  the structured file tools match by kind. The expanded lexicon recognizes more
  completions (added, implemented, generated, ran, pushed, merged, …) while
  present-tense and plan phrasing stay untouched. An offline false-positive/recall
  benchmark scores the gate without a live model (ADR-0023).
- Added a **pull-based tool surface** (ADR-0031), off by default. With `[tools]
  broker = true`, each turn advertises only a small working-set of tool *schemas*
  (a configurable core plus the broker's own tools plus what has been revealed)
  instead of every tool's schema; tool names are still listed cheaply. Two
  read-only tools, `tool_search` and `tool_load`, let the model find and reveal a
  tool on demand. A call to a tool that is not advertised (unknown,
  out-of-working-set, or retired) no longer returns a bare `unknown tool` error —
  the broker resolves it to the closest available tool, reveals it, and asks the
  model to retry, without running the attempted call. An opt-in `[tools] marker`
  lets the model write a `NEED: <capability>` line to request a tool proactively.
  **Reveal-never-grant:** revealing changes visibility only; a revealed
  write/network tool still passes the full permission gate. The broker searches a
  live, fingerprinted catalog of the registry (MCP tools attributed to their
  server; a retired tool drops out, with an optional old→replacement overlay since
  MCP carries no deprecation field). With `[tools] learning = true` the broker
  re-ranks tools by past success, graduates frequently-revealed tools into the
  always-advertised set (persisted across sessions), and records redacted
  `tool_resolution` telemetry. All `[tools]` defaults reproduce prior behaviour.
- Added a **look-before-launch** discipline (ADR-0030). The agent is now nudged to
  inspect a named target before standing up its own competing server. A new
  always-on system-prompt convention states it, and a deterministic
  `check_before_launch` rule enforces it: when the task prompt named a local
  serveable target (a loopback host, or any `host:port` with an explicit port) that
  has not been probed this session, an attempt to launch a local HTTP server
  (`python -m http.server`, `npx serve`, `php -S`, `vite`, …) or scaffold a
  competing `index.html` surfaces a model-visible verdict — *probe it first; only
  launch your own server if the probe fails*. The probe state is read from the
  session evidence ledger (a successful `fetch`, or a `curl`/`Invoke-WebRequest`
  probe command), never the model's claim. It is advisory and tighten-only: default
  `warn` (the call still runs), tunable via `[harness.rules] check_before_launch` to
  `block` (refuses the launch) or `off`. Auto-extracted targets ignore external
  reference URLs without a port.
- The per-turn tool-call ceiling is now **progress-aware** (ADR-0029). A turn that
  keeps making forward progress runs up to a hard cost ceiling instead of stopping
  at a single fixed count; a turn that spins on the same successful calls gets a
  strategy-change nudge and then stops on a distinct `no_progress` reason at the
  soft start, rather than wasting the rest of the budget. The hard ceiling always
  stops the loop, so a turn can never run unbounded. Two new `[harness]` keys —
  `tool_call_budget` (soft start) and `tool_call_budget_max` (hard ceiling) — both
  default to `50`, so behaviour is unchanged until an operator raises the maximum.
- Added a cross-context **handoff**: `localpilot handoff` writes a redacted,
  git-ignored snapshot (`.localpilot/handoffs/<id>.md`) of the latest session's
  durable state — a machine-checkable header plus a body separating confirmed facts
  from assumptions, referencing `brief.md`/`PROGRESS.md`/`DECISIONS.md` by path rather
  than copying them. `localpilot handoff resume <id>` runs a deterministic check
  (branch, commit, dirty-state, referenced paths/session) and surfaces mismatches as
  warnings before a fresh agent acts. A handoff is an execution record — never
  committed and never promoted into LocalMind memory.
- Project-local skills (advisory prompt modules under `.localpilot/skills/` or
  `.agents/skills/`) are now a live, pull-based surface. `localpilot skills list`
  and `localpilot skills show <name>` read them deterministically; with `[skills]
  autonomous_discovery = true` (off by default) the model can also discover them on
  demand via the read-only `skill_search` / `skill_load` tools. The loader now
  respects a skill's `disable-model-invocation` flag — a user-only skill is reached
  only by exact name, never auto-surfaced by search. Loading a skill runs nothing;
  declared permissions are surfaced, not granted, and the workspace trust gate and
  permission engine still apply.

## 2026-06-17 - Retrieval and learning

- Ingested chunks are now prefixed with offline document context (front matter
  or leading line) before indexing, so a chunk split mid-thought still matches
  its document's subject. Opt-in model-written prefixes are gated and audited.
- Added a layered retrieval contract — `knowledge_expand` and `knowledge_fetch`
  tools alongside `knowledge_search` — so a turn spends a bounded number of
  tokens to locate the right knowledge before paying for full bodies.
- Off-machine learning extraction is now gated: model-backed extraction runs
  against a loopback endpoint by default, and an off-machine endpoint is reached
  only with the `LOCALPILOT_LEARNING_ALLOW_REMOTE` opt-in (audited); otherwise
  close-out falls back to the deterministic extractor and the transcript stays
  local.
- Added a local "memories used this turn" inspector: a `memory used` CLI
  subcommand and a TUI panel showing each used memory's provenance, confidence,
  epistemic status, contradictions, and staleness. Fully offline.
- Fixed a TUI-only build break in the `ingest resume` path.

## 2026-06-17 - Documentation

- README now documents the `ingest` and `knowledge` commands and the
  `localpilot-verify` crate, which had shipped without a README entry.
- Added an in-repo wiki source (`docs/wiki/`) one-way CI-synced to the GitHub
  Wiki, a `docs/README.md` doc-ownership index, and an offline link check over
  the docs.

## 0.3.0-beta.2 - 2026-06-15

Coordinated LocalX beta release. The learning loop now closes end to end.

- The learning adapter selects the model-backed extractor when an `[inference]`
  endpoint is configured (with graceful deterministic fallback), instead of
  always running deterministic. See ADR-0019.
- New learning projects auto-wire `[inference]` to the host's own loopback
  provider endpoint, so local models do the learning jobs with no manual config;
  a remote provider is never wired automatically (remote-egress policy).
- Added a read-only `active_skills` tool: active skills are advisory prompt
  modules surfaced with provenance, never installed or executed. See ADR-0020.
- Committed an end-to-end learning-loop regression fixture (closeout → promote →
  durable memory + audit + retrieval).
- Extracted the `run_shell` builtin into its own module.
- Docs: scoped `context-intelligence-vision.md` against LocalMind's vision; added
  the extractor-selection and skill-consumption contracts to the integration doc.

## 0.3.0-beta.1 - 2026-06-12

- Fixed interactive input editing: the caret is visible, and Left/Right,
  Home/End, Backspace, Delete, newlines, and pastes edit at the cursor. Provider
  streams that disconnect before a completion marker now recover instead of
  persisting a visibly truncated response as complete.
- Made the session context budget configurable with `[harness]
  context_token_limit` (default 24000) so a model's full context window is used
  for compaction instead of a fixed default.
- Reworked the REPL input box: it grows with multi-line content up to a cap and
  then scrolls; newlines now work across terminals (a trailing `\` before Enter,
  plus Ctrl+J / Shift+Enter where the terminal reports enhanced keys); large
  pastes collapse to a `[pasted #n · N lines]` placeholder and expand to full
  text on submit.
- Added a first-run trust gate: the REPL shows the workspace folder and asks
  whether to trust it before acting, remembering the answer per folder (skipped
  under `--bypass`).
- Added the Anthropic Messages API provider (`kind = "anthropic"`), a second,
  protocol-distinct adapter implemented clean-room from the public API:
  top-level `system`, `tool_use`/`tool_result` blocks, required `max_tokens`,
  `x-api-key` + `anthropic-version`, and a typed SSE stream (ADR-0008).
- Added `localpilot update [--check]`: checks the repository for a newer release
  tag and, on confirmation, reinstalls from source with the same feature set
  (MSVC toolchain on Windows for the TUI). The REPL and bare launch also do a
  cached, once-a-day check; disable with `LOCALPILOT_NO_UPDATE_CHECK`. The
  binary now embeds a real version via `build.rs`.
- Fixed the installers to build `--features tui,learning`, initialize the
  LocalMind submodule, and prefer the MSVC toolchain on Windows for the TUI.
- Documented the configuration reference and stability policy
  (`docs/configuration.md`) and consolidated the extension points into
  `docs/extending.md`.
- Updated the vendored LocalMind engine to the coordinated LocalX
  `v0.3.0-beta.1` release train and exposed active LocalMind skills through
  the adapter.

## 0.1.0-alpha.6

- Fixed the interactive REPL: drain buffered events so a fast response is shown
  (not dropped) and surface provider/stream errors instead of failing silently;
  handle only key *press* events (Windows no longer doubles typed characters);
  add a working spinner + elapsed timer; support bracketed paste and Alt+Enter
  for a newline.
- Added a task checklist panel driven by an `update_plan` tool.
- Retry transient provider connection failures (network/5xx) with exponential
  backoff and a notice; rate-limit/quota errors still pause.

## 0.1.0-alpha.5

- Integrated the LocalMind learning engine (vendored as a git submodule) behind
  the opt-in `learning` feature: session closeout, the review queue, memory
  promotion and search, skill drafts, an audit log, retrieved-context injection
  before turns, and automatic closeout on REPL exit — one-way edge, bundled into
  the binary, all state local under `.localmind/`. New `localpilot learning`
  commands.

## 0.1.0-alpha.4

- Added interactive tool-approval prompts in the REPL (the approval interface is
  now asynchronous); default-profile sessions can perform approved actions
  without `--bypass`.
- Connected MCP servers and exposed their tools to the session through the same
  permission engine and redaction.
- Sized quota pauses from provider rate-limit metadata; show live tokens/sec and
  a quota reset timer in the footer.

## 0.1.0-alpha.3

- Added `localpilot harness wait-resume` to continue a run paused on a provider
  quota/rate limit once it is safe.

## 0.1.0-alpha.2

- Made the `chat` REPL launchable and bundled the `tui` feature into release
  builds; the bare `localpilot` command launches the REPL when a provider and
  model are configured.

## 0.1.0-alpha.1

- Created the clean-room Rust workspace and the product/architecture/harness/
  provider/security/testing/release specifications, with two operating modes
  (agent and enforced harness) and configurable permission profiles.
- Added the full crate roster (`localpilot-memory`, `-skills`, `-recovery`,
  `-quota`, and the rest) and centralized the lint policy in `[workspace.lints]`
  (`unsafe_code` forbidden; `unwrap`/`expect`/`todo`/`dbg!` denied on library
  runtime paths, relaxed in tests).
- Added real `doctor` diagnostics: version, platform, config search paths,
  provider credential presence (never values), tool availability, trust state.
- Added the provider runtime: an object-safe provider trait with typed
  capabilities, a stable error taxonomy, and quota metadata behind one streaming
  contract. The OpenAI-compatible adapter serves local servers and the official
  OpenAI API, with streaming, tool calls, reasoning round-trip, and a
  config-driven registry. Added `localpilot ask`.
- Added the sandbox: a workspace path boundary, per-OS command risk
  classification, and a permission engine with `default`/`relaxed`/`bypass`
  profiles, a secret-file guard, and a workspace-trust floor.
- Added the tool system: a permission-gated registry and the builtin tools
  (`read_file`, `write_file`, `edit_file`, `list_files`, `search_text`,
  `run_shell`, `git_status`, `git_commit`) with generated schemas, atomic writes,
  and output redaction on every profile.
- Added the shared agent-mode session runtime (cancellable streaming loop, tool
  execution, transcript persistence, context compaction, loop limits) with
  bad-output detection and a budgeted recovery ladder, plus `localpilot print`
  and the `chat` REPL behind the opt-in `tui` feature.
- Added the harness core: lossless `brief.md` / `PROGRESS.md` documents; the
  `init`, `harness status`, `intake`, `plan`, `feature`, and `resume` commands;
  original intake/planner prompts; a deterministic rule engine with protected
  critical rules; and an anti-sunk-cost worker that commits one step at a time.
- Added the v1 extensions: quota wait/resume with safety gates, a local redacted
  memory store with ranked retrieval and `memory` commands, the skill
  manifest/loading/suggestion system, and an MCP client.
- Added the terminal UI: a dense ratatui view (header, transcript with live
  streaming, always-visible footer, optional thinking panel, approval modal,
  slash commands, model/provider picker, transcript search, responsive collapse)
  snapshot-tested with a test backend.
- Updated pinned dependencies for security (`tokio` → 1.44.2,
  `tracing-subscriber` → 0.3.20); no MSRV change. Added editor/CI tooling and an
  opt-in pre-commit gate; CI runs tests under `cargo nextest` plus a
  supply-chain job (`cargo deny`, `cargo audit`).
