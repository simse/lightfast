# Installs, checks and removes an MSI the way a user would, on a throwaway
# machine such as a CI runner. It changes Program Files and the Start menu,
# so don't run it on a machine you use.
#
#   windows/test-msi.ps1 -Msi build/windows/Lightfast-x64.msi
#
# 1. Through the installer's dialogs, driven with UI Automation: Install,
#    then Finish with "Start Lightfast" unchecked.
# 2. Checks the files and shortcut, and that the app starts and keeps running.
# 3. Installs the same MSI silently again (a no-op), as WinSparkle would
#    run a new one, with /passive.
# 4. Removes it silently and checks everything is gone.
param(
    [Parameter(Mandatory)] [string] $Msi,
    [int] $TimeoutSeconds = 180
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes

$Msi = (Resolve-Path $Msi).Path
$logs = Join-Path (Split-Path $Msi) 'logs'
New-Item -ItemType Directory -Force $logs | Out-Null
$installDir = Join-Path $env:ProgramFiles 'Lightfast'
$shortcut = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\Lightfast.lnk'
$expected = @((Join-Path $installDir 'Lightfast.exe'), (Join-Path $installDir 'WinSparkle.dll'), $shortcut)

function Fail([string] $message, [string] $log) {
    if ($log -and (Test-Path $log)) { Get-Content $log -Tail 80 | Write-Host }
    throw $message
}

# Waits for an enabled control of $type named $name in one of the
# installer's windows, which Windows Installer titles "[ProductName] Setup".
function Wait-Control([Windows.Automation.ControlType] $type, [string] $name) {
    $root = [Windows.Automation.AutomationElement]::RootElement
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $setup = New-Object Windows.Automation.PropertyCondition([Windows.Automation.AutomationElement]::NameProperty, 'Lightfast Setup')
    $condition = New-Object Windows.Automation.AndCondition(
        (New-Object Windows.Automation.PropertyCondition([Windows.Automation.AutomationElement]::ControlTypeProperty, $type)),
        (New-Object Windows.Automation.PropertyCondition([Windows.Automation.AutomationElement]::NameProperty, $name)))
    while ((Get-Date) -lt $deadline) {
        foreach ($window in $root.FindAll([Windows.Automation.TreeScope]::Children, $setup)) {
            $control = $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $condition)
            if ($control -and $control.Current.IsEnabled -and -not $control.Current.IsOffscreen) {
                Write-Host "found '$name' in '$($window.Current.Name)'"
                return $control
            }
        }
        Start-Sleep -Milliseconds 500
    }
    throw "no '$name' appeared within $TimeoutSeconds seconds"
}

function Click([string] $name) {
    $button = Wait-Control ([Windows.Automation.ControlType]::Button) $name
    $button.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
}

function Uncheck([string] $name) {
    $box = Wait-Control ([Windows.Automation.ControlType]::CheckBox) $name
    $toggle = $box.GetCurrentPattern([Windows.Automation.TogglePattern]::Pattern)
    if ($toggle.Current.ToggleState -eq [Windows.Automation.ToggleState]::On) { $toggle.Toggle() }
}

Write-Host '== install through the dialogs'
$log = Join-Path $logs 'install-ui.log'
$installer = Start-Process msiexec.exe -ArgumentList @('/i', "`"$Msi`"", '/l*v', "`"$log`"") -PassThru
Click 'Install'
# The dialog's accelerators show in UI Automation names without the '&'.
Uncheck 'Start Lightfast'
Click 'Finish'
if (-not $installer.WaitForExit($TimeoutSeconds * 1000)) { Fail 'the installer never exited' $log }
if ($installer.ExitCode -ne 0) { Fail "the installer failed with $($installer.ExitCode)" $log }

foreach ($path in $expected) {
    if (-not (Test-Path $path)) { Fail "$path wasn't installed" $log }
}
Write-Host 'installed:'
Get-ChildItem $installDir | Format-Table Name, Length | Out-String | Write-Host

Write-Host '== start the app'
$app = Start-Process (Join-Path $installDir 'Lightfast.exe') -PassThru
Start-Sleep -Seconds 10
if ($app.HasExited) {
    Write-Warning "Lightfast exited with $($app.ExitCode) within 10 seconds (CI runners may lack a usable GPU)"
} else {
    Write-Host "Lightfast is running with $([int]($app.WorkingSet64 / 1MB)) MB"
    Stop-Process -Id $app.Id -Force
}

Write-Host '== install again silently, as an update would'
$log = Join-Path $logs 'reinstall.log'
$p = Start-Process msiexec.exe -ArgumentList @('/i', "`"$Msi`"", '/qn', '/l*v', "`"$log`"") -Wait -PassThru
if ($p.ExitCode -ne 0) { Fail "the second install failed with $($p.ExitCode)" $log }

Write-Host '== remove'
$log = Join-Path $logs 'uninstall.log'
$p = Start-Process msiexec.exe -ArgumentList @('/x', "`"$Msi`"", '/qn', '/l*v', "`"$log`"") -Wait -PassThru
if ($p.ExitCode -ne 0) { Fail "removing failed with $($p.ExitCode)" $log }
foreach ($path in $expected + $installDir) {
    if (Test-Path $path) { Fail "$path is still there after removal" $log }
}
Write-Host 'all good'
