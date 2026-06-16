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
```
