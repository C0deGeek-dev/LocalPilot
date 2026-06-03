# Extending Unshackled

Three extension points: providers (models), tools (capabilities), and MCP
servers (external tools). All run through the same permission engine and
redaction — an extension is never a side channel.

## Connect an MCP server (no code)

The simplest extension: point Unshackled at a Model Context Protocol server. Its
tools are discovered at startup and dispatched like builtins, gated as a network
effect.

```toml
[mcp.servers.files]
command = "my-mcp-file-server"
args = ["--root", "."]
```

Details: [mcp.md](mcp.md).

## Add a provider

A provider adapts a model API to the internal streaming contract. Two ways:

1. **Configuration only** — for any OpenAI-compatible endpoint, no code:

   ```toml
   [providers.myapi]
   kind = "openai-compatible"
   base_url = "https://my-gateway.example/v1"
   api_key_env = "MYAPI_KEY"
   ```

2. **A new adapter** — for a different wire protocol. Implement the
   `ModelProvider` trait in `unshackled-llm` and register a `kind` for it.

   - `declaration()` returns a `ProviderDeclaration` whose `Capabilities` the
     runtime branches on — never the provider's name.
   - `stream()` issues the request and returns a `ModelEventStream`: a stream of
     `ModelEvent`s (`TextDelta`, `ReasoningDelta`, `ToolCall`, `Usage`, `Done`).
     Accumulate any streamed tool-argument fragments before emitting `ToolCall`.
   - Map provider errors into the stable `ProviderError` taxonomy, including
     `QuotaInfo` on rate-limit/quota responses.
   - Add the `kind` to the registry's `build_provider`.

   The OpenAI (`openai.rs`) and Anthropic (`anthropic.rs`) adapters are the two
   reference implementations — one per wire protocol. Contract:
   [04-provider-contract.md](04-provider-contract.md).

   **Clean-room:** implement from the provider's public API docs only; never
   copy a vendor SDK, and use only documented official endpoints or local
   servers ([00-clean-room.md](00-clean-room.md)).

## Add a tool

A tool is a capability the model can call. Implement the `Tool` trait in
`unshackled-tools` and register it in `ToolRegistry::with_builtins` (or add it to
a session's registry).

- `name()` / `description()` / `schema()` — the model discovers the tool from
  these; `schema()` is generated from a typed input struct.
- `effects(input, ctx)` declares the side effects (read/write path, run command,
  network) **without performing them**; the permission engine authorizes each
  before the tool runs. A tool with no effects needs no approval.
- `invoke(input, ctx)` runs only after every effect is authorized. Its output is
  redacted by the registry on every profile, including `bypass`.

Never reach a side effect outside the declared effects — dispatch is the single
authorized path. Contract: [05-tool-system.md](05-tool-system.md).

## Permissions and redaction

Every extension is gated by the permission engine
([07-security-and-privacy.md](07-security-and-privacy.md)): risky effects prompt
in interactive mode and are denied non-interactively unless a trusting profile
is set. Output is redacted before it reaches the transcript, the model, or the
logs.
