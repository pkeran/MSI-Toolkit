param([string]$Exe = "target\debug\msilt-ui.exe")

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win32Min {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr hWnd, int x, int y, int w, int h, bool repaint);
}
"@

$p = Start-Process -FilePath $Exe -ArgumentList "--demo" -PassThru
Start-Sleep -Milliseconds 3500
$p.Refresh()
$hwnd = $p.MainWindowHandle
if ($hwnd -eq 0) { Write-Error "no window"; exit 1 }

$r = New-Object Win32Min+RECT
[void][Win32Min]::GetWindowRect($hwnd, [ref]$r)
Write-Host "initial: $($r.Right-$r.Left) x $($r.Bottom-$r.Top)"

# ask for something far below the documented 960x620 minimum
[void][Win32Min]::MoveWindow($hwnd, 200, 200, 500, 360, $true)
Start-Sleep -Milliseconds 600
[void][Win32Min]::GetWindowRect($hwnd, [ref]$r)
$w = $r.Right - $r.Left
$h = $r.Bottom - $r.Top
Write-Host "after shrink request (500x360): $w x $h"

[void]$p.CloseMainWindow()
Start-Sleep -Milliseconds 800
$still = Get-Process -Id $p.Id -ErrorAction SilentlyContinue
if ($still) { $still | Stop-Process -Force; Write-Host "RESIDUAL"; exit 2 }
Write-Host "closed cleanly"

# 960x620 logical at the current DPI, plus the invisible resize border (~8px each side)
$scale = [Math]::Round($w / 960.0, 2)
Write-Host "implied scale vs 960 logical: $scale"
if ($w -ge 950 -and $h -ge 610) { Write-Host "MIN SIZE OK"; exit 0 } else { Write-Host "MIN SIZE NOT ENFORCED"; exit 3 }
