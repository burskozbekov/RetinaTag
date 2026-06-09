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
Write-Host ("Foreground match (before): {0}" -f ($h -eq $fg))

# Use SendKeys for ctrl+s
Add-Type -AssemblyName System.Windows.Forms
[System.Windows.Forms.SendKeys]::SendWait('^s')
Write-Host 'Sent ^s'

# Hammer: 5x health checks over 10s to see if responsiveness drops
for ($i = 0; $i -lt 5; $i++) {
    Start-Sleep -Seconds 2
    $p2 = Get-Process -Id $p.Id -ErrorAction SilentlyContinue
    if ($p2) {
        $mb = [math]::Round($p2.WorkingSet64/1MB, 1)
        Write-Host ("[{0}s] PID {1} Responding={2} Mem={3}MB Threads={4}" -f (($i+1)*2), $p2.Id, $p2.Responding, $mb, $p2.Threads.Count)
    } else {
        Write-Host "[$($i*2)s] PROCESS DEAD"
        break
    }
}

# Final: check foreground (Settings modal should still be a RetinaTag window)
$fg2 = [WinApi]::GetForegroundWindow()
Write-Host ("Foreground match (after): {0}" -f ($h -eq $fg2))

# Now do the screenshot to see what's rendered
Add-Type -AssemblyName System.Drawing
$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bitmap = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen(0, 0, 0, 0, $bitmap.Size)
$out = 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\after_settings.png'
$bitmap.Save($out)
$graphics.Dispose()
$bitmap.Dispose()
Write-Host $out
