# Unshackled

Unshackled is a Rust-native, provider-neutral coding-agent harness.

It is not a fork, clone, port, or redistribution of any vendor CLI. The project
is designed from first principles around a small set of public concepts:

- a terminal interface for agentic software development
- two operating modes: a default conversational agent mode and an opt-in,
  rule-enforced harness mode
- official model/provider APIs
- a rule-enforced harness that turns vague tasks into inspectable plans
- local state stored in ordinary project files
- explicit permission boundaries for filesystem, shell, network, and external tools

## Project Status

This repository is a clean-room scaffold. It contains:

- a compilable Cargo workspace
- initial crate boundaries
- product and technical specifications
- implementation roadmap
- legal/provenance guardrails for original development

It intentionally does not contain implementation copied from any existing
closed-source or leaked codebase.

## Repository Layout

```text
crates/
  unshackled-cli/       CLI entrypoint and command routing
  unshackled-core/      Provider-neutral domain types
  unshackled-config/    Config schema and loading
  unshackled-llm/       Official provider API abstraction
  unshackled-tools/     Tool registry and tool execution contracts
  unshackled-harness/   Intake, planning, progress, and rule engine
  unshackled-tui/       Terminal UI
  unshackled-store/     Session persistence
  unshackled-sandbox/   Permission and execution policy
  unshackled-mcp/       MCP integration boundary
docs/
  00-clean-room.md
  01-product-spec.md
  02-architecture.md
  03-implementation-plan.md
  04-provider-contract.md
  05-tool-system.md
  06-harness-spec.md
  07-security-and-privacy.md
  08-testing.md
  09-release-plan.md
  10-decisions.md
  11-implementation-checklist.md
  12-feature-specs.md
```

## Build

```powershell
cargo check
cargo test
cargo run -p unshackled -- doctor
```

## Design Principles

1. Original implementation only.
2. Official APIs only.
3. Provider-neutral core.
4. Local-first project state.
5. Explicit user control for risky actions.
6. Reproducible planning and progress.
7. No hidden consumer-product automation.
8. No vendor branding as product identity.

## First Milestone

Milestone 1 is a non-interactive harness:

```powershell
unshackled init
unshackled harness intake --idea "build a small todo CLI"
unshackled harness plan
unshackled harness status
```

No autonomous file editing is required for Milestone 1. That keeps the first
release small, auditable, and legally clean.
