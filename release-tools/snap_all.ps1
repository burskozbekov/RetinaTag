Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$screens = [System.Windows.Forms.Screen]::AllScreens
Write-Host ("Found {0} screens" -f $screens.Count)
$i = 0
foreach ($s in $screens) {
    $b = $s.Bounds
    Write-Host ("Screen {0}: ({1},{2}) {3}x{4}" -f $i, $b.X, $b.Y, $b.Width, $b.Height)
    $bmp = New-Object System.Drawing.Bitmap($b.Width, $b.Height)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($b.X, $b.Y, 0, 0, $bmp.Size)
    $out = "C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\screen_$i.png"
    $bmp.Save($out)
    $g.Dispose()
    $bmp.Dispose()
    Write-Host "  -> $out"
    $i++
}
