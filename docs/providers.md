# Configuring a provider

LocalPilot is provider-neutral. It talks to models through official public APIs
and local OpenAI-compatible servers; it never uses private or undocumented
endpoints. Providers are configured in `.localpilot.toml`.

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
api_key_env = "LOCALPILOT_LOCAL_API_KEY"
# Optional for slow local inference:
request_timeout_secs = 600
# Optional: the model's context window in tokens. When set, the session
# budget becomes (window - response reserve) instead of the global
# [harness] context_token_limit:
# context_window = 32768
```

TLS is not required for `localhost`.

External launchers may also provide a local endpoint without editing
`.localpilot.toml`: if an OpenAI-compatible provider has no `base_url`,
`OPENAI_BASE_URL` is used as a fallback. If `api_key_env` is not set,
OpenAI-compatible providers fall back to `OPENAI_API_KEY`.

With a `model` set on the default provider, running `localpilot` with no
subcommand launches the interactive REPL against it. Without a resolvable
provider and model it prints the doctor report instead, so a fresh or headless
checkout still gives a useful result. (The REPL is in release builds; the
default-feature build prints the doctor report.)

## The official OpenAI API

Uses the documented OpenAI API and its API-key authentication.

```toml
[providers.openai]
kind = "openai"
# api_key_env defaults to OPENAI_API_KEY when omitted.
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
# api_key_env defaults to ANTHROPIC_API_KEY when omitted.
# max_tokens defaults to 8192 (sized for a coding agent writing whole files);
# override per provider if you like:
# max_tokens = 16384
```

```sh
export ANTHROPIC_API_KEY=sk-ant-...     # Linux / macOS
$env:ANTHROPIC_API_KEY = "sk-ant-..."   # Windows PowerShell
```

The credential is sent as the `x-api-key` header with the documented
`anthropic-version`; it is wrapped so it never appears in logs or transcripts.

If `base_url` is omitted, Anthropic providers use
`ANTHROPIC_BASE_URL` before falling back to the official API URL. If the config
does not set `model`, `ANTHROPIC_MODEL` can provide the default model for
`chat` and the no-argument launcher path.

## Storing credentials: `login` / `logout`

Instead of hand-setting an environment variable, you can store a key with the
bring-your-own-key helper:

```sh
localpilot login anthropic     # or: localpilot login openai
localpilot logout anthropic    # remove a stored key
```

`login` opens the provider's official key-creation page
(`https://console.anthropic.com/settings/keys` for Anthropic,
`https://platform.openai.com/api-keys` for OpenAI — the URL is always printed so
a headless host works by paste alone), prompts for the pasted key, validates it
with one minimal `GET /models` request, and stores it. The key is shown back
only masked and is never logged. Flags: `--no-browser` skips opening the browser,
`--no-verify` skips the validation request (an offline or odd-endpoint key is
stored with a warning either way). `login <id>` also accepts a configured
provider id, in which case the key is stored under that id.

This is **bring-your-own-key only**: you create a standard API key in the
provider dashboard. There is no "sign in with Claude/ChatGPT" and no use of
Claude/ChatGPT *subscription* credentials — that is a provider terms violation
(see `docs/07-security-and-privacy.md` and ADR-0042).

**Where the key is stored.** On Windows, in the Credential Manager (built with
the `keychain` feature). On macOS and Linux, and on any host without a keychain
backend, in a `0600` file under the per-user directory beside `config.toml`
(`credentials.json`). The key never enters the repo or a config file.

**Resolution precedence.** When a provider needs a credential it is resolved in
order: a stored credential (keychain → fallback file) first, then the
`api_key_env` environment variable (or the kind default `ANTHROPIC_API_KEY` /
`OPENAI_API_KEY`), then config. So a logged-in user needs no environment
variable, and an existing env-only setup keeps working unchanged.

`localpilot doctor` reports the resolved *source* per provider — `keychain`,
`file`, `env`, or `not set` — never the secret itself.

## Switching provider/model mid-session: `/model`

In the `chat` REPL, `/model` switches the active provider/model without losing
the conversation:

- `/model` — list the configured providers and the models each reports (via the
  same `GET /models` discovery as `localpilot models`), marking the active one.
  Discovery failure is non-fatal: the configured model is shown with a note.
- `/model <provider>` — switch to that provider, adopting its configured default
  model (or keeping the current model name with a warning if it has none).
- `/model <provider> <model>` — switch to that provider and model. An unlisted
  model id warns but is still used.

The switch selects an already-built provider (every configured provider is built
once at startup), so it does not rebuild or re-authenticate; it takes effect at
the next turn boundary and preserves the full transcript, which is
provider-neutral. An unknown provider id, or a switch attempted while a turn is
running, is reported as a plain message and leaves the session unchanged. See
ADR-0041.

## Runtime tuning

`request_timeout_secs` can be set on any provider entry. It applies to the HTTP
client used by that provider and is intended for slower local models or gateways.

Provider options not modeled by LocalPilot are passed through from the provider
table into the request body. `suppress_thinking = true` is an LocalPilot-owned
switch: adapters avoid optional thinking output where the public request shape
supports it, and the switch itself is not forwarded as a raw API field. Inline
`<think>...</think>` text emitted by compatible local models is routed to the
reasoning stream and is not treated as final answer text, including blocks that
span many stream chunks or tags split across chunks.

`reasoning_round_trip` is another LocalPilot-owned switch for OpenAI-compatible
providers: when true, assistant reasoning is replayed to the server as the
`reasoning_content`/`reasoning_signature` message fields (a local-inference
convention used by vLLM-style servers). The default is on for local and custom
endpoints and off for the official hosted API, which does not document those
fields. The switch itself is never forwarded as a raw API field.

## Model discovery

`localpilot models` queries each configured OpenAI-compatible server's public
`GET /models` listing and prints what is actually loaded, with the context
window where the server reports one. The request is a network effect and
passes the permission engine like any other. In the interactive REPL the same
listing is consulted at startup (best-effort, silent on failure) to derive the
session budget when no `context_window` is configured.

## Reasoning effort

`/effort minimal|low|medium|high` in the REPL sets a typed reasoning-effort
level for subsequent turns. On effort-aware OpenAI-compatible servers it maps
to the documented `reasoning_effort` request field; on protocol shapes without
one it clamps to a no-op. Harness integrations can set it per step via the
session runtime.

## Context estimates

Context usage figures are a bytes/4 heuristic, not a tokenizer count: they
over-count CJK text (up to ~3x) and under-count dense code. The TUI footer
marks the figure with `~` to state that basis. The session budget the figure
is measured against is the model's real context window minus a response
reserve when the window is known (config `context_window` or discovery), and
the global `[harness] context_token_limit` otherwise.

## Evals

The offline golden-task scorecard runs with the normal workspace test suite:

```sh
cargo test -p localpilot-harness --test evals
```

Live validation is opt-in and never commits credentials. Set
`LOCALPILOT_LIVE_TESTS=1` only in a local shell that already has provider
configuration and credentials. The live runner uses the default configured
provider and model. If no model is configured, set `LOCALPILOT_LIVE_MODEL` in
that same local shell.

## Verifying

```sh
localpilot doctor                       # shows which credentials are present
localpilot ask --model <name> "hello"   # one-shot streamed completion
```

Provider names appear here only as compatibility statements. LocalPilot is a
provider-neutral harness, not a vendor product.
