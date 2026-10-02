# Reference

This wiki does not duplicate the in-repo specification. Reference material is
indexed in [`docs/README.md`](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/README.md),
which maps every area to its owning doc.

> **Do not edit on github.com.** This wiki is generated from in-repo Markdown
> under `docs/wiki/` and synced one-way on every push to `main`. Edit the source
> in `docs/wiki/`; web edits are overwritten on the next sync.

## LocalMind CLI contracts

- **Search output format** — non-terminal stdout returns JSON by default, a
  terminal returns the human table, and `--format human|json` overrides either way
  (ADR-0048). See
  [`docs/configuration.md`](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/configuration.md#project-context-files).
- **Store resolution** — `learning`/`memory` walk up to the nearest ancestor
  `.localmind` store; `--workspace <path>` pins it. See
  [`docs/localmind-integration.md`](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/localmind-integration.md#store-resolution).

## Research

- **Research surface** — `/research` (interactive) and `localpilot research`
  (headless) drive one local-first loop that writes a report and review-gated
  memory candidates (ADR-0060). Configure it under `[research]`; see
  [`docs/configuration.md`](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/configuration.md).
- **Web egress** — on by default with open-web reach (ADR-0076): disclosed on
  every run, allowlist/disallowlist-gated, audited, and disableable per run
  (`--no-web`) or globally (`[research.web].enabled = false`). Both surfaces
  (interactive and headless) share the posture. See
  [`docs/07-security-and-privacy.md`](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/07-security-and-privacy.md).

## Lesson lab

Lessons that come out of a finished harness run can be tested before a person
accepts them. Specified in
[the harness specification](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/06-harness-spec.md#completion-hindsight);
what each tier may do on the machine is in
[security and privacy](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/07-security-and-privacy.md#lesson-lab-execution).

| Command | What it does |
|---|---|
| `localpilot lab list` | The lessons the lab classified, what it can run for each, their results and open rerun requests |
| `localpilot lab replay [<lesson>]` | Re-run the project's own check on the commits a lesson came from, in temporary worktrees. Needs `[lab] replay = true` in the committed `.localpilot.toml`; shows what will run and asks first |
| `localpilot lab tasks draft\|show\|approve <lesson>` | A model drafts test questions for a lesson; a named person approves them. Nothing runs from a draft |
| `localpilot lab uplift <lesson> --model <m>` | Run the approved questions with and without the lesson through LocalBench. Drives real model sessions. Needs `[lab] uplift = true` in the committed `.localpilot.toml`; shows its ceilings and commands and asks first |
| `localpilot lab status` | Each uplift run: ended, running or interrupted |
| `localpilot lab rerun <lesson> --tier replay\|uplift --reviewer <name>` | Record a request to run a tier again. Runs nothing |
| `localpilot learning review show <item>` | The lesson with its hindsight and lab results, and what you can do next |
| `localpilot learning review edit <item> --replacement "…"` | Rewrite a lesson. The original is kept as history with its results; the rewrite starts untested |
| `localpilot learning review split draft\|show\|approve <item>` | A model drafts narrower lessons; a named person approves them |

A lab result is shown to the reviewer and never accepts or changes a lesson.
`<lesson>` is the lesson's identity from `lab list` (or a prefix of it);
`<item>` is its id in `learning review list`.

## Pair collaboration

- **Command and cost controls** — see the canonical
  [`configuration.md`](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/configuration.md#pair-collaboration).
- **Topology, messaging, terminal, and security contracts** — follow the owner
  links in the
  [`docs/README.md` map](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/README.md#pair-collaboration-ownership).

## Interactive harness

`/harness` guides brief and plan review before confirmed execution. The direct
`/harness-*` command family and cancellation/compatibility contract are documented
in [the harness specification](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/06-harness-spec.md#guided-harness-and-direct-lifecycle-commands).

## Automatic work sizing

Agent and harness runtimes share context-pressure and observed-reliability
profiles. See [the work-unit contract](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/06-harness-spec.md#automatic-work-granularity)
and [stricter configuration caps](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/configuration.md#harnessgranularity).
