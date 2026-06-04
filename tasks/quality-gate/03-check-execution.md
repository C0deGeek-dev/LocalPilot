# 03 — Check execution through the permission/sandbox path

## Goal
Run a ratified check command through the *existing* `run_shell`/sandbox path so
it is classified, permissioned, timed, and captured — then turn its result into
findings. No side channel (docs/05).

## Boxes

- [ ] **03.1** (agent) Add a check-runner that builds a `PermissionRequest` for a
      `CheckConfig` (program+args) presenting a **distinct tool identity** (D003,
      e.g. `quality_check`), calls `classify()` + `PermissionEngine::decide`, and
      only spawns on `Allow`. Reuse classification/permission; do not re-implement.
- [ ] **03.2** (agent) Capture exit code + stdout/stderr (bounded, redacted per
      the result model) into a `CheckOutcome { check, passed, findings }`.
      First-cut `findings` = exit-code + captured output (per 00.7).
- [ ] **03.3** (agent) Auto-fix execution: when a check fails and `auto_fix` is
      `Full`/`Safe`, run `fix_command` (also through the permission path,
      project-write class) and re-run the check once; record both outcomes.
- [ ] **03.4** (agent) Tests: a passing check → `passed`; a failing check →
      finding with captured output; fix-then-pass path; assert the command went
      through the classifier (permission decision observed), not a raw spawn.

## Hindsight checkpoint
- [ ] Captain Hindsight review recorded
- [ ] Verdict is `CLOSE`

## Progress log
