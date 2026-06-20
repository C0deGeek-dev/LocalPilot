# Product Specification

## Product Definition

LocalPilot is a terminal-based coding-agent harness. It helps a developer turn
an idea into an explicit brief, turn the brief into a stepwise plan, and execute
that plan through an LLM plus local tools under rules that preserve reviewability.

The product is not a general chatbot. It is an engineering workflow controller.

## Target Users

- individual developers building software locally
- maintainers who want repeatable agent workflows
- power users who run local models
- teams that want auditable agent sessions before adopting hosted automation

## Maintainers

LocalPilot is developed and maintained by C0deGeek.dev (David, Bram). The
canonical repository is <https://github.com/C0deGeek-dev/LocalPilot>.

## Supported Platforms

Windows, Linux, and macOS are all first-class, tier-1 targets. No platform is a
second-class port:

- behavior parity across the three platforms is a release requirement
- shell and filesystem policy is defined explicitly for both Windows and POSIX
  (see the security spec)
- CI builds and tests on Windows, Ubuntu, and macOS for every change
- installers ship for all three platforms

## Non-Goals

- no private consumer-product endpoint automation
- no vendor-specific clone behavior
- no hidden telemetry
- no cloud sync in v1
- no remote code execution service in v1
- no browser IDE replacement in v1
- no model training or fine-tuning in v1

## Core Jobs

### Job 1: Convert an Idea into a Brief

Input:

- a short idea from the user
- optional project files
- optional constraints

Output:

- `brief.md`
- structured requirements
- constraints
- non-goals
- acceptance criteria

The brief must be understandable without the transcript.

### Job 2: Convert a Brief into a Plan

Input:

- `brief.md`
- repository summary
- optional user-edited constraints

Output:

- `PROGRESS.md`
- numbered steps
- completion state
- branch name
- test strategy

The plan must be editable by the user. The next run treats the edited file as
the source of truth. The planner prefers steps that extend or reuse the existing
code named in the repository summary over adding parallel code, and the step list
must collectively satisfy every acceptance criterion in the brief (ADR-0035).

When a run completes its last step, an advisory completion retrospective reviews
the work against the brief — unmet acceptance criteria, scope drift, and
test-quality — and records durable lessons to `LESSONS.md`. It reports only; it
never blocks completion or edits code.

### Job 3: Execute One Step at a Time

Input:

- next incomplete plan step
- current repository state
- configured tools
- configured provider

Output:

- code changes
- test output
- one commit per completed step
- updated `PROGRESS.md`

The agent must not mark a step complete until the rule engine allows it.

### Job 4: Recover from Failed Attempts

When a step repeatedly fails:

- the attempt is logged
- the current working changes are discarded only inside the target workspace
- the model context is reset
- the planner reconceives the failed step with the attempt log
- a capped retry counter prevents infinite loops

This is the anti-sunk-cost behavior: do not let the same failing context keep
digging.

### Job 5: Recover from Bad Model Status

When a provider or local model enters a visibly bad state, LocalPilot should
detect it and recover without corrupting the session.

Examples:

- empty responses
- repeated-token loops
- slash floods such as `/////////`
- malformed tool calls
- malformed structured output
- repeated provider-side transient errors

Recovery should be conservative: stop the bad stream, save a diagnostic event,
retry with reduced risk, and surface the degraded state when automatic recovery
is exhausted.

### Job 6: Preserve Useful Local Context

LocalPilot should help the user retain useful project knowledge locally:

- project facts
- recurring workflows
- durable decisions
- generated skills
- frequent errors and fixes

Memory is local-only. Project memory may be enabled by default with visible
controls. Global/personal memory requires explicit consent.

### Job 7: Continue After Provider Quota Resets

Some hosted providers expose session, message, token, or time-window limits.
LocalPilot should understand quota reset windows and optionally resume a paused
harness run when the provider becomes usable again.

This must be configurable per run and globally. Global unattended resume is
allowed only when the user explicitly enables it.

## Operating Modes

LocalPilot has two operating modes. The operating mode decides how much control
the harness exerts. It is independent of the interface (REPL, CLI, print). Mode
and permission profile are selectable per launch via flags (`--mode`,
`--permission`/`--bypass`) or config; see the harness spec.

### Agent Mode (default)

A conversational coding agent. The model drives the loop, calls tools, and edits
the workspace directly. There is no enforced rule engine, no forced per-step
commits, and no required plan file. This is the familiar default for exploratory
work and the closest analog to a general coding assistant.

Tools still pass through the permission engine. The permission policy is
configurable per project and globally:

- `default`: prompts on for risky actions (writes, shell, network, secret-like
  reads). Least privilege.
- `relaxed`: a user-defined allowlist auto-approves common safe actions; the rest
  still prompt.
- `bypass`: allow-all launch mode, no prompts, like running fully localpilot.
  Explicit opt-in, surfaced in the footer.

The default is least privilege. Bypass is never the default and must be set by
the user.

### Harness Mode (enforced)

The deterministic workflow. The model proposes actions; the rule engine decides
whether they advance the project. Per-step commits, the anti-sunk-cost replan
loop, test gates, and `brief.md`/`PROGRESS.md` as source of truth all apply.

Harness mode is entered three ways:

- ground-up: greenfield project, full intake -> plan -> build
- single task: wrap one bounded task in the rule engine without a full project
  plan
- adopt existing: summarize an existing repo, generate or import
  `brief.md`/`PROGRESS.md`, then resume under the rules

Switching between modes is allowed at safe boundaries. Harness mode reuses the
same permission engine; rule verdicts layer on top of permission decisions.

## Interfaces

### Interactive REPL

An always-on terminal session with:

- message history
- tool approvals
- slash commands
- progress display
- model switching
- always-visible footer stats
- optional thinking/reasoning side panel

Interactive slash commands are REPL-scoped. Mode and permission switches
(`/agent`, `/harness`, `/default`, `/relaxed`, `/bypass`), the reasoning panel
(`/think`), reasoning effort (`/effort <level>`), and session controls (`/new`,
`/fork`, `/clone`, `/tree`, `/sessions`, `/session <id>`) act on the live
session. The rest:

- `/clear` clears the visible conversation and runtime message history while
  preserving the session id, workspace, trust decision, provider/model, mode,
  and permission profile.
- `/compact` manually applies the same context compaction rules used before
  provider requests, then reports whether history was compacted and shows the
  resulting context usage; `/compact force` compacts even when within budget.
- `/continue` (alias `/resume`) reopens the previous session in this workspace.
  The harness workflows are separate: `/harness-resume` resumes harness plan
  work, and `/wait-resume` waits out a provider quota window and then resumes.
- `/ingest <action>` manages project-local folder ingestion (`run`, `refresh`,
  `resume`, `preview`, `status`, `review`, and so on). The walking actions
  (`run`, `refresh`, `resume`) show a live progress loader — discovering,
  parsing, indexing, writing — and can be interrupted with Ctrl-C, which pauses
  the job so a later `/ingest resume` continues from the chunks already written.
- `/knowledge <query>` searches ingested knowledge; `/context <task>` builds a
  task-specific context bundle from it.
- `/bg` lists this session's background processes (`/bg stop <id>` / `/bg stop
  all`).

### Harness CLI

Scriptable commands:

- `localpilot init`
- `localpilot harness intake`
- `localpilot harness plan`
- `localpilot harness resume`
- `localpilot harness status`
- `localpilot harness feature`
- `localpilot harness wait-resume`

### Print Mode

Single prompt in, answer out:

- no workspace mutation unless explicitly enabled
- useful for shell pipelines

### Continuous Development Mode

Optional mode for long-running harness work:

- pauses cleanly on provider quota/rate limits
- records the reset timer
- resumes automatically when allowed by policy
- never bypasses permission policy
- never continues after destructive pending approvals without user consent

## User-Facing Files

### `.localpilot.toml`

Project-local config.

### `brief.md`

Problem statement and contract.

### `PROGRESS.md`

Plan and execution state.

### `.localpilot/`

Ignored runtime state:

- transcripts
- attempt logs
- cache
- provider metadata
- tool-output snapshots
- local memory store/index
- generated skill drafts
- quota wait/resume records

## Scope

### First Milestone

The first runnable milestone is intentionally small and auditable:

- config loading
- one official hosted provider
- one local provider
- text-only model calls
- file read/write/edit tools
- shell command tool with approval
- agent mode loop with the permission engine
- brief generation
- plan generation
- progress parsing
- status display
- deterministic rule engine
- tests for all parsers and rule decisions

### v1 Committed Scope

v1 is not limited to the first milestone. The following are committed v1
capabilities, not deferred ideas:

- both operating modes (agent and harness)
- configurable permission profiles, including the bypass launch mode
- MCP client (servers, tools, resources)
- local memory store with inspect/delete controls
- skills, including auto-suggested skill drafts
- recovery engine for bad-output states
- quota wait/resume and continuous development mode

### Later (Separate Tracks)

Real goals, sequenced after v1. These are larger surfaces, not core agent
capabilities:

- remote agents
- web UI surface
- plugin/skill marketplace
- multi-repo orchestration
- image input
- IDE integration

### Out of Scope

- voice
- hidden telemetry
- vendor-specific clone behavior
- private or undocumented endpoint adapters
- model training or fine-tuning
