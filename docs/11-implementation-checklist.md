# Implementation Checklist

Status as of 2026-06-05. Checked items mean the implementation exists and has
local automated coverage unless the note says it still needs live validation.

## Implemented And Covered

### Foundation

- [x] Repository URLs are real.
- [x] GitHub Actions run the Rust gate on Windows, Linux, and macOS.
- [x] CI includes blocking supply-chain hygiene: `cargo deny check`,
      `cargo audit`, and `cargo machete`.
- [x] `deny.toml` defines license, source, advisory, duplicate, and wildcard
      dependency policy.

### Core Runtime

- [x] Typed session and tool-call IDs.
- [x] Message content model with text, reasoning, tool-use, and tool-result
      blocks.
- [x] Usage accounting model.
- [x] Provider-neutral stream event model, including reasoning deltas and usage.
- [x] Typed error hierarchies at crate boundaries.
- [x] Transcript persistence with redaction before write.
- [x] Context compaction that preserves tool-call/tool-result pairing.

### Configuration

- [x] Config loading from defaults, user config, project config, environment,
      and CLI overrides.
- [x] User and project config path resolution.
- [x] Config diagnostics through `doctor`.
- [x] Permission profile config for `default`, `relaxed`, and `bypass`.
- [x] Quota wait/resume policy config.

### Providers

- [x] Provider trait and registry.
- [x] Local/OpenAI-compatible provider.
- [x] Official hosted provider adapters currently in tree.
- [x] Fake provider for deterministic tests.
- [x] Stream parser tests for text, tool calls, malformed streams, and quota
      metadata.
- [x] Quota and rate-limit classification with reset-window metadata.
- [x] Provider capability declarations.
- [x] Reasoning/thinking event translation and request continuity tests.

### Tools And Permissions

- [x] Tool registry with schema generation from typed inputs.
- [x] Workspace path policy.
- [x] File read, write, edit, multi-edit, list, find, and search tools.
- [x] Shell command tool with argument-list execution and timeout.
- [x] Git status, diff, log, add, restore, and commit tools.
- [x] Approval interface with scripted approval test support.
- [x] Command classification for POSIX, PowerShell, `cmd.exe`, and direct
      executables.
- [x] Non-interactive denial policy for risky commands.
- [x] Destructive-command and approval-path regression tests.

### Harness

- [x] Brief parser and renderer.
- [x] Progress parser and renderer.
- [x] Status command.
- [x] Intake and planner flows with fake-provider tests.
- [x] Rule engine with trigger/verdict coverage.
- [x] Resume loop with bounded retry and replan behavior.
- [x] Quality gate discovery, ratification, execution, and auto-fix handling.
- [x] Legacy `harness.test_command` runs through the mediated quality-check
      path.
- [x] Resume session-start preflight blocks unrelated dirty work before provider
      work.
- [x] Harness step commits stage only intended project paths, excluding local
      `.localpilot/` runtime state.
- [x] Worker-loop trace events.
- [x] Quota wait/resume records and safety gates.
- [x] Mid-stream quota/rate-limit errors pause cleanly instead of entering
      bad-output recovery.

### TUI

- [x] Ratatui/crossterm stack selected by ADR.
- [x] Message list, input box, streaming render, approval modal, status line,
      footer stats, and optional thinking panel.
- [x] Narrow-terminal layout behavior and render snapshots.
- [x] Slash command surface used by the interactive runtime.

### Recovery

- [x] Empty/incomplete stream handling.
- [x] Repeated-token loop and slash-flood detection.
- [x] False-positive guards for fenced/code-like content.
- [x] Malformed tool-call handling.
- [x] Recovery retry ladder and hard repair budget.
- [x] Recovery diagnostics persistence.

### Skills, Memory, MCP, Store, And Evals

- [x] Skill manifest, loading, validation, suggestions, and generated drafts.
- [x] Local memory integration with inspect/delete/opt-out/redaction surfaces.
- [x] MCP client, tool discovery, and permission/redaction-gated MCP tool calls.
- [x] MCP registry rebuilds use owned dynamic descriptors rather than leaked
      static strings.
- [x] Store transcript format, atomic writes, session index, export, cache,
      provider metadata, tool-output snapshots, and quota pause records.
- [x] Fake-provider eval runner and golden-task smoke coverage.

## Implemented But Needs Hardening

- [ ] Workspace trust prompts need more end-to-end UI coverage across CLI and
      TUI surfaces.
- [ ] Memory relevance thresholds and token-budget behavior need broader
      real-project calibration.
- [ ] MCP resource reads and server health display need live server validation.
- [ ] Provider adapters need periodic review against current official API docs
      before public release.
- [ ] `cargo audit` currently allows informational warnings for transitive
      `paste`, `rustls-pemfile`, and `lru`; revisit when upstream dependency
      updates are compatible.
- [ ] `time 0.3.37` is temporarily ignored for `RUSTSEC-2026-0009` because the
      fixed `time 0.3.47` crate requires edition 2024 metadata that Cargo 1.82
      cannot parse. Remove the ignore when the workspace MSRV is raised enough
      to adopt `time >=0.3.47`.

## Not Implemented

- [ ] Changelog.
- [ ] Contributor guide.
- [ ] Install docs.
- [ ] Alpha release checklist.
- [ ] Public release tag `v0.1.0-alpha.1`.

## Release Gate

Before an alpha tag, run and record:

```powershell
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check --workspace
cargo clippy -p localpilot --features tui --all-targets -- -D warnings
cargo build -p localpilot --features tui
cargo deny check
cargo audit
cargo machete
cargo run -p localpilot -- doctor
```

Also complete the clean-room audit from `docs/00-clean-room.md`, review all
remaining advisory warnings, and confirm no transcripts, API keys, tokens, or
private data are tracked.
