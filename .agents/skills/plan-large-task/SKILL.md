---
name: plan-large-task
description: >-
  Choose and run the right planning ceremony for a build task in this repo.
  Small tasks plan in-session with EnterPlanMode; large multi-slice tasks copy
  the canonical plan template (c0degeek-ai plan-from-template skill) into
  tasks/<Name>-Plan.md, splice in the repo overrides file, and run with
  subjects, a decision log, resume-safe checkpoints, and a Captain Hindsight
  review at each subject close. Use when starting any multi-step build effort
  and you must decide how heavy the plan should be.
---

# plan a large task

This skill routes a build effort to the right *planning weight*. It is
developer-process tooling, not product behaviour — keep it separate from the
product harness's own `localpilot harness plan` (`brief.md` / `PROGRESS.md`),
which is spec'd in
[`docs/06-harness-spec.md`](../../../docs/06-harness-spec.md).

## Tier trigger — pick S or L

**Tier L (use the canonical template)** if **any** of these hold:

- spans 3+ crates, or needs a new crate
- likely to outlast one session / survive a context-window reset
- touches a non-negotiable surface: sandbox, permission engine, secret
  redaction, the provider trait, or the tool trait
- needs a new ADR in [`docs/10-decisions.md`](../../../docs/10-decisions.md)

**Tier S (no template)** otherwise — 1-2 files, single session, no new crate.
Plan in-session with `EnterPlanMode`, implement, then review with `/code-review`
and `/simplify`. Do not create `tasks/` files for Tier S.

When unsure, it is Tier S. Don't manufacture ceremony.

## Template source

The plan template is **not** bundled in this repo. The canonical copy lives in
the c0degeek-ai repository at `skills/plan-from-template/plan-template.md`
(github.com/David-c0degeek/c0degeek-ai). In Claude Code it is available via
the installed `c0degeek` plugin's `plan-from-template` skill; other tools read
it from a local checkout of that repo. If neither is reachable, the
`plan-from-template` skill's standalone required shape is the fallback.

Repo-specific content is spliced from
[`plan-template-overrides.md`](plan-template-overrides.md) (next to this
file) — never by editing or forking the canonical template. Generic
improvements go upstream to c0degeek-ai.

## Tier L procedure

1. Copy the canonical template to `tasks/<Name>-Plan.md` and follow its "How
   to use" steps. The template embeds the subject/box/slice shape, decision
   log, master tracker, gate, and the Captain Hindsight prompt.
2. Splice [`plan-template-overrides.md`](plan-template-overrides.md) into the
   copy at the extension points it names (purpose notes, §2 verification
   rows, §6 principles, §7 gates, Hindsight prompt lines).
3. Start in `solo` collaboration mode unless multiple workers are known now.
4. Subject `00` (tooling research) runs first unless waived by a §4 decision.
5. Keep checkpoints resume-safe: update plan files, run the gate, commit, push.

## Repo-specific rules (also in the overrides file)

- **Gate per checkpoint** comes from the plan's §2 Verification-commands
  table — the single source for checkpoint and §7 gate commands. Repo defaults
  (mirroring CI): `cargo check --workspace`, `cargo test --workspace`,
  `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D
  warnings`. Hygiene (`cargo machete` on dep change; `cargo deny check` /
  `cargo audit` before a release milestone) is not per-checkpoint. Subject
  00.3 confirms the table against the real repo.
- **Risks-and-rollback table (§1) and Depends-on column (§3) are mandatory**
  at plan creation; `n/a` risks need a one-line reason, and the dependency
  graph must stay acyclic.
- **ADR promotion.** A decision-log row dies with the plan folder. Durable
  architecture decisions graduate to a real ADR in docs/10 and are cited by
  number. Transient build-sequencing choices stay in §4.
- **Plan-agnostic output (§6.11).** Commits, identifiers, comments, and tests
  must not reference box IDs, decision IDs, `slice`, or the plan file — the
  `tasks/` folder is deleted before v1. Put the *why* in the comment or an ADR.
- **Name clash.** Never name a build-plan file `PROGRESS.md` or `brief.md` —
  reserved for the product harness runtime.
- **Clean-room still applies** ([[clean-room-guard]]): everything the plan
  produces must stay clean-room compliant.

## Captain Hindsight

Run the embedded prompt (template Appendix, plus the overrides-file extra
lines) at each subject close (§6.12), not per box. Record Keep / Fix before
closing / Record / Risk / Verdict. A `DO NOT CLOSE` verdict is a blocker. For
Tier S, `/code-review` + `/simplify` is the lighter equivalent.
