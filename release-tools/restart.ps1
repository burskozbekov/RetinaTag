Get-Process retina-tag -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 3
Start-Process 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
Start-Sleep -Seconds 12
$p = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p) { Write-Host ("Re-launched. PID {0}" -f $p.Id) }
