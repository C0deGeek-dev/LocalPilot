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

# The Visual C++ runtime offer, with every effect injected: nothing is
# downloaded, asked or run for real.
$vc = Join-Path ([System.IO.Path]::GetTempPath()) ("localx-vc-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $vc | Out-Null
# A throwaway self-signed certificate with the given common name, so the
# signer check runs against a real X509Certificate2.
function New-TestCertificate([string]$Subject) {
    $rsa = [System.Security.Cryptography.RSA]::Create(2048)
    $request = New-Object System.Security.Cryptography.X509Certificates.CertificateRequest($Subject, $rsa, [System.Security.Cryptography.HashAlgorithmName]::SHA256, [System.Security.Cryptography.RSASignaturePadding]::Pkcs1)
    return $request.CreateSelfSigned([DateTimeOffset]::UtcNow.AddDays(-1), [DateTimeOffset]::UtcNow.AddDays(1))
}
$microsoft = New-TestCertificate 'CN=Microsoft Corporation, O=Microsoft Corporation, L=Redmond, S=Washington, C=US'
$lookAlike = New-TestCertificate 'CN=Microsoft Corporation Fake, O=Microsoft Corporation'
$quoted = New-TestCertificate 'O=Someone, CN=Someone Else, OU=CN=Microsoft Corporation'
function New-TestSignature($Status, $Certificate) { [pscustomobject]@{ Status = $Status; SignerCertificate = $Certificate } }
try {
    Assert-Equal (Get-LocalXNativeSystemDirectory -Is64BitOperatingSystem $true -Is64BitProcess $false -WindowsDirectory 'C:\Windows') 'C:\Windows\Sysnative' '32-bit shell on 64-bit Windows'
    Assert-Equal (Get-LocalXNativeSystemDirectory -Is64BitOperatingSystem $true -Is64BitProcess $true -WindowsDirectory 'C:\Windows') 'C:\Windows\System32' '64-bit shell'
    Assert-Equal ((Get-LocalXMissingVCRuntime -SystemDirectory $vc) -join ',') 'vcruntime140.dll,vcruntime140_1.dll,msvcp140.dll' 'all runtime DLLs missing'

    Assert-Equal (Test-LocalXInteractive -UserInteractive $true -InputRedirected $false -CommandLine @('pwsh', '-NoProfile')) $true 'a console can be asked'
    Assert-Equal (Test-LocalXInteractive -UserInteractive $true -InputRedirected $false -CommandLine @('powershell', '-NonInteractive', '-File', 'x.ps1')) $false '-NonInteractive cannot be asked'
    Assert-Equal (Test-LocalXInteractive -UserInteractive $true -InputRedirected $false -CommandLine @('powershell', '-noni')) $false '-noni cannot be asked'
    Assert-Equal (Test-LocalXInteractive -UserInteractive $true -InputRedirected $true -CommandLine @('pwsh')) $false 'redirected input cannot be asked'
    Assert-Equal (Test-LocalXInteractive -UserInteractive $false -InputRedirected $false -CommandLine @('pwsh')) $false 'a service cannot be asked'

    $script:calls = @()
    $installDlls = { foreach ($dll in 'vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll') { Set-Content -LiteralPath (Join-Path $vc $dll) -Value '' } }
    $fake = @{
        SystemDirectory = $vc
        Architecture = 'AMD64'
        Interactive = $true
        Ask = { $script:calls += 'ask'; 'y' }
        Download = { param($Url, $File) $script:calls += "download $Url" }
        Hold = { param($File) $script:calls += 'hold'; New-Object System.IO.MemoryStream }
        GetSignature = { param($File) $script:calls += 'signature'; New-TestSignature 'Valid' $microsoft }
        Run = { param($File) $script:calls += 'run'; & $installDlls; 0 }
    }
    function Invoke-Offer([hashtable]$Overrides = @{}) {
        $script:calls = @()
        $arguments = $fake.Clone()
        foreach ($k in $Overrides.Keys) { $arguments[$k] = $Overrides[$k] }
        Request-LocalXVCRuntime @arguments 6>$null 3>$null
    }
    function Clear-Dlls { Remove-Item (Join-Path $vc '*.dll') -ErrorAction SilentlyContinue }

    Assert-Equal (Invoke-Offer @{ Architecture = 'ARM64' }) 'unsupported' 'ARM64 is not offered'
    Assert-Equal ($script:calls -join ',') '' 'ARM64 asks and downloads nothing'
    Assert-Equal (Invoke-Offer @{ Interactive = $false }) 'noninteractive' 'no one to ask'
    Assert-Equal ($script:calls -join ',') '' 'non-interactive asks and downloads nothing'
    Assert-Equal (Invoke-Offer @{ Ask = { $script:calls += 'ask'; throw 'PowerShell is in NonInteractive mode' } }) 'noninteractive' 'a prompt that cannot be shown'
    Assert-Equal ($script:calls -join ',') 'ask' 'a failed prompt downloads nothing'
    Assert-Equal (Invoke-Offer @{ Ask = { $script:calls += 'ask'; '' } }) 'declined' 'Enter means No'
    Assert-Equal ($script:calls -join ',') 'ask' 'a decline downloads nothing'
    Assert-Equal (Invoke-Offer @{ Ask = { $script:calls += 'ask'; 'yes please' } }) 'declined' 'only y or yes means Yes'
    Assert-Equal (Invoke-Offer @{ Interactive = $false; Consent = $true }) 'installed' '-InstallVCRuntime installs unattended'
    Assert-Equal ($script:calls -join ',') "download $(Get-LocalXVCRuntimeUrl),hold,signature,run" 'unattended install never asks; checks the held file before running it'
    Clear-Dlls

    Assert-Equal (Invoke-Offer @{ GetSignature = { param($File) $script:calls += 'signature'; New-TestSignature 'Valid' $lookAlike } }) 'untrusted' 'a look-alike signer'
    Assert-Equal (Invoke-Offer @{ GetSignature = { param($File) $script:calls += 'signature'; New-TestSignature 'Valid' $quoted } }) 'untrusted' 'Microsoft Corporation only inside another attribute'
    Assert-Equal (Invoke-Offer @{ GetSignature = { param($File) $script:calls += 'signature'; New-TestSignature 'HashMismatch' $microsoft } }) 'untrusted' 'a broken signature'
    Assert-Equal (Invoke-Offer @{ GetSignature = { param($File) $script:calls += 'signature'; New-TestSignature 'Valid' ([pscustomobject]@{ Subject = 'CN=Microsoft Corporation' }) } }) 'untrusted' 'a certificate that is not an X509Certificate2'
    Assert-Equal (($script:calls | Where-Object { $_ -eq 'run' }).Count) 0 'an untrusted download is never run'
    Assert-Equal (Invoke-Offer @{ Hold = { param($File) $script:calls += 'hold'; throw 'the file is in use' } }) 'error' 'a file that cannot be held'
    Assert-Equal (($script:calls | Where-Object { $_ -eq 'run' }).Count) 0 'a file that cannot be held is never run'

    Assert-Equal (Invoke-Offer @{ Run = { param($File) 1603 } }) 'failed' 'installer failure'
    Assert-Equal (Invoke-Offer @{ Run = { param($File) throw 'the operation was canceled by the user' } }) 'error' 'a declined permission prompt'
    Assert-Equal (Invoke-Offer @{ Run = { param($File) 0 } }) 'failed' 'success reported but the DLLs are still missing'
    Assert-Equal (Invoke-Offer @{ Run = { param($File) 3010 } }) 'restart' 'restart required stays visible while DLLs are missing'
    Assert-Equal (Invoke-Offer @{ Run = { param($File) 1638 } }) 'failed' 'another version registered, DLLs still missing'
    Assert-Equal (Invoke-Offer @{ Run = { param($File) & $installDlls; 1638 } }) 'registered' 'another version registered, DLLs present'
    Clear-Dlls
    Assert-Equal (Invoke-Offer @{ Run = { param($File) & $installDlls; 3010 } }) 'restart' 'installed, restart required'
    Clear-Dlls
    Assert-Equal (Invoke-Offer) 'installed' 'installed'
    Assert-Equal ($script:calls -join ',') "ask,download $(Get-LocalXVCRuntimeUrl),hold,signature,run" 'asks first, then downloads, holds, checks and runs'
    Assert-Equal (Invoke-Offer) 'present' 'nothing to do once present'
    Assert-Equal ($script:calls -join ',') '' 'present asks nothing'

    # The default file hold really denies writes and deletes until released.
    $held = Join-Path $vc 'held.exe'
    Set-Content -LiteralPath $held -Value 'checked bytes'
    $handle = [System.IO.File]::Open($held, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
    try {
        $writeDenied = $false
        try { [System.IO.File]::WriteAllText($held, 'swapped') } catch { $writeDenied = $true }
        $deleteDenied = $false
        try { [System.IO.File]::Delete($held) } catch { $deleteDenied = $true }
        Assert-Equal $writeDenied $true 'a held file cannot be rewritten'
        Assert-Equal $deleteDenied $true 'a held file cannot be deleted'
    }
    finally { $handle.Dispose() }
    if ((Get-Content -LiteralPath $held -Raw).Trim() -ne 'checked bytes') { $script:failures += 'held file changed' }

    Assert-Equal (Test-LocalXMicrosoftSignature -Signature $null) $false 'no signature'
    Assert-Equal (Test-LocalXMicrosoftSignature -Signature (New-TestSignature 'Valid' $microsoft)) $true 'Microsoft Corporation'
    Assert-Equal (Get-LocalXVCRuntimeOutcome -ExitCode 1641) 'restart' 'restart started by the installer'
    Assert-Equal (Get-LocalXMissingDllHint -ExitCode 1 -Program 'localx.exe') $null 'other exit codes keep their message'
    $hint = Get-LocalXMissingDllHint -ExitCode -1073741515 -Program 'localx.exe'
    if ($hint -notmatch 'could not find a DLL' -or $hint -notmatch 'vc_redist\.x64\.exe') { $script:failures += "missing-DLL hint: $hint" }

    # A real PowerShell started with -NonInteractive: the offer prints the link
    # and returns without asking or downloading.
    Clear-Dlls
    $probe = Join-Path $vc 'noninteractive.ps1'
    $definitions = ($functions | Where-Object { $_.Name -like '*VCRuntime*' -or $_.Name -in 'Get-LocalXNativeSystemDirectory', 'Test-LocalXInteractive', 'Test-LocalXMicrosoftSignature' } | ForEach-Object { $_.Extent.Text }) -join "`n"
    Set-Content -LiteralPath $probe -Value ($definitions + "`n" + "Request-LocalXVCRuntime -SystemDirectory '$vc' -Architecture AMD64 -Download { throw 'downloaded' } 6>`$null")
    $shell = (Get-Process -Id $PID).Path
    $result = & $shell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $probe
    Assert-Equal ("$result".Trim()) 'noninteractive' 'a -NonInteractive PowerShell is not asked'
}
finally {
    Remove-Item -Recurse -Force $vc
}

if ($failures) {
    foreach ($f in $failures) { Write-Host "FAIL: $f" }
    Write-Error "$($failures.Count) installer PATH check(s) failed."
}
Write-Host "ok: installer PATH functions pass against a scratch registry key."
