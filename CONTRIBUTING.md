# Contributing

## Ground Rules

LocalPilot is original Rust software. Contributions must follow
[docs/00-clean-room.md](docs/00-clean-room.md).

Do not submit code, prompts, tests, endpoint adapters, docs, or UI copy copied
from proprietary or leaked projects.

## Development Setup

```powershell
cargo check --workspace
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

The CI quartet above is also available as `cargo ci-fmt`, `cargo ci-lint`,
`cargo ci-test`, and `cargo ci-check` (see `.cargo/config.toml`); run all four to
mirror `.github/workflows/ci.yml`.

### Optional pre-commit hook

A `pre-commit` hook that runs `cargo fmt --check` and a fast `cargo clippy` lives
in `.githooks/`. It is opt-in so contributors without a local toolchain are not
blocked. Enable it once per clone:

```sh
git config core.hooksPath .githooks
```

## Pull Request Requirements

Each PR should include:

- what changed
- why it changed
- tests added or updated
- docs updated for any behaviour, architecture, command, usage, configuration,
  setup, troubleshooting, or developer-workflow change (`CHANGELOG.md`,
  `README.md`, `docs/`, `docs/wiki/`), or a note that none apply and why
- provenance note for API behavior or protocol details

Example provenance note:

```text
Provider request shape implemented from public API docs at <url>.
No private endpoint behavior used.
```

## Coding Style

- Keep crate boundaries narrow.
- Prefer typed data over stringly contracts.
- Put provider-specific code only in provider modules.
- Put local side effects only in tools.
- Keep prompts in harness modules and test them as product behavior.
- Use `tracing` for diagnostics.
- Redact secrets before persistence or logging.

## Review Checklist

- [ ] Code is original.
- [ ] Docs updated for the change (or n/a noted) — `CHANGELOG.md`/`README.md`/`docs/`/`docs/wiki/` per the `docs/README.md` ownership map.
- [ ] Public docs are cited where protocol behavior matters.
- [ ] Tests cover failure paths.
- [ ] No private endpoints.
- [ ] No vendor branding as product identity.
- [ ] No secrets in fixtures.
- [ ] No broad unrelated refactors.

## Contributor Certificate of Origin

By submitting a contribution (for example, a pull request) to this
repository, you certify that:

1. the contribution was created in whole or in part by you and you have the
   right to submit it under the terms below; or
2. the contribution is based on prior work that, to your knowledge, is
   covered under an appropriate license, and you have the right to submit
   that work under this license; or
3. the contribution was provided to you by someone who certified (1) or (2)
   and you have not modified it.

You keep copyright in your contribution. You grant David Ben-Yishai and Bram
Hammer a perpetual, worldwide, non-exclusive, royalty-free, irrevocable
license to use, reproduce, modify, prepare derivative works of, publicly
display, publicly perform, sublicense, and distribute your contribution as
part of this project, including under license terms other than this
repository's PolyForm Noncommercial License — for example, a commercial
license. Sign off every commit with `git commit -s`. The resulting
`Signed-off-by` trailer certifies that you agree to this Contributor
Certificate of Origin and the license grant above; this project does not use
a separate signed CLA process beyond that trailer.

