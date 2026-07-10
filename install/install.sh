#!/bin/sh
# Build and install the LocalPilot CLI from source on Linux or macOS.
#
# Usage:
#   ./install/install.sh                       # full build (tui + LocalMind)
#   LOCALPILOT_FEATURES= ./install/install.sh  # no interactive TUI
#
# A dev build (working tree not exactly on a clean release tag) tracks
# LocalMind's latest `main` instead of the pinned release commit; see
# docs/localmind-integration.md.
set -eu

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo (the Rust toolchain) is required." >&2
    echo "       install it from https://rustup.rs and re-run this script." >&2
    exit 1
fi

features="${LOCALPILOT_FEATURES-tui}"
root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"

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
