# Guard the Windows installer against the bug class that broke it once already:
# PowerShell variable names are case-insensitive, so a local `$binary` IS the
# `[switch]$Binary` parameter, and assigning anything but a boolean to it dies
# with a MetadataError at run time. Nothing in the build catches that -- the
# installer is a script, never compiled, and CI's install smoke test uses the
# cargo path -- so it only surfaced when a user piped it into `iex`.
#
# A `[switch]` parameter is a caller-supplied flag; the script has no reason to
# assign to one. Any assignment to a switch parameter name is therefore either
# this collision or a flag being silently overwritten. Both are bugs.
#
# Run:  pwsh -NoProfile -File install/lint-install-script.ps1
[CmdletBinding()]
param([string]$Path = (Join-Path $PSScriptRoot 'install.ps1'))
$ErrorActionPreference = 'Stop'

$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    (Resolve-Path -LiteralPath $Path).ProviderPath, [ref]$tokens, [ref]$errors)
if ($errors) {
    foreach ($e in $errors) { Write-Host "$Path`:$($e.Extent.StartLineNumber): $($e.Message)" }
    Write-Error "$Path does not parse."
}

$switchParams = @{}
foreach ($p in $ast.ParamBlock.Parameters) {
    $isSwitch = $p.StaticType -eq [System.Management.Automation.SwitchParameter]
    if ($isSwitch) { $switchParams[$p.Name.VariablePath.UserPath] = $p.Name.Extent.StartLineNumber }
}

$failures = @()
$assignments = $ast.FindAll({ param($n) $n -is [System.Management.Automation.Language.AssignmentStatementAst] }, $true)
foreach ($a in $assignments) {
    $left = $a.Left
    if ($left -isnot [System.Management.Automation.Language.VariableExpressionAst]) { continue }
    $name = $left.VariablePath.UserPath
    foreach ($switch in $switchParams.Keys) {
        if ($name -cne $switch -and $name -ieq $switch) {
            $failures += "$Path`:$($a.Extent.StartLineNumber): `$$name collides with the [switch] parameter `$$switch declared on line $($switchParams[$switch]) (PowerShell variable names are case-insensitive). Rename the local variable."
        }
        elseif ($name -ceq $switch) {
            $failures += "$Path`:$($a.Extent.StartLineNumber): assigns to the [switch] parameter `$$switch, overwriting what the caller passed."
        }
    }
}

if ($failures) {
    foreach ($f in $failures) { Write-Host $f }
    Write-Error "$Path has $($failures.Count) switch-parameter collision(s)."
}
Write-Host "ok: $Path parses and has no switch-parameter collisions ($($switchParams.Count) switch parameters checked)."
