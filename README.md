```
██╗   ██╗███╗   ██╗███████╗██╗  ██╗ █████╗  ██████╗██╗  ██╗██╗     ███████╗██████╗ 
██║   ██║████╗  ██║██╔════╝██║  ██║██╔══██╗██╔════╝██║ ██╔╝██║     ██╔════╝██╔══██╗
██║   ██║██╔██╗ ██║███████╗███████║███████║██║     █████╔╝ ██║     █████╗  ██║  ██║
██║   ██║██║╚██╗██║╚════██║██╔══██║██╔══██║██║     ██╔═██╗ ██║     ██╔══╝  ██║  ██║
╚██████╔╝██║ ╚████║███████║██║  ██║██║  ██║╚██████╗██║  ██╗███████╗███████╗██████╔╝
 ╚═════╝ ╚═╝  ╚═══╝╚══════╝╚═╝  ╚═╝╚═╝  ╚═╝ ╚═════╝╚═╝  ╚═╝╚══════╝╚══════╝╚═════╝ 
```
# Unshackled

Unshackled is a Rust-native, provider-neutral coding-agent harness.

Maintained by C0deGeek.dev (David, Bram).
Repository: <https://github.com/David-c0degeek/Unshackled>
Runs on Windows, Linux, and macOS — all first-class, tier-1 platforms.

It is not a fork, clone, port, or redistribution of any vendor CLI. The project
is designed from first principles around a small set of public concepts:

- a terminal interface for agentic software development
- two operating modes: a default conversational agent mode and an opt-in,
  rule-enforced harness mode
- official model/provider APIs and local OpenAI-compatible servers
- a rule-enforced harness that turns vague tasks into inspectable plans
- local state stored in ordinary project files
- explicit permission boundaries for filesystem, shell, network, and external tools

## Project status

Pre-release alpha. The full agent loop, harness, tools, permissions, provider
adapter, TUI, MCP integration, and the LocalMind learning subsystem are
implemented and tested across Windows, Linux, and macOS in CI. The one gate
before a tagged public alpha is a live run against a real provider (the suite is
offline by default).

It contains no implementation copied from any closed-source or leaked codebase.

## Getting started

Clone with submodules (the LocalMind learning engine is vendored as one):

```sh
git clone --recurse-submodules https://github.com/David-c0degeek/Unshackled.git
# or, in an existing clone:
git submodule update --init --recursive
```

Build and check the environment:

```sh
cargo build -p unshackled
cargo run -p unshackled -- doctor
```

Point it at a provider in `.unshackled.toml` (official API or a local
OpenAI-compatible server such as llama.cpp / Ollama / vLLM):

```toml
[provider]
default = "local"

[providers.local]
kind = "openai-compatible"
base_url = "http://localhost:8080/v1"
model = "your-local-model"
# api_key_env = "OPENAI_API_KEY"   # for a hosted API
```

Then talk to it:

```sh
unshackled ask --model your-local-model "explain this repo's error handling"
unshackled chat                 # interactive REPL (release builds)
unshackled                      # no args: launches the REPL, or doctor if unset
```

See [`docs/providers.md`](docs/providers.md) for provider setup,
[`docs/configuration.md`](docs/configuration.md) for the full config reference
and stability policy, [`docs/mcp.md`](docs/mcp.md) for MCP tool servers, and
[`docs/extending.md`](docs/extending.md) for adding providers and tools.

## Commands

| Command | What it does |
| --- | --- |
| `doctor` | Report version, platform, config, providers, tools, trust state |
| `update [--check]` | Check the repo for a newer release and reinstall from source on confirm |
| `init` | Initialize project-local state (`.unshackled.toml`, `.gitignore`) |
| `ask` | Send one prompt and stream the answer (no tools) |
| `chat` | Interactive terminal REPL with tool approvals, a working indicator, and a task panel |
| `print` | Run the agent loop once non-interactively (pipelines) |
| `harness intake \| plan \| feature \| resume \| wait-resume` | Rule-enforced mode: idea → `brief.md` → `PROGRESS.md` → worked, committed steps; pause/resume on quota |
| `memory` | Inspect/search/manage local project memory |
| `learning` | LocalMind loop: `closeout`, `review`, `promote`, `search`, `skills`, `audit` |
| `export` | Export a redacted session bundle |

## Build features

The default binary links the LocalMind learning subsystem. The `tui` feature
adds the interactive `chat` REPL; `learning` remains accepted as a compatibility
alias for older build commands.

```sh
cargo build -p unshackled --features tui
```

## Repository layout

```text
crates/
  unshackled-cli/        CLI entrypoint and command routing
  unshackled-core/       Provider-neutral domain types
  unshackled-config/     Config schema and loading
  unshackled-llm/        Provider API abstraction (OpenAI-compatible adapter)
  unshackled-tools/      Tool registry and permission-gated execution
  unshackled-harness/    Session runtime, intake/planning, rule engine, recovery
  unshackled-tui/        Terminal UI (ratatui), backend-agnostic core
  unshackled-store/      Redacted session persistence and export
  unshackled-sandbox/    Permission engine and execution policy
  unshackled-mcp/        Model Context Protocol client and stdio transport
  unshackled-skills/     Skill manifests and drafts (alpha bridge surface)
  unshackled-recovery/   Bad-output detection and recovery ladder
  unshackled-quota/      Quota window tracking and wait/resume policy
  unshackled-localmind/  Adapter to the bundled LocalMind learning engine
external/
  localmind/             LocalMind learning engine (git submodule)
docs/                    Product and technical specifications
```

## Design principles

1. Original implementation only.
2. Official APIs (or local servers) only — no private/undocumented endpoints.
3. Provider-neutral core.
4. Local-first project state.
5. Explicit user control for risky actions; `bypass` is never the default.
6. Reproducible planning and progress.
7. No hidden consumer-product automation.
8. No vendor branding as product identity.

## Local gate (mirrors CI)

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check --workspace
cargo build -p unshackled --features tui
cargo clippy -p unshackled --features tui --all-targets -- -D warnings
cargo machete
cargo deny check
cargo audit
```
