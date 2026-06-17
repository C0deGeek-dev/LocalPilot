# LocalPilot docs

Documentation index and doc-ownership map for the spec set. Read the doc that
owns the area you are changing before you change it; don't restate the same area
in two places. `CLAUDE.md` ("## The specs") routes here; ADRs in
[`10-decisions.md`](10-decisions.md) win over style rules.

## Numbered spec set (canonical)

| # | Doc | Owns |
|---|---|---|
| 00 | [`00-clean-room.md`](00-clean-room.md) | Clean-room provenance (read first) |
| 01 | [`01-product-spec.md`](01-product-spec.md) | Product definition, jobs, operating modes |
| 02 | [`02-architecture.md`](02-architecture.md) | System shape, per-crate responsibilities |
| 03 | [`03-implementation-plan.md`](03-implementation-plan.md) | Implementation phases |
| 04 | [`04-provider-contract.md`](04-provider-contract.md) | Provider contract |
| 05 | [`05-tool-system.md`](05-tool-system.md) | Tool system |
| 06 | [`06-harness-spec.md`](06-harness-spec.md) | Harness runtime (`brief.md`/`PROGRESS.md` are runtime files) |
| 07 | [`07-security-and-privacy.md`](07-security-and-privacy.md) | Security and privacy |
| 08 | [`08-testing.md`](08-testing.md) | Testing |
| 09 | [`09-release-plan.md`](09-release-plan.md) | Release plan |
| 10 | [`10-decisions.md`](10-decisions.md) | Decisions / ADRs |
| 11 | [`11-implementation-checklist.md`](11-implementation-checklist.md) | Implementation checklist |
| 12 | [`12-feature-specs.md`](12-feature-specs.md) | Feature specs |
| 13 | [`13-rust-best-practices.md`](13-rust-best-practices.md) | Engineering style guide |
| 14 | [`14-dev-tooling.md`](14-dev-tooling.md) | Developer tooling |

## Topic docs

| Doc | Owns |
|---|---|
| [`install.md`](install.md) | Installation |
| [`configuration.md`](configuration.md) | Configuration reference |
| [`providers.md`](providers.md) | Per-provider usage |
| [`mcp.md`](mcp.md) | MCP server integration |
| [`extending.md`](extending.md) | Extending LocalPilot |
| [`embedding.md`](embedding.md) | Embedding / context intelligence |
| [`context-intelligence-vision.md`](context-intelligence-vision.md) | Context-intelligence direction |
| [`localmind-integration.md`](localmind-integration.md) | LocalMind integration |
| [`security.md`](security.md) | Security usage notes (see also `07-security-and-privacy.md` + `SECURITY.md`) |
| [`mvp-test-coverage.md`](mvp-test-coverage.md) | MVP test-coverage tracking |

## Wiki

User-facing guides (Getting Started, How-Tos, Examples, Troubleshooting) are
authored as in-repo Markdown under `docs/wiki/` and one-way CI-synced to the
GitHub Wiki. The in-repo source is authoritative — never edit pages on
github.com. Wiki Reference pages **link** this spec set rather than duplicating
it; the spec set is not re-authored for the wiki.

## Changelog & version

Every user-facing change updates the top-level `CHANGELOG.md` in the same
checkpoint. No doc, README, or wiki page may claim behaviour beyond the current
`VERSION`. Durable architecture decisions are promoted to `10-decisions.md`.
