param(
    [string]$Exe = "target\debug\msilt-ui.exe",
    [string]$Out = "$env:TEMP\msilt-ui-shot.png",
    [string]$AppArgs = "--demo",
    [int]$WaitMs = 3500
)

Add-Type -AssemblyName System.Drawing

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win32Rect {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")]
    public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
}
"@

$p = Start-Process -FilePath $Exe -ArgumentList $AppArgs -PassThru
Start-Sleep -Milliseconds $WaitMs
$p.Refresh()
if ($p.HasExited) {
    Write-Error "process exited early with code $($p.ExitCode)"
    exit 1
}

$hwnd = $p.MainWindowHandle
[void][Win32Rect]::ShowWindow($hwnd, 5)
[void][Win32Rect]::SetForegroundWindow($hwnd)
Start-Sleep -Milliseconds 700

$rect = New-Object Win32Rect+RECT
[void][Win32Rect]::GetWindowRect($hwnd, [ref]$rect)
$w = $rect.Right - $rect.Left
$h = $rect.Bottom - $rect.Top
Write-Host "window rect: $($rect.Left),$($rect.Top) ${w}x${h}"

$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($rect.Left, $rect.Top, 0, 0, (New-Object System.Drawing.Size $w, $h))
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
Write-Host "saved: $Out"

# close via WM_CLOSE and verify the process really goes away
[void]$p.CloseMainWindow()
Start-Sleep -Milliseconds 900
$still = Get-Process -Id $p.Id -ErrorAction SilentlyContinue
if ($still) {
    Write-Host "RESIDUAL PROCESS after close (force killing)"
    $still | Stop-Process -Force
    exit 2
}
Write-Host "closed cleanly (0 residual)"
exit 0
