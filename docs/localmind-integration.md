# LocalMind Integration Contract

## Why

Learning (candidate lessons, review queues, memory promotion, retrieval, skill
generation and maintenance, audit, self-improvement) is a coherent capability
that should not be re-implemented inside every coding agent. LocalMind owns that
core as a standalone engine usable by native hosts and generic transcript
workflows.

Unshackled is LocalMind's first native host. The LocalMind crates are bundled
into the Unshackled binary through `unshackled-localmind`; users do not install
LocalMind separately.

## Ownership Boundary

- **LocalMind core is host-neutral and must not depend on Unshackled.** It owns
  session closeout, redaction-on-import, summarization, candidate-lesson
  extraction, the review queue, accepted-lesson persistence, Markdown-backed
  memory with a SQLite audit/search index, agent-ready context export, and
  `SKILL.md` draft emission.
- **Unshackled owns the native host role.** It captures session evidence,
  enforces permissions and redaction before persistence, drives TUI/CLI
  surfaces, and adapts Unshackled session records into LocalMind contracts.

## Bundling

LocalMind is vendored as a git submodule at `external/localmind` and excluded
from the Unshackled workspace because it is its own workspace. The
`unshackled-localmind` adapter depends on `localmind-core` and `localmind-store`
by path.

```sh
git clone --recurse-submodules <repo>
git submodule update --init --recursive
```

CI checks out submodules recursively. The adapter is a one-way edge: Unshackled
depends on LocalMind, never the reverse.

## Current Surfaces

- `unshackled-localmind::closeout_session` imports an Unshackled transcript into
  LocalMind, extracts candidate lessons, and enqueues them for review.
- `unshackled learning` exposes the rich LocalMind loop: `closeout`, `review`,
  `promote`, `search`, `skills`, and `audit`.
- `unshackled memory` uses LocalMind accepted memory for status, inspect, search,
  delete, and context-injection disable.
- Agent turns seed relevant accepted LocalMind memory as best-effort context.
- Interactive sessions close out into LocalMind on exit.

State is project-local under `.localmind/`. Durable memory is readable Markdown;
queue, audit, and search index state live in SQLite.

## Signal Mapping

| Unshackled signal | LocalMind use |
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
unshackled learning closeout --session <id>
unshackled learning review list
unshackled learning review accept <item-id>
unshackled learning promote <item-id>
unshackled learning search "<query>"
unshackled memory inspect
unshackled memory delete <memory-id>
```

New rich-learning behavior lands in LocalMind, not by expanding host-local memory
implementations.
