# How-To guides

Task-oriented recipes — each answers a single "how do I…?" against shipped
behaviour at the current `VERSION`. See **[[Getting-Started]]** first.

> **Do not edit on github.com.** This wiki is generated from in-repo Markdown
> under `docs/wiki/` and synced one-way on every push to `main`. Edit the source
> in `docs/wiki/`; web edits are overwritten on the next sync.

## Initialize a project

```sh
localpilot init                 # writes .localpilot.toml + .gitignore entries
localpilot doctor               # version, platform, config, providers, tools, trust
```

## Configure a provider

Add a provider block to `.localpilot.toml` and set the default. A hosted API
reads its key from an environment variable named by `api_key_env`; a local server
needs only `base_url` + `model`:

```toml
[provider]
default = "local"

[providers.local]
kind = "openai-compatible"
base_url = "http://localhost:8080/v1"
model = "your-local-model"
```

List what a configured server actually has loaded:

```sh
localpilot models
```

Per-model context windows and reasoning effort:
[providers.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/providers.md);
full config reference + stability policy:
[configuration.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/configuration.md).

## Store an API key, or switch model mid-chat

Store a key without an environment variable (bring-your-own-key — it goes to the
OS keychain or a `0600` file, never config):

```sh
localpilot login anthropic      # deep-link → paste → validate → store
localpilot logout anthropic     # remove it
```

In `chat`, `/model` switches the active provider/model without losing the
conversation (`/model` lists them; `/model <provider> [model]` switches). Details
and the credential resolution order:
[providers.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/providers.md)
§Storing credentials and §Switching provider/model.

For Google Cloud projects that require ADC instead of an API key, configure a
`google-vertex-openai` provider with `auth = "google_adc"`; see
[providers.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/providers.md)
§Google Cloud Vertex AI Gemini with ADC.

## Name a conversation and resume it by name

Every session has a UUID, but a name is easier to remember. Name the current
conversation from inside `chat`:

```text
/name my-refactor        # or the alias: /rename my-refactor
```

The name shows in the header and status line, and beside the id in `/sessions`
and `localpilot session list`. Names are unique per workspace.

Resume by name (or id) from the shell — no flag needed to tell them apart, since a
session id is a UUID:

```sh
localpilot chat --resume my-refactor    # reopen it interactively
localpilot chat --continue              # reopen the most recent session
localpilot session resume my-refactor --prompt "..." --model <m>
localpilot print --resume my-refactor "..." --model <m>
```

You can also name or rename a session from outside a chat:

```sh
localpilot session name <id-or-name> my-refactor
```

## Add an MCP tool server

Configure a Model Context Protocol server so its tools become available to the
agent. The setup and transport details are in
[mcp.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/mcp.md).

## Use tools safely

Tools run through a permission-gated registry; risky actions need explicit
approval and `bypass` is never the default. The tool model and its contracts are
in [05-tool-system.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/05-tool-system.md);
security boundaries in
[07-security-and-privacy.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/07-security-and-privacy.md).

## Ingest a folder and query its knowledge

```sh
localpilot ingest run                        # walk the workspace and build the index
localpilot ingest refresh                    # re-index only changed files
localpilot ingest status                     # current job + what the next run will do
localpilot knowledge search "retry policy"   # query the ingested knowledge
localpilot knowledge pack "fix the parser"   # package a task-specific context bundle
```

Each subcommand is required — bare `localpilot ingest` prints the subcommand
list. Inside the `chat` REPL the same actions are `/ingest run`, `/ingest
refresh`, and `/ingest resume`; the walking actions show a live progress loader
(discovering → parsing → indexing → writing) while they run, and Ctrl-C pauses
the job so `/ingest resume` can continue it.

## Research a topic

Research pulls together local sources and (by default) the web to answer a topic,
with a loud egress disclosure shown before any request. From inside `chat`:

- `/research <topic>` runs one pass and returns to your prior mode. Long output
  opens a scrollable report — `Ctrl+C` copies it, `Esc` closes it. A single
  `Ctrl+C` during a run ends it early with a partial report rather than losing it.
- Bare `/research` enters a persistent research mode: every plain prompt is a new
  topic. The footer, settings, and composer show the mode; `/agent` exits back to
  agent mode.

Research is text-only — if you submit a prompt with an attached image while in
research mode, it is declined with a notice and your draft and attachment are kept.
Web research is on by default and disclosed; `[research.web] enabled = false` runs
local-only, and `[research] enabled = false` turns research off entirely (see
[configuration.md](../configuration.md)). The headless equivalent is
`localpilot research <topic>`.

## Run the rule-enforced harness

```sh
localpilot harness intake       # idea -> brief.md
localpilot harness plan         # brief.md -> PROGRESS.md
localpilot harness feature      # worked, committed steps; resume on quota
```

Inside full-screen chat you can also resume harness work without leaving the session:
`/harness-resume` continues plan steps and `/wait-resume` waits for quota and then
resumes, both entering Harness mode (the footer shows it; `/agent` exits). They use the
model, provider, and permission profile in force when you invoke them, tool approvals
appear in the normal dialog, a single Ctrl+C stops gracefully, and the result opens as a
bounded report.

Intake can optionally gate on a guidance score (`[harness.guidance]` or
`--guidance`): when the idea leaves load-bearing product decisions open,
intake asks about them before writing `brief.md` instead of guessing — see
[06-harness-spec.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/06-harness-spec.md)
§`localpilot harness intake`.

The nine harness gates are specified in
[06-harness-spec.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/06-harness-spec.md).
