#!/bin/sh
# Install the LocalX stack (localpilot, localmind, localbox, localbench) on
# Linux or macOS.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/C0deGeek-dev/LocalPilot/main/install/install.sh | sh
#
#   ./install/install.sh                       # in a checkout: build from source
#   ./install/install.sh --binary              # in a checkout: install the release
#   ./install/install.sh --version 2.6.0       # a specific release
#   ./install/install.sh --from-source         # force a source build
#   LOCALPILOT_FEATURES= ./install/install.sh  # source build without the TUI
#
# Run standalone (piped from the network, no checkout) it installs prebuilt,
# checksum-verified binaries and needs no Rust toolchain. Run from inside a
# checkout it builds that working tree, because a developer running the installer
# in their own clone means their code, not the last release.
set -eu

REPO=C0deGeek-dev/LocalPilot
mode=
version=

while [ $# -gt 0 ]; do
    case "$1" in
        --binary) mode=binary ;;
        --from-source) mode=source ;;
        --version) shift; version="${1-}" ;;
        --version=*) version="${1#--version=}" ;;
        -h|--help) sed -n '2,20p' "$0" 2>/dev/null || echo "see docs/install.md"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

# A checkout is only detectable when the script is a real file on disk; piped
# through a shell it has no path, which is exactly the standalone case.
root=
case "$0" in
    */*) candidate="$(CDPATH= cd -- "$(dirname -- "$0")/.." 2>/dev/null && pwd)" || candidate=
         [ -n "$candidate" ] && [ -f "$candidate/Cargo.toml" ] && root="$candidate" ;;
esac
[ -z "$mode" ] && { [ -n "$root" ] && mode=source || mode=binary; }

if [ "$mode" = source ] && [ -z "$root" ]; then
    echo "error: --from-source needs a checkout; clone the repository first:" >&2
    echo "       git clone --recurse-submodules https://github.com/$REPO.git" >&2
    exit 1
fi

# --- binary install ---------------------------------------------------------

if [ "$mode" = binary ]; then
    for tool in curl tar; do
        command -v "$tool" >/dev/null 2>&1 || { echo "error: $tool is required." >&2; exit 1; }
    done

    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os/$arch" in
        Linux/x86_64)   target=x86_64-unknown-linux-gnu ;;
        Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-gnu ;;
        Darwin/arm64)   target=aarch64-apple-darwin ;;
        Darwin/x86_64)
            echo "error: no published build for Intel macOS." >&2
            echo "       build from source: https://github.com/$REPO#from-source" >&2
            exit 1 ;;
        *)
            echo "error: no published build for $os/$arch." >&2
            echo "       build from source: https://github.com/$REPO#from-source" >&2
            exit 1 ;;
    esac

    # A musl libc has no glibc to link against; the static build is the one that
    # runs there.
    if [ "$target" = x86_64-unknown-linux-gnu ] && ! ldd /bin/sh 2>/dev/null | grep -q 'libc\.so'; then
        target=x86_64-unknown-linux-musl
    fi

    if [ -n "$version" ]; then
        base="https://github.com/$REPO/releases/download/v${version#v}"
    else
        base="https://github.com/$REPO/releases/latest/download"
    fi

    archive="localpilot-$target.tar.gz"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT INT TERM

    echo "downloading $archive ..."
    curl -fsSL -o "$work/$archive" "$base/$archive"
    curl -fsSL -o "$work/SHA256SUMS" "$base/SHA256SUMS"

    # Verify before anything is unpacked, let alone executed.
    echo "verifying checksum ..."
    if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "$work/$archive" | cut -d' ' -f1)"
    elif command -v shasum >/dev/null 2>&1; then
        actual="$(shasum -a 256 "$work/$archive" | cut -d' ' -f1)"
    else
        echo "error: no sha256sum or shasum available to verify the download." >&2
        exit 1
    fi
    expected="$(grep " \{1,2\}\*\{0,1\}$archive\$" "$work/SHA256SUMS" | cut -d' ' -f1 | head -n1)"
    if [ -z "$expected" ]; then
        echo "error: $archive is not listed in the published SHA256SUMS." >&2
        exit 1
    fi
    if [ "$actual" != "$expected" ]; then
        echo "error: checksum mismatch for $archive — refusing to install." >&2
        echo "       expected $expected" >&2
        echo "       actual   $actual" >&2
        exit 1
    fi

    tar -xzf "$work/$archive" -C "$work"
    binary="$(find "$work" -name localpilot -type f -print -quit)"
    [ -n "$binary" ] || { echo "error: the archive contained no localpilot binary." >&2; exit 1; }
    chmod +x "$binary"

    # From here the binary owns the install layout. Duplicating the cache and
    # marker rules in shell would be a second implementation to keep in step.
    bin="${XDG_DATA_HOME:-$HOME/.local/share}/localx/bin"
    mkdir -p "$bin"
    cp "$binary" "$bin/localpilot"

    echo
    echo "installing the stack ..."
    # `update --all` arrived in 2.6.0. Installing an older release is legitimate
    # (--version), and it must not look like the whole install failed: localpilot
    # is on disk and working either way. It also owns the PATH advice, so the
    # fallback below is the only place this script gives its own.
    if ! "$bin/localpilot" update --all; then
        echo
        echo "note: this release cannot install the rest of the stack itself."
        echo "      localpilot is installed; for localmind, localbox, and localbench"
        echo "      install 2.6.0 or later, then run: localpilot update --all"
        case ":$PATH:" in
            *":$bin:"*) ;;
            *) echo
               echo "add this directory to PATH:"
               echo "    export PATH=\"$bin:\$PATH\"   (add to your shell profile)" ;;
        esac
    fi

    echo
    echo "verify with:"
    echo "    $bin/localpilot doctor"
    echo "authenticity of the downloaded archives (needs the GitHub CLI):"
    echo "    gh attestation verify $archive --repo $REPO"
    exit 0
fi

# --- source install ---------------------------------------------------------

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo (the Rust toolchain) is required for a source build." >&2
    echo "       install it from https://rustup.rs, or drop --from-source to" >&2
    echo "       install the prebuilt binary instead." >&2
    exit 1
fi

features="${LOCALPILOT_FEATURES-tui}"

# The LocalMind learning engine is a git submodule and is always linked into the
# CLI. A release build (working tree exactly on a clean version tag) stays on
# the pinned, tested LocalMind commit; any other build is treated as local
# development and tracks LocalMind's latest `main` instead. See
# docs/localmind-integration.md for the rationale.
is_release_build=false
if command -v git >/dev/null 2>&1; then
    if git -C "$root" describe --tags --exact-match --match 'v[0-9]*' >/dev/null 2>&1 \
        && [ -z "$(git -C "$root" status --porcelain)" ]; then
        is_release_build=true
    fi
fi

if [ -f "$root/.gitmodules" ] && command -v git >/dev/null 2>&1; then
    echo "updating submodules ..."
    git -C "$root" submodule update --init --recursive
    if [ "$is_release_build" = false ]; then
        localmind="$root/external/localmind"
        echo "dev build detected: tracking LocalMind's latest main instead of the pinned release ..."
        if git -C "$localmind" fetch origin main; then
            git -C "$localmind" checkout FETCH_HEAD
        else
            echo "warning: could not fetch LocalMind's latest main; staying on the pinned commit." >&2
        fi
    fi
fi

echo "building and installing the localpilot CLI (features: $features) ..."
if [ -n "$features" ]; then
    cargo install --path "$root/crates/localpilot-cli" --features "$features" --locked
else
    cargo install --path "$root/crates/localpilot-cli" --locked
fi

echo
echo "installed 'localpilot'. verify with:"
echo "    localpilot doctor"
