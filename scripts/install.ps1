#Requires -Version 5.1
<#
.SYNOPSIS
    MSI-Toolkit script installer (for portable ZIP / automated deployment).
    Copies binaries to C:\Program Files\MSI-Toolkit\, registers Windows Apps & Features
    uninstall metadata, registers elevated AtLogon task \MSI-Toolkit\Daemon (zero UAC on boot),
    creates Start Menu shortcut, starts daemon, and launches msilt-ui.exe.
#>
[CmdletBinding()]
param(
    [switch]$NoAutoStart,
    [switch]$PreserveAutoStart,
    [switch]$CreateDesktopShortcut,
    [switch]$NoLaunch,
    [string]$SourceDir = $PSScriptRoot
)

$ErrorActionPreference = 'Stop'
$SourceDir = $SourceDir.TrimEnd('\', '/')

# Self-elevate cleanly via UAC if not running as Administrator
$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-SourceDir', "`"$SourceDir`"")
    if ($NoAutoStart) { $argList += '-NoAutoStart' }
    if ($PreserveAutoStart) { $argList += '-PreserveAutoStart' }
    if ($CreateDesktopShortcut) { $argList += '-CreateDesktopShortcut' }
    if ($NoLaunch) { $argList += '-NoLaunch' }
    $proc = Start-Process -FilePath 'powershell.exe' -Verb RunAs -ArgumentList $argList -PassThru
    $null = $proc.Handle
    $proc.WaitForExit()
    if ($null -eq $proc.ExitCode) { exit 0 }
    exit $proc.ExitCode
}

$InstallDir = Join-Path $env:ProgramFiles 'MSI-Toolkit'
$Binaries = @('msilt-daemon.exe', 'msilt-ui.exe', 'msilt-cli.exe', 'msilt-probe.exe')

foreach ($bin in $Binaries) {
    $src = Join-Path $SourceDir $bin
    if (-not (Test-Path -LiteralPath $src -PathType Leaf)) {
        throw "Missing binary: $src (run scripts\package.ps1 first)"
    }
}

$enableAutoStart = -not $NoAutoStart
if ($PreserveAutoStart) {
    $existingTask = Get-ScheduledTask -TaskPath '\MSI-Toolkit\' -TaskName 'Daemon' -ErrorAction SilentlyContinue
    if ($null -ne $existingTask) {
        $enableAutoStart = ($existingTask.State -ne 'Disabled')
    }
}

Write-Host '[1/4] Stopping existing MSI-Toolkit processes...' -ForegroundColor Cyan
Get-Process -Name 'msilt-ui', 'msilt-daemon' -ErrorAction SilentlyContinue | ForEach-Object {
    $_.CloseMainWindow() | Out-Null
}
Start-Sleep -Milliseconds 700
Get-Process -Name 'msilt-ui', 'msilt-daemon' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

Write-Host "[2/4] Installing binaries to $InstallDir ..." -ForegroundColor Cyan
if (-not (Test-Path -LiteralPath $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}
$totalBytes = 0L
foreach ($bin in $Binaries) {
    $dst = Join-Path $InstallDir $bin
    Copy-Item -LiteralPath (Join-Path $SourceDir $bin) -Destination $dst -Force
    $totalBytes += (Get-Item -LiteralPath $dst).Length
}

$hasNativeUninst = Test-Path -LiteralPath (Join-Path $SourceDir 'uninstall.exe') -PathType Leaf
if ($hasNativeUninst) {
    $dst = Join-Path $InstallDir 'uninstall.exe'
    Copy-Item -LiteralPath (Join-Path $SourceDir 'uninstall.exe') -Destination $dst -Force
    $totalBytes += (Get-Item -LiteralPath $dst).Length
} else {
    foreach ($helper in @('uninstall.ps1', 'uninstall.cmd')) {
        $helperSrc = Join-Path $SourceDir $helper
        if (Test-Path -LiteralPath $helperSrc -PathType Leaf) {
            Copy-Item -LiteralPath $helperSrc -Destination (Join-Path $InstallDir $helper) -Force
        }
    }
}

Write-Host '[3/4] Registering Windows Apps entry, shortcuts & elevated daemon task...' -ForegroundColor Cyan
$uiExe = Join-Path $InstallDir 'msilt-ui.exe'
$version = '0.1.0'
try {
    $vi = (Get-Item -LiteralPath $uiExe).VersionInfo.ProductVersion
    if (-not [string]::IsNullOrWhiteSpace($vi)) { $version = $vi.Trim() }
} catch {}

$regPath = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\MSI-Toolkit'
New-Item -Path $regPath -Force | Out-Null
$uninstCmd = if ($hasNativeUninst) {
    "`"$(Join-Path $InstallDir 'uninstall.exe')`" --uninstall"
} else {
    "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$(Join-Path $InstallDir 'uninstall.ps1')`""
}
$quietUninstCmd = if ($hasNativeUninst) {
    "`"$(Join-Path $InstallDir 'uninstall.exe')`" --uninstall --quiet"
} else {
    $uninstCmd
}
Set-ItemProperty -Path $regPath -Name 'DisplayName' -Value 'MSI-Toolkit'
Set-ItemProperty -Path $regPath -Name 'DisplayVersion' -Value $version
Set-ItemProperty -Path $regPath -Name 'Publisher' -Value 'MSI-Toolkit'
Set-ItemProperty -Path $regPath -Name 'InstallLocation' -Value $InstallDir
Set-ItemProperty -Path $regPath -Name 'DisplayIcon' -Value "$uiExe,0"
Set-ItemProperty -Path $regPath -Name 'UninstallString' -Value $uninstCmd
Set-ItemProperty -Path $regPath -Name 'QuietUninstallString' -Value $quietUninstCmd
Set-ItemProperty -Path $regPath -Name 'URLInfoAbout' -Value 'https://github.com/pkeran/MSI-Toolkit'
Set-ItemProperty -Path $regPath -Name 'EstimatedSize' -Type DWord -Value ([int][Math]::Max(1, [Math]::Floor($totalBytes / 1024)))
Set-ItemProperty -Path $regPath -Name 'NoModify' -Type DWord -Value 1
Set-ItemProperty -Path $regPath -Name 'NoRepair' -Type DWord -Value 1

$startMenu = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\MSI-Toolkit.lnk'
$wshell = New-Object -ComObject WScript.Shell
$shortcut = $wshell.CreateShortcut($startMenu)
$shortcut.TargetPath = $uiExe
$shortcut.WorkingDirectory = $InstallDir
$shortcut.IconLocation = "$uiExe,0"
$shortcut.Description = 'MSI-Toolkit - Ultra-lightweight native hardware controller'
$shortcut.Save()

if ($CreateDesktopShortcut) {
    $desktopLnk = Join-Path ([Environment]::GetFolderPath('Desktop')) 'MSI-Toolkit.lnk'
    $dShortcut = $wshell.CreateShortcut($desktopLnk)
    $dShortcut.TargetPath = $uiExe
    $dShortcut.WorkingDirectory = $InstallDir
    $dShortcut.IconLocation = "$uiExe,0"
    $dShortcut.Description = 'MSI-Toolkit - Ultra-lightweight native hardware controller'
    $dShortcut.Save()
}

$daemonExe = Join-Path $InstallDir 'msilt-daemon.exe'
$daemonArgs = '--acpi --allow-platform-control --enable-ec-writes'
$currentUser = [Security.Principal.WindowsIdentity]::GetCurrent().Name

$action = New-ScheduledTaskAction -Execute $daemonExe -Argument $daemonArgs -WorkingDirectory $InstallDir
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $currentUser
$principalTask = New-ScheduledTaskPrincipal -UserId $currentUser -LogonType Interactive -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -MultipleInstances IgnoreNew `
    -RestartCount 3 `
    -RestartInterval (New-TimeSpan -Minutes 1)

Register-ScheduledTask `
    -TaskPath '\MSI-Toolkit\' `
    -TaskName 'Daemon' `
    -Action $action `
    -Trigger $trigger `
    -Principal $principalTask `
    -Settings $settings `
    -Force | Out-Null

if (-not $enableAutoStart) {
    Disable-ScheduledTask -TaskPath '\MSI-Toolkit\' -TaskName 'Daemon' | Out-Null
}
Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'MSIToolkit' -ErrorAction SilentlyContinue

Write-Host '[4/4] Starting daemon...' -ForegroundColor Cyan
if ($enableAutoStart) {
    Start-ScheduledTask -TaskPath '\MSI-Toolkit\' -TaskName 'Daemon'
} else {
    Start-Process -FilePath $daemonExe -ArgumentList $daemonArgs -WorkingDirectory $InstallDir -WindowStyle Hidden
}

if (-not $NoLaunch) {
    Start-Sleep -Milliseconds 500
    # Launch UI in user desktop session via Explorer so UI runs at Medium integrity
    Start-Process -FilePath 'explorer.exe' -ArgumentList "`"$uiExe`""
}

Write-Host "MSI-Toolkit v$version installed cleanly to $InstallDir." -ForegroundColor Green
