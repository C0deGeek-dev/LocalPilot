# Changelog

Notable changes per release. This project is pre-1.0; the configuration schema
stability policy is in [docs/configuration.md](docs/configuration.md).

## Unreleased

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
