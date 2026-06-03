# Lessons — `unshackled` Run Notes

> Append a line the moment a slice teaches something — not at the gate. These
> are disposable run-notes; durable lessons migrate to the permanent
> `tasks/lessons.md` at the §7 gate before this folder is deleted.

| Date | Slice | Lesson | Box / file |
|---|---|---|---|
| 2026-06-02 | 00/1 | MSRV-1.82 pin blocks newest dev tools: `cargo-nextest` ≥0.9.98 needs rustc 1.91, `cargo-machete` ≥0.8 needs `edition2024`. Pin `nextest 0.9.92` (the `0.9.97-b.2` beta segfaults on Windows), `machete 0.7.0`, `insta 1.47.2`. | 00.2 / D004 |
| 2026-06-02 | 03 | Pulling `reqwest`/`wiremock` cascades newer transitives that break MSRV 1.82: pin `hyper-rustls 0.27.5` (≥0.27.9 needs rustc 1.85), `idna_adapter 1.2.0`, and `getrandom@0.3 → 0.3.1` (0.3.4 pulls `wasip2`→`wit-bindgen 0.57` which needs `edition2024`). `cargo deny`/`cargo metadata` parse all-target manifests, so a wasi-only transitive still breaks the supply-chain gate. | 03 / D010 |
| 2026-06-02 | 03 | Local toolchain is `x86_64-pc-windows-gnu`; aggregate test binaries can crash (`0xc0000005`) when dev-only feature graphs are unified into another member's test binary. Concrete fix: remove `figment/test` from `unshackled-config` dev-deps and use a local env-isolation helper instead of `figment::Jail`; `cargo test --workspace` passes locally again. | 03 / D012 |
| 2026-06-02 | 04 | `ignore 0.4.23` pulls `globset 0.4.18` which needs `edition2024`; pin `globset 0.4.16` under MSRV 1.82. | 04 |
| 2026-06-02 | 04 | clippy `allow-unwrap-in-tests` does NOT exempt non-`#[test]` helper fns in `tests/` integration files; add `#![allow(clippy::unwrap_used, clippy::expect_used)]` at the top of such test files. | 04 |
| 2026-06-02 | 04 | On Windows a path like `/tmp/x` is not absolute (no drive prefix), so it resolves *inside* the workspace; "outside-workspace" tests must use a real second `tempdir()` absolute path for cross-platform correctness. | 04 |
| 2026-06-02 | 00/1 | `cargo deny check` already reports `advisories FAILED` on the scaffold's `Cargo.lock` — a pinned dep carries a RustSec advisory. Resolve in the subject 01 supply-chain gate before it blocks CI. | 01 / deny.toml |
| 2026-06-02 | 07 | `toml 0.8.19` + `toml_edit 0.22.27` are API-incompatible (build error in `toml`); pin `toml_edit 0.22.22`. Prefer parsing TOML via `figment` to avoid a direct `toml` dep. | 07 |
| 2026-06-02 | 08 | `crossterm` (ratatui default backend / `tui-textarea`) crashes the test binary at init on `x86_64-pc-windows-gnu` (`0xc0000005`). Set `ratatui = { default-features = false }` at the workspace level; keep crossterm/`tui-textarea` in the CLI terminal driver. The TUI core then snapshot-tests via `TestBackend`. | 08 / D015 |
| 2026-06-02 | 08 | ratatui transitives need rustc ≥1.85/1.88: pin `unicode-segmentation 1.12.0`, `instability 0.3.7`, `darling 0.20.10`. It also pulls Zlib + unmaintained `paste` (RUSTSEC-2024-0436): allow Zlib + ignore the advisory in `deny.toml`. | 08 |
