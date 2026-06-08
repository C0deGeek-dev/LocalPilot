# LocalPilot Next-Phase Research: Lessons from OpenCode and Pi

Date: 2026-06-08

This is a standalone, source-level study of two mature open-source AI coding
agents — **OpenCode** and **Pi** — read against **LocalPilot**, to decide what
LocalPilot's next phase should and should not build. It is self-contained: no
external document is required to read or act on it.

What this document does:
- **Studies three agents** at the source level and **triangulates** the two
  independent open-source agents against each other — where two teams who
  disagree on almost everything still converge, that convergence is a
  high-confidence signal for LocalPilot.
- States an explicit **identity contract** (§0) that every recommendation must
  pass, so the next phase strengthens LocalPilot's core instead of diluting it.
- Organizes the resulting work into self-contained **workstreams W0–W6**, defined
  and made authoritative in **§9**. Every "W1…W6" reference resolves there.

---

## 0. North Star — the identity contract

> **This section governs every other section.** If a recommendation below fails
> this contract, it does not get built, no matter how many other agents ship it.

### The one line

**LocalPilot is the agent you hand an ambiguous task and walk away from — because
it leaves an auditable trail, cannot do anything you did not permit, and runs
entirely on your machine.**

The compression of that: **accountable autonomy.**

### Why this is defensible (the trades the others made)

The differentiator is not a feature — it is a *posture*, and it exists precisely
because the two strongest open-source agents each made the opposite trade:

- **Pi** punts safety entirely ("no permission system — use a container").
  Optimizes **extensibility**.
- **OpenCode** went cloud platform — server, session sharing, SDK, enterprise,
  accounts. Optimizes **reach**.
- **LocalPilot** optimizes **trust under autonomy**: determinism, auditability,
  containment, recoverability, locality.

Neither competitor built — and neither can follow LocalPilot here without
abandoning its own bet (Pi would have to grow a permission engine; OpenCode would
have to give up cloud) — a single system that has **all** of:

1. a deterministic, rule-enforced workflow whose **plan and progress live in
   user-editable repo files** (`brief.md` / `PROGRESS.md` / `DECISIONS.md`);
2. **every step passes a quality gate and gets committed**;
3. **bad attempts are discarded by an anti-sunk-cost replan loop**;
4. **every side effect passes a typed permission engine, enforced even under
   bypass** (argv shell, workspace boundary, secret-path prompts);
5. **runs against local models**, with explicit **bad-output recovery** and
   **quota pause/resume** for their failure modes;
6. **learns through reviewed local memory** (LocalMind: review → promote → audit);
7. **ships as one Rust binary** and **emits zero telemetry.**

### The user the others ignore

Not "the developer who wants a powerful agent." The **developer or org that must
trust an agent working unsupervised on real code**: regulated, air-gapped,
privacy-bound, or simply careful. Local models + permission engine + audit trail
is the only combination that serves them.

### The decision filter (apply to every candidate capability)

> **Does this make the agent more trustworthy to run unsupervised?**
> If yes → build it, in **LocalPilot's own design**, and **bend it toward the
> harness.**
> If it only makes LocalPilot more like OpenCode or Pi → skip it.

We do not copy these agents; we study them and build our own version of the ideas
that serve the core. The risk is **not** any single capability. No one feature
betrays the core. The risk is the **sum**: adopt everything in this document
uncritically and the center of gravity slides from "auditable harness" to "another
agent platform, in Rust," and the differentiator drowns. **Parity is not the
goal.** The same idea can either serve the core or dilute it depending entirely on
whether it is rebuilt to fit the harness or merely bolted beside it.

### Identity-fit classification (the durable filter)

Every external capability surveyed here is tagged **SERVES**, **NEUTRAL**, or
**DANGER** against the contract above. This is the lens; §5/§8/§9 carry the detail.

**SERVES — build these; they make the harness stronger:**

| Capability | Serves only if… (the bend) |
| --- | --- |
| Event log + **session tree** (W1) | branches model harness **replan attempts** → the anti-sunk-cost loop becomes replayable and auditable. Identity, not platform. |
| **Provider/model catalog** (W3) | it gates a **harness-safe / local-capable** flag. A list of models is neutral; a list that enforces "this model is safe to run unsupervised" is core. |
| **Hook fabric** (W1/W5) | the **permission engine is the first hook**, and recovery/quota/rules run as hooks. Then extensibility *is* the safety model. A generic plugin marketplace is dilution. |
| **RPC / ACP** (W2) | it exposes **harness step + permission state** for inspection/automation. "Drive a chat agent headless" is merely neutral. |
| **Background / concurrent subagents** (W5) | each subagent runs **on its own session-tree branch (W1)** with **permissions inherited-and-narrowed from the parent**, **write isolation via file-scope locks** so no two agents touch the same files, and **every action in the event log**. Concurrency that is fully attributable, permissioned, and replayable *is* auditable — see §5.13. |
| **Session lifecycle: list / resume / continue / reload** (W1) | sessions are durable, named, branchable local artifacts — list, resume, continue-last, fork, reload. Audit trail you can pick back up. See §5.12. |
| Thinking levels, iterative compaction, bounded output, agentskills | cheap, low identity cost; good when bent to per-step harness control. |

**NEUTRAL — fine, low risk, low identity payoff; add when convenient:**
`apply_patch`, `webfetch`, LSP, dynamic local-model discovery, supply-chain
hardening. Table stakes, not differentiators.

**DANGER — defer or refuse; these are where LocalPilot stops being itself:**

| Capability | Verdict | Reason |
| --- | --- | --- |
| **Web / desktop / SDK product surface, cloud share** | **[DEFERRED] — see §1a** | highest dilution, lowest identity value: the OpenCode trap. |
| Pi's **no-permission / container-only** model | **refuse** | direct contradiction of the contract. (A container *behind* the permission engine, as defense-in-depth, is allowed.) |
| **Auto-merge of subagent writes without isolation** | **refuse** | concurrent writes to overlapping files *do* break auditability. The SERVES path (above) requires branch isolation + file-scope locks; only that disciplined form is built. |

**Bottom line for the rest of this document:** implement the **SERVES** tier and
bend each item toward the harness; take **NEUTRAL** items when cheap; **defer or
refuse the DANGER** tier. That discipline is what keeps the unique thing visible.

---

## 1. Executive summary

OpenCode and Pi are two independently designed, mature, open-source AI coding
agents. They disagree on a lot — which makes the places where they **agree** the
strongest signal for LocalPilot's next phase.

The headline:

- **OpenCode is a platform.** Server, SDK, web/desktop/VS Code clients, broad
  provider catalog, subagents/task tool, remote MCP + OAuth, LSP tool, session
  sharing. Its bet: one runtime, many surfaces.
- **Pi is a kernel + extension fabric.** Small typed core (`pi-ai`,
  `pi-agent-core`, `pi-tui`, `pi-coding-agent`), a deep event-driven **extension
  API** ("ask pi to build an extension for you"), RPC/embeddable session, session
  **tree with branching/fork/clone**, a **1,936-entry generated model catalog**,
  and explicit **supply-chain hardening**. Its bet: a tiny auditable core that
  users and the agent itself extend.
- **LocalPilot is a harness.** Rule-enforced idea→brief→plan→step workflow,
  explicit permission engine, bad-output recovery, quota pause/resume,
  LocalMind-backed reviewed memory, Rust single-binary, clean-room provenance.
  Its bet: an auditable, local-first, recoverable engineering harness.

Pi changes the next-phase picture in four concrete ways the OpenCode-only
analysis did not:

1. **The cheapest high-leverage win is an event-driven extension/hook fabric, not
   a plugin DLL system.** Pi proves a coding agent can be made deeply extensible
   purely through a typed lifecycle-event bus plus tool/command registration —
   no second process, no broad plugin authority. This maps cleanly onto
   LocalPilot's permission model and is lighter than OpenCode's PluginV2.
2. **You do not need an HTTP server to unlock external clients.** Pi's RPC mode
   (JSONL over stdin/stdout) plus an embeddable in-process `AgentSession` gives
   IDEs/automation a driveable runtime with a fraction of OpenCode's surface.
   Recommend RPC-over-stdio *before* the axum server in W2.
3. **Sessions should be a branching tree, not a linear log.** Pi's fork/clone +
   branch summaries + tree navigation is a better target than a flat event log
   alone. The event-sourcing subject (01) should bake in `parentId` from day one.
4. **The provider/model catalog can and should be generated and large.** Pi
   ships ~1,936 model entries as generated code with capabilities, costs, context
   windows, and reasoning flags. W3 should plan for *generated* catalog
   data, organized by **API protocol shape** (which LocalPilot already does), not
   hand-maintained vendor lists.

The single most important non-adoption: **Pi has no permission system at all** —
it explicitly punts isolation to containers/sandboxes. That is the exact opposite
of LocalPilot's identity. Treat Pi as a source of *runtime and extensibility*
ideas, and keep LocalPilot's permission engine as the thing Pi lacks.

---

## 1a. EXPLICITLY DEFERRED — not in scope for any planned phase

The following are **explicitly out of scope** for the foreseeable roadmap. They
are recorded here only so future readers know the omission is **deliberate**, not
an oversight. Revisit **only** if LocalPilot becomes very successful, and even
then only years out, with a fresh identity review first:

- **Web client / browser UI**
- **Desktop application**
- **Generated SDK as a product surface**
- **Cloud / off-machine session sharing**
- **Hosted/enterprise services, accounts, remote control plane**

Why deferred (not "later — soon", but **deferred hard**):

1. **Highest dilution, lowest identity value.** This is the OpenCode trap:
   building these turns a focused, auditable local harness into "another agent
   platform," and the differentiator drowns. The cost is not just the code — it is
   the permanent shift of the project's center of gravity and maintenance
   attention.
2. **Each one pulls against the core posture.** Cloud sharing and hosted services
   pull against local-first and no-telemetry. A web/desktop/SDK product surface
   pulls attention from the harness, permission engine, recovery, and LocalMind —
   the things nobody else has.
3. **They are not table stakes.** Pi ships none of them and is a credible agent;
   OpenCode shipping them is *its* bet, not a universal requirement.

What is **NOT** deferred and still serves the identity (do not confuse these with
the above):

- **RPC-over-stdio** and an **ACP adapter** (§5.2) — headless drive for IDEs and
  automation over the *same* local runtime. These keep LocalPilot a local tool;
  they do not make it a platform. They are the sanctioned integration path
  *instead of* a web/desktop/SDK product.
- **Local, redacted HTML replay/share bundle** (§5.11) — a file on your disk, not
  a cloud service. Permitted; cloud sharing is not.

Treatment in this document: wherever a deferred item appears below (e.g. in the
"OpenCode still leads" recap or the feature matrix), it is tagged **[DEFERRED]**
and points back here. The cross-agent matrix and the §9 workstreams do **not** add
roadmap work for these.

---

## 2. Method

Source-level reading of both agents, no live model runs. What was examined:

- **OpenCode** — its package layout (server, SDK, clients, providers), session
  runtime, tool set, permission model, provider/model catalog, MCP and LSP
  surfaces, and its **ACP (Agent Client Protocol)** and **models.dev** integration.
- **Pi** — the `ai` / `agent-core` / `tui` / `coding-agent` packages and their
  docs: the extension event-bus, RPC mode, embeddable session, session
  tree/compaction, tools, model catalog generation, trust and output handling.
- **LocalPilot** — its current crates, to baseline what already exists versus what
  the workstreams below propose (summarized next).

Two findings are worth stating up front because they recur below:

1. **ACP (Agent Client Protocol)** — OpenCode implements this public editor↔agent
   standard (agent / session / permission / tool / usage). It is the
   standards-based path for the headless-drive recommendation (§5.2).
2. **models.dev** — OpenCode reads its model catalog from this public dataset, and
   Pi independently **generates** its catalog from the **same** source. Two
   independently designed agents converging on one public catalog is the strongest
   single signal in this document (§5.5, W3).

LocalPilot baseline (current state):

- **Tools** (`builtins.rs`): `read_file`, `write_file`, `edit_file`,
  `multi_edit`, `list_files`, `find_files`, `search_text`, `run_shell` (argv,
  not shell-string), `update_plan`, and git (`git_status/diff/log/add/restore/
  commit`). No `apply_patch`, `webfetch`, `websearch`, `lsp`, or `task`.
- **Store** (`store/src/lib.rs`): transcript append (`append_message`), session
  index, cache, **tool-output retention already exists** (`put_tool_output`/
  `get_tool_output`), provider-metadata cache, redacted export. **No
  event/`SessionEvent` log yet** — still final-message persistence.
- **Providers**: two kinds — `anthropic` and `openai-compatible` (clean,
  protocol-shaped, but a small static catalog).
- **Server / RPC / SDK / subagents / LSP / plugins**: none yet.

So the next-phase workstreams (§9) are all still unstarted; this research defines
their content rather than reporting on shipped work.

---

## 3. The three agents at a glance

| Dimension | OpenCode | Pi | LocalPilot |
| --- | --- | --- | --- |
| Stack | TS / Bun / Effect / Solid / OpenTUI | TS / Node ≥22 / typebox / custom TUI | Rust / Tokio / ratatui / clap |
| Core size | Very large platform (~5.6k files) | Small kernel + 4 packages | Small (~238 files), 14 crates |
| Identity bet | One runtime, many surfaces | Tiny core + extension fabric | Auditable local harness |
| Extensibility | PluginV2 hooks + plugin packages | **Event-bus extensions** + tools/commands/UI | Traits; no general plugin system yet |
| External drive | HTTP server + generated SDK | **RPC (JSONL/stdio) + embeddable `AgentSession`** | CLI/TUI only |
| Sessions | Evented DB + projectors + context epochs | **JSONL tree** (id/parentId), fork/clone, branch summaries | Transcript append + in-memory vec |
| Provider catalog | Broad plugin catalog | **~1,936 generated model entries** | 2 protocol kinds, static |
| Reasoning control | Per-model/agent options | **Thinking levels** (off→xhigh) | None first-class |
| Subagents | **Yes** (`task`, fg/bg) | **No** (extend via extensions instead) | No (modes only) |
| Permissions | Rich permission system | **None** (punts to container/sandbox) | **Explicit typed permission engine** |
| Memory/learning | Skills + instructions | agentskills.io skills + prompt templates | **LocalMind reviewed memory** |
| Recovery | Runtime failure handling | Output guard + abort plumbing | **Bad-output recovery ladder** |
| Quota | Retry/overflow | Retry/headers (`after_provider_response`) | **Quota pause/resume** |
| Supply chain | Standard | **Hardened** (pinned, min-release-age, shrinkwrap, `--ignore-scripts`) | Pinned `=` deps, `cargo deny/audit` |
| Skills standard | Custom | **agentskills.io** + cross-harness dirs | Custom skill drafts |

---

## 4. Triangulation: where two independent agents converge

When OpenCode and Pi — designed by different teams, in different styles —
independently arrive at the same mechanism, that is the strongest possible signal
that LocalPilot will eventually need it too. These are **high-confidence**
adoptions (subject to LocalPilot's safety/clean-room framing):

| Converged design | OpenCode | Pi | Confidence for LocalPilot |
| --- | --- | --- | --- |
| Durable, replayable session persistence as a structured log | Evented DB + projectors | JSONL entry log, versioned + migrated | **Very high** — W1 |
| A headless way to drive the runtime from other programs | HTTP server + SDK | RPC/stdio + embeddable session | **Very high** — W2 (start with RPC) |
| Rich model metadata (capabilities, cost, context, reasoning) | Provider/model catalog **from models.dev** | `models.generated.ts` **from models.dev** | **Very high** — W3; same public source |
| Providers organized by **API protocol shape**, not vendor | openai-compatible etc. | openai-completions / -responses / anthropic-messages / google / bedrock | **Very high** — LocalPilot already does this; keep it |
| Compaction with a structured, iterative summary | Compaction + rebuild | `firstKeptEntryId` + previous-summary feedback | **High** — LocalPilot has basic compaction; deepen |
| On-demand skills via progressive disclosure | Skills | agentskills.io skills | **High** — align LocalPilot skills to the standard |
| Extension/plugin layer for tools, context, UI | PluginV2 | Event-bus extensions | **High** — W5; prefer Pi's event model |
| First-class "context files" / instruction loading | Context sources | `AGENTS.md` chain + `systemPromptOptions` | **High** — W1 context sources |
| Session export/share artifacts | Share service | `pi-share-hf`, export-html | **Medium** — keep LocalPilot's redacted-local-first stance |
| Trust gate before running project-local code | Project trust | `trust-manager` + trust before `.pi/extensions` | **High** — LocalPilot already added a trust gate; extend it to plugins/skills |

Two notable **non-convergences** (the agents disagree), which tell LocalPilot
where to make a deliberate choice rather than follow the crowd:

- **Subagents:** OpenCode yes, Pi no. → Not a settled best practice industry-wide,
  which means LocalPilot designs its own form rather than copying either. It will
  build **background/concurrent** subagents on the auditable-concurrency design in
  §5.13 (branch-isolated, permission-narrowed, file-scope-locked), not the
  read-only-only minimum.
- **Built-in permissions:** OpenCode yes, Pi no (container instead). → LocalPilot's
  permission engine is a real differentiator; do **not** weaken it toward Pi's
  model. Optionally *add* container/sandbox as an extra layer (see §6.10).

---

## 5. What Pi does distinctively — and how LocalPilot adapts it

This is the net-new material. Each item ends with a concrete, clean-room,
LocalPilot-native recommendation.

### 5.1 The extension event-bus (Pi's signature idea)

Pi's core differentiator is that almost everything is an **extension**: a
TypeScript module exporting a factory that receives a `pi` API and subscribes to
a typed lifecycle event bus. The lifecycle is fully specified — `session_start`,
`resources_discover`, `input`, `before_agent_start`, `agent_start`,
`turn_start`/`turn_end`, `context`, `before_provider_request`,
`after_provider_response`, `tool_execution_start`, `tool_call` (**can block**),
`tool_execution_update`, `tool_result` (**can modify**), `message_*`,
`model_select`, `thinking_level_select`, `session_before_compact`/`session_compact`,
`session_before_fork`/`session_before_tree`, `user_bash`, `session_shutdown`.

Extensions can:
- register LLM-callable tools (`pi.registerTool`),
- register commands (`/cmd`), shortcuts, and CLI flags,
- register providers/models at startup (sync or async factory),
- intercept/transform/handle user input before expansion,
- block or mutate tool calls and tool results (middleware chaining in load order),
- render custom TUI, set footer status/widgets, prompt the user (`ctx.ui`),
- persist their own session entries (`pi.appendEntry`), and
- be **hot-reloaded** with `/reload`.

The example set is striking and shows the ceiling: permission gates
(`confirm-destructive`, `dirty-repo-guard`), `git-checkpoint`, `handoff`,
`dynamic-tools`, `dynamic-resources`, `custom-compaction`, custom providers
(anthropic, gitlab-duo), `gondolin` (route tools into a micro-VM), down to a
`doom-overlay`. The docs literally say "pi can create extensions. Ask it to build
one."

Why this matters for LocalPilot: W5 (extensibility) is naturally framed as a
**plugin manifest** system (capability-scoped, manifest under
`.localpilot/plugins/<name>/`). Pi shows that the **event/hook fabric is the more
valuable half** and can ship first, independently, and entirely inside the trust
+ permission model LocalPilot already has. Critically, many things otherwise
treated as separate features become *extensions over the same hook surface*:
git-checkpoint/snapshot, destructive-command confirmation, dirty-repo guard,
custom compaction, input transforms.

**Recommendation (refines W1 + W5):**
- Define a typed `HarnessHook`/event surface in Rust *as part of W1*, not
  as a late plugin add-on. Even before third-party plugins exist, route
  LocalPilot's own cross-cutting behavior (trust gate, recovery, quota,
  LocalMind injection, quality gate) through it. This is the Rust analogue of
  Pi's event bus and makes those behaviors composable and testable.
- Phase the hooks: notify-only first (`turn_start`, `tool_execution_*`,
  `model_select`), then mutating hooks behind the permission engine
  (`tool_call` block/mutate, `context` rewrite, `before_provider_request`).
- A `tool_call` hook that can return `Block{reason}` is exactly LocalPilot's
  permission verdict shape — unify them: the permission engine becomes the
  first, always-on hook.
- Keep third-party hook code out-of-process or trusted-only (LocalPilot can't run
  arbitrary TS in-process the way jiti does; native/WASI/external-command hooks
  are the clean-room-safe path). The *event contract* is the reusable lesson, not
  jiti.

### 5.2 RPC over stdio + embeddable session (do this before the HTTP server)

Pi exposes the runtime two ways without a web server:
- **`pi --mode rpc`**: strict JSONL over stdin/stdout. Commands (`prompt`,
  `steer`, `follow_up`, …) in, agent events streamed out, optional `id` for
  request/response correlation, LF-only framing (explicitly warns Node
  `readline` is non-compliant because it splits on U+2028/U+2029).
- **Embeddable `AgentSession`**: Node apps import the class directly instead of
  spawning a subprocess.

This is dramatically less surface than OpenCode's axum-style HTTP server + OpenAPI
+ generated SDK, yet it already unlocks IDE integration, automation, and testing.

**Recommendation (refines W2):** Re-order W2 to ship
**RPC-over-stdio first**, HTTP server second. Concretely:
- `localpilot rpc` (or `localpilot serve --stdio`) speaking newline-delimited
  JSON: input commands, streamed `SessionEvent`s (reuse W1's event enum
  directly — the event log *is* the wire format).
- Keep the in-process path as the library API (`localpilot-harness`
  `SessionRuntime` already is this); document it as the embedding surface.
- Adopt Pi's framing discipline as a hard *requirement*: LF-only records,
  tolerate trailing `\r`, never use a line reader that splits on Unicode
  separators. This is a real interop footgun.
- The HTTP server (axum) then becomes a thin transport over the same command/event
  types, added only when web/desktop needs it.

**Name the standard target: ACP (Agent Client Protocol).** OpenCode ships a full
ACP implementation (`packages/opencode/src/acp/`: agent, session, permission,
tool, usage) — the public editor↔agent protocol (Zed-originated, now multi-editor).
Where Pi rolled its own RPC, OpenCode targets the emerging *standard*. For
LocalPilot the sequencing is: (1) RPC/stdio over the event types for the cheap
in-house win, then (2) an **ACP adapter** over that same runtime so any
ACP-capable editor drives LocalPilot without a bespoke extension. ACP is a public
spec — implementing against it is clean-room-safe, and its **permission request**
message maps directly onto LocalPilot's permission verdict (the editor renders the
prompt; LocalPilot owns the decision). Put the ACP adapter in W2's scope
as the IDE-integration path, ahead of any custom VS Code extension.

### 5.3 Sessions as a branching tree (not a flat log)

Pi sessions are JSONL where each entry has `id`/`parentId`, forming a **tree**.
This enables:
- `/fork` (branch before an entry) and `/clone` (branch at an entry),
- `/tree` navigation between branches with **branch summaries** generated to
  preserve context across a switch,
- in-place branching without new files,
- versioned format (v1 linear → v2 tree → v3 renamed roles) with **automatic
  migration on load**.

LocalPilot's planned event log (W1) is currently framed as a linear
append. A linear log makes "undo this attempt and try another approach" — which
is *exactly* the harness's anti-sunk-cost replan loop — awkward.

**Recommendation (refines W1):**
- Put `parent_id` on `SessionEvent`/entries from the start, even if the first
  release only ever appends linearly. Retrofitting a tree later is expensive.
- Map the harness's replan/discard loop onto fork: a discarded step attempt
  becomes a dead branch; a replan forks from the last good step. This makes the
  anti-sunk-cost loop *inspectable and replayable*, strengthening LocalPilot's
  signature feature.
- Adopt a **session format version + migration-on-load** contract now, while the
  format is young and cheap to migrate.
- Generate **branch summaries** when abandoning a branch so context isn't lost —
  this is a natural LocalMind tie-in (the abandoned branch's lesson becomes a
  closeout candidate).

### 5.4 Iterative, structured compaction

Pi's compaction: walk back from newest until `keepRecentTokens` (~20k) is held;
summarize everything older with a **structured** format; **feed the previous
summary back in** as iterative context; store a `CompactionEntry` with
`firstKeptEntryId`; reload from there. Auto-trigger is
`contextTokens > contextWindow - reserveTokens` (default reserve 16384), all
configurable. The *same* structured-summary machinery powers branch
summarization.

LocalPilot already has compaction (`harness/src/compaction.rs`, configurable
`context_token_limit`, default 24000). The deltas worth adopting:

**Recommendation (refines W1/W4):**
- Make the trigger window-relative (`context_window - reserve`) rather than a
  flat token cap, using real per-model context windows from the new catalog (§5.5).
- Pass the **previous summary** into the next compaction so summaries accumulate
  rather than restart — better long-session fidelity.
- Use **one structured summary format** shared by compaction and any future
  branch/replan summary, so the harness, LocalMind, and replay all read the same
  shape.

### 5.5 A large, generated provider/model catalog organized by API shape

Pi's `ai/src/models.generated.ts` is **16,939 lines / ~1,936 entries** — model
id, provider, reasoning flag, input modalities, **cost** (input/output/cacheRead/
cacheWrite), **contextWindow**, **maxTokens**. Providers are organized by **API
protocol shape**: `openai-completions`, `openai-responses`, `anthropic-messages`,
`google-generative-ai`, `google-vertex`, `bedrock-converse-stream`,
`mistral-conversations`, `azure-openai-responses`, `openai-codex-responses`. Model
discovery can also be **dynamic** (an async extension fetches `/v1/models` from a
local server and registers them at startup, so they show up in `--list-models`).

This is the same conclusion OpenCode reached (rich catalog) but Pi shows the
*implementation strategy*: **generate it**, keep it data, and key behavior off
**protocol shape**, which LocalPilot already does (`kind = "openai-compatible" |
"anthropic"`).

Crucially, **both agents draw from the same public source: [models.dev]**.
OpenCode reads it via `core/src/models-dev.ts` + `catalog.ts`; Pi generates
`models.generated.ts` from it via `scripts/generate-models.ts`. Two independently
designed agents converging on one public catalog is the strongest signal in this
whole document — and it gives W3 a concrete, clean-room-safe input
(models.dev is a public, openly licensed dataset) instead of a vague "transcribe
the pricing pages."

**Recommendation (refines W3):**
- Ship the catalog as **generated data** (a build step / `xtask` producing a Rust
  table or a checked-in TOML/JSON the binary embeds), not a hand-maintained list.
  Generate from **models.dev** (the public dataset both OpenCode and Pi use);
  vendor a pinned snapshot into the repo and regenerate via `xtask`, so the binary
  stays offline and reproducible. Verify license/attribution before vendoring
  (clean-room).
- Catalog fields to match the converged set: capabilities, context window, max
  output, reasoning support, **cost**, modality, and a **harness-safe** flag
  (tool-capable + deterministic-enough for harness use).
- Keep `kind` = API protocol shape as the dispatch key; vendors are just data.
- Support **dynamic discovery** for local servers (LocalBox/Ollama/llama.cpp/
  vLLM): query the OpenAI-compatible `/v1/models` and merge into the catalog at
  runtime so `localpilot models` lists what's actually loaded. This is a perfect
  fit for the LocalX ecosystem and a clear differentiator for local-first users.
- Surface cost/context/capability in `doctor`, the TUI footer, and LocalBench.

### 5.6 Thinking levels (reasoning effort as a first-class control)

Pi models a `ThinkingLevel` of `off | minimal | low | medium | high | xhigh`,
with its own `thinking_level_select` event, keybinding/`Ctrl+P` cycling, and
clamping when a model can't honor a level. It's a top-line UX control, not a
buried request option.

LocalPilot has no first-class reasoning-effort concept.

**Recommendation (new — fold into W3):**
- Add a `ReasoningEffort`/thinking-level to the request model and provider
  contract, mapped per provider (Anthropic thinking budget, OpenAI reasoning
  effort, local model no-op/clamp).
- Make it visible and switchable in the REPL, and **catalog-aware** (clamp to
  what the model supports). For the harness, allow per-step effort (e.g. high for
  planning, low for mechanical edits) — a genuine harness-quality lever.

### 5.7 Steering and follow-up message queues

Pi distinguishes three input dispositions while the agent is running:
- **steer**: delivered after the current assistant turn's tool calls finish,
  before the next LLM call (mid-stream course-correction),
- **follow_up**: delivered only once the agent goes idle,
- immediate **extension commands** (run even mid-stream).

This is exposed in both interactive and RPC modes and is a notably good
human-in-the-loop UX.

**Recommendation (new — fold into W1/W2):** Model input admission as
`{steer, follow_up, immediate}` at safe provider-turn boundaries (which W1's epoch model already needs). It pairs naturally with the harness: a "steer"
during a step is a mid-step correction; a "follow_up" is a queued next step.

### 5.8 Prompt templates + agentskills.io standard

Pi separates **skills** (agentskills.io standard: `SKILL.md` with frontmatter,
progressive disclosure, `/skill:name` commands, cross-harness directories — it can
load `~/.claude/skills` and `~/.codex/skills`) from **prompt templates**
(parameterized reusable prompts, `/template`). Skills, templates, and themes are
all contributed via the `resources_discover` event.

LocalPilot has skill *drafts* (alpha) via LocalMind, but not standard-aligned
skill loading or prompt templates.

**Recommendation (refines W4/W5 + LocalMind):**
- Align LocalPilot's skill loading to the **agentskills.io** standard so the
  ecosystem's skills (and Claude/Codex skill dirs) work out of the box — a cheap
  interop win and clean-room-safe (it's a public spec).
- Add **prompt templates** as a distinct, simple feature (parameterized prompts,
  user/project scoped, trust-gated for project ones).
- Keep LocalMind as the *authoring/review* path that promotes a reviewed lesson
  into a standard skill file.

### 5.9 Supply-chain hardening (a posture worth adopting)

Pi treats dependency changes as reviewed code: exact-pinned direct deps,
`.npmrc save-exact=true` + **`min-release-age=2`** (no same-day deps),
`package-lock.json` as ground truth with a pre-commit guard, a generated
**shrinkwrap** for published transitive pins, an **allowlist for dependency
lifecycle scripts**, `--ignore-scripts` on installs, `npm audit` (incl.
signatures) on a schedule, and release smoke-tests in isolated installs.

LocalPilot already pins `=` versions and runs `cargo deny`/`cargo audit`. The
*additional posture* worth adopting:

**Recommendation (refines W6 / dev-tooling):**
- Add a **min-release-age** equivalent for new crates (don't adopt a crate version
  published in the last N days) as a `cargo deny` advisory/policy or CI check.
- Treat `Cargo.lock` as ground truth with a CI guard on unexpected changes
  (mostly already true for a bin).
- Document the lifecycle-script analogue: be explicit about `build.rs` in deps
  (LocalPilot already has its own `build.rs`); audit new build-script deps.
- Keep the existing `cargo machete`/`deny`/`audit` gate; add release smoke-tests
  that install the produced binary in a clean environment (ties into W6
  packaging).

### 5.10 Containerization as an *additional* isolation layer (not a replacement)

Pi has **no permission system** and documents three isolation patterns instead:
OpenShell (whole process under policy sandbox), the **Gondolin** extension (keep
pi + auth on host, route built-in tools and `!` commands into a local Linux
micro-VM), and plain Docker.

LocalPilot must **not** adopt Pi's "no permissions, use a container" stance — the
permission engine is core identity. But the *Gondolin pattern* is interesting as
an **optional extra layer**: route `run_shell`/write effects through a
sandbox/VM backend while keeping the permission engine in front of it.

**Recommendation (optional, post-subject-05):** Expose an execution-backend
abstraction so `run_shell` (and writes) can optionally target a
container/sandbox/microVM, *behind* the permission engine, not instead of it.
This is "defense in depth," consistent with LocalPilot's safety-first posture,
and a good story for untrusted-repo work. Low priority; design the seam now so it
isn't precluded.

### 5.11 Smaller Pi mechanics worth noting

- **Output guard / accumulator / truncate** (`core/output-guard.ts`,
  `tools/output-accumulator.ts`, `truncate.ts`): bounded tool output with
  head/tail retention and `fullOutputPath` spill — LocalPilot already has
  `put_tool_output`; align the *truncation + reference* UX (cap, keep head+tail,
  reference full output by id).
- **`bashExecution` as a first-class message role** with `excludeFromContext`
  for `!!` commands — lets users run shell that doesn't pollute context. Cheap,
  useful REPL feature.
- **Footer data provider / status / widgets**: structured footer (model, context
  usage, cost) and extension-set widgets above the editor. Good TUI target.
- **`ctx.getContextUsage()`** surfaced to extensions and UI — context budget as a
  visible number. LocalPilot has the budget; surface it.
- **Settings manager + keybindings + themes** as discoverable, layered config
  (global/project) with hot-reload. LocalPilot config is figment-based; a
  keybindings/themes layer is a later UX nicety.
- **`/reload`** for extensions and resources — hot reload without restart.
- **Native clipboard image paste + EXIF orientation + photon image handling** —
  multimodal input plumbing if/when LocalPilot supports image inputs.
- **HTML export** (`export-html`) as a shareable, local artifact — supports a
  "local share bundle before any cloud sharing" stance.

### 5.12 Session lifecycle: list, resume, continue, fork, reload

Both reference agents (and the agents LocalPilot's users already know — Claude
Code with `--continue` / `--resume`, Codex with session resume) treat a session as
a **durable, named, resumable artifact**, not an ephemeral process. Pi makes this
explicit: sessions persist as files keyed by working directory; `/resume` lists
and reopens them (with delete), `/new` starts fresh, `/fork` and `/clone` branch,
`/tree` navigates branches, and `/reload` hot-reloads the session and extensions
without restarting. Session events carry a `reason` (`startup` / `new` / `resume`
/ `fork`) so the runtime can react to *how* it was opened.

LocalPilot persists transcripts and a session index already, but has no
user-facing resume/continue/reload UX. This is a high-value, identity-aligned
addition: durable local sessions you can pick back up *are* the audit trail made
usable.

**Recommendation (W1, surfaced in CLI + REPL):**
- CLI: `localpilot session list`, `localpilot session resume <id>`,
  `localpilot --continue` (resume the most recent session for this workspace),
  `localpilot --resume <id>`, `localpilot session export <id>`, and a session
  delete that prefers OS trash over hard delete.
- REPL slash-commands: `/resume` (interactive picker), `/new`, `/fork`, `/clone`,
  `/tree` (branch navigation), `/reload` (reload session + extensions/config).
- Key sessions by **workspace + id**, store under `.localpilot/`, and make resume
  rebuild state **from the event log (W1)** — so resume, replay, and audit are the
  same mechanism. Emit an open-reason on every session start so the harness can
  resume mid-step rather than restart a step.
- Resume must re-apply the **current permission profile and trust state**, never
  inherit stale elevated permissions from the saved session.

### 5.13 Auditable concurrent subagents (LocalPilot's own design)

OpenCode has a `task` tool that launches subagents (foreground and background,
with permissions derived from the parent); Pi deliberately has none. LocalPilot
**wants** background/concurrent subagents — planner delegating reconnaissance,
a reviewer inspecting a diff while the worker proceeds, parallel independent
edits — **and can have them without giving up auditability**, provided
concurrency is built LocalPilot's way rather than copied.

The reconciliation: "single auditable thread-of-control" was always shorthand for
the real requirement — **every action must be attributable, permissioned, and
replayable.** Concurrency does not break that; *unisolated* concurrency does. So
the design constraints are:

- **Each subagent runs on its own session-tree branch (W1).** Its messages, tool
  calls, permission decisions, and results are full event-log entries under a
  parent→child link. Nothing a subagent does is invisible.
- **Permissions are inherited and narrowed, never widened.** A subagent's effect
  set is a subset of its parent's profile (a read-only reviewer gets no write/
  network). The permission engine remains the first hook on every subagent effect.
- **Write isolation via file-scope locks.** A write-capable subagent must declare
  (or be assigned) a non-overlapping file/path scope; the runtime holds locks so
  two agents can never edit the same file concurrently. Overlap requests are
  serialized or denied, not silently merged.
- **Deterministic, reviewable merge-back.** A subagent's results return to the
  parent as structured, attributable entries; any workspace changes land as a
  reviewable unit (tie into snapshots/`apply_patch`), so the harness can accept or
  discard a subagent's work as one auditable step — consistent with the
  anti-sunk-cost loop.
- **Bounded and cancellable.** Background subagents respect the same quota,
  recovery, and cancellation plumbing; cancelling the parent cancels children, and
  the event log records partial/aborted subagent activity.

Built this way, concurrent subagents *strengthen* the harness (parallel planning/
review/execution that is still fully recorded and permissioned) instead of
turning LocalPilot into an unaccountable swarm. The thing LocalPilot refuses is
not concurrency — it is **unisolated, unattributable** concurrency (see the §0
DANGER row).

---

## 6. Where Pi is weaker — guardrails so LocalPilot doesn't regress

Adopting Pi ideas must not import Pi's gaps. LocalPilot is already ahead here:

1. **No permission engine.** Pi runs with full user permissions by default. This
   is LocalPilot's single biggest advantage. Keep typed effects, workspace
   boundary under bypass, secret-path prompts, argv shell. Any hook/extension/RPC
   path must route through it.
2. **No deterministic harness ceremony.** Pi's `AgentHarness` is a *runtime
   orchestrator* (loop/compaction/session) — confusingly named, but it is **not**
   LocalPilot's idea→brief→plan→step rule-enforced workflow. LocalPilot's harness
   remains unique across all three agents.
3. **No reviewed local memory.** Pi has skills/templates and a "share your
   sessions to HuggingFace" data-collection ask; LocalPilot's LocalMind
   review→promote→audit loop is a stronger *and more private* local-learning
   story. (Note Pi's session-sharing ask is the opposite of LocalPilot's stance —
   keep redacted-local-first.)
4. **No subagents.** Pi has none; OpenCode does. LocalPilot will build them, but
   on its own auditable-concurrency design (§5.13) — branch-isolated,
   permission-narrowed, file-scope-locked — not Pi's omission and not an
   unaccountable swarm.
5. **Node/npm runtime + large dependency surface.** LocalPilot's Rust
   single-binary is an advantage for local-first distribution; don't trade it
   away by porting Pi's jiti/in-process-TS extension execution.
6. **Telemetry present** (`core/telemetry.ts`) — LocalPilot's no-hidden-telemetry
   posture is a differentiator; keep it.

---

## 7. Where OpenCode leads both (capabilities to build later)

Capabilities OpenCode has that LocalPilot lacks today — and where Pi *also* lacks
them, which signals they are genuinely later-phase, not table stakes:

- **Subagents / `task` tool** (OpenCode yes, Pi no) — LocalPilot will build these
  as **auditable background/concurrent** subagents (§5.13, W5), not the
  read-only-only minimum.
- **Full HTTP server + ACP** (OpenCode yes, Pi uses bespoke RPC instead) —
  RPC-first per §5.2, then an **ACP adapter** as the standards-based IDE path; a
  local HTTP server only if a sanctioned local client needs it. **Generated SDK,
  web client, desktop app, and VS Code product surface are [DEFERRED] — see §1a.**
- **Remote MCP transports + OAuth + resources/prompts** (OpenCode yes, Pi minimal)
  — keep the staged MCP plan; remote opt-in and network-gated.
- **LSP tool** (OpenCode yes, Pi no) — still worth adding for Rust; read-only,
  permission-gated, `rust-analyzer` first.

The fact that Pi *omits* all four and is still a credible agent confirms the
sequencing: these are later-phase, not foundational.

---

## 8. Cross-agent feature matrix (with recommendation)

| Capability | OpenCode | Pi | LocalPilot (now) | Recommendation |
| --- | --- | --- | --- | --- |
| Durable session log | Evented DB | JSONL entry log | Transcript append, **no event log** | W1: event log **with `parent_id`** |
| Session tree / fork / branch summary | partial | **Yes** | No | W1: tree-from-day-one; map to replan loop |
| Format version + migration | Yes | **Yes** | No | Adopt version + migrate-on-load now |
| Headless drive | HTTP+SDK + **ACP** | **RPC/stdio + embeddable** | No | W2: **RPC first**, then **ACP adapter**, HTTP later |
| Context sources / epochs | **Formal** | `AGENTS.md` + systemPromptOptions | seed_system + compaction | W1: typed context sources |
| Provider/model catalog | Broad (**models.dev**) | **Generated ~1,936 (models.dev)** | 2 kinds static | W3: **generate from models.dev**, by API shape |
| Dynamic local-model discovery | some | **Yes** (`/v1/models`) | No | W3: query local servers, merge |
| Reasoning effort control | per-model opt | **Thinking levels** | None | W3: first-class effort, per harness step |
| Steering / follow-up queue | yes | **Yes (explicit)** | partial | W1/W2: typed input disposition |
| Iterative structured compaction | yes | **Yes** | Basic | W1/W4: window-relative + iterative |
| Extension/hook fabric | PluginV2 | **Event bus** | None | W1/W5: typed hooks; perms = first hook |
| Manifest plugins | packages | npm/git packages | None | W5: capability-scoped, trust-gated |
| Tools: apply_patch | **Yes** | edit/edit-diff | No | W4: `apply_patch` |
| Tools: webfetch/websearch | **Yes** | via extensions | No | W4: network-gated, off in harness |
| Tools: LSP | **Yes** | No | No | W4: read-only, rust-analyzer first |
| Tool output bounding + spill | yes | **Yes** | Retention exists | W4: cap + head/tail + id reference |
| Subagents/task | **Yes** | No | No | W5: **auditable background/concurrent** (§5.13) |
| Skills standard | custom | **agentskills.io** | drafts (LocalMind) | W4/W5: adopt the standard |
| Prompt templates | partial | **Yes** | No | New: parameterized templates |
| Permission engine | yes | **No** | **Yes (strong)** | Keep; make it the always-on hook |
| Reviewed local memory | no | no | **LocalMind** | Keep; promote lessons → standard skills |
| Bad-output recovery | runtime | output guard | **Recovery ladder** | Keep; route through hooks |
| Quota pause/resume | retry | headers | **Yes** | Keep; emit as events |
| Container/sandbox isolation | — | **Gondolin/Docker** | — | Optional layer *behind* permissions |
| Supply-chain hardening | standard | **Hardened** | pinned+deny+audit | W6: add min-release-age, smoke installs |
| Telemetry | present | present | **none** | Keep none |
| Session list/resume/continue/reload | partial | **Yes** (`/resume`,`/new`,`/fork`,`/reload`) | index only | W1: full lifecycle UX (§5.12) |
| HTML/local share bundle | share svc | **export-html** | redacted export | Local bundle only; **cloud share [DEFERRED] §1a** |
| Web / desktop / SDK product surface | Yes | No | No | **[DEFERRED] §1a** — not on the roadmap |
| Cloud / hosted / enterprise services | Yes | session-share ask | No | **[DEFERRED] §1a** — contradicts local-first |

---

## 9. Recommended next-phase workstreams (authoritative)

This is the self-contained, authoritative work list for LocalPilot's next phase.
**W0 (tooling research)** is the readiness step; the rest are independently
shippable. Every "W1…W6" reference elsewhere in this document resolves here.

**Every item below has already been filtered through §0.** Build order favors
**SERVES** items first (W1 hook fabric + session tree, W3 catalog), then NEUTRAL
table stakes, with DANGER items deferred/constrained as marked.

**W1 — Durable runtime events & context epochs** *(SERVES — highest priority):*
- Add `parent_id` to the event/entry model; design the log as a **tree**, not a
  linear append. Map the harness replan/discard loop to fork/branch.
- Adopt a **session format version + migrate-on-load** contract from the first
  release.
- Add a **typed input-disposition** (`steer` / `follow_up` / `immediate`) resolved
  at safe provider-turn boundaries.
- Define the **internal hook/event surface** here (notify-only first) and route
  LocalPilot's own cross-cutting behaviors (permission verdict, recovery, quota,
  LocalMind injection, quality gate) through it. Permissions = the first,
  always-on hook with a `Block{reason}` verdict.
- Make compaction **window-relative** (`context_window - reserve`) and
  **iterative** (feed previous summary forward); share one structured summary
  format with branch summaries.
- Ship the **session lifecycle UX** (§5.12) on top of the event log: CLI
  `session list/resume/export`, `--continue` / `--resume <id>`, and REPL
  `/resume` `/new` `/fork` `/clone` `/tree` `/reload`. Resume rebuilds state from
  the event log and re-applies current trust/permission profile (never inherits
  stale elevated permissions).

**W2 — Local headless drive: RPC + ACP** *(SERVES if it exposes harness/permission state):*
- Ship **RPC-over-stdio (JSONL) first**; HTTP/axum second over the same
  command/event types. Document the embeddable in-process `SessionRuntime` as the
  library surface.
- Bake in the **LF-only framing** rule and request/response `id` correlation.
- Add an **ACP (Agent Client Protocol) adapter** over the same runtime as the
  standards-based IDE-integration path (OpenCode implements ACP; it's a public
  spec). ACP's permission-request message maps onto LocalPilot's permission
  verdict. Prefer this over a bespoke VS Code extension.

**W3 — Provider/model catalog** *(SERVES if it gates a harness-safe/local-capable flag):*
- Make the catalog **generated data** from **models.dev** (the public dataset
  both OpenCode and Pi use), vendored as a pinned snapshot regenerated via xtask;
  key it by **API protocol shape**; include cost, context window, max output,
  reasoning support, modality, and a **harness-safe** flag.
- Add **dynamic discovery** for local OpenAI-compatible servers (LocalBox/Ollama/
  vLLM/llama.cpp) merged into `localpilot models`.
- Add **reasoning-effort / thinking levels** to the request + provider contract,
  catalog-aware clamping, per-harness-step override.

**W4 — Tool / LSP / MCP upgrades** *(mostly NEUTRAL table stakes; agentskills = SERVES):*
- Add a first-class **`bashExecution`/`!`-style** transcript role with an
  `exclude_from_context` option.
- Align tool-output bounding to Pi's **cap + head/tail + spill-to-id** UX (build
  on existing `put_tool_output`).
- Adopt **agentskills.io** skill loading (incl. cross-harness skill dirs).

**W5 — Agents, subagents, scoped plugins** *(hook fabric = SERVES; subagents = SERVES via §5.13 design):*
- Reframe the plugin system around the **event/hook fabric** (from W1) as
  the primary extension mechanism; manifest plugins are the *packaging* of hooks +
  tools + providers + skills, capability-scoped and trust-gated. Keep third-party
  code out-of-process or trusted-only (no in-process arbitrary code).
- Add **prompt templates** as a distinct lightweight feature.
- Build **background/concurrent subagents** on the auditable-concurrency design
  (§5.13): each subagent on its own **session-tree branch (W1)**, permissions
  **inherited-and-narrowed** from the parent, **file-scope write locks** to
  prevent overlapping edits, every action in the event log, and
  **deterministic reviewable merge-back** (tie into snapshots/`apply_patch`).
  Cancellation and quota/recovery cascade parent→child. The only refused form is
  **unisolated/unattributable** concurrency (§0 DANGER).

**W6 — Release readiness & local surfaces** *(NEUTRAL; product surfaces DEFERRED):*
- Add **supply-chain posture**: min-release-age for new crates, lockfile guard,
  build-script dep audit, clean-environment install smoke tests.
- Add **local, redacted HTML replay/share bundle** (a file on disk). **No cloud
  sharing — [DEFERRED] §1a.**
- TUI: footer data provider (model/context/cost), `ctx.getContextUsage()`-style
  visible budget, extension-set status/widgets, `/reload`.
- **Scope guard:** the IDE/automation path is **RPC/ACP** (W2). Web client,
  desktop app, and SDK-as-product are **[DEFERRED] §1a** and must not reappear as
  roadmap work.

**W0 — Tooling research & readiness:** dependency/feasibility spikes for the above
(axum vs poem for W2, rust-analyzer embedding for W4, models.dev licensing for W3,
WASI/external-command hook isolation for W5). This document is its primary input.

---

## 10. Clean-room cautions (specific to Pi and OpenCode)

- Both reference agents are inspectable here; that makes them a **behavior
  reference**, governed by `docs/00-clean-room.md`. Do **not** copy code, prompts,
  identifiers, UI copy, event names, JSON shapes, or package structure. Re-derive
  from the public *concept* (e.g., "lifecycle hook bus," "JSONL session tree,"
  "generated model catalog") and name things in LocalPilot's own vocabulary.
- The **agentskills.io** spec and the OpenAI-compatible `/v1/models` endpoint are
  public standards — implementing against them is fine; copying Pi's parser is
  not.
- Pi's `models.generated.ts` is generated from upstream sources; build
  LocalPilot's catalog from the **original public** provider docs/pricing, not by
  transcribing Pi's table.
- Pi's "share your sessions" / HuggingFace dataset pitch and OpenCode's session
  sharing both push data off-machine. LocalPilot's stance (redacted, local-first,
  reviewed) is a differentiator — do not weaken it; any sharing stays opt-in and
  visibly labeled.
- Pi's no-permission-by-default model is a non-goal. The permission engine is
  identity.

---

## 11. Bottom line

OpenCode and Pi attack the same problem from opposite ends — platform vs. kernel —
and where they **agree** is where LocalPilot's next phase has the least risk:
durable structured sessions, a headless drive surface, a rich generated model
catalog, an extension/hook layer, and standard skills.

Pi's specific, transferable lessons on top of the existing OpenCode-derived plan:

1. **Build the hook/event fabric early and make permissions its first hook** —
   it's the cheapest path to extensibility and unifies LocalPilot's own
   cross-cutting behaviors.
2. **RPC-over-stdio before an HTTP server** — most of the external-drive value at
   a fraction of the surface.
3. **Sessions are a tree** — `parent_id` from day one; map the anti-sunk-cost
   replan loop onto fork/branch-summaries.
4. **Generate the model catalog, key it on API shape, discover local models
   dynamically, and add thinking levels** — a strong local-first differentiator.
5. **Adopt the agentskills.io standard, prompt templates, iterative compaction,
   steering/follow-up queues, bounded tool output, and a hardened supply-chain
   posture** — each is a contained, high-value increment.

And the non-adoptions are as important as the adoptions: **keep the permission
engine, the rule-enforced harness, LocalMind reviewed memory, no telemetry, and
the Rust single-binary.** Those are exactly the things neither OpenCode nor Pi
has — which is precisely why they are LocalPilot's identity.

---

## External references named in this document

- **models.dev** — public model-metadata catalog (used by both OpenCode and Pi).
  Source for W3's generated catalog.
- **ACP (Agent Client Protocol)** — public editor↔agent protocol implemented by
  OpenCode (`packages/opencode/src/acp/`). Target for W2's IDE path.
- **agentskills.io** — public skills standard implemented by Pi. Target for
  LocalPilot skill-loading interop.
- These are public specs/datasets; implement against the spec, do not copy any
  agent's parser or transcribe its generated tables (see §10).
