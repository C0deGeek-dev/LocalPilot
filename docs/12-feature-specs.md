# Feature Specs

## UI Direction

Reference images:

- `img/Base.png`
- `img/Idea.jpg`

The UI should stay terminal-native, dense, and quiet:

- header at top-left with app, version, provider/model, and workspace
- label short session IDs explicitly when shown
- large main transcript/input area
- always-visible footer stats
- optional right-side thinking/reasoning panel
- permission/mode indicators in the footer
- thinking panel toggleable at runtime
- stats footer never hidden by the thinking panel
- right panel auto-collapses on narrow terminals

Footer stats should be compact:

- model/provider
- mode
- permission state
- tokens in/out
- tokens per second
- context usage
- estimated cost/usage when known
- quota/reset timer when paused or close to limit

## Bad-Output Recovery

LocalPilot should detect visibly bad model/backend states and recover before the
bad output corrupts the session.

Detected states:

- empty assistant turn
- repeated-token loop
- slash flood such as `/////////`
- malformed tool call
- malformed structured output
- repeated provider transient error
- local vision degeneration after too many images

Detection must be context-aware. Repeated punctuation or slash-like content
inside fenced code blocks, quoted logs, base64, or explicit user-requested output
should not trigger recovery unless the run exceeds a degenerate threshold.

Malformed structured output means one of:

- provider stream cannot be decoded
- tool-call JSON fails schema validation
- required structured-output schema fails validation
- tool result pairing is impossible to repair

Recovery ladder:

1. abort the current stream
2. save a recovery diagnostic event
3. retry once with a short repair prompt
4. reduce risky context if needed
5. drop or summarize oversized tool results
6. lower local image count when relevant
7. mark provider/model degraded if recovery fails
8. stop harness progress until a clean turn is produced

Invariant: a recovered turn may continue the session, but a bad turn may not
complete a harness step.

The repair prompt has a hard token/turn budget. If it loops or produces another
bad output, stop and mark the provider/model degraded.

## Skills

A skill is a local, user-inspectable package of instructions and optional assets
that guides the agent on a specific workflow.

Initial skill shape:

```text
skills/<skill-name>/
  SKILL.md
  skill.toml
  assets/
  scripts/
  tests/
```

`skill.toml` declares:

- name
- description
- version
- triggers
- required tools
- permissions
- assets
- scripts

Trigger semantics:

- description-based relevance is the default path
- optional explicit triggers provide deterministic activation
- explicit triggers may be command names, file globs, or regexes
- model-judged relevance must be explainable in debug output
- a skill can be manually invoked by name

Skills can be:

- project-local
- user-local
- generated drafts

Generated skills are never enabled silently. The user must review the content,
permissions, and triggers.

## Skill Suggestions

LocalPilot can suggest skill creation when repeated usage patterns appear.

Skill suggestions depend on a local usage log or memory store. They should not
ship before the local store exists.

Examples:

- same command sequence repeated across sessions
- same project setup workflow repeated
- same error/fix loop repeated
- same prompt template used repeatedly

Suggestion policy:

- suggestion-only
- cooldown per pattern
- no silent file creation outside disabled drafts
- show proposed triggers and permissions
- require explicit enable

## Local Memory Store

Memory is local-only. The first implementation should be a flat, inspectable
project memory store. A graph layer can be added later if the flat store proves
insufficient.

Memory stores tagged entries:

- project facts
- durable decisions
- recurring workflows
- dependency and architecture notes
- frequent failures and fixes
- accepted skill suggestions

Memory does not store by default:

- secrets
- raw private transcripts
- credentials
- unrelated personal data

Project memory may be enabled by default with visible controls. Global memory
requires explicit first-run consent.

Retrieval rules:

- inject only the top relevant memories
- enforce a token cap
- prefer recent and verified entries
- do not inject stale entries below the relevance threshold
- show injected memories in debug/inspect output

Secret detection is best-effort. Local inspect/delete commands are required so
users can correct memory mistakes.

Required commands:

- `localpilot memory status`
- `localpilot memory search`
- `localpilot memory inspect`
- `localpilot memory delete`
- `localpilot memory disable`

## Named Sessions

Sessions are identified by a UUID. A UUID is precise but not memorable, so a
session may also be given a human name and later resumed by that name instead of
by id.

Rules:

- a name is optional; an unnamed session is unaffected
- names are unique within a workspace — naming a session to a name another
  session already holds is rejected (case-insensitive)
- a name is trimmed; an empty name, or one that parses as a session id (a UUID),
  is rejected as ambiguous
- because a session id is a UUID, a resume reference is disambiguated without a
  flag: a value that parses as a UUID resolves by id, otherwise by name
- the name is stored in the workspace session index (`.localpilot/index.json`),
  not in the replayable event log — it is metadata, not transcript
- renaming is idempotent for the owning session (re-applying its own name, in any
  case, is not a clash)

Surfaces:

- in-session: `/name <text>` (alias `/rename <text>`) names the current session;
  the name shows in the header and status line
- `localpilot session name <id|name> <new-name>` names or renames a session
- `localpilot session list` and `/sessions` show the name beside the id
- resume by name or id: `localpilot session resume <id|name>`,
  `localpilot print --resume <id|name>`, and
  `localpilot chat --resume <id|name>` (the interactive launcher, which also
  accepts `--continue` for the most recent session)

## Quota Wait/Resume

Some providers enforce token, message, session, or time-window limits. LocalPilot
should pause cleanly and resume after the reset window when the user allows it.

Modes:

- off: stop and report the reset time
- ask: prompt before waiting/resuming
- run: wait for this run and resume automatically
- global: always resume eligible paused runs when provider limits reset

Config example:

```toml
[quota]
auto_resume = "ask" # off | ask | run | global
max_wait_minutes = 360
resume_requires_clean_workspace = true
resume_requires_no_pending_approval = true
resume_only_at_step_boundary = true
```

Safety gates:

- never resume through a pending destructive approval
- never resume with dirty unrelated workspace state
- never resume mid-step
- never resume after user cancellation
- never resume if provider identity/config changed during the wait
- re-probe the provider after the reset timer
- use backoff with jitter when reset metadata is approximate
- always record why the run paused and why it resumed
- honor documented provider retry windows; do not frame this as bypassing limits

CLI:

- a run that hits a provider quota/rate limit pauses cleanly at the step
  boundary and writes an inspectable paused-run record under the project store
- `localpilot harness wait-resume --model <m>` re-evaluates every safety gate and
  either continues the run, reports the remaining wait, or explains what blocks it

UI:

- footer shows quota state and reset timer
- paused sessions show next eligible resume time
- continuous mode shows that unattended resume is enabled

## Self-Review

A **read-only** repo-health scan: `localpilot self-review` walks the workspace
and emits a ranked, advisory findings report. It writes nothing — every output
is data the reader acts on, never an action. This is the read-only front of the
human-gated self-improvement loop (ADR-0034); it never proposes or applies a
change itself.

Findings come from three sources, ranked together:

- a **static repo scan** with independent detectors — leftover
  `TODO`/`FIXME`/`XXX`/`HACK` markers, a decision **index** (registry) lagging
  the actual decision **log** (`ADR-####`/`D-LM-####`), incomplete plan/tracking
  rows (`TODO` status cells, "pending sign-off"), broken local doc links
  (doc drift), and a heuristic, opt-in missing-test signal;
- **model-reported session friction** — a model auditing the harness during a
  real task emits a structured block (`--audit-prompt` prints the prompt; feed the
  result back with `--friction-file`), normalised into the same finding shape;
- **measured (auto-captured) session friction** — the deterministic counterpart:
  a captured run's capability-scorecard `process` block (tool-call count,
  redundant calls, reproduce-before-fix, test-before-done, recovery, exit reason)
  is projected into the same finding shape with no model in the loop. Feed a
  scorecard JSON with `--process-file <path>`. Redundant tool calls, a
  budget-exceeded/no-progress stop, an edit before any observation, a done-claim
  with no test run, and a mid-task failure each surface as a ranked friction
  finding. A clean run yields none.

Each finding carries `{ kind, path, span, severity, confidence, evidence,
suggested_owner }`. The report ranks by **severity × confidence** (deterministic
tie-breaks), so the highest-value signals lead. Prior accepted lessons retrieved
from the learning engine inform the scan: a lesson naming a finding's file marks
it as a recurring issue (nudging confidence up). The report schema is tagged
(`localpilot-selfreview-v1`).

CLI:

- `localpilot self-review` prints the human summary; `--json` emits the
  machine-readable report; `--missing-tests` enables the heuristic detector;
  `--friction-file <path>` folds in a model audit block; `--process-file <path>`
  folds in measured friction from a captured run's scorecard `process` block;
  `--audit-prompt` prints the audit prompt and exits.

## Human-Approved Patch Generation

The write half of the self-improvement loop, built so the human gate is
**structural** (ADR-0034). Turning an approved finding into a fix never
touches the main branch on its own:

- **Isolated worktree.** A proposal is written into a fresh git worktree on its
  own branch under `.localpilot/worktrees/<branch>` (git-ignored, ADR-0012),
  based on the current `HEAD`. The main working tree is never modified. Rollback
  is to drop the worktree/branch.
- **Scope-bound, minimal.** A proposal carries the files the finding named; an
  edit to any other file is rejected, and the *produced* diff is re-checked to
  touch only those files. A no-op proposal (nothing actually changes) is rejected.
- **Path containment.** Edit paths are joined under the worktree with a guard
  that rejects absolute paths, `..` traversal, and drive prefixes — an edit can
  only land inside the worktree.
- **Change-provenance record.** Every proposal carries
  `{ prompt, model, tools_used, test_evidence, rationale, risks, rollback_notes,
  lessons }` (plus an eval result once the gate runs). It is meant to live with
  the proposal (e.g. the private hub), **not** in shipped code.
- **Hard approval gate.** The only operation that writes outside the worktree is
  promoting the proposal onto the main branch, and it requires an approval token
  that authorizes exactly that patch. The token's sole constructor is an explicit
  human-confirmation call; the autonomous loop never mints one, so there is no
  path from "propose" to a main-branch write without a human. Promotion is
  conservative — it refuses a dirty target tree, fast-forwards only, and **never
  pushes**. The agent never self-merges.

The git surface is a fixed set of subcommands run as argv (never a shell), with
no network subcommand anywhere; see [docs/07-security-and-privacy.md](07-security-and-privacy.md).

CLI (the gated write-half surface):

- `localpilot self-review propose-patch --finding <rank> --model <model> [--provider <id>]`
  — a model authors a minimal, scope-confined fix for the ranked finding (from a fresh
  `self-review` scan) into an isolated worktree; the command prints the diff and stops,
  leaving the proposal on disk for review.
- `localpilot self-review promote --id <id> --reviewer <you> --approve` — apply the
  reviewed proposal onto the main branch. `--approve` is the explicit human act that
  mints the approval token; without it, promotion is refused. Fast-forward only; never
  pushes.
- `localpilot self-review discard --id <id>` — drop the proposal's worktree and branch.

A proposal **persists across invocations** (`propose-patch`, then a later `promote` or
`discard`) via the on-disk worktree plus its provenance record, so the human reviews the
diff between proposing and promoting. Reattaching to a proposal mints no token and writes
no main branch — the approval gate on `promote` is unchanged.
