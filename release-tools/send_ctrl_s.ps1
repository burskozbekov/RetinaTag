Add-Type @"
using System;
using System.Runtime.InteropServices;
public class K {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool fAttach);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] public static extern void keybd_event(byte bVk, byte bScan, uint dwFlags, IntPtr dwExtraInfo);
}
"@

$p = Get-Process retina-tag -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
$h = $p.MainWindowHandle

$fgWin = [K]::GetForegroundWindow()
$fgThreadId = 0
[void][K]::GetWindowThreadProcessId($fgWin, [ref]$fgThreadId)
$currentTid = [K]::GetCurrentThreadId()
[K]::AttachThreadInput($fgThreadId, $currentTid, $true) | Out-Null
[K]::ShowWindow($h, 9) | Out-Null
[K]::BringWindowToTop($h) | Out-Null
[K]::SetForegroundWindow($h) | Out-Null
[K]::AttachThreadInput($fgThreadId, $currentTid, $false) | Out-Null
Start-Sleep -Milliseconds 800
$fg = [K]::GetForegroundWindow()
Write-Host ("Match: {0}" -f ($h -eq $fg))

# Ctrl+S via keybd_event (VK_CONTROL=0x11, VK_S=0x53)
[K]::keybd_event(0x11, 0, 0, [IntPtr]::Zero)  # Ctrl down
Start-Sleep -Milliseconds 30
[K]::keybd_event(0x53, 0, 0, [IntPtr]::Zero)  # S down
Start-Sleep -Milliseconds 30
[K]::keybd_event(0x53, 0, 2, [IntPtr]::Zero)  # S up (KEYEVENTF_KEYUP=2)
Start-Sleep -Milliseconds 30
[K]::keybd_event(0x11, 0, 2, [IntPtr]::Zero)  # Ctrl up
Write-Host 'Ctrl+S sent'

# Monitor 6s
$lastCpu = (Get-Process -Id $p.Id).CPU
for ($i = 0; $i -lt 12; $i++) {
    Start-Sleep -Milliseconds 500
    $p2 = Get-Process -Id $p.Id -ErrorAction SilentlyContinue
    if (-not $p2) { Write-Host 'DEAD'; break }
    $mb = [math]::Round($p2.WorkingSet64/1MB, 1)
    $dCpu = if ($p2.CPU -and $lastCpu) { [math]::Round(($p2.CPU - $lastCpu) * 2, 1) } else { 0 }
    $lastCpu = $p2.CPU
    Write-Host ("[t+{0,4:F1}s] Resp={1,-5} CPU%={2,5} Mem={3,5}MB Threads={4}" -f ($i*0.5+0.5), $p2.Responding, $dCpu, $mb, $p2.Threads.Count)
}

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bitmap = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$g = [System.Drawing.Graphics]::FromImage($bitmap)
$g.CopyFromScreen(0, 0, 0, 0, $bitmap.Size)
$bitmap.Save('C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\after_ctrl_s.png')
$g.Dispose()
$bitmap.Dispose()
