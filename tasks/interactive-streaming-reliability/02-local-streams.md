# 02 - Local Compatible Streams

## Goal

Prevent compatible local server stream variations from creating duplicated,
false-degenerate, or silently truncated responses.

## Boxes

- [x] **02.1** (agent) Diagnose LocalBox-compatible stream and recovery behavior from existing contracts and fixtures.
- [x] **02.2** (agent) Implement the smallest provider-neutral compatibility fix.
- [x] **02.3** (agent) Add deterministic stream and recovery regression tests.

## Hindsight checkpoint

- [x] Captain Hindsight review recorded
- [x] Verdict is `CLOSE`

Keep: provider-neutral lifecycle validation and session-level recovery. Fix:
tighten stop-reason compatibility so open or unframed text cannot be accepted
as complete. Record: a degenerate retry disables tool schemas for that retry
only. Risk: no live LocalBox replay while the endpoint is offline. Verdict:
`CLOSE`.

## Progress log

- 2026-06-04: Added no-tools recovery for repeated degenerate output and rejected transport EOF/message stop while streamed content remains incomplete.
