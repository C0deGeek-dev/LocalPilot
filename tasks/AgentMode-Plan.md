# AgentMode-Plan.md — Bridge agent mode to a full-fledged agentic CLI

> **Disposable.** This plan and its `tasks/agent-mode/` folder are deleted once
> the work ships. Shipped code, comments, tests, identifiers, and commit messages
> must not reference the plan, subjects, box IDs, decision IDs, or `slice` — see
> §6.

## 1. Subject

Bring **agent mode** (the `chat` / `print` conversational loop) up to the
practical capability of established agentic coding CLIs: a strong, original
system-prompt and tool-use scaffold; a broader, well-described tool surface;
robust handling of local and hosted providers (env-var compatibility, timeouts,
thinking/`<think>` handling); tuned context management; and a real
end-to-end evaluation against a capable model. The architecture (tool loop,
permission engine, recovery, providers) already exists — this plan closes the
**proof-and-polish** gap so a strong model reliably reads, edits, runs, and
finishes real repo tasks.

**Out of scope:** the harness (rule-enforced) mode beyond what agent mode shares;
the LocalMind learning subsystem (already integrated); multimodal/image input;
sub-agent orchestration beyond a single optional delegation primitive (§5 may
defer it); any GUI. No new operating mode.

## 2. Authoritative inputs

| Source | Contribution |
|---|---|
| `docs/01-product-spec.md`, `docs/06-harness-spec.md` | Agent-mode definition, shared loop, operating-mode boundary |
| `docs/04-provider-contract.md`, `docs/05-tool-system.md` | Provider trait + tool trait contracts the new work extends |
| `docs/00-clean-room.md`, `AGENTS.md`, ADR-0004/0005 | Clean-room provenance rules; the read-only behavior reference policy |
| `crates/unshackled-harness/src/session.rs` | The existing `run_turn` loop, recovery, compaction to extend |
| Read-only behavior reference (`AGENTS.md`-documented path) | **Black-box behavior only.** Observable capabilities, workflows, and edge cases of a mature agent CLI. Not a source of prompts, code, schemas, identifiers, tests, file structure, UI copy, or branding. |
| Official Anthropic / OpenAI API docs | The documented env-var and request conventions to support (e.g. `ANTHROPIC_BASE_URL`) |
| `<base>` | TBD (record the branch/commit this plan branches from) |

### Behavior distillation workflow

Use the read-only behavior reference as a map of product expectations, not as a
source artefact:

1. Prefer black-box observation: run workflows, trigger failures, inspect
   user-visible behavior, and note only what happened at the capability level.
2. Convert observations into neutral requirement notes under
   `tasks/agent-mode/behavior-requirements.md`. Requirements describe observable
   outcomes, not text, identifiers, source layout, prompt structure, tool schema
   shape, or implementation approach.
3. Implement from this repository's architecture, original design, and official
   public documentation. Do not keep the reference open while authoring prompts,
   schemas, tests, user-facing text, or code.
4. Test against the neutral requirements and this repo's specs. Do not snapshot
   or match reference output.
5. When a requirement was informed by the reference, record provenance in the
   Decision log or subject progress log at the behavior level only.

### Maturity benchmark workflow

The mature fork is the product benchmark for the maturity envelope, not an
implementation source. Subject 05 must prove that Rust agent mode carries
forward the hard-earned behavioral lessons:

1. Build a scenario catalog from real mature-fork usage history, dogfood tasks,
   and black-box observation. Include happy paths and failure paths.
2. Classify each scenario by failure mode: malformed tool call, stalled loop,
   context loss, bad edit, permission denial, timeout, provider parse issue,
   final-answer pollution, local-model quality limit, or user interruption.
3. Map every scenario to one of: automated regression test, live eval task,
   manual dogfood check, or explicitly deferred limitation.
4. Run Rust agent mode and the mature fork through representative task classes
   and compare outcomes only. Do not compare output wording, prompts, schemas,
   internal traces, or implementation shape.
5. Treat maturity gaps as blocking unless they are recorded as accepted
   limitations with a concrete rationale.

## 3. Subject file index

| # | File | Subject |
|---|---|---|
| 00 | `tasks/agent-mode/00-tooling-research-and-readiness.md` | Tooling research and readiness |
| 01 | `tasks/agent-mode/01-system-prompt-and-loop.md` | Original agent system prompt + tool-use scaffold |
| 02 | `tasks/agent-mode/02-tool-surface.md` | Tool-surface expansion |
| 03 | `tasks/agent-mode/03-provider-runtime.md` | Provider runtime: env compat, timeouts, thinking |
| 04 | `tasks/agent-mode/04-context-management.md` | Context, compaction, and long-session handling |
| 05 | `tasks/agent-mode/05-evaluation.md` | Live validation + maturity benchmark |

## 4. Decision log

| ID | Date | Title | Decision | Rationale | Refs (box IDs / files) |
|---|---|---|---|---|---|
| D001 | 2026-06-03 | Defer network fetch tool | Deferred. | The existing command/tool surface can perform approved network operations through classified shell commands. A first-class fetch tool needs a separate network policy and enablement story, so it should not be bundled into this polish pass. | 02.4 |
| D002 | 2026-06-03 | Defer sub-task delegation primitive | Deferred. | Single-agent tool use, recovery, provider runtime, and context handling are the current maturity gap. Delegation would introduce orchestration behavior outside the scoped operating mode. | 02.5 |
| D003 | 2026-06-03 | Defer MCP and project-skill setup | Deferred. | Current shell, cargo, rg, git, and focused tests cover this polish pass. Adding MCP servers or repo skills now would increase review surface without closing the remaining live-provider and eval maturity gaps. | 00.7, 00.8 |
| D004 | 2026-06-05 | Defer live gateway verification | Deferred. | Provider setup, env-var fallback, timeout, endpoint normalization, and thinking handling are documented and covered by deterministic tests. Live OpenAI-compatible and Anthropic-compatible gateway runs are environment-dependent and will continue as normal development validation rather than blocking this plan close. | 03.6, `docs/providers.md` |
| D005 | 2026-06-05 | Defer live maturity benchmark | Deferred. | Offline golden tasks, behavior requirements, failure taxonomy, and the opt-in live runner exist. Real hosted/local model runs and black-box maturity comparison need credentials and local model availability, so they are accepted ongoing-development work instead of a blocker for rounding off the plan. | 05.5, 05.6, 05.7, 05.8, `tasks/agent-mode/manual-actions.md` |
| D006 | 2026-06-05 | Defer dependency hygiene findings | Deferred. | `cargo deny check` passes, but the final local closeout found `cargo audit` red on the vendored LocalMind `time` pin and `cargo machete` red on LocalMind's placeholder MCP crate. Updating the vendored submodule is a separate dependency-maintenance action, not part of agent-mode proof-and-polish. | §7 gate, `external/localmind` |

## 5. Master progress tracker

| Done | # | File | Status | Owner summary | Human actions mirrored? |
|---|---|---|---|---|---|
| [x] | 00 | `tasks/agent-mode/00-tooling-research-and-readiness.md` | DONE | agent: 9 done | n/a |
| [x] | 01 | `tasks/agent-mode/01-system-prompt-and-loop.md` | DONE | agent: 6 done | n/a |
| [x] | 02 | `tasks/agent-mode/02-tool-surface.md` | DONE | agent: 4 implemented; 2 deferred by §4 decision | n/a |
| [x] | 03 | `tasks/agent-mode/03-provider-runtime.md` | DONE | agent: provider docs/tests done; live gateway verification deferred by D004 | n/a |
| [x] | 04 | `tasks/agent-mode/04-context-management.md` | DONE | agent: 4 done | n/a |
| [x] | 05 | `tasks/agent-mode/05-evaluation.md` | DONE | agent: offline eval/docs + live runner done; live runs and maturity benchmark deferred by D005 | yes |

## 6. Cross-cutting principles

> Violations are blockers, not nits.

1. KISS · YAGNI · CLEAN · SOLID · DRY (in that order when conflicting).
2. **Clean-room provenance is blocking and is the defining constraint of this
   plan.** The behavior reference is a *patched third-party product*: consult it
   only for **observable behavior**, never as a source. Do **not** copy,
   paraphrase, translate, port, summarize, or mechanically transform its prompts,
   code, tests, identifiers, file structure, UI copy, schema wording, or
   branding. Every prompt, tool, test, and message shipped here is written from
   first principles and original to this repo. Where the reference informed a
   design decision, record a provenance note in the Decision log naming *what
   behavior* was matched — never *what text* was read. See
   `docs/00-clean-room.md`.
3. **Official public APIs and local servers only.** No private or undocumented
   endpoints. Env-var/header conventions adopted must be the documented public
   ones (e.g. the Anthropic SDK's `ANTHROPIC_BASE_URL`), used as integration
   labels, not as product identity.
4. **Engineering rules from `docs/13-rust-best-practices.md` hold** — MSRV 1.82,
   exact-pinned deps, `#![forbid(unsafe_code)]`, typed per-crate errors, no
   `unwrap`/`expect`/`panic!` on library runtime paths, cross-platform discipline.
5. **Keep modules small and locally understandable.** Split a prompt/tool/loop
   file before it becomes a dumping ground; cover coordinated behavior with tests.
6. **Low cyclomatic complexity.** Prefer guard clauses, extracted decision
   helpers, and table-driven cases over deep branching.
7. **Spec at the contract level, not the SDK level.** State what must be
   observably true (the model completes the task, the tool is gated, the flood is
   aborted), not how to call a crate.
8. **Tests pin observable behavior, not implementation.** Each test prevents a
   nameable bug; offline `FakeProvider`/`wiremock` for determinism, with the live
   path validated separately in subject 05.
9. **Every box has an owner and a stable ID** (`<subject-id>.<box-number>`).
10. **Lessons land in `tasks/agent-mode/lessons.md` as they happen.**
11. **Code is plan-agnostic.** No box IDs, `D###`, `slice`, or plan references in
    shipped code, comments, identifiers, or commit messages.
12. **Captain Hindsight review before each subject close** (verdict `CLOSE`).
13. **Tooling research before implementation** — subject 00 first.
14. **Capability coverage across Windows, Linux, macOS** (ADR-0007); the
    interactive paths are validated where they can run (TUI on MSVC).

## 7. Gate review (run last; tick everything)

- [x] All §5 subjects `DONE` (or `ABANDONED` with a §4 row)
- [x] Subject 00 completed or waived with a §4 row
- [x] Behavior requirements, if any, are neutral, observable, and provenance-noted
      in `tasks/agent-mode/behavior-requirements.md`
- [x] Maturity benchmark scorecard passed, or every gap is fixed, tested, or
      recorded as an accepted limitation with rationale
- [x] `cargo check --workspace`, `cargo build --workspace` pass locally; 3-OS
      coverage remains the normal CI validation path
- [x] `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` clean
- [x] `cargo test --workspace` green locally; Windows/Ubuntu/macOS parity remains
      the normal CI validation path
- [x] `cargo deny check` passes; `cargo audit` and `cargo machete` findings are
      recorded as accepted dependency-maintenance follow-up by D006
- [x] §6 cross-cutting reviewed; agent-mode behavior tests hold
- [x] Every non-abandoned subject has a recorded Captain Hindsight checkpoint `CLOSE`
- [x] **Clean-room audit**: text scan for prohibited framing and for any prompt,
      identifier, or UI string traceable to the behavior reference; provenance
      notes present where the reference was consulted
- [x] Shipped artefacts plan-agnostic — grep (excluding `tasks/`) for box IDs,
      `\bD\d{3}\b`, `tasks/agent-mode/`, `AgentMode-Plan.md`, `\bslices?\b`
- [x] Commit messages plan-agnostic
- [x] `tasks/agent-mode/manual-actions.md` — every human-owned box resolved or deferred
- [x] **Live-provider validation passed or explicitly deferred** (subject 05):
      agent mode completes representative eval and dogfood tasks against a
      capable hosted model and a capable local model, or the live proof gap is
      recorded as an accepted limitation. Deferred by D005.
- [x] `tasks/agent-mode/lessons.md` reconciled; lasting lessons migrated to `tasks/lessons.md`
- [x] Plan handed to reviewer for §8 sign-off

## 8. Acceptance / sign-off

| Date | Reviewer | Result | Notes |
|---|---|---|---|
| 2026-06-05 | David | Accepted with deferrals | Agent-mode proof-and-polish accepted as implemented. Live gateway verification, hosted/local live evals, and black-box maturity comparison are deferred by D004/D005 and will continue during normal development. |
