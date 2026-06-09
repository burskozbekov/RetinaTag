$exe = 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
$existing = Get-Process retina-tag -ErrorAction SilentlyContinue
if (-not $existing) {
    Start-Process $exe
    Start-Sleep -Seconds 6
}
$p = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p) {
    $v = (Get-Item $exe).VersionInfo.ProductVersion
    $mb = [math]::Round($p.WorkingSet64/1MB, 1)
    Write-Host ("OK: PID {0} v{1} Responding={2} Mem={3}MB Threads={4}" -f $p.Id, $v, $p.Responding, $mb, $p.Threads.Count)
} else {
    Write-Host 'FAIL: process not running'
}
$dbPath = 'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
if (Test-Path $dbPath) {
    $f = Get-Item $dbPath
    $libMb = [math]::Round($f.Length/1MB, 1)
    Write-Host ("Library DB: {0} MB" -f $libMb)
} else {
    Write-Host 'No library DB'
}
