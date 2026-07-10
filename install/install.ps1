# Build and install the LocalPilot CLI from source on Windows.
#
# Usage:
#   ./install/install.ps1                         # full build (tui + LocalMind)
#   ./install/install.ps1 -Features ''            # no interactive TUI
#   ./install/install.ps1 -Toolchain stable       # force a toolchain
#   ./install/install.ps1 -Target x86_64-pc-windows-gnu   # force a target
#
# A dev build (working tree not exactly on a clean release tag) tracks
# LocalMind's latest `main` instead of the pinned release commit; see
# docs/localmind-integration.md.
#requires -Version 5
param(
    [string]$Features = 'tui',
    [string]$Toolchain = '',
    [string]$Target = ''
)
$ErrorActionPreference = 'Stop'

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo (the Rust toolchain) is required. Install it from https://rustup.rs and re-run."
}

$root = Split-Path -Parent $PSScriptRoot
$cli = Join-Path $root 'crates/localpilot-cli'

# The LocalMind learning engine is a git submodule and is always linked into the
# CLI. A release build (working tree exactly on a clean version tag) stays on
# the pinned, tested LocalMind commit; any other build is treated as local
# development and tracks LocalMind's latest `main` instead. See
# docs/localmind-integration.md for the rationale.
$isReleaseBuild = $false
if (Get-Command git -ErrorAction SilentlyContinue) {
    git -C $root describe --tags --exact-match --match 'v[0-9]*' *> $null
    $tagMatch = ($LASTEXITCODE -eq 0)
    $clean = -not (git -C $root status --porcelain)
    $isReleaseBuild = $tagMatch -and $clean
}

if ((Test-Path (Join-Path $root '.gitmodules')) -and (Get-Command git -ErrorAction SilentlyContinue)) {
    Write-Host "updating submodules ..."
    git -C $root submodule update --init --recursive
    if (-not $isReleaseBuild) {
        $localmind = Join-Path $root 'external/localmind'
        Write-Host "dev build detected: tracking LocalMind's latest main instead of the pinned release ..."
        git -C $localmind fetch origin main
        if ($LASTEXITCODE -eq 0) {
            git -C $localmind checkout FETCH_HEAD
        } else {
            Write-Warning "could not fetch LocalMind's latest main; staying on the pinned commit."
        }
    }
}

# The interactive TUI (crossterm) is unstable under the windows-gnu toolchain;
# prefer the MSVC toolchain (and target) when building with the `tui` feature.
if (-not $Toolchain -and ($Features -match 'tui') -and (Get-Command rustup -ErrorAction SilentlyContinue)) {
    if ((rustup toolchain list) -match 'msvc') {
        $Toolchain = 'stable-x86_64-pc-windows-msvc'
        # A global `build.target = x86_64-pc-windows-gnu` in ~/.cargo/config.toml
        # would otherwise force a gnu binary even under the MSVC toolchain, so the
        # MSVC target is set explicitly.
        if (-not $Target) { $Target = 'x86_64-pc-windows-msvc' }
        Write-Host "using the MSVC toolchain/target for a stable 'chat' (TUI) build."
    } else {
        Write-Warning "the 'tui' feature (chat) is unstable on the windows-gnu toolchain."
        Write-Warning "install MSVC for a working 'chat':  rustup toolchain install stable-x86_64-pc-windows-msvc"
        Write-Warning "or skip it:  ./install/install.ps1 -Features ''"
    }
}

Write-Host "building and installing the localpilot CLI (features: $Features) ..."
$cargoArgs = @()
if ($Toolchain) { $cargoArgs += "+$Toolchain" }
$cargoArgs += @('install', '--path', $cli, '--locked', '--force')
if ($Features) { $cargoArgs += @('--features', $Features) }
if ($Target) { $cargoArgs += @('--target', $Target) }
cargo @cargoArgs
# A native command failure does not trip $ErrorActionPreference; check explicitly
# so a failed build never reports success.
if ($LASTEXITCODE -ne 0) {
    Write-Error "cargo install failed (exit $LASTEXITCODE). See the build error above. If it is a missing C compiler (SQLite/rusqlite for LocalMind), install the Visual Studio Build Tools 'Desktop development with C++' workload."
}

Write-Host ""
Write-Host "installed 'localpilot'. verify with:"
Write-Host "    localpilot doctor"
