# Getting started

LocalPilot is a Rust-native, provider-neutral coding-agent harness for the
terminal. It runs against official model APIs or local OpenAI-compatible servers,
with explicit permission boundaries for filesystem, shell, network, and external
tools. Windows, Linux, and macOS are all tier-1.

> **Do not edit on github.com.** This wiki is generated from in-repo Markdown
> under `docs/wiki/` and synced one-way on every push to `main`. Edit the source
> in `docs/wiki/`; web edits are overwritten on the next sync.

## Install

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/C0deGeek-dev/LocalPilot/main/install/install.ps1 | iex
```

Linux / macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/C0deGeek-dev/LocalPilot/main/install/install.sh | sh
```

No Rust toolchain is needed. On Windows the installer puts LocalX on `PATH`
and, when the installed release includes it, continues into the guided first
run; elsewhere, add the printed directory to `PATH` and run it yourself:

```sh
localx setup              # local model sized to this computer, LocalPilot pointed at it, chat
localx setup --existing   # keep a provider you already use
```

`localx setup` arrives in the first release after v6.0.0; until then it needs a
current source build.
Setup is safe to run again. After a restart, `localpilot` starts the model
setup made its default through LocalBox by itself.

### Build from source

Clone with submodules (the LocalMind learning engine is vendored as one), then
build:

```sh
git clone --recurse-submodules https://github.com/C0deGeek-dev/LocalPilot.git
cargo build -p localpilot
cargo run -p localpilot -- doctor
```

The `tui` feature adds the interactive `chat` REPL:

```sh
cargo build -p localpilot --features tui
```

Full install paths (release archive, crates.io) are in
[install.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/install.md).

## Point it at a provider

In `.localpilot.toml` (an official API or a local OpenAI-compatible server such
as llama.cpp / Ollama / vLLM):

```toml
[provider]
default = "local"

[providers.local]
kind = "openai-compatible"
base_url = "http://localhost:8080/v1"
model = "your-local-model"
# api_key_env = "OPENAI_API_KEY"   # for a hosted API
```

## Talk to it

```sh
localpilot ask --model your-local-model "explain this repo's error handling"
localpilot chat                 # interactive REPL (tui builds)
localpilot                      # no args: REPL, or doctor if unconfigured
```

## Next steps

- [[How-To]] — configure providers, add MCP servers, use tools.
- [[Examples]] — one-shot, pipeline, and headless drive.
- [[Reference]] — the full product + technical specification.
- [[Troubleshooting]] — common problems and fixes.
