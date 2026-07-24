---
name: add-mcp-integration
description: >-
  Wire an MCP client integration — exposing MCP tools/resources through the SAME
  permission and redaction pipeline as builtin tools, never a side channel. Use
  when working in localpilot-mcp.
---

# add an MCP integration

Authoritative shape: [`docs/02-architecture.md`](../../../docs/02-architecture.md)
§`localpilot-mcp`. MCP is v1 scope.

## The one rule that matters

MCP-provided tools and resources go through the **same permission engine and the
same redaction pipeline** as builtin tools. MCP is not a permission side channel.
A model must not reach a side effect via MCP that the permission engine would
otherwise gate.

## Steps

- Speak the published MCP protocol; treat the server as untrusted third-party
  code (it runs against this workspace).
- Map MCP tool descriptors into the same typed tool/permission model the builtin
  tools use; route every call through `localpilot-sandbox`.
- Redact secrets before logging or persisting any MCP payload, not after.

## Must-pass tests

An MCP tool call that is gated/denied by the permission engine exactly as a
builtin would be; a redaction test on MCP I/O. Keep all of it original
(see [[clean-room-guard]]).
