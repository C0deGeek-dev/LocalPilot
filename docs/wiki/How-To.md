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

## Run the rule-enforced harness

```sh
localpilot harness intake       # idea -> brief.md
localpilot harness plan         # brief.md -> PROGRESS.md
localpilot harness feature      # worked, committed steps; resume on quota
```

The nine harness gates are specified in
[06-harness-spec.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/06-harness-spec.md).
