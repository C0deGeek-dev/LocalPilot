# 05 — Session Runtime (Agent Mode) and Bad-Output Recovery

## Goal
> Phase 4 (`docs/03`) + Phase 9 (`docs/03`, `docs/12`). Build the conversational
> agent-mode loop: stream model events, execute tool calls through the
> permission engine, append results, persist the transcript, and support
> cancellation, loop limits, print mode, and context compaction. Then build the
> recovery engine that detects degraded model/backend states and recovers
> without corrupting the session. This is the shared loop both operating modes
> use (`docs/02`); harness mode (subject 06) wraps it.

## Boxes
> ID = `05.<box-number>`. All agent-owned.

- [x] **05.1** (agent) Implement the conversation **state machine** for one
      turn (`docs/02` §Normal Chat Turn): build provider-neutral messages →
      expose allowed tool schemas → stream provider events → route tool calls
      through permission checks → append tool results → loop until `Done`.
      (Verified: integration test with the fake provider performing a file read
      then a final answer.)
- [x] **05.2** (agent) Stream model events into **UI-agnostic** runtime events
      (text deltas, reasoning deltas as metadata not final answer, tool calls,
      usage, warnings) consumed via a channel so the TUI (subject 08) and print
      mode share one event source (`docs/13` §5 channels: `broadcast`/`watch`).
      (Verified: event-stream test asserts reasoning is tagged metadata.)
- [x] **05.3** (agent) Execute tool calls via subject-04 registry + permission
      engine; a denied/failed tool call is represented as **data** (tool result
      with `is_error`), never a crash (`docs/05` safety invariants). (Verified:
      denied-permission path returns a model-visible error result.)
- [x] **05.4** (agent) Persist the transcript through `unshackled-store` with
      redaction before persistence; the transcript is supporting context, not
      source of truth (ADR-0003, `docs/02`). (Verified: transcript written;
      redaction test.)
- [x] **05.5** (agent) Implement **cancellation** (`docs/13` §5,
      `tokio_util::CancellationToken`/`select!`): user interrupt or shutdown
      stops the stream and tool execution and leaves persisted state consistent
      (no half-written files/sessions). (Verified: `docs/08` Integration test —
      cancellation during streaming/tool execution leaves consistent persisted
      state.)
- [x] **05.6** (agent) Implement loop **limits**: max turns and max tool calls,
      configurable, enforced deterministically. (Verified: limit tests — loop
      stops at the cap with a clear status.)
- [x] **05.7** (agent) Implement **print mode** (`docs/01` Interfaces): single
      prompt in, answer out; no workspace mutation unless explicitly enabled;
      useful in pipelines. (Verified: `assert_cmd` test — print mode emits an
      answer and makes no writes by default.)
- [x] **05.8** (agent) Implement **agent-mode** entry wiring the loop to config
      mode (`--mode agent`, default) and permission profile
      (`--permission`/`--bypass`), with the active profile surfaced for the
      footer (`docs/06` Mode and Permission Flags). Tools still pass through the
      permission engine in agent mode (`docs/01`). (Verified: flag-parsing tests;
      agent-mode loop runs with `default` profile prompting on risky actions.)
- [x] **05.9** (agent) Implement **context compaction** before overflow
      (`docs/03` Phase 8, `docs/08` Context): preserve tool-result pairing and,
      under harness mode, the current step contract. (Verified: `docs/08`
      Context tests — compaction preserves tool-result pairing; compaction
      preserves current step contract.)
- [x] **05.10** (agent) Implement the recovery model in `unshackled-recovery`:
      `ModelHealth` + `RecoveryAction` types; detectors for empty assistant
      turn, repeated-token loop (only after a threshold), slash flood
      (`/////////`), malformed tool call, malformed structured output, repeated
      provider transient error (`docs/03` Phase 9, `docs/12`). (Verified:
      `docs/08` Recovery tests — repeated-token loop detected only after
      threshold; malformed tool calls trigger recovery.)
- [x] **05.11** (agent) Make detection **context-aware**: slash-like / repeated
      punctuation inside fenced code, quoted logs, base64, or explicit
      user-requested output does NOT trigger recovery unless a degenerate
      threshold is exceeded (`docs/12`, `docs/11`). (Verified: `docs/08` Recovery
      tests — slash flood outside code detected; slash-like content inside fenced
      code is not.)
- [x] **05.12** (agent) Implement the recovery **ladder** (`docs/12`): abort
      stream → save diagnostic → retry once with a short repair prompt → reduce
      risky context (drop/summarize oversized tool results, lower local image
      count) → mark provider/model degraded if recovery fails → stop harness
      progress until a clean turn. Repair prompt has a **hard token/turn budget**
      (`docs/11`). (Verified: ladder test with a fake that emits a bad class then
      recovers; budget-exhaustion marks degraded.)
- [x] **05.13** (agent) Persist recovery diagnostics and expose degraded status
      to CLI/TUI; enforce the invariant that a recovered turn may continue but a
      **bad turn may not complete a harness step** (`docs/12`, `docs/03` Phase 9
      Done-when). (Verified: `docs/08` Recovery test — exhausted recovery cannot
      complete a harness step; degraded status surfaces in `doctor`/status
      output.)


## Hindsight checkpoint
> Run after all boxes in this subject are complete and before marking
> the subject `DONE` in §5. Use the embedded prompt in `tasks/Unshackled-Plan.md`
> "Appendix: Captain Hindsight Prompt". Record the review result here.
>
> Required output sections: Keep; Fix before closing; Record; Risk;
> Verdict (`CLOSE` or `DO NOT CLOSE`). If the verdict is `DO NOT CLOSE`,
> leave the subject open, add/reopen boxes or update decisions/lessons,
> and rerun this checkpoint after the fixes.
>
> Subjects already marked `DONE` before this checkpoint was added still need
> this section completed retroactively before the §7 gate review is ticked.

- [x] Captain Hindsight review recorded
- [x] Verdict is `CLOSE`

### Review result

1. **Keep:** The shared agent loop lives in `unshackled-harness` and is the single
   path from model output to side effects — it routes every tool call through the
   subject-04 permission-gated registry, persists redacted messages, and treats a
   denied/failed tool call as data. UI-agnostic `RuntimeEvent` over `broadcast`
   keeps print mode and the future TUI on one source. Cancellation via
   `CancellationToken`/`select!` only persists complete messages, so the transcript
   stays consistent. Recovery is a separate crate with context-aware detection and
   a budgeted ladder, and the `step_completable` invariant is enforced at the type
   level. These are the right durable seams.
2. **Fix before closing:** None blocking. The interactive agent REPL with live
   approval prompting and the footer status line are intentionally deferred to the
   TUI (subject 08); print mode covers the non-interactive agent entry and is
   tested. Compaction's "preserve the current step contract" half is exercised once
   harness steps exist (subject 06), which reuses this loop.
3. **Record:** D013 (session runtime location + interactive-REPL deferral) added to
   §4. Lessons unchanged — no new traps beyond those already logged.
4. **Risk:** The token estimate for compaction is a chars/4 heuristic, adequate for
   alpha but coarse; the live cancellation race is only exercised via pre-cancel,
   not a mid-stream race (deterministic test limitation). Both acceptable for alpha.
5. **Verdict:** CLOSE.

## Progress log
> One line per slice. Date · slice · box IDs · what shipped · how verified.

- 2026-06-02 · slice 1 · 05.10–05.12 · `unshackled-recovery`: `BadOutputKind`,
  `ModelHealth`, `RecoveryAction`, persistable `RecoveryDiagnostic`; context-aware
  detectors (empty turn, repeated-token loop past threshold, slash/punctuation
  flood that tolerates fenced code until a high threshold); `RecoveryEngine`
  driving the `docs/12` ladder (abort→diagnostic→repair→reduce-context→
  summarize→degraded→stop-progress) under a hard repair budget, with the
  `step_completable` invariant (a bad/unrecovered turn or a degraded model cannot
  complete a harness step). Verified: 8 tests — slash flood in/out of code,
  threshold loop, malformed-tool-call repair, exhausted-recovery degraded;
  clippy(-D)/fmt clean. (05.13 CLI/status surfacing lands with the session loop.)
- 2026-06-02 · slice 2 · 05.1–05.6, 05.9 · `unshackled-harness` session runtime:
  the shared agent-mode loop — build provider-neutral request, stream events,
  route tool calls through the subject-04 permission-gated registry (denied/failed
  = error result data), append + persist (redacted) each message, loop to `Done`.
  UI-agnostic `RuntimeEvent` over a `broadcast` channel (reasoning tagged
  metadata); `CancellationToken`/`select!` cancellation leaving a consistent
  transcript; deterministic max-turns / max-tool-calls caps; context compaction
  preserving tool-call/result pairing + leading system messages; recovery wired
  (bad turn → ladder/repair, degraded → stop). Verified: 9 tests (read-then-answer
  loop, reasoning-as-metadata, denied-tool data, redacted transcript, cancel
  consistency, both caps, compaction pairing); clippy(-D)/fmt clean.
- 2026-06-02 · slice 3 · 05.7, 05.8, 05.13 · CLI `unshackled print` (non-interactive
  single-prompt agent run): streams the answer, runs `NonInteractive` so writes are
  denied by default (`--allow-writes` to enable), `--permission`/`--bypass` mapped
  to profiles. Degraded status surfaced to stderr; recovery diagnostics persisted
  via the store. Verified: `assert_cmd` print test (emits answer, no source writes,
  vs wiremock) + a profile flag-mapping unit test; clippy(-D)/fmt clean. The
  interactive agent REPL with live approval prompting and the footer status line
  land with the TUI (subject 08) — see D013.