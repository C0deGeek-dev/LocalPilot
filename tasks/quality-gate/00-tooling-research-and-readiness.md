# 00 — Tooling Research And Readiness

## Goal
Map the real seams the quality gate plugs into (rule engine, config schema,
sandbox/permission path, run_shell, harness loop), run the baseline gate, and
record the profile/finding-parsing strategy so subjects 01-06 execute against
known structures.

## Boxes

- [x] **00.1** (agent) Read repo instructions + ADR-0009 and the spec sections
      (docs/06 §Quality Gate / §Rule Engine, docs/05 §quality-gate checks, docs/07
      §Discovered Tooling). Constraints recorded in the plan §2/§6.
- [x] **00.2** (agent) Inventory crates touched: `unshackled-config` (schema),
      `unshackled-harness` (rules + loop: worker/resume/session), `unshackled-tools`
      (run_shell builtins/registry), `unshackled-sandbox` (command/permission/path).
- [x] **00.3** (agent) Baseline gate: `fmt` clean, `test` pass, `check` clean;
      `clippy --all-targets` RED (exit 101) — **pre-existing** unwrap in helper fns
      in `unshackled-config/tests/config.rs:37,83,84`, introduced by commit
      `4679ceb`, unrelated to this plan. Blocks every checkpoint gate. See D006.
- [x] **00.4** (agent) Execution path: `run_shell` (builtins.rs) runs program+args
      with **no shell**, `classify()`→`CommandClass`→`Effect::RunCommand` via
      `Tool::effects()`; `PermissionEngine::decide` (permission.rs) maps effect→
      decision. A check must build a `PermissionRequest` and route through
      `decide` — there is no spawn that skips it. See D002/D003.
- [~] **00.5** (agent) `step_complete` fires in the rule engine on
      `Trigger::StepComplete`. No explicit `phase` boundary exists yet — "phase"
      must be introduced (group of PROGRESS steps / milestone). Defer the exact
      seam to subject 04 after reading `worker.rs`/`resume.rs` there.
- [x] **00.6** (agent) Placement: profiles + discovery + gate-runner live in
      `unshackled-harness`, reusing `unshackled-sandbox` (`classify`,
      `PermissionEngine`) and the spawn pattern from `unshackled-tools` run_shell.
      See D004.
- [x] **00.7** (agent) Finding parse: first cut = exit-code + bounded/redacted
      stdout+stderr as one finding; structured per-tool parsing deferred. Rust
      auto_fix: fmt=`true`, clippy=`safe`, others `false`.
- [~] **00.8** (agent) Baked findings into D002-D006 and subjects 01/03; readiness
      summary pending the baseline-blocker decision (D006).

## Hindsight checkpoint
- [ ] Captain Hindsight review recorded
- [ ] Verdict is `CLOSE`

## Progress log
- 2026-06-04 · s1 · 00.1 00.2 · read ADR-0009 + specs, mapped crates/seams from
  rules.rs and config/schema.rs · verified by reading source · baseline gate
  launched in background, scaffolding committed next.
- 2026-06-04 · s2 · 00.3 00.4 00.6 00.7 · read command.rs/permission.rs/
  builtins.rs; recorded execution path, classification, and module placement;
  ran baseline gate (clippy red, pre-existing — D006) · verified by reading
  source + gate output · checkpoint commit next. 00.5/00.8 left partial.
