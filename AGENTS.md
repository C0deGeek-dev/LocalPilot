# Agent Notes

Treat this repository's tracked source, tests, and documentation as the complete
implementation reference. Keep implementation, prompts, tests, private endpoint
details, identifiers, UI copy, and branding original to this repository.

Do not inspect or modify files outside this checkout unless the user explicitly
places them in scope.

## Planning your work

Plan at the right weight. Small changes can use an in-session plan; large or
multi-session changes can use the `plan-large-task` skill with an ignored local
`tasks/` workspace and resume-safe checkpoints. Never commit those transient
plans to the distribution. Durable architecture decisions belong in
[`docs/10-decisions.md`](docs/10-decisions.md). Never name a build-plan file
`PROGRESS.md` or `brief.md` — those belong to the product harness runtime.
