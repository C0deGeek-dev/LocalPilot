# 03 — Provider Runtime

## Goal
> Phase 2 (`docs/03`) + the full provider contract (`docs/04`). Call official
> public APIs and local OpenAI-compatible servers through one object-safe trait,
> hiding API differences behind a single internal stream contract while exposing
> capabilities and quota metadata. Provider-specific code lives only in
> `unshackled-llm` provider modules (`docs/13` §2). No private/undocumented
> endpoints (ADR-0004). Provenance notes cite public API docs (`docs/00`,
> `CONTRIBUTING.md`).

## Boxes
> ID = `03.<box-number>`. Owners: agent · tech-lead · release-engineer.

- [x] **03.1** (agent) Finalize the `ModelProvider` trait (object-safe,
      `async-trait`, `docs/13` §6) and the provider **declaration** every
      provider must expose (`docs/04` Requirements): id, display name, source
      type (`official_api`/`local_server`/`custom_user_endpoint`), supported
      input blocks, output events, tool-call shape, reasoning shape, max context
      if known, auth requirements, rate-limit behaviour if known. (Verified:
      trait is object-safe — `Box<dyn ModelProvider>` compiles; declaration is a
      typed struct.)
- [x] **03.2** (agent) Implement the internal request/event model (`docs/04`):
      `ModelRequest { model, messages, tools }` with namespaced provider options
      reserved for the future fields; `ModelEvent` covering `TextDelta`,
      `ReasoningDelta`, `ToolCall`, `Usage`, `ProviderWarning`, `Done`. Stream
      returns `impl Stream`/boxed stream consumed with `StreamExt`; malformed
      stream surfaces a typed `ProviderError`, never a panic (`docs/13` §5).
      (Verified: stream-parse unit tests for text and tool-call deltas.)
- [x] **03.3** (agent) Implement the provider **capability descriptors** the
      runtime branches on (NOT provider names, `docs/04` §Provider Differences):
      parallel tool calls, incremental JSON tool args, reasoning availability,
      streaming-vs-final usage, per-request tool disable, quota/reset surfacing,
      no-tool prompt path. (Verified: capability struct + a test asserting the
      runtime selects behaviour by capability.)
- [x] **03.4** (agent) Implement the error taxonomy (`docs/04`): auth,
      rate_limit, quota, invalid_request, model_not_found, server, network,
      stream_decode, unsupported_feature — as a `#[non_exhaustive]`
      `ProviderError` (`thiserror`). UI-facing messages concise; debug may carry
      request IDs, never secrets. (Verified: classification unit tests map
      representative responses to the right variant.)
- [x] **03.5** (agent) Implement quota/reset metadata model (`docs/04`
      §Quota Semantics): `retry_after`, `reset_at`, `limit_kind`, `retryable`,
      `raw_provider_code`. Classify quota vs rate_limit. (Verified:
      `docs/08` Provider test "quota reset metadata is classified correctly".)
- [x] **03.6** (agent) Implement the **local OpenAI-compatible provider**
      (Ollama/vLLM/llama.cpp/local gateways, `docs/04` Local Server) using the
      official OpenAI-compatible request/stream shapes from public docs. Config
      via `base_url` + optional `api_key_env` (`docs/04` config example). TLS not
      required for localhost. (Verified: request-translation + stream-parse tests
      against scripted responses; provenance note cites public docs.)
- [x] **03.7** (agent) Implement **one official hosted provider** behind the
      same trait, from its public API docs (auth via env-var key, TLS, redact
      auth headers in logs, expose request IDs, `docs/07` Network Policy). Choice
      of which official provider is 03.13. (Verified: request-translation,
      stream-parse, error-classification, reasoning-event, redaction tests.)
- [x] **03.8** (agent) Implement the **mock/fake provider** for tests
      (`docs/11` Providers, `docs/13` §10 hand-written fakes): returns scripted
      stream events incl. each `ModelEvent`, malformed streams, and quota errors.
      This fake is reused by subjects 05/06/09. (Verified: fake drives a
      text-only and a tool-call scenario deterministically.)
- [x] **03.9** (agent) Implement the provider **registry** keyed by config;
      resolve `[provider].default` and `[providers.*]` to a `Box<dyn
      ModelProvider>` (`docs/02`/`docs/03`). (Verified: registry-resolution test
      for local + official + custom-user-endpoint.)
- [x] **03.10** (agent) Implement retry/backoff with jitter and rate-limit
      classification for transient/server/network errors, honouring documented
      retry windows and never framing it as bypassing limits (`docs/04`,
      `docs/07`). (Verified: backoff test with a fake that returns transient then
      success; bounded attempts.)
- [x] **03.11** (agent) Implement reasoning/thinking event translation and the
      **round-trip**: `ReasoningDelta` for display only; reasoning blocks needed
      for continuity persist in `ContentBlock::Reasoning` (signature + provider
      metadata) and replay on the next request for tool-use loops (`docs/04`,
      `docs/02` §Messages). (Verified: `docs/11` "reasoning round-trip tests for
      tool-use loops".)
- [x] **03.12** (agent) Add HTTP-adapter tests with `wiremock` (`docs/13` §10):
      status codes, malformed bodies, quota headers — offline. Provider tests
      require no real credentials by default (`docs/04` §Provider Tests).
      Confirm Phase 2 "Done when": `unshackled ask "..."` text-only works; tests
      use recorded fixtures / mock HTTP; no private endpoint in code or tests.
      (Verified: `wiremock` suite green; a clean-room grep finds no private
      endpoint.)
- [x] **03.13** (tech-lead) Choose which official hosted provider ships first
      for 03.7 (must be an official public API per ADR-0004; e.g. an
      OpenAI-compatible official API, Vertex AI, or Bedrock from `docs/04`
      Examples) and confirm the provenance/docs source. Record in §4 / Decision
      log; mirror to `manual-actions.md`. (Verified: §4 row names the provider +
      public-docs URL.)
- [x] **03.14** (release-engineer) Provide (locally, never committed) the
      credentials needed for the **opt-in** live provider test
      (`UNSHACKLED_LIVE_TESTS`) so 03.7 can be validated against the real API
      once before alpha; live tests skip without creds and never run in default
      CI (`docs/08` Live Tests). Mirror to `manual-actions.md`. (Verified: a live
      run is recorded as done or explicitly deferred.)


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

### Review result

1. Keep: Provider runtime stayed contract-level and clean-room: one
   provider-neutral trait/event model, capability-based behavior, OpenAI chosen
   through official public docs, local and hosted OpenAI-compatible paths sharing
   one adapter, and no private endpoints.
2. Fix before closing: None. The live-provider credential action is explicitly
   deferred in `manual-actions.md`; the local aggregate Windows GNU runner crash
   is documented as environmental rather than product behavior.
3. Record: D009, D010, D011, and D012 cover the provider choice, MSRV transitive
   pins, license/advisory posture, and local runner issue. `lessons.md` captures
   the dependency and runner learnings.
4. Risk: `03.14` remains deferred until release validation: the opt-in live
   OpenAI run still needs real credentials before alpha. CI/MSVC should remain
   the authority for aggregate workspace tests until the local Windows GNU
   `ring` crash is no longer relevant.
5. Verdict: CLOSE.

- [x] Captain Hindsight review recorded
- [x] Verdict is `CLOSE`

## Progress log
> One line per slice. Date · slice · box IDs · what shipped · how verified.

- 2026-06-02 · slice 1 · 03.1–03.14 · `unshackled-llm` provider runtime: object-safe
  `ModelProvider` trait + typed `ProviderDeclaration`/`Capabilities`; internal
  `ModelRequest`/`ModelEvent` (Text/Reasoning/ToolCall/Usage/Warning/Done) boxed
  stream; `#[non_exhaustive]` `ProviderError` taxonomy + `QuotaInfo` with
  quota-vs-rate-limit classification. One OpenAI-compatible adapter serves the
  local server **and** the official OpenAI API (D009): request translation, SSE
  decoding with incremental tool-call assembly, reasoning round-trip, header-based
  quota metadata, redacted bearer auth. Plus scripted `FakeProvider`, config-driven
  `ProviderRegistry` (local/official/custom), and retry/backoff with jitter
  honouring `retry_after`. Wired `unshackled ask` (phase-2 done-when).
  Verified: 19 llm unit + 5 wiremock HTTP + registry/retry tests; CLI `ask`
  end-to-end vs wiremock; opt-in `live.rs` (skips w/o `UNSHACKLED_LIVE_TESTS`).
  Clean-room: provenance note cites public OpenAI docs; endpoint grep finds no
  private endpoints (only localhost / api.openai.com). fmt/clippy(-D)/deny/audit
  green. Local aggregate test runner crashes on windows-gnu `ring` (D012); CI is
  MSVC. 03.13 OpenAI chosen; 03.14 live creds deferred (manual-actions).
