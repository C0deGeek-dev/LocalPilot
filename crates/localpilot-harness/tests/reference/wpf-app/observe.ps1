param([string]$ProcessName = 'Notes')
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$root = [System.Windows.Automation.AutomationElement]::RootElement
$deadline = (Get-Date).AddSeconds(30)
$window = $null
$process = $null
while ((Get-Date) -lt $deadline -and -not $window) {
    $process = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($process) {
        $byPid = New-Object System.Windows.Automation.PropertyCondition(
            [System.Windows.Automation.AutomationElement]::ProcessIdProperty, $process.Id)
        $window = $root.FindFirst([System.Windows.Automation.TreeScope]::Children, $byPid)
    }
    if (-not $window) { Start-Sleep -Milliseconds 300 }
}
if (-not $window) { Write-Output 'NOWINDOW'; exit 3 }
function Find-ById($id) {
    $cond = New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::AutomationIdProperty, $id)
    $window.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $cond)
}
$status = Find-ById 'status'
$before = $status.Current.Name
$save = Find-ById 'save'
$invoke = $save.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
$invoke.Invoke(); $invoke.Invoke()
Start-Sleep -Milliseconds 300
$after = (Find-ById 'status').Current.Name
Write-Output ("title=" + $window.Current.Name)
Write-Output ("pid=" + $process.Id)
Write-Output ("hwnd=" + $window.Current.NativeWindowHandle)
Write-Output ("before=" + $before)
Write-Output ("after=" + $after)
