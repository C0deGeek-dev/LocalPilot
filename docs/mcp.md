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

Each entry is one server: `command`, optional `args`, and an optional `env`
table. On startup LocalPilot spawns the process, performs the MCP handshake, and
discovers its tools. A server that fails to start is skipped with a note on
stderr — it never aborts the session.

### Server environment

A server inherits LocalPilot's environment. Add an `env` table when it needs
something more — a setting, or a credential it cannot otherwise be given:

```toml
[mcp.servers.ask_google]
command = "npx"
args = ["-y", "@gpriday/ask-google-mcp"]

[mcp.servers.ask_google.env]
GOOGLE_API_KEY = { credential = "google-api-key" }
LOG_LEVEL = "info"
```

Store the credential once, then reference it by name:

```console
$ localpilot credential set google-api-key
Paste the value for "google-api-key" and press Enter (stored secret, shown masked):
stored credential "google-api-key" in the keychain (AIza…9xQ2)
```

The config file holds only the alias. The value is read from the credential
store immediately before the server is spawned. This is the recommended way to
give a server a credential.

**Inheritance and precedence.** The child starts from the environment LocalPilot
has, and each configured entry replaces any inherited variable of the same name.
Everything else is inherited unchanged, so a variable you already export keeps
working. Configure nothing and behaviour is exactly as it was.

**A missing credential stops that server.** If `{ credential = "..." }` names
something that is not stored, the server is not spawned at all — the failure
stays a configuration problem instead of becoming a confusing runtime error from
a server that started without the value it needed. Other servers are unaffected.
`localpilot doctor` names the variable and the alias:

```text
ask_google (npx): credential missing; environment variable GOOGLE_API_KEY needs
  the credential "google-api-key", which is not stored (add it with
  `localpilot credential set google-api-key`) (args: 2; command available;
  env: GOOGLE_API_KEY, LOG_LEVEL)
```

**The plaintext escape hatch.** A credential can also be written directly into a
project-local, git-ignored `.localpilot.toml`:

```toml
[mcp.servers.ask_google.env]
GOOGLE_API_KEY = { value = "..." }
```

The object form is what marks it sensitive. It gets the same runtime masking and
the same response filtering as a stored credential; the only difference is that
it sits in plaintext in the file. Prefer the credential store.

> **Never use the plain-string form for a credential.** `KEY = "..."` is treated
> as an ordinary, non-sensitive value: it is *not* filtered out of what the
> server sends back. The object forms exist so you can say "this is a secret".

**What comes back is filtered.** An MCP server is an untrusted subprocess that
can read its own environment, so anything you give it can be returned — in the
handshake, in the tool descriptions it advertises, in a tool result, or in an
error. Every value LocalPilot receives from a server is stripped of the
credentials that server was given, before any of it reaches the model, a
transcript, stored output, or a log. Values shorter than 8 characters are left to
the shared pattern-based redaction instead, because matching a short string
verbatim would corrupt ordinary text. This is defence in depth rather than a
containment boundary — see
[07-security-and-privacy.md](07-security-and-privacy.md).

Adding an environment grants a server no new tool permission and bypasses no
gate; see [Permissions](#permissions) below.

### Tool name collisions

Builtin and earlier-registered tools keep their names. When an MCP tool has the
same name, LocalPilot advertises it as `<server>_<tool>` and prints the rename
on stderr. Characters outside ASCII letters, digits, `_`, and `-` in the server
key become `_` in that prefix. The MCP server still receives `tools/call` with
the tool's original name.

If the prefixed name is already registered, LocalPilot skips the later tool and
prints a warning rather than advertising a duplicate function name. MCP tools
that do not collide keep their original names.

## Permissions

MCP tools are gated as a **network** effect: in an interactive session the REPL
prompts for approval before each call; in a non-interactive run (`print`,
`harness`) they require a trusting profile. Output is redacted before it reaches
the transcript, the model, or the logs.

A server that answers a call with `isError: true` reaches the model as a
failure (`status: error`) with its text intact — a reported failure, not a tool
malfunction, so it never counts against the loop's stuck detection (ADR-0116).
Only a transport- or protocol-level fault is a malfunction. MCP tools are held
to the same result contract as builtins.

## Finding the right tool

Connecting a server proves availability, not use. Two things close that gap, and
neither names a vendor (ADR-0120):

- The agent prompt carries a **version-sensitive documentation policy**: when a
  task depends on current or version-specific behaviour of an external library,
  framework, SDK, API, CLI, or cloud service — an upgrade error, a migration, a
  deprecated API, a changed configuration shape — the model consults a
  documentation tool rather than its own recollection. With the full tool set
  advertised it calls the suitable tool directly; with the pull-discovery broker
  on it searches, reveals, then calls. Stable local implementation questions do
  not trigger a lookup, and when nothing suitable is configured the model
  continues from local evidence and says current documentation was unverified.
- Broker resolution is **capability-aware**, so a need phrased as
  `<library> version upgrade problem` can reach a tool that only describes
  itself as querying documentation. See
  [`docs/extending.md`](extending.md#pull-discovery-broker-host-installed).

## Scope

Only local servers launched over stdio are supported. The connection is used by
the interactive REPL, `print`, and `harness` runs; harness connects each server
once and reuses it across steps.

### One MCP pool per server process

Under `serve` (the opt-in local server that hosts many sessions for multiple
attached clients), the configured MCP servers are spawned **once** at start-up
and their connections form a single shared pool for the whole server process.
Each hosted session projects its own tool registry, but that registry only
references the one pool's MCP clients — MCP subprocesses are never re-spawned per
session. So N concurrent sessions still speak to one set of MCP servers, not N,
and the per-session RAM cost stays small (see the multi-session RAM model in
[02-architecture.md](02-architecture.md)). Redaction and permission gating are
per session as always; only the underlying connections are shared.

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
allowlist/disallowlist-gated, audited fetch path as model-proposed URLs — a
search result is a lead, never evidence. A provider that returns an attribution
or grounding wrapper URL rather than a direct source URL still works: the
wrapper's redirect is followed, but only through LocalPilot's own policy, with
every hop independently re-gated and audited and the final URL recorded as the
evidence locator (ADR-0100, [security](07-security-and-privacy.md)). Each search
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
