Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
# Capture region where RetinaTag window with overlay sits
# Window was at (992, 230) 1456x939 — capture overlay portion
$bmp = New-Object System.Drawing.Bitmap(1456, 350)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen(992, 230, 0, 0, $bmp.Size)
$bmp.Save('C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\overlay_zoom.png')
$g.Dispose()
$bmp.Dispose()
Write-Host done
