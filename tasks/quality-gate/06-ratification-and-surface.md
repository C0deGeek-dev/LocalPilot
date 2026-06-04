# 06 — Ratification, CLI surface, security, docs/eval sync

## Goal
Close the loop the user actually touches: discovery proposes → user ratifies into
`.unshackled.toml`; non-interactive runs use only the ratified gate; results are
surfaced. Sync docs and add an eval.

## Boxes

- [ ] **06.1** (agent) Ratification: a command/flow that takes the proposed gate
      (subject 02) and writes ratified `[[harness.checks]]` into `.unshackled.toml`
      (preserving existing config). Re-probe proposes *additions*; never auto-adds.
- [ ] **06.2** (agent) Security enforcement: a non-interactive run executes only
      ratified checks; a proposed-but-unratified check never runs. Surface a
      check's command class at ratification; `destructive`/`privileged`/`network`
      shown with explicit warning. Test both.
- [ ] **06.3** (agent) Surface results: gate outcomes shown in `harness status`
      and at step/phase boundaries (which checks ran, pass/fail, what was
      auto-fixed). Bounded, redacted output.
- [ ] **06.4** (product-owner) Confirm the ratification UX wording and the default
      gate proposed for a Rust repo. Mirror into `manual-actions.md`.
- [ ] **06.5** (agent) Sync: ensure docs/06 examples match the shipped config;
      add a `write-golden-eval` task exercising a discovered→ratified→run→fixed
      gate; update docs/14 §6 quick reference if commands changed. Run full gate.

## Hindsight checkpoint
- [ ] Captain Hindsight review recorded
- [ ] Verdict is `CLOSE`

## Progress log
