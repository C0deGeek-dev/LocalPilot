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
