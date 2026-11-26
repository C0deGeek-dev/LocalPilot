# Exercise the installer's PATH functions against a scratch registry key, so
# the real user PATH is never touched. Windows only (the user PATH lives in the
# registry). The functions are lifted out of install.ps1 by parsing it, because
# running the installer itself would install.
#
# Run:  pwsh -NoProfile -File install/test-install-path.ps1
[CmdletBinding()]
param([string]$Path)
$ErrorActionPreference = 'Stop'
# Windows PowerShell 5 has no $PSScriptRoot while binding parameter defaults.
if (-not $Path) { $Path = Join-Path $PSScriptRoot 'install.ps1' }

$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    (Resolve-Path -LiteralPath $Path).ProviderPath, [ref]$tokens, [ref]$errors)
if ($errors) { Write-Error "$Path does not parse." }
$functions = $ast.FindAll({ param($n) $n -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $false)
foreach ($function in $functions) {
    . ([scriptblock]::Create($function.Extent.Text))
}

$failures = @()
function Assert-Equal($Actual, $Expected, $What) {
    if ($Actual -cne $Expected) { $script:failures += "$What - expected '$Expected', got '$Actual'" }
}

$subKey = "Software\LocalXInstallTest-$([guid]::NewGuid().ToString('N'))"
$bin = Join-Path $env:LOCALAPPDATA 'localx\bin'
try {
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($subKey)

    # No Path value yet: created as REG_EXPAND_SZ holding just the directory.
    Assert-Equal (Add-LocalXUserPath -Directory $bin -SubKey $subKey) 'added' 'empty PATH'
    Assert-Equal $key.GetValue('Path') $bin 'empty PATH value'
    Assert-Equal $key.GetValueKind('Path') ([Microsoft.Win32.RegistryValueKind]::ExpandString) 'empty PATH kind'

    # Unexpanded variables survive, the kind is kept, and the entry is appended.
    $key.SetValue('Path', '%USERPROFILE%\tools;C:\other;', [Microsoft.Win32.RegistryValueKind]::ExpandString)
    Assert-Equal (Add-LocalXUserPath -Directory $bin -SubKey $subKey) 'added' 'append'
    Assert-Equal $key.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) "%USERPROFILE%\tools;C:\other;$bin" 'append keeps variables'

    # Already present under another spelling: nothing is written.
    $key.SetValue('Path', '%LOCALAPPDATA%\LocalX\Bin\', [Microsoft.Win32.RegistryValueKind]::String)
    Assert-Equal (Add-LocalXUserPath -Directory $bin -SubKey $subKey) 'present' 'present via variable and case'
    Assert-Equal $key.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) '%LOCALAPPDATA%\LocalX\Bin\' 'present leaves value alone'
    Assert-Equal $key.GetValueKind('Path') ([Microsoft.Win32.RegistryValueKind]::String) 'present keeps REG_SZ'

    # A REG_SZ value stays REG_SZ when appended to.
    $key.SetValue('Path', 'C:\only', [Microsoft.Win32.RegistryValueKind]::String)
    [void](Add-LocalXUserPath -Directory $bin -SubKey $subKey)
    Assert-Equal $key.GetValueKind('Path') ([Microsoft.Win32.RegistryValueKind]::String) 'append keeps REG_SZ'

    # The change notification loads and reports a boolean (a broadcast is harmless).
    Assert-Equal ((Send-LocalXEnvironmentChange) -is [bool]) $true 'environment broadcast result'

    # The setup handover: detected from --help, then run unredirected.
    $fake = Join-Path ([IO.Path]::GetTempPath()) "localx-setup-$([guid]::NewGuid().ToString('N'))"
    New-Item -ItemType Directory -Path $fake | Out-Null
    try {
        $withSetup = Join-Path $fake 'with.cmd'
        Set-Content -LiteralPath $withSetup -Encoding ascii -Value @(
            '@echo off',
            'if "%1"=="--help" (echo Commands:& echo   status   show versions& echo   setup    guided first run& exit /b 0)',
            'if "%1"=="setup" (echo visible setup output& exit /b 0)',
            'exit /b 2')
        $without = Join-Path $fake 'without.cmd'
        Set-Content -LiteralPath $without -Encoding ascii -Value @(
            '@echo off',
            'echo Commands:& echo   status   show versions& echo   setupper  not it',
            'exit /b 0')
        Assert-Equal (Test-LocalXSetup -Localx $withSetup) $true 'setup detected'
        Assert-Equal (Test-LocalXSetup -Localx $without) $false 'no setup on an older localx'
        & $withSetup setup
        Assert-Equal $LASTEXITCODE 0 'setup exit code read from LASTEXITCODE'
    }
    finally {
        Remove-Item -Recurse -Force $fake
    }

    # Every `<localx> setup` call in the installer is a bare statement: not
    # assigned, not piped, not inside a function's output. A captured call would
    # hand setup a pipe instead of the terminal and swallow its prompts.
    $setupCalls = $ast.FindAll({
            param($n)
            $n -is [System.Management.Automation.Language.CommandAst] -and
            $n.InvocationOperator -eq [System.Management.Automation.Language.TokenKind]::Ampersand -and
            @($n.CommandElements | Where-Object { $_ -is [System.Management.Automation.Language.StringConstantExpressionAst] -and $_.Value -eq 'setup' }).Count -gt 0
        }, $true)
    Assert-Equal @($setupCalls).Count 2 'setup handover sites (binary and source)'
    foreach ($call in $setupCalls) {
        $pipeline = $call.Parent
        $statementParent = $pipeline.Parent -is [System.Management.Automation.Language.StatementBlockAst] -or
            $pipeline.Parent -is [System.Management.Automation.Language.NamedBlockAst]
        $bare = ($pipeline -is [System.Management.Automation.Language.PipelineAst]) -and
            ($pipeline.PipelineElements.Count -eq 1) -and $statementParent -and
            ($call.Redirections.Count -eq 0)
        for ($p = $call.Parent; $null -ne $p; $p = $p.Parent) {
            if ($p -is [System.Management.Automation.Language.FunctionDefinitionAst]) { $bare = $false }
        }
        Assert-Equal $bare $true "setup call on line $($call.Extent.StartLineNumber) runs as a bare statement"
    }

    # The session PATH: prepended once, idempotent, and moved to the front when
    # it was already present further down.
    $savedPath = $env:PATH
    try {
        $env:PATH = 'C:\Windows\System32'
        Assert-Equal (Add-LocalXSessionPath -Directory $bin) 'added' 'session add'
        Assert-Equal $env:PATH "$bin;C:\Windows\System32" 'session order'
        Assert-Equal (Add-LocalXSessionPath -Directory $bin) 'present' 'session idempotent'
        $env:PATH = "C:\stale;$($bin.ToUpperInvariant())\;C:\Windows\System32"
        Assert-Equal (Add-LocalXSessionPath -Directory $bin) 'moved' 'session present later'
        Assert-Equal $env:PATH "$bin;C:\stale;C:\Windows\System32" 'session moved to front without a duplicate'
    }
    finally {
        $env:PATH = $savedPath
    }

    # Shadowing: a stale same-named program ahead of the LocalX copy.
    $scratch = Join-Path ([IO.Path]::GetTempPath()) "localx-shadow-$([guid]::NewGuid().ToString('N'))"
    $stale = Join-Path $scratch 'stale'
    $managed = Join-Path $scratch 'managed'
    New-Item -ItemType Directory -Path $stale, $managed | Out-Null
    try {
        Copy-Item (Join-Path $env:SystemRoot 'System32\where.exe') (Join-Path $stale 'localbox.exe')
        Copy-Item (Join-Path $env:SystemRoot 'System32\where.exe') (Join-Path $managed 'localbox.exe')
        Copy-Item (Join-Path $env:SystemRoot 'System32\where.exe') (Join-Path $managed 'localx.exe')
        Set-Content -LiteralPath (Join-Path $stale 'localx.cmd') -Value '@echo off'

        $shadows = @(Get-LocalXShadowing -Directory $managed -SearchPath "$stale;$managed")
        Assert-Equal $shadows.Count 2 'stale first shadows both'
        Assert-Equal (($shadows | ForEach-Object Tool) -join ',') 'localx,localbox' 'shadowed tools'
        Assert-Equal ($shadows | Where-Object Tool -eq 'localx').Shadow.ToLowerInvariant() (Join-Path $stale 'localx.cmd').ToLowerInvariant() 'a .cmd shadows too'
        Assert-Equal @(Get-LocalXShadowing -Directory $managed -SearchPath "$managed;$stale").Count 0 'managed first is not shadowed'

        # An entry on a drive that does not exist (a retired letter, a dropped
        # mapping) or with illegal characters is skipped, not fatal.
        $used = @(Get-PSDrive -PSProvider FileSystem | ForEach-Object { $_.Name.ToUpperInvariant() })
        $missing = @([char[]]'QRSTUVWXYZ' | Where-Object { $used -notcontains [string]$_ })[0]
        $broken = "${missing}:\retired\bin;C:\bad|<chars>;$stale;$managed"
        $shadows = @(Get-LocalXShadowing -Directory $managed -SearchPath $broken)
        Assert-Equal $shadows.Count 2 'missing drive and illegal entry ahead of a real candidate'

        # A new terminal puts the machine PATH before the user PATH, so a stale
        # copy on the machine PATH wins even though this window was repaired.
        $newShell = Get-LocalXNewShellPath -MachinePath "C:\Windows\System32;$stale" -UserPath "$managed"
        Assert-Equal $newShell "C:\Windows\System32;$stale;$managed" 'new-shell order is machine then user'
        Assert-Equal @(Get-LocalXShadowing -Directory $managed -SearchPath $newShell).Count 2 'machine-PATH copy shadows in new terminals'
        $newShell = Get-LocalXNewShellPath -MachinePath 'C:\Windows\System32' -UserPath "%TEMP%\nothing;$managed;$stale"
        Assert-Equal @(Get-LocalXShadowing -Directory $managed -SearchPath $newShell).Count 0 'user PATH copy after the LocalX entry is harmless'
    }
    finally {
        Remove-Item -Recurse -Force $scratch
    }
}
finally {
    [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($subKey, $false)
}

if ($failures) {
    foreach ($f in $failures) { Write-Host "FAIL: $f" }
    Write-Error "$($failures.Count) installer PATH check(s) failed."
}
Write-Host "ok: installer PATH functions pass against a scratch registry key."
