Add-Type @"
using System;
using System.Runtime.InteropServices;
public class W {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT r);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
}
"@
$p = Get-Process retina-tag -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
$r = New-Object W+RECT
[void][W]::GetWindowRect($p.MainWindowHandle, [ref]$r)

# Capture overlay region only (top 350 pixels of window) at FULL resolution
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$w = $r.Right - $r.Left
$h = 350
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size)
$bmp.Save('C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\overlay_hires.png')
$g.Dispose()
$bmp.Dispose()
Write-Host ("Captured {0}x{1}" -f $w, $h)
