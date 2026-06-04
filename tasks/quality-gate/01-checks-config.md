# 01 — `[[harness.checks]]` config schema + parsing

## Goal
Add the ratified quality-gate config to `unshackled-config`: a `CheckConfig`
list on `HarnessConfig`, parsing `.unshackled.toml`'s `[[harness.checks]]`, with
defaults, validation, and serde round-trip.

## Boxes

- [ ] **01.1** (agent) Add `checks: Vec<CheckConfig>` to `HarnessConfig`
      (`crates/unshackled-config/src/schema.rs`), defaulting empty. `CheckConfig`
      fields (D002 — program+args, not a shell string): `name: String`,
      `program: String`, `args: Vec<String>`, `fix_program: Option<String>`,
      `fix_args: Vec<String>`, `cadence: Cadence` (`Step`/`Phase`),
      `auto_fix: AutoFix` (`No`/`Safe`/`Full`), `severity: Option<RuleSeverity>`.
- [ ] **01.2** (agent) Serde: `cadence`/`auto_fix` snake_case; `auto_fix = true`
      maps to `Full`, `"safe"` to `Safe`, `false`/absent to `No`. `test_command`
      back-compat: an unset `checks` with a set `test_command` yields one
      synthesized `Phase` test check (document the equivalence).
- [ ] **01.3** (agent) Validate: unique non-empty `name`; non-empty `command`;
      typed error in `unshackled-config` error enum for duplicate/empty.
- [ ] **01.4** (agent) Tests: round-trip a `[[harness.checks]]` fixture
      (parse→serialize→equal); `auto_fix` bool/"safe" variants; `test_command`
      synthesis; duplicate-name rejection.

## Hindsight checkpoint
- [ ] Captain Hindsight review recorded
- [ ] Verdict is `CLOSE`

## Progress log
