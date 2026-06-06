# LocalMind Integration Contract

## Why

Learning (candidate lessons, review queues, memory promotion, retrieval, skill
generation and maintenance, audit, self-improvement) is a coherent capability
that should not be re-implemented inside every coding agent. LocalMind owns that
core as a standalone engine usable by native hosts and generic transcript
workflows.

LocalPilot is LocalMind's first native host. The LocalMind crates are bundled
into the LocalPilot binary through `localpilot-localmind`; users do not install
LocalMind separately.

## Ownership Boundary

- **LocalMind core is host-neutral and must not depend on LocalPilot.** It owns
  session closeout, redaction-on-import, summarization, candidate-lesson
  extraction, the review queue, accepted-lesson persistence, Markdown-backed
  memory with a SQLite audit/search index, agent-ready context export, and
  `SKILL.md` draft emission.
- **LocalPilot owns the native host role.** It captures session evidence,
  enforces permissions and redaction before persistence, drives TUI/CLI
  surfaces, and adapts LocalPilot session records into LocalMind contracts.

## Bundling

LocalMind is vendored as a git submodule at `external/localmind` and excluded
from the LocalPilot workspace because it is its own workspace. The
`localpilot-localmind` adapter depends on `localmind-core` and `localmind-store`
by path.

```sh
git clone --recurse-submodules <repo>
git submodule update --init --recursive
```

CI checks out submodules recursively. The adapter is a one-way edge: LocalPilot
depends on LocalMind, never the reverse.

## Current Surfaces

- `localpilot-localmind::closeout_session` imports an LocalPilot transcript into
  LocalMind, extracts candidate lessons, and enqueues them for review.
- `localpilot learning` exposes the rich LocalMind loop: `closeout`, `review`,
  `promote`, `search`, `skills`, and `audit`.
- `localpilot memory` uses LocalMind accepted memory for status, inspect, search,
  delete, and context-injection disable.
- Agent turns seed relevant accepted LocalMind memory as best-effort context.
- Interactive sessions close out into LocalMind on exit.

State is project-local under `.localmind/`. Durable memory is readable Markdown;
queue, audit, and search index state live in SQLite.

## Signal Mapping

| LocalPilot signal | LocalMind use |
| --- | --- |
| Session transcript bundle | imported, redacted session for summarization |
| Tool events in transcript | evidence for lesson extraction |
| Code diffs and commits | future durable outcome anchors |
| Test output and quality gate results | future pass/fail signal attached to lessons |
| Recovery events | future frequent-failure candidate lessons |
| Accepted memory | LocalMind retrieval and context injection |
| Skill drafts | LocalMind disabled `SKILL.md` draft emission |

All capture stays redacted-before-persistence and inside the permission boundary;
LocalMind never bypasses either.

## Commands

```sh
localpilot learning closeout --session <id>
localpilot learning review list
localpilot learning review accept <item-id>
localpilot learning promote <item-id>
localpilot learning search "<query>"
localpilot memory inspect
localpilot memory delete <memory-id>
```

New rich-learning behavior lands in LocalMind, not by expanding host-local memory
implementations.
