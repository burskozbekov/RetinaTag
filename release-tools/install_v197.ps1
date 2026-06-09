Get-Process retina-tag -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
$un = 'C:\Users\dede_\AppData\Local\RetinaTag\uninstall.exe'
if (Test-Path $un) { & $un /S; Start-Sleep -Seconds 5 }
$setup = 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\src-tauri\target\release\bundle\nsis\RetinaTag_1.5.97_x64-setup.exe'
& $setup /S
Start-Sleep -Seconds 8
$exe = 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
Write-Host ("Installed: {0}" -f (Get-Item $exe).VersionInfo.ProductVersion)
Start-Process $exe
Start-Sleep -Seconds 6
$p = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p) {
    $mb = [math]::Round($p.WorkingSet64/1MB, 1)
    Write-Host ("PID {0} Resp={1} Mem={2}MB Threads={3}" -f $p.Id, $p.Responding, $mb, $p.Threads.Count)
}
