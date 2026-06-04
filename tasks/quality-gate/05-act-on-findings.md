# 05 — Act-on-findings loop

## Goal
Turn gate outcomes into bounded action inside the harness loop: safe auto-fix
already ran (subject 03); remaining failures retry (bounded) then replan; audit/
dependency findings block. Replans are logged to `DECISIONS.md`.

## Boxes

- [ ] **05.1** (agent) Finding→verdict mapping helper (used by subject 04's rule):
      fixed-and-passing → `Allow`; lint/test still failing → `Retry(reason)`;
      `audit`/dependency findings → `Block(reason)`. Pure function, unit-tested.
- [ ] **05.2** (agent) Loop integration: on `Retry`, feed the finding back to the
      model bounded by `attempts_per_step`; on exhaustion, `replan` the step.
      Reuse the existing anti-sunk-cost path; do not add a parallel loop.
- [ ] **05.3** (agent) `DECISIONS.md` writer in `unshackled-harness`: append a
      `D### · date · title / decision / rationale / refs` block on replan. Idempotent
      round-trip (parse→render) fixture.
- [ ] **05.4** (agent) Tests: retry-then-pass within limit; retry-exhausted →
      replan + `DECISIONS.md` entry; audit finding blocks regardless of attempts.
- [ ] **05.5** (agent) Cross-platform: assert classification + act-on-findings on a
      Windows-style and a POSIX-style check command (ADR-0007).

## Hindsight checkpoint
- [ ] Captain Hindsight review recorded
- [ ] Verdict is `CLOSE`

## Progress log
