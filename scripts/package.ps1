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
    [switch]$SkipBuild,
    [string]$BinaryDir,
    [string]$OutputDir
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

    if (-not $BinaryDir) { $BinaryDir = Join-Path $Root 'target\release' }
    if (-not $OutputDir) { $OutputDir = Join-Path $Root 'dist' }
    $OutputDir = [IO.Path]::GetFullPath($OutputDir)
    $DistDir = Join-Path $OutputDir 'MSI-Toolkit'
    $ZipPath = Join-Path $OutputDir 'MSI-Toolkit-win64.zip'
    $SetupPath = Join-Path $OutputDir 'MSI-Toolkit-Setup.exe'
    if (Test-Path -LiteralPath $ZipPath) { Remove-Item -LiteralPath $ZipPath -Force }
    if (Test-Path -LiteralPath $SetupPath) { Remove-Item -LiteralPath $SetupPath -Force }
    New-Item -ItemType Directory -Path $DistDir -Force | Out-Null

    Write-Host '[2/4] Staging portable release folder...' -ForegroundColor Cyan
    foreach ($bin in @('msilt-daemon.exe', 'msilt-ui.exe', 'msilt-cli.exe', 'msilt-probe.exe')) {
        Copy-Item -LiteralPath (Join-Path $BinaryDir $bin) -Destination (Join-Path $DistDir $bin) -Force
    }
    Copy-Item -LiteralPath (Join-Path $BinaryDir 'msilt-setup.exe') -Destination (Join-Path $DistDir 'uninstall.exe') -Force
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
    $iscc = @((Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'), (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe')) | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    if (-not $iscc) { throw 'Install Inno Setup 6: winget install --id JRSoftware.InnoSetup -e' }
    $translation = Join-Path $Root 'target\ChineseSimplified-6.7.3.isl'
    if (-not (Test-Path -LiteralPath $translation)) {
        Invoke-WebRequest 'https://raw.githubusercontent.com/jrsoftware/issrc/is-6_7_3/Files/Languages/Unofficial/ChineseSimplified.isl' -UseBasicParsing -OutFile $translation
    }
    if ((Get-FileHash -LiteralPath $translation).Hash -ne '7D544B9BB1D142CFA11F2E5D3CC8ABE2E55F8E066C5124E3772675AA236E1278') { throw 'Incorrect Inno Setup translation' }
    $version = (Get-Item -LiteralPath (Join-Path $DistDir 'msilt-ui.exe')).VersionInfo.ProductVersion
    if ($version -notmatch '^\d+\.\d+\.\d+$') { throw 'Invalid app version' }
    & $iscc /Q "/O$OutputDir" "/DPayload=$DistDir" "/DReleaseVersion=$version" "/DChineseMessages=$translation" (Join-Path $PSScriptRoot 'setup.iss')
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed (exit $LASTEXITCODE)" }
    if (-not (Test-Path -LiteralPath $SetupPath -PathType Leaf)) { throw "Expected packed installer missing: $SetupPath" }

    Write-Host '[4/4] Creating dist\MSI-Toolkit-win64.zip & dist\SHA256SUMS.txt...' -ForegroundColor Cyan
    $payload = 'msilt-daemon.exe','msilt-ui.exe','msilt-cli.exe','msilt-probe.exe','uninstall.exe','install.ps1','install.cmd','uninstall.ps1','uninstall.cmd','README.txt' | ForEach-Object { Join-Path $DistDir $_ }
    Compress-Archive -LiteralPath $payload -DestinationPath $ZipPath -CompressionLevel Optimal -Force

    $SumPath = Join-Path $OutputDir 'SHA256SUMS.txt'
    $setupHash = (Get-FileHash -LiteralPath $SetupPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $zipHash = (Get-FileHash -LiteralPath $ZipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    $version = (Get-Item -LiteralPath (Join-Path $DistDir 'msilt-ui.exe')).VersionInfo.ProductVersion
    if ($version -notmatch '^\d+\.\d+\.\d+$') { throw 'Missing release version in msilt-ui.exe' }
    [System.IO.File]::WriteAllText($SumPath, "# MSI-Toolkit v$version`n$setupHash  MSI-Toolkit-Setup.exe`n$zipHash  MSI-Toolkit-win64.zip`n", $utf8NoBom)

    $setupBytes = (Get-Item -LiteralPath $SetupPath).Length
    $zipBytes = (Get-Item -LiteralPath $ZipPath).Length
    $folderBytes = (Get-ChildItem -LiteralPath $DistDir -File | Measure-Object -Property Length -Sum).Sum
    Write-Host ("Release ready:`n  Setup EXE : {0} ({1:N2} MiB) SHA256={2}`n  ZIP Bundle: {3} ({4:N2} MiB, {5:N2} MiB unpacked) SHA256={6}" -f $SetupPath, ($setupBytes / 1MB), $setupHash, $ZipPath, ($zipBytes / 1MB), ($folderBytes / 1MB), $zipHash) -ForegroundColor Green
} finally {
    Pop-Location
}
