# Connecting MCP servers

LocalPilot can expose tools from [Model Context Protocol](https://modelcontextprotocol.io)
servers to the model. Each server is launched as a local subprocess that speaks
JSON-RPC over stdio. Its tools are registered alongside the builtins and run
through the **same** permission engine and output redaction — an MCP tool call
prompts (or is denied) exactly like a builtin, and is never a side channel.

## Configuration

Declare servers in `.localpilot.toml`:

```toml
[mcp.servers.files]
command = "my-mcp-file-server"
args = ["--root", "."]

[mcp.servers.search]
command = "uvx"
args = ["some-mcp-search-server"]
```

Each entry is one server: `command` plus optional `args`. On startup LocalPilot
spawns the process, performs the MCP handshake, and discovers its tools. A server
that fails to start is skipped with a note on stderr — it never aborts the
session.

## Permissions

MCP tools are gated as a **network** effect: in an interactive session the REPL
prompts for approval before each call; in a non-interactive run (`print`,
`harness`) they require a trusting profile. Output is redacted before it reaches
the transcript, the model, or the logs.

## Scope

Only local servers launched over stdio are supported. The connection is used by
the interactive REPL, `print`, and `harness` runs; harness connects each server
once and reuses it across steps.
