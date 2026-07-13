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

## Research search tools

Web research (see [`docs/configuration.md`](configuration.md) `[research]`)
can use designated MCP search tools as **candidate-URL proposers** — real
search instead of model-guessed URLs. Designation is explicit, per
`(server, tool)` pair; nothing is auto-discovered (search servers share no
tool-naming convention, and consulting one sends the redacted sub-question
text to it):

```toml
[research.mcp]
tools = [{ server = "search", tool = "search" }]
```

The named server must exist under `[mcp.servers]`. During a web-active
research run each designated tool is called once per sub-question with the
**redacted** query only; the URLs extracted from its results feed the same
allowlist/disallowlist-gated, audited, no-redirect fetch path as
model-proposed URLs — a search result is a lead, never evidence. Each search
call is itself audited (`decision=search…` lines), a tool that errors or
rate-limits is skipped without failing the run, and the run's egress
disclosure names every designated tool. The proposer parses the common result
shapes (plain-text `URL:` lists, JSON-in-text, `structuredContent`,
`resource_link` items) and treats URL-less prose as an empty round.

Note on provenance: some community search servers scrape engines that offer
no official API, while vendor servers (and self-hosted SearXNG) speak
official interfaces — the choice of server, and its provenance, is yours.

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
