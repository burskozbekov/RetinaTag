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

# F12 = 0x7B
[K]::keybd_event(0x7B, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 30
[K]::keybd_event(0x7B, 0, 2, [IntPtr]::Zero)
Write-Host 'F12 sent'

Start-Sleep -Seconds 2

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bitmap = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$g = [System.Drawing.Graphics]::FromImage($bitmap)
$g.CopyFromScreen(0, 0, 0, 0, $bitmap.Size)
$bitmap.Save('C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\after_f12.png')
$g.Dispose()
$bitmap.Dispose()
Write-Host 'Screenshot saved'
