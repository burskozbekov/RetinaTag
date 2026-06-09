Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bitmap = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen(0, 0, 0, 0, $bitmap.Size)
$out = 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\after_install.png'
$bitmap.Save($out)
$graphics.Dispose()
$bitmap.Dispose()
Write-Host $out
