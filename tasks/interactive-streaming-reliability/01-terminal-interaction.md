# 01 - Terminal Interaction

## Goal

Keep the input editor visible and responsive during active turns and make
cancellation responsive under high-volume streams.

## Boxes

- [x] **01.1** (agent) Accept editable input keys while a turn is active.
- [x] **01.2** (agent) Bound redraw frequency and prioritize keyboard polling/cancellation.
- [x] **01.3** (agent) Add focused input and rendering regression tests.

## Hindsight checkpoint

- [x] Captain Hindsight review recorded
- [x] Verdict is `CLOSE`

Keep: a periodic, prioritized input/redraw tick independent of model event
volume. Fix: preserve press-only key handling to avoid duplicate Windows input.
Record: active-turn text remains buffered and editable; Enter does not submit a
second concurrent turn. Risk: no automated pseudo-terminal timing test.
Verdict: `CLOSE`.

## Progress log

- 2026-06-04: Added active-turn editing, visible busy-state caret, bounded redraws, prioritized cancellation and approval handling, and latest-transcript scrolling.
