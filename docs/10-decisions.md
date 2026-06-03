# Architecture Decision Records

This file starts the decision log. Add new records at the top.

## ADR-0008: Anthropic Messages API as the Second Provider

Status: accepted

A second, protocol-distinct provider adapter is added alongside the
OpenAI-compatible one: the Anthropic Messages API. It is implemented clean-room
from the public API reference, talks only to the documented official endpoint,
and exercises the provider trait's generality (top-level `system`,
`tool_use`/`tool_result` content blocks, a required `max_tokens`, and a typed
SSE stream).

Reason:

- satisfies the Stable requirement of at least two provider implementations
  ([`docs/09`](09-release-plan.md))
- proves the provider abstraction is not OpenAI-shaped by construction
- adds a major hosted model family without coupling the core to it (ADR-0002)

## ADR-0007: Windows, Linux, and macOS Are All Tier-1

Status: accepted

Unshackled targets Windows, Linux, and macOS as equal first-class platforms. No
platform is a second-class port. Behavior parity is a release requirement, CI
builds and tests on all three, and installers ship for all three.

Reason:

- the target users run on all three platforms
- shell/filesystem security policy must be correct per-platform, not POSIX-only
- treating one OS as primary causes silent breakage on the others
- forces explicit Windows and POSIX command/path handling from the start

## ADR-0006: Ratatui as the TUI Framework

Status: accepted

The terminal UI is built on `ratatui` with the `crossterm` backend and
`tui-textarea` for input. This is a committed choice, not a recommendation.

Reason:

- `ratatui` is actively maintained and the de facto Rust TUI framework
- `crossterm` provides one terminal backend across Windows, Linux, and macOS,
  supporting the tier-1 platform commitment (ADR-0007)
- a single committed stack keeps rendering, layout, and snapshot tests uniform
- alternatives are out of scope unless a future ADR supersedes this one

## ADR-0005: Read-Only Local Behavior Reference

Status: accepted

A local working implementation may be inspected as a read-only behavior
reference while planning and implementing this Rust project.

The reference may be used to clarify expected workflows, command behavior,
configuration shape, user-facing edge cases, and high-level product
requirements. It must not be used as source material for copied, translated, or
mechanically ported code, prompts, tests, private endpoint behavior,
implementation structure, identifiers, UI copy, branding, or other prohibited
material.

Reason:

- preserves momentum while the Rust specs are still incomplete
- gives implementers a working behavior baseline for ambiguous flows
- keeps this repository independently authored and clean-room auditable
- makes provenance expectations explicit in planning and review

## ADR-0004: No Private Endpoint Adapters

Status: accepted

Unshackled will not implement adapters for private, undocumented, or
consumer-product endpoints. Provider integrations must use official APIs, local
servers, or explicit user-owned custom endpoints.

Reason:

- reduces legal and account risk
- keeps provider contracts stable
- avoids brittle reverse-engineered behavior
- preserves trust in the project

## ADR-0003: Project Files Are Harness Source of Truth

Status: accepted

The harness treats `brief.md` and `PROGRESS.md` as authoritative. Transcripts
are helpful context but not authoritative state.

Reason:

- users can inspect and edit plans
- sessions can resume after crashes
- implementation remains auditable

## ADR-0002: Provider-Neutral Core

Status: accepted

The core crate must not depend on provider-specific APIs or payload shapes.

Reason:

- avoids coupling the product to one vendor
- makes local models first-class
- keeps tests independent of network access

## ADR-0001: Rust Workspace with Narrow Crates

Status: accepted

Unshackled is split into narrow crates rather than one large binary crate.

Reason:

- clearer boundaries
- easier clean-room review
- smaller test surfaces
- easier future embedding

