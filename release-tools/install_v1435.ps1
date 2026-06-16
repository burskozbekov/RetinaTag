$setup = 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\src-tauri\target\release\bundle\nsis\RetinaTag_1.5.435_x64-setup.exe'
if (-not (Test-Path $setup)) { Write-Host "SETUP MISSING - aborting, keeping current version"; exit 1 }
Get-Process retina-tag -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
$un = 'C:\Users\dede_\AppData\Local\RetinaTag\uninstall.exe'
if (Test-Path $un) { & $un /S; Start-Sleep -Seconds 6 }
& $setup /S
Start-Sleep -Seconds 10
Start-Process 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
Start-Sleep -Seconds 9
$p = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p) {
    $v = (Get-Item 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe').VersionInfo.ProductVersion
    Write-Host ("t+9s  PID {0} v{1} Resp={2}" -f $p.Id, $v, $p.Responding)
} else { Write-Host "t+9s  NOT RUNNING" }
Start-Sleep -Seconds 8
$p = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p) { Write-Host ("t+17s PID {0} Resp={1}" -f $p.Id, $p.Responding) } else { Write-Host "t+17s NOT RUNNING" }
