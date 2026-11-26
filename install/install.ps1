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
#   ./install/install.ps1 -PowerShellShortcuts     # also install the optional llm* shortcuts
#   ./install/install.ps1 -NoSetup                 # install only; skip the guided first run
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
    [string]$Target = '',
    [Alias('InstallPowerShellShortcuts')][switch]$PowerShellShortcuts,
    [switch]$NoSetup
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

# --- PATH -------------------------------------------------------------------
# The installer runs in the user's own PowerShell session, so it is the one
# step that can make the commands work both in this terminal and in every new
# one. These functions are self-contained so install/test-install-path.ps1 can
# exercise them against a scratch registry key.

# One PATH entry in comparable form: variables expanded, quotes and a trailing
# separator dropped, case folded.
function ConvertTo-LocalXPathKey {
    param([string]$Entry)
    $expanded = [Environment]::ExpandEnvironmentVariables($Entry.Trim().Trim('"'))
    return $expanded.TrimEnd('\', '/').ToLowerInvariant()
}

# Append $Directory to the user PATH in the registry when no entry already
# names it. Reads the raw value (unexpanded, so %VARIABLES% survive), keeps its
# registry kind, and never touches the machine PATH. Returns 'present' or
# 'added'.
function Add-LocalXUserPath {
    param(
        [Parameter(Mandatory)][string]$Directory,
        [string]$SubKey = 'Environment'
    )
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($SubKey)
    try {
        $raw = $key.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
        if ($null -ne $raw) {
            $existingKind = $key.GetValueKind('Path')
            if ($existingKind -eq [Microsoft.Win32.RegistryValueKind]::String) { $kind = $existingKind }
        }
        $wanted = ConvertTo-LocalXPathKey $Directory
        $entries = @("$raw" -split ';' | Where-Object { $_.Trim() })
        foreach ($entry in $entries) {
            if ((ConvertTo-LocalXPathKey $entry) -eq $wanted) { return 'present' }
        }
        $updated = (@($entries) + $Directory) -join ';'
        $key.SetValue('Path', $updated, $kind)
        return 'added'
    }
    finally {
        $key.Dispose()
    }
}

# Tell running programs (Explorer, so new terminals) that the environment
# changed. Returns $false when the broadcast failed or a window did not answer
# within its timeout (5 s per window, not overall); the registry change is
# still there, and every new sign-in picks it up.
function Send-LocalXEnvironmentChange {
    try {
        if (-not ('LocalX.EnvironmentBroadcast' -as [type])) {
            Add-Type -Namespace LocalX -Name EnvironmentBroadcast -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll", SetLastError = true, CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
public static extern System.IntPtr SendMessageTimeout(System.IntPtr hWnd, uint Msg, System.UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out System.UIntPtr lpdwResult);
'@
        }
        $result = [System.UIntPtr]::Zero
        # HWND_BROADCAST, WM_SETTINGCHANGE, SMTO_ABORTIFHUNG, 5000 ms per window.
        $sent = [LocalX.EnvironmentBroadcast]::SendMessageTimeout([System.IntPtr]0xffff, 0x1A, [System.UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$result)
        return ($sent -ne [System.IntPtr]::Zero)
    }
    catch {
        return $false
    }
}

# Put $Directory first on this session's PATH, moving it there when it is
# already present further down (a rerun then repairs resolution in this window).
# Returns 'present' when it was already first, else 'added' or 'moved'.
function Add-LocalXSessionPath {
    param([Parameter(Mandatory)][string]$Directory)
    $wanted = ConvertTo-LocalXPathKey $Directory
    $entries = @($env:PATH -split ';' | Where-Object { $_.Trim() })
    if ($entries.Count -gt 0 -and (ConvertTo-LocalXPathKey $entries[0]) -eq $wanted) { return 'present' }
    $rest = @($entries | Where-Object { (ConvertTo-LocalXPathKey $_) -ne $wanted })
    $outcome = if ($rest.Count -lt $entries.Count) { 'moved' } else { 'added' }
    $env:PATH = (@($Directory) + $rest) -join ';'
    return $outcome
}

# The PATH a terminal opened from now on gets: the machine value, then the
# user value, each with its variables expanded. Both can be passed in so the
# ordering is testable without the real registry.
function Get-LocalXNewShellPath {
    param([string]$MachinePath, [string]$UserPath)
    if (-not $PSBoundParameters.ContainsKey('MachinePath')) {
        $MachinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    }
    if (-not $PSBoundParameters.ContainsKey('UserPath')) {
        $UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    }
    return [Environment]::ExpandEnvironmentVariables("$MachinePath;$UserPath")
}

# A same-named program found ahead of the LocalX copy on $SearchPath runs
# instead of it. Resolution follows Windows: directories in order, and within
# one directory the PATHEXT order.
function Get-LocalXShadowing {
    param(
        [Parameter(Mandatory)][string]$Directory,
        [Parameter(Mandatory)][string]$SearchPath
    )
    $wanted = ConvertTo-LocalXPathKey $Directory
    $extensions = @(("$env:PATHEXT" -split ';' | Where-Object { $_ }))
    if ($extensions.Count -eq 0) { $extensions = @('.COM', '.EXE', '.BAT', '.CMD') }
    $found = @()
    foreach ($tool in 'localx', 'localpilot', 'localbox', 'localmind', 'localbench') {
        $managed = Join-Path $Directory "$tool.exe"
        if (-not (Test-Path -LiteralPath $managed)) { continue }
        $winner = $null
        foreach ($entry in ($SearchPath -split ';')) {
            $dir = $entry.Trim().Trim('"')
            if (-not $dir) { continue }
            if ((ConvertTo-LocalXPathKey $dir) -eq $wanted) { break }
            foreach ($ext in $extensions) {
                # Plain string paths and a non-throwing existence check: a PATH
                # entry on a retired drive or with illegal characters is just
                # absent, never a reason to stop the installer.
                $candidate = $null
                try { $candidate = [System.IO.Path]::Combine($dir, "$tool$ext") } catch { continue }
                if ([System.IO.File]::Exists($candidate)) { $winner = $candidate; break }
            }
            if ($winner) { break }
        }
        if ($winner) {
            $found += [pscustomobject]@{ Tool = $tool; Shadow = $winner; Managed = $managed }
        }
    }
    return $found
}

# Name each program that would run instead of the LocalX copy, with the way out.
function Write-LocalXShadowing {
    param(
        [Parameter(Mandatory)][string]$Directory,
        [Parameter(Mandatory)][string]$SearchPath,
        [Parameter(Mandatory)][string]$Where
    )
    foreach ($shadow in Get-LocalXShadowing -Directory $Directory -SearchPath $SearchPath) {
        Write-Warning ("in $Where, '$($shadow.Tool)' resolves to $($shadow.Shadow), not the LocalX copy at $($shadow.Managed). " +
            "Remove or rename that older copy (or its PATH entry), or run the LocalX one by its full path: & '$($shadow.Managed)'")
    }
}

# Make the LocalX bin directory reachable now and in new terminals.
function Set-LocalXPath {
    param([Parameter(Mandatory)][string]$Directory)
    $user = Add-LocalXUserPath -Directory $Directory
    if ($user -eq 'added') {
        Write-Host "added $Directory to your user PATH."
        if (-not (Send-LocalXEnvironmentChange)) {
            Write-Host "note: not every running program confirmed the PATH change; new terminals pick it up after you sign in again."
        }
    }
    [void](Add-LocalXSessionPath -Directory $Directory)
}

# After the tools are in place: check this window and a new terminal.
function Test-LocalXPathResolution {
    param([Parameter(Mandatory)][string]$Directory)
    Write-LocalXShadowing -Directory $Directory -SearchPath $env:PATH -Where 'this window'
    Write-LocalXShadowing -Directory $Directory -SearchPath (Get-LocalXNewShellPath) -Where 'new terminals'
}

# Whether this localx has the guided first run. Older releases do not, and
# `localx <word>` would otherwise treat 'setup' as a tool name. Only the
# check lives here: the setup command itself must run as a plain statement in
# the caller, so it keeps the terminal (prompts, chat) and its output is never
# captured into a variable.
function Test-LocalXSetup {
    param([Parameter(Mandatory)][string]$Localx)
    $help = (& $Localx --help 2>&1 | Out-String)
    return ($help -match '(?m)^\s+setup\b')
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
    # Named `$targetTriple`, not `$target`: PowerShell variables are
    # case-insensitive, so `$target` is the `[string]$Target` parameter and this
    # would discard an explicit -Target.
    $targetTriple = if ($Target) { $Target } else { 'x86_64-pc-windows-msvc' }

    $base = if ($Version) {
        "https://github.com/$repo/releases/download/v$($Version.TrimStart('v'))"
    } else {
        "https://github.com/$repo/releases/latest/download"
    }

    # Bootstrap the umbrella binary (it installs the whole stack + engine); fall
    # back to localpilot for releases cut before localx existed.
    $tool = 'localx'
    $postcmd = @('install', 'all')
    $archive = "localx-$targetTriple.tar.gz"
    $work = Join-Path ([System.IO.Path]::GetTempPath()) ("localx-" + [System.Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $work | Out-Null
    try {
        # Progress rendering makes Invoke-WebRequest dramatically slower.
        $previousProgress = $ProgressPreference
        $ProgressPreference = 'SilentlyContinue'
        Write-Host "downloading $archive ..."
        try {
            Invoke-WebRequest -Uri "$base/$archive" -OutFile (Join-Path $work $archive)
        } catch {
            Write-Host "note: this release has no localx binary; bootstrapping localpilot instead."
            $tool = 'localpilot'
            $postcmd = @('update', '--all')
            $archive = "localpilot-$targetTriple.tar.gz"
            Invoke-WebRequest -Uri "$base/$archive" -OutFile (Join-Path $work $archive)
        }
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
        # Named `$binaryFile`, not `$binary`: `$binary` is the `[switch]$Binary`
        # parameter under PowerShell's case-insensitive variable names, and
        # assigning a FileInfo to it throws a MetadataError.
        $binaryFile = Get-ChildItem -Path $work -Filter "$tool.exe" -Recurse -File | Select-Object -First 1
        if (-not $binaryFile) { Write-Error "the archive contained no $tool.exe." }

        # From here the binary owns the install layout. Duplicating the cache and
        # marker rules in PowerShell would be a second implementation to keep in step.
        $bin = Join-Path $env:LOCALAPPDATA 'localx\bin'
        New-Item -ItemType Directory -Path $bin -Force | Out-Null
        Copy-Item $binaryFile.FullName (Join-Path $bin "$tool.exe") -Force
        Set-LocalXPath -Directory $bin

        Write-Host ""
        Write-Host "installing the stack ..."
        # The bootstrapped binary owns the install of everything else (and the
        # PATH advice), so the fallback below is the only place this script gives
        # its own.
        if ($PowerShellShortcuts -and $tool -eq 'localx') {
            $postcmd += '--powershell-shortcuts'
        }
        elseif ($PowerShellShortcuts) {
            Write-Warning "this older release has no localx shortcut installer; install a current release, then run 'localx install powershell-shortcuts'."
        }

        # Tell localx which PowerShell host launched this installer. That keeps
        # `$PROFILE` resolution on the same edition (PowerShell 7 vs Windows
        # PowerShell 5) instead of guessing when both are installed.
        $previousPowerShellHost = $env:LOCALX_POWERSHELL_HOST
        if ($PowerShellShortcuts -and $tool -eq 'localx') {
            $env:LOCALX_POWERSHELL_HOST = (Get-Process -Id $PID).Path
        }
        try {
            & (Join-Path $bin "$tool.exe") @postcmd
            $stackExitCode = $LASTEXITCODE
        }
        finally {
            if ($null -eq $previousPowerShellHost) {
                Remove-Item Env:\LOCALX_POWERSHELL_HOST -ErrorAction SilentlyContinue
            }
            else {
                $env:LOCALX_POWERSHELL_HOST = $previousPowerShellHost
            }
        }
        if ($stackExitCode -ne 0) {
            Write-Host ""
            Write-Host "note: the stack install did not complete."
            Write-Host "      $tool is on disk at $bin\$tool.exe; run it again to retry: & '$bin\$tool.exe' $($postcmd -join ' ')"
        }
        Test-LocalXPathResolution -Directory $bin
        $runSetup = ($stackExitCode -eq 0 -and $tool -eq 'localx' -and -not $NoSetup)

        Write-Host ""
        if ($tool -eq 'localx') {
            Write-Host "verify with:"
            Write-Host "    $bin\localx.exe status"
        } else {
            Write-Host "verify with:"
            Write-Host "    $bin\localpilot.exe doctor"
        }
        Write-Host "authenticity of the downloaded archives (needs the GitHub CLI):"
        Write-Host "    gh attestation verify $archive --repo $repo"
        $setupLocalx = Join-Path $bin 'localx.exe'
        if ($runSetup -and (Test-LocalXSetup -Localx $setupLocalx)) {
            Write-Host ""
            & $setupLocalx setup
            if ($LASTEXITCODE -ne 0) {
                Write-Host ""
                Write-Host "note: the guided setup stopped; resume it with: & '$setupLocalx' setup"
            }
        }
        elseif ($runSetup) {
            Write-Host "this localx has no guided setup yet; run '$setupLocalx status' to check the install."
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

# Both binaries are built into one staging root and then published into the
# managed directory - the same directory the binary installer and every later
# `localx update` writes to. cargo's own bin directory is deliberately not used:
# a copy there is a second install that usually wins PATH, which is how an
# updated stack ends up invisible.
$bin = Join-Path $env:LOCALAPPDATA 'localx\bin'
$staging = Join-Path $root 'target/localx-install'
Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue

Write-Host "building and installing the localpilot CLI (features: $Features) ..."
$cargoArgs = @()
if ($Toolchain) { $cargoArgs += "+$Toolchain" }
$cargoArgs += @('install', '--path', $cli, '--locked', '--root', $staging)
if ($Features) { $cargoArgs += @('--features', $Features) }
if ($Target) { $cargoArgs += @('--target', $Target) }
cargo @cargoArgs
# A native command failure does not trip $ErrorActionPreference; check explicitly
# so a failed build never reports success.
if ($LASTEXITCODE -ne 0) {
    Write-Error "cargo install failed (exit $LASTEXITCODE). See the build error above. If it is a missing C compiler (SQLite/rusqlite for LocalMind), install the Visual Studio Build Tools 'Desktop development with C++' workload."
}

# Build the umbrella from the same checkout, so `localx` matches the localpilot
# you just built. It links no TUI, so it needs no features.
Write-Host "building and installing localx ..."
$localxArgs = @()
if ($Toolchain) { $localxArgs += "+$Toolchain" }
$localxArgs += @('install', '--path', (Join-Path $root 'crates/localx'), '--locked', '--root', $staging)
if ($Target) { $localxArgs += @('--target', $Target) }
cargo @localxArgs
if ($LASTEXITCODE -ne 0) {
    Write-Error "cargo install localx failed (exit $LASTEXITCODE). See the build error above."
}

New-Item -ItemType Directory -Path $bin -Force | Out-Null
# Displace the old binary, then copy - with rollback. A running image can be
# renamed but not overwritten, so the new binary lands on a free path; if the
# copy fails, the displaced binary is restored so the canonical path is never
# left empty, and the staged build is kept (LocalHub#79).
function Publish-StackTool {
    param([string]$Tool)
    $built = Join-Path $staging "bin/$Tool.exe"
    if (-not (Test-Path $built)) { Write-Error "the build produced no $Tool.exe at $built" }
    $dest = Join-Path $bin "$Tool.exe"
    $aside = "$dest.old"
    $displaced = $false
    if (Test-Path $dest) {
        Remove-Item -Force $aside -ErrorAction SilentlyContinue
        Move-Item -Force $dest $aside
        $displaced = $true
    }
    try {
        Copy-Item -Force $built $dest
    } catch {
        if ($displaced) { Move-Item -Force $aside $dest }
        Write-Error "could not install $Tool to $dest (the staged build is kept at $built): $_"
    }
    if ($displaced) { Remove-Item -Force $aside -ErrorAction SilentlyContinue }
    return $dest
}

Publish-StackTool -Tool 'localpilot' | Out-Null
$destLocalx = Publish-StackTool -Tool 'localx'
Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue
Set-LocalXPath -Directory $bin
Test-LocalXPathResolution -Directory $bin

if ($PowerShellShortcuts) {
    if ([string]::IsNullOrWhiteSpace($destLocalx) -or -not (Test-Path -LiteralPath $destLocalx)) {
        Write-Error "localx was not installed, so its PowerShell shortcuts cannot be installed."
    }
    $previousPowerShellHost = $env:LOCALX_POWERSHELL_HOST
    $env:LOCALX_POWERSHELL_HOST = (Get-Process -Id $PID).Path
    try {
        & $destLocalx install powershell-shortcuts
        $shortcutExitCode = $LASTEXITCODE
    }
    finally {
        if ($null -eq $previousPowerShellHost) {
            Remove-Item Env:\LOCALX_POWERSHELL_HOST -ErrorAction SilentlyContinue
        }
        else {
            $env:LOCALX_POWERSHELL_HOST = $previousPowerShellHost
        }
    }
    if ($shortcutExitCode -ne 0) {
        Write-Error "localx could not install the PowerShell shortcuts (exit $shortcutExitCode)."
    }
}

Write-Host ""
Write-Host "installed 'localpilot' and 'localx' from source into $bin. verify with:"
Write-Host "    localx status"
Write-Host "install the rest of the stack (localmind, localbox, localbench) and the engine:"
Write-Host "    localx install                 # released binaries"
Write-Host "    localx install --prerelease    # or build each from its latest main"
Write-Host "developing every LocalX repository side by side? build the stack from them:"
Write-Host "    localx dev use <workspace>; localx update"

if (-not $NoSetup -and (Test-LocalXSetup -Localx $destLocalx)) {
    Write-Host ""
    & $destLocalx setup
    if ($LASTEXITCODE -ne 0) {
        Write-Host ""
        Write-Host "note: the guided setup stopped; resume it with: & '$destLocalx' setup"
    }
}
