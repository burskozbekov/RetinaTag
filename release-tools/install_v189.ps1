Write-Host '=== Killing current process ===' -ForegroundColor Yellow
Get-Process retina-tag -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host ("Killing PID {0}" -f $_.Id)
    Stop-Process -Id $_.Id -Force
}
Start-Sleep -Seconds 2

Write-Host ''
Write-Host '=== Uninstalling v1.5.88 ===' -ForegroundColor Yellow
$un = 'C:\Users\dede_\AppData\Local\RetinaTag\uninstall.exe'
if (Test-Path $un) {
    & $un /S
    Start-Sleep -Seconds 5
    Write-Host 'Uninstall ran'
}
if (Test-Path 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe') {
    Write-Host 'WARN: retina-tag.exe still present after uninstall'
} else {
    Write-Host 'Install dir cleaned'
}

Write-Host ''
Write-Host '=== Library DB check ===' -ForegroundColor Yellow
$dbPath = 'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
if (Test-Path $dbPath) {
    $f = Get-Item $dbPath
    $mb = [math]::Round($f.Length/1MB, 1)
    Write-Host ("Library DB OK: {0} MB" -f $mb)
}

Write-Host ''
Write-Host '=== Installing v1.5.89 ===' -ForegroundColor Yellow
$setup = 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\src-tauri\target\release\bundle\nsis\RetinaTag_1.5.89_x64-setup.exe'
if (Test-Path $setup) {
    & $setup /S
    Start-Sleep -Seconds 8
    Write-Host 'Installer ran'
}
$exe = 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
if (Test-Path $exe) {
    $v = (Get-Item $exe).VersionInfo.ProductVersion
    Write-Host ("Installed version: {0}" -f $v)
} else {
    Write-Host 'INSTALL FAILED'
    return
}

Write-Host ''
Write-Host '=== Launching ===' -ForegroundColor Yellow
Start-Process $exe
Start-Sleep -Seconds 6
$p = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p) {
    $mb = [math]::Round($p.WorkingSet64/1MB, 1)
    Write-Host ("PID {0} v{1} Responding={2} Mem={3}MB Threads={4}" -f $p.Id, (Get-Item $exe).VersionInfo.ProductVersion, $p.Responding, $mb, $p.Threads.Count)
} else {
    Write-Host 'PROCESS NOT RUNNING'
}
