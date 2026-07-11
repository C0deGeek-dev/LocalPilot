# Examples

Copy-pasteable samples that match shipped behaviour at the current `VERSION`.

> **Do not edit on github.com.** This wiki is generated from in-repo Markdown
> under `docs/wiki/` and synced one-way on every push to `main`. Edit the source
> in `docs/wiki/`; web edits are overwritten on the next sync.

## One-shot question (no tools)

```sh
localpilot ask --model your-local-model "where is retry handled in this repo?"
```

Streams a single answer and exits — no tool calls, no session.

## Non-interactive agent run in a pipeline

```sh
localpilot print "add a unit test for the config loader"
localpilot print --continue "now run it and fix any failures"
localpilot print --resume <session-id> "pick up where we left off"
```

`print` runs the agent loop once and prints the result, suitable for scripts and
CI. `--continue` / `--resume` rebuild an existing session from the event log.

## Inspect and resume durable sessions

```sh
localpilot session list
localpilot session export <session-id> bundle.json
localpilot session resume <session-id>
```

Sessions are rebuilt from a durable event log, so you can resume or fork them.

## Drive it headless over RPC

```sh
localpilot rpc            # newline-delimited JSON commands in, streamed events out
localpilot rpc --continue # same, resuming the workspace's most recent session
localpilot acp            # Agent Client Protocol (JSON-RPC over stdio) for editors
localpilot mcp serve      # MCP server: another agent host drives + steers a session
```

Embedding and headless drive are covered in
[embedding.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/embedding.md).
