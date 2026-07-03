# Harness Specification

## Definition

The harness is a deterministic workflow layer around an LLM agent. It controls
state, rules, retries, and commits. The model proposes actions. The harness
decides whether those actions are allowed to advance the project.

## Operating Modes

The harness is the enforced operating mode. The other mode is agent mode, a plain
conversational loop with no rule engine. See the product spec for the split.

Harness mode is entered three ways:

- ground-up: `localpilot harness intake` then `localpilot harness plan` on a new
  project
- single task: wrap one bounded task in the rule engine without a full project
  plan
- adopt existing: summarize an existing repo, generate or import
  `brief.md`/`PROGRESS.md`, then `localpilot harness resume`

## Mode and Permission Flags

Mode and permission profile are selectable per launch. Flags override config;
config overrides built-in defaults.

- `--mode <agent|harness>`: operating mode. Default `agent`.
- `--permission <default|relaxed|bypass>`: permission profile. Default `default`.
- `--bypass`: shorthand for `--permission bypass`. Allow-all, no prompts. Must be
  set explicitly; the active profile is shown in the footer/status.

These flags apply to the interactive REPL, print mode, and every `localpilot
harness` subcommand. The `localpilot harness` subcommands imply `--mode harness`.

Config equivalents:

```toml
[harness]
mode = "agent"

[permissions]
profile = "default"
```

## Files

### `.localpilot.toml`

Project-local config. Local-only: it is git-ignored, never committed —
external launchers generate machine-local provider config into it, and the
ratified gate below shares the file (ADR-0012).

```toml
[harness]
mode = "agent"
attempts_per_step = 3
auto_commit = true
test_command = "cargo test"   # shorthand; equivalent to a single cadence="phase" check
claim_gate = "off"            # "warn" flags a final-reply action claim no verified call backs (ADR-0023)

[harness.rules]
require_tests_before_impl = "warn"
suite_green = "block"
no_stale_uncommitted = "block"
decision_logged = "warn"
quality_gate = "block"

# Discovered, user-ratified quality gate, written by `harness gate ratify`. Each
# check runs through the permission engine like any shell command. A check is a
# program plus an argument list (no shell interpretation): `program` + `args`,
# and `fix_program` + `fix_args` for an auto-fixer. cadence: "step" | "phase".
# auto_fix: true | "safe" | false. severity maps a check's findings to a verdict.
[[harness.checks]]
name = "fmt"
program = "cargo"
args = ["fmt", "--check"]
cadence = "step"
auto_fix = true
fix_program = "cargo"
fix_args = ["fmt"]

[[harness.checks]]
name = "clippy"
program = "cargo"
args = ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]
cadence = "step"
auto_fix = "safe"
fix_program = "cargo"
fix_args = ["clippy", "--fix", "--allow-dirty", "--allow-staged"]

[[harness.checks]]
name = "test"
program = "cargo"
args = ["test", "--workspace"]
cadence = "phase"
auto_fix = false

[[harness.checks]]
name = "deps"
program = "cargo"
args = ["machete"]
cadence = "phase"
auto_fix = false

[[harness.checks]]
name = "audit"
program = "cargo"
args = ["audit"]
cadence = "phase"
auto_fix = false
severity = "block"   # advisory findings need a human/dependency decision
```

### `brief.md`

Required sections:

```markdown
# Brief: <name>

## Summary

## Requirements

## Constraints

## Non-Goals

## Acceptance Criteria
```

An optional trailing `## Risks & Rollback` section captures what could go wrong
once the work ships and how it is undone (revert, feature flag, config switch, or
migration down). It is omitted from an older or hand-written brief without error
and rendered only when present, so a brief round-trips losslessly either way.

### `PROGRESS.md`

Required shape:

```markdown
# Progress: <name>
Branch: feature/<name>

## Steps

- [ ] 1. Write failing test for parser errors
- [ ] 2. Implement parser errors
- [ ] 3. Document parser errors
```

Completed steps include metadata:

```markdown
- [x] 1. Write failing test for parser errors
  - commit: abc1234
  - attempts: 1
```

### `DECISIONS.md`

Append-only log of deviations the loop makes from `brief.md` / `PROGRESS.md`
during a run. A replan, a scope change, or any departure from a plan literal is
recorded here — never left implicit in a step — so the reason survives a context
reset and the next run reads why the plan changed.

```markdown
# Decisions: <name>

- D001 · <date> · <title>
  - decision: <what changed>
  - rationale: <why>
  - refs: <step number(s) / files>
```

Like `brief.md` and `PROGRESS.md`, this file is authoritative and user-editable
(ADR-0003). It is optional for a clean run and created on first deviation.

### `LESSONS.md`

Append-only log of durable lessons the completion retrospective captures at the
end of a run (see §Completion Retrospective). Each entry is a dated single line:

```markdown
# Lessons: <name>

- <date> · <a durable lesson worth keeping for future work>
```

Like the other runtime documents it is authoritative and user-editable (ADR-0003),
sited at the project root, and round-trips losslessly. It is created on the first
lesson and is never required for a clean run. The retrospective only appends to
it — it does not commit it; the user reviews and commits the artifact.

## Commands

### `localpilot init`

Creates:

- `.localpilot.toml`
- `.gitignore` entry for `.localpilot/`

Initializes git if requested.

### `localpilot harness intake`

Inputs:

- `--idea <text>`
- `--refine`
- `--continue`
- `--auto`

Output:

- `brief.md`
- `.localpilot/intake.jsonl`

### `localpilot harness plan`

Inputs:

- `brief.md`
- repository summary
- optional `--replan`

Output:

- `PROGRESS.md`

The planner is instructed to study the repository summary and prefer steps that
extend or reuse the existing module/type/function they name over adding parallel
code, and to produce a step list that collectively satisfies every acceptance
criterion in the brief (ADR-0035). These are contracts on the generated plan, not
a `PROGRESS.md` format change.

### `localpilot harness resume`

Inputs:

- current repo
- `brief.md`
- `PROGRESS.md`

Output:

- code changes
- step commit
- progress commit
- attempt logs when needed

### `localpilot harness feature`

Adds a new feature to an existing brief and plan.

Input:

- feature description

Output:

- appended brief notes
- appended or inserted progress steps

### `localpilot harness gate`

Inspect or ratify the discovered quality gate (no provider needed).

- `gate propose` — read-only. Detects the stack, probes which tools are on
  `PATH`, and prints the proposed checks with each command's risk class and an
  explicit warning for a destructive/privileged/network command. Writes nothing.
- `gate ratify` — writes the proposed checks into `.localpilot.toml` as
  `[[harness.checks]]`, adding only checks not already ratified and preserving
  the rest of the config. Ratification is the trust boundary: a discovered check
  does not run until the user has explicitly written it here. The file is
  local-only (ADR-0012); a fresh clone re-establishes its gate with
  `gate propose` / `gate ratify`. A re-probe proposes additions; it never
  auto-adopts them.

### `localpilot harness status`

Read-only summary:

- current branch
- next step
- completed count
- dirty state
- test command
- ratified quality gate
- provider config status

### `localpilot handoff`

Write a cross-context handoff for the most recent session in the workspace.
`localpilot handoff` derives the objective from the harness documents;
`localpilot handoff write "<objective>"` sets it explicitly. The artifact is
written redacted to `.localpilot/handoffs/<id>.md` (git-ignored) and the command
prints the id and the `handoff resume` line to continue with.

### `localpilot handoff resume <id>`

Run the deterministic resume check for a handoff against the current repo and print
the result, before a fresh agent acts on it. See §Handoff.

## Handoff

A **handoff** is the cross-context bridge between one session ending and another
picking the work up. It is an *execution record*, not memory.

- **Format.** A machine-checkable header (schema, id, repo, branch, commit, dirty,
  session, references, suggested skills, confidence, created) followed by a
  human-readable Markdown body that separates **confirmed facts** (what the event log
  and git record) from **assumptions** (the inferred objective and next action). The
  resume check reads the header fields, not the prose.
- **Sources.** The writer reads the session event log (committed steps) and the
  harness documents (`brief.md` / `PROGRESS.md` / `DECISIONS.md`) — never the raw
  transcript — and **references** those documents by path rather than duplicating
  them. The whole artifact is redacted through the canonical host redactor before it
  is written.
- **Location.** `.localpilot/handoffs/<id>.md`: git-ignored, never committed, distinct
  from the root-level `brief.md` / `PROGRESS.md` runtime files. **Never** promoted to
  LocalMind accepted memory — close-out reads the transcript, not the handoff.
- **Resume check.** Deterministic and warning-not-failure: it verifies branch
  identity, that the recorded commit exists, dirty-state match, that referenced paths
  and the referenced session are present, and surfaces any mismatch as a *flag to
  re-verify* rather than a hard stop. No model judges the prose.

See ADR-0028 for the decision.

## Rule Engine

### Trigger Types

- `session_start`
- `pre_tool`
- `post_tool`
- `pre_edit`
- `post_edit`
- `pre_shell`
- `post_shell`
- `pre_commit`
- `post_test`
- `step_complete`

### Verdicts

- `allow`: continue
- `warn`: continue and surface message
- `retry`: send failure reason to model and retry same step
- `discard`: reset working tree for this step and restart with fresh context
- `block`: stop and ask user

### Baseline Rules

> **Runtime status.** Each rule below has verdict logic and unit tests, but a
> rule only fires in a live run when the runtime populates its trigger facts.
> These are **runtime-active** (facts populated on the real path):
> `no_stale_uncommitted`, `suite_green`, `quality_gate`, `commit_message_clean`,
> `check_before_launch`, `attempt_limit` (the effective step cap is the
> `StepLoop`; the rule receives `attempts = 1`). These are **declared but not
> evaluated on the live path** because the runtime does not yet populate their
> facts — configuring them (e.g. `rules.secret_file_guard = "block"`) is
> currently a no-op, so do not rely on them for enforcement:
> `workspace_boundary` and `secret_file_guard` (real workspace containment and
> secret-read protection are enforced by the **permission engine**, not these
> rules — see `docs/07`), `test_first_when_configured` (no PreEdit evaluation is
> emitted), `progress_updated` (`progress_reflects_completion` is currently
> constant). `decision_logged` is not implemented as a rule — a deviation
> auto-appends to `DECISIONS.md` on replan, but nothing gates on it. Phase-cadence
> `quality_gate` checks require a `phase_complete` trigger the live loop does not
> emit outside tests. This list is the source of truth; treat a rule's prose
> below as its *intent*, gated by this status.

#### `no_stale_uncommitted`

At session start, block if unrelated uncommitted files exist.

Rationale: the harness must not mix user changes with agent changes.

#### `workspace_boundary`

Before file tools, deny writes outside workspace unless explicitly approved.

#### `secret_file_guard`

Before reads and edits, ask before touching secret-like files:

- `.env`
- private keys
- credential stores
- cloud config with tokens

#### `test_first_when_configured`

If a step is implementation-heavy and config requires test-first behavior, warn
or block when implementation files are edited before tests.

#### `suite_green`

Before step completion, configured tests must pass. `suite_green` is the
`test` check of the quality gate; it remains named for back-compat with a bare
`test_command`.

#### `quality_gate`

At each check's cadence (`step` at `step_complete`, `phase` at a phase
boundary), run the ratified `[[harness.checks]]` and act on findings per the
Quality Gate section. Generalizes `suite_green` from one test command to the
full discovered gate. Per-check `severity` overrides the rule's default verdict.

#### `progress_updated`

Before final commit, `PROGRESS.md` must reflect completed state.

#### `decision_logged`

Before a replan, or before completing a step that departed from a plan literal,
require a matching `DECISIONS.md` entry. Keeps the reason for a deviation durable
across context resets instead of vanishing into a step. Configurable
`warn`/`block`.

#### `commit_message_clean`

Commit messages must not include secrets, vendor-internal references, or private
implementation names.

#### `attempt_limit`

After `attempts_per_step` failures, stop or replan depending on config.

#### `check_before_launch`

Before a shell command (`pre_shell`) or file tool (`pre_tool`), if the task prompt
named a local serveable target (a loopback host, or any `host:port` with an
explicit port) that has **not** been probed this session, and the call launches a
local HTTP server or scaffolds a competing entry file (an `index.html`-family
page), surface a verdict steering the model to probe the target first and only
launch its own server if the probe fails.

The probe state is read from the session evidence ledger — a successful `fetch`,
or a probe shell command (`curl`, `wget`, `Invoke-WebRequest`/`iwr`,
`Test-NetConnection`) whose arguments hit the target — never from the model's
claim, exactly like `RequiresPriorRead`. Named targets are auto-extracted from the
prompt; an external reference URL without a port is not a serveable target and is
ignored.

Advisory and best-effort: non-critical, default `Warn` (the call still runs and
the nudge reaches the model), tunable to `block` (refuses the launch before it
runs) or `off`. It is tighten-only — it never grants a side effect the permission
engine would deny — and the launch/probe pattern set is curated and extensible, so
an unrecognised launcher is a documented miss, not a guarantee. See ADR-0030.

## Quality Gate

The quality gate is the discovered, language-specific set of inspection checks
the harness runs and acts on as code is written (ADR-0009). It generalizes the
single `test_command` into an ordered set of `[[harness.checks]]`.

### Toolchain profiles

Built-in profiles per stack (e.g. Rust, Node, Python, PowerShell, Go) declare
the default checks, how to interpret each check's output into findings, and which
findings are safely auto-fixable. Profiles are original code in this repository;
they are the fixed abstraction. The specific commands, versions, and paths are
*discovered*, not hardcoded into the engine.

### Discovery and ratification

During intake/plan setup, the harness detects the project's stack, selects the
matching profile(s), and probes which tools are actually available. It then
*proposes* a gate. Discovered commands are untrusted: nothing runs until the
user ratifies the gate into the project's local `.localpilot.toml`
(local-only, ADR-0012). After ratification each
check runs through the permission engine and sandbox like any other shell
command (see [`docs/05`](05-tool-system.md), [`docs/07`](07-security-and-privacy.md)).
A re-probe proposes additions when the toolchain changes; additions are
ratified, never auto-adopted.

### Cadence

Each check declares a cadence. `step` checks (format, lint on changed files) run
at `step_complete` for fast feedback. `phase` checks (whole-suite test,
dependency hygiene, advisory audit, deep static analysis) run at phase
boundaries to avoid paying full-suite cost on every step.

### Acting on findings

Findings map to verdicts:

- A check with `auto_fix = true` (deterministic formatter) applies its
  `fix_command` and re-runs. `auto_fix = "safe"` applies only the tool's own
  safe-fix mode (e.g. a linter's `--fix`); anything left is a finding.
- Remaining lint/test failures return `retry`: the failure is fed back to the
  model, bounded by `attempt_limit`, then `replan` (recorded in `DECISIONS.md`).
- Dependency and advisory findings (`audit`, license/ban) return `block`: they
  need a human or dependency decision, not a code edit.
- The harness never blind-edits logic to satisfy a check; it fixes via declared
  fixers or feeds the failure back through the loop.

Auto-fix edits are ordinary project-write side effects and are subject to the
permission profile and commit policy like any other change.

## Reliability Contract — Session-Loop Invariants

These invariants are the loop half of the reliability contract (ADR-0010):
what the session runtime guarantees on *every* exit path — success, rejected
tool batch, tool-budget exhaustion, cancellation, stream error. They are what
makes unattended multi-step execution trustworthy rather than aspirational.
Each is pinned by a named test; breaking the test is a contract change and
needs an ADR.

1. **Tool pairing.** After any turn, every `tool_use` block in the persisted
   history has exactly one matching `tool_result`, in call order. A call that
   is rejected or never executed receives a synthesized error result; a call
   that can never be answered (blank id) never enters history. Providers
   reject unpaired histories, so this is what keeps an unattended run from
   poisoning its own next request. Enforced by the
   `localpilot-harness` `pairing` test suite
   (`cargo test -p localpilot-harness --test pairing`), including a property
   run over arbitrary turn interleavings.
2. **No partial replies persist.** A turn that ends in cancellation or a
   stream error persists no partial assistant message; the transcript stays
   consistent and resumable. Enforced by
   `cancellation_leaves_a_consistent_transcript` and
   `incomplete_stream_is_retried_and_never_persisted_as_a_finished_reply`
   (`localpilot-harness`).
3. **Transcript fidelity.** The persisted transcript equals the model-visible
   history: any message that shapes the conversation is persisted (or
   explicitly marked synthetic). Synthesized tool results and corrective user
   messages are persisted today; full fidelity (including repair prompts)
   lands with the durable session store and is pinned by its
   transcript-equivalence test when it does.

The permission half of the contract lives in
[`docs/07`](07-security-and-privacy.md) §Reliability Contract.

## Bad-Output Recovery

A turn can end badly without a provider error: degenerate text (a punctuation
flood or a repeated-token loop), an empty turn, or a tool call whose streamed
arguments do not parse. The runtime detects these and runs a bounded recovery
ladder rather than persisting a corrupted turn or stopping outright. Within a
small repair budget it re-prompts; once the budget is spent the model/provider is
marked degraded and the turn stops (a degraded turn may not complete a harness
step). Each recovery is recorded as a diagnostic in the session event log.

Two recovery levers act on the *content* of the next attempt, not just the
retry:

- **Input shrink.** On a repeated bad turn the runtime compacts active history
  (which also truncates oversized tool results) before re-prompting, so the retry
  sees a smaller context.
- **Chunked write.** When the bad turn was a **file-write tool call whose
  arguments failed to parse** — the failure a local model hits on a single
  oversized write — the provider reports which tool failed
  (`MalformedToolArguments`), and the repair prompt steers the model to write the
  file in pieces: the first section with `write_file`, each remaining section with
  [`append_file`](05-tool-system.md). This recovers the write instead of replaying
  the same oversized call until the budget is spent. See ADR-0038.

A third lever acts on a tool call whose arguments are *well-formed JSON but do not
match the tool's schema* — a distinct failure from the wire-malformed cases above
(it parses, so it is not a bad-output turn):

- **Schema-aware argument correction.** At the pre-dispatch seam the runtime
  validates the call's arguments; a shape-invalid call is answered with a concise,
  schema-aware error (the `RepairToolArguments` rung, sibling of the chunked-write
  rung) instead of the raw deserializer string, so the model self-corrects on the
  next turn. Unlike the bad-output ladder this rung is **non-degrading** — a
  recoverable argument mistake is bounded by the per-turn tool-call budget, not the
  degrade counter. When `[tools] repair` is enabled the runtime first repairs the
  validator-reported fields and runs the repaired call (with a model-visible note);
  see [`05-tool-system.md`](05-tool-system.md) §Input Validation and ADR-0051.

Pinned by the `localpilot-harness` session tests (a malformed large write
recovers by writing in pieces; a repeated bad turn compacts history; a
shape-invalid call gets a schema-aware error and the model recovers on the next
call without a repair engine).

## Built-In Safety Rails

A turn must never run unbounded out of the box. When `[harness]` leaves the
budget and `turn_timeout_secs` unset, a **conservative built-in bound** still
applies so a fresh `localpilot init` project self-bounds and finalizes with a
scorecard instead of running to an external SIGKILL (ADR-0055, refining
ADR-0029/0052). An explicit `[harness]` value always wins; the built-in default
only fills an unset rail.

The default is profile-aware, because the safety need is strongest where no human
is watching:

- **Headless** (`eval`, `print`, a `harness` step): a tool-call ceiling of 200
  **and** a 600 s wall-clock bound — a non-interactive run self-bounds on both
  axes.
- **Interactive** (the REPL, `serve`): a higher tool-call ceiling of 500 and
  **no** default wall-clock — a long interactive turn is legitimate and the user
  can cancel it; the ceiling still stops an unattended runaway.

This is a safety default, not a feature lever: unlike the verify gate (opt-in) or
the broker (opt-in), an unbounded loop is a defect, so the rails ship on with a
conservative bound. Rollback/tuning is config — raise or set the explicit
`tool_call_budget`/`turn_timeout_secs`.

## Per-Turn Tool-Call Budget

With neither budget key set in `[harness]`, a turn carries the built-in headless
or interactive ceiling above rather than running with no cost bound. Setting
either key replaces that default with an explicit, progress-aware budget
(ADR-0029), set by two numbers in `[harness]`:

- `tool_call_budget` — the **soft start**. A turn that keeps making forward
  progress runs past this; a turn detected as making no progress stops here. An
  ordinary task stays well under it.
- `tool_call_budget_max` — the **hard cost ceiling**. The loop always stops at
  this count, regardless of progress, so a turn can never run unbounded.

Setting either key enables the budget; a single configured bound serves as both
the soft start and the hard ceiling.

"No forward progress" is judged deterministically: the same `(tool, arguments)`
call returning the same output repeatedly, or a turn cycling a tiny set of calls.
On the first such signal the runtime appends a one-shot strategy-change hint to
the tool result (mirroring the repeated-error hint), nudging the model to act on
what it has or change approach before any stop.

The two stops are distinct, recorded exit reasons: a cost-ceiling stop
(`BudgetExceeded`) and a no-progress stop (`NoProgress`). Both leave a
model-visible synthetic message and honour the tool-pairing invariant above, like
every other exit path. With `tool_call_budget_max == tool_call_budget` the bound
is a flat fixed ceiling; raise the maximum above the soft start to let a
productive turn extend.

### Always-On Degenerate-Loop Guard

Independent of the opt-in budget, the loop carries an always-on guard so a turn
can never spin unbounded even with the budget off (ADR-0052). When the budget is
disabled, the turn still stops with `NoProgress` if either the no-progress
detector trips (a repeated or cyclic *successful* call set) or a run of
consecutive *failing* calls — the denied/failing spin the detector never sees,
since it is fed only by successful calls — exceeds a fixed conservative limit. The
failure streak resets on any successful call, so a productive turn is never cut;
when the budget is configured the controller above owns the no-progress stop and
this guard is inert. It is a safety backstop, not a cost control — "budget off"
still means no *cost* ceiling. Pinned by the `localpilot-harness` budget tests
(a spinning loop and a run of failing calls both halt with the budget off; a long
productive turn is not).

## Verify-Before-Done Gate

A solve loop ends when the model stops calling tools — it "submits" by replying
without a tool call. That lets a turn finalize code it never built: the largest
single cause of avoidable losses on compiled languages is a turn that declares
success on a workspace that does not compile. The verify-before-done gate closes
that gap (ADR-0054).

When `[harness] verify_before_done` is on, a turn that would finalize with no
tool call first runs a **verification command** — "does this workspace build /
do its tests pass?". On a failure the captured diagnostics are fed back as the
next turn's input and the loop continues, so the model fixes the problem instead
of stopping; on a pass (or no detectable target, or the gate off) the turn
finalizes as before.

- **Command resolution.** `[harness] verify_command` (a single command line,
  split on whitespace — no shell) wins; otherwise the command is detected from
  the workspace's marker files (`Cargo.toml` → `cargo test`, `go.mod` →
  `go test ./...`, `pom.xml` → `mvn test`, `build.gradle` → `gradle test`,
  `package.json` → `npm test`, a Python project → `python -m pytest`, a
  `Makefile` → `make`, otherwise C++ sources at the root → an artifact-free
  `g++ -std=c++17 -I. -fsyntax-only <sources>` compile check). A workspace with
  no detectable target and no override is a clean no-op — the turn finalizes
  unchanged, and a warning is emitted so the un-verified finalize is visible
  rather than mistaken for a pass.
- **Reuses the quality-gate runner.** The command runs through the same
  permission-gated [`CheckRunner`](05-tool-system.md) the step-cadence quality
  gate and `harness resume` use — there is no second command engine and no second
  retry loop. A denied or unstartable command does not wedge a finished turn: it
  is recorded and the turn finalizes without a verify signal.
- **Bounded.** The gate can never loop forever: it is bound by the per-turn
  tool-call budget and `turn_timeout` rails *and* a fixed re-entry cap. After the
  cap is reached the turn finalizes with the failing state recorded, rather than
  spinning.
- **In-workspace, de-verbatim working directory.** This gate's build/test
  command — like every child process the harness spawns (the shell and git
  tools, background processes) — runs with the workspace as its working
  directory in a form a launched shell can actually use. The sandbox
  canonicalizes the workspace root to a verbatim extended-length path
  (`\\?\…` on Windows) for containment, but that form is unusable as a process
  cwd: a shell handed a verbatim cwd falls back to a system directory, so a
  relative build/test command would run *outside* the workspace and fail. Spawns
  therefore use the de-verbatim equivalent, while the containment boundary keeps
  the verbatim root unchanged. See [security & privacy](07-security-and-privacy.md).
- **Default: off interactively, on for `eval`.** As a config lever
  (`[harness] verify_before_done`) it ships **off**, so an interactive or `print`
  turn is unchanged. For `localpilot eval` it is **on by default**: a benchmark
  must measure compiled+tested solves, not code that was never built. Opt out
  with `localpilot eval --no-verify` (byte-identical to the pre-default
  behaviour); `--verify-command <cmd>` overrides the detected command. The legacy
  `--verify` flag is accepted but redundant. The per-call `localpilot-verify`
  contract verifier is a separate mechanism.

## Anti-Sunk-Cost Loop

For each step:

1. Start from committed state.
2. Try to complete the step.
3. If rules return `retry`, keep context and feed back the reason.
4. If rules return `discard`, save attempt log and restore committed state.
5. After repeated discard/retry failures, replan the step with attempt logs and
   record the replan in `DECISIONS.md`.
6. Cap replans to avoid runaway automation.

## Completion Retrospective

When a resume run reaches a plan with no incomplete step left, the harness runs
one bounded, **advisory** review over the brief and the completed plan (ADR-0035).
It surfaces which acceptance criteria are still unmet, scope drift from the brief,
and tests that pin implementation detail instead of observable behaviour, and it
appends any durable lessons to `LESSONS.md`.

It is advisory by construction: it reports findings and records lessons — it never
blocks completion, edits shipped code, or commits. It runs once, after the final
step is already committed, and it is best-effort: a provider or quota error at that
point is swallowed so a finished run is never broken, and a reply in the wrong
shape degrades to "no findings" rather than an error. The worker prompt also
carries a doc-currency cue, so a step that changes observable behaviour,
configuration, or interfaces updates the matching documentation in the same step.

## Completion Teardown Sweep

At the same completion seam, when `[harness] teardown_sweep` is enabled, the
harness also runs an advisory **whole-repo teardown sweep** — the in-harness mirror
of the plan template's `cleanup-audit` §7 gate (ADR-0047). Where the retrospective
looks back at the brief, the sweep looks across the whole tree for cruft to clean
up before work is called done: dead/abandoned code, duplicate/parallel logic,
over-engineering, redundant data access, and doc/test drift. It is the same
read-only scanner as `localpilot self-review`, with its cleanup detectors turned
on, and its findings rank into the same advisory report (each carries a category,
confidence, a `risk`, a recommended action, and the hidden-usage channels the
detector ruled out).

It is **off by default** (features ship off) and advisory by construction: the
sweep is deterministic and offline (no model call), read-only, and human-gated. It
never blocks completion, edits code, or commits — a finished run's outputs are
untouched whether it is on or off. Categories tooling already owns (unused deps,
unused imports/vars, advisories) are surfaced as pointers to `cargo
machete`/`clippy`/`cargo deny`, not re-derived. Findings are report-only and open a
*new* plan when acted on; nothing is auto-applied or auto-enqueued as accepted
memory. The same pass is available on demand as `self-review --cleanup`.

## Background Processes

A long-running command (a dev server or watcher) does not exit, so `run_shell`
would only block until its timeout. The `run_background` tool
([`docs/05`](05-tool-system.md)) instead starts such a command detached from the
turn, confirms it stayed up past a short grace period, captures its startup
output, and tracks it so later turns can list it, read its logs, or stop it.

The registry of running processes is **session-scoped and in-memory**: it lives
on the session runtime, every child is started with `kill_on_drop`, and the
session terminates all of them when it closes or a new session starts. No
background process survives the session, so there are no orphaned daemons across
invocations. `run_shell` recognizes a dev-server/watcher command and points the
model at `run_background` rather than hanging.

## Commit Policy

Default:

- one commit for setup files
- one commit per completed step
- one commit for progress update if separate from step work

Commit messages:

```text
harness: <step description>
```

User can disable auto-commit, but the harness must then report reduced
recoverability.

