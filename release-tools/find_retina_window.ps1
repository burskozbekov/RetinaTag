Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class W {
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder lpString, int nMaxCount);
    [DllImport("user32.dll")] public static extern int GetWindowTextLength(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
}
"@

$retinaPid = (Get-Process retina-tag -ErrorAction SilentlyContinue | Select-Object -First 1).Id
Write-Host ("RetinaTag PID: {0}" -f $retinaPid)

$found = @()
$cb = {
    param($hWnd, $lParam)
    $procId = 0
    [void][W]::GetWindowThreadProcessId($hWnd, [ref]$procId)
    if ($procId -eq $retinaPid) {
        $len = [W]::GetWindowTextLength($hWnd)
        $sb = New-Object System.Text.StringBuilder ($len + 1)
        [W]::GetWindowText($hWnd, $sb, $sb.Capacity) | Out-Null
        $visible = [W]::IsWindowVisible($hWnd)
        $script:found += [PSCustomObject]@{HWnd=$hWnd; Title=$sb.ToString(); Visible=$visible}
    }
    return $true
}
$delegate = [W+EnumWindowsProc]$cb
[void][W]::EnumWindows($delegate, [IntPtr]::Zero)

Write-Host ("Found {0} windows for retina-tag PID" -f $found.Count)
foreach ($w in $found) {
    Write-Host ("  HWND={0} visible={1} title='{2}'" -f $w.HWnd, $w.Visible, $w.Title)
}

# Try to show the first non-empty titled window
$target = $found | Where-Object { $_.Title.Length -gt 0 } | Select-Object -First 1
if (-not $target) { $target = $found | Select-Object -First 1 }
if ($target) {
    Write-Host ("Restoring HWND {0}" -f $target.HWnd)
    [W]::ShowWindow($target.HWnd, 9) | Out-Null   # SW_RESTORE
    Start-Sleep -Milliseconds 300
    [W]::SetForegroundWindow($target.HWnd) | Out-Null
}
