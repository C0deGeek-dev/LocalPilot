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

Any other keys under a provider table are preserved and passed through as
provider options (for example `max_tokens` for `anthropic`). See
[providers.md](providers.md).

### `[harness]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mode` | `agent` \| `harness` | `agent` | Operating mode |
| `attempts_per_step` | int | `3` | Max attempts per plan step |
| `auto_commit` | bool | `true` | Commit each completed step |
| `test_command` | string | none | Command run to gate step completion |
| `rules.<name>` | `off` \| `warn` \| `block` | — | Per-rule severity overrides |

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

[permissions]
profile = "default"

[quota]
auto_resume = "ask"

[mcp.servers.files]
command = "my-mcp-file-server"
args = ["--root", "."]
```
