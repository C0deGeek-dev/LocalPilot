# Agent Notes

When working in this repository, `D:\repos\unshackled` may be inspected as a
read-only behavior reference when local docs are incomplete.

Use it only to understand workflows, command behavior, configuration shape,
user-facing edge cases, and high-level requirements. Keep implementation,
prompts, tests, private endpoint details, identifiers, UI copy, and branding
original to this repository.

Do not modify files outside this workspace unless the user explicitly asks for
that.

## Planning your work

Plan at the right weight. Small change (1-2 files, one session, no new crate):
`EnterPlanMode` in-session, then `/code-review` + `/simplify`. Large change (3+
crates, a new crate, multi-session, a non-negotiable surface, or a new ADR): use
the `plan-large-task` skill — copy its template to `tasks/<Name>-Plan.md` and
run subjects with resume-safe checkpoints and a Captain Hindsight review at each
subject close. The trigger and rules live in
[`docs/14-dev-tooling.md`](docs/14-dev-tooling.md) §7.

`tasks/` holds disposable build-plan files (deleted before v1); shipped code and
commits stay plan-agnostic, and durable architecture decisions are promoted to
ADRs in [`docs/10-decisions.md`](docs/10-decisions.md). Never name a build-plan
file `PROGRESS.md` or `brief.md` — those belong to the product harness runtime.
