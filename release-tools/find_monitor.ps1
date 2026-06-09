Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class W {
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder lpString, int nMaxCount);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);
}
"@
$h = [W]::GetForegroundWindow()
$sb = New-Object System.Text.StringBuilder 256
[W]::GetWindowText($h, $sb, $sb.Capacity) | Out-Null
$pid_ = 0
[void][W]::GetWindowThreadProcessId($h, [ref]$pid_)
$proc = Get-Process -Id $pid_ -ErrorAction SilentlyContinue
Write-Host ("Foreground HWND={0} title='{1}' PID={2} ProcessName={3}" -f $h, $sb.ToString(), $pid_, $proc.ProcessName)
