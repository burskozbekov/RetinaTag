Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinApi {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool fAttach);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint cButtons, IntPtr dwExtraInfo);
    [StructLayout(LayoutKind.Sequential)] public struct RECT {
        public int Left; public int Top; public int Right; public int Bottom;
    }
}
"@

$p = Get-Process retina-tag -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $p) { Write-Host 'NO HANDLE'; exit }
$h = $p.MainWindowHandle

# Force foreground
$fgWin = [WinApi]::GetForegroundWindow()
$fgThreadId = 0
[void][WinApi]::GetWindowThreadProcessId($fgWin, [ref]$fgThreadId)
$currentTid = [WinApi]::GetCurrentThreadId()
[WinApi]::AttachThreadInput($fgThreadId, $currentTid, $true) | Out-Null
[WinApi]::ShowWindow($h, 9) | Out-Null
[WinApi]::BringWindowToTop($h) | Out-Null
[WinApi]::SetForegroundWindow($h) | Out-Null
[WinApi]::AttachThreadInput($fgThreadId, $currentTid, $false) | Out-Null
Start-Sleep -Milliseconds 1000

$rect = New-Object WinApi+RECT
[void][WinApi]::GetWindowRect($h, [ref]$rect)
$wL = $rect.Left
$wT = $rect.Top
$wW = $rect.Right - $rect.Left
$wH = $rect.Bottom - $rect.Top
Write-Host ("Window: ({0},{1}) {2}x{3}" -f $wL, $wT, $wW, $wH)

# Baseline
Write-Host ''
Write-Host '=== Pre-click baseline (5 samples, 500ms apart) ==='
$lastCpu = $p.CPU
for ($i = 0; $i -lt 5; $i++) {
    Start-Sleep -Milliseconds 500
    $p2 = Get-Process -Id $p.Id -ErrorAction SilentlyContinue
    if (-not $p2) { Write-Host 'DEAD'; exit }
    $mb = [math]::Round($p2.WorkingSet64/1MB, 1)
    $dCpu = if ($p2.CPU -and $lastCpu) { [math]::Round(($p2.CPU - $lastCpu) * 2, 1) } else { 0 }
    $lastCpu = $p2.CPU
    Write-Host ("[base-{0}] Resp={1} CPU%~={2,4} Mem={3,5}MB Threads={4}" -f $i, $p2.Responding, $dCpu, $mb, $p2.Threads.Count)
}

# Click Settings button at bottom-left of sidebar (~168, 891 from top-left of window per earlier sweep)
$btnX = $wL + 168
$btnY = $wT + 891
Write-Host ''
Write-Host ("=== Clicking Settings at ({0},{1}) ===" -f $btnX, $btnY)
[void][WinApi]::SetCursorPos($btnX, $btnY)
Start-Sleep -Milliseconds 200
[WinApi]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 50
[WinApi]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)

# Post-click monitor: 30 samples × 500ms = 15s
Write-Host ''
Write-Host '=== Post-click monitor (30 samples × 500ms = 15s) ==='
$lastCpu = (Get-Process -Id $p.Id -ErrorAction SilentlyContinue).CPU
for ($i = 0; $i -lt 30; $i++) {
    Start-Sleep -Milliseconds 500
    $p2 = Get-Process -Id $p.Id -ErrorAction SilentlyContinue
    if (-not $p2) { Write-Host "[t+$($i*0.5)s] DEAD"; break }
    $mb = [math]::Round($p2.WorkingSet64/1MB, 1)
    $dCpu = if ($p2.CPU -and $lastCpu) { [math]::Round(($p2.CPU - $lastCpu) * 2, 1) } else { 0 }
    $lastCpu = $p2.CPU
    Write-Host ("[t+{0,4:F1}s] Resp={1,-5} CPU%~={2,5} Mem={3,5}MB Threads={4}" -f ($i*0.5+0.5), $p2.Responding, $dCpu, $mb, $p2.Threads.Count)
}

# Screenshot
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bitmap = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$g2 = [System.Drawing.Graphics]::FromImage($bitmap)
$g2.CopyFromScreen(0, 0, 0, 0, $bitmap.Size)
$out = 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\freeze_repro.png'
$bitmap.Save($out)
$g2.Dispose()
$bitmap.Dispose()
Write-Host ''
Write-Host "Screenshot: $out"
