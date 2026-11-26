#Requires -Version 5.1
<#
.SYNOPSIS
    MSI-Toolkit clean script uninstaller.
    Stops daemon/UI, removes scheduled task, Run key, Start Menu & Desktop shortcuts,
    Windows Apps & Features entry, and C:\Program Files\MSI-Toolkit\.
    Never touches msiapcfg.dll or any system driver.
#>
[CmdletBinding()]
param(
    [switch]$PurgeConfig
)

$ErrorActionPreference = 'Stop'

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"")
    if ($PurgeConfig) { $argList += '-PurgeConfig' }
    $proc = Start-Process -FilePath 'powershell.exe' -Verb RunAs -ArgumentList $argList -PassThru
    $null = $proc.Handle
    $proc.WaitForExit()
    if ($null -eq $proc.ExitCode) { exit 0 }
    exit $proc.ExitCode
}

Write-Host '[1/3] Stopping MSI-Toolkit...' -ForegroundColor Cyan
Get-Process -Name 'msilt-ui', 'msilt-daemon' -ErrorAction SilentlyContinue | ForEach-Object {
    $_.CloseMainWindow() | Out-Null
}
Start-Sleep -Milliseconds 700
Get-Process -Name 'msilt-ui', 'msilt-daemon' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

Write-Host '[2/3] Removing startup task, shortcuts & Windows Apps entry...' -ForegroundColor Cyan
Unregister-ScheduledTask -TaskPath '\MSI-Toolkit\' -TaskName 'Daemon' -Confirm:$false -ErrorAction SilentlyContinue
try {
    $svc = New-Object -ComObject Schedule.Service
    $svc.Connect()
    $svc.GetFolder('\').DeleteFolder('MSI-Toolkit', 0)
} catch {}

Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'MSIToolkit' -ErrorAction SilentlyContinue
Remove-Item -LiteralPath (Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\MSI-Toolkit.lnk') -Force -ErrorAction SilentlyContinue
$desktopLnk = Join-Path ([Environment]::GetFolderPath('Desktop')) 'MSI-Toolkit.lnk'
Remove-Item -LiteralPath $desktopLnk -Force -ErrorAction SilentlyContinue
Remove-Item -Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\MSI-Toolkit' -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -Path 'HKLM:\SOFTWARE\MSI-Toolkit' -Recurse -Force -ErrorAction SilentlyContinue

Write-Host '[3/3] Removing installation directory...' -ForegroundColor Cyan
$InstallDir = Join-Path $env:ProgramFiles 'MSI-Toolkit'
Set-Location $env:TEMP
if (Test-Path -LiteralPath $InstallDir) {
    Remove-Item -LiteralPath $InstallDir -Recurse -Force -ErrorAction SilentlyContinue
}

if ($PurgeConfig) {
    $DataDir = Join-Path $env:LOCALAPPDATA 'MSI-Toolkit'
    if (Test-Path -LiteralPath $DataDir) {
        Remove-Item -LiteralPath $DataDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Host 'MSI-Toolkit uninstalled cleanly. System drivers and msiapcfg.dll were untouched.' -ForegroundColor Green
