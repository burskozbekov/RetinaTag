Get-Process retina-tag -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
$un = 'C:\Users\dede_\AppData\Local\RetinaTag\uninstall.exe'
if (Test-Path $un) { & $un /S; Start-Sleep -Seconds 5 }
& 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\src-tauri\target\release\bundle\nsis\RetinaTag_1.5.106_x64-setup.exe' /S
Start-Sleep -Seconds 8
Start-Process 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
Write-Host 'Launched v1.5.106. Backfill scheduled 8s after start.'
Write-Host 'Waiting 120s for sweep on 63k photos...'
Start-Sleep -Seconds 120
$log = 'C:\Users\dede_\AppData\Roaming\com.retinatag.app\xmp_import.log'
if (Test-Path $log) {
    Write-Host '--- xmp_import.log ---'
    Get-Content $log
} else {
    Write-Host 'xmp_import.log NOT created yet — backfill may still be running or did not start'
}
