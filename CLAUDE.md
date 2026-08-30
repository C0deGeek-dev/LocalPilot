# CLAUDE.md

Instructions for AI assistants working in this repository. This file is a
router, not a rulebook — it points at the authoritative specs rather than
restating them.

## Start here

- [`AGENTS.md`](AGENTS.md) — agent ground rules, including the read-only
  behavior-reference policy.
- [`docs/`](docs/) — the product specification. Read the doc that owns the area
  you are changing before you change it.

## The specs

| Area | Doc |
| --- | --- |
| Clean-room provenance (**read first**) | [`docs/00-clean-room.md`](docs/00-clean-room.md) |
| Product definition, jobs, operating modes | [`docs/01-product-spec.md`](docs/01-product-spec.md) |
| System shape, per-crate responsibilities | [`docs/02-architecture.md`](docs/02-architecture.md) |
| Implementation phases | [`docs/03-implementation-plan.md`](docs/03-implementation-plan.md) |
| Provider contract | [`docs/04-provider-contract.md`](docs/04-provider-contract.md) |
| Tool system | [`docs/05-tool-system.md`](docs/05-tool-system.md) |
| Harness spec | [`docs/06-harness-spec.md`](docs/06-harness-spec.md) |
| Security and privacy | [`docs/07-security-and-privacy.md`](docs/07-security-and-privacy.md) |
| Testing | [`docs/08-testing.md`](docs/08-testing.md) |
| Release plan | [`docs/09-release-plan.md`](docs/09-release-plan.md) |
| Decisions (ADRs win over style rules) | [`docs/10-decisions.md`](docs/10-decisions.md) |
| Engineering style guide | [`docs/13-rust-best-practices.md`](docs/13-rust-best-practices.md) |
| Developer tooling | [`docs/14-dev-tooling.md`](docs/14-dev-tooling.md) |

## Non-negotiables

- **Clean-room provenance is blocking.** All code, prompts, tests, identifiers,
  and UI copy must be original to this repository. Official public APIs or
  local servers only — never private or undocumented endpoints. See
  [`docs/00-clean-room.md`](docs/00-clean-room.md) and `CONTRIBUTING.md` for the
  PR provenance note.
- **Engineering rules live in [`docs/13-rust-best-practices.md`](docs/13-rust-best-practices.md)** —
  MSRV 1.82, exact-pinned workspace deps, typed errors per crate,
  `#![forbid(unsafe_code)]`, no `unwrap`/`expect`/`panic!` on library runtime
  paths, cross-platform path/shell discipline. Do not duplicate them here.
- **Windows, Linux, and macOS are equal tier-1 platforms** (ADR-0007).

## Local gate (mirror CI)

```powershell
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check --workspace
```
