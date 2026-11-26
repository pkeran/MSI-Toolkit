param(
    [string]$Workspace = (Join-Path $PSScriptRoot '..\..\..'),
    [ValidateRange(15, 300)][int]$Seconds = 15,
    [string]$TargetDirectory,
    [switch]$PassThru
)

$ErrorActionPreference = 'Stop'
$taskRoot = (Resolve-Path -LiteralPath $Workspace -ErrorAction Stop).Path
if (-not $TargetDirectory) { $TargetDirectory = Join-Path $taskRoot 'target' }
$taskTarget = (Resolve-Path -LiteralPath $TargetDirectory).Path
$taskExe = (Resolve-Path -LiteralPath (Join-Path $taskTarget 'release\msilt-daemon.exe')).Path
$taskPipe = 'MSIToolkit-bench-' + [guid]::NewGuid().ToString('N')
$taskProcess = $null
$taskConnection = $null
$settleSeconds = 3

if (-not ('Msilt.IntegrationBench' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace Msilt {
    public static class IntegrationBench {
        private delegate bool Callback(IntPtr window, IntPtr parameter);
        [DllImport("user32.dll")] private static extern bool EnumWindows(Callback callback, IntPtr parameter);
        [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
        [DllImport("user32.dll")] private static extern bool PostMessageW(IntPtr window, uint message, IntPtr w, IntPtr l);
        [DllImport("kernel32.dll", SetLastError=true)] public static extern bool GetNamedPipeServerProcessId(Microsoft.Win32.SafeHandles.SafePipeHandle pipe, out uint process);
        public static int Close(uint process) {
            int count = 0;
            EnumWindows((window, parameter) => {
                uint owner;
                GetWindowThreadProcessId(window, out owner);
                if (owner == process && PostMessageW(window, 0x10, IntPtr.Zero, IntPtr.Zero)) count++;
                return true;
            }, IntPtr.Zero);
            return count;
        }
    }
}
'@
}

function Connect-BenchPipe([string]$Name, [Diagnostics.Process]$Process) {
    $pipe = [IO.Pipes.NamedPipeClientStream]::new('.', $Name, [IO.Pipes.PipeDirection]::InOut, [IO.Pipes.PipeOptions]::Asynchronous, [Security.Principal.TokenImpersonationLevel]::Identification)
    try {
        $pipe.Connect(10000)
        [uint32]$serverPid = 0
        if (-not [Msilt.IntegrationBench]::GetNamedPipeServerProcessId($pipe.SafePipeHandle, [ref]$serverPid) -or $serverPid -ne $Process.Id) {
            throw 'The connected endpoint is not the spawned benchmark daemon'
        }
        return $pipe
    } catch {
        $pipe.Dispose()
        throw
    }
}

function Read-Exact([System.IO.Stream]$Stream, [byte[]]$Bytes) {
    $offset = 0
    while ($offset -lt $Bytes.Length) {
        $pending = $Stream.BeginRead($Bytes, $offset, $Bytes.Length - $offset, $null, $null)
        try {
            if (-not $pending.AsyncWaitHandle.WaitOne(10000)) {
                $Stream.Dispose()
                throw 'IPC response exceeded 10 seconds'
            }
            $read = $Stream.EndRead($pending)
            if ($read -eq 0) { throw 'IPC connection closed before the frame completed' }
            $offset += $read
        } finally {
            $pending.AsyncWaitHandle.Dispose()
        }
    }
}

function Invoke-Request([System.IO.Stream]$Stream, [int]$Id, [hashtable]$Command) {
    $json = @{ type = 'request'; message = @{ id = $Id; ttlMs = 10000; command = $Command } } | ConvertTo-Json -Compress -Depth 12
    $bytes = [Text.Encoding]::UTF8.GetBytes($json)
    $prefix = [BitConverter]::GetBytes([uint32]$bytes.Length)
    $Stream.Write($prefix, 0, 4)
    $Stream.Write($bytes, 0, $bytes.Length)
    $Stream.Flush()
    $header = [byte[]]::new(4)
    Read-Exact $Stream $header
    $length = [BitConverter]::ToUInt32($header, 0)
    if ($length -lt 1 -or $length -gt 65536) { throw 'Invalid IPC frame length' }
    $payload = [byte[]]::new($length)
    Read-Exact $Stream $payload
    $response = [Text.Encoding]::UTF8.GetString($payload) | ConvertFrom-Json
    if ($response.type -ne 'response' -or $response.message.id -ne $Id -or $response.message.result.status -ne 'ok') {
        throw "IPC request failed: $([Text.Encoding]::UTF8.GetString($payload))"
    }
    return $response.message.result.reply
}

try {
    if (-not $PassThru) { "Command: `"$taskExe`" --no-tray --readonly --pipe $taskPipe" }
    $taskProcess = Start-Process -FilePath $taskExe -ArgumentList @('--no-tray', '--readonly', '--pipe', $taskPipe) -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $taskTarget 'integration-bench-stdout.log') -RedirectStandardError (Join-Path $taskTarget 'integration-bench-stderr.log')
    $null = $taskProcess.Handle
    $taskConnection = Connect-BenchPipe $taskPipe $taskProcess
    $hello = Invoke-Request $taskConnection 1 @{ op = 'hello'; version = 1; client = 'cli' }
    if ($hello.snapshot.status -ne 'ready') { throw "Hardware initialization: $($hello.snapshot.status)" }
    $snapshot = (Invoke-Request $taskConnection 2 @{ op = 'getSnapshot' }).snapshot
    if ($snapshot.status -ne 'ready' -or $snapshot.writable -ne 0) { throw 'Expected an initialized read-only snapshot' }
    $config = Invoke-Request $taskConnection 3 @{ op = 'getConfig' }
    $initialization = [pscustomobject]@{
        Initialized = $snapshot.status
        BoardId = $snapshot.hardware.identity.boardId
        RefreshHz = $snapshot.hardware.displayRefreshHz
        BrightnessPercent = $snapshot.hardware.displayBrightnessPercent
        BatteryPercent = $snapshot.hardware.batteryPercent
        ColorProfile = $snapshot.hardware.colorProfileId
        ColorChoices = $snapshot.hardware.supportedColorProfiles.Count
        Writable = $snapshot.writable
        EcWriteStatus = $snapshot.ecWriteStatus
        ConfigRevision = $config.revision
    }
    if (-not $PassThru) { $initialization | Format-List | Out-String }
    $taskConnection.Dispose()
    $taskConnection = $null
    # Measure steady-state CPU after the daemon's one-shot 2 s cooldown has elapsed.
    Start-Sleep -Seconds $settleSeconds
    $taskProcess.Refresh()
    $cpuStart = $taskProcess.TotalProcessorTime.TotalMilliseconds
    $clock = [Diagnostics.Stopwatch]::StartNew()
    Start-Sleep -Seconds $Seconds
    $clock.Stop()
    $taskProcess.Refresh()
    $cpuEnd = $taskProcess.TotalProcessorTime.TotalMilliseconds
    $modules = @($taskProcess.Modules | ForEach-Object { $_.ModuleName.ToLowerInvariant() } | Sort-Object)
    $result = [ordered]@{
        MeasuredAt = [DateTimeOffset]::Now.ToString('o')
        ProcessId = $taskProcess.Id
        SettleSeconds = $settleSeconds
        IdleElapsedMs = $clock.Elapsed.TotalMilliseconds
        WorkingSetBytes = $taskProcess.WorkingSet64
        WorkingSetMiB = $taskProcess.WorkingSet64 / 1MB
        PrivateBytes = $taskProcess.PrivateMemorySize64
        PrivateMiB = $taskProcess.PrivateMemorySize64 / 1MB
        Threads = $taskProcess.Threads.Count
        Handles = $taskProcess.HandleCount
        CpuStartMs = $cpuStart
        CpuEndMs = $cpuEnd
        CpuDeltaMs = $cpuEnd - $cpuStart
        NvmlLoaded = $modules -contains 'nvml.dll'
        NvapiLoaded = @($modules | Where-Object { $_ -like 'nvapi*' }).Count -ne 0
        Initialization = $initialization
        LoadedModules = $modules
        BaselinePrivateMiB = 790
        BaselineProcesses = 15
        DaemonProcesses = 1
        PrivateReductionPercent = 100 * (1 - $taskProcess.PrivateMemorySize64 / (790 * 1MB))
    }

    $resume = [Diagnostics.Stopwatch]::StartNew()
    $taskConnection = Connect-BenchPipe $taskPipe $taskProcess
    $hello = Invoke-Request $taskConnection 1 @{ op = 'hello'; version = 1; client = 'cli' }
    $resume.Stop()
    $result.ReconnectHelloMs = $resume.Elapsed.TotalMilliseconds
    if ($hello.snapshot.status -ne 'ready' -or $hello.snapshot.writable -ne 0) { throw 'The daemon did not return a ready read-only snapshot after idle' }
    $resume.Restart()
    $reconnected = (Invoke-Request $taskConnection 2 @{ op = 'getSnapshot' }).snapshot
    $resume.Stop()
    $result.ReconnectedSnapshotMs = $resume.Elapsed.TotalMilliseconds
    if ($reconnected.status -ne 'ready' -or $reconnected.writable -ne 0) { throw 'The snapshot was not ready and read-only after reconnecting' }
    $taskConnection.Dispose()
    $taskConnection = $null

    $closed = [Msilt.IntegrationBench]::Close([uint32]$taskProcess.Id)
    if (-not $taskProcess.WaitForExit(8000)) { throw 'Daemon did not stop after WM_CLOSE' }
    $taskProcess.WaitForExit()
    $remaining = @(Get-Process -Id $taskProcess.Id -ErrorAction SilentlyContinue).Count
    if ($taskProcess.ExitCode -ne 0 -or $remaining -ne 0) {
        throw "Daemon shutdown check failed (exitCode=$($taskProcess.ExitCode), remaining=$remaining)"
    }
    $result.GracefulExitCode = $taskProcess.ExitCode
    $result.RemainingOwnedProcesses = $remaining
    $result = [pscustomobject]$result
    $json = $result | ConvertTo-Json -Depth 5
    $json | Set-Content -LiteralPath (Join-Path $taskTarget 'integration-benchmark.json') -Encoding utf8
    if ($PassThru) {
        $result
    } else {
        $json
        "Shutdown: WM_CLOSE windows=$closed; exitCode=$($taskProcess.ExitCode); remainingOwnedProcesses=$remaining"
    }
} finally {
    if ($null -ne $taskConnection) { $taskConnection.Dispose() }
    if ($null -ne $taskProcess -and -not $taskProcess.HasExited) {
        $null = [Msilt.IntegrationBench]::Close([uint32]$taskProcess.Id)
        if (-not $taskProcess.WaitForExit(8000)) {
            $taskProcess.Kill()
            $taskProcess.WaitForExit()
        }
    }
    if ($null -ne $taskProcess) { $taskProcess.Dispose() }
}
