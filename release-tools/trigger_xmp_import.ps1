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

# Force foreground
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
Write-Host ("RetinaTag foreground: {0}" -f ([K]::GetForegroundWindow() -eq $h))

# Press F12 to open DevTools (VK_F12 = 0x7B)
[K]::keybd_event(0x7B, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 30
[K]::keybd_event(0x7B, 0, 2, [IntPtr]::Zero)
Write-Host 'F12 sent — waiting 3s for DevTools to open'
Start-Sleep -Seconds 3

# Put the invoke command into clipboard
$cmd = "doImportXmp()"
Set-Clipboard -Value $cmd
Write-Host "Clipboard set to: $cmd"

# DevTools focused — click into console prompt area (assume same coords as before)
# Use mouse_event to click at console prompt
Add-Type @"
using System.Runtime.InteropServices;
public class M {
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint cButtons, System.IntPtr dwExtraInfo);
}
"@
# Click somewhere into console (412, 706 from earlier snapshot)
[void][M]::SetCursorPos(412, 706)
Start-Sleep -Milliseconds 200
[M]::mouse_event(0x0002, 0, 0, 0, [System.IntPtr]::Zero)
Start-Sleep -Milliseconds 30
[M]::mouse_event(0x0004, 0, 0, 0, [System.IntPtr]::Zero)
Start-Sleep -Milliseconds 300

# Ctrl+V (VK_CONTROL=0x11, VK_V=0x56)
[K]::keybd_event(0x11, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 30
[K]::keybd_event(0x56, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 30
[K]::keybd_event(0x56, 0, 2, [IntPtr]::Zero)
[K]::keybd_event(0x11, 0, 2, [IntPtr]::Zero)
Start-Sleep -Milliseconds 200

# Enter
[K]::keybd_event(0x0D, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 30
[K]::keybd_event(0x0D, 0, 2, [IntPtr]::Zero)
Write-Host 'Pasted + Enter — backfill should now be running'
