# 05 — Act-on-findings loop

## Goal
Turn gate outcomes into bounded action inside the harness loop: safe auto-fix
already ran (subject 03); remaining failures retry (bounded) then replan; audit/
dependency findings block. Replans are logged to `DECISIONS.md`.

## Boxes

- [x] **05.1** (agent) Finding→verdict mapping — DONE in subject 04 (`gate_verdict`
      in rules.rs, D007): fixed-and-passing → `Allow`; lint/test failing →
      `Retry`; `audit`/dependency (severity block) → `Block`. Unit-tested in 04.
- [x] **05.2** (agent) Loop integration: `resume_one_step` now drives the existing
      `StepLoop`; each pass works the step, runs the step-cadence gate via the new
      `SessionRuntime::run_gate_checks` (its own engine/approver), reduces rules +
      gate to a `StepAction` (new `decide_step`, generalizing `evaluate_completion`),
      and on `Retry` re-prompts bounded by `attempts_per_step`; on exhaustion,
      replans. No parallel loop.
- [x] **05.3** (agent) `DECISIONS.md` writer: new `decisions.rs` document model
      (parse/render/append, next-`D###` id, in-crate `today()`); `record_replan`
      creates-or-appends on replan. Round-trip fixtures (`render`→`parse`) hold.
- [x] **05.4** (agent) Tests (tests/resume.rs): retry-then-pass within limit;
      retry-exhausted → replan + `DECISIONS.md` entry; audit finding blocks with no
      retry (single attempt, no DECISIONS.md). Plus `decide_step` unit tests.
- [x] **05.5** (agent) Cross-platform: `act_on_findings_is_cross_platform` asserts
      both `classify_posix`/`classify_windows` directly and runs the native failing
      check → `Retry` (ADR-0007). Gate builders are native-per-OS like runner.rs.

## Hindsight checkpoint
- [x] Captain Hindsight review recorded
- [x] Verdict is `CLOSE`

**Keep:** `StepLoop` is reused as the one anti-sunk-cost path — `resume_one_step`
loops it, no parallel machinery. `decide_step` generalizes `evaluate_completion`
(the latter now delegates), so the commit/retry/block reduction is single-sourced.
The gate runs through the session's own `PermissionEngine`/`Approver` (D004), so a
check can never skip a permission decision. `DECISIONS.md` follows the brief/progress
document-model idiom and round-trips.

**Fix before closing:** none. Workspace gate green (harness lib + 5 resume
integration tests; fmt/clippy/test/check `--workspace`).

**Record:** D008 — on replan exhaustion `resume_one_step` records the deviation to
`DECISIONS.md` and halts the step (`committed:false`, "queued for replanning")
rather than regenerating `PROGRESS.md` in-loop; regeneration stays the existing
`plan --replan` path. This keeps the highest-risk core-loop edit (auto-rewriting
the plan) out of scope. `MAX_REPLANS` is an in-crate constant (no new config
field, YAGNI); `today()`/civil-date is implemented in-crate (no date dependency).

**Risk:** `decide_step` maps a `Discard` verdict to a keep-context `Retry` (no
working-tree reset) — the gate never emits `Discard`, so this is defensive; a true
context-reset retry needs a fresh runtime, deferred. `PhaseComplete` is wired in
the rule/trigger but `resume_one_step` only fires `StepComplete`; the phase-boundary
driver is subject 06's surface.

**Verdict:** CLOSE.

## Progress log
- 2026-06-04 · s8 · 05.2-05.5 · wired the gate into the live loop: `decide_step` +
  `StepAction` in worker.rs (evaluate_completion delegates), `SessionRuntime::
  run_gate_checks`, `resume_one_step` drives `StepLoop` with retry/replan/block,
  new `decisions.rs` (DECISIONS.md model) + `record_replan`; 05.1 was D007 in s7 ·
  fmt/clippy/test/check `--workspace` green · commit `bc2a607`.
