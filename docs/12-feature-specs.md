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

Unshackled should detect visibly bad model/backend states and recover before the
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

Unshackled can suggest skill creation when repeated usage patterns appear.

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

- `unshackled memory status`
- `unshackled memory search`
- `unshackled memory inspect`
- `unshackled memory delete`
- `unshackled memory disable`

## Quota Wait/Resume

Some providers enforce token, message, session, or time-window limits. Unshackled
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
- `unshackled harness wait-resume --model <m>` re-evaluates every safety gate and
  either continues the run, reports the remaining wait, or explains what blocks it

UI:

- footer shows quota state and reset timer
- paused sessions show next eligible resume time
- continuous mode shows that unattended resume is enabled
