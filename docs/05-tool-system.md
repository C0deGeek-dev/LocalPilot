# Tool System

## Purpose

Tools are the only path from model output to local side effects. Every tool call
must pass through schema validation, permission policy, execution, and result
normalization.

## Tool Trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> serde_json::Value;
    fn effects(&self, invocation: &ToolInvocation) -> ToolEffects;
    async fn invoke(&self, invocation: ToolInvocation) -> anyhow::Result<ToolOutput>;
}
```

Builtin tools normally return static string literals. Dynamically discovered
tools, such as MCP tools, may return borrowed metadata from owned registry
entries; dynamic metadata must not be forced into a static lifetime.

## Builtin Tools

### `read_file`

Reads UTF-8 text from a workspace path.

Rules:

- deny paths outside workspace unless approved
- deny secret-like files by default
- support line ranges
- cap output size

### `write_file`

Writes a new file or replaces an existing file.

Rules:

- require approval for overwrite until trust is established
- create parent directories only inside workspace
- preserve newline style where possible
- refuse a single payload larger than the soft write-size limit (64 KiB),
  steering the model to split the work into smaller modular files or to build
  the file up with `append_file` — an oversized write risks being truncated in
  transit, so this prevents the malformed-call failure before it happens rather
  than only recovering from it (see [06-harness-spec.md](06-harness-spec.md)
  Bad-Output Recovery)

### `edit_file`

Applies structured edits.

Rules:

- reject ambiguous edits
- require exact old text or AST-aware operation
- show diff before approval when interactive

**Edit matching contract** (shared by `edit_file`, `multi_edit`, and
`apply_patch` — one matcher, not three copies). Matching is **anchored, never
fuzzy** — a wrong-location edit is far worse than a failed one:

1. **Line-ending-insensitive.** Matching runs on the LF-normalized file; the
   file's original CRLF/LF style is restored on write. An `old_text` written
   with `\n` lands on a CRLF file.
2. **Exact, unique match first.** A single exact substring match is replaced. A
   match that occurs more than once is rejected as ambiguous (with the count) —
   never a best-guess pick.
3. **One leading-indentation-tolerant rung.** If the exact match misses, the
   `old_text` lines are matched against the file ignoring leading indentation,
   but only when they form a *unique* contiguous block whose indentation differs
   by **one consistent whitespace prefix** across the whole block. The
   replacement is re-indented to the file's own indentation. A non-unique or
   inconsistent-indent candidate applies nothing.
4. **Guiding errors.** A failed edit returns the match count (ambiguous) or the
   nearest existing line + a re-read/stale hint (not found), instead of a bare
   "old_text was not found", so the model can correct rather than give up and
   rewrite the whole file.
5. **No-op guard.** An empty `old_text`, or an `old_text` identical to
   `new_text`, is rejected up front.
6. **Atomicity preserved.** `multi_edit` applies every edit in memory then does
   one atomic write; `apply_patch` validates every hunk before any write — a
   miss on any one aborts the batch with nothing changed.

### `append_file`

Appends content to the end of a file, creating it if absent. Lets a large file
be written in pieces — the first section with `write_file`, each remaining
section appended — which the bad-output recovery path steers a model toward when
a single oversized write fails to emit as one well-formed tool call (see
[`06-harness-spec.md`](06-harness-spec.md), "Bad-output recovery").

Rules:

- gated as a workspace write, identical containment and atomic write to
  `write_file`
- preserve the file's newline style; default to LF for a new file
- refuse a non-UTF-8 (binary) file rather than clobbering it
- non-idempotent by contract: re-running appends again

### `list_files`

Lists files under a workspace path.

Rules:

- respect ignore files
- cap result count
- include hidden files only when requested

### `search_text`

Searches text using ripgrep when available.

Rules:

- respect ignore files by default
- cap matches
- never traverse outside workspace without approval

### `multi_edit`

Applies several exact-text replacements to one file atomically; rejects
missing or ambiguous context before writing anything.

### `replace_in_file`

Whole-file find/replace on a workspace file: replace an exact block of text
(which may span multiple lines) with another. The default editing tool — the
model should reach for it instead of rewriting a file with `write_file`. Runs
through the platform stream editor (PowerShell on Windows, `perl` on Unix); the
implementation keeps the dangerous parts native.

Rules:

- the editor is a pure stdin→stdout transform over the whole file; path
  containment and the atomic write are native Rust, identical to `write_file`
- `find`/`replace` are passed via the environment, never interpolated into a
  shell string — model input cannot become another command
- literal by default; opt-in platform-native regex (.NET on Windows, Perl on
  Unix). Regex replacement backreferences (`$1`) are supported on Windows but not
  on the Unix/`perl` path
- `perl` is used on Unix because `sed` cannot do portable multi-line edits
- gated as a workspace write; output is capped and redacted

### `apply_patch`

Applies a structured multi-file patch: create, update (exact-match hunks), and
delete operations, expressed as typed JSON generated from the input schema.

Rules:

- the whole patch is validated against the current tree before any write;
  a rejected hunk fails the call with an operation- and hunk-named error
- every touched path passes the same workspace containment as `write_file`
- the approval prompt previews the operation list

### `find_files`

Finds files by name pattern, respecting ignore files; capped results.

### `ask_user`

Puts a decision to the user: one to four questions, each offering two to four
options, asked in order. Free text is always offered as a final row — the
options are the model's guess at the answer space, the user is the authority on
it — and multi-select is per question, for choices that genuinely combine.

Rules:

- it declares **no effects**, so the permission engine never gates it. The real
  gate is the host capability: a profile that grants everything cannot conjure a
  user, and a profile that grants nothing does not stop one being asked
- where **no human is reachable** — a non-interactive run, or a host that wired
  no prompter — it returns a model-visible string saying so and telling the model
  to choose and state its assumption. A piped run, a CI run, and a subagent
  therefore never stall. Subagents get no prompter, consistent with a child never
  answering its own asks
- a **dismissed** question is not a failure: the transcript hands the decision
  back to the model with an instruction to say which way it went
- malformed calls (no questions, too many, one option, too many options, a blank
  question, duplicate labels within a question) are rejected before a human is
  made to look at them

See ADR-0121.

### `delegate`

Hands a bounded, self-contained task to a **subagent** — a child session with its
own context window, its own prompt, and its own narrower tool set — and returns
that agent's summary. Agents are declared in `*.agent.yaml` files; list them with
`localpilot agents list`.

Rules:

- the child's tools are a **filter of the calling session's registry**, so its
  set is a subset by construction: `effective = parent's tools ∩ the definition's
  list`. A definition naming a tool the session does not hold does not gain it —
  the entry is reported as narrowed
- the child's permission engine carries the **calling session's own profile**;
  delegation changes who asks, never what is allowed
- a permission ask raised inside a child is **forwarded to the caller's approver**,
  prefixed with the agent's name (for example `agent verify: cargo test`). A child never
  answers its own asks, and it cannot talk its way past a refusal
- `delegate` itself declares no effect: the child's tool calls each declare their
  own and are authorized individually
- subagents nest one level deep by default — a subagent cannot spawn another
- the caller gets a bounded summary, not the child's transcript
- a refusal (ceiling reached, no usable tools) comes back as readable output
  with a `ReportedFailure` outcome: `status: error` so the model cannot read a
  delegation that never ran as done, without counting against tool-health
  guards (ADR-0116). A session with no agent definitions at all is a
  configuration fact, reported as a success that says to do the work directly
- the child's tool calls are charged to the delegating turn's own per-turn
  ceiling, so delegation cannot be used to slip past it, and its token usage is
  republished to the caller so a delegating turn reports what it really cost.
  Nothing else from the child crosses over — its tool calls and stream output
  belong to its transcript, not the caller's

### Swarm prompts

Two pieces of first-party prompt text exist for multi-agent runs, and they are
separate because they reach different readers:

- The **coordinator directive** is appended to a session's prompt when it joins a
  swarm, and only then — guidance a session cannot act on is context it pays for
  and nothing else. It fights the three failure modes of a model given workers:
  decomposing into pieces too small to be worth a worker, spawning before it
  knows what the pieces are, and treating a worker's confident report as fact.
- The **assignment contract** travels with each dispatched task, because a worker
  has none of the coordinator's prompt. It is the entire behavioural contract a
  worker sees.

Both have a depth variant. In depth mode the coordinator is told about gates, and
the worker is told that its report must state what it did *not* check — the field
that is the point of the mode, and the one a confident model omits.

### File-touch reporting

Every file-mutating builtin — `write_file`, `append_file`, `edit_file`,
`multi_edit`, `apply_patch`, `replace_in_file` — and `read_file` report what they
touched, as typed data on their own output: the path, the operation, and the
**line range** where one is known.

Reported, not inferred. The three tempting alternatives all fail:

- *Inferring from the tool name and arguments* is fine for `write_file` and
  useless for `multi_edit`, which is one call, several ranges, some of which may
  not apply.
- *Parsing the range out of the tool's prose output* reads like it works and then
  silently stops when the wording changes.
- *Watching the filesystem* catches everything and attributes nothing.

The range is computed by comparing the file's content before and after, so it is
the extent that actually changed rather than what the tool intended. Scattered
edits in one call collapse into the single enclosing range: for an advisory
alert, over-reporting costs a message and under-reporting costs the collision.

Tools know nothing about swarms. The registry hands the touches to the caller
(`dispatch_reporting`), the turn loop publishes them as a `FilesTouched` runtime
event, and anything that cares subscribes. A session outside a swarm pays a
`Vec` that stays empty.

### `swarm`

Agent-to-agent messaging for the sessions collaborating on one repository.
Registered always, but gated by a **host capability** rather than by the
permission engine: it declares no effects (a message touches no file and runs no
command), and a session that is not part of a swarm is told so and moves on. It
is therefore inert on the single-agent path, which is nearly every session.

One tool with three actions rather than three tools. A session not in a swarm
should cost one unused entry in the schema, not three — and a model shown
`swarm_send`, `swarm_broadcast`, and `swarm_roster` will reliably invent
`swarm_reply`.

- `send` — one peer, addressed by name or by session id. An unknown *or
  ambiguous* name is refused: delivering "tell the reviewer" to an arbitrary one
  of two reviewers is worse than not delivering it.
- `broadcast` — every agent below the sender in the spawn tree. `scope: swarm`
  reaches every member, and is **coordinator-only**: without that, one worker
  deciding to keep everyone informed costs every other worker a turn, and the
  cost scales with the square of the swarm.
- `roster` — who is here, what each is doing, and which of them the asker
  spawned.

**Verb and field normalisation.** Models do not spell an action the way the
schema does; they write `dm`, `tell`, `msg`, `announce`, and put the body under
`text` or `content` or `message`. All of those are accepted. Refusing them costs
a turn every time and usually produces the same guess again.

**A long body requires a `tldr`.** Above ~600 bytes the message is rejected
without a one-line summary. The recipient is another agent in the middle of its
own task, deciding whether this is worth breaking off for — and it cannot make
that decision by reading the whole message, because reading it *is* breaking off
for it.

**Delivery modes**, all riding the same soft-interrupt substrate the user's own
steering uses, so there is exactly one set of ordering rules:

| Mode | Behaviour |
| --- | --- |
| `notify` (default) | Queued for the recipient's next safe boundary, non-urgent. |
| `interrupt` | Queued urgent — admitted between tool calls rather than after the batch. |
| `wake` | Interrupt if the recipient is mid-turn; if it is idle, start a turn, since there is nothing to interrupt. |

A message to a member that has already finished is **not** reported as
delivered: it has no turn to reach and will never start another, and counting it
would be a lie the sender then acts on.

#### Pair peer messages

The convergence driver for `localpilot pair` reuses this transport with a
narrower contract. A peer's typed protocol envelope is sent only to its bound
counterpart as `Audience::One` with `notify` delivery. It is labelled as a
system-sourced message from the actual sender, enqueued before the recipient's
next scheduled turn, and never broadcasts, wakes an idle session, or chooses who
runs next. The driver alone schedules turns, so peer traffic cannot create
concurrent model calls.

User steering is a separate path: the terminal host targets one named
`SessionHost::steer` and preserves the user source. A peer envelope therefore
cannot masquerade as user input, and a user correction cannot be mistaken for
protocol agreement. Messaging declares no tool effect and grants no authority;
any later tool call still passes through that peer session's own permission
engine.

### `search_definitions`

Searches code and returns the **enclosing declaration** — function, type, module,
or test — rather than matching lines. Each hit carries the declaration's symbol
path, its signature with the body elided, and its file and line. Optional
`language` and `kind` filters narrow the search.

Use it for "where is X defined", "which function handles Y", "what implements Z".
Use `search_text` for prose, configuration, non-code files, or when you want
every matching line — for a query that is already narrow, `search_text` returns
less. Use `find_files` to locate files by name.

Rules:

- same walk, ignore-file handling, and workspace scoping as `search_text`; the
  permission engine decides containment, the tool only reports it
- **text-first**: the literal/regex scan runs before any parsing, and only files
  that already contain a match are parsed, so cost scales with hits rather than
  repository size
- **stateless**: no index, no cache, no store — nothing to ingest and nothing to
  go stale. For questions that need a project-wide graph (callers, change impact)
  use the code-graph commands instead
- multiple matches inside one declaration collapse to one hit with a count
- default 20 hits, hard cap 100; a cut result is marked truncated and says how
  many more matched
- never silently empty: an unsupported file type, a match outside any
  declaration, or a reached scan ceiling is reported with the reason
- languages come from the shared code-intelligence grammars; a file type without
  a grammar is reported, not skipped in silence

### `read_tool_output`

Reads back the full retained output of an earlier tool call that was truncated
in context, by its retention id, optionally a line range. No new side effect:
the output was captured under the permission decision that produced it.

### `fetch`

Retrieves the body of an http/https URL over the network.

Rules:

- accept `http` and `https` schemes only; reject everything else so the tool
  cannot read local resources and sidestep the workspace boundary
- declare a network effect, so the call is gated by the permission engine (ask
  interactive, deny non-interactive unless allowlisted) like any other network
  action
- set a timeout
- cap output size and honor an optional smaller byte limit
- output is redacted like every other tool result

### `run_shell`

Runs a shell command.

Rules:

- classify command risk
- approve writes, deletes, network, package installs, and privileged commands
- set timeout
- capture stdout/stderr separately
- an explicit full-screen `!` submission is already user-confirmed: command-risk
  effects do not ask the user to confirm the exact text a second time. The
  permission engine still classifies every effect, retains `deny`, and separately
  asks for protected access such as network, secret-like, or out-of-workspace reads
- never chain destructive commands generated from untrusted path lists
- a recognized long-running command (dev server or watcher — `npm run dev`,
  `bun serve`, `vite`, `*--watch*`, …) is not run here: it would only block until
  the timeout. The tool returns a hint to use `run_background` instead, and the
  timeout message carries the same hint for the ambiguous cases.

Shell and process behaviour:

- **`&&`-capable shell.** A `command` string runs through the platform shell:
  `$SHELL -lc` on Unix, and on Windows **PowerShell 7+ (`pwsh`) when it is on
  PATH**, falling back to `powershell.exe` (Windows PowerShell 5.1) otherwise.
  `pwsh` supports the `&&`/`||` pipeline-chain operators that 5.1 lacks, so a
  chained command (`cargo build && cargo test`) runs as written instead of
  erroring. The selection is detected once and cached. It is *prefer*, not
  *require*: a host without `pwsh` still works, with `;` as the separator.
- **Working directory.** The command runs in the workspace, in the de-verbatim
  form a launched shell can use (see [harness spec](06-harness-spec.md) and
  [security & privacy](07-security-and-privacy.md)) — never a fallback like
  `C:\Windows`.
- **Whole-tree termination on timeout.** When a command exceeds its timeout its
  *entire* process tree is killed (`taskkill /T /F` on Windows; a process-group
  `kill` on Unix), so a shell-wrapped build's grandchildren (`make`→`cc1`,
  `gradle`→its daemon) never orphan and leak memory for the rest of the session.
- **Whole-tree termination on cancellation.** Dropping an in-flight
  `run_shell` future synchronously drops its capture readers, so late child
  output cannot enter model context, and an armed process-tree guard then
  best-effort signals the same Windows tree or Unix process group. The runtime
  synthesizes an explicit cancelled error result and records the failed tool
  completion rather than reporting success.

### `run_background`

Starts a long-running command (a dev server like `npm run dev` or `bun run
index.ts`, or a watcher) as a background process, then manages it. One tool with
an `action`:

- `start` (default) — spawn the command detached from the turn, draining its
  stdout/stderr into a capped rolling log. Wait a short grace period
  (`grace_secs`, default 2); if the process is still alive it is tracked under an
  id and the id plus startup output are returned, otherwise it is reported as
  having failed to stay up (with its exit code and output) and is not tracked.
- `list` — the tracked processes (id, running/exited, age, command line).
- `logs` — the captured output of a tracked process by `id`.
- `stop` — terminate and forget a tracked process by `id`.

Rules:

- a `start` is classified, permission-checked, and captured exactly like
  `run_shell`; `list`/`logs`/`stop` only manage already-approved processes and
  carry no external effect.
- the registry is **session-scoped and in-memory**: every child is started with
  `kill_on_drop`, and the session terminates all of them on close (and when a new
  session starts). No background process outlives the session — there are no
  cross-invocation daemons.

### quality-gate checks

The harness quality gate ([`docs/06`](06-harness-spec.md)) runs its ratified
`[[harness.checks]]` commands through `run_shell` — not a side channel. A check
command is classified, permission-checked, timed, and captured like any other
shell command; ratification at setup records the command, it does not exempt it
from the engine. Auto-fix invocations (`fix_command`) are project-write side
effects and are mediated the same way.

### `git_status`

Reads repository state.

Rules:

- read-only
- allowed by default inside workspace

### `git_commit`

Creates commits for harness steps.

Rules:

- pre-commit rules must pass
- message must not contain secrets
- include only intended files

## Permission Model

Decision:

- `Allow`: run immediately
- `Ask`: prompt user
- `Deny`: block and return model-visible error

Inputs:

- tool name
- normalized path
- command classification
- workspace trust
- interactive/non-interactive mode
- user policy
- harness rule state

## Result Model

Tool result text must be:

- bounded
- deterministic enough for tests
- explicit about truncation
- free of secrets where redaction is possible

A result carries a three-state **outcome**, not a boolean (ADR-0116):

- `Ok` — the tool ran and the work it wrapped succeeded.
- `ReportedFailure` — the tool ran to completion and the work it wrapped
  reported failure: a non-zero exit, a non-2xx `fetch`, a background process
  dying inside its grace period, a refused delegation, an MCP response with
  `isError: true`. The tool is healthy; the world said no.
- `Unusable` — the tool could not do its job at all: a spawn error, a timeout,
  invalid input, an unknown tool, a denial, a gate block, a cancellation.

Both failure kinds render `status: error` to the model; the distinction feeds
tool-health guards (only `Unusable` counts as a malfunction) and the turn
handoff. On the wire the pre-outcome `is_error` boolean is always written
alongside the refinement, so transcripts written before the outcome existed
keep parsing.

These bounds apply to **every** result the model can see: a tool *error* takes
the same redaction and the same context bound as a success, at the same
dispatch chokepoint (ADR-0117). There is no unredacted, unbounded path out of a
tool.

Oversized output is bounded at a single seam — the dispatch chokepoint, after
redaction: the head and tail stay in context with an explicit truncation note,
and the *full* redacted output spills to the retention store under the call id,
where `read_tool_output` can fetch it. Tools return their complete output to
that seam; there is no per-tool pre-cap that would drop data before it is
retained, so a large result is always recoverable in full.

With `[tools] elide_seen_reads` on (off by default), a `read_file` that returns
a file+range already read this session — and unchanged since (same mtime and
length) — is replaced with a compact stub pointing at the earlier read, cutting
context waste on read-heavy loops. It is conservative: a changed file (or any
doubt) always returns full content, never a stale stub, and the model can
re-page any range with `read_file` start_line/end_line. The elided read still
records as a successful `read_file`, so anything that checks "was this read"
(e.g. the require-prior-read precondition) is unaffected.

## Input Validation, Readable Errors, and Repair

Before dispatch, a tool call's arguments are validated against the tool's
generated JSON schema. The outcome drives one of three paths:

- **Valid** — the call dispatches byte-unchanged.
- **Invalid** — when `[tools] readable_errors` is on (the default), the model
  receives a concise, schema-aware message (the offending field, the expected
  shape, and a valid example from the tool's contract) instead of the raw
  deserializer string, so it can self-correct on the next turn. The raw detail is
  always kept in the logs/telemetry. Off restores the raw message (the rollback).
- **Repaired** — when `[tools] repair` is `warn`/`on` (default `off`), a small set
  of conservative, schema-guided rules fix the *validator-reported* fields only —
  wrapping a bare string as a one-element array, parsing a stringified array/object
  of the matching item type, or unwrapping a degenerate markdown autolink on a path
  field — then re-validate. A repaired call runs with the rewritten arguments and
  carries a model-visible note; `warn` additionally logs each repair loudly.

Repair is **validate-first** (a valid input is never preprocessed),
**issue-path-localized**, **schema-guided** (a rule fires only when the schema
proves the target type), and **auditable** (every repair and refusal is a redacted
session event). It changes arguments, never authority: the permission engine and
gate chain run on the repaired input. It is gated by the tool's safety contract —
a `Destructive`, `ExternalWrite`, or `Irreversible` tool, an MCP tool (no typed
schema), and any content/command field are **never** repaired.

### Declaring a field's repair intent

A tool author marks what a field *means* so the repair stage keys off declared
intent rather than a field-name guess. The markers are `#[schemars(schema_with =
"...")]` helpers in `localpilot-tools::schema_intent`; they annotate the generated
schema with an `x-localpilot-intent` extension and leave the field's Rust type and
deserialization unchanged:

- `path_string` / `glob_string` — a single path / glob. The markdown-autolink
  repair fires only on a `path`-intent field.
- `file_content_string` / `command_string` — a file body / shell string, marked
  **repair-exempt**: no rule ever parses or rewrites it, even if it looks JSON- or
  markdown-shaped.
- `one_or_many_string` — a path list (an `array<string>`) the model may give as one
  or many; the wrap/parse repairs target it.
- `line_range` — a 1-based line endpoint. A marker only — no rule consumes it
  (relational repair is deferred); it documents intent for readers and future work.

Marking content/command fields is what makes a file body or shell string
*provably* repair-exempt, so the safety guarantee is structural rather than a
heuristic over field names.

## Tool Gates

Dispatch accepts an ordered chain of tighten-only gates consulted *after* the
permission engine. A gate may block a call with a model-visible reason; it can
never grant what the engine refused. The permission engine is the always-on
first link of the chain and is not removable. Hosts register gates through the
session runtime's hook fabric (see [`docs/extending.md`](extending.md)).

### Look Before You Launch

The session runtime evaluates the `check_before_launch` rule at the dispatch gate
(see [`docs/06-harness-spec.md`](06-harness-spec.md)). When the task prompt named a
local serveable target (a loopback host, or any `host:port` with an explicit port)
that has not been probed this session, an attempt to launch a local HTTP server
(`python -m http.server`, `npx serve`, `php -S`, `vite`, …) or scaffold a competing
`index.html` surfaces a verdict nudging the model to probe the target first — *only
launch your own server if the probe fails*. A satisfied probe in the evidence
ledger (a successful `fetch`, or a `curl`/`Invoke-WebRequest`-style shell command
that hits the target) clears it, exactly like `RequiresPriorRead`. It is advisory
and best-effort: default `Warn` (the call still runs), tunable to `block` or `off`,
tighten-only, and grounded in evidence rather than the model's claim. The system
prompt carries the same look-before-launch convention as an always-on nudge.

## Project Skill Discovery

Project-local skills are advisory prompt modules (a `SKILL.md`, optionally a
`skill.toml`) under `.localpilot/skills/` or `.agents/skills/`. They are reached
**pull-based**, never pushed into context — a skill's text is loaded only when it
is searched for and chosen (the skill model is ADR-0027).

- **Deterministic, user-facing** (always available): `localpilot skills list`
  shows the discovered skills with their invocation (`user-only` /
  `discoverable`); `localpilot skills show <name>` prints one skill's body by exact
  name, with no model in the loop.
- **Model-callable** (opt-in, off by default): when `[skills]
  autonomous_discovery = true`, three read-only tools are registered —
  `skill_list` pages the whole discoverable catalog (name + one-line summary +
  scope, name order, default 50/max 100 per page, with a `next offset` when more
  remain), `skill_search` returns lean ranked locators (name + one-line summary +
  score) over the *discoverable* skills only, and `skill_load` returns one skill's
  body by exact name. `skill_list` and `skill_search` are package-only — for the
  LocalMind advisory-skill lane the model uses `active_skills`/`skill_drafts`. Search matches a skill's name, description, and command triggers with
  one shared, punctuation-insensitive signal — so a query like `threejs` finds a
  `Three.js` skill — and the inclusion gate and the ranking derive from that same
  signal (every returned skill scores at least 1). When nothing matches strongly
  it reports how many discoverable skills exist rather than implying none do, and
  when more skills match than fit the page it reports the full match count instead
  of silently dropping the rest; it never invents a match. A user-only skill
  (`disable-model-invocation: true`) is
  never returned by search or counted in that total; it is reachable only by an
  exact typed name.

Both tools are read-only (`Effect::ReadPath`) and trust-gated: project-local
skills load only when the workspace is trusted. Loading a skill injects *content
the agent reads* — it runs, installs, and enables nothing. A skill's declared
`required_tools`/`permissions` are surfaced when it is loaded for transparency,
but loading grants nothing: any real action the guidance leads to still passes
through the permission engine. This keeps the no-silent-execution contract intact
(see [`docs/localmind-integration.md`](localmind-integration.md) for the parallel
advisory-skill contract on the LocalMind side).

### Review-only discovery (`skills research`)

`localpilot skills research [-g] <query>` (and `/skills research …`) discovers
skills relevant to a query and is strictly **read-only** — it recommends, it never
registers a source or installs a skill (ADR-0099). It classifies each match as
`installed` (in the effective catalog), `available` (in a registered source), or
`discovered` (in a new public repository), ranks them by name+description, and —
for a newly discovered repository it validated read-only — saves a review proposal
to `<scope>/.localpilot/skill-proposals.toml`. `-g` searches the user-global
catalog and defaults proposals to global scope; otherwise the effective
project+global view with a project-scope default.

Web discovery (finding *new* public repositories) runs only when web research is
enabled and reuses the `[research.web]` egress — allowlist/disallowlist, egress
disclosure, audit log, and `--no-web` — with configured research search providers
preferred and the official public GitHub repository-search API as the
fresh-install fallback; a rate limit or outage yields an honest partial result and
never discards local matches. `/research <topic>` runs the same lane automatically
and adds a separate `Relevant skills` report section. Skill recommendations are
**not** research findings or memory candidates; LocalMind's Skills review tab reads
the proposals and delegates any resulting mutation back to `skills repo add` /
`skills install`.

## Pull-Discovery Broker

The tool surface can be made **pull-based** instead of advertising every tool's
schema every turn (ADR-0031). It is **off by default** (`[tools] broker = false`),
in which case the full registry is advertised exactly as before — the rollback
path.

When `[tools] broker = true`:

- **Working set.** Each turn advertises only a small **working set** of tool
  schemas — a configurable core default (a lean read/edit/search/shell set) plus
  the broker's own tools plus any tools revealed this session. Tool *names* are
  still listed in the prompt (cheap); only the *schemas* (the token cost) are
  narrowed.
- **`tool_search` / `tool_load`.** Two read-only (`Effect::ReadPath`) tools, like
  `skill_search`/`skill_load`. `tool_search` returns lean ranked locators (name,
  one-line summary, score) for a need; `tool_load` reveals one tool by exact name —
  adding it to the working set and returning its schema plus a one-line example.
- **Failure-driven trigger (always on with the broker).** A call to a tool that is
  not advertised — unknown, out-of-working-set, or retired — does not return a bare
  `unknown tool` error. The broker resolves the attempt to the closest available
  tool, reveals it, and asks the model to retry. The attempted call **does not
  run**.
- **Loose `NEED:` marker (opt-in, `[tools] marker = true`).** The model may write a
  `NEED: <capability>` line; the harness reveals the closest tool proactively.
- **Reveal-never-grant.** Revealing changes *visibility only*. Dispatch is
  unchanged: the permission engine and the tighten-only gate chain remain the sole
  execution authority, so a freshly revealed write/network tool still resolves to
  `Ask`/`Deny` exactly as if it had always been advertised. The broker never
  translates the model's arguments and runs a tool itself (no resolve-and-run).
- **Catalog.** The broker searches a live, fingerprinted projection of the
  registry, rebuilt on the registry-change signal (see [`docs/mcp.md`](mcp.md) for
  the MCP volatile edge and the deprecation overlay).
- **Learning (opt-in, `[tools] learning = true`).** The broker records a redacted
  `tool_resolution` session event per resolution, re-ranks tools that have resolved
  and succeeded before above equal-text peers, and graduates a frequently-revealed
  tool into the always-advertised set (persisted across sessions in the disposable
  project store). With learning off, the broker still works — it just does not
  learn.

Configuration: [`docs/configuration.md`](configuration.md). Host integration of
the failure-driven seam / marker parse: [`docs/extending.md`](extending.md).

## Safety Invariants

- The model cannot execute a tool outside the registry.
- The model cannot bypass permission policy.
- The harness cannot bypass permission policy.
- A gate can only tighten, never loosen, a permission outcome.
- Tool outputs are stored only after redaction.
- A failed tool call is represented as data, not a process crash.
- A cancelled tool execution is aborted (child processes killed), answered
  with a synthesized error result, and recorded in the session event log.
- A user-elicitation cancellation is returned to the model as data and never
  interpreted as consent or as an implicit option selection.
- Revealing a tool (pull-discovery broker) changes only what is advertised; it
  grants no authority, so a revealed tool still passes the full permission gate.
