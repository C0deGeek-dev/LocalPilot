# 00 - Tooling Research And Readiness

## Goal

Identify the owning terminal, session, recovery, and provider code and establish
the baseline before implementation.

## Boxes

- [x] **00.1** (agent) Read repository instructions, clean-room policy, provider contract, and applicable skills.
- [x] **00.2** (agent) Inventory TUI, CLI event-loop, harness session, recovery, and OpenAI-compatible stream surfaces.
- [x] **00.3** (agent) Run the baseline targeted tests and record failures.
- [x] **00.4** (agent) Convert findings into implementation constraints.

## Hindsight checkpoint

- [x] Captain Hindsight review recorded
- [x] Verdict is `CLOSE`

Keep: direct tracing from the active-turn loop through rendering, recovery, and
provider decoding. Fix: none. Record: the endpoint was offline, but recorded
sessions established a real slash flood and persisted mid-word responses. Risk:
live LocalBox behavior remains environment-dependent. Verdict: `CLOSE`.

## Progress log

- 2026-06-04: Read guidance and traced the reported interaction failures to the active-turn CLI loop; provider diagnosis continues.
- 2026-06-04: Baseline and recorded-session inspection identified redraw starvation, hidden busy-state caret, non-scrolling transcript, actual slash-flood recovery, and incomplete streamed content blocks.
