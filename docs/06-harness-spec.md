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

## Per-Turn Tool-Call Budget

Each turn bounds how many tool calls it runs, so an unattended loop cannot spend
without limit. The bound is progress-aware (ADR-0029), set by two numbers in
`[harness]`:

- `tool_call_budget` — the **soft start**. A turn that keeps making forward
  progress runs past this; a turn detected as making no progress stops here. An
  ordinary task stays well under it.
- `tool_call_budget_max` — the **hard cost ceiling**. The loop always stops at
  this count, regardless of progress, so a turn can never run unbounded.

"No forward progress" is judged deterministically: the same `(tool, arguments)`
call returning the same output repeatedly, or a turn cycling a tiny set of calls.
On the first such signal the runtime appends a one-shot strategy-change hint to
the tool result (mirroring the repeated-error hint), nudging the model to act on
what it has or change approach before any stop.

The two stops are distinct, recorded exit reasons: a cost-ceiling stop
(`BudgetExceeded`) and a no-progress stop (`NoProgress`). Both leave a
model-visible synthetic message and honour the tool-pairing invariant above, like
every other exit path. With `tool_call_budget_max == tool_call_budget` the bound
is a flat fixed ceiling — the default, so behaviour is unchanged until an operator
raises the maximum.

## Anti-Sunk-Cost Loop

For each step:

1. Start from committed state.
2. Try to complete the step.
3. If rules return `retry`, keep context and feed back the reason.
4. If rules return `discard`, save attempt log and restore committed state.
5. After repeated discard/retry failures, replan the step with attempt logs and
   record the replan in `DECISIONS.md`.
6. Cap replans to avoid runaway automation.

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

