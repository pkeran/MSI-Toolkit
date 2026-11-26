#Requires -Version 5.1
<#
.SYNOPSIS
    Builds release artifacts in dist\:
      - dist\MSI-Toolkit-Setup.exe   (single-file native Windows 11 installer/updater)
      - dist\MSI-Toolkit-win64.zip   (portable / script-deployable bundle)
      - dist\SHA256SUMS.txt          (SHA-256 checksums for both artifacts)
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
Push-Location $Root
try {
    if (-not $SkipBuild) {
        Write-Host '[1/4] Building release binaries...' -ForegroundColor Cyan
        cargo build --release -p msilt-daemon -p msilt-ui -p msilt-cli -p msilt-probe -p msilt-setup
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }

    $DistDir = Join-Path $Root 'dist\MSI-Toolkit'
    $ZipPath = Join-Path $Root 'dist\MSI-Toolkit-win64.zip'
    $SetupPath = Join-Path $Root 'dist\MSI-Toolkit-Setup.exe'
    if (Test-Path -LiteralPath $DistDir) { Remove-Item -LiteralPath $DistDir -Recurse -Force }
    if (Test-Path -LiteralPath $ZipPath) { Remove-Item -LiteralPath $ZipPath -Force }
    if (Test-Path -LiteralPath $SetupPath) { Remove-Item -LiteralPath $SetupPath -Force }
    New-Item -ItemType Directory -Path $DistDir -Force | Out-Null

    Write-Host '[2/4] Staging portable release folder...' -ForegroundColor Cyan
    foreach ($bin in @('msilt-daemon.exe', 'msilt-ui.exe', 'msilt-cli.exe', 'msilt-probe.exe')) {
        Copy-Item -LiteralPath (Join-Path $Root "target\release\$bin") -Destination (Join-Path $DistDir $bin) -Force
    }
    Copy-Item -LiteralPath (Join-Path $Root 'target\release\msilt-setup.exe') -Destination (Join-Path $DistDir 'uninstall.exe') -Force
    Copy-Item -LiteralPath (Join-Path $Root 'scripts\install.ps1') -Destination (Join-Path $DistDir 'install.ps1') -Force
    Copy-Item -LiteralPath (Join-Path $Root 'scripts\uninstall.ps1') -Destination (Join-Path $DistDir 'uninstall.ps1') -Force

    $installCmd = "@echo off`r`nsetlocal`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -File `"%~dp0install.ps1`" -SourceDir `"%~dp0.`"`r`nif %errorlevel% neq 0 (`r`n    echo [ERROR] MSI-Toolkit installation failed ^(code %errorlevel%^).`r`n    pause`r`n    exit /b %errorlevel%`r`n)`r`necho [OK] MSI-Toolkit installed and launched.`r`ntimeout /t 2 /nobreak >nul`r`n"
    $uninstallCmd = "@echo off`r`nsetlocal`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -File `"%~dp0uninstall.ps1`"`r`nif %errorlevel% neq 0 (`r`n    echo [ERROR] MSI-Toolkit uninstallation failed ^(code %errorlevel%^).`r`n    pause`r`n    exit /b %errorlevel%`r`n)`r`necho [OK] MSI-Toolkit uninstalled cleanly.`r`ntimeout /t 2 /nobreak >nul`r`n"
    Set-Content -LiteralPath (Join-Path $DistDir 'install.cmd') -Encoding ASCII -Value $installCmd
    Set-Content -LiteralPath (Join-Path $DistDir 'uninstall.cmd') -Encoding ASCII -Value $uninstallCmd

    $readme = @"
MSI-Toolkit (Windows 11 Native Release Bundle)
==============================================
- Recommended: Double-click MSI-Toolkit-Setup.exe for the native 1-click GUI installer & updater.
- Script install: Double-click install.cmd to install to C:\Program Files\MSI-Toolkit\ and register the zero-UAC logon task.
- Uninstall: Use Windows Settings -> Apps -> MSI-Toolkit, or run uninstall.exe / uninstall.cmd.
"@
    Set-Content -LiteralPath (Join-Path $DistDir 'README.txt') -Encoding UTF8 -Value $readme

    Write-Host '[3/4] Packing single-file dist\MSI-Toolkit-Setup.exe...' -ForegroundColor Cyan
    $setupStub = Join-Path $Root 'target\release\msilt-setup.exe'
    $packProc = Start-Process -FilePath $setupStub -ArgumentList @('--pack', "`"$DistDir`"", "`"$SetupPath`"") -PassThru
    $null = $packProc.Handle
    $packProc.WaitForExit()
    if ($packProc.ExitCode -ne 0) { throw "msilt-setup --pack failed (exit $($packProc.ExitCode))" }
    if (-not (Test-Path -LiteralPath $SetupPath -PathType Leaf)) { throw "Expected packed installer missing: $SetupPath" }

    Write-Host '[4/4] Creating dist\MSI-Toolkit-win64.zip & dist\SHA256SUMS.txt...' -ForegroundColor Cyan
    Compress-Archive -Path "$DistDir\*" -DestinationPath $ZipPath -CompressionLevel Optimal -Force

    $SumPath = Join-Path $Root 'dist\SHA256SUMS.txt'
    $setupHash = (Get-FileHash -LiteralPath $SetupPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $zipHash = (Get-FileHash -LiteralPath $ZipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($SumPath, "$setupHash  MSI-Toolkit-Setup.exe`n$zipHash  MSI-Toolkit-win64.zip`n", $utf8NoBom)

    $setupBytes = (Get-Item -LiteralPath $SetupPath).Length
    $zipBytes = (Get-Item -LiteralPath $ZipPath).Length
    $folderBytes = (Get-ChildItem -LiteralPath $DistDir -File | Measure-Object -Property Length -Sum).Sum
    Write-Host ("Release ready:`n  Setup EXE : {0} ({1:N2} MiB) SHA256={2}`n  ZIP Bundle: {3} ({4:N2} MiB, {5:N2} MiB unpacked) SHA256={6}" -f $SetupPath, ($setupBytes / 1MB), $setupHash, $ZipPath, ($zipBytes / 1MB), ($folderBytes / 1MB), $zipHash) -ForegroundColor Green
} finally {
    Pop-Location
}
