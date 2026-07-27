# Installing LocalPilot

LocalPilot is a Rust-native, provider-neutral coding-agent harness for Windows,
Linux, and macOS (all tier-1).

## Quick install

```sh
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/C0deGeek-dev/LocalPilot/main/install/install.sh | sh
```

```powershell
# Windows
irm https://raw.githubusercontent.com/C0deGeek-dev/LocalPilot/main/install/install.ps1 | iex
```

No Rust toolchain needed. The script downloads the prebuilt `localpilot` for your
platform, **checks it against the published SHA-256 before unpacking it**, and
then uses it to install the rest of the stack.

### What it installs

Four tools, all at the same version: **`localpilot`** (the agent harness),
**`localmind`** (the learning and memory engine), **`localbox`** (the local-model
launcher), and **`localbench`** (the benchmark runner).

They are cut as a set — one version, one tag — and only tested together, so the
installer treats them as a set. A stack assembled from different releases is a
configuration nobody has run.

### Putting them on `PATH`

Everything lands in one directory, and the installer prints it:

| Platform | Directory |
|---|---|
| Linux / macOS | `~/.local/share/localx/bin` (or `$XDG_DATA_HOME/localx/bin`) |
| Windows | `%LOCALAPPDATA%\localx\bin` |

```sh
export PATH="$HOME/.local/share/localx/bin:$PATH"   # add to your shell profile
```

That entry never changes. Updates, pins, and rollbacks swap what the directory
points at, so `PATH` is something you set once.

### Reading a script before you run it

Piping a script from the internet into a shell runs whatever is at that URL. If
you would rather look first — a reasonable habit, not paranoia:

```sh
curl -fsSL -o install.sh https://raw.githubusercontent.com/C0deGeek-dev/LocalPilot/main/install/install.sh
less install.sh
sh install.sh
```

### Options

```sh
sh install.sh --version 2.6.0   # a specific release instead of the latest
sh install.sh --from-source     # compile instead (needs a checkout and cargo)
```

Run inside a LocalPilot checkout, the script builds **that working tree** rather
than downloading a release — a developer running the installer in their own clone
means their code. Pass `--binary` to override that.

## Prebuilt binary, by hand

Every release publishes an archive per platform, a `SHA256SUMS` file, and a
`manifest.json` indexing the release. Download the archive for your platform from
the [latest release](https://github.com/C0deGeek-dev/LocalPilot/releases/latest),
check it against the published digest, unpack it, and put `localpilot` on your
`PATH`.

| Platform | Archive |
|---|---|
| Windows x86-64 | `localpilot-x86_64-pc-windows-msvc.tar.gz` |
| Linux x86-64 (glibc) | `localpilot-x86_64-unknown-linux-gnu.tar.gz` |
| Linux x86-64 (static) | `localpilot-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 | `localpilot-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `localpilot-aarch64-apple-darwin.tar.gz` |

Use the **musl** build on Alpine, in containers, or on any Linux whose glibc is
older than the build host's — it is statically linked and does not care.

```sh
# Linux / macOS — verify before unpacking
curl -LO https://github.com/C0deGeek-dev/LocalPilot/releases/latest/download/localpilot-x86_64-unknown-linux-gnu.tar.gz
curl -LO https://github.com/C0deGeek-dev/LocalPilot/releases/latest/download/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf localpilot-x86_64-unknown-linux-gnu.tar.gz
```

```powershell
# Windows — tar is built in
Invoke-WebRequest -OutFile localpilot.tar.gz https://github.com/C0deGeek-dev/LocalPilot/releases/latest/download/localpilot-x86_64-pc-windows-msvc.tar.gz
Invoke-WebRequest -OutFile SHA256SUMS https://github.com/C0deGeek-dev/LocalPilot/releases/latest/download/SHA256SUMS
(Get-FileHash localpilot.tar.gz -Algorithm SHA256).Hash.ToLower()   # compare with SHA256SUMS
tar -xzf localpilot.tar.gz
```

### Verifying who built it

The checksum proves the archive is byte-for-byte what CI produced — not corrupted
or truncated. On its own it does not prove *origin*: anyone able to alter the
release could alter the checksum beside it.

Releases also carry **build provenance**, signed keylessly through Sigstore and
recorded in its public transparency log. That binds each archive to the workflow,
repository, and commit that built it:

```sh
gh attestation verify localpilot-x86_64-unknown-linux-gnu.tar.gz   --repo C0deGeek-dev/LocalPilot
```

A pass means GitHub's build system produced that exact archive from this
repository. There is no signing key to trust, hold, or rotate — the workflow's own
identity is the signer.

**What this is not.** It is not OS-level code signing. macOS Gatekeeper will still
warn about an unidentified developer, and Windows SmartScreen may still prompt —
those need an Apple Developer ID and an EV certificate respectively, both paid
annual accounts, and neither is in place. On macOS you may need
`xattr -d com.apple.quarantine ./localpilot` after unpacking.

### Staying up to date

```sh
localpilot update            # fetch, verify, and install the newest release
localpilot update --check    # only report whether one is available
localpilot update --all      # install the whole stack at this binary's version
localpilot version list      # what is installed, and which one would run
localpilot version pin 2.5.0 # hold a version
localpilot version rollback  # go back to the previous installed version
```

`update` downloads the archive for your platform, checks it against the digest in
the release manifest, and only then unpacks it. Each version installs into its own
directory, so the running binary is never overwritten, an interrupted update
leaves the previous version working, and `rollback` is instant.

Every one of these commands also refreshes the executable in the `bin` directory
on your `PATH`, so a pin or a rollback takes effect in your shell rather than only
in `version list`.

`update --all` re-installs the whole stack at the running binary's version — the
command to reach for after a manual install, or when a tool is missing. It does
not check for a newer release; update `localpilot` first, then run it.

> Releases before 2.6.0 shipped a `.zip` on Windows. `update` reads `.tar.gz`, so
> on Windows it can install 2.6.0 and later; for anything earlier, download by
> hand or use `--from-source`.

## From source

Use this when you want to build from a working tree, or on a platform with no
published archive — `update --from-source` also falls back here automatically.

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

## Build and install

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
