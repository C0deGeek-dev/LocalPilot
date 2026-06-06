# Changelog

Notable changes per release. This project is pre-1.0; the configuration schema
stability policy is in [docs/configuration.md](docs/configuration.md).

## Unreleased

- Fixed interactive input editing: the caret is visible, and Left/Right,
  Home/End, Backspace, Delete, newlines, and pastes edit at the cursor. Provider
  streams that disconnect before a completion marker now recover instead of
  persisting a visibly truncated response as complete.
- Made the session context budget configurable with `[harness]
  context_token_limit` (default 24000) so a model's full context window is used
  for compaction instead of a fixed default.
- Reworked the REPL input box: it grows with multi-line content up to a cap and
  then scrolls; newlines now work across terminals (a trailing `\` before Enter,
  plus Ctrl+J / Shift+Enter where the terminal reports enhanced keys); large
  pastes collapse to a `[pasted #n · N lines]` placeholder and expand to full
  text on submit.
- Added a first-run trust gate: the REPL shows the workspace folder and asks
  whether to trust it before acting, remembering the answer per folder (skipped
  under `--bypass`).
- Added the Anthropic Messages API provider (`kind = "anthropic"`), a second,
  protocol-distinct adapter implemented clean-room from the public API:
  top-level `system`, `tool_use`/`tool_result` blocks, required `max_tokens`,
  `x-api-key` + `anthropic-version`, and a typed SSE stream (ADR-0008).
- Added `localpilot update [--check]`: checks the repository for a newer release
  tag and, on confirmation, reinstalls from source with the same feature set
  (MSVC toolchain on Windows for the TUI). The REPL and bare launch also do a
  cached, once-a-day check; disable with `LOCALPILOT_NO_UPDATE_CHECK`. The
  binary now embeds a real version via `build.rs`.
- Fixed the installers to build `--features tui,learning`, initialize the
  LocalMind submodule, and prefer the MSVC toolchain on Windows for the TUI.
- Documented the configuration reference and stability policy
  (`docs/configuration.md`) and consolidated the extension points into
  `docs/extending.md`.

## 0.1.0-alpha.6

- Fixed the interactive REPL: drain buffered events so a fast response is shown
  (not dropped) and surface provider/stream errors instead of failing silently;
  handle only key *press* events (Windows no longer doubles typed characters);
  add a working spinner + elapsed timer; support bracketed paste and Alt+Enter
  for a newline.
- Added a task checklist panel driven by an `update_plan` tool.
- Retry transient provider connection failures (network/5xx) with exponential
  backoff and a notice; rate-limit/quota errors still pause.

## 0.1.0-alpha.5

- Integrated the LocalMind learning engine (vendored as a git submodule) behind
  the opt-in `learning` feature: session closeout, the review queue, memory
  promotion and search, skill drafts, an audit log, retrieved-context injection
  before turns, and automatic closeout on REPL exit — one-way edge, bundled into
  the binary, all state local under `.localmind/`. New `localpilot learning`
  commands.

## 0.1.0-alpha.4

- Added interactive tool-approval prompts in the REPL (the approval interface is
  now asynchronous); default-profile sessions can perform approved actions
  without `--bypass`.
- Connected MCP servers and exposed their tools to the session through the same
  permission engine and redaction.
- Sized quota pauses from provider rate-limit metadata; show live tokens/sec and
  a quota reset timer in the footer.

## 0.1.0-alpha.3

- Added `localpilot harness wait-resume` to continue a run paused on a provider
  quota/rate limit once it is safe.

## 0.1.0-alpha.2

- Made the `chat` REPL launchable and bundled the `tui` feature into release
  builds; the bare `localpilot` command launches the REPL when a provider and
  model are configured.

## 0.1.0-alpha.1

- Created the clean-room Rust workspace and the product/architecture/harness/
  provider/security/testing/release specifications, with two operating modes
  (agent and enforced harness) and configurable permission profiles.
- Added the full crate roster (`localpilot-memory`, `-skills`, `-recovery`,
  `-quota`, and the rest) and centralized the lint policy in `[workspace.lints]`
  (`unsafe_code` forbidden; `unwrap`/`expect`/`todo`/`dbg!` denied on library
  runtime paths, relaxed in tests).
- Added real `doctor` diagnostics: version, platform, config search paths,
  provider credential presence (never values), tool availability, trust state.
- Added the provider runtime: an object-safe provider trait with typed
  capabilities, a stable error taxonomy, and quota metadata behind one streaming
  contract. The OpenAI-compatible adapter serves local servers and the official
  OpenAI API, with streaming, tool calls, reasoning round-trip, and a
  config-driven registry. Added `localpilot ask`.
- Added the sandbox: a workspace path boundary, per-OS command risk
  classification, and a permission engine with `default`/`relaxed`/`bypass`
  profiles, a secret-file guard, and a workspace-trust floor.
- Added the tool system: a permission-gated registry and the builtin tools
  (`read_file`, `write_file`, `edit_file`, `list_files`, `search_text`,
  `run_shell`, `git_status`, `git_commit`) with generated schemas, atomic writes,
  and output redaction on every profile.
- Added the shared agent-mode session runtime (cancellable streaming loop, tool
  execution, transcript persistence, context compaction, loop limits) with
  bad-output detection and a budgeted recovery ladder, plus `localpilot print`
  and the `chat` REPL behind the opt-in `tui` feature.
- Added the harness core: lossless `brief.md` / `PROGRESS.md` documents; the
  `init`, `harness status`, `intake`, `plan`, `feature`, and `resume` commands;
  original intake/planner prompts; a deterministic rule engine with protected
  critical rules; and an anti-sunk-cost worker that commits one step at a time.
- Added the v1 extensions: quota wait/resume with safety gates, a local redacted
  memory store with ranked retrieval and `memory` commands, the skill
  manifest/loading/suggestion system, and an MCP client.
- Added the terminal UI: a dense ratatui view (header, transcript with live
  streaming, always-visible footer, optional thinking panel, approval modal,
  slash commands, model/provider picker, transcript search, responsive collapse)
  snapshot-tested with a test backend.
- Updated pinned dependencies for security (`tokio` → 1.44.2,
  `tracing-subscriber` → 0.3.20); no MSRV change. Added editor/CI tooling and an
  opt-in pre-commit gate; CI runs tests under `cargo nextest` plus a
  supply-chain job (`cargo deny`, `cargo audit`).
