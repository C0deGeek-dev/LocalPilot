# Provider Contract

## Goals

Providers connect LocalPilot to models. They must hide API differences behind a
single internal stream contract while preserving provider capabilities.

## Requirements

Every provider must declare:

- id
- display name
- source type: `official_api`, `local_server`, or `custom_user_endpoint`
- supported input blocks
- supported output events
- supported tool-call shape
- supported reasoning/thinking shape
- max context tokens if known
- auth requirements
- rate-limit behavior if known

## Allowed Provider Types

### Official API

Uses a provider's documented API and authentication method.

Examples:

- OpenAI API
- Google Vertex AI
- AWS Bedrock
- other official provider APIs

### Local Server

Uses an endpoint running on the user's machine or infrastructure.

Examples:

- Ollama
- vLLM
- llama.cpp server
- local OpenAI-compatible gateways

### Custom User Endpoint

Allowed only when the user explicitly configures it. The docs must state that
the user is responsible for authorization and terms compliance.

## Prohibited Provider Types

- private consumer-product endpoints
- scraped web sessions
- undocumented subscription backends
- endpoints requiring browser cookie reuse unless the provider explicitly
  documents that as supported

## Internal Request

```rust
pub struct ModelRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}
```

Future fields:

- temperature
- max output tokens
- reasoning effort
- response format
- provider metadata
- cache policy

Provider-specific options must be namespaced.

## Internal Events

```rust
pub enum ModelEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCall { id: String, name: String, input_json: Value },
    Usage { input_tokens: u64, output_tokens: u64 },
    ProviderWarning { message: String },
    Done,
}
```

Provider adapters may emit `ReasoningDelta` only when the provider exposes
reasoning/thinking content through an official API surface. The UI can render
these events in the optional thinking panel; the core loop must treat them as
metadata, not user-visible final answer text.

Reasoning blocks needed for provider continuity are persisted in message content
and replayed on the next request. Adapters that require a reasoning signature or
provider metadata must round-trip it through `ContentBlock::Reasoning`; display
events alone are not enough for tool-use loops on those models.

Future events:

- reasoning summary
- refusal
- structured output delta

## Late System Messages

System messages are positional. The leading run of system messages at the start
of a conversation is the setup prompt; a system message appearing later (for
example host-injected retrieved context) is a mid-conversation instruction and
must reach the model at its original position in the history.

- An adapter whose wire has a positional system role (OpenAI-style) keeps a
  late system message at its original position but delivers it as user-role
  content. The chat-completions wire permits a positional system role, but many
  model chat templates (served behind an OpenAI-compatible endpoint) reject a
  system message that is not the first message; demoting the role — without
  moving the message — is compatible with both.
- An adapter whose wire hoists system content to a top-level field
  (Anthropic-style) hoists only the leading run. A later system message keeps
  its position and is delivered as user-role content, merged with adjacent
  user-role turns where the wire requires alternating roles.

An adapter must never silently reorder a late system message to the front of
the conversation; that changes what the model reads on one wire but not
another. Each adapter pins this with a behavioral test.

## Error Taxonomy

Providers return errors classified as:

- auth
- rate_limit
- quota
- invalid_request
- model_not_found
- server
- network
- stream_decode
- unsupported_feature

The UI should show concise messages. Debug logs may include request IDs but must
not log secrets.

## Provider Differences

The provider-neutral layer will leak unless differences are explicit. Each
provider implementation must document:

- whether parallel tool calls are supported
- whether partial JSON tool arguments stream incrementally
- whether reasoning/thinking blocks are available
- whether usage arrives during streaming or only at completion
- whether tools can be disabled per request
- how quota/rate-limit reset metadata is surfaced
- whether no-tool models need a different prompt path
- whether the provider can constrain decoding to a JSON schema (a local
  `llama-server` can, via a `json_schema` constraint, so tool-call arguments are
  schema-valid by construction); hosted providers leave this off and keep native
  tool-calling

The session runtime should branch on provider capabilities, not provider names.

### Constraint encoding (`constraint_mode`)

When a provider declares constrained decoding, the constraint's **wire encoding**
is selectable per provider via the `constraint_mode` option (default
`response_format`):

- `response_format` — the OpenAI structured-output wrapper
  (`response_format: { type: "json_schema", json_schema: { name, schema } }`).
  The default and the floor.
- `json_schema` — a documented llama.cpp server extension: the schema is sent as
  a **top-level `json_schema` field**, which the server compiles to a GBNF
  grammar. Documented in the public llama.cpp HTTP server API; no private endpoint
  behaviour. (Note: on a turboquant build whose json-schema→grammar path rejects a
  `<think>` prefix, this still `400`s — use `grammar`.)
- `grammar` — a top-level **GBNF `grammar`** string built from the tool names: a
  valid-tool-call grammar (`{ "name": <one of the tools>, "arguments": <JSON
  object> }`). Use this for a turboquant `llama-server` whose lazy-grammar engages
  a GBNF even after a `<think>` prefix (where the json-schema path `400`s).
  Constrains the tool-call *shape* and a valid-JSON arguments payload, not each
  tool's argument schema.

```toml
[providers.local]
kind = "openai-compatible"
base_url = "http://localhost:8080/v1"

[providers.local.options]
constraint_mode = "json_schema"   # reach a turboquant server's grammar
```

If a server accepts neither encoding, the client error caches the rejection and
the constraint is dropped for the session — native tool-calling, the floor. An
unknown `constraint_mode` value falls back to `response_format`.

### Vision (image input)

Whether a provider accepts image (vision) input is a **resolved capability**, not
an assumption from the source type (ADR-0061). It resolves in a fixed precedence —
**config > probe > false**:

1. **Config.** A per-provider `supports_vision` flag (see
   [`configuration.md`](configuration.md)) is authoritative — set by the user, or
   auto-written by LocalBox when it loads a multimodal projector for a vision
   launch. It is a user/launcher **assertion**: declaring vision on a text-only
   model can still send images the server rejects.
2. **Probe.** When config does not declare it, a best-effort, **read-only**
   discovery-time probe of a local llama.cpp `llama-server` reads the documented
   `GET /props` `modalities.vision` field (set when an `--mmproj` projector is
   loaded). It runs **no model inference**, is toggleable via `[discovery]
   vision_probe` (default on), and an unreachable or signal-less server resolves to
   *unknown* (no vision claimed), never a false positive. No private endpoint
   behaviour is used; LocalPilot does **not** augment the `GET /v1/models` response.
3. **Default false.** Otherwise the model is treated as text-only — byte-identical
   to the prior behaviour for any undeclared, unprobed provider.

The OpenAI adapter's `Image` input block is advertised when the source is the
official API **or** vision resolves true. The official API is unaffected; a local
server advertises image input only once vision is declared or probed. `doctor`
surfaces the config-declared capability (it is offline); `localpilot models`
surfaces the full resolved capability and which signal decided it.

In interactive chat, pasting an image (Ctrl+V, or a terminal that routes it as a
bracketed paste) re-resolves this capability once before deciding: if vision is
not already known, LocalPilot runs the same config > probe resolution again
(catching a server that came up after startup), attaches on success, and
otherwise shows a notice naming both levers (`supports_vision` and `[discovery]
vision_probe`). A clipboard read that fails for any reason other than "no image
present" always surfaces a notice — an image paste never fails silently.

## Quota Semantics

Quota wait/resume honors provider contracts. A provider adapter may expose:

- `retry_after`
- `reset_at`
- `limit_kind`
- `retryable`
- `raw_provider_code`

When a provider gives no machine-readable reset time, LocalPilot should use
bounded backoff with jitter and re-probe before resuming. It must not frame this
as bypassing limits or retry against a provider's documented policy.

Reset metadata is parsed from the formats the official APIs document — HTTP
`retry-after` as delay-seconds or an HTTP-date, OpenAI-style per-window reset
headers as compact duration strings (`"1s"`, `"6m0s"`), Anthropic-style reset
headers as RFC 3339 timestamps. An unparseable header value degrades to absent
metadata, never to an error.

## Provider Tests

Provider tests must not require real credentials by default.

Required:

- request translation tests
- stream parsing tests
- error classification tests
- quota/reset metadata tests
- provider capability tests
- redaction tests

Optional:

- live tests gated by env var

## Configuration Example

```toml
[provider]
default = "local"

[providers.local]
kind = "openai-compatible"
base_url = "http://localhost:11434/v1"
api_key_env = "LOCALPILOT_LOCAL_API_KEY"

[providers.openai]
kind = "openai"
api_key_env = "OPENAI_API_KEY"

[providers.gemini]
kind = "google-vertex-openai"
auth = "google_adc"
google_project = "your-project-id"
google_location = "global"
model = "google/gemini-3.5-flash"
```

## Credential Resolution

For `auth = "api_key"`, the credential value is never stored in config — only
the *name* of the environment variable (`api_key_env`) that may hold it. At use,
a provider's credential is resolved with precedence: a stored credential (OS
keychain → a `0600` fallback file, written by `localpilot login`) → the
`api_key_env` environment variable (or a kind default) → none. So `login` makes
the environment variable optional, while an env-only setup keeps working
unchanged.

For `auth = "google_adc"`, LocalPilot reads a Google ADC file path (explicit
`google_adc_path`, `GOOGLE_APPLICATION_CREDENTIALS`, or the gcloud default ADC
path) and mints a short-lived OAuth access token in-process. The current
implementation supports gcloud `authorized_user` ADC files. The resolved secret
is wrapped so it never reaches logs, transcripts, or error output, and
`localpilot doctor` reports only the *source* (keychain / file / env /
google_adc / google_adc_file / none).
Bring-your-own-key only — no subscription-credential or "sign in with
Claude/ChatGPT" path (ADR-0042). See
[providers.md](providers.md) §Storing credentials.
