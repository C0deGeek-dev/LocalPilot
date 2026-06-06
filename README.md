```
██╗   ██╗███╗   ██╗███████╗██╗  ██╗ █████╗  ██████╗██╗  ██╗██╗     ███████╗██████╗ 
██║   ██║████╗  ██║██╔════╝██║  ██║██╔══██╗██╔════╝██║ ██╔╝██║     ██╔════╝██╔══██╗
██║   ██║██╔██╗ ██║███████╗███████║███████║██║     █████╔╝ ██║     █████╗  ██║  ██║
██║   ██║██║╚██╗██║╚════██║██╔══██║██╔══██║██║     ██╔═██╗ ██║     ██╔══╝  ██║  ██║
╚██████╔╝██║ ╚████║███████║██║  ██║██║  ██║╚██████╗██║  ██╗███████╗███████╗██████╔╝
 ╚═════╝ ╚═╝  ╚═══╝╚══════╝╚═╝  ╚═╝╚═╝  ╚═╝ ╚═════╝╚═╝  ╚═╝╚══════╝╚══════╝╚═════╝ 
```
# LocalPilot

[![install](https://img.shields.io/badge/install-one--liner-555?style=flat-square)](#getting-started)
[![stars](https://img.shields.io/github/stars/David-c0degeek/LocalPilot?style=flat-square&label=stars&color=007ec6)](https://github.com/David-c0degeek/LocalPilot/stargazers)
[![issues](https://img.shields.io/github/issues/David-c0degeek/LocalPilot?style=flat-square&label=issues&color=4c1)](https://github.com/David-c0degeek/LocalPilot/issues)
[![agent loop](https://img.shields.io/badge/agent%20loop-alpha-orange?style=flat-square)](#commands)
[![harness](https://img.shields.io/badge/harness-mode-555?style=flat-square)](#commands)
[![rules](https://img.shields.io/badge/rules-9%20gates-4c1?style=flat-square)](docs/06-harness-spec.md)

LocalPilot is a Rust-native, provider-neutral coding-agent harness.

Maintained by C0deGeek.dev (David, Bram).
Repository: <https://github.com/David-c0degeek/LocalPilot>
Runs on Windows, Linux, and macOS — all first-class, tier-1 platforms.

## LocalX Ecosystem

- [LocalStack](https://github.com/David-c0degeek/LocalStack) is the umbrella
  ecosystem for the LocalX tools.
- [LocalBox](https://github.com/David-c0degeek/LocalBox) is the model runtime
  and launcher for local GGUF models.
- [LocalMind](https://github.com/David-c0degeek/LocalMind) is the local-first
  memory and RAG layer embedded by LocalPilot.
- [LocalBench](https://github.com/David-c0degeek/LocalBench) is the benchmarking
  and evaluation companion for local model/runtime choices.
- [LocalPilot](https://github.com/David-c0degeek/LocalPilot) is this local CLI
  coding agent.

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
git clone --recurse-submodules https://github.com/David-c0degeek/LocalPilot.git
# or, in an existing clone:
git submodule update --init --recursive
```

Build and check the environment:

```sh
cargo build -p localpilot
cargo run -p localpilot -- doctor
```

Point it at a provider in `.localpilot.toml` (official API or a local
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
localpilot ask --model your-local-model "explain this repo's error handling"
localpilot chat                 # interactive REPL (release builds)
localpilot                      # no args: launches the REPL, or doctor if unset
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
| `init` | Initialize project-local state (`.localpilot.toml`, `.gitignore`) |
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
cargo build -p localpilot --features tui
```

## Repository layout

```text
crates/
  localpilot-cli/        CLI entrypoint and command routing
  localpilot-core/       Provider-neutral domain types
  localpilot-config/     Config schema and loading
  localpilot-llm/        Provider API abstraction (OpenAI-compatible adapter)
  localpilot-tools/      Tool registry and permission-gated execution
  localpilot-harness/    Session runtime, intake/planning, rule engine, recovery
  localpilot-tui/        Terminal UI (ratatui), backend-agnostic core
  localpilot-store/      Redacted session persistence and export
  localpilot-sandbox/    Permission engine and execution policy
  localpilot-mcp/        Model Context Protocol client and stdio transport
  localpilot-skills/     Skill manifests and drafts (alpha bridge surface)
  localpilot-recovery/   Bad-output detection and recovery ladder
  localpilot-quota/      Quota window tracking and wait/resume policy
  localpilot-localmind/  Adapter to the bundled LocalMind learning engine
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
cargo build -p localpilot --features tui
cargo clippy -p localpilot --features tui --all-targets -- -D warnings
cargo machete
cargo deny check
cargo audit
```
