# Interactive Reliability Plan

## Collaboration model

| Field | Value |
|---|---|
| Mode | `solo` |
| Primary owner | agent |
| Resume safety | required |
| Parallel branches | no |
| Notes | Preserve unrelated harness worktree changes. |

## 1. Subject

Fix interactive input editing/caret visibility and prevent visibly truncated,
mid-word model responses from being accepted as complete. Provider protocol
redesign and broad TUI refactoring are out of scope.

## 2. Authoritative inputs

| Source | Contribution |
|---|---|
| User report and recorded `.unshackled` session | Reproduction and acceptance behavior |
| `docs/01-product-spec.md` | Conservative bad-output recovery requirement |
| `docs/02-architecture.md` | TUI/runtime ownership boundaries |
| `docs/08-testing.md` | Focused fake-provider and render tests |
| `docs/00-clean-room.md` | Original implementation/provenance constraints |

## 3. Subject file index

| # | File | Subject |
|---|---|---|
| 00 | `tasks/interactive-reliability/00-tooling-research-and-readiness.md` | Tooling research and readiness |
| 01 | `tasks/interactive-reliability/01-input-editing.md` | Input editing and caret |
| 02 | `tasks/interactive-reliability/02-response-completion.md` | Response completion |
| 03 | `tasks/interactive-reliability/03-verification.md` | Verification and review |

## 4. Decision log

| ID | Date | Title | Decision | Rationale | Refs |
|---|---|---|---|---|---|
| D001 | 2026-06-04 | Require protocol completion | Treat transport EOF without a protocol completion marker as a failed stream rather than guessing from response text. | This directly addresses the recorded mid-word endings without rejecting valid short replies. | 02.1 |

## 5. Master progress tracker

| Done | # | File | Status | Owner summary | Human actions mirrored? |
|---|---|---|---|---|---|
| [x] | 00 | `tasks/interactive-reliability/00-tooling-research-and-readiness.md` | DONE | agent: 3 | n/a |
| [x] | 01 | `tasks/interactive-reliability/01-input-editing.md` | DONE | agent: 3 | n/a |
| [x] | 02 | `tasks/interactive-reliability/02-response-completion.md` | DONE | agent: 3 | n/a |
| [x] | 03 | `tasks/interactive-reliability/03-verification.md` | DONE | agent: 3 | n/a |

## 6. Cross-cutting principles

1. Preserve unrelated worktree changes.
2. Keep implementation original and provider-neutral.
3. Maintain UTF-8 character boundaries for every input cursor edit.
4. Do not classify ordinary short replies as truncated.
5. Pin observable behavior with focused tests.

## 7. Gate review

- [x] `cargo fmt --check`
- [x] Focused TUI, CLI, LLM, and harness tests pass
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
- [x] `cargo test --workspace`
- [x] `cargo check -p unshackled --features tui`
- [x] Code review and simplification pass complete

## 8. Acceptance / sign-off

| Date | Reviewer | Result | Notes |
|---|---|---|---|
| 2026-06-05 | David | Accepted | Stale fmt/clippy blockers cleared by fresh local gate; focused tests and workspace checks were already recorded. |
