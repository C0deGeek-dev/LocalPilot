# Plan-Template Overrides — LocalPilot

Project-specific content spliced into a copy of the canonical plan template
(the `plan-from-template` skill in the c0degeek-ai plugin/repo). The canonical
template is generic; everything LocalPilot-specific lives here. Never fork the
template — generic improvements go upstream to c0degeek-ai instead.

Each section below names the extension point in the copied plan where its
content lands.

## After the Purpose block

> This is a *build-process* plan for LocalPilot — developer-process tooling,
> not a shipped product artefact. Keep it separate from the product harness's
> own `brief.md` / `PROGRESS.md` (runtime files spec'd in
> `docs/06-harness-spec.md`).

> **Name-clash rule.** Never name a build-plan file `PROGRESS.md` or
> `brief.md` — those names are reserved for the product harness runtime.
> Build-plan tracking lives only in subject Progress-log sections under
> `tasks/`.

Disposable timing for this repo: the plan and its `tasks/<name>/` folder are
deleted (or archived out of the repo) **before v1**.

## §2 Verification-commands rows (repo defaults, mirror CI)

| Purpose | Command | Notes |
|---|---|---|
| Build | `cargo check --workspace` | |
| Test | `cargo test --workspace` | or `cargo nextest run --workspace` |
| Lint/format | `cargo fmt --check` then `cargo clippy --workspace --all-targets -- -D warnings` | both must be clean |
| Dep hygiene | `cargo machete` | on dependency change only |
| Release hygiene | `cargo deny check` and `cargo audit` | before a release milestone only |

## §4 ADR promotion target

Durable architecture decisions graduate to a real ADR in
`docs/10-decisions.md` in the house format; cite the ADR number in the Refs
column. Transient build-sequencing choices stay in the plan's decision log.

## §6 plan-specific principles (slot 16)

- **Clean-room provenance is blocking.** All code, prompts, tests,
  identifiers, and UI copy original to this repo; official public APIs or
  local servers only. See the `clean-room-guard` skill and
  `docs/00-clean-room.md`.
- **Rust engineering rules hold** (`docs/13-rust-best-practices.md`): MSRV
  1.82, exact-pinned workspace deps, typed errors per crate,
  `#![forbid(unsafe_code)]`, no `unwrap`/`expect`/`panic!` on library runtime
  paths, cross-platform path/shell discipline.
- **Tier-1 parity.** Windows, Linux, and macOS are equal tier-1 (ADR-0007). A
  box that only works on one OS is not done.
- **Doc-ownership map (routes §6.16's documentation-impact review).** When the
  §6.16 review fires, match the change to its owning doc before editing; do not
  duplicate an area across docs. The numbered
  spec set is canonical — the routing table in `CLAUDE.md` ("## The specs") is
  the index:
  - `docs/00-clean-room.md` — provenance (read first)
  - `docs/01-product-spec.md` — product definition, jobs, operating modes
  - `docs/02-architecture.md` — system shape, per-crate responsibilities
  - `docs/04-provider-contract.md` — provider contract; `docs/providers.md` — per-provider usage
  - `docs/05-tool-system.md` — tool system; `docs/mcp.md` — MCP integration
  - `docs/06-harness-spec.md` — harness runtime (`brief.md`/`PROGRESS.md` are runtime files, never plan files)
  - `docs/07-security-and-privacy.md` + `SECURITY.md` — security/privacy
  - `docs/08-testing.md` — testing; `docs/09-release-plan.md` — release
  - `docs/10-decisions.md` — ADRs (durable decisions land here)
  - `docs/13-rust-best-practices.md` — engineering style; `docs/14-dev-tooling.md` — dev tooling
  - `docs/install.md`, `docs/configuration.md`, `docs/extending.md`, `docs/embedding.md`, `docs/localmind-integration.md` — task topics
  - `README.md` — lean overview + entry points only; deep content lives in `docs/`
- **Wiki source of truth is in-repo.** `docs/wiki/` is authoritative and
  PR-reviewed; the published GitHub Wiki is a one-way generated mirror — never
  hand-edited on github.com. Wiki Reference pages **link** the owned `docs/`,
  they do not duplicate it.
- **CHANGELOG + VERSION discipline.** Any user-facing change updates
  `CHANGELOG.md` under an Unreleased/next-version heading in the same checkpoint;
  no doc, README, or wiki page may claim behaviour beyond the current `VERSION`.

## §7 plan-specific gates

- [ ] Durable architecture decisions promoted to ADRs in
      `docs/10-decisions.md` (house format), cited by number in the plan's
      decision log.

## Captain Hindsight prompt — extra "Check specifically for" lines

- Clean-room provenance: any copied prompt/identifier/UI copy, or any
  private/undocumented endpoint use.
- Cross-platform parity (Windows/Linux/macOS) for anything OS-specific.
- Whether a spec deviation is durable enough to promote to an ADR in
  `docs/10-decisions.md`.
- Any `docs/`, `README.md`, or `docs/wiki/` claim that does not match shipped
  behaviour at the current `VERSION`, and any wiki page hand-edited on
  github.com instead of the in-repo `docs/wiki/` source.
