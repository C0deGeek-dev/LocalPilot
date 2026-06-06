# Rust Best Practices

This is the engineering style guide for LocalPilot. It is opinionated and
project-specific. It assumes the architecture in
[02-architecture.md](02-architecture.md) and the rules in
[../CONTRIBUTING.md](../CONTRIBUTING.md) and does not repeat them.

When a rule here conflicts with an accepted ADR in
[10-decisions.md](10-decisions.md), the ADR wins. Propose a new ADR to change a
rule that has architectural weight.

Conventions: **MUST** / **MUST NOT** are blocking in review. **SHOULD** is the
default; deviating requires a one-line justification in the PR or an
`#[allow(...)]` with a reason. **MAY** is discretionary.

---

## 1. Toolchain and MSRV

- MSRV is **1.82** (`rust-toolchain.toml`, `workspace.package.rust-version`).
  CI builds on this exact channel. Do not use APIs or syntax newer than 1.82.
- Edition is **2021** for every crate. Inherit it from the workspace.
- All external dependencies are **exact-pinned** (`=x.y.z`) in the workspace
  `[workspace.dependencies]` table. Member crates reference them with
  `dep = { workspace = true }`. Do not pin a second copy in a member crate.
- Raising the MSRV or unpinning a dependency is a deliberate change with its own
  commit and a note in `CHANGELOG.md`. It is never a side effect of another PR.
- Before bumping a dependency, check it still satisfies `deny.toml` (license
  allowlist) and `cargo audit`.

## 2. Workspace and crate hygiene

- Crates stay narrow and single-purpose (ADR-0001). If a type needs HTTP, it
  does not belong in `localpilot-core`.
- **Dependency direction is one-way.** `core` depends on nothing internal.
  Everything may depend on `core`. The CLI sits at the top and may depend on
  many crates; nothing depends on the CLI. There MUST be no dependency cycles —
  `cargo` will reject them, but design so the question never comes up.
- Provider-specific code lives only in `localpilot-llm` provider modules.
  Local side effects live only in `localpilot-tools`. Permission decisions live
  only in `localpilot-sandbox`. Do not leak these concerns across boundaries.
- Keep crate public APIs small. Default to private; export only what another
  crate needs. A `pub` item is a maintenance contract.
- Enable feature unification awareness: a feature turned on for a dependency in
  one crate is on everywhere. Keep `tokio` and `reqwest` feature sets minimal
  and declared once in the workspace table.

## 3. Type-driven design

Prefer making illegal states unrepresentable over validating them at runtime.

- Use **newtypes** for identifiers and units instead of bare `String`/`u64`.
  A `SessionId(Uuid)` cannot be confused with a `MessageId(Uuid)`.
- Model alternatives as `enum`, not as a struct with optional fields and a
  "kind" tag. Match exhaustively; let the compiler find unhandled variants.
- Parse, don't validate: convert untrusted input into a typed value once at the
  boundary, then pass the typed value inward. Inner code never re-checks.
- Mark public enums and error types that may grow with `#[non_exhaustive]` so
  adding a variant is not a breaking change for downstream crates.
- Derive `Debug` widely, but see §10 — types holding secrets MUST NOT derive a
  `Debug`/`Display` that prints the secret.
- **Generate tool/provider JSON schemas from typed structs, not by hand.** When
  tools are implemented, define a typed input struct per tool and derive its
  JSON Schema (e.g. `schemars`) instead of maintaining hand-written JSON. The
  schema and the deserialized type then cannot drift, and `localpilot-tools`
  owns one generation path.

## 4. Error handling

- Library crates (everything except the CLI binary) define typed errors with
  **`thiserror`**. One error enum per crate boundary is the target:
  `ConfigError`, `ProviderError`, `ToolError`, `PermissionError`,
  `HarnessError`, `StoreError` (per architecture §Error Handling).
- The **CLI binary MAY use `anyhow`** at the top level to aggregate and render.
  Library code MUST NOT return `anyhow::Error` across a public boundary —
  callers need to match on cause.
- Every fallible public function documents a `# Errors` section.
- **No `unwrap()` / `expect()` in library code** on anything that can fail at
  runtime. Allowed only for invariants that are provably impossible (with a
  message stating the invariant) and in tests. `panic!` is for unrecoverable
  programmer error, never for expected failure (bad input, network, quota).
- Add context as errors cross layers. With `thiserror`, model the cause as a
  `#[from]` source variant; do not stringify and lose the chain.
- Map errors to user-facing output only in the CLI: short message, optional
  detail behind `--verbose`, stable non-zero exit code.

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ToolError {
    #[error("path {0} is outside the workspace")]
    OutsideWorkspace(PathBuf),
    #[error("io error")]
    Io(#[from] std::io::Error),
}
```

## 5. Async and Tokio

- The runtime is multi-threaded `tokio`. Treat the executor as shared: **never
  block it.** No `std::thread::sleep`, no blocking file IO outside `tokio::fs`,
  no CPU-bound loops on an async task. Push blocking/CPU work to
  `tokio::task::spawn_blocking` or a dedicated thread.
- **Do not hold a lock across `.await`.** Acquire, copy/clone what you need,
  drop the guard, then await. Holding a `std::sync::Mutex` guard across an await
  point will not compile cleanly for `Send` futures and is a bug even when it
  does.
- Use `std::sync::Mutex`/`RwLock` for short, non-async critical sections.
  Reach for `tokio::sync::Mutex` only when the guard must be held across an
  `.await`. The std mutex is faster; prefer it.
- Cancellation is first-class for this product (recovery engine, quota pauses,
  user interrupt). Make long operations cancellable: thread a
  `tokio_util::sync::CancellationToken` or use `tokio::select!` against a
  shutdown signal. Assume any `.await` may be cancelled and leave state
  consistent if it is (no half-written files — write to temp, then rename).
- Use the right channel: `oneshot` for a single reply, `mpsc` for a work queue,
  `watch` for "latest value" state (quota window, status), `broadcast` for
  fan-out events (stream/UI updates). Do not simulate these with shared locks.
- `async fn` in traits: the provider and tool traits use **`async-trait`** for
  object safety (`Box<dyn Provider>`). Accept the boxing cost — it is on the
  network/tool path, not a hot loop. Use native `async fn` in traits only for
  non-object-safe internal traits.
- Streaming responses are core (provider events). Return `impl Stream<Item = …>`
  or a boxed stream; consume with `StreamExt`. Propagate cancellation through
  the stream, and surface a malformed stream as a typed `ProviderError`, never
  a panic.
- Spawned tasks MUST have their `JoinHandle` awaited or deliberately detached
  with a comment. A silently dropped handle hides panics.

## 6. Traits and abstraction

- Design the provider trait and tool trait to be **object-safe** so registries
  can hold `Vec<Box<dyn Tool>>` / `Box<dyn Provider>`. Keep generic methods off
  the trait; put them in extension traits or free functions.
- Prefer `&dyn Trait` / `Box<dyn Trait>` at runtime-plugin boundaries (tools,
  providers, MCP servers) where the set is open. Prefer generics (`impl Trait`,
  `T: Trait`) for internal, closed, performance-sensitive code.
- Consider **sealing** traits that are not meant to be implemented outside the
  crate (private supertrait) so the public trait list stays controlled.
- Do not over-abstract early. A trait with one implementer and no test fake is
  usually premature; introduce it when the second implementer or the test fake
  actually appears. (The provider/tool/store seams are known multi-impl points
  and are exempt — those traits exist by design.)

## 7. Cross-platform (Windows, Linux, macOS are tier-1, ADR-0007)

Parity is a release requirement. Write platform-correct code from the start, not
a POSIX version plus Windows patches.

- **Paths**: use `Path`/`PathBuf` and `.join()`. Never concatenate path strings
  or hardcode `/` or `\`. Do not assume paths are UTF-8 — use `Path`, and only
  go through `to_string_lossy()` for display, never for logic.
- **Workspace containment** (sandbox): canonicalize and compare with
  `Path::starts_with` on normalized paths. Beware Windows `\\?\` verbatim
  prefixes from `canonicalize`, case-insensitive and 8.3 short names, and ADS
  (`file.txt:stream`). A naive string `starts_with` is a security bug here.
- **Shell**: the `run_shell` tool and command classification differ per OS —
  `cmd.exe`/PowerShell vs `sh`. Quoting, env var syntax (`%VAR%` vs `$VAR`), and
  the list of "destructive" commands are platform-specific. Do not hardcode
  `/bin/sh`.
- **Line endings**: write `\n` internally; `rustfmt` `newline_style = "Auto"`
  handles source. For parsing `brief.md` / `PROGRESS.md`, accept both `\n` and
  `\r\n`. For git-tracked generated files, do not depend on the platform default.
- **Process/exec**: argument lists, not shell strings, when invoking tools.
  Handle that executables may need `.exe` and `PATH` lookup differs.
- Gate genuinely platform-specific code with `#[cfg(windows)]` /
  `#[cfg(unix)]` and keep a shared trait/function signature so both paths are
  tested. CI runs all three OSes — a `#[cfg]` branch with no test is a gap.

## 8. Security and redaction

See [07-security-and-privacy.md](07-security-and-privacy.md) for policy; this is
the implementation discipline.

- Secrets (API keys, tokens) get a wrapper type whose `Debug`/`Display` prints
  `***` (or a redacted prefix), never the value. The raw value is reachable only
  through an explicit `expose()`-style method. Do not put a secret in a plain
  `String` field of a `#[derive(Debug)]` struct.
- **Redact before persistence and before logging**, not after. The store and the
  `tracing` layer both apply redaction; new fields that may carry secrets opt in.
- Treat all tool input as untrusted: validate paths against the workspace policy
  (§7), reject path traversal, and never build a shell command by string
  interpolation of model output.
- `deny.toml` gates licenses today; keep `[bans]` and `[sources]` in mind as the
  dependency tree grows. Run `cargo audit` / `cargo deny check` before release.
- No telemetry by default. Network egress happens only through the provider
  runtime and explicitly user-configured endpoints (ADR-0004).

## 9. Lints and formatting

- `cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D warnings`
  are CI gates. **Warnings are errors.** Fix the cause; do not blanket-allow.
- Prefer a workspace `[workspace.lints]` table over scattering attributes, once
  lint policy stabilizes. Until then, any `#[allow(...)]` MUST carry a trailing
  comment explaining why.
- Recommended baseline beyond default clippy: deny
  `clippy::unwrap_used` and `clippy::expect_used` in library crates (allow in
  `#[cfg(test)]`), and `clippy::todo` / `clippy::dbg_macro` everywhere. Consider
  `clippy::pedantic` as warn-level and silence the noisy ones deliberately.
- Each library crate SHOULD start with `#![forbid(unsafe_code)]` (see §12).

## 10. Testing

Aligns with [08-testing.md](08-testing.md); this is the how.

- Unit tests live in a `#[cfg(test)] mod tests` next to the code. Integration
  tests live in the crate's `tests/` dir and exercise only the public API.
- **Prefer hand-written fakes over mocking frameworks.** A `FakeProvider` that
  returns scripted stream events is clearer and matches the fixture policy
  (fixtures authored for this repo, never copied).
- Tests MUST be deterministic and offline by default. Use `tempfile` for the
  filesystem; never touch the user's real config or home dir. Live provider
  tests are opt-in behind `LOCALPILOT_LIVE_TESTS` and skip without credentials.
- Use **snapshot tests** (`insta`) for CLI help, error rendering, TUI output,
  and generated prompts — anything where the assertion is "the output text".
  Review snapshot diffs deliberately; do not blind-accept.
- Test failure paths, not just happy paths (CONTRIBUTING review checklist):
  denied permissions, malformed streams, ambiguous edits, quota pauses.
- Keep tests fast; `cargo nextest` is the recommended runner locally and in CI.

Test toolbox (add as dev-dependencies when the relevant layer lands; see
[14-dev-tooling.md](14-dev-tooling.md) §3 for the cargo-subcommand tools):

- **`assert_cmd` + `predicates`** — drive the built CLI binary and assert on exit
  code / stdout / stderr. **`trycmd`** or **`snapbox`** for snapshot-style CLI
  session tests (help text, error output).
- **`proptest`** — property tests for the high-risk parsers and classifiers:
  path normalization/containment (§7), config precedence, command risk
  classification, and brief/progress parsing. Generate adversarial input rather
  than enumerating cases by hand.
- **`wiremock`** (or a local mock server) — test provider HTTP adapters against a
  scripted server for status codes, malformed bodies, and quota headers. Keeps
  adapter tests offline; live tests stay opt-in behind `LOCALPILOT_LIVE_TESTS`.
- **`tokio-test`** — for time/IO control in async unit tests. Reach for **`loom`**
  only on genuinely tricky shared-state synchronization; it is slow and scoped to
  the specific concurrent type under test, not general use.

## 11. Logging and observability

- Use `tracing`, not `println!`/`eprintln!`, for diagnostics. The CLI's
  user-facing output is separate from instrumentation and goes through the
  command-output layer.
- Instrument the meaningful spans: a chat turn, a tool call, a harness step, a
  provider request. Use `#[tracing::instrument(skip(...))]` and **skip fields
  that carry secrets or large payloads**.
- Respect the level meanings from architecture §Observability (`error`/`warn`/
  `info`/`debug`/`trace`). `debug` may carry payload *metadata*, never raw
  secrets; `trace` is local-only.

## 12. Unsafe, panics, and footguns

- `unsafe` is **forbidden** by default. A crate that needs it requires an ADR and
  a `// SAFETY:` comment on every block justifying the invariants. Default each
  crate to `#![forbid(unsafe_code)]` and only downgrade to `#![deny(unsafe_code,
  reason = "...")]` with the ADR.
- No `unwrap`/`expect`/`panic!`/`todo!`/`unimplemented!`/`unreachable!` on
  runtime paths. `unreachable!` is acceptable only with a proof comment.
- Avoid `as` casts that can truncate (`u64 as u32`); use `TryFrom` and handle
  the error. Avoid integer overflow assumptions — use checked/saturating ops on
  untrusted arithmetic.
- Avoid `clone()` to silence the borrow checker in hot paths. Borrow, restructure,
  or use `Arc` deliberately. Premature `clone` is a smell, not a crime — flag it
  in review, do not block on micro-allocs off the hot path.

## 13. Documentation

- Every crate has a `//!` module doc stating its responsibility and what it must
  not own (mirror architecture §Crate Responsibilities).
- Public items have `///` docs. Fallible ones have `# Errors`; panicking ones (if
  any) have `# Panics`. Non-obvious examples are doc-tested.
- Keep prompts and protocol decisions documented and cited to public API docs
  (provenance note), per CONTRIBUTING.

## 14. PR checklist (Rust-specific)

Before requesting review, locally green:

```powershell
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace        # or: cargo nextest run --workspace
cargo check --workspace
```

And confirm:

- [ ] No new `unwrap`/`expect`/`panic!` on a runtime path.
- [ ] Errors typed at the crate boundary; no `anyhow` leaking from a library.
- [ ] No lock held across `.await`; no blocking call on the async runtime.
- [ ] No secret reachable via `Debug`/logs; redaction applied before persist.
- [ ] Paths use `Path`/`PathBuf`; platform-specific code is `#[cfg]`-gated and
      tested on the relevant OS.
- [ ] New deps pinned in the workspace table and pass `deny.toml`.
- [ ] Tests cover at least one failure path; snapshots reviewed.
