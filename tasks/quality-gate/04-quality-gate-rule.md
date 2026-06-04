# 04 — `quality_gate` rule, `PhaseComplete` trigger, cadence dispatch

## Goal
Wire the gate into the deterministic rule engine: a `quality_gate` rule that runs
the right checks at the right cadence and maps outcomes to verdicts. Generalize
the relationship to `suite_green`.

## Boxes

- [ ] **04.1** (agent) Add `Trigger::PhaseComplete` to `rules.rs` (the enum is
      `#[non_exhaustive]`); `Step` checks evaluate on `StepComplete`, `Phase`
      checks on `PhaseComplete`.
- [ ] **04.2** (agent) Extend `RuleContext` with the gate outcomes for the
      current trigger (e.g. `gate_outcomes: Vec<CheckOutcome>`), set by the loop
      before evaluation. Keep `RuleContext` cheap/cloneable.
- [ ] **04.3** (agent) Implement the `quality_gate` rule: critical, default
      `Block`; per-check `severity` override; a failed check with no fix maps to
      the act-on-findings verdict (subject 05 supplies the mapping helper). Keep
      `suite_green` as the named `test` check for back-compat.
- [ ] **04.4** (agent) Register in `RuleEngine::with_baseline`; tests: step-cadence
      check fires on `StepComplete` not `PhaseComplete` and vice versa; per-check
      `severity` override respected; critical clamp (cannot be `Off`).

## Hindsight checkpoint
- [ ] Captain Hindsight review recorded
- [ ] Verdict is `CLOSE`

## Progress log
