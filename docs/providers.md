# Configuring a provider

Unshackled is provider-neutral. It talks to models through official public APIs
and local OpenAI-compatible servers; it never uses private or undocumented
endpoints. Providers are configured in `.unshackled.toml`.

## A local OpenAI-compatible server

Works with any local server that speaks the OpenAI Chat Completions API (for
example Ollama, vLLM, llama.cpp's server, or a local gateway).

```toml
[provider]
default = "local"

[providers.local]
kind = "openai-compatible"
base_url = "http://localhost:11434/v1"
# Default model, used when a command does not pass --model (and by the REPL):
model = "your-local-model"
# Optional, only if your gateway requires a key:
api_key_env = "UNSHACKLED_LOCAL_API_KEY"
```

TLS is not required for `localhost`.

With a `model` set on the default provider, running `unshackled` with no
subcommand launches the interactive REPL against it. Without a resolvable
provider and model it prints the doctor report instead, so a fresh or headless
checkout still gives a useful result. (The REPL is in release builds; the
default-feature build prints the doctor report.)

## The official OpenAI API

Uses the documented OpenAI API and its API-key authentication.

```toml
[providers.openai]
kind = "openai"
api_key_env = "OPENAI_API_KEY"
```

Then set the key in your environment (never commit it):

```sh
export OPENAI_API_KEY=sk-...        # Linux / macOS
$env:OPENAI_API_KEY = "sk-..."      # Windows PowerShell
```

Credentials are read from the named environment variable at use and wrapped so
they never appear in logs, transcripts, or error output. The config file only
records the *name* of the variable, never the secret.

## The official Anthropic API

Uses the documented Anthropic Messages API (a distinct wire protocol from
OpenAI: a top-level `system`, `tool_use`/`tool_result` content blocks, and a
required `max_tokens`).

```toml
[providers.anthropic]
kind = "anthropic"
model = "claude-sonnet-4-6"
api_key_env = "ANTHROPIC_API_KEY"
# max_tokens defaults to 4096; override per provider if you like:
# max_tokens = 8192
```

```sh
export ANTHROPIC_API_KEY=sk-ant-...     # Linux / macOS
$env:ANTHROPIC_API_KEY = "sk-ant-..."   # Windows PowerShell
```

The credential is sent as the `x-api-key` header with the documented
`anthropic-version`; it is wrapped so it never appears in logs or transcripts.

## Verifying

```sh
unshackled doctor                       # shows which credentials are present
unshackled ask --model <name> "hello"   # one-shot streamed completion
```

Provider names appear here only as compatibility statements. Unshackled is a
provider-neutral harness, not a vendor product.
