---
name: add-provider
description: >-
  Add or change a model provider — implement the provider trait in localpilot-llm
  behind the registry, map the stream-event model, declare capabilities and quota
  metadata, handle the error taxonomy, and write the required provider tests. Use
  for provider/adapter work.
---

# add a provider

Authoritative contract: [`docs/04-provider-contract.md`](../../../docs/04-provider-contract.md).
This skill lists where provider code lives and the must-pass tests; the spec owns
the field-level detail.

## Rules

- Provider code lives ONLY in `localpilot-llm`, behind the one provider trait and
  the registry. `localpilot-core` stays provider-neutral (ADR-0002).
- Official public API surfaces or local OpenAI-compatible servers only — no
  private or undocumented endpoint adapters (ADR-0004). Cite the public API docs
  in the PR provenance note (see [[clean-room-guard]]).
- Map the provider's stream to the typed `ModelEvent` model; surface
  capabilities, quota metadata, and the error taxonomy from
  [`docs/04`](../../../docs/04-provider-contract.md). Round-trip reasoning content
  where the provider supports it.
- Retry/backoff and recovery are typed, not ad hoc.

## Must-pass tests (per docs/04 "required provider tests")

Text completion, tool call, streaming, malformed/garbage response, and quota
exhaustion — each against a hand-written fake transport, deterministic and
offline. Live tests are opt-in behind `LOCALPILOT_LIVE_TESTS`.
