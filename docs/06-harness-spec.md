# Harness Specification

## Definition

The harness is a deterministic workflow layer around an LLM agent. It controls
state, rules, retries, and commits. The model proposes actions. The harness
decides whether those actions are allowed to advance the project.

## Operating Modes

The harness is the enforced operating mode. The other mode is agent mode, a plain
conversational loop with no rule engine. See the product spec for the split.

Harness mode is entered three ways:

- ground-up: `unshackled harness intake` then `unshackled harness plan` on a new
  project
- single task: wrap one bounded task in the rule engine without a full project
  plan
- adopt existing: summarize an existing repo, generate or import
  `brief.md`/`PROGRESS.md`, then `unshackled harness resume`

## Mode and Permission Flags

Mode and permission profile are selectable per launch. Flags override config;
config overrides built-in defaults.

- `--mode <agent|harness>`: operating mode. Default `agent`.
- `--permission <default|relaxed|bypass>`: permission profile. Default `default`.
- `--bypass`: shorthand for `--permission bypass`. Allow-all, no prompts. Must be
  set explicitly; the active profile is shown in the footer/status.

These flags apply to the interactive REPL, print mode, and every `unshackled
harness` subcommand. The `unshackled harness` subcommands imply `--mode harness`.

Config equivalents:

```toml
[harness]
mode = "agent"

[permissions]
profile = "default"
```

## Files

### `.unshackled.toml`

Project-local config.

```toml
[harness]
mode = "agent"
attempts_per_step = 3
auto_commit = true
test_command = "cargo test"

[harness.rules]
require_tests_before_impl = "warn"
suite_green = "block"
no_stale_uncommitted = "block"
decision_logged = "warn"
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

### `unshackled init`

Creates:

- `.unshackled.toml`
- `.gitignore` entry for `.unshackled/`

Initializes git if requested.

### `unshackled harness intake`

Inputs:

- `--idea <text>`
- `--refine`
- `--continue`
- `--auto`

Output:

- `brief.md`
- `.unshackled/intake.jsonl`

### `unshackled harness plan`

Inputs:

- `brief.md`
- repository summary
- optional `--replan`

Output:

- `PROGRESS.md`

### `unshackled harness resume`

Inputs:

- current repo
- `brief.md`
- `PROGRESS.md`

Output:

- code changes
- step commit
- progress commit
- attempt logs when needed

### `unshackled harness feature`

Adds a new feature to an existing brief and plan.

Input:

- feature description

Output:

- appended brief notes
- appended or inserted progress steps

### `unshackled harness status`

Read-only summary:

- current branch
- next step
- completed count
- dirty state
- test command
- provider config status

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

Before step completion, configured tests must pass.

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

