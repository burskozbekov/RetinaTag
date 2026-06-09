Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win32 {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
}
"@
$p = Get-Process retina-tag -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if ($p) {
    [Win32]::ShowWindow($p.MainWindowHandle, 9) | Out-Null  # SW_RESTORE
    Start-Sleep -Milliseconds 300
    [Win32]::SetForegroundWindow($p.MainWindowHandle) | Out-Null
    Start-Sleep -Seconds 2
    Write-Host ("Brought forward: PID {0} title='{1}'" -f $p.Id, $p.MainWindowTitle)
} else {
    Write-Host 'No window handle yet'
}
Start-Sleep -Seconds 1
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bitmap = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen(0, 0, 0, 0, $bitmap.Size)
$out = 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\after_install_front.png'
$bitmap.Save($out)
$graphics.Dispose()
$bitmap.Dispose()
Write-Host $out
