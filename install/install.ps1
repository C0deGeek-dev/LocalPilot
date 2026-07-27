# Install the LocalX stack (localpilot, localmind, localbox, localbench) on
# Windows.
#
# Usage:
#   irm https://raw.githubusercontent.com/C0deGeek-dev/LocalPilot/main/install/install.ps1 | iex
#
#   ./install/install.ps1                         # in a checkout: build from source
#   ./install/install.ps1 -Binary                 # in a checkout: install the release
#   ./install/install.ps1 -Version 2.6.0          # a specific release
#   ./install/install.ps1 -FromSource             # force a source build
#   ./install/install.ps1 -Features ''            # source build without the TUI
#   ./install/install.ps1 -Toolchain stable       # force a toolchain
#   ./install/install.ps1 -Target x86_64-pc-windows-gnu   # force a target
#
# Run standalone (piped from the network, no checkout) it installs prebuilt,
# checksum-verified binaries and needs no Rust toolchain. Run from inside a
# checkout it builds that working tree, because a developer running the installer
# in their own clone means their code, not the last release.
#requires -Version 5
param(
    [switch]$Binary,
    [switch]$FromSource,
    [string]$Version = '',
    [string]$Features = 'tui',
    [string]$Toolchain = '',
    [string]$Target = ''
)
$ErrorActionPreference = 'Stop'
$repo = 'C0deGeek-dev/LocalPilot'

# A checkout is only detectable when the script is a real file on disk; piped
# into `iex` it has no path, which is exactly the standalone case.
$root = $null
if ($PSScriptRoot) {
    $candidate = Split-Path -Parent $PSScriptRoot
    if ($candidate -and (Test-Path (Join-Path $candidate 'Cargo.toml'))) { $root = $candidate }
}

$mode = if ($Binary) { 'binary' } elseif ($FromSource) { 'source' } elseif ($root) { 'source' } else { 'binary' }

if ($mode -eq 'source' -and -not $root) {
    Write-Error "a source build needs a checkout; clone it first: git clone --recurse-submodules https://github.com/$repo.git"
}

# --- binary install ---------------------------------------------------------

if ($mode -eq 'binary') {
    if (-not (Get-Command tar -ErrorAction SilentlyContinue)) {
        Write-Error "tar is required (shipped with Windows 10 1803 and later)."
    }

    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($arch -ne 'AMD64') {
        Write-Error "no published build for $arch. Build from source: https://github.com/$repo#from-source"
    }
    $target = 'x86_64-pc-windows-msvc'

    $base = if ($Version) {
        "https://github.com/$repo/releases/download/v$($Version.TrimStart('v'))"
    } else {
        "https://github.com/$repo/releases/latest/download"
    }

    $archive = "localpilot-$target.tar.gz"
    $work = Join-Path ([System.IO.Path]::GetTempPath()) ("localx-" + [System.Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $work | Out-Null
    try {
        Write-Host "downloading $archive ..."
        # Progress rendering makes Invoke-WebRequest dramatically slower.
        $previousProgress = $ProgressPreference
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -Uri "$base/$archive" -OutFile (Join-Path $work $archive)
        Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile (Join-Path $work 'SHA256SUMS')
        $ProgressPreference = $previousProgress

        # Verify before anything is unpacked, let alone executed.
        Write-Host "verifying checksum ..."
        $actual = (Get-FileHash (Join-Path $work $archive) -Algorithm SHA256).Hash.ToLower()
        $line = Get-Content (Join-Path $work 'SHA256SUMS') |
            Where-Object { $_ -match "\s\*?$([regex]::Escape($archive))$" } |
            Select-Object -First 1
        if (-not $line) {
            Write-Error "$archive is not listed in the published SHA256SUMS."
        }
        $expected = ($line -split '\s+')[0].ToLower()
        if ($actual -ne $expected) {
            Write-Error "checksum mismatch for $archive - refusing to install.`n  expected $expected`n  actual   $actual"
        }

        tar -xzf (Join-Path $work $archive) -C $work
        $binary = Get-ChildItem -Path $work -Filter 'localpilot.exe' -Recurse -File | Select-Object -First 1
        if (-not $binary) { Write-Error "the archive contained no localpilot.exe." }

        # From here the binary owns the install layout. Duplicating the cache and
        # marker rules in PowerShell would be a second implementation to keep in step.
        $bin = Join-Path $env:LOCALAPPDATA 'localx\bin'
        New-Item -ItemType Directory -Path $bin -Force | Out-Null
        Copy-Item $binary.FullName (Join-Path $bin 'localpilot.exe') -Force

        Write-Host ""
        Write-Host "installing the rest of the stack at the same version ..."
        # `update --all` arrived in 2.6.0. Installing an older release is
        # legitimate (-Version), and it must not look like the whole install
        # failed: localpilot is on disk and working either way.
        & (Join-Path $bin 'localpilot.exe') update --all
        if ($LASTEXITCODE -ne 0) {
            Write-Host ""
            Write-Host "note: this release cannot install the rest of the stack itself."
            Write-Host "      localpilot is installed; for localmind, localbox, and localbench"
            Write-Host "      install 2.6.0 or later, then run: localpilot update --all"
        }

        Write-Host ""
        Write-Host "verify with:"
        Write-Host "    $bin\localpilot.exe doctor"
        Write-Host "authenticity of the downloaded archives (needs the GitHub CLI):"
        Write-Host "    gh attestation verify $archive --repo $repo"
        if (($env:PATH -split ';') -notcontains $bin) {
            Write-Host ""
            Write-Host "add this directory to PATH:"
            Write-Host "    setx PATH `"`$env:PATH;$bin`"   (new terminals only)"
        }
    } finally {
        Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
    }
    return
}

# --- source install ---------------------------------------------------------

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo (the Rust toolchain) is required for a source build. Install it from https://rustup.rs, or drop -FromSource to install the prebuilt binary instead."
}

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
