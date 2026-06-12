# Architecture Decision Records

This file starts the decision log. Add new records at the top.

## ADR-0012: Project `.localpilot.toml` Is Local-Only, Never Committed

Status: accepted. Amends the "committed `.localpilot.toml`" wording in
ADR-0009.

The project-local `.localpilot.toml` is a machine-local file: it is listed in
`.gitignore` and is not committed. External launchers generate provider
config into it in the project directory (base URL, model, key env-var name),
and those values are inherently machine-local. The ratified quality gate
(`[[harness.checks]]`, ADR-0009) lives in the same file and is therefore also
local-only.

Consequences:

- The ratification trust boundary is the explicit user action that writes
  checks into the local file — not version control. Wording in
  [`docs/06`](06-harness-spec.md) and [`docs/07`](07-security-and-privacy.md)
  says "ratified into the project's local `.localpilot.toml`" rather than
  "committed".
- A fresh clone has no ratified gate; `gate propose` / `gate ratify` is the
  supported way to re-establish one. A team that wants a shared, reviewed
  gate definition can keep one in its own committed docs and ratify from it,
  but the harness never reads checks from a committed file.

Reason:

- committing the file would leak machine-local endpoints and invite config
  drift between what a launcher generates and what the repo pins
- one file with one clear lifecycle (generated/edited locally, ignored) beats
  splitting harness config across a committed and an ignored file
- ratification was always defined as the user's explicit act; tying trust to
  VCS state added nothing and contradicted the launcher workflow

## ADR-0011: Store Convergence — Execution Record vs Memory

Status: accepted

LocalPilot persists state in two stacks, which were growing toward overlap.
This record fixes the ownership boundary:

- **The LocalPilot store (`.localpilot/`) is the execution record, and only
  that**: transcripts, the durable session event log (tree-shaped, format-
  versioned), caches, tool-output snapshots, provider metadata, and recovery
  diagnostics. It never grows memory, lesson, retrieval, or review features.
- **LocalMind (`.localmind/`) is the only memory and learning backend**:
  session closeout, candidate lessons, the review queue, accepted memory,
  retrieval/context injection, skill drafts, and audit. New rich-learning
  behavior lands in LocalMind, never as a host-local memory implementation.
- **One redaction authority at the host boundary.** LocalPilot's redaction
  stack (`localpilot-config::redact`) is the canonical redactor: everything
  the host persists or hands to LocalMind is redacted by it first. LocalMind's
  import-time redaction remains as engine-internal defense in depth, not a
  second authority — divergence between the two pattern sets is resolved by
  updating the host stack.

Reason:

- two stores with drifting responsibilities and two redaction pattern sets is
  how secrets leak and how features get implemented twice
- the event log needs a single unambiguous home (the execution record) before
  later features (headless drive, hooks, subagents) build on it
- LocalMind is host-neutral and reusable; baking memory into the LocalPilot
  store would fork that capability

## ADR-0010: Reliability Contract for Unattended Operation

Status: accepted

LocalPilot's differentiator is unattended multi-step execution. That claim is
made testable by an explicit **reliability contract**: a small set of named
invariants the runtime guarantees on every exit path, each pinned by a named
test, split across the owning specs:

- Session-loop invariants (tool-result pairing on every exit path, no partial
  replies persisted, transcript fidelity) —
  [`docs/06`](06-harness-spec.md) §Reliability Contract.
- Permission invariants (no `run_shell` path weaker than the equivalent
  builtin, floor-aware allowlists that never lift destructive/privileged/
  unknown gating, wrapper commands never auto-allowed, approval prompts that
  state their target) — [`docs/07`](07-security-and-privacy.md) §Reliability
  Contract.

A change that breaks a contract-pinning test is a contract change: it requires
a superseding ADR, not a test edit. The bypass profile's scope is part of the
contract: bypass keeps the workspace boundary for path-bearing effects only;
shell commands are not path-contained, and the docs state this rather than
implying containment that does not exist.

Reason:

- the product's central claim ("every side effect passes a typed permission
  engine"; "safe to run unsupervised") was previously aspiration enforced
  only by convention — line-level review found exit paths and classification
  gaps that falsified it
- invariants stated in the spec and enforced by property tests survive
  refactors; workflow descriptions do not
- naming the tests in the spec makes the contract auditable: a reader can run
  the contract

## ADR-0009: Discovered Project Quality Gate

Status: accepted

The harness's single `test_command` is generalized into a quality gate: a set of
language-specific inspection checks — format, lint, test, dependency hygiene,
advisory audit, static analysis — drawn from the project's own toolchain rather
than hardcoded into the engine. Built-in toolchain profiles per stack declare
the default checks, how to interpret a check's findings, and which findings are
safely auto-fixable; a discovery step detects the stack, probes which tools are
actually available, and proposes a gate the user ratifies into committed
`.localpilot.toml`. The rule engine runs checks at a per-check cadence (fast
checks each step, full checks at phase boundaries) and acts on findings: safe
deterministic fixers are applied and re-run, remaining failures feed the
anti-sunk-cost loop (retry, bounded, then replan recorded in `DECISIONS.md`), and
dependency/audit findings block for a human decision. Discovered commands are
untrusted — discovery proposes, the user ratifies, and every check runs through
the same permission engine and sandbox as any other shell command.

Reason:

- replaces a single test hook with real per-language cleanup and inspection
  without baking tool lists into the engine
- keeps the engine stack-neutral: the abstraction is built in, the instances are
  discovered (the spirit of ADR-0002)
- makes findings actionable inside the loop instead of advisory, with bounded
  auto-fix and replan rather than runaway churn
- preserves the security model: discovered commands are ratified once and always
  mediated by the permission engine ([`docs/07`](07-security-and-privacy.md)),
  never auto-trusted
- per-check cadence keeps fast per-step feedback without paying full-suite cost
  on every step

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

LocalPilot targets Windows, Linux, and macOS as equal first-class platforms. No
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

LocalPilot will not implement adapters for private, undocumented, or
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

LocalPilot is split into narrow crates rather than one large binary crate.

Reason:

- clearer boundaries
- easier clean-room review
- smaller test surfaces
- easier future embedding

