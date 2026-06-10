# 06 — Hook Fabric + Session Lifecycle UX

## Goal
Define the typed internal hook/event surface (the Rust analogue of an
extension event bus, research §5.1) and route LocalPilot's own cross-cutting
behaviors through it — with the permission engine as the first, always-on
hook, so extensibility *is* the safety model. On top of the subject-03 event
log, ship the session lifecycle UX (list/resume/continue/fork/tree) that makes
the audit trail something a user can pick back up (research §5.12). Third-party
plugin *packaging* is the follow-on plan (D002); this subject builds the fabric
and proves it internally. Requires subject 03 `DONE`.

## Boxes
> ID = `<subject-id>.<box-number>`. Stable — never renumber; mark retired boxes
> done and struck with `ABANDONED; see D###`, don't delete.
> Owner MUST be one of: agent · release-engineer · product-owner · tech-lead ·
> domain-sme.

- [x] **06.1** (agent) Typed hook surface, notify-only first: lifecycle
      notifications (turn start/end, tool execution start/end, model selected,
      compaction, recovery, quota transitions) with a registration API used
      in-process. Hook activity lands in the session event log so hooks are
      auditable like everything else.
- [x] **06.2** (agent) Permission engine as the first hook: the engine's
      verdict (`Allow`/`Ask`/`Deny`/`Block{reason}`) is delivered through the
      same hook chain every effect passes; recovery, quota, LocalMind
      injection, and the quality gate are re-routed as hooks with behavior
      pinned by existing tests (no behavior change, structural change only).
- [x] **06.3** (agent) Mutating hooks (block/modify tool calls, rewrite
      context) exist behind the permission engine and are exercised by at
      least one internal consumer; the stance for third-party hook code is
      documented now (out-of-process or trusted-only; no in-process arbitrary
      code), so the follow-on plugin plan has a fixed boundary.
- [x] **06.4** (agent) Session lifecycle UX on the event log: CLI
      `session list` / `session resume <id>` / `session export <id>`,
      `--continue` (most recent for this workspace) / `--resume <id>`; REPL
      `/resume` (picker), `/new`, `/fork`, `/clone`, `/tree`. Resume rebuilds
      state from the event log (resume, replay, and audit are one mechanism),
      records an open-reason, and **re-applies the current permission profile
      and trust state — never inherits stale elevated permissions** (test
      pins this). (Research §5.12.)
- [x] **06.5** (agent) Cancellation reaches running tools: a cancel races the
      executing tool future and kills spawned child processes instead of
      waiting out their timeout; the event log records the aborted execution.
      (Review §5.3.)

## Hindsight checkpoint
> Run after all boxes in this subject are complete and before marking the
> subject `DONE` in §5 of `tasks/NextPhase-Plan.md`. Use the plan's embedded
> "Appendix: Captain Hindsight Prompt". Record the review result here. An
> interim run after a large or risky box is allowed and recorded the same way;
> it does not replace the closing run.
>
> Required output sections: Keep; Fix before closing; Record; Risk; Verdict
> (`CLOSE` or `DO NOT CLOSE`). If the verdict is `DO NOT CLOSE`, leave the
> subject open, add/reopen boxes or update decisions/lessons, and rerun this
> checkpoint after the fixes.

- [x] Captain Hindsight review recorded
- [x] Verdict is `CLOSE`

**Captain Hindsight review (2026-06-10):**

1. **Keep:** the tighten-only gate design — gates run inside dispatch *after*
   the permission engine and can only block, so "extensibility is the safety
   model" is structural, not policy. Context hooks reuse the seeded-system
   path (one mutation mechanism, already evented). Resume/fork/clone built
   directly on the subject-03 event log: resume, replay, and audit are
   literally one mechanism (`transcript_from_events`), and the
   permission-reapplication pin
   (`resume_applies_the_current_profile_never_the_logged_one`) is the
   security heart of the box. Cancellation through tools rides `kill_on_drop`
   + a select race — no new process-management machinery.
2. **Fix before closing:** an unused-import lint slipped into the slice-1
   checkpoint commit (caught by the local gate immediately after; fixed in
   this slice). Run the full clippy matrix *before* committing, not in the
   same breath.
3. **Record:** D010 — the box's REPL `/resume` (session picker) collides with
   the existing `/resume` (harness step resume); the lifecycle commands
   shipped as `/sessions` (list) + `/session <id>` (switch), alongside
   `/new`, `/fork`, `/clone`, `/tree`. Gate blocks land in the event log via
   the error `ToolFinished` + the gate-named output, not a dedicated
   `PermissionDecided` entry; the variant remains for the follow-on plugin
   plan.
4. **Risk:** the re-route of recovery/quota/quality-gate is notification-level
   (their events flow through the fabric; their control logic stays in the
   loop) — full inversion would have risked the pinned recovery behavior for
   structure's sake. LocalMind injection is the one full re-route (now a
   `ContextHook`). The TUI session commands render through notices rather
   than a dedicated picker widget; a richer picker is UI polish, not
   contract.
5. **Verdict:** CLOSE.

## Progress log
> One line per slice. Date · slice number (per-subject, starting at 1) · box
> IDs touched · what shipped · how verified · checkpoint commit/push status.
> Append a `tasks/next-phase/lessons.md` line here too whenever the slice
> taught something.
- 2026-06-10 · slice 1 · 06.1–06.3, 06.5 · Hook fabric (notify-only
  observers, pre-turn context hooks, tighten-only tool gates after the
  engine inside dispatch); LocalMind injection re-routed as the built-in
  context hook; third-party stance fixed in docs/extending.md (in-process =
  compiled-in Rust only; external = RPC/ACP/MCP); cancellation races the
  executing tool with child-process kill and event-log audit. Verified:
  hooks test suite + full gate. Checkpoint: committed + pushed.
- 2026-06-10 · slice 2 · 06.4 · Session lifecycle on the event log:
  `start_new_session`/`load_session`/`fork_session` on the runtime
  (self-contained fork logs, open reasons, fresh-profile resume); CLI
  `session list|export|resume` and print `--continue`/`--resume <id>`; REPL
  `/new`, `/fork`, `/clone`, `/tree`, `/sessions`, `/session <id>` (D010
  name-clash note); lifecycle test suite incl. the
  stale-permissions-never-inherited pin. Verified: full workspace gate
  green. Checkpoint: committed + pushed.
