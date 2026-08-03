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

Releases also carry **build provenance**, signed keylessly through Sigstore. As
long as LocalPilot remains a public repository, GitHub uses Sigstore's
public-good instance and records the attestation in the public Rekor
transparency log. That binds each archive to the workflow, repository, and
commit that built it:

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

### Bringing a Claude Code session across

```sh
localpilot import claude-code                     # newest session for this directory
localpilot import claude-code --session <id>      # a specific session id
localpilot import claude-code --project <path>    # a .jsonl file or project directory
```

Imports a Claude Code session (`~/.claude/projects/.../<id>.jsonl`) into this
workspace as a resumable LocalPilot session. The history is text-flattened — tool
calls and results become plain-text markers and reasoning is dropped — so it
resumes under any provider, and it is redacted on write. Resume it with
`localpilot --resume imported_cc_<id>`; it shows a `[cc-import]` badge in
`localpilot session list`. A re-import never overwrites an existing session (use
`--force` to import again under a new name).

### Staying up to date

```sh
localpilot update            # fetch, verify, and install the newest release
localpilot update --check    # only report whether one is available
localpilot update --all      # install the whole stack at this binary's version
localpilot version list      # what is installed, and which one would run
localpilot version pin 2.6.0 # hold a version
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

### Self-dev builds (developer feature)

A build LocalPilot makes from its *own* source keeps its state entirely separate
from the releases above, under a `selfdev/` subtree beside the release cache
(`%LOCALAPPDATA%\localx\selfdev` on Windows, `~/.local/share/localx/selfdev` on
Linux/macOS). You can delete that subtree wholesale without touching an installed
release.

Inside it, the same one-directory-per-version idea holds, keyed by a source label
rather than a release version:

| Path | Holds |
| --- | --- |
| `selfdev/versions/<label>/` | one immutable build; `<label>` is `<short-hash>` for a clean tree, `<short-hash>-dirty-<fingerprint>` for a modified one |
| `selfdev/versions/<label>/.selfdev.json` | the build's marker (source hash, fingerprint, embedded version); its presence is what makes the build resolvable |
| `selfdev/channels/<name>.json` | a *channel pointer* — a small file naming the label that `<name>` (e.g. `current`, `stable`, `slow`) currently resolves to |
| `selfdev/reload/<session>.json` | a *continuation intent* — written before a reload so the session on the far side continues itself; kept until delivered, then reclaimed |

A version directory is written once and never modified; switching which build
runs only rewrites a channel pointer, so a running process is never exec'd from a
path a later build can overwrite. The pointer is a marker file, not a symlink, on
every platform — identical behaviour on Windows, Linux, and macOS, and no
elevated privilege required.

Each build copies a whole binary in, so `selfdev publish` reclaims versions
beyond the most recent few afterwards, and `selfdev gc` does it on demand; a
version a channel points at is never reclaimed. You can delete the whole
`selfdev/` subtree at any time — it is rebuilt on the next `selfdev` command and
never affects an installed release.

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

Add `--hygiene` to also report **context hygiene** — the authored context a
session assembles from the current directory (your `CLAUDE.md`/`AGENTS.md`
instruction files and the skills visible to the project), each with a token
estimate, plus advisory findings: the same directive stated in more than one
layer (redundancy), directives that disagree (conflict), and layers large enough
to be worth right-sizing. It reads and reports only — it never edits your files —
and every quoted snippet passes the same redactor as the rest of `doctor`, so a
secret in an instruction file is not echoed. It rides the shared `--format
human|json`:

```sh
localpilot doctor --hygiene
localpilot doctor --hygiene --format json
```

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

## The update notice

The interactive REPL and the bare `localpilot` launch do a best-effort, cached
check (at most once a day) and show a notice when a newer release exists.
**Nothing self-updates** — installing is always something you run. Disable the
notice with `LOCALPILOT_NO_UPDATE_CHECK=1`. The automatic check is off on the
`windows-gnu` toolchain (its TLS stack is unstable); `localpilot update` still
works there.

## From crates.io

```sh
cargo install localpilot --features tui
```

The `tui` feature is required for the interactive `chat` REPL — the default
feature set is empty, so a bare `cargo install localpilot` yields a binary
without `chat` (the `ask`/`print`/`harness` commands still work). (Available once
the crate is published; the source build above always works.)

## Running the optional server

By default LocalPilot runs in-process: `chat`, `ask`, `print`, and `harness`
never start a background service. If you want several clients to share one
long-lived session, start the **opt-in** local-IPC server for a workspace and
attach clients to it:

```console
$ localpilot serve        # foreground; Ctrl-C to stop (scoped to this workspace)
$ localpilot connect      # attach a plain-text client (--resume <id|name>, --server)
```

It is a local-only Unix-socket / Windows-named-pipe transport — never a network
server — and stays entirely opt-in. See
[embedding.md](embedding.md#running-the-opt-in-server-serve--connect).

## Next steps

- Configure a provider — see [providers.md](providers.md).
- Connect MCP tool servers — see [mcp.md](mcp.md).
- Read the security model — see [security.md](security.md).
