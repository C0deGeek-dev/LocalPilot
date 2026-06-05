#!/bin/sh
# Build and install the Unshackled CLI from source on Linux or macOS.
#
# Usage:
#   ./install/install.sh                       # full build (tui + LocalMind)
#   UNSHACKLED_FEATURES= ./install/install.sh  # no interactive TUI
set -eu

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo (the Rust toolchain) is required." >&2
    echo "       install it from https://rustup.rs and re-run this script." >&2
    exit 1
fi

features="${UNSHACKLED_FEATURES-tui}"
root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"

# The LocalMind learning engine is a git submodule and is always linked into the
# CLI.
if [ -f "$root/.gitmodules" ] && command -v git >/dev/null 2>&1; then
    echo "updating submodules ..."
    git -C "$root" submodule update --init --recursive
fi

echo "building and installing the unshackled CLI (features: $features) ..."
if [ -n "$features" ]; then
    cargo install --path "$root/crates/unshackled-cli" --features "$features" --locked
else
    cargo install --path "$root/crates/unshackled-cli" --locked
fi

echo
echo "installed 'unshackled'. verify with:"
echo "    unshackled doctor"
