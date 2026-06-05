# Lessons — durable engineering notes

Permanent home for lessons worth keeping past any one work cycle. Plan-agnostic:
no disposable IDs, just the gotcha and how to handle it. Append as you learn.

## MSRV 1.82 dependency pinning

The newest releases of several deps require a rustc newer than the pinned MSRV
(1.82). Pin the last MSRV-compatible version explicitly in the workspace and the
lockfile:

- Dev tools: `cargo-nextest` ≥ 0.9.98 wants rustc 1.91, and the `0.9.97-b.2`
  beta segfaults on Windows — pin `nextest 0.9.92`. `cargo-machete` ≥ 0.8 needs
  `edition2024` — pin `machete 0.7.0`. Pin `insta 1.47.2`.
- HTTP stack transitives (pulled by `reqwest`/`wiremock`): pin
  `hyper-rustls 0.27.5` (≥ 0.27.9 needs rustc 1.85), `idna_adapter 1.2.0`, and
  `getrandom@0.3 → 0.3.1` (0.3.4 pulls a `wasip2`/`wit-bindgen 0.57` chain that
  needs `edition2024`).
- File-walking: `ignore 0.4.23` pulls `globset 0.4.18` (needs `edition2024`) —
  pin `globset 0.4.16`.
- TUI transitives (via ratatui): pin `unicode-segmentation 1.12.0`,
  `instability 0.3.7`, `darling 0.20.10`.

Note: `cargo deny` and `cargo metadata` parse manifests for **all** targets, so a
wasm/wasi-only transitive that breaks MSRV still fails the supply-chain gate even
though it never builds on a normal target. Pin it anyway.

## Supply-chain gate

`cargo deny check` reports `advisories FAILED` as soon as any pinned dep carries a
RustSec advisory — resolve advisories at the point you add the dep, not later, or
it blocks CI. Unmaintained-only advisories with no fix (e.g. the `paste` crate)
can be explicitly ignored in `deny.toml` with a rationale; new licenses pulled in
(Zlib, CDLA-Permissive-2.0) must be added to the allow-list.

## TOML parsing

`toml 0.8.19` and `toml_edit 0.22.27` are API-incompatible (the `toml` crate fails
to build against that `toml_edit`). Pin `toml_edit 0.22.22`, and prefer parsing
TOML through `figment` so there is no direct `toml` dependency to keep in sync.

## Windows `x86_64-pc-windows-gnu` native-crate crashes

The GNU toolchain on Windows is sensitive to native-dependency build shape.
Avoid dev-only feature unification when a member crate is also a dependency of
another member: `unshackled-config` enabling `figment/test` made
`cargo test --workspace` build a different `unshackled-harness` test binary that
crashed (`0xc0000005`) before test listing. Replacing `figment::Jail` with a
small local environment-isolation helper removed that feature edge and made
`cargo test --workspace` pass locally again.

One native crash class remains known:

- `crossterm` (ratatui's default backend / `tui-textarea`): crashes the test
  binary at init. Set `ratatui = { default-features = false }` workspace-wide and
  keep `crossterm`/`tui-textarea` only in the CLI terminal driver (behind the
  opt-in `tui` feature). The TUI core then snapshot-tests via `TestBackend`
  without linking a real terminal backend.

The same `crossterm` link also makes the **default** binary segfault at startup on
windows-gnu, so the interactive REPL must stay feature-gated; the default build
links no terminal backend.

## Clippy in integration tests

`allow-unwrap-in-tests` does **not** exempt non-`#[test]` helper functions in
`tests/` integration files. Add
`#![allow(clippy::unwrap_used, clippy::expect_used)]` at the top of any such test
file that has helpers.

## Cross-platform path tests

On Windows, a path like `/tmp/x` is **not** absolute (no drive prefix), so it
resolves *inside* the workspace. "Outside-workspace" boundary tests must use a
real second `tempdir()` absolute path, not a hardcoded POSIX path, to be correct
on all three tier-1 platforms.

## Live validation versus contract tests

Provider adapter tests and offline golden tasks prove deterministic contracts:
request shaping, parsing, recovery, permission handling, and pass/fail checks.
They do not prove that a real hosted model, local model, or gateway completes
representative dogfood tasks. If live validation is blocked by credentials,
budget, or model availability, record it as an explicit accepted limitation and
keep the live run as ongoing development validation rather than leaving a plan
box vaguely open.
