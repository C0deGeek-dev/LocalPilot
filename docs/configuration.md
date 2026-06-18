# Configuration

LocalPilot reads `.localpilot.toml` from the user config directory and the
project root (project overrides user), with environment variables and CLI flags
layered on top. `localpilot init` writes a starter file; `localpilot doctor`
shows the resolved search paths.

## Stability

The configuration schema is **stable under semantic versioning** from v1.0:

- Within a major version, the documented tables and keys below keep their
  meaning. New optional keys may be added (a minor change); existing keys are
  not renamed, removed, or retyped without a major-version bump and a documented
  migration.
- **Unknown keys are ignored**, so a config written for a newer minor version
  still loads on an older binary, and vice versa. Per-provider keys the core
  does not model are preserved (see `[providers.*]` options).
- Defaults are stable: an omitted key behaves as documented here.

Before v1.0 (the current `0.x` alphas) the schema may still change; such changes
are noted in `CHANGELOG.md`.

## Reference

### `[provider]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `default` | string | `"local"` | Id of the provider used when `--provider` is omitted |

### `[providers.<id>]`

One table per provider. `<id>` is the name referenced by `[provider].default`
and `--provider`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `kind` | string | — | `openai`, `openai-compatible` (alias `local`), `anthropic`, or `custom` |
| `base_url` | string | per kind | API base URL (required for local/custom) |
| `api_key_env` | string | none | Name of the env var holding the credential (never the value) |
| `model` | string | none | Default model when a command does not pass `--model` |
| `request_timeout_secs` | int | per adapter | HTTP timeout; useful for slow local inference |
| `context_window` | int | none | The model's context window in tokens; when set, the session budget derives from it (window minus a response reserve) and takes precedence over `[harness] context_token_limit` |

Any other keys under a provider table are preserved and passed through as
provider options (for example `max_tokens` for `anthropic`, or the
LocalPilot-owned switches `suppress_thinking` and `reasoning_round_trip`). See
[providers.md](providers.md).

### `[harness]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mode` | `agent` \| `harness` | `agent` | Operating mode |
| `attempts_per_step` | int | `3` | Max attempts per plan step |
| `auto_commit` | bool | `true` | Commit each completed step |
| `test_command` | string | none | Command run to gate step completion |
| `tool_call_budget` | int | `50` | Soft start for the per-turn tool-call ceiling. A turn making forward progress runs past this up to `tool_call_budget_max`; a turn detected as making no progress stops here |
| `tool_call_budget_max` | int | `50` | Hard cost ceiling: the per-turn tool-call count that always stops the loop, regardless of progress. Equal to `tool_call_budget` by default (a flat fixed ceiling); raise it above the soft start to let a productive turn extend |
| `rules.<name>` | `off` \| `warn` \| `block` | — | Per-rule severity overrides |

Notable rule key:

| Rule | Default | Meaning |
| --- | --- | --- |
| `check_before_launch` | `warn` | When the task prompt named a local serveable target (a loopback host, or any `host:port` with an explicit port) that has not been probed this session, an attempt to launch a local server or scaffold a competing `index.html` is nudged (`warn`, the call still runs), refused (`block`), or ignored (`off`). Auto-extracted from the prompt — an external reference URL without a port is not a target. Advisory, tighten-only, best-effort. See [06-harness-spec.md](06-harness-spec.md). |

### `[compaction]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mode` | `deterministic` \| `smart_with_fallback` | `deterministic` | Runtime context compaction mode. `smart_with_fallback` keeps deterministic compaction as the completed-only fallback when no validated summarizer backend is available |
| `summary_token_limit` | int | `1024` | Target maximum size for rendered compact summaries |
| `summarizer_input_tokens` | int | `8192` | Reserved input budget for model-backed summarization when enabled |
| `summarizer_timeout_secs` | int | `20` | Timeout budget for a future model-backed summarizer call |

### `[permissions]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `profile` | `default` \| `relaxed` \| `bypass` | `default` | Permission profile. `bypass` is never the default and is always surfaced |

### `[quota]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `auto_resume` | `off` \| `ask` \| `run` \| `global` | `off` | When to resume a quota-paused run |
| `max_wait_minutes` | int | `360` | Cap on how long to wait before resuming |
| `resume_requires_clean_workspace` | bool | `true` | Refuse to resume with a dirty tree |
| `resume_requires_no_pending_approval` | bool | `true` | Refuse to resume through a pending approval |
| `resume_only_at_step_boundary` | bool | `true` | Resume only between steps |

### `[mcp.servers.<name>]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `command` | string | — | Command that launches the MCP server |
| `args` | array of string | `[]` | Arguments to the command |

See [mcp.md](mcp.md).

### `[skills]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `autonomous_discovery` | bool | `false` | Register the `skill_search`/`skill_load` tools so the model may discover and read project skills on its own. Off by default, so a small local model never auto-injects a skill. The deterministic `localpilot skills list \| show` surface works regardless. |

Project skills are advisory prompt modules under `.localpilot/skills/` or
`.agents/skills/`; see [05-tool-system.md](05-tool-system.md) §Project Skill
Discovery.

## Example

```toml
[provider]
default = "anthropic"

[providers.anthropic]
kind = "anthropic"
model = "claude-sonnet-4-6"
api_key_env = "ANTHROPIC_API_KEY"

[providers.local]
kind = "openai-compatible"
base_url = "http://localhost:8080/v1"
model = "qwen2.5-coder"

[harness]
mode = "agent"
test_command = "cargo test"

[compaction]
mode = "deterministic"

[permissions]
profile = "default"

[quota]
auto_resume = "ask"

[mcp.servers.files]
command = "my-mcp-file-server"
args = ["--root", "."]
```
