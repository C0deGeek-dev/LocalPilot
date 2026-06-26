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

## Project context files

Beyond `.localpilot.toml`, a project may carry free-text **instruction files** —
`CLAUDE.md` and `AGENTS.md` — that orient the agent with project conventions and
constraints. LocalPilot discovers them, resolves their `@`-imports, and merges
them into one ordered context document that the learning engine ingests as
first-class project knowledge (so retrieval can surface a convention on demand).

**Discovery.** Three layers are collected:

- **repo-root** — `CLAUDE.md` / `AGENTS.md` at the workspace root;
- **nested** — the same file names in subdirectories of the workspace (the walk
  honours ignore files and is depth-bounded);
- **global** — `CLAUDE.md` / `AGENTS.md` under the per-user `~/.localpilot/`
  directory (resolved cross-platform from the home directory).

**Precedence** (most → least specific): **repo-root > nested directory >
global**. The workspace-root files are the authoritative project instructions
and lead the merge; nested-directory files refine within their subtree and
follow (ordered by ascending directory depth, then path, for determinism); the
per-user global files are the baseline and come last.

**`@`-imports.** A line whose trimmed text is exactly `@<path>` imports that
file's body inline at that point (relative paths resolve against the importing
file's directory; an absolute path is used as-is). Imports may nest; resolution
is bounded by a maximum depth and guarded against cycles, so the merged output is
always finite and deterministic. A missing or unreadable import, or one past the
depth bound or in a cycle, is replaced by a short marker comment rather than
failing discovery. A prose `@mention` (with surrounding text on the line) is not
an import directive.

**Ingestion.** Folder ingestion (`localpilot ingest run` / `refresh`, and the
session-open background build) captures the merged document as a derived chunk
under a synthetic `<project-context>` path, distinct from the raw files, so
`knowledge_search` surfaces project conventions even when the source files are
large or scattered. The merged context is derived, disposable state under
`.localmind/ingest/` like any other ingested knowledge (ADR-0013); it is never
written to accepted memory without review.

**Store location.** `localpilot learning` and `localpilot memory` find the
LocalMind store by walking up from the current directory to the nearest ancestor
holding `.localmind` (git-style), so a subdirectory answers from the project's
store rather than a second empty one. `--workspace <path>` pins the root
explicitly. The full contract — including the read-only, never-create search
behaviour and the three distinguished empty states — is in
[`localmind-integration.md`](localmind-integration.md#store-resolution).

**Search output format.** `learning search` and `memory search` choose their
output format from context (ADR-0048), so a program reading the output gets a
machine-readable form without having to know a flag exists:

- **stdout is not a terminal** (piped or redirected) → a **JSON array** by
  default (`memory_id`, `score`, `path`, `snippet`, `category`);
- **stdout is a terminal** → the **human table**, plus a one-line stderr hint
  pointing at the structured form;
- **`--format human|json`** overrides either way (with `--json` as an alias for
  `--format json`): `--format human` forces the table even when piped, `--format
  json` forces JSON on a terminal.

Stdout stays script-stable in every case — an empty result is a valid empty JSON
array, and all diagnostics (the format hint, the store-resolution and empty-state
lines) go to stderr.

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
| `kind` | string | — | `openai`, `openai-compatible` (alias `local`), `anthropic`, `custom`, or `google-vertex-openai` |
| `auth` | string | `api_key` | Authentication mode: `api_key` or `google_adc` |
| `base_url` | string | per kind | API base URL (required for local/custom) |
| `api_key_env` | string | none | Name of the env var holding the credential (never the value) |
| `google_project` | string | none | Google Cloud project id for `google-vertex-openai` when `base_url` is omitted |
| `google_location` | string | none | Vertex AI location for `google-vertex-openai` when `base_url` is omitted |
| `google_adc_path` | string | ADC default | Optional path to a gcloud ADC `authorized_user` file |
| `model` | string | none | Default model when a command does not pass `--model` |
| `request_timeout_secs` | int | per adapter | HTTP timeout; useful for slow local inference |
| `context_window` | int | none | The model's context window in tokens; when set, the session budget derives from it (window minus a response reserve) and takes precedence over `[harness] context_token_limit` |

Any other keys under a provider table are preserved and passed through as
provider options (for example `max_tokens` for `anthropic`, or the
LocalPilot-owned switches `suppress_thinking` and `reasoning_round_trip`). See
[providers.md](providers.md).

**Credentials are never config keys.** For `auth = "api_key"`, a provider's API
key is never written to config. It is resolved at use with precedence: a stored
credential (OS keychain → `0600` fallback file, written by `localpilot login`) →
the `api_key_env` environment variable → none. So `login` makes `api_key_env`
optional. For `auth = "google_adc"`, LocalPilot reads the ADC file path (not the
token value) and mints a short-lived Google OAuth access token in-process. The OS
keychain backend is an opt-in build feature (`keychain`, Windows only at present;
macOS/Linux use the fallback file for API-key credentials — ADR-0042). See
[providers.md](providers.md) §Storing credentials and
[07-security-and-privacy.md](07-security-and-privacy.md) §Stored API Credentials.

### `[harness]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mode` | `agent` \| `harness` | `agent` | Operating mode |
| `attempts_per_step` | int | `3` | Max attempts per plan step |
| `auto_commit` | bool | `true` | Commit each completed step |
| `test_command` | string | none | Command run to gate step completion |
| `tool_call_budget` | int | off | Soft start for the per-turn tool-call ceiling. A turn making forward progress runs past this up to `tool_call_budget_max`; a turn detected as making no progress stops here. Unset by default; set it to enable a progress-aware budget |
| `tool_call_budget_max` | int | built-in | Hard cost ceiling: the per-turn tool-call count that always stops the loop, regardless of progress. Setting either budget field enables an explicit budget (and an explicit value always wins). **When both are unset a conservative built-in ceiling applies** so a fresh project never runs an unbounded, externally-killed loop (D003 safety default, ADR-0055): **200** tool calls for a headless run (`eval`/`print`/`harness` step) and **500** for an interactive session. Raise it above `tool_call_budget` to let a productive turn extend |
| `claim_gate` | `off` \| `warn` | `off` | The no-unsupported-claim gate over the final reply. `warn` appends a visible, non-destructive note to a completed-action claim no verified tool call this turn supports (matched per claim); `off` skips it. Default `off` while its false-positive rate is measured (ADR-0023) |
| `turn_timeout_secs` | int | built-in (headless) | Bounded per-turn wall-clock timeout in seconds. When set, a turn that runs longer stops cleanly with a parseable `handoff:` summary instead of hanging — the bound a non-interactive caller (`print`) relies on. An explicit value always wins. **When unset, a headless run (`eval`/`print`/`harness` step) applies a conservative built-in bound of 600 s** so it self-bounds without a human watching (D003, ADR-0055); an interactive session sets no default wall-clock — a long interactive turn is legitimate and the user can cancel it. To run a headless turn without a wall-clock bound, set an explicit large value |
| `verify_before_done` | bool | `false` | Verify the workspace builds/tests before a turn finalizes. When on, a turn that would end with no tool call first runs a verification command; on failure the diagnostics are fed back and the loop continues (bounded by the budget/timeout rails plus a fixed re-entry cap) instead of "finishing" code that never compiled. Off by default (a feature lever); a workspace with no detectable target finalizes unchanged. See [06-harness-spec.md](06-harness-spec.md) §Verify-Before-Done Gate (ADR-0054) |
| `verify_command` | string | none | Override the verify-before-done command (a single command line, split on whitespace — no shell, like `test_command`). Unset resolves a command from the workspace stack (`cargo test`, `go test ./...`, `npm test`, `python -m pytest`, `mvn test`, `gradle test`, or `make`); set this for a non-standard build/test invocation, or for a stack the detector does not cover (e.g. a CMake-only C++ project) |
| `rules.<name>` | `off` \| `warn` \| `block` | — | Per-rule severity overrides |

Notable rule key:

| Rule | Default | Meaning |
| --- | --- | --- |
| `check_before_launch` | `warn` | When the task prompt named a local serveable target (a loopback host, or any `host:port` with an explicit port) that has not been probed this session, an attempt to launch a local server or scaffold a competing `index.html` is nudged (`warn`, the call still runs), refused (`block`), or ignored (`off`). Auto-extracted from the prompt — an external reference URL without a port is not a target. Advisory, tighten-only, best-effort. See [06-harness-spec.md](06-harness-spec.md). |

### `[context]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `project_analysis` | bool | `true` | Inject a compact, read-only project-facts block before each turn. LocalPilot derives it from manifests, lockfiles, package/dependency names, scripts, and common entrypoint markers so the model reuses existing project structure before inventing alternatives. |

### `[memory]`

Tunes always-on accepted-memory injection. Every default preserves the prior
fixed behaviour, so the section is additive and opt-in.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `injection_min_score` | int | `0` | Minimum retrieval score a memory must clear to be injected. `0` injects every match (prior behaviour); raise it so weak matches do not fill the per-turn budget. |
| `injection_char_budget` | int | `1200` | Char budget for the injected accepted-memory block, and the ceiling when `injection_context_aware` scales it down. |
| `injection_context_aware` | bool | `false` | Scale the injected budget toward the default provider's declared `context_window` (a small model gets less), never above `injection_char_budget`. |
| `injection_skip_categories` | list | `[]` | Lesson categories to skip injecting because a rule already enforces equivalent guidance (e.g. `["SecurityWarning"]`). Values match `LessonCategory` names. |

### `[docs]`

Controls when the agent should expand beyond local project facts into available
knowledge, docs, MCP, or tool-discovery surfaces. This does not grant any new
permission: network and MCP/tool calls still pass through the normal permission
engine.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `lookup_policy` | `local_only` \| `evidence` \| `proactive` | `evidence` | `local_only` keeps the model within repo/context unless the user asks for external information. `evidence` starts local and looks up docs/tools when local facts are insufficient, ambiguous, or a local attempt fails. `proactive` nudges the model to use available docs/MCP/tool discovery early for package or framework work. |

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

### `[tools]`

The pull-discovery broker (ADR-0031): narrow each turn's advertised tool schemas
to a small working set and resolve a need to the right tool on demand. Every key
defaults so an absent `[tools]` block reproduces prior behaviour exactly — the
broker is off and the full tool set is advertised.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `broker` | bool | `false` | Enable the broker. Off advertises the full registry (the rollback path); on narrows advertised schemas to the working set and resolves/reveals on a miss. |
| `core` | array of string | `[]` | The core working set always advertised when the broker is on. Empty uses the built-in default (a lean read/edit/search/shell set). |
| `working_set_cap` | int | `24` | Maximum revealed tools retained before LRU eviction. |
| `score_floor` | int | `1` | Minimum resolution score to reveal; below it a miss is a clean "no match". |
| `marker` | bool | `false` | Enable the loose `NEED: <capability>` marker trigger. Off by default; the always-on failure-driven trigger does not need it. |
| `learning` | bool | `false` | Re-rank by past success, graduate hot tools into the always-advertised set, and record redacted resolution telemetry. Off keeps the broker working with mechanical freshness only. |
| `graduation_threshold` | int | `3` | Reveals of one tool before it graduates into the always-advertised set (when `learning`). |
| `readable_errors` | bool | `true` | When a tool call's arguments do not match the tool schema, hand the model a concise, schema-aware error (the offending field, the expected shape, and a valid example) instead of the raw deserializer string, so it can self-correct on the next turn. Set `false` to restore the raw message (the rollback). The raw detail is always kept in the logs/telemetry. |
| `repair` | `off`/`warn`/`on` | `off` | Conservative, schema-guided repair of a shape-invalid call's arguments (wrap a bare string as a one-element array, parse a stringified array/object of the right item type, unwrap a markdown autolink on a path field) — applied **only** on read-only / project-write tools, **never** on a destructive/external-write/irreversible/MCP tool or a content/command field, and only to the validator-reported fields. A repaired call carries a model-visible note and runs through the permission engine on the repaired input. `warn` applies the repair and logs it loudly (for vetting before any default change to `on`); `off` (the default) never rewrites arguments — a shape-invalid call gets the readable error instead. |
| `repair_learning` | bool | `false` | Offer the session's argument-repair patterns to LocalMind as aggregate, redacted, **review-gated** candidates at session close (which model needed which repair on which tool). Reuse-only: it stores no raw inputs/paths/content, writes no accepted memory, and adds no new store — a human promotes a candidate or it expires in review. Off by default. |

**Migration:** these defaults reproduce prior behaviour, so an existing config
keeps working unchanged. Opt in with `[tools] broker = true`; see
[05-tool-system.md](05-tool-system.md) §Pull-Discovery Broker.

`readable_errors` defaults **on** — a pure message improvement with no behaviour
change beyond the text the model reads — so a shape-invalid tool call is answered
with an actionable, schema-aware correction by default.

### `[history]`

Durable prompt history for the interactive `chat` composer: submitted prompts are
persisted so Up/Down recall survives a restart (ADR-0040).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `persistence` | `"save-all"` \| `"none"` | `"save-all"` | `save-all` persists each submitted prompt and seeds recall at startup; `none` is a full opt-out — no read, no write, no file created. |

Recall is scoped to the current directory by default; **Ctrl-T** toggles a view of
every project's history. The store is one global `prompt-history.jsonl` under the
per-user directory beside this config file, mode `0600` on unix. Because prompts
are stored **raw** (not redacted — recall must be faithful), the opt-out and the
restrictive mode/location are the privacy controls; see
[07-security-and-privacy.md](07-security-and-privacy.md) §Prompt History At Rest.

### `[self_improvement]`

The **outward** half of the human-gated self-improvement loop (ADR-0053, extending
ADR-0034): the agent may author a **draft** issue/PR proposing an improvement, but
publishing one to an external repo is gated. Both keys ship **off**, so an absent
`[self_improvement]` block leaves the surface inert — nothing is publishable.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | The explicit feature switch for the outward `propose-issue`/`propose-pr`/`emit-draft` commands. Off refuses them entirely. |
| `outward_targets` | array of string | `[]` | The allowlist of `owner/repo` targets a draft may be proposed for or published to. Empty → nothing is publishable even when `enabled = true`. A target outside this list is refused at propose time, before any draft is written. |

A draft is publishable only when `enabled = true` **and** its target is on
`outward_targets` (fail-closed: both are required). Even then, publishing is
**draft-only** (`gh issue create` / `gh pr create --draft`, never ready/merge),
**dry-run by default** (`emit-draft` prints the `gh` plan and publishes nothing
unless `--approve` is passed), and requires an explicit human approval — the
autonomous loop can author a draft but can never publish one. See
[07-security-and-privacy.md](07-security-and-privacy.md) §Outward Draft Emission.

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

[context]
project_analysis = true

[docs]
lookup_policy = "evidence"

[compaction]
mode = "deterministic"

[permissions]
profile = "default"

[quota]
auto_resume = "ask"

[mcp.servers.files]
command = "my-mcp-file-server"
args = ["--root", "."]

[history]
persistence = "save-all"
```
