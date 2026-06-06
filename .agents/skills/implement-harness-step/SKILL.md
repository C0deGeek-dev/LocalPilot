---
name: implement-harness-step
description: >-
  Implement or modify the harness execution loop — reading brief.md / PROGRESS.md,
  running a step, applying rule verdicts, respecting attempt limits, updating
  progress, and committing per the commit policy. Use when touching
  localpilot-harness or any init/status/intake/plan/feature/resume command.
---

# implement a harness step

Authoritative contracts: [`docs/06-harness-spec.md`](../../../docs/06-harness-spec.md)
and the phase tasks in
[`docs/03-implementation-plan.md`](../../../docs/03-implementation-plan.md). This
skill lists the invariants to honour; the specs own the detail.

## Source of truth (ADR-0003)

`brief.md` and `PROGRESS.md` are authoritative and user-editable. The next run
treats the edited file as truth. Transcripts are supporting context only — never
override a project file from a transcript.

## Loop invariants

- Parse `brief.md` / `PROGRESS.md` into typed structures; re-render
  deterministically (round-trip a fixture and assert equality).
- Run the rule engine before/after a step; honour each verdict (block vs warn)
  exactly as [`docs/06`](../../../docs/06-harness-spec.md) defines. Rules are
  deterministic.
- Respect attempt limits and the anti-sunk-cost replan loop — bounded retries,
  then replan rather than grind.
- Update `PROGRESS.md` as part of the step, not after the fact.
- Commit policy: one commit per completed step, message shaped
  `harness: <step description>` (this is the product runtime's own commit format,
  not a repo-development commit).

## Must-pass tests

A parse→render round-trip fixture; a rule-trigger table test (each rule's
trigger → verdict); an attempt-limit / replan test. Keep prompts original
(see [[clean-room-guard]]).
