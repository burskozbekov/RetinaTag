param([int]$x = 1129, [int]$y = 1137, [string]$label = 'Settings')
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
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint cButtons, IntPtr dwExtraInfo);
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
Start-Sleep -Milliseconds 600

$fg = [WinApi]::GetForegroundWindow()
Write-Host ("Foreground match: {0}" -f ($h -eq $fg))

Write-Host ("Clicking {0} @ ({1},{2})" -f $label, $x, $y)
[void][WinApi]::SetCursorPos($x, $y)
Start-Sleep -Milliseconds 100
[WinApi]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 30
[WinApi]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)

# Monitor 8s
$lastCpu = (Get-Process -Id $p.Id).CPU
for ($i = 0; $i -lt 16; $i++) {
    Start-Sleep -Milliseconds 500
    $p2 = Get-Process -Id $p.Id -ErrorAction SilentlyContinue
    if (-not $p2) { Write-Host "DEAD at $i"; break }
    $mb = [math]::Round($p2.WorkingSet64/1MB, 1)
    $dCpu = if ($p2.CPU -and $lastCpu) { [math]::Round(($p2.CPU - $lastCpu) * 2, 1) } else { 0 }
    $lastCpu = $p2.CPU
    Write-Host ("[t+{0,4:F1}s] Resp={1,-5} CPU%={2,5} Mem={3,5}MB Threads={4}" -f ($i*0.5+0.5), $p2.Responding, $dCpu, $mb, $p2.Threads.Count)
}

# Screenshot
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bitmap = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$g = [System.Drawing.Graphics]::FromImage($bitmap)
$g.CopyFromScreen(0, 0, 0, 0, $bitmap.Size)
$bitmap.Save("C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\click_${label}.png")
$g.Dispose()
$bitmap.Dispose()
