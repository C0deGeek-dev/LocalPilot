# Interactive Streaming Reliability

## 1. Subject

Make interactive chat input remain editable and cancellable while a model turn is
running, and make local OpenAI-compatible streaming robust enough to avoid false
degenerate-output recovery and unexplained truncation. Provider-neutral contracts,
permission behavior, and unrelated TUI features are out of scope.

## Collaboration model

| Field | Value |
|---|---|
| Mode | `solo` |
| Primary owner | agent |
| Coordinator | agent |
| Resume safety | required |
| Parallel branches | no |
| Notes | User requested an end-to-end fix; no human-owned actions identified. |

## 2. Authoritative inputs

| Source | Contribution |
|---|---|
| `AGENTS.md` | Planning and clean-room constraints |
| `docs/04-provider-contract.md` | Provider stream and error contract |
| `docs/08-testing.md` | Cancellation and malformed-stream coverage |
| User report dated 2026-06-04 | Observable failures and acceptance targets |

## 3. Subject file index

| # | File | Subject |
|---|---|---|
| 00 | `tasks/interactive-streaming-reliability/00-tooling-research-and-readiness.md` | Tooling research and readiness |
| 01 | `tasks/interactive-streaming-reliability/01-terminal-interaction.md` | Responsive input and cancellation |
| 02 | `tasks/interactive-streaming-reliability/02-local-streams.md` | Local compatible stream handling |
| 03 | `tasks/interactive-streaming-reliability/03-verification.md` | Regression verification |
| 04 | `tasks/interactive-streaming-reliability/04-review.md` | Review and simplification |

## 4. Decision log

| ID | Date | Title | Decision | Rationale | Refs |
|---|---|---|---|---|---|
| D001 | 2026-06-04 | No behavior reference | Use repository code, specs, and the user report only. | Local reference is unnecessary for conventional terminal and SSE behavior. | all |
| D002 | 2026-06-04 | Buffer active-turn input | Keep text typed during a running turn in the editor, but do not submit it until the turn ends. | This restores responsive drafting and editing without interleaving concurrent conversation turns. | 01 |
| D003 | 2026-06-04 | Require complete content lifecycle | Treat a compatible Anthropic stop reason as successful EOF only after every started content block has closed. | A stop reason alone can otherwise silently accept the short, mid-word responses observed from LocalBox. | 02 |

## 5. Master progress tracker

| Done | # | File | Status | Owner summary | Human actions mirrored? |
|---|---|---|---|---|---|
| [x] | 00 | `tasks/interactive-streaming-reliability/00-tooling-research-and-readiness.md` | DONE | agent: 4 | n/a |
| [x] | 01 | `tasks/interactive-streaming-reliability/01-terminal-interaction.md` | DONE | agent: 3 | n/a |
| [x] | 02 | `tasks/interactive-streaming-reliability/02-local-streams.md` | DONE | agent: 3 | n/a |
| [x] | 03 | `tasks/interactive-streaming-reliability/03-verification.md` | DONE | agent: 3 | n/a |
| [x] | 04 | `tasks/interactive-streaming-reliability/04-review.md` | DONE | agent: 3 | n/a |

## 6. Cross-cutting principles

1. Keep provider handling provider-neutral and based only on documented OpenAI-compatible SSE shapes.
2. Do not lose user input, completed model text, or cancellation signals.
3. Bound terminal redraw frequency independently from model event frequency.
4. Preserve Windows, Linux, and macOS terminal behavior.
5. Add observable regression tests for each reported failure where practical.

## 7. Gate review

- [x] All subjects done
- [x] `cargo fmt --check`
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
- [x] `cargo test --workspace`
- [x] `cargo check --workspace`
- [x] Review and simplification complete

## 8. Acceptance / sign-off

| Date | Reviewer | Result | Notes |
|---|---|---|---|
| 2026-06-04 | agent | PASS | Workspace and TUI feature gates passed; live LocalBox replay unavailable because the configured endpoint was offline. |
