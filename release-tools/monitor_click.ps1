$p = Get-Process retina-tag -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $p) { Write-Host 'DEAD'; exit }
$lastCpu = $p.CPU
Write-Host 'Monitoring 15s after click...'
for ($i = 0; $i -lt 30; $i++) {
    Start-Sleep -Milliseconds 500
    $p2 = Get-Process -Id $p.Id -ErrorAction SilentlyContinue
    if (-not $p2) { Write-Host "[t+$($i*0.5+0.5)s] DEAD"; break }
    $mb = [math]::Round($p2.WorkingSet64/1MB, 1)
    $dCpu = if ($p2.CPU -and $lastCpu) { [math]::Round(($p2.CPU - $lastCpu) * 2, 1) } else { 0 }
    $lastCpu = $p2.CPU
    Write-Host ("[t+{0,4:F1}s] Resp={1,-5} CPU%~={2,5} Mem={3,5}MB Threads={4}" -f ($i*0.5+0.5), $p2.Responding, $dCpu, $mb, $p2.Threads.Count)
}

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bitmap = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$g = [System.Drawing.Graphics]::FromImage($bitmap)
$g.CopyFromScreen(0, 0, 0, 0, $bitmap.Size)
$bitmap.Save('C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\after_settings_click.png')
$g.Dispose()
$bitmap.Dispose()
