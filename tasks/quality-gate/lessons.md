# Lessons (disposable run-notes)

> Append as slices teach something. Durable lessons migrate to `tasks/lessons.md`
> at the gate before this folder is deleted.

- 2026-06-04 · The rule engine already models `Verdict::Retry/Discard/Block` and
  `Trigger` is `#[non_exhaustive]`, so the gate extends cleanly — add a
  `PhaseComplete` trigger and a `quality_gate` rule rather than a parallel system.
- 2026-06-04 · `HarnessConfig.test_command` is `Option<String>`; the gate must
  keep it as a back-compat synonym for a single phase `test` check.
- 2026-06-04 · `run_shell` runs program+args with NO shell. Store checks as
  program+args (D002), not a command string — avoids unsafe word-splitting.
- 2026-06-04 · The Relaxed permission allowlist matches by TOOL NAME, not by
  command. The gate needs its own tool identity (D003) or allowlisting it would
  open all shell. Ratification must also grant that identity an allowance (D005),
  because cargo checks are `ProjectWrite` = deny non-interactively.
- 2026-06-04 · No `phase` boundary exists in the engine yet; `Trigger` is
  `#[non_exhaustive]` so `PhaseComplete` adds cleanly, but the loop must define
  what a phase is (subject 04, after reading worker.rs/resume.rs).
- 2026-06-04 · BLOCKER: baseline `clippy --all-targets` red on config tests
  (D006), pre-existing. Every checkpoint gate is red until fixed. RESOLVED s3.
- 2026-06-04 · `unshackled_sandbox::classify()` dispatches per OS
  (classify_windows vs classify_posix). Cross-platform tests must assert against a
  `CommandClass` directly, not feed a POSIX command (`rm -rf`) and expect
  `Destructive` — on Windows that classifies `Unknown`. Bites subject 05's
  cross-platform act-on-findings box.
- 2026-06-04 · `recovery::detect("", false)` flags an empty turn as
  `BadOutputKind::EmptyTurn` → `REPAIR_PROMPT` → eventually `MaxTurns`. So a
  multi-attempt resume test must script every attempt to emit **non-empty** text,
  and a `tool_call` attempt needs a trailing `text(...)` to close the turn (the
  post-tool stream would otherwise be the empty default `Done`). `FakeProvider`
  scripts are a single FIFO across all `stream()` calls, not per `run_turn`.
- 2026-06-04 · The live loop is `resume_one_step`, not `StepLoop` directly —
  `StepLoop` existed but was test-only. Subject 05 made `resume_one_step` drive it.
  The gate runs at `StepComplete` only; `PhaseComplete` has no driver yet (the
  phase-boundary surface is subject 06).
