param(
    [ValidateRange(15, 300)][int]$Seconds = 15
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
if ($env:OS -ne 'Windows_NT') { throw 'MSI-Toolkit verification requires Windows.' }
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
try {
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Run verify-all.ps1 from a non-elevated PowerShell session.'
    }
} finally {
    $identity.Dispose()
}

function Find-Dumpbin {
    $command = Get-Command dumpbin.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($command) { return $command.Source }

    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path -LiteralPath $vswhere -PathType Leaf) {
        $paths = @(& $vswhere -products '*' -latest -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe')
        if ($LASTEXITCODE -eq 0 -and $paths.Count -gt 0) { return $paths[0] }
    }
    throw 'dumpbin.exe is required. Install the MSVC C++ build tools or run in their developer shell.'
}

function Get-PeImports([string]$Dumpbin, [string]$Path) {
    $lines = & $Dumpbin /nologo /imports $Path
    if ($LASTEXITCODE -ne 0) { throw "dumpbin failed for $Path (exit $LASTEXITCODE)" }

    $normal = [Collections.Generic.List[string]]::new()
    $delay = [Collections.Generic.List[string]]::new()
    $table = $null
    foreach ($line in $lines) {
        if ($line -match '^\s*Section contains the following imports:\s*$') {
            $table = $normal
        } elseif ($line -match '^\s*Section contains the following delay load imports:\s*$') {
            $table = $delay
        } elseif ($line -match '^\s*Summary\s*$') {
            break
        } elseif ($null -ne $table -and $line -match '^ {4}(\S+\.(?:dll|drv|ocx))\s*$') {
            $table.Add($Matches[1].ToLowerInvariant())
        }
    }
    if ($normal.Count -eq 0) { throw "Could not read the PE import table in $Path" }
    [pscustomobject]@{ Normal = $normal.ToArray(); Delay = $delay.ToArray() }
}

# GDI/GDI+, DWM and DirectWrite are part of the CPU rendering path.
$gpuModule = '^(?:(?:api|ext)-ms-win-(?:dx|d3d|d2d|opengl)|(?:lib)?(?:d2d|d3d|direct2d|direct3d|dxgi|dxcore|dcomp|opengl|egl|gles|vulkan|wgpu|webgpu|slint|skia|webview|nvml|nvapi|nvcuda|nvoglv)).*\.dll$'
$sizeBudget = [ordered]@{
    'msilt-daemon.exe' = 1280KB
    'msilt-cli.exe' = 512KB
    'msilt-probe.exe' = 1MB
    'msilt-setup.exe' = 1MB
    'msilt-ui.exe' = 12000000
}

$workspace = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$cargo = (Get-Command cargo.exe -CommandType Application -ErrorAction Stop).Source
$dumpbin = Find-Dumpbin
Push-Location -LiteralPath $workspace
try {
    $failures = @()
    Write-Host '[1/5] Workspace tests'
    & $cargo test --workspace --locked
    if ($LASTEXITCODE -ne 0) {
        $failures += "Workspace tests failed (exit $LASTEXITCODE)"
        Write-Warning $failures[-1]
    }

    Write-Host '[2/5] Release build and executable sizes'
    & $cargo build --release --locked -p msilt-daemon -p msilt-cli -p msilt-probe -p msilt-setup
    if ($LASTEXITCODE -ne 0) { throw "Backend Release build failed (exit $LASTEXITCODE)" }
    & $cargo build --release --locked -p msilt-ui
    $uiBuilt = $LASTEXITCODE -eq 0
    if (-not $uiBuilt) {
        $failures += "UI Release build failed (exit $LASTEXITCODE); UI size and imports were not verified"
        Write-Warning $failures[-1]
    }
    $metadata = & $cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw "Cargo metadata failed (exit $LASTEXITCODE)" }
    $target = ($metadata | ConvertFrom-Json).target_directory
    $release = Join-Path $target 'release'

    $binaries = foreach ($entry in $sizeBudget.GetEnumerator()) {
        if ($entry.Key -eq 'msilt-ui.exe' -and -not $uiBuilt) { continue }
        $binary = Get-Item -LiteralPath (Join-Path $release $entry.Key)
        if ($binary.Length -gt $entry.Value) {
            throw "$($entry.Key) is $($binary.Length) bytes, over the $($entry.Value)-byte budget."
        }
        [pscustomobject]@{ Name = $entry.Key; Bytes = $binary.Length; LimitBytes = $entry.Value; Path = $binary.FullName }
    }
    $binaries | Format-Table Name, Bytes, LimitBytes -AutoSize | Out-Host

    Write-Host '[3/5] GPU-free PE imports (normal and delay-load)'
    $audit = foreach ($binary in $binaries) {
        $imports = Get-PeImports $dumpbin $binary.Path
        $forbidden = @(($imports.Normal + $imports.Delay) | Where-Object { $_ -match $gpuModule })
        if ($forbidden.Count -gt 0) { throw "$($binary.Name) imports forbidden modules: $($forbidden -join ', ')" }
        [pscustomobject]@{
            Name = $binary.Name
            Normal = $imports.Normal.Count
            DelayLoad = $imports.Delay.Count
            Forbidden = $forbidden.Count
        }
    }
    $audit | Format-Table -AutoSize | Out-Host

    Write-Host '[4/5] Non-elevated probe (read-only, ACPI and NVML off)'
    $probeExe = Join-Path $release 'msilt-probe.exe'
    $probeJson = & $probeExe --json
    $probeExit = $LASTEXITCODE
    $probe = ($probeJson -join "`n") | ConvertFrom-Json
    if ($probeExit -ne 0) {
        throw "msilt-probe exited $probeExit with $(@($probe.diagnostics).Count) diagnostics."
    }
    if ($null -eq $probe -or $null -ne $probe.acpi -or $null -ne $probe.nvml -or $probe.ecMap.writesEnabled -ne $false) {
        throw 'Unexpected probe result: expected validated JSON with ACPI, NVML and EC writes off.'
    }
    Write-Host "Probe passed: board=$($probe.identity.boardId); EC writes disabled."

    Write-Host "[5/5] Daemon idle for $Seconds s (isolated read-only pipe)"
    $measure = Join-Path $workspace 'crates\msilt-daemon\tools\Measure-Readonly.ps1'
    $idle = & $measure -Workspace $workspace -TargetDirectory $target -Seconds $Seconds -PassThru
    $idle | Format-List IdleElapsedMs, WorkingSetBytes, WorkingSetMiB, PrivateBytes, PrivateMiB,
        Threads, Handles, CpuDeltaMs, ReconnectHelloMs, ReconnectedSnapshotMs, NvmlLoaded, NvapiLoaded,
        GracefulExitCode, RemainingOwnedProcesses | Out-Host
    $loaded = @($idle.LoadedModules | Where-Object { $_ -match $gpuModule })
    if ($loaded.Count -gt 0) { throw "Daemon loaded forbidden modules: $($loaded -join ', ')" }
    if ($idle.WorkingSetBytes -gt 12MB -or $idle.PrivateBytes -gt 4MB -or $idle.CpuDeltaMs -ne 0) {
        throw 'Daemon idle budget exceeded (Working Set <= 12 MiB, Private <= 4 MiB, CPU delta = 0 ms).'
    }
    Write-Host "Daemon idle budget passed. Benchmark JSON: $(Join-Path $target 'integration-benchmark.json')"
    if ($failures.Count -gt 0) { throw ($failures -join '; ') }
    Write-Host 'All checks passed.'
} finally {
    Pop-Location
}
