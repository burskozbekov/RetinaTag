Get-Process retina-tag -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
$un = 'C:\Users\dede_\AppData\Local\RetinaTag\uninstall.exe'
if (Test-Path $un) { & $un /S; Start-Sleep -Seconds 5 }
& 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\src-tauri\target\release\bundle\nsis\RetinaTag_1.5.105_x64-setup.exe' /S
Start-Sleep -Seconds 8
Start-Process 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
Write-Host 'Launched. Now waiting 90s for auto XMP import to run on 63k photos...'
Start-Sleep -Seconds 90
$p = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p) { Write-Host ("Still running PID {0} Resp={1}" -f $p.Id, $p.Responding) }
