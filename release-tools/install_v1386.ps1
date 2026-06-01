$setup = 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\src-tauri\target\release\bundle\nsis\RetinaTag_1.5.386_x64-setup.exe'
# Guard: never uninstall the working version unless the new installer exists.
if (-not (Test-Path $setup)) { Write-Host "SETUP MISSING - aborting, keeping current version"; exit 1 }
Get-Process retina-tag -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
$un = 'C:\Users\dede_\AppData\Local\RetinaTag\uninstall.exe'
if (Test-Path $un) { & $un /S; Start-Sleep -Seconds 6 }
& $setup /S
Start-Sleep -Seconds 10
# v1.5.386 — seed last_rescan_at so the gated launch-rescan SKIPS on this first
# launch (app is not running yet, so the DB write is uncontended). Result: no
# "Scanning..." at all after this update; the library is already fully imported.
try { python 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\seed_last_rescan.py' } catch { Write-Host ("seed skipped: " + $_) }
Start-Process 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
Start-Sleep -Seconds 8
$p = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p) {
    $v = (Get-Item 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe').VersionInfo.ProductVersion
    Write-Host ("PID {0} v{1} Resp={2}" -f $p.Id, $v, $p.Responding)
}
