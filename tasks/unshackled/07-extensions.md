# 07 — Extensions: Quota, MCP, Skills, Memory

## Goal
> Phases 11–14 (`docs/03`) — the v1-committed capabilities beyond the core loop
> (`docs/01` §v1 Committed Scope): quota wait/resume + continuous development
> mode (Phase 13), the MCP client (Phase 14), skills incl. generated drafts
> (Phase 11), and the local memory store (Phase 12). Each reuses the same
> permission/redaction pipeline as builtin tools — no side channels (`docs/02`,
> `docs/12`). Ordering note: memory (12) before skill suggestions (11) because
> suggestions depend on a local usage/memory store (`docs/12` Skill
> Suggestions, `docs/03` Phase 12 "defer graph until flat store proves useful").

## Boxes
> ID = `07.<box-number>`. Owners: agent · tech-lead.

- [x] **07.1** (agent) Implement quota error classification + reset-window
      parsing in `unshackled-quota` using the subject-03 provider metadata
      (`retry_after`/`reset_at`/`limit_kind`/`retryable`/`raw_provider_code`,
      `docs/03` Phase 13, `docs/12`); estimate windows with bounded backoff +
      jitter when the provider returns only prose, re-probing before resuming
      (`docs/07`). (Verified: classify-and-parse tests with fake quota
      responses.)
- [ ] **07.2** (agent) Persist paused run state and implement
      `unshackled harness wait-resume` (`docs/06`, `docs/03` Phase 13). Resume
      only at harness **step boundaries**. (Verified: `docs/08` Harness test —
      quota pause/resume at a step boundary; paused state persisted as an
      inspectable file.)
- [x] **07.3** (agent) Implement the resume **modes** (`docs/12` Quota): `off`,
      `ask`, `run` (per-run auto-resume), `global` (unattended); config keys
      `auto_resume`, `max_wait_minutes`, `resume_requires_clean_workspace`,
      `resume_requires_no_pending_approval`, `resume_only_at_step_boundary`.
      Global unattended resume requires explicit config (`docs/03` Phase 13
      Done-when). (Verified: mode-selection tests; a test that `global` is off
      unless explicitly set.)
- [x] **07.4** (agent) Enforce quota **safety gates** (`docs/07`, `docs/12`):
      never resume through a pending destructive approval; never with dirty
      unrelated workspace; never mid-step; never after user cancellation; never
      if provider identity/config changed during the wait; re-probe after the
      timer; record why it paused and why it resumed; do not frame as bypassing
      limits. (Verified: one test per gate; a fake quota window pauses then
      resumes in tests; permission gates still stop unsafe actions.)
- [x] **07.5** (agent) Implement **continuous development mode** (`docs/01`
      Continuous Development Mode): long-running harness work that pauses cleanly
      on quota/rate limits, records the reset timer, resumes per policy, never
      bypasses permission policy, never continues after destructive pending
      approvals without consent. (Verified: integration test — continuous run
      pauses + resumes across a fake reset window without bypassing approvals.)
- [x] **07.6** (agent) Implement the **MCP client** in `unshackled-mcp`
      (`docs/02` §`unshackled-mcp`, `docs/03` Phase 14): protocol client, server
      lifecycle, tool discovery, resource reads, persisted server configs, server
      health status. (Verified: client handshake + tool-discovery test against a
      scripted/fake MCP server.)
- [x] **07.7** (agent) Route **all** MCP tool calls through the same permission
      checks + redaction as builtin tools — MCP tools behave like builtin tools
      from the model's perspective and permissions apply uniformly; not a side
      channel (`docs/03` Phase 14 Done-when, `docs/02`). (Verified: a permission
      test asserting an MCP write prompts/denies exactly like a builtin write;
      redaction applied to MCP tool output.)
- [x] **07.8** (agent) Define the local **memory file format** (flat,
      inspectable) in `unshackled-memory` (`docs/03` Phase 12, `docs/12`): tagged
      entries (project facts, durable decisions, recurring workflows,
      dependency/architecture notes, frequent failures+fixes, accepted skill
      suggestions). Defer graph/entity extraction until the flat store proves
      useful. (Verified: format round-trip; inspectable plain files.)
- [x] **07.9** (agent) Implement memory **retrieval** with relevance ranking,
      token cap, recency/verified preference, and a relevance threshold below
      which stale entries are not injected; injected memories shown in
      debug/inspect output (`docs/12`, `docs/08` Context). Redact before memory
      writes (`docs/07`). (Verified: `docs/08` Context tests — memory injection
      respects token caps; stale memory not injected below threshold.)
- [x] **07.10** (agent) Implement memory commands (`docs/12`): `memory status`,
      `memory search`, `memory inspect`, `memory delete`, `memory disable`;
      project-level opt-out; explicit first-run consent for global memory
      (`docs/03` Phase 12 Done-when). (Verified: `assert_cmd` tests — inspect
      lists entries; delete removes one; disable stops injection; global memory
      requires consent.)
- [x] **07.11** (agent) Define the Unshackled **skill manifest** (`docs/12`
      Skills): `skills/<name>/{SKILL.md, skill.toml, assets/, scripts/, tests/}`;
      `skill.toml` declares name, description, version, triggers, required tools,
      permissions, assets, scripts. (Verified: manifest parse test; invalid
      manifest reports the bad field.)
- [x] **07.12** (agent) Implement skill **loading** (project-local + user-local),
      exposing skill instructions to the agent, with skill validation and
      permission declarations visible before execution (`docs/03` Phase 11
      Done-when). Trigger semantics: description-based relevance default; optional
      explicit triggers (command names / file globs / regexes); model-judged
      relevance explainable in debug; manual invocation by name (`docs/12`).
      (Verified: a checked-in local skill guides an agent turn; skill permissions
      shown before execution.)
- [x] **07.13** (agent) Implement skill **asset/script** support with permission
      declarations routed through the permission engine (never a bypass,
      `docs/03` Phase 11, `docs/05` safety invariants). (Verified: a skill script
      invocation prompts/denies per its declared permissions.)
- [x] **07.14** (agent) Implement usage-pattern tracking + **generated skill
      drafts** from repeated workflows (same command sequence / setup / error-fix
      loop / prompt template), saved as **disabled drafts** requiring explicit
      user review of content, permissions, and triggers; suggestion cooldown per
      pattern; no silent file creation outside disabled drafts (`docs/12` Skill
      Suggestions, `docs/03` Phase 11 Done-when). Depends on the memory/usage
      store (07.8). (Verified: a repeated pattern produces a disabled draft;
      enabling requires explicit action; cooldown test.)
- [ ] **07.15** (agent) Add the `docs/08` Store tests for these surfaces: persist
      memory store, persist skill drafts, persist quota wait/resume records — all
      redacted, atomic, inspectable. (Verified: round-trip + redaction tests for
      each.)
- [ ] **07.16** (tech-lead) Review the MCP permission integration and any
      shipped/sample skill manifests as security-relevant changes (`docs/14` §5
      trust boundary, `docs/07`): confirm MCP cannot bypass permissions and no
      skill grants a broad tool allowlist. Mirror to `manual-actions.md`.
      (Verified: sign-off noted; uniform-permission test referenced.)


## Hindsight checkpoint
> Run after all boxes in this subject are complete and before marking
> the subject `DONE` in §5. Use the embedded prompt in `tasks/Unshackled-Plan.md`
> "Appendix: Captain Hindsight Prompt". Record the review result here.
>
> Required output sections: Keep; Fix before closing; Record; Risk;
> Verdict (`CLOSE` or `DO NOT CLOSE`). If the verdict is `DO NOT CLOSE`,
> leave the subject open, add/reopen boxes or update decisions/lessons,
> and rerun this checkpoint after the fixes.
>
> Subjects already marked `DONE` before this checkpoint was added still need
> this section completed retroactively before the §7 gate review is ticked.

- [ ] Captain Hindsight review recorded
- [ ] Verdict is `CLOSE`
## Progress log
> One line per slice. Date · slice · box IDs · what shipped · how verified.

- 2026-06-02 · slice 1 · 07.1, 07.3–07.5 · `unshackled-quota`: window estimation
  from subject-03 `QuotaInfo` (retry_after → reset_at → bounded backoff+jitter),
  persistable inspectable `PausedRun`, resume modes (`off`/`ask`/`run`/`global`,
  default Off), and `decide_resume` enforcing all six safety gates before mode
  (never through pending destructive approval / dirty workspace / mid-step / after
  cancellation / on provider-identity change / past max wait). Verified: 7 tests
  incl. one-per-gate, global-off-by-default, continuous pause→resume across a
  window; clippy(-D)/fmt clean. (07.2 paused-state store persistence lands with
  07.15; the inspectable file format + round-trip are done here.)
