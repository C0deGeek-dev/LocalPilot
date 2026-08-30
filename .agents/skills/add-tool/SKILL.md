---
name: add-tool
description: >-
  Add or change a builtin tool — implement the Tool trait, generate its JSON
  schema from typed structs, register it, route every call through the permission
  engine, apply the sandbox/path policy, and write the required allow/deny tests.
  Use when working in localpilot-tools.
---

# add a tool

Authoritative contract: [`docs/05-tool-system.md`](../../../docs/05-tool-system.md).
This skill lists the steps and the safety invariants; the spec owns per-tool
rules.

## Steps

1. Implement the `Tool` trait in `localpilot-tools`. Inputs/outputs are typed
   structs; derive the JSON schema with `schemars` — never hand-write schema.
2. Register the tool in the tool registry.
3. Route execution through the permission engine in `localpilot-sandbox`. The
   tool MUST NOT decide its own permission or bypass the engine.
4. Apply path containment: canonicalize and check normalized `starts_with`
   against the workspace root (mind Windows `\\?\`, case-insensitivity, 8.3, ADS).
5. Use argument lists, never shell strings, for any process execution; classify
   commands per the per-OS table in
   [`docs/07-security-and-privacy.md`](../../../docs/07-security-and-privacy.md).

## Must-pass tests

At least one allow path and one deny path per tool; a containment test that
rejects an escape outside the workspace; a malformed-input test. Treat tool
input and model output as untrusted.
