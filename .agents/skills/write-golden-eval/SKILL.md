---
name: write-golden-eval
description: >-
  Author a golden-task eval — a deterministic, original task fixture with the
  scorecard fields the suite records. Use when adding to the eval suite so tasks
  stay first-party and reproducible, not ad hoc benchmarks.
---

# write a golden eval

Authoritative contract: [`docs/08-testing.md`](../../../docs/08-testing.md)
§Golden-Task Evals.

## Rules

- Each task is original to this repo and derived from the spec — never a copied
  benchmark or a fixture lifted from another implementation (see [[clean-room-guard]]).
- Record every scorecard field [`docs/08`](../../../docs/08-testing.md) names
  (task id, setup, success criteria, result, etc.). Success criteria are
  observable and deterministic.
- Fixtures use `tempfile` / in-repo sample workspaces — never the real
  home/config. Default-offline; anything live is opt-in behind
  `LOCALPILOT_LIVE_TESTS`.

## Must-pass

The eval runs end to end and reports a per-task pass/fail plus the aggregate
success rate the scorecard expects. A new task fails loudly if its success
criteria are not met.
