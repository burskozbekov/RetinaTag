Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win32 {
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
if (-not $p) {
    Write-Host 'NO HANDLE'
    return
}
$h = $p.MainWindowHandle
# AttachThreadInput trick to force foreground
$fgWin = [Win32]::GetForegroundWindow()
$fgThreadId = 0
[void][Win32]::GetWindowThreadProcessId($fgWin, [ref]$fgThreadId)
$currentTid = [Win32]::GetCurrentThreadId()
[Win32]::AttachThreadInput($fgThreadId, $currentTid, $true) | Out-Null
[Win32]::ShowWindow($h, 9) | Out-Null   # SW_RESTORE
[Win32]::BringWindowToTop($h) | Out-Null
[Win32]::SetForegroundWindow($h) | Out-Null
[Win32]::AttachThreadInput($fgThreadId, $currentTid, $false) | Out-Null
Start-Sleep -Milliseconds 500
$fg = [Win32]::GetForegroundWindow()
Write-Host ("RetinaTag HWND={0} Foreground={1} Match={2}" -f $h, $fg, ($h -eq $fg))
