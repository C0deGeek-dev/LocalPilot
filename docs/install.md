# Installing LocalPilot

LocalPilot is a Rust-native, provider-neutral coding-agent harness for Windows,
Linux, and macOS (all tier-1).

## Requirements

- The Rust toolchain (`cargo`, MSRV 1.82) from <https://rustup.rs>.
- `git` (the LocalMind learning engine is a submodule).
- A C compiler for the bundled LocalMind SQLite store: `cc`/`clang` on
  Linux/macOS, the MSVC C++ build tools on Windows.

Clone with submodules (or initialize them after cloning):

```sh
git clone --recurse-submodules https://github.com/C0deGeek-dev/LocalPilot.git
# or, in an existing clone:
git submodule update --init --recursive
```

## From source (recommended)

```sh
# Linux / macOS
./install/install.sh

# Windows (PowerShell)
./install/install.ps1
```

Both build a full binary with the interactive TUI and run `cargo install --path
crates/localpilot-cli --locked`. After install:

```sh
localpilot doctor
```

`doctor` reports your platform, the config search paths, which provider
credentials are present (never their values), tool availability, and workspace
trust state.

### Build features

The default binary includes LocalMind-backed learning and memory. The installers
enable the interactive TUI feature by default:

- `tui` — the interactive `chat` REPL.

Pick a different set when you don't want one:

```sh
# Linux / macOS — skip the interactive TUI:
LOCALPILOT_FEATURES= ./install/install.sh

# Windows — skip the interactive TUI:
./install/install.ps1 -Features ''
```

### Windows: use the MSVC toolchain for `chat`

The interactive TUI is unstable under the `windows-gnu` toolchain. `install.ps1`
automatically builds with the MSVC toolchain when it is installed; install it if
needed:

```powershell
rustup toolchain install stable-x86_64-pc-windows-msvc
```

If you only need non-interactive commands (`ask`, `print`, `harness`, `memory`,
`learning`), the gnu toolchain is fine.

## Updating

```sh
localpilot update          # check the repo and, on confirmation, reinstall
localpilot update --check   # only report whether a newer release exists
```

`update` queries the project repository for the newest release tag, compares it
to the running binary's embedded version, and on your confirmation reinstalls
from source with the same feature set (`cargo install --git … --tag …`), using
the MSVC toolchain on Windows when the TUI is built.

The interactive REPL and the bare `localpilot` launch also do a best-effort,
cached check (at most once a day) and show a notice when an update is available.
Disable it with `LOCALPILOT_NO_UPDATE_CHECK=1`. The automatic check is off on the
`windows-gnu` toolchain (its TLS stack is unstable); `localpilot update` still
works there.

## From a release archive

Each tagged release publishes per-platform archives that contain the
`localpilot` binary plus `LICENSE-MIT`. Download the archive for your platform,
extract it, and put the binary on your `PATH`.

## From crates.io

```sh
cargo install localpilot --features tui
```

The `tui` feature is required for the interactive `chat` REPL — the default
feature set is empty, so a bare `cargo install localpilot` yields a binary
without `chat` (the `ask`/`print`/`harness` commands still work). (Available once
the crate is published; the source build above always works.)

## Next steps

- Configure a provider — see [providers.md](providers.md).
- Connect MCP tool servers — see [mcp.md](mcp.md).
- Read the security model — see [security.md](security.md).
