# Changelog

## Unreleased

- Created clean-room Rust workspace scaffold.
- Added product, architecture, harness, provider, security, testing, and release docs.
- Specified two operating modes: default agent mode and enforced harness mode.
- Specified configurable permission profiles, including a bypass launch mode.
- Moved MCP, memory, skills, recovery, and quota wait/resume into committed v1 scope.
- Added a root `CLAUDE.md` and the repository's project skills.
- Added the `unshackled-memory`, `unshackled-skills`, `unshackled-recovery`, and
  `unshackled-quota` crates, bringing the workspace to its full crate roster.
- Centralized the lint policy in `[workspace.lints]`: `unsafe_code` forbidden and
  `unwrap`/`expect`/`todo`/`dbg!` denied on library runtime paths (relaxed in
  tests). The `clippy::pedantic` group is intentionally deferred to a later,
  deliberate adoption.
- Updated pinned dependencies for security: `tokio` 1.42.0 → 1.44.2
  (RUSTSEC-2025-0023) and `tracing-subscriber` 0.3.19 → 0.3.20
  (RUSTSEC-2025-0055). No MSRV change.
- Added `.editorconfig`, `.gitattributes` (LF normalization), `.cargo` CI-quartet
  aliases, and an opt-in `.githooks/pre-commit` gate.
- Expanded CI: tests run under `cargo nextest`, plus a non-blocking supply-chain
  job (`cargo deny check`, `cargo audit`) to be promoted to blocking before
  release.
- Replaced the stub `doctor` command with real diagnostics: version, platform,
  config search paths, provider credential presence (never values), tool
  availability, and workspace trust state.
- Added the provider runtime: an object-safe provider trait with typed
  capabilities, a stable error taxonomy, and quota metadata, behind one internal
  streaming contract. One OpenAI-compatible adapter serves both local
  OpenAI-compatible servers and the official OpenAI API, with streaming, tool
  calls, reasoning round-trip, retry/backoff, and a config-driven registry.
- Added the `unshackled ask` command for a single streamed text completion.
- Added the sandbox: a workspace path boundary, per-OS command risk
  classification, and a permission engine with `default`/`relaxed`/`bypass`
  profiles, a secret-file guard, and a workspace-trust floor.
- Added the tool system: a permission-gated registry and the eight builtin tools
  (`read_file`, `write_file`, `edit_file`, `list_files`, `search_text`,
  `run_shell`, `git_status`, `git_commit`) with generated JSON schemas, atomic
  writes, and output redaction on every profile.
- Added the shared agent-mode session runtime: a cancellable streaming loop with
  tool execution, transcript persistence, context compaction, and loop limits,
  plus context-aware bad-output detection and a budgeted recovery ladder.
- Added the `unshackled print` command for a non-interactive, single-prompt
  agent run that makes no workspace writes by default.

