# QualityGate-Plan.md

> Build-process plan for the discovered project quality gate (ADR-0009).
> Disposable: this folder is deleted before v1. Shipped code/commits stay
> plan-agnostic; durable decisions are promoted to ADRs in `docs/10-decisions.md`.

## Collaboration model

| Field | Value |
|---|---|
| Mode | `solo` |
| Primary owner | agent |
| Coordinator | agent |
| Resume safety | required |
| Parallel branches | `no` |
| Notes | Build on branch `docs/build-workflow` (spec + ADR already landed here). |

### Parallel work tracker

| Subject | Owner | Branch | Dependencies | Conflict-risk files | Status | Handoff notes |
|---|---|---|---|---|---|---|
| n/a | | | | | | |

---

## 1. Subject

Implement the discovered project quality gate spec'd in ADR-0009 and
`docs/06-harness-spec.md`: a set of language-specific inspection checks, drawn
from built-in toolchain profiles, discovered per project, ratified into
`.unshackled.toml`, run by the rule engine at a per-check cadence, and acted on
(safe auto-fix + bounded retry/replan; block on dependency/audit findings).

**In scope:** `[[harness.checks]]` config; toolchain-profile abstraction with a
Rust profile and one second profile to prove generality; stack detection + tool
probing producing a *proposed* gate; check execution through the existing
permission/sandbox path with output→findings parsing; a `quality_gate` rule with
a phase cadence; the act-on-findings loop; ratification surface + security model.

**Out of scope:** new providers; TUI views beyond surfacing gate results; deep
per-tool finding parsers beyond exit-code + a minimal structured parse; profiles
beyond the two built here (later profiles are follow-on work, not this plan).

---

## 2. Authoritative inputs

| Source | Contribution |
|---|---|
| `docs/10-decisions.md` ADR-0009 | The decision and its constraints (profiles = abstraction, instances discovered; ratify-then-run; per-check cadence; act-on-findings). |
| `docs/06-harness-spec.md` §Quality Gate, §Rule Engine | File/config shape, `quality_gate` rule, cadence, act-on-findings policy, `DECISIONS.md`. |
| `docs/05-tool-system.md` §quality-gate checks | Checks run through `run_shell`/permission engine, not a side channel. |
| `docs/07-security-and-privacy.md` §Discovered Tooling | Trust model: discovery proposes, user ratifies, engine mediates. |
| `crates/unshackled-harness/src/rules.rs` | `Trigger`, `Verdict`, `Rule`, `RuleContext`, `RuleEngine`, existing `SuiteGreen`. |
| `crates/unshackled-config/src/schema.rs` | `HarnessConfig`, `RuleSeverity`; where `[[harness.checks]]` lands. |
| `crates/unshackled-sandbox`, `crates/unshackled-tools` | Command classification, permission engine, `run_shell` execution path. |
| `docs/13-rust-best-practices.md` | Engineering rules (MSRV 1.82, typed errors, no unwrap/expect on lib paths, pinned deps). |

---

## 3. Subject file index

| # | File | Subject |
|---|---|---|
| 00 | `tasks/quality-gate/00-tooling-research-and-readiness.md` | Tooling research and readiness |
| 01 | `tasks/quality-gate/01-checks-config.md` | `[[harness.checks]]` config schema + parsing |
| 02 | `tasks/quality-gate/02-profiles-and-discovery.md` | Toolchain profiles + stack detection/probe → proposed gate |
| 03 | `tasks/quality-gate/03-check-execution.md` | Run a check through the permission/sandbox path; parse findings |
| 04 | `tasks/quality-gate/04-quality-gate-rule.md` | `quality_gate` rule, `PhaseComplete` trigger, cadence dispatch |
| 05 | `tasks/quality-gate/05-act-on-findings.md` | Auto-fix + bounded retry/replan/block; `DECISIONS.md` |
| 06 | `tasks/quality-gate/06-ratification-and-surface.md` | Ratification UX, CLI surface, security, docs/eval sync |

---

## 4. Decision log

> Append-only. Spec deviations recorded here; durable architecture calls promoted
> to an ADR in `docs/10-decisions.md` and cited by number.

| ID | Date | Title | Decision | Rationale | Refs (box IDs / files / ADR#) |
|---|---|---|---|---|---|
| D001 | 2026-06-04 | Plan created | Build the quality gate as a Tier-L plan, dogfooding `plan-large-task`. | ADR-0009 is accepted; spec is in docs/06/05/07. | ADR-0009 |
| D002 | 2026-06-04 | Checks are program+args, not a shell string | `CheckConfig` stores `program: String` + `args: Vec<String>`, not a `command` string. | `run_shell` runs an arg list with no shell interpretation; storing a string would force unsafe splitting. The docs/06 `command = "…"` example is presentational; config is structured. | 01.1, builtins.rs |
| D003 | 2026-06-04 | Gate uses a distinct tool identity | Check execution presents a dedicated tool name (e.g. `quality_check`) to the permission engine, not `run_shell`. | The Relaxed allowlist matches by **tool name**; allowlisting `run_shell` would auto-approve all shell. A distinct identity lets ratification allow only the gate. | 03.1, 06.2, permission.rs |
| D004 | 2026-06-04 | Gate-runner lives in `unshackled-harness` | Profiles, discovery, and the gate-runner live in `unshackled-harness`, reusing `unshackled-sandbox` `classify`/`PermissionEngine` and the run_shell spawn pattern. | Loop-adjacent; avoids a new crate; keeps classification/permission single-sourced. | 00.6, 02, 03 |
| D005 | 2026-06-04 | Ratification = permission allowance | Ratifying the gate records the checks AND grants their tool identity an allowance (relaxed allowlist / equivalent) so cargo `ProjectWrite` checks run non-interactively. | cargo fmt/clippy/test classify `ProjectWrite` → non-interactive Default = Deny; without an allowance the gate could never run headless. | 06.1, 06.2, permission.rs |
| D007 | 2026-06-04 | Verdict mapping landed in subject 04, not 05 | `gate_verdict` (finding→verdict) lives in `rules.rs` with the `quality_gate` rule; `CheckOutcome` carries the check's `severity`. | The rule can't compile without the mapping; 05.1 originally planned it. Subject 05 now only wires the loop + `DECISIONS.md`, consuming the rule's Retry/Block verdicts. | 04.3, 05.1, rules.rs |
| D006 | 2026-06-04 | Baseline clippy was pre-existing red — RESOLVED | `cargo clippy --all-targets` failed on `unshackled-config/tests/config.rs` (unwrap in non-`#[test]` helper fns). User chose proper handling (no lint allow): recover the poisoned env lock and make `isolated()` return `TestResult` so callers propagate with `?`. Gate green. Commit `b6b7791`. | Honest baseline; resume-safe checkpoints require a green gate. | 00.3, config tests, b6b7791 |
| D008 | 2026-06-04 | Replan records-and-halts; no in-loop plan regeneration | On replan-cap exhaustion `resume_one_step` appends to `DECISIONS.md` and returns the step uncommitted ("queued for replanning"); it does **not** regenerate `PROGRESS.md` in the loop. Plan regeneration stays the existing `plan --replan` command. `MAX_REPLANS` is an in-crate constant (no config field); `today()`/civil-date is in-crate (no date dep). | Keeps the highest-risk core-loop edit (auto-rewriting the plan mid-resume) out of scope while still making the deviation durable. YAGNI on a config knob and a date crate. | 05.2, 05.3, resume.rs, decisions.rs |

---

## 5. Master progress tracker

| Done | # | File | Status | Owner summary | Human actions mirrored? |
|---|---|---|---|---|---|
| [x] | 00 | `tasks/quality-gate/00-tooling-research-and-readiness.md` | DONE | agent: 8 | n/a |
| [x] | 01 | `tasks/quality-gate/01-checks-config.md` | DONE | agent: 4 | n/a |
| [x] | 02 | `tasks/quality-gate/02-profiles-and-discovery.md` | DONE | agent: 5 | n/a |
| [x] | 03 | `tasks/quality-gate/03-check-execution.md` | DONE | agent: 4 | n/a |
| [x] | 04 | `tasks/quality-gate/04-quality-gate-rule.md` | DONE | agent: 4 | n/a |
| [x] | 05 | `tasks/quality-gate/05-act-on-findings.md` | DONE | agent: 4 (05.1=D007) | n/a |
| [ ] | 06 | `tasks/quality-gate/06-ratification-and-surface.md` | TODO | agent: 5; product-owner: 1 | TBD |

---

## 6. Cross-cutting principles

1. KISS · YAGNI · CLEAN · SOLID · DRY (in that order when conflicting).
2. **Clean-room provenance is blocking** — original code/prompts/tests; official
   public APIs or local servers only. See `clean-room-guard`, `docs/00-clean-room.md`.
3. **Rust rules hold** (docs/13): MSRV 1.82, exact-pinned workspace deps, typed
   errors per crate, `#![forbid(unsafe_code)]`, no `unwrap`/`expect`/`panic!` on
   library runtime paths, cross-platform path/shell discipline.
4. **Tier-1 parity** (ADR-0007): a check that only works on one OS is not done.
   Profiles must classify Windows and POSIX commands correctly.
5. **Security is not optional** (ADR-0009, docs/07): discovered commands are
   untrusted. Discovery proposes; the user ratifies into committed config; every
   check runs through the permission engine and sandbox. No standing bypass.
6. **The engine stays stack-neutral.** Profiles are the fixed abstraction;
   commands/versions/paths are discovered. No hardcoded tool list inside the rule
   engine.
7. **Findings are data, not crashes.** A failing check is a finding mapped to a
   verdict, never a process panic (mirrors the tool result model, docs/05).
8. **The harness never blind-edits logic** to satisfy a check; it fixes via a
   declared `fix_command` or feeds the failure back through the loop.
9. Every box has an owner (`agent` unless human sign-off) and a stable ID.
10. Lessons land in `tasks/quality-gate/lessons.md` as they happen.
11. **Plan-agnostic output** (§6.11): commits/identifiers/comments/tests carry no
    box IDs, decision IDs, `slice`, or plan filenames. Put the *why* in the
    comment or an ADR.
12. **Captain Hindsight** before each subject close (§ Appendix in the template).
13. Tooling research (subject 00) before implementation subjects start.
14. Resume-safe checkpoints: plan update → gate → commit → push.

---

## 7. Gate review (run last; tick everything)

- [ ] All §5 subjects done (or `ABANDONED` with a §4 row)
- [ ] Subject 00 completed
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo test --workspace` passes
- [ ] `cargo check --workspace` clean
- [ ] `cargo machete` clean (deps changed); `cargo deny check` + `cargo audit`
      clean (release milestone)
- [ ] §6 principles reviewed; cross-platform check classification tests hold
- [ ] Every non-abandoned subject has a Captain Hindsight checkpoint `CLOSE`
- [ ] Durable decisions promoted to ADRs in `docs/10-decisions.md`
- [ ] Shipped code/tests/commits are plan-agnostic (grep excluding `tasks/`)
- [ ] `tasks/quality-gate/manual-actions.md` resolved or deferred
- [ ] `tasks/quality-gate/lessons.md` reconciled; lasting lessons migrated to
      `tasks/lessons.md` before the folder is deleted
- [ ] Plan handed to reviewer for §8 sign-off

---

## 8. Acceptance / sign-off

| Date | Reviewer | Result | Notes |
|---|---|---|---|
| | | | |
