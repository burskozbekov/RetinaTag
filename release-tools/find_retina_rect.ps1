Add-Type @"
using System;
using System.Runtime.InteropServices;
public class W {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT r);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
}
"@
$p = Get-Process retina-tag -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $p) { Write-Host 'NO'; exit }
$r = New-Object W+RECT
[void][W]::GetWindowRect($p.MainWindowHandle, [ref]$r)
Write-Host ("RECT: ({0},{1}) to ({2},{3}) size {4}x{5}" -f $r.Left, $r.Top, $r.Right, $r.Bottom, ($r.Right-$r.Left), ($r.Bottom-$r.Top))

# Capture just RetinaTag region — adjust for the overlay banner
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$w = $r.Right - $r.Left
$h = [math]::Min(450, $r.Bottom - $r.Top)
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size)
$bmp.Save('C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\overlay_zoom.png')
$g.Dispose()
$bmp.Dispose()
Write-Host done
