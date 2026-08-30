# Clean-Room Development Policy

## Purpose

This project must be independently authored. The goal is not to reproduce any
vendor CLI. The goal is to build an original coding-agent harness in Rust.

This policy is part of the technical spec. Pull requests that violate it should
be rejected even if the code works.

## Scoped Owner Exception: Terminal Chat Experience

The project owner has authorized one narrow exception for LocalPilot's
interactive terminal chat: its observable layout, geometry, interactions, and
behavior may deliberately match the normally used public distribution of
GitHub Copilot CLI 1.0.75. Normal-use observation, public documentation, and the
public changelog are allowed evidence for that surface.

The exception does **not** permit proprietary or leaked source, source maps,
hidden prompts, private endpoints, credentials, non-UI internals, or copied
implementation structure. Shipped UI remains LocalPilot: no GitHub/Copilot
name, logo, wordmark, mascot, brand theme, or brand-identifying verbatim copy.
Provider and tool behavior stays provider-neutral and uses only the public/local
surfaces allowed below. Tests are authored from the recorded observable
contract, not a proprietary implementation.

This exception applies only to that terminal-chat surface. Every other change
continues to follow the policy without modification. See ADR-0108.

## Hard Rules

Do not copy, translate, port, summarize, or mechanically transform:

- proprietary source code
- leaked source code
- private source maps
- bundled prompts from closed-source products
- endpoint payloads from private or undocumented consumer APIs
- hidden feature flags from private products
- tests or fixtures derived from proprietary implementations
- logos, product names, color systems, UI copy, or branding from vendor tools
- internal file names, internal class names, function names, or identifiers

Do not implement adapters for private consumer-product endpoints. All providers
must use official public API surfaces or local model servers that the user runs.

## Allowed Inputs

The following inputs are acceptable:

- original ideas written for this repository
- public API documentation
- published protocol specifications
- behavior observed by using a product normally, documented at a high level
- open-source crates used according to their licenses
- user-authored requirements
- conventional CLI and TUI patterns

## Local Behavior Reference

A local working implementation may be used as a read-only behavior reference
while planning and implementing this Rust project. Its purpose is to clarify
expected workflows, command behavior, configuration shape, user-facing edge
cases, and high-level product requirements when this repository's own docs are
incomplete.

Using that reference does not relax the hard rules above. Contributors must not
copy, translate, port, mechanically transform, or derive tests, prompts, private
endpoint behavior, identifiers, UI copy, branding, or implementation structure
from the reference. Rust implementations must be independently designed
from this repository's specs, public documentation, and original requirements.

When a change was informed by a local behavior reference, record the observed
behavior at a high level in the PR provenance note, for example:

```text
Behavior cross-checked against a local read-only behavior reference.
Implementation, prompts, tests, and API details are original to this repository.
```

## Clean-Room Roles

For any feature inspired by an existing product category:

1. A spec writer writes a feature-level requirement without code, prompt text,
   private endpoint details, or implementation structure.
2. An implementer builds from that spec and public documentation.
3. A reviewer checks for provenance, not just correctness.

One person may fill all roles for features that are obviously generic, such as
`localpilot --help`, TOML config loading, or a local `git status` wrapper. Use
separation when implementing workflows that resemble proprietary coding-agent
products.

## Prohibited Framing

Do not describe this project as:

- a free build of another product
- a fork of another product
- a replacement for a named vendor CLI
- an unlocked version of another product
- a redistribution of exposed source

Acceptable framing:

- "Rust-native coding-agent harness"
- "provider-neutral terminal agent"
- "local-first planning and execution harness"
- "supports official provider APIs"

## Provider Naming

Model and provider names may appear only as compatibility statements, for
example:

- "supports OpenAI through the official OpenAI API"
- "supports local OpenAI-compatible servers such as vLLM"
- "supports other providers through their official APIs"

Provider names must not be used as product identity.

## Review Checklist

Every PR must answer:

- Is this original code?
- Did the author cite public documentation for external APIs?
- Does this use only official APIs or local servers?
- Does this avoid vendor branding and private implementation names?
- Are prompts authored for LocalPilot rather than copied from a product?
- Are tests derived from LocalPilot's spec, not another implementation?
- If the terminal-chat exception applies, is the change limited to observable
  layout/interaction behavior and free of vendor identity or private internals?

## Repository Hygiene

Before public release:

- run a text scan for prohibited product framing
- run license/advisory checks
- verify dependencies have compatible licenses
- verify no API keys, tokens, transcripts, or private data are committed
- verify all example endpoints are official public APIs or localhost
