# OpenCode vs LocalPilot: Research Notes and Recommendations

Date: 2026-06-07  
OpenCode repo inspected: `D:\repos\opencode` at `e82542b80`  
LocalPilot repo inspected: `D:\repos\rust\LocalPilot` at `94d820d`

## Executive Summary

OpenCode is much larger and more platform-shaped than LocalPilot. It is not just
a terminal coding agent; it is a multi-surface agent platform with a CLI, TUI,
web/desktop clients, SDK, HTTP API, sharing service, provider catalog, plugin
loader, MCP integration, LSP tooling, background subagents, docs site, release
infrastructure, and a broad test suite.

LocalPilot is smaller, more opinionated, and more focused. Its strongest ideas
are not the same as OpenCode's. LocalPilot is a Rust-native, local-first,
provider-neutral coding-agent harness with an explicit permission engine,
rule-enforced harness mode, quota pause/resume, bad-output recovery, and
LocalMind-backed local memory/learning. Those are real differentiators. The
right move is not to clone OpenCode. The right move is to selectively adopt the
platform mechanics OpenCode has proven useful, while keeping LocalPilot's
harness, local-first privacy model, Rust portability, and explicit safety model.

The biggest opportunities for LocalPilot are:

1. Add a durable local session API/event server so CLI, TUI, future desktop/web,
   tests, and automation all drive the same runtime.
2. Upgrade the session model from an in-memory message vector plus transcript
   append into an event-sourced durable state machine with resumable activity,
   stable tool-call records, and replayable UI events.
3. Build a real provider/model catalog with capability flags, model variants,
   costs, limits, auth metadata, and provider policies.
4. Add OpenCode-style agent definitions and subagents, but adapt them to
   LocalPilot's permission and harness contracts.
5. Expand tools where OpenCode is plainly ahead: `apply_patch`, `webfetch`,
   optional web search, LSP/symbol tools, richer file discovery, tool output
   retention, and snapshot/revert.
6. Mature MCP beyond local stdio: remote transports, OAuth, resources, prompts,
   status surfaces, and dynamic tools.
7. Create a scoped plugin system for providers, tools, agents, context sources,
   skills, and UI/runtime hooks.
8. Invest in product packaging: generated config schema, docs site, installers,
   command completion, upgrade flow, SDKs, and a thin VS Code bridge.

The main caution: OpenCode's broader surface also brings complexity and more
cloud/product assumptions. LocalPilot should avoid adopting session sharing,
telemetry, remote services, or broad plugin authority by default. Make every
external/remote behavior opt-in and permissioned.

## Method

This was a local codebase comparison, not a benchmark run. I inspected:

- OpenCode docs and specs: `README.md`, `CONTEXT.md`, `specs/project.md`,
  `specs/v2/config.md`, `specs/v2/tools.md`,
  `specs/v2/provider-model.md`.
- OpenCode implementation: `packages/core/src/session/runner/llm.ts`,
  `packages/core/src/tool/*`, `packages/core/src/permission.ts`,
  `packages/core/src/agent.ts`, `packages/core/src/config.ts`,
  `packages/core/src/plugin.ts`, `packages/opencode/src/tool/*`,
  `packages/opencode/src/mcp/index.ts`, `packages/opencode/src/skill/*`,
  `packages/opencode/src/share/*`, CLI entrypoints, SDK/package layout.
- LocalPilot docs: `README.md`, `docs/01-product-spec.md`,
  `docs/02-architecture.md`, `docs/05-tool-system.md`,
  `docs/06-harness-spec.md`, `docs/providers.md`, `docs/mcp.md`,
  `docs/localmind-integration.md`, `docs/08-testing.md`,
  `docs/12-feature-specs.md`, `docs/14-dev-tooling.md`.
- LocalPilot implementation: `crates/localpilot-harness/src/session.rs`,
  `crates/localpilot-harness/src/rules.rs`,
  `crates/localpilot-tools/src/builtins.rs`,
  `crates/localpilot-sandbox/src/permission.rs`,
  `crates/localpilot-llm/src/provider.rs`,
  `crates/localpilot-config/src/schema.rs`,
  `crates/localpilot-cli/src/main.rs`,
  `crates/localpilot-cli/src/repl.rs`,
  MCP, LocalMind, store, skills, recovery, quota crates.

I did not run live model/provider tests. I also did not mutate either repository
except creating this report at `D:\repos\opencode-vs-localpilot.md`.

## Scale and Maturity Snapshot

| Area | OpenCode | LocalPilot |
| --- | --- | --- |
| Rough tracked files from `rg --files` | 5,597 | 238 |
| Rough lines in inspected source/docs types | ~821,979 across 2,943 TS/TSX/MD/JSON/RS files | ~31,817 across 217 RS/MD/TOML files |
| Main implementation stack | TypeScript/Bun/Effect/Solid/OpenTUI/Electron-ish desktop/web packages | Rust/Tokio/ratatui/crossterm/clap |
| Runtime shape | Agent platform with server, event projectors, database, clients | Local CLI/TUI plus harness runtime |
| Provider breadth | Very broad provider plugin catalog | OpenAI-compatible, OpenAI, Anthropic-oriented provider runtime |
| User surfaces | CLI, TUI, web, desktop, docs site, SDK, VS Code extension, server | CLI, optional ratatui chat REPL, docs |
| Strongest product idea | Broad open agent platform | Rule-enforced local coding harness plus LocalMind learning |
| Strongest engineering idea | Durable sessions, event streams, tool settlement, provider catalog, pluginized extension points | Safety-first Rust crates, permission/harness/recovery/quota/local-memory boundaries |

OpenCode is far more mature as an ecosystem. LocalPilot is more coherent as a
focused local-first harness.

## Product Positioning

### OpenCode

OpenCode describes itself as "the open source AI coding agent", but the local
repo shows a product broader than a CLI:

- Multiple clients and packages: `packages/app`, `packages/web`,
  `packages/desktop`, `packages/tui`, `packages/ui`, `packages/docs`,
  `packages/sdk/js`, `sdks/vscode`.
- Runtime/server split: `packages/opencode/src/server`, public routes, SDK
  OpenAPI, project/session APIs, event streams.
- Multi-project/session model in `specs/project.md`.
- Session sharing and enterprise/share configuration.
- Broad install/distribution story in `README.md`: install script, npm, Scoop,
  Chocolatey, Homebrew, Pacman/AUR, mise, Nix, desktop downloads.
- Many translated READMEs.

OpenCode's product bet is: one agent runtime should power many interfaces,
providers, projects, worktrees, agents, and integrations.

### LocalPilot

LocalPilot's docs position it as "a Rust-native, provider-neutral coding-agent
harness." The real distinction is workflow control:

- Agent mode for normal conversational work.
- Harness mode for deterministic idea -> brief -> plan -> step execution.
- `brief.md`, `PROGRESS.md`, and `DECISIONS.md` as user-editable source of
  truth.
- Rule engine with blocking/warning/retry/discard verdicts.
- Per-step quality gate and commit policy.
- Explicit permission engine for paths, shell, network, secrets, trust, and
  interactivity.
- Bad-output recovery and quota wait/resume.
- LocalMind integration for local-only learning, review, memory, skills, and
  audit.

LocalPilot's product bet is: an agent should be auditable, local-first,
recoverable, and useful with local models.

## Where OpenCode Is Clearly Ahead

### 1. Platform Surface

OpenCode has an ecosystem shape LocalPilot does not yet have:

- CLI command suite: `run`, `serve`, `web`, `session`, `providers`, `models`,
  `agent`, `mcp`, `github`, `pr`, `import`, `export`, `plug`, `db`, `stats`.
- Server routes and project/session APIs.
- Generated JavaScript SDK in `packages/sdk/js` from `openapi.json`.
- VS Code extension under `sdks/vscode`.
- Desktop and web packages.
- Storybook/UI package.
- Docs website package.
- Release and installer scripts.

LocalPilot currently has strong CLI commands, but no runtime API/server, SDK,
web client, desktop client, or IDE bridge. Adding these would make LocalPilot
usable by more than one terminal process and would unlock automation, testing,
and external UI work.

### 2. Durable Session Runtime

OpenCode's v2 runner in `packages/core/src/session/runner/llm.ts` is notably
careful:

- Promotes queued/steering user input at safe boundaries.
- Initializes and prepares durable context epochs.
- Loads projected history for a runner turn.
- Materializes tools per agent permissions.
- Streams provider events into a publisher.
- Persists assistant text, reasoning, provider errors, tool calls, usage, and
  tool results as events.
- Records local tool calls before side effects begin.
- Settles local tools through a registry, then continues the provider loop.
- Handles context overflow with compaction and rebuild.
- Bounds the number of tool/LLM steps.
- Handles unsettled tools on provider failure/interruption.

LocalPilot's `SessionRuntime` is well structured and testable, but simpler:

- It stores `messages: Vec<Message>` in memory.
- It appends redacted messages to `Store`.
- It compacts before provider requests.
- It collects streamed text/reasoning/tool calls.
- It persists the assistant message after stream completion.
- It executes tools serially and appends tool results.

The LocalPilot runtime is easier to reason about, but less durable. Cancellation
claims "no partial message is persisted", which is clean, but it also means
there is no granular persisted event trail of partial assistant output or tool
activity. OpenCode's model is better for replay, crash recovery, multi-client
UI, server APIs, and debugging.

### 3. System Context Model

OpenCode's `CONTEXT.md` is one of the most useful design artifacts in the repo.
It names and formalizes:

- System Context
- Context Sources
- Context Snapshot
- Context Epoch
- Baseline System Context
- Mid-Conversation System Messages
- Safe Provider-Turn Boundary
- Unavailable Context
- context replacement on compaction/model/agent switch

This is more advanced than LocalPilot's current `seed_system` and
memory-injection path. LocalPilot injects accepted memory as best-effort system
messages and keeps a simple compaction summary. That is useful, but it does not
yet give the runtime a first-class model for changing ambient context,
instruction files, provider/cache prefixes, skill guidance, local memory,
workspace state, date/time, project trust, or agent mode.

LocalPilot should adopt the concept, not necessarily the implementation:

- Define typed `ContextSource`s in Rust.
- Persist a baseline context per session epoch.
- Reconcile dynamic context only at safe provider-turn boundaries.
- Represent memory, skills, project instructions, trust state, harness step,
  quota state, and mode as sources.
- Start a new epoch on compaction, provider/model switch, agent switch, or
  harness step boundary.

This would make LocalPilot's local memory and harness context much more robust.

### 4. Provider and Model Catalog

OpenCode has a much richer provider/model catalog design:

- Providers have IDs, names, enabled state, env metadata, endpoints, options.
- Models have provider IDs, API IDs, capabilities, variants, release time,
  cost, status, limits, endpoint, and options.
- Provider plugins include many providers:
  `alibaba`, `amazon-bedrock`, `anthropic`, `azure`, `cerebras`,
  `cloudflare-ai-gateway`, `cloudflare-workers-ai`, `cohere`, `deepinfra`,
  `gateway`, `github-copilot`, `gitlab`, `google`, `google-vertex`, `groq`,
  `kilo`, `llmgateway`, `mistral`, `nvidia`, `openai-compatible`, `openai`,
  `opencode`, `openrouter`, `perplexity`, `sap-ai-core`,
  `snowflake-cortex`, `togetherai`, `venice`, `vercel`, `xai`, `zenmux`.
- Provider policy can allow/deny provider use.
- Agents can override model/request options.

LocalPilot's provider abstraction is intentionally clean and provider-neutral.
It already models source type, auth requirement, tool-call shape, reasoning
shape, capability flags, context limit, and quota behavior. That is a good core.
But the user-facing provider catalog is small and static compared with
OpenCode.

High-value additions:

- A built-in catalog of provider templates and known model defaults.
- Model capability detection/selection independent of provider name.
- Cost and context metadata for UI/footer, planning, and LocalBench.
- Model variants for reasoning effort, tool mode, low-latency mode, etc.
- Policy rules for provider/model allow/deny.
- Provider health/degraded state persisted across sessions.
- Import from a machine-readable catalog, ideally generated and tested.

### 5. Agent and Subagent System

OpenCode has built-in `build`, `plan`, `title`, and subagent-style concepts. In
the current repo it also has a `task` tool that can launch a subagent session,
derive subagent permissions from parent/agent rules, run foreground or
background, inject background results, and continue the parent session.

LocalPilot has two operating modes, but not a comparable multi-agent system.
The mode split is valuable, especially `harness`, but LocalPilot could benefit
from agent definitions:

- `build`: default editing agent.
- `plan`: read-only analysis agent.
- `review`: code review agent.
- `test`: quality-gate diagnosis agent.
- `memory`: LocalMind closeout/retrieval agent.
- `harness-worker`: constrained step executor.
- `harness-planner`: planner/replanner.
- user-defined/project-defined agents.

OpenCode's task/subagent model would be especially useful for LocalPilot's
harness:

- Planner can delegate codebase reconnaissance to a read-only subagent.
- Worker can ask a review subagent to inspect a diff before step completion.
- A quality-gate failure can spawn a targeted diagnostic subagent.
- LocalMind can generate skill/memory summaries as background work after a
  session exits.

LocalPilot should not blindly copy background concurrency. It should enforce
workspace conflict rules: background subagents must be read-only by default or
bound to non-overlapping file scopes.

### 6. Tool Breadth and Tool Ergonomics

OpenCode has a broader tool set:

- `bash` / shell
- `read`
- `glob`
- `grep`
- `edit`
- `write`
- `apply_patch`
- `question`
- `todowrite`
- `webfetch`
- `websearch`
- `skill`
- `task`
- optional `lsp`
- plan enter/exit in legacy path
- MCP tools
- plugin tools

LocalPilot's builtins are solid and safer in some places:

- `read_file`
- `write_file`
- `edit_file`
- `multi_edit`
- `list_files`
- `find_files`
- `search_text`
- `run_shell` as program + args, no shell interpretation
- git status/diff/log/add/restore/commit
- `update_plan`
- MCP tools via stdio

OpenCode is ahead in:

- `apply_patch` as a first-class edit mechanism.
- `webfetch` and configurable web search.
- LSP/symbol operations.
- `task` subagents.
- richer plugin tool discovery.
- tool-output retention files.
- tool filtering based on model/provider capability.
- dynamic tool definitions that include skill/subagent descriptions.

LocalPilot should add these in a LocalPilot-native way:

- Keep `run_shell` as argv, not a string shell, for safety.
- Add `apply_patch` with strict grammar and workspace-boundary checks.
- Add `webfetch` as network-gated and disabled by default in harness.
- Add optional web search only through explicit provider/user configuration.
- Add LSP tools through `rust-analyzer`/language servers and permission gates.
- Add output spooling/managed output references for large command/search output.
- Add a `task`/subagent tool only after durable sessions exist.

### 7. MCP Implementation

LocalPilot currently supports local stdio MCP servers. That is a good safe v1:
servers are launched as subprocesses, discovered tools are registered alongside
builtins, and calls are gated through the same permission engine.

OpenCode's MCP implementation is broader:

- Stdio, streamable HTTP, and SSE transports.
- Remote URL parsing.
- OAuth flow and callback support.
- stored token/auth status.
- tool list changed notifications.
- resources and prompts.
- status states: connected, disabled, failed, needs auth,
  needs client registration.
- tolerant fallback when MCP output-schema validation fails.
- commands for auth/connect/disconnect/status.

Recommended LocalPilot additions:

1. Add MCP status surfaces before remote transports.
2. Add resources and prompts; expose resource reads through permission and
   redaction.
3. Add remote MCP only after OAuth/token storage policy is designed.
4. Treat remote MCP as network effect and off by default in harness.
5. Persist MCP server status and last error in `doctor`.

### 8. Plugin System

OpenCode has two plugin generations visible in the repo:

- Legacy/plugin package loading in `packages/opencode/src/plugin`.
- V2 `PluginV2` hooks in `packages/core/src/plugin.ts`.

Plugin hooks cover catalog transforms, account switching, AI SDK language/model
customization, and SDK construction. Legacy paths also support configured
plugins and custom tool modules from `{tool,tools}/*.{js,ts}`.

LocalPilot has extensibility docs and clean traits, but not a general plugin
system. This is a major gap if LocalPilot wants an ecosystem.

Recommended LocalPilot plugin design:

- Use a narrow capability model, not arbitrary full-process hooks.
- Start with manifest-based plugins under `.localpilot/plugins/<name>/`.
- Plugin capabilities:
  - register tools
  - register providers
  - register agents
  - register context sources
  - register quality-gate profiles
  - contribute skills
- Every plugin declares permissions in a manifest.
- Project plugins are disabled until trusted.
- Prefer WASI/process boundary later for untrusted plugins; initially support
  Rust/native or external-command plugins only for trusted local development.

OpenCode's lesson: plugin order, scope, and lifecycle matter. LocalPilot should
design those from the start.

### 9. LSP and Code Intelligence

OpenCode has an LSP subsystem and optional `lsp` tool operations:

- go to definition
- find references
- hover
- document symbols
- workspace symbols
- implementation
- call hierarchy

LocalPilot's current file/search tools are useful but text-first. For Rust and
large repos, LSP will materially improve performance and accuracy:

- Better symbol navigation than regex search.
- Better change planning before edits.
- Ability to surface diagnostics after edits.
- Better test selection and dependency impact analysis.

Recommended LocalPilot sequence:

1. Add `localpilot-lsp` crate or module.
2. Start with read-only operations and `rust-analyzer`.
3. Add diagnostics ingestion for quality gate context.
4. Add config for built-in and custom language servers.
5. Gate every LSP operation with read permissions and workspace boundaries.

### 10. Session Sharing, Import/Export, and Replay

OpenCode supports session share/unshare and automatic sharing. This is useful
for collaboration and product growth, but it does not align perfectly with
LocalPilot's local-first privacy stance.

LocalPilot already has redacted session export. That is the right base.

Recommended approach:

- Do not add cloud sharing by default.
- First add local replay: `localpilot session replay <id>` and a stable JSONL
  event format.
- Add "share bundle" generation: redacted HTML/Markdown artifact from local
  transcript and events.
- Later, support user-configured share backends as plugins.
- Keep cloud/off-machine sharing opt-in and visibly labeled.

### 11. Docs, Packaging, and Developer Experience

OpenCode is ahead in distribution and documentation:

- Many install paths.
- Desktop downloads.
- Docs app/package.
- SDK package.
- OpenAPI.
- README translations.
- Release scripts and infrastructure.
- Nix support.
- VS Code extension.

LocalPilot has good technical docs, but they read more like an engineering spec
than a product documentation site. To compete, LocalPilot needs:

- `localpilot.dev` or GitHub Pages docs.
- One-page quickstart for local model users.
- Provider setup pages with tested examples.
- Harness tutorial with screenshots/transcripts.
- `doctor` troubleshooting guide.
- generated `.localpilot.toml` JSON schema or TOML schema docs.
- shell completions.
- binary releases/installers.
- "local-first/privacy" page.
- "OpenCode/Codex/Claude Code comparison" page that states the differentiated
  harness value.

## What LocalPilot Already Does Better

### 1. Deterministic Harness Workflow

OpenCode has plan/build agent modes and todo tools, but LocalPilot has a more
explicit harness specification:

- `brief.md` as durable requirements.
- `PROGRESS.md` as durable plan and step state.
- `DECISIONS.md` for deviations.
- rule engine verdicts.
- per-step attempt limits.
- quality-gate checks.
- commit policy.
- quota-safe pause/resume.
- anti-sunk-cost replan loop.

This is LocalPilot's most important differentiator. Do not dilute it into a
generic chat agent clone.

### 2. Safety-First Permission Model

LocalPilot's permission engine is simple, explicit, and cross-platform:

- Effects are typed: read path, write path, run command, network.
- Workspace boundary is enforced even under `bypass`.
- Secret-like paths prompt/deny.
- Non-interactive mode denies asks.
- Commands are classified read-only/project-write/external-write/network/
  destructive/privileged/unknown.
- Shell execution uses program + args, not a generated shell string.

OpenCode has a powerful permission system, but LocalPilot's model is easier to
audit. Keep that advantage.

### 3. Local-First Memory and Learning

OpenCode has skills, skill discovery, instruction context, and session sharing,
but LocalPilot's LocalMind integration is a stronger local-learning story:

- Close out sessions into candidate lessons.
- Review queue.
- Promote accepted memory.
- Search accepted memory.
- Generate disabled skill drafts.
- Audit changes.
- Store project-local readable Markdown plus SQLite index.
- Inject relevant accepted memory before turns.

This is more aligned with local-first coding than cloud session sharing. It is a
good strategic pillar.

### 4. Bad-Output Recovery

LocalPilot explicitly detects and recovers from local-model degeneration:

- slash floods
- repeated-token loops
- empty/bad turns
- malformed structured output
- stream decode failures
- provider transient errors
- model health degradation

OpenCode has robust session/runtime failure handling, but LocalPilot's recovery
layer is more explicitly tuned for local model failure modes. This matters for
the LocalX ecosystem.

### 5. Quota Pause/Resume

LocalPilot models provider quota metadata and has a quota wait/resume policy.
OpenCode has retries/overflow handling and provider errors, but LocalPilot's
explicit "pause at safe boundary and resume when allowed" product feature is
valuable and unusual.

### 6. Rust-Native Cross-Platform Focus

LocalPilot is much smaller and can be easier to ship as a single native binary.
The Rust crate boundaries are clear:

- `localpilot-cli`
- `localpilot-core`
- `localpilot-config`
- `localpilot-llm`
- `localpilot-tools`
- `localpilot-harness`
- `localpilot-tui`
- `localpilot-store`
- `localpilot-sandbox`
- `localpilot-mcp`
- `localpilot-skills`
- `localpilot-recovery`
- `localpilot-quota`
- `localpilot-localmind`

OpenCode has much more product surface, but also much more Node/Bun/package
complexity. LocalPilot should exploit the single-binary story.

## Feature Matrix

| Capability | OpenCode | LocalPilot | Recommendation |
| --- | --- | --- | --- |
| CLI | Mature, broad command set | Solid but narrower | Add session/provider/agent/plugin/API commands |
| TUI | Rich OpenTUI/Solid stack | Ratatui REPL alpha | Keep ratatui, improve with event replay and durable runtime |
| Web/desktop | Present | Absent | Add only after local API/server exists |
| VS Code | Extension present | Absent | Add thin bridge to local server later |
| HTTP API | Present/design visible | Absent | High priority |
| SDK | JS SDK generated | Absent | Generate after API stabilizes |
| Session durability | Evented DB/projectors/context epochs | Store append plus in-memory runtime | High priority |
| Tool settlement | Durable call records, output bounding | Tool dispatch/results appended | Add durable tool-call lifecycle |
| Context sources | Formal context epoch model | seed system + compaction | Adopt typed context source/epoch model |
| Provider catalog | Very broad | Clean but small | Add catalog, model metadata, variants, policies |
| Agents/subagents | Built-in agents, task tool, background jobs | Modes only | Add agent definitions and read-only subagents |
| Harness workflow | Less deterministic | Strong differentiator | Preserve and deepen |
| MCP | Stdio + remote + OAuth + resources/prompts | Stdio tools only | Expand gradually |
| Plugins | Significant | Not general yet | Add capability-scoped plugin system |
| LSP | Present/optional tool | Absent | Add read-only LSP tools |
| Web fetch/search | Present | Absent | Add opt-in network-gated tools |
| Local memory | Skills/context, no LocalMind equivalent | Strong LocalMind integration | Keep pushing this |
| Sharing | Present | Export only | Prefer local replay/share bundle first |
| Recovery | Runtime failure handling | Strong bad-output recovery | Keep LocalPilot approach |
| Quota wait/resume | Less central | Explicit feature | Keep and integrate into durable events |
| Install/distribution | Strong | Basic source/install scripts | Improve packaging |
| Docs | Product docs site and translations | Strong specs, less product docs | Add user docs site |

## High-Value Additions for LocalPilot

### Priority 1: Local Session Server and Event API

Build a local-only daemon/server before web/desktop/IDE work.

Core endpoints:

- `GET /health`
- `GET /projects`
- `POST /projects/init`
- `GET /sessions`
- `POST /sessions`
- `GET /sessions/:id`
- `POST /sessions/:id/input`
- `POST /sessions/:id/cancel`
- `POST /sessions/:id/compact`
- `GET /sessions/:id/events`
- `GET /sessions/:id/messages`
- `GET /sessions/:id/tool-output/:id`
- `GET /providers`
- `GET /models`
- `GET /tools`
- `GET /mcp/status`
- `POST /permissions/:id/reply`

Implementation direction:

- New crate: `localpilot-server`.
- Use `axum` or `poem`.
- Bind to localhost by default.
- Require a local auth token for non-stdio clients.
- Use SSE for events first; WebSocket later only if needed.
- TUI can remain direct initially, then migrate to the same API.

Why this matters:

- Enables desktop/web/VS Code without duplicating runtime.
- Enables replay and automation.
- Forces durable session semantics.
- Makes tests closer to real product behavior.

### Priority 2: Event-Sourced Session Store

Introduce a durable `SessionEvent` log. Do not rely only on storing final
messages.

Events should include:

- session created/opened
- user input admitted/promoted
- context epoch initialized/replaced
- provider turn started/ended
- assistant text delta
- reasoning delta
- usage delta
- tool call recorded
- permission requested/replied
- tool execution started/ended/failed
- model/provider warning
- recovery diagnostic
- quota pause/resume
- compaction started/ended
- harness step started/completed/blocked
- quality check started/ended
- cancellation

Then derive views:

- transcript
- TUI state
- harness progress
- permission pending list
- provider usage
- tool outputs
- replay artifact

This is the biggest architectural lesson from OpenCode.

### Priority 3: Context Source and Epoch Model

Add typed context sources:

- date/time
- workspace/project metadata
- git status summary
- project instructions (`AGENTS.md`, future `LOCALPILOT.md`)
- configured mode/permission profile
- active harness step
- accepted LocalMind memories
- active skills
- provider/model capability summary
- quota state
- user/system config

Persist a baseline context per epoch and a snapshot used to decide whether a
mid-conversation update should be emitted. This will make LocalMind memory
injection, skill guidance, and harness step changes safer.

### Priority 4: Provider/Model Catalog

Extend `localpilot-llm` and `localpilot-config`:

- Provider templates for local OpenAI-compatible, OpenAI, Anthropic,
  OpenRouter, Google, Azure, Groq, Bedrock, Mistral, Together, xAI, etc.
- Known model entries with:
  - capabilities
  - context/output limits
  - tool-call shape
  - reasoning support
  - cost metadata
  - local/server compatibility hints
- Model variants:
  - fast
  - high-reasoning
  - low-cost
  - no-tools
  - suppress-thinking
- Provider/model policy:
  - allow/deny by provider/model
  - harness-safe provider list
  - local-only mode

Tie this into `doctor`, footer stats, LocalBench, and quality/eval runs.

### Priority 5: Agent Definitions and Subagents

Add config:

```toml
[agents.plan]
mode = "primary"
description = "Read-only planning and code exploration"
permissions = [{ action = "tool.write", resource = "*", effect = "deny" }]

[agents.review]
mode = "subagent"
description = "Review diffs for bugs and missing tests"
```

Start with foreground subagents only:

- `task` tool launches a child session.
- Child session has derived restricted permissions.
- Parent gets a structured result.
- No background writes in v1.

Later:

- Background read-only subagents.
- Background LocalMind closeout.
- Background quality diagnostics.
- File-scope locking for write-capable subagents.

### Priority 6: Tool Upgrades

Add these tools in order:

1. `apply_patch`: strict patch grammar, workspace-bound writes, diff preview in
   interactive mode.
2. `webfetch`: HTTP/HTTPS, network-gated, max bytes, markdown/text conversion,
   disabled by default in harness unless configured.
3. `lsp`: read-only symbol operations, start with Rust.
4. `diagnostics`: collect current compiler/LSP diagnostics.
5. `snapshot`: create/list/revert local workspace snapshots.
6. `tool_output_read`: read retained full output by managed ID.
7. `task`: subagent session tool.

Also upgrade output handling:

- Cap by bytes and lines.
- Preserve head and tail for long outputs.
- Store full output in managed files.
- Redact before persistence.
- Refer to managed output by opaque ID rather than raw path.

### Priority 7: MCP v2

LocalPilot MCP roadmap:

1. Status model and `doctor` integration.
2. Resources and prompts support.
3. Tool-list changed notifications.
4. Per-server timeout config.
5. Remote HTTP/SSE support.
6. OAuth/token storage with redaction and audit.
7. MCP command group:
   - `localpilot mcp status`
   - `localpilot mcp connect`
   - `localpilot mcp disconnect`
   - `localpilot mcp auth`
   - `localpilot mcp resources`

Keep remote MCP opt-in and visibly network-gated.

### Priority 8: Plugin System

Start narrow:

```toml
[[plugins]]
name = "local-tools"
path = ".localpilot/plugins/local-tools"
enabled = true
```

Manifest:

```toml
name = "local-tools"
version = "0.1.0"
capabilities = ["tools"]
permissions = [
  { action = "tool.register", resource = "*", effect = "ask" }
]
```

Initial plugin types:

- external-command tools
- provider definitions
- agent definitions
- context source files
- quality gate profiles
- skill directories

Do not give plugins arbitrary access to internal Rust services until the trust
model is mature.

### Priority 9: Product Packaging

Adopt from OpenCode:

- generated completions
- generated config schema
- install script that respects install dir env vars
- Scoop/Homebrew/Nix packaging
- prebuilt GitHub releases
- docs site
- screenshots/GIFs of harness and REPL
- `localpilot upgrade` that works from binary installs, not only source
- changelog automation

## Things OpenCode Has That LocalPilot Does Not

This is a non-exhaustive inventory of useful gaps:

- Local HTTP server/API.
- Generated SDK.
- Web client.
- Desktop client.
- VS Code extension.
- Session sharing/unsharing.
- Project/worktree API.
- Rich provider/model catalog.
- Many provider integrations.
- Model variants and cost/limit metadata.
- Agent definitions beyond mode.
- Subagents/task tool.
- Background jobs.
- LSP tool.
- Web fetch/search tools.
- Remote MCP and OAuth.
- MCP resources/prompts.
- Plugin tools and plugin hook system.
- Remote skill discovery.
- Snapshot/revert system.
- Tool output retention store.
- Event projector architecture.
- Context source/epoch abstraction.
- File watcher integration.
- Formatter/LSP config surfaces.
- Config migration and JSONC config.
- Docs site and translations.
- Storybook/UI package.
- Release infra for many distribution channels.

## Things LocalPilot Has That OpenCode Does Not Emphasize As Strongly

- Rule-enforced harness mode as a first-class product surface.
- `brief.md` / `PROGRESS.md` / `DECISIONS.md` contracts.
- Explicit anti-sunk-cost retry/discard/replan loop.
- LocalMind-backed reviewable memory and skill generation.
- Local-first learning with user review and audit.
- Bad-output recovery tuned for local models.
- Quota wait/resume as a first-class workflow.
- Rust-native single-binary path.
- Program-plus-args shell tool rather than shell-string execution.
- Workspace boundary enforced even under bypass.
- Strong clean-room/provenance orientation in docs.

These should remain part of LocalPilot's identity.

## Proposed Roadmap

### Phase 1: Runtime Durability Foundation

Goal: make one LocalPilot session replayable, resumable, and externally
observable.

Work:

- Add `SessionEvent` enum and event log in `localpilot-store`.
- Emit events from `SessionRuntime`.
- Derive transcript from events.
- Persist tool-call lifecycle before executing tools.
- Add managed tool output store.
- Add local session replay command.
- Keep TUI behavior unchanged where possible.

Acceptance:

- A cancelled/crashed run can explain exactly what happened.
- TUI can rebuild from event history.
- Export includes events and transcript.
- Existing tests pass plus event-log roundtrip tests.

### Phase 2: Local API Server

Goal: one runtime can serve CLI/TUI/automation/IDE.

Work:

- Add `localpilot-server`.
- Add local token auth.
- Add session CRUD and event stream.
- Add permission reply endpoint.
- Add provider/model/tools introspection endpoints.
- Add `localpilot serve`.

Acceptance:

- A simple script can create a session, send input, watch events, and cancel.
- TUI can optionally connect to the local server.
- API has generated OpenAPI.

### Phase 3: Provider Catalog and Model Metadata

Goal: make provider/model choice visible, testable, and policy-controlled.

Work:

- Add catalog file/schema.
- Add provider templates.
- Add model metadata.
- Add model variants.
- Add provider/model policy.
- Show cost/context/capability in `doctor` and TUI footer.

Acceptance:

- `localpilot models` lists usable models.
- `localpilot providers` explains missing credentials/config.
- Harness can require local-only or tool-capable models.

### Phase 4: Tool and Context Expansion

Goal: improve code navigation, edits, and context without weakening safety.

Work:

- Add context source/epoch model.
- Add `apply_patch`.
- Add `webfetch`.
- Add LSP read-only operations.
- Add snapshots/revert.
- Add tool output retention.

Acceptance:

- Model gets stable dynamic context updates.
- Large command outputs are retained safely.
- LSP can answer symbol queries in a Rust repo.
- Snapshot/revert can undo a step's workspace edits.

### Phase 5: Agents/Subagents and Plugins

Goal: make LocalPilot extensible without losing control.

Work:

- Add agent config.
- Add read-only `plan` and `review` agents.
- Add foreground `task` tool.
- Add plugin manifest and scoped registrations.
- Let plugins contribute tools/providers/agents/context/skills.

Acceptance:

- Planner can delegate read-only repo exploration.
- Review subagent can inspect a diff.
- Project plugin cannot run until trusted.
- Plugin permissions are visible in `doctor`.

### Phase 6: Product Surfaces

Goal: ship beyond the terminal.

Work:

- Docs site.
- Prebuilt binaries/installers.
- Shell completions.
- VS Code bridge to local server.
- Optional web UI.
- Local HTML share/replay bundle.

Acceptance:

- A new user can install and run against Ollama/OpenAI/Anthropic in minutes.
- A developer can inspect a LocalPilot session in browser/VS Code.
- No cloud sharing is required.

## Architectural Advice

### Keep the Rust Core Small and Typed

Do not port OpenCode's TypeScript architecture directly. The valuable lessons
are domain boundaries:

- session runner
- event projector
- context registry
- tool registry
- permission service
- provider catalog
- plugin lifecycle
- API/server

LocalPilot should express those as Rust crates/traits with strong types and
clear persistence contracts.

### Avoid Cloud/Product Coupling

OpenCode's share/enterprise/stats/account surfaces are useful for that product,
but they are not automatically good for LocalPilot. Keep:

- no hidden telemetry
- local state first
- remote behavior opt-in
- redacted export before share
- LocalMind review before memory promotion

### Make Harness Mode the North Star

Every borrowed feature should answer: how does this improve the harness?

- Session API: lets harness runs pause/resume/replay.
- Event log: makes harness decisions auditable.
- Provider catalog: lets harness choose safe-capable models.
- Subagents: lets harness separate planning/review/execution.
- LSP: lets harness inspect code more accurately.
- Snapshots: lets harness discard bad attempts safely.
- Plugins: lets teams encode local workflow without forking.

If a feature only makes LocalPilot a generic chat agent, it is lower priority.

## Concrete "Do Next" List

1. Write an ADR for event-sourced sessions and context epochs.
2. Add `localpilot-store` event log schema and tests.
3. Emit session events from `SessionRuntime` without changing behavior.
4. Add managed tool output retention and update tool dispatch to use it.
5. Add `localpilot session list/show/replay/export`.
6. Add `localpilot serve` with read-only session/event endpoints.
7. Add provider/model catalog commands.
8. Add `apply_patch` tool.
9. Add context sources for project instructions and LocalMind memory.
10. Add read-only `plan` agent and permissions.

This order improves reliability and unlocks later UI/API/plugin work without
pulling LocalPilot away from its harness identity.

## Bottom Line

OpenCode does the "agent platform" part better. It has broader provider
coverage, more interfaces, a better packaging story, richer MCP/plugin/tool
surfaces, durable session machinery, and a stronger ecosystem around the core
agent.

LocalPilot does the "local, auditable engineering harness" part better. Its
rule engine, permission model, LocalMind integration, quota handling, recovery
layer, and Rust-native design are valuable and differentiated.

The best path is to absorb OpenCode's platform lessons in this order:

1. Durable events and context epochs.
2. Local session API/server.
3. Provider/model catalog.
4. Tool/LSP/MCP upgrades.
5. Agents/subagents.
6. Scoped plugins.
7. Product packaging and IDE/web surfaces.

Do that while keeping LocalPilot's local-first, safety-first, harness-first
contract intact.
