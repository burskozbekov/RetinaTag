Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
# Capture top-left 800x800 region where DevTools sits
$bmp = New-Object System.Drawing.Bitmap(800, 800)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen(0, 0, 0, 0, $bmp.Size)
$bmp.Save('C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\devtools_zoom.png')
$g.Dispose()
$bmp.Dispose()
Write-Host 'Zoom saved'
