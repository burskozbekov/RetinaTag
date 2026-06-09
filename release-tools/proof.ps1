Write-Host '=== SETUP DOSYASI ===' -ForegroundColor Yellow
$setup = 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\src-tauri\target\release\bundle\nsis\RetinaTag_1.5.88_x64-setup.exe'
if (Test-Path $setup) {
    $f = Get-Item $setup
    Write-Host ("Path: {0}" -f $f.FullName)
    Write-Host ("Size: {0:N1} MB" -f ($f.Length/1MB))
    Write-Host ("Created: {0}" -f $f.LastWriteTime)
} else {
    Write-Host 'SETUP YOK'
}

Write-Host ''
Write-Host '=== KURULU BINARY (uninstall+install kanit) ===' -ForegroundColor Yellow
$exe = 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
if (Test-Path $exe) {
    $b = Get-Item $exe
    Write-Host ("Path: {0}" -f $b.FullName)
    Write-Host ("Size: {0:N1} MB" -f ($b.Length/1MB))
    Write-Host ("Install zamani: {0}" -f $b.LastWriteTime)
    Write-Host ("Version: {0}" -f $b.VersionInfo.ProductVersion)
} else {
    Write-Host 'BINARY YOK'
}

Write-Host ''
Write-Host '=== UNINSTALLER (yeni install kanit) ===' -ForegroundColor Yellow
$un = 'C:\Users\dede_\AppData\Local\RetinaTag\uninstall.exe'
if (Test-Path $un) {
    $u = Get-Item $un
    Write-Host ("Path: {0}" -f $u.FullName)
    Write-Host ("Created: {0}" -f $u.LastWriteTime)
}

Write-Host ''
Write-Host '=== REGISTRY (Windows Add/Remove Programs) ===' -ForegroundColor Yellow
$reg = Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*' -ErrorAction SilentlyContinue |
       Where-Object { $_.DisplayName -like '*RetinaTag*' }
if ($reg) {
    foreach ($r in $reg) {
        Write-Host ("Name: {0}" -f $r.DisplayName)
        Write-Host ("Version: {0}" -f $r.DisplayVersion)
        Write-Host ("InstallDate: {0}" -f $r.InstallDate)
        Write-Host ("UninstallString: {0}" -f $r.UninstallString)
    }
} else {
    Write-Host 'Registry kaydi yok'
}

Write-Host ''
Write-Host '=== PROCESS ===' -ForegroundColor Yellow
$p = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p) {
    Write-Host ("PID: {0}" -f $p.Id)
    Write-Host ("StartTime: {0}" -f $p.StartTime)
    Write-Host ("Responding: {0}" -f $p.Responding)
    Write-Host ("Window title: {0}" -f $p.MainWindowTitle)
} else {
    Write-Host 'Process yok'
}
