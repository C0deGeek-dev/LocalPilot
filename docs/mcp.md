# Connecting MCP servers

LocalPilot can expose tools from [Model Context Protocol](https://modelcontextprotocol.io)
servers to the model. Each server is launched as a local subprocess that speaks
JSON-RPC over stdio. Its tools are registered alongside the builtins and run
through the **same** permission engine and output redaction — an MCP tool call
prompts (or is denied) exactly like a builtin, and is never a side channel.

This page covers LocalPilot as an MCP *client*. The reverse direction —
`localpilot mcp serve`, which lets an MCP client such as another agent host
drive a LocalPilot session — is part of the headless-drive surface, documented
in [embedding.md](embedding.md#mcp-over-stdio).

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

## MCP as the catalog's volatile edge

When the pull-discovery broker is enabled (`[tools] broker`, see
[`docs/05-tool-system.md`](05-tool-system.md) and
[`docs/configuration.md`](configuration.md)), each MCP tool is attributed to its
server in the live, fingerprinted tool **catalog**. MCP is the catalog's volatile
edge: a server's advertised `tools/list` is authoritative for that server's
entries, so a tool a server stops advertising simply drops out of the next
projection (a `removed` delta) and a schema bump shows up as a `changed` delta.
The catalog is a derived projection of the registry — never a second source of
truth.

**Deprecation is overlay-only.** The MCP protocol carries no `deprecated` flag,
version, or replacement field on a tool (spec rev 2025-06-18): a tool's
*disappearance* from `tools/list` is the only removal signal. So a retired tool is
handled two ways: a call to a name the registry no longer has routes through the
broker's failure-driven re-resolution ("X retired; closest now: Y"), and an
optional hand-maintained old→replacement **overlay** sharpens that hint when known.
The overlay only annotates and de-ranks an entry; it grants and removes nothing. A
server that volunteers a non-standard `_meta.deprecated`/`_meta.replacedBy` hint is
read best-effort, but that is off the standard.
